package dev.sessiontap.android.ui.onboarding

import android.Manifest
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.ContextCompat
import androidx.core.net.toUri
import dev.sessiontap.android.notify.notificationsAllowed
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Bold
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.bold.CheckCircle
import com.adamglin.phosphoricons.regular.ArrowLeft
import com.adamglin.phosphoricons.regular.Broadcast
import com.adamglin.phosphoricons.regular.Copy
import com.adamglin.phosphoricons.regular.Info
import com.adamglin.phosphoricons.regular.QrCode
import dev.sessiontap.android.ui.components.PrimaryButton
import dev.sessiontap.android.ui.components.SecondaryButton
import dev.sessiontap.android.ui.components.StCard
import dev.sessiontap.android.ui.components.copyText
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St

@Composable
fun WelcomeScreen(onScan: () -> Unit, contentPadding: PaddingValues) {
    val c = St.colors
    val clipboardContext = LocalContext.current
    Column(Modifier.fillMaxSize().background(c.bg).padding(contentPadding).padding(start = 24.dp, end = 24.dp, top = 20.dp, bottom = 24.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Icon(PhosphorIcons.Regular.Broadcast, null, tint = c.acc, modifier = Modifier.size(18.dp))
            Text("SessionTap", fontSize = 14.sp, fontWeight = FontWeight.Medium, color = c.mute)
        }
        Spacer(Modifier.weight(1f))
        Text("Watch your coding agents from your pocket.", fontSize = 34.sp, lineHeight = 37.sp, fontWeight = FontWeight.Medium, letterSpacing = (-0.68).sp, modifier = Modifier.padding(bottom = 12.dp))
        Text(
            "Pair with the computer running Claude Code, Codex, Pi or Qwen. Follow every session, and on hubs that allow it, answer agents in their live terminal.",
            fontSize = 15.sp,
            color = c.mute,
            modifier = Modifier.padding(bottom = 24.dp),
        )
        Text("On your computer, run", fontSize = 12.sp, color = c.mute, modifier = Modifier.padding(bottom = 6.dp))
        StCard(Modifier.fillMaxWidth(), shape = RoundedCornerShape(14.dp), padding = PaddingValues(start = 16.dp, end = 6.dp, top = 4.dp, bottom = 4.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                Text("$", fontFamily = Mono, fontSize = 14.sp, color = c.mute)
                Text("sessiontap-hub pair", fontFamily = Mono, fontSize = 14.sp, modifier = Modifier.weight(1f))
                IconButton(onClick = { copyText(clipboardContext, "sessiontap-hub pair") }) {
                    Icon(PhosphorIcons.Regular.Copy, "Copy", tint = c.mute, modifier = Modifier.size(18.dp))
                }
            }
        }
        Text("A QR code appears in the terminal.", fontSize = 12.sp, color = c.mute, modifier = Modifier.padding(top = 8.dp, bottom = 28.dp))
        PrimaryButton("Scan QR", onScan, Modifier.fillMaxWidth().testTag("scan-qr"), icon = PhosphorIcons.Regular.QrCode)
    }
}

data class PermissionState(val camera: Boolean, val notifications: Boolean, val battery: Boolean)

fun permissionState(context: Context): PermissionState = PermissionState(
    camera = ContextCompat.checkSelfPermission(context, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED,
    notifications = notificationsAllowed(context),
    battery = context.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(context.packageName),
)

fun appSettingsIntent(context: Context) =
    Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.fromParts("package", context.packageName, null))

@Composable
fun PermissionsScreen(onBack: () -> Unit, onContinue: () -> Unit, contentPadding: PaddingValues) {
    val c = St.colors
    val context = LocalContext.current
    var tick by remember { mutableIntStateOf(0) }
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { tick++ }
    val state = remember(tick) { permissionState(context) }
    var asked by remember { mutableIntStateOf(0) }
    val askCamera = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { tick++; asked = asked or 1 }
    val askNotify = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { tick++; asked = asked or 2 }
    Column(Modifier.fillMaxSize().background(c.bg).padding(contentPadding).padding(start = 24.dp, end = 24.dp, top = 12.dp, bottom = 24.dp)) {
        IconButton(onClick = onBack, modifier = Modifier.padding(start = 0.dp)) {
            Icon(PhosphorIcons.Regular.ArrowLeft, "Back", tint = c.text, modifier = Modifier.size(22.dp))
        }
        Text("Before you scan", fontSize = 28.sp, fontWeight = FontWeight.Medium, letterSpacing = (-0.56).sp, modifier = Modifier.padding(top = 16.dp, bottom = 8.dp))
        Text("Three things keep alerts reliable. You can change them later in Android settings.", fontSize = 14.sp, color = c.mute, modifier = Modifier.padding(bottom = 20.dp))
        StCard(Modifier.fillMaxWidth()) {
            Column {
                PermRow(
                    "Camera", "To scan the pairing code", state.camera, denied = asked and 1 != 0, first = true, tag = "perm:camera",
                    onAllow = { askCamera.launch(Manifest.permission.CAMERA) },
                    onSettings = { context.startActivity(appSettingsIntent(context)) },
                )
                PermRow(
                    "Notifications", "So you hear when an agent is blocked", state.notifications, denied = asked and 2 != 0, tag = "perm:notifications",
                    onAllow = {
                        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) askNotify.launch(Manifest.permission.POST_NOTIFICATIONS)
                        else context.startActivity(appSettingsIntent(context))
                    },
                    onSettings = { context.startActivity(appSettingsIntent(context)) },
                )
                PermRow(
                    "Keep running in background", "Turns off battery optimization so the encrypted channel stays open", state.battery, denied = false, tag = "perm:battery",
                    onAllow = {
                        @Suppress("BatteryLife")
                        val intent = Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, "package:${context.packageName}".toUri())
                        context.startActivity(intent)
                    },
                    onSettings = {},
                )
            }
        }
        Row(Modifier.padding(top = 16.dp, start = 4.dp, end = 4.dp), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            Icon(PhosphorIcons.Regular.Info, null, tint = c.mute, modifier = Modifier.size(16.dp).padding(top = 1.dp))
            Text("Hubs on another network are reached over Tailscale. Keep it connected on this phone. On a new shared Wi-Fi network, the app finds a hub only when that hub enables remote.discovery.", fontSize = 12.5.sp, color = c.mute)
        }
        Spacer(Modifier.weight(1f))
        PrimaryButton("Continue", onContinue, Modifier.fillMaxWidth().testTag("continue"))
    }
}

@Composable
private fun PermRow(
    title: String,
    sub: String,
    done: Boolean,
    denied: Boolean,
    tag: String,
    first: Boolean = false,
    onAllow: () -> Unit,
    onSettings: () -> Unit,
) {
    val c = St.colors
    Column(Modifier.testTag(tag)) {
        if (!first) Box(Modifier.fillMaxWidth().height(1.dp).background(c.line))
        Row(Modifier.padding(start = 16.dp, end = 14.dp, top = 14.dp, bottom = 14.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(14.dp)) {
            Box(Modifier.size(24.dp), contentAlignment = Alignment.Center) {
                if (done) Icon(PhosphorIcons.Bold.CheckCircle, "Granted", tint = c.ok, modifier = Modifier.size(22.dp))
                else Box(Modifier.size(18.dp).border(1.5.dp, c.dim, CircleShape))
            }
            Column(Modifier.weight(1f)) {
                Text(title, fontSize = 14.sp, fontWeight = FontWeight.Medium)
                Text(if (!done && denied) "Denied · grant it later in Android settings" else sub, fontSize = 12.5.sp, color = c.mute)
            }
            if (!done) {
                if (denied) SecondaryButton("Settings", onSettings, height = 32.dp)
                else SecondaryButton("Allow", onAllow, height = 32.dp)
            }
        }
    }
}
