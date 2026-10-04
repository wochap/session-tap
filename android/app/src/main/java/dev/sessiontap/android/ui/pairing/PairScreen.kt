package dev.sessiontap.android.ui.pairing

import android.content.Intent
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.net.toUri
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Bold
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.bold.CheckCircle
import com.adamglin.phosphoricons.bold.Prohibit
import com.adamglin.phosphoricons.bold.X
import com.adamglin.phosphoricons.regular.Plugs
import com.adamglin.phosphoricons.regular.Timer
import com.adamglin.phosphoricons.regular.Warning
import com.adamglin.phosphoricons.regular.X as XRegular
import dev.sessiontap.android.ui.components.PrimaryButton
import dev.sessiontap.android.ui.components.RunningArc
import dev.sessiontap.android.ui.components.SecondaryButton
import dev.sessiontap.android.ui.components.StCard
import dev.sessiontap.android.ui.hubs.endpointKind
import dev.sessiontap.android.ui.hubs.hubKeyShort
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St
import java.net.ConnectException
import java.net.NoRouteToHostException
import java.net.SocketTimeoutException
import java.net.UnknownHostException
import javax.net.ssl.SSLException

@Composable
fun PairScreen(
    state: PairState,
    nowMs: Long,
    onClose: () -> Unit,
    onScanAgain: () -> Unit,
    onRetry: () -> Unit,
    onDone: () -> Unit,
    contentPadding: PaddingValues,
) {
    val c = St.colors
    Column(Modifier.fillMaxSize().background(c.bg).padding(contentPadding).padding(start = 24.dp, end = 24.dp, top = 12.dp, bottom = 24.dp)) {
        IconButton(onClick = onClose) { Icon(PhosphorIcons.Regular.XRegular, "Close", tint = c.text, modifier = Modifier.size(22.dp)) }
        Column(Modifier.padding(top = 40.dp).weight(1f).testTag("pair:${state::class.simpleName}")) {
            when (state) {
                PairState.Idle, is PairState.Connecting -> {
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        RunningArc()
                        Text("Connecting", fontSize = 13.sp, color = c.mute)
                    }
                    Title("Reaching ${(state as? PairState.Connecting)?.hubName ?: "hub"}")
                    Body("Checking the hub's key against the code you scanned.")
                    Spacer(Modifier.weight(1f))
                    SecondaryButton("Cancel", onClose, Modifier.fillMaxWidth(), height = 48.dp)
                }
                is PairState.Waiting -> {
                    val left = ((state.expiresAt - nowMs) / 1000).coerceAtLeast(0)
                    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                        RunningArc()
                        Text("Waiting · expires in ${left / 60}:${"%02d".format(left % 60)}", fontSize = 13.sp, color = c.mute)
                    }
                    Title("Approve on ${state.hubName}", top = 14.dp)
                    Text(
                        buildAnnotatedString {
                            append("The terminal on ${state.hubName} is asking whether to trust this phone. Check the fingerprint matches before you type ")
                            withStyle(SpanStyle(fontFamily = Mono, color = c.text)) { append("y") }
                            append(".")
                        },
                        fontSize = 14.sp,
                        color = c.mute,
                        modifier = Modifier.padding(bottom = 24.dp),
                    )
                    StCard(Modifier.fillMaxWidth(), padding = PaddingValues(16.dp)) {
                        Column {
                            Text("This phone", fontSize = 12.sp, color = c.mute)
                            Text(state.deviceName, fontSize = 16.sp, fontWeight = FontWeight.Medium, modifier = Modifier.padding(top = 2.dp, bottom = 16.dp))
                            Text("Device fingerprint", fontSize = 12.sp, color = c.mute, modifier = Modifier.padding(bottom = 8.dp))
                            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                state.fingerprint.forEach { g ->
                                    StCard(Modifier.weight(1f), shape = RoundedCornerShape(10.dp), color = c.bg, padding = PaddingValues(vertical = 10.dp)) {
                                        Text(
                                            g,
                                            fontFamily = Mono,
                                            fontSize = 15.sp,
                                            fontWeight = FontWeight.Medium,
                                            textAlign = TextAlign.Center,
                                            modifier = Modifier.fillMaxWidth().testTag("fp"),
                                        )
                                    }
                                }
                            }
                        }
                    }
                    Spacer(Modifier.weight(1f))
                    SecondaryButton("Cancel", onClose, Modifier.fillMaxWidth(), height = 48.dp)
                }
                is PairState.Paired -> {
                    BigIcon(PhosphorIcons.Bold.CheckCircle, c.ok)
                    Title("Paired with ${state.hubName}")
                    Body(
                        "Connected over ${endpointKind(state.endpoint)}. " +
                            "When you leave home it switches to your tailnet automatically.",
                    )
                    Column(Modifier.padding(top = 20.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        Kv("Endpoint", "${endpointKind(state.endpoint)} · ${state.endpoint}")
                        Kv("Hub key", hubKeyShort(state.hubId))
                    }
                    Spacer(Modifier.weight(1f))
                    PrimaryButton("View sessions", onDone, Modifier.fillMaxWidth().testTag("view-sessions"))
                }
                PairState.Expired -> {
                    BigIcon(PhosphorIcons.Regular.Timer, c.mute)
                    Title("Pairing code expired")
                    Text(
                        buildAnnotatedString {
                            append("Codes last 2 minutes. Run ")
                            withStyle(SpanStyle(fontFamily = Mono, color = c.text)) { append("sessiontap-hub pair") }
                            append(" again for a fresh one.")
                        },
                        fontSize = 14.sp,
                        color = c.mute,
                    )
                    Spacer(Modifier.weight(1f))
                    PrimaryButton("Scan again", onScanAgain, Modifier.fillMaxWidth())
                }
                is PairState.Rejected -> {
                    BigIcon(PhosphorIcons.Bold.Prohibit, c.block)
                    Title("${state.hubName} declined this phone")
                    Body("Someone answered no on the hub, or the fingerprints didn't match. Nothing was shared.")
                    Spacer(Modifier.weight(1f))
                    SecondaryButton("Start over", onScanAgain, Modifier.fillMaxWidth())
                }
                is PairState.Failed -> {
                    BigIcon(PhosphorIcons.Regular.Warning, c.block)
                    Title("Pairing failed")
                    Body("${state.hubName} did not accept the pairing proof. Run sessiontap-hub pair again and rescan.")
                    Text(state.message, fontFamily = Mono, fontSize = 12.sp, color = c.mute, modifier = Modifier.padding(top = 12.dp))
                    Spacer(Modifier.weight(1f))
                    PrimaryButton("Scan again", onScanAgain, Modifier.fillMaxWidth())
                }
                is PairState.Unreachable -> Unreachable(state, onRetry)
                is PairState.Invalid -> {
                    BigIcon(PhosphorIcons.Regular.Warning, c.mute)
                    Title("Not a pairing code")
                    Body(state.message)
                    Spacer(Modifier.weight(1f))
                    PrimaryButton("Scan again", onScanAgain, Modifier.fillMaxWidth())
                }
            }
        }
    }
}

@Composable
private fun ColumnScope.Unreachable(state: PairState.Unreachable, onRetry: () -> Unit) {
    val c = St.colors
    val context = LocalContext.current
    BigIcon(PhosphorIcons.Regular.Plugs, c.mute)
    Title("Can't reach ${state.hubName}")
    Body(if (state.failures.size > 1) "The code is valid, but no address answered." else "The code is valid, but the address did not answer.")
    StCard(Modifier.fillMaxWidth().padding(top = 16.dp), shape = RoundedCornerShape(14.dp), color = Color.Transparent) {
        Column {
            state.failures.forEachIndexed { i, f ->
                if (i > 0) Box(Modifier.fillMaxWidth().height(1.dp).background(c.line))
                Row(Modifier.padding(horizontal = 12.dp, vertical = 10.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                    Icon(PhosphorIcons.Bold.X, null, tint = c.block, modifier = Modifier.size(14.dp))
                    Text(f.endpoint, fontFamily = Mono, fontSize = 12.sp, modifier = Modifier.weight(1f))
                    Text("${endpointKind(f.endpoint)} · ${describe(f.error)}", fontFamily = Mono, fontSize = 12.sp, color = c.mute)
                }
            }
        }
    }
    Text("Join the same Wi-Fi as ${state.hubName}, or connect Tailscale on this phone.", fontSize = 13.sp, color = c.mute, modifier = Modifier.padding(top = 14.dp))
    Spacer(Modifier.weight(1f))
    Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
        SecondaryButton("Open Tailscale", { openTailscale(context) }, Modifier.weight(1f))
        PrimaryButton("Retry", onRetry, Modifier.weight(1f))
    }
}

fun openTailscale(context: android.content.Context) {
    val intent = context.packageManager.getLaunchIntentForPackage("com.tailscale.ipn")
        ?: Intent(Intent.ACTION_VIEW, "https://tailscale.com/download/android".toUri())
    context.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
}

private fun describe(error: Throwable): String = when (error) {
    is SocketTimeoutException -> "timeout"
    is NoRouteToHostException -> "no route"
    is UnknownHostException -> "unknown host"
    is ConnectException -> "refused"
    is SSLException -> "key mismatch"
    else -> error.javaClass.simpleName.removeSuffix("Exception").lowercase().ifEmpty { "failed" }
}

@Composable
private fun BigIcon(icon: ImageVector, tint: Color) = Icon(icon, null, tint = tint, modifier = Modifier.size(40.dp))

@Composable
private fun Title(text: String, top: androidx.compose.ui.unit.Dp = 16.dp) =
    Text(text, fontSize = 28.sp, fontWeight = FontWeight.Medium, letterSpacing = (-0.56).sp, modifier = Modifier.padding(top = top, bottom = 8.dp))

@Composable
private fun Body(text: String) = Text(text, fontSize = 14.sp, color = St.colors.mute)

@Composable
private fun Kv(label: String, value: String) {
    Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        Text(label, fontSize = 13.sp, color = St.colors.mute, modifier = Modifier.width(88.dp))
        Text(value, fontFamily = Mono, fontSize = 13.sp)
    }
}
