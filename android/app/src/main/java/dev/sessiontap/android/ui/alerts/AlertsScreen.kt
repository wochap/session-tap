package dev.sessiontap.android.ui.alerts

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.regular.BellSlash
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.domain.AlertSettings
import dev.sessiontap.android.ui.components.PrimaryButton
import dev.sessiontap.android.ui.components.StCard
import dev.sessiontap.android.ui.components.StSwitch
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Locale

private val CLOCK = DateTimeFormatter.ofPattern("HH:mm", Locale.US)

@Composable
fun AlertsScreen(
    settings: AlertSettings,
    mutes: Map<String, Long>,
    hubs: List<HubEntity>,
    notificationsEnabled: Boolean,
    now: Instant,
    onPermission: (Boolean) -> Unit,
    onInput: (Boolean) -> Unit,
    onFinished: (Boolean) -> Unit,
    onMute: (HubEntity, Long) -> Unit,
    onUnmute: (HubEntity) -> Unit,
    onOpenSettings: () -> Unit,
    contentPadding: PaddingValues,
) {
    val c = St.colors
    Column(Modifier.fillMaxSize().background(c.bg).padding(top = contentPadding.calculateTopPadding())) {
        Column(Modifier.padding(start = 16.dp, end = 16.dp, top = 10.dp, bottom = 6.dp)) {
            Text("Notifications", fontSize = 22.sp, fontWeight = FontWeight.Medium)
            Text("What reaches you, and when", fontSize = 12.sp, color = c.mute)
        }
        Column(Modifier.weight(1f).verticalScroll(rememberScrollState()).padding(start = 12.dp, end = 12.dp, bottom = contentPadding.calculateBottomPadding() + 24.dp)) {
            if (!notificationsEnabled) {
                StCard(Modifier.fillMaxWidth().padding(top = 8.dp).testTag("notifications-off"), padding = PaddingValues(horizontal = 16.dp, vertical = 14.dp)) {
                    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            Icon(PhosphorIcons.Regular.BellSlash, null, tint = c.block, modifier = Modifier.size(18.dp))
                            Text("Notifications are off", fontSize = 15.sp, fontWeight = FontWeight.Medium)
                        }
                        Text("Android blocks SessionTap's notifications. The session list still updates.", fontSize = 13.sp, color = c.mute)
                        PrimaryButton("Open settings", onOpenSettings, Modifier.padding(top = 6.dp), height = 32.dp)
                    }
                }
            }
            val dim = !notificationsEnabled
            Head("Alert me when")
            val rows = listOf(
                Triple("Needs permission", "An agent wants to run a tool", settings.permission) to onPermission,
                Triple("Needs input", "An agent asked you a question", settings.input) to onInput,
                Triple("Finished", "An agent completed its response · quiet", settings.finished) to onFinished,
            )
            rows.forEachIndexed { i, (row, set) ->
                ToggleRow(row.first, row.second, row.third, radius(i, rows.size), dim, Modifier.testTag("toggle:${row.first}")) { set(!row.third) }
            }
            if (hubs.isNotEmpty()) {
                Head("Hubs")
                hubs.forEachIndexed { i, hub ->
                    val until = mutes[hub.hubId]?.takeIf { it > now.toEpochMilli() }
                    var menu by remember { mutableStateOf(false) }
                    Box {
                        ToggleRow(
                            hub.name,
                            until?.let { "Muted until " + Instant.ofEpochMilli(it).atZone(ZoneId.systemDefault()).format(CLOCK) } ?: "Alerts on",
                            until == null,
                            radius(i, hubs.size),
                            dim,
                            Modifier.testTag("mute:${hub.name}"),
                            mono = until != null,
                        ) { if (until != null) onUnmute(hub) else menu = true }
                        DropdownMenu(expanded = menu, onDismissRequest = { menu = false }, containerColor = c.surf2) {
                            listOf("Mute for 1 hour" to 1L, "Mute for 8 hours" to 8L, "Mute for 24 hours" to 24L).forEach { (label, hours) ->
                                DropdownMenuItem(text = { Text(label) }, onClick = {
                                    menu = false
                                    onMute(hub, now.toEpochMilli() + hours * 3_600_000)
                                })
                            }
                        }
                    }
                }
            }
        }
    }
}

private fun radius(i: Int, n: Int): RoundedCornerShape = when {
    n == 1 -> RoundedCornerShape(16.dp)
    i == 0 -> RoundedCornerShape(16.dp, 16.dp, 4.dp, 4.dp)
    i == n - 1 -> RoundedCornerShape(4.dp, 4.dp, 16.dp, 16.dp)
    else -> RoundedCornerShape(4.dp)
}

@Composable
private fun Head(title: String) =
    Text(title, fontSize = 12.sp, color = St.colors.mute, modifier = Modifier.padding(start = 4.dp, end = 4.dp, top = 18.dp, bottom = 6.dp))

@Composable
private fun ToggleRow(
    title: String,
    sub: String,
    on: Boolean,
    shape: RoundedCornerShape,
    dim: Boolean,
    modifier: Modifier = Modifier,
    mono: Boolean = false,
    onClick: () -> Unit,
) {
    val c = St.colors
    Row(
        modifier.padding(top = 2.dp).fillMaxWidth().clip(shape).background(c.surf).clickable(onClick = onClick)
            .alpha(if (dim) 0.45f else 1f).padding(horizontal = 14.dp, vertical = 12.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Column(Modifier.weight(1f)) {
            Text(title, fontSize = 14.sp, fontWeight = FontWeight.Medium)
            Text(sub, fontSize = 12.sp, color = c.mute, fontFamily = if (mono) Mono else null)
        }
        StSwitch(on)
    }
}

