package dev.sessiontap.android.ui.components

import androidx.compose.animation.core.LinearEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Icon
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Bold
import com.adamglin.phosphoricons.bold.Check
import com.adamglin.phosphoricons.bold.X
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St

/** Display status: the effective status, with stopped split by its root reason. */
enum class Glyph { Running, Blocked, Idle, Completed, Failed, Stopped }

@Composable
fun RunningArc(size: Dp = 12.dp) {
    val c = St.colors
    val t = rememberInfiniteTransition(label = "arc")
    val angle by t.animateFloat(0f, 360f, infiniteRepeatable(tween(900, easing = LinearEasing)), label = "angle")
    Canvas(Modifier.size(size).rotate(angle)) {
        val w = 1.75.dp.toPx()
        val inset = w / 2
        val arcSize = Size(this.size.width - w, this.size.height - w)
        drawArc(c.run.copy(alpha = 0.22f), 0f, 360f, false, Offset(inset, inset), arcSize, style = Stroke(w))
        drawArc(c.run, -135f, 90f, false, Offset(inset, inset), arcSize, style = Stroke(w))
    }
}

@Composable
fun StatusGlyph(glyph: Glyph, modifier: Modifier = Modifier, small: Boolean = false) {
    val c = St.colors
    val dot = if (small) 7.dp else 8.dp
    Box(modifier.size(if (small) 14.dp else 16.dp), contentAlignment = Alignment.Center) {
        when (glyph) {
            Glyph.Running -> RunningArc()
            Glyph.Blocked -> Box(
                Modifier.size(if (small) dot else 14.dp).let { if (small) it else it.background(c.block.copy(alpha = 0.25f), CircleShape).padding(3.dp) }
                    .background(c.block, CircleShape),
            )
            Glyph.Idle -> Box(Modifier.size(dot).border(1.5.dp, c.mute, CircleShape))
            Glyph.Completed -> Icon(PhosphorIcons.Bold.Check, null, tint = c.ok, modifier = Modifier.size(if (small) 12.dp else 13.dp))
            Glyph.Failed -> Icon(PhosphorIcons.Bold.X, null, tint = c.block, modifier = Modifier.size(if (small) 12.dp else 13.dp))
            Glyph.Stopped -> Box(Modifier.size(if (small) 6.dp else 7.dp).background(c.dim, CircleShape))
        }
    }
}

enum class Conn { Live, Reconnecting, Offline, Revoked }

fun connOf(state: ConnState?): Conn = when (state) {
    is ConnState.Live, is ConnState.NoAccess -> Conn.Live
    is ConnState.Reconnecting -> if (state.offline) Conn.Offline else Conn.Reconnecting
    ConnState.Revoked -> Conn.Revoked
    ConnState.Connecting -> Conn.Reconnecting
    null -> Conn.Offline
}

@Composable
fun ConnDot(conn: Conn, size: Dp = 6.dp) {
    val c = St.colors
    when (conn) {
        Conn.Live -> Box(Modifier.size(size).background(c.ok, CircleShape))
        Conn.Reconnecting -> {
            val t = rememberInfiniteTransition(label = "pulse")
            val a by t.animateFloat(1f, 0.3f, infiniteRepeatable(tween(600), RepeatMode.Reverse), label = "alpha")
            Box(Modifier.size(size).alpha(a).background(c.run, CircleShape))
        }
        Conn.Offline -> Box(Modifier.size(size).border(1.25.dp, c.mute, CircleShape))
        Conn.Revoked -> Box(Modifier.size(size).background(c.block, CircleShape))
    }
}

@Composable
fun ProviderMark(mark: String) {
    val c = St.colors
    Text(
        mark,
        fontFamily = Mono,
        fontWeight = FontWeight.Medium,
        fontSize = 9.5.sp,
        color = c.mute,
        modifier = Modifier.border(1.dp, c.line, RoundedCornerShape(4.dp)).padding(horizontal = 4.dp, vertical = 1.dp),
    )
}

/** Rounded surface card with a 1dp hairline, as in the handoff. */
@Composable
fun StCard(
    modifier: Modifier = Modifier,
    shape: Shape = RoundedCornerShape(16.dp),
    color: Color = St.colors.surf,
    padding: PaddingValues = PaddingValues(0.dp),
    content: @Composable () -> Unit,
) {
    Box(modifier.clip(shape).background(color).border(1.dp, St.colors.line, shape).padding(padding)) { content() }
}

@Composable
fun PrimaryButton(text: String, onClick: () -> Unit, modifier: Modifier = Modifier, height: Dp = 52.dp, icon: ImageVector? = null) {
    val c = St.colors
    Button(
        onClick = onClick,
        modifier = modifier.height(height),
        shape = RoundedCornerShape(height / 2),
        colors = ButtonDefaults.buttonColors(containerColor = c.acc, contentColor = c.bg),
    ) {
        if (icon != null) {
            Icon(icon, null, Modifier.size(20.dp))
            Spacer(Modifier.width(8.dp))
        }
        Text(text, fontSize = 15.sp, fontWeight = FontWeight.Medium)
    }
}

@Composable
fun SecondaryButton(
    text: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    height: Dp = 52.dp,
    color: Color = St.colors.text,
    borderColor: Color = St.colors.line,
    icon: ImageVector? = null,
) {
    OutlinedButton(
        onClick = onClick,
        modifier = modifier.height(height),
        shape = RoundedCornerShape(height / 2),
        border = androidx.compose.foundation.BorderStroke(1.dp, borderColor),
        colors = ButtonDefaults.outlinedButtonColors(containerColor = St.colors.surf2.copy(alpha = 0.5f), contentColor = color),
        contentPadding = PaddingValues(horizontal = 14.dp),
    ) {
        if (icon != null) {
            Icon(icon, null, Modifier.size(18.dp))
            Spacer(Modifier.width(8.dp))
        }
        Text(text, fontSize = 15.sp, fontWeight = FontWeight.Medium)
    }
}

/** The handoff's custom switch: accent tint with check when on, hollow track when off. */
@Composable
fun StSwitch(on: Boolean) {
    val c = St.colors
    val shape = RoundedCornerShape(14.dp)
    if (on) {
        Box(Modifier.size(46.dp, 28.dp).clip(shape).background(c.accTint).border(1.5.dp, c.acc, shape), contentAlignment = Alignment.CenterEnd) {
            Box(Modifier.padding(end = 4.dp).size(20.dp).background(c.acc, CircleShape), contentAlignment = Alignment.Center) {
                Icon(PhosphorIcons.Bold.Check, null, tint = c.bg, modifier = Modifier.size(11.dp))
            }
        }
    } else {
        Box(Modifier.size(46.dp, 28.dp).border(1.5.dp, c.dim, shape), contentAlignment = Alignment.CenterStart) {
            Box(Modifier.padding(start = 7.dp).size(14.dp).background(c.dim, CircleShape))
        }
    }
}

/** Selectable chip: hub chips (radius 10, height 32) and filter chips (radius 14, height 28). */
@Composable
fun StChip(
    selected: Boolean,
    onClick: () -> Unit,
    height: Dp,
    radius: Dp,
    modifier: Modifier = Modifier,
    content: @Composable RowScope.() -> Unit,
) {
    val c = St.colors
    val shape = RoundedCornerShape(radius)
    Row(
        modifier
            .height(height)
            .clip(shape)
            .background(if (selected) c.accTint else Color.Transparent)
            .border(1.dp, if (selected) c.acc else c.line, shape)
            .clickable(onClick = onClick)
            .padding(horizontal = if (height > 30.dp) 12.dp else 10.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
        content = content,
    )
}

@Composable
fun KeyValueRow(label: String, value: String, labelWidth: Dp = 88.dp, mono: Boolean = true) {
    Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        Text(label, color = St.colors.mute, fontSize = 13.sp, modifier = Modifier.width(labelWidth))
        Text(value, fontFamily = if (mono) Mono else null, fontSize = 13.sp)
    }
}

@Composable
fun Hairline() = Box(Modifier.fillMaxWidth().height(1.dp).background(St.colors.line))

fun copyText(context: android.content.Context, text: String) {
    val clipboard = context.getSystemService(android.content.ClipboardManager::class.java)
    clipboard.setPrimaryClip(android.content.ClipData.newPlainText("SessionTap", text))
}
