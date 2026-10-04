package dev.sessiontap.android.ui.hubs

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.regular.LinkBreak
import com.adamglin.phosphoricons.regular.Plus
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.domain.relativeAge
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.ui.components.Conn
import dev.sessiontap.android.ui.components.ConnDot
import dev.sessiontap.android.ui.components.PrimaryButton
import dev.sessiontap.android.ui.components.StCard
import dev.sessiontap.android.ui.components.connOf
import dev.sessiontap.android.ui.sessions.connText
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St
import java.time.Instant

/** "tailnet" for Tailscale addresses and MagicDNS names, else "LAN". */
fun endpointKind(endpoint: String): String {
    val host = endpoint.substringBeforeLast(':').trim('[', ']')
    if (host.endsWith(".ts.net")) return "tailnet"
    val parts = host.split('.').mapNotNull { it.toIntOrNull() }
    if (parts.size == 4 && parts[0] == 100 && parts[1] in 64..127) return "tailnet"
    if (host.startsWith("fd7a:115c:a1e0")) return "tailnet"
    return "LAN"
}

/** First 16 hex characters of the hub id in groups of four, uppercase. */
fun hubKeyShort(hubId: String): String = hubId.take(16).uppercase().chunked(4).joinToString(" ")

@Composable
fun HubsScreen(
    hubs: List<HubEntity>,
    conn: Map<String, ConnState>,
    now: Instant,
    vpnActive: Boolean,
    onPair: () -> Unit,
    onUnpair: (HubEntity) -> Unit,
    contentPadding: PaddingValues,
) {
    val c = St.colors
    var confirm by remember { mutableStateOf<HubEntity?>(null) }
    Column(Modifier.fillMaxSize().background(c.bg).padding(top = contentPadding.calculateTopPadding())) {
        Column(Modifier.padding(start = 16.dp, end = 16.dp, top = 10.dp, bottom = 10.dp)) {
            Text("Hubs", fontSize = 22.sp, fontWeight = FontWeight.Medium)
            Text("${hubs.size} paired · end-to-end encrypted", fontSize = 12.sp, color = c.mute)
        }
        Column(
            Modifier.weight(1f).verticalScroll(rememberScrollState())
                .padding(start = 12.dp, end = 12.dp, top = 4.dp, bottom = contentPadding.calculateBottomPadding() + 24.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            hubs.forEach { hub ->
                val state = conn[hub.hubId] ?: if (hub.revoked) ConnState.Revoked else null
                val endpoint = (state as? ConnState.Live)?.endpoint ?: hub.lastGoodEndpoint ?: hub.endpoints.firstOrNull().orEmpty()
                StCard(Modifier.fillMaxWidth().testTag("hub:${hub.name}"), padding = PaddingValues(start = 16.dp, end = 16.dp, top = 14.dp, bottom = 8.dp)) {
                    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            ConnDot(connOf(state), 7.dp)
                            Text(hub.name, fontSize = 16.sp, fontWeight = FontWeight.Medium, modifier = Modifier.weight(1f))
                            Text(connText(hub, state, now), fontSize = 12.sp, color = if (connOf(state) == Conn.Revoked) c.block else c.mute, modifier = Modifier.testTag("conn:${hub.name}"))
                        }
                        Column(verticalArrangement = Arrangement.spacedBy(5.dp)) {
                            Field("Endpoint", "${endpointKind(endpoint)} · $endpoint")
                            Field("Hub key", hubKeyShort(hub.hubId))
                            Field("Last seen", hub.lastSeenAt?.let { relativeAge(Instant.ofEpochMilli(it), now) + " ago" } ?: "never")
                            Field("Access", hub.scopes.joinToString(", ").ifEmpty { "none" })
                        }
                        Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                            if (connOf(state) == Conn.Revoked) {
                                TextButton(onClick = onPair) { Text("Pair again", color = c.acc, fontSize = 13.sp) }
                            }
                            TextButton(onClick = { confirm = hub }, modifier = Modifier.testTag("unpair:${hub.name}")) {
                                Text("Unpair", color = c.block, fontSize = 13.sp)
                            }
                        }
                    }
                }
            }
            PrimaryButton("Pair another hub", onPair, Modifier.fillMaxWidth().padding(top = 6.dp), height = 48.dp, icon = PhosphorIcons.Regular.Plus)
            Row(Modifier.padding(horizontal = 4.dp, vertical = 6.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Box(Modifier.size(6.dp).background(if (vpnActive) c.ok else c.mute, RoundedCornerShape(3.dp)))
                Text(
                    if (vpnActive) "VPN connected on this phone · off-LAN hubs reachable through Tailscale"
                    else "No VPN on this phone · connect Tailscale to reach hubs off your LAN",
                    fontSize = 12.sp,
                    color = c.mute,
                )
            }
            Text(
                "If Tailscale blocks connections without VPN, LAN addresses are unreachable and the tailnet address is used.",
                fontSize = 12.sp,
                color = c.mute,
                modifier = Modifier.padding(horizontal = 4.dp),
            )
        }
    }
    confirm?.let { hub ->
        AlertDialog(
            onDismissRequest = { confirm = null },
            containerColor = c.surf2,
            shape = RoundedCornerShape(28.dp),
            icon = { Icon(PhosphorIcons.Regular.LinkBreak, null, tint = c.block, modifier = Modifier.size(24.dp)) },
            title = { Text("Unpair ${hub.name}?", fontSize = 22.sp, fontWeight = FontWeight.Medium) },
            text = {
                Text(
                    "This phone stops receiving ${hub.name}'s sessions and alerts and forgets the hub's key. " +
                        "To revoke this phone on the hub too, run sessiontap-hub revoke there.",
                    fontSize = 14.sp,
                    color = c.mute,
                )
            },
            dismissButton = { TextButton(onClick = { confirm = null }) { Text("Cancel", color = c.acc) } },
            confirmButton = {
                TextButton(onClick = { confirm = null; onUnpair(hub) }, modifier = Modifier.testTag("confirm-unpair")) { Text("Unpair", color = c.block) }
            },
        )
    }
}

@Composable
private fun Field(label: String, value: String) {
    Row {
        Text(label, fontSize = 12.5.sp, color = St.colors.mute, modifier = Modifier.width(84.dp))
        Text(value, fontFamily = Mono, fontSize = 12.sp, maxLines = 1, overflow = TextOverflow.Ellipsis)
    }
}
