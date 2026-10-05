package dev.sessiontap.android.ui.terminal

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.RoundedCornerShape
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
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.lerp
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntRect
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Popup
import androidx.compose.ui.window.PopupPositionProvider
import androidx.compose.ui.window.PopupProperties
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Bold
import com.adamglin.phosphoricons.Fill
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.bold.ArrowDown
import com.adamglin.phosphoricons.bold.ArrowLeft
import com.adamglin.phosphoricons.bold.ArrowRight
import com.adamglin.phosphoricons.bold.ArrowUp
import com.adamglin.phosphoricons.fill.LockSimple
import com.adamglin.phosphoricons.regular.Backspace
import com.adamglin.phosphoricons.regular.PencilSimple
import dev.sessiontap.android.domain.KeyLayout
import dev.sessiontap.android.domain.KeySpec
import dev.sessiontap.android.net.TerminalKeys
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St

/** Icon a key shows instead of its label: the arrows and Backspace. */
fun keyIcon(key: KeySpec): ImageVector? = when ((key as? KeySpec.Named)?.key) {
    TerminalKeys.UP -> PhosphorIcons.Bold.ArrowUp
    TerminalKeys.DOWN -> PhosphorIcons.Bold.ArrowDown
    TerminalKeys.LEFT -> PhosphorIcons.Bold.ArrowLeft
    TerminalKeys.RIGHT -> PhosphorIcons.Bold.ArrowRight
    TerminalKeys.BACKSPACE -> PhosphorIcons.Regular.Backspace
    else -> null
}

private fun isArrow(key: KeySpec) = (key as? KeySpec.Named)?.key in setOf(TerminalKeys.UP, TerminalKeys.DOWN, TerminalKeys.LEFT, TerminalKeys.RIGHT)

/** How a key cap is drawn. */
data class KeyLook(val bg: Color, val ring: Color, val ringWidth: Dp, val content: Color)

@Composable
fun keyLook(key: KeySpec, mod: ModState = ModState.Off, armed: Boolean = false, selected: Boolean = false): KeyLook {
    val c = St.colors
    var look = KeyLook(
        bg = if (isArrow(key)) lerp(c.surf2, c.text, 0.18f) else c.surf2,
        ring = c.line,
        ringWidth = 1.dp,
        content = if (key == KeySpec.CtrlC) c.block else c.text,
    )
    if (mod == ModState.Latched) look = KeyLook(c.accTint, c.acc, 1.dp, c.accInk)
    if (mod == ModState.Locked) look = KeyLook(lerp(c.surf2, c.acc, 0.34f), c.acc, 1.5.dp, c.text)
    if (armed) look = look.copy(bg = c.blockTint, ring = c.block, ringWidth = 1.5.dp)
    if (selected) look = look.copy(ring = c.acc, ringWidth = 1.5.dp)
    return look
}

/** Icon or mono label of a key, centred in its cap. */
@Composable
fun KeyFace(key: KeySpec, color: Color, iconSize: Dp = 16.dp, fontSize: Float = 12.5f) {
    val icon = keyIcon(key)
    if (icon != null) {
        Icon(icon, TerminalKeys.label((key as KeySpec.Named).key), tint = color, modifier = Modifier.size(iconSize))
    } else {
        Text(key.label, fontFamily = Mono, fontSize = fontSize.sp, fontWeight = FontWeight.Medium, color = color, maxLines = 1, softWrap = false)
    }
}

/** What the key grid sends; every callback is a no-op for the editor preview. */
data class KeyGridActions(
    val onKey: (KeySpec) -> Unit = {},
    val onModTap: (ModKey) -> Unit = {},
    val onModLock: (ModKey) -> Unit = {},
    val onEditKeys: () -> Unit = {},
)

private fun KeySpec.modifier(): ModKey? = when (this) {
    KeySpec.Ctrl -> ModKey.Ctrl
    KeySpec.Alt -> ModKey.Alt
    else -> null
}

/**
 * The key bar: rows of equal-width 42dp keys rendered from [layout], growing
 * upward with more rows. Ctrl and Alt latch on tap and lock on long-press; a
 * long-press on any other key opens a popover with "Edit keys…".
 */
@OptIn(ExperimentalFoundationApi::class)
@Composable
fun KeyGrid(
    layout: KeyLayout,
    enabled: Boolean,
    mods: Modifiers,
    ctrlArmed: Boolean,
    actions: KeyGridActions,
    modifier: Modifier = Modifier,
    interactive: Boolean = true,
) {
    val haptic = LocalHapticFeedback.current
    var popover by remember { mutableStateOf<Pair<Int, Int>?>(null) }
    Column(modifier.fillMaxWidth().testTag("key-bar"), verticalArrangement = Arrangement.spacedBy(4.dp)) {
        layout.rows.forEachIndexed { r, row ->
            if (row.isEmpty()) return@forEachIndexed
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                row.forEachIndexed { i, key ->
                    val mod = key.modifier()
                    val armed = key == KeySpec.CtrlC && ctrlArmed && enabled
                    val look = keyLook(key, mod?.let { mods[it] } ?: ModState.Off, armed, selected = popover == r to i && enabled)
                    val shape = RoundedCornerShape(8.dp)
                    Box(Modifier.weight(1f)) {
                        Box(
                            Modifier.fillMaxWidth().height(42.dp).clip(shape).background(look.bg).border(look.ringWidth, look.ring, shape)
                                .alpha(if (enabled) 1f else 0.38f)
                                .combinedClickable(
                                    enabled = enabled && interactive,
                                    role = Role.Button,
                                    onLongClickLabel = if (mod != null) "Lock" else "Edit keys",
                                    onLongClick = {
                                        haptic.performHapticFeedback(HapticFeedbackType.LongPress)
                                        if (mod != null) actions.onModLock(mod) else popover = r to i
                                    },
                                    onClick = {
                                        popover = null
                                        if (mod != null) actions.onModTap(mod) else actions.onKey(key)
                                    },
                                )
                                .testTag("key:${key.id}"),
                            contentAlignment = Alignment.Center,
                        ) {
                            KeyFace(key, look.content)
                            if (mod != null && mods[mod] == ModState.Locked) {
                                Icon(PhosphorIcons.Fill.LockSimple, "locked", tint = look.content, modifier = Modifier.align(Alignment.TopEnd).padding(4.dp).size(9.dp))
                            }
                            if (mod != null && mods[mod] == ModState.Latched) {
                                Box(Modifier.align(Alignment.BottomCenter).padding(bottom = 4.dp).fillMaxWidth(0.36f).height(2.dp).clip(RoundedCornerShape(1.dp)).background(St.colors.acc))
                            }
                        }
                        when {
                            armed -> KeyPopover("Ctrl+C", "Tap again within 2.5s to interrupt", alert = true, onEdit = null, onDismiss = {}, tag = "ctrl-c-armed")
                            popover == r to i && enabled -> KeyPopover(key.label, key.description, alert = false, onEdit = {
                                popover = null
                                actions.onEditKeys()
                            }, onDismiss = { popover = null }, tag = "key-popover")
                        }
                    }
                }
            }
        }
    }
}

/** Places a popup just above its anchor, kept inside the window. */
private class AboveAnchor(private val gap: Int) : PopupPositionProvider {
    override fun calculatePosition(anchorBounds: IntRect, windowSize: IntSize, layoutDirection: LayoutDirection, popupContentSize: IntSize): IntOffset {
        val x = (anchorBounds.center.x - popupContentSize.width / 2).coerceIn(0, (windowSize.width - popupContentSize.width).coerceAtLeast(0))
        val y = (anchorBounds.top - popupContentSize.height - gap).coerceAtLeast(0)
        return IntOffset(x, y)
    }
}

@Composable
private fun KeyPopover(title: String, desc: String, alert: Boolean, onEdit: (() -> Unit)?, onDismiss: () -> Unit, tag: String) {
    val c = St.colors
    val gap = with(LocalDensity.current) { 4.dp.roundToPx() }
    val shape = RoundedCornerShape(12.dp)
    Popup(popupPositionProvider = remember(gap) { AboveAnchor(gap) }, onDismissRequest = onDismiss, properties = PopupProperties(focusable = onEdit != null)) {
        Column(
            Modifier.widthIn(max = 280.dp).clip(shape).background(c.surf2)
                .border(1.dp, if (alert) c.block.copy(alpha = 0.5f) else c.line, shape).padding(horizontal = 12.dp, vertical = 10.dp).testTag(tag),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            Row(verticalAlignment = Alignment.Bottom, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(title, fontFamily = Mono, fontSize = 15.sp, fontWeight = FontWeight.SemiBold, color = if (alert) c.block else c.text)
                Text(desc, fontSize = 12.sp, color = c.mute)
            }
            if (onEdit != null) {
                val pill = RoundedCornerShape(20.dp)
                Row(
                    Modifier.height(40.dp).clip(pill).background(c.bg).border(1.dp, c.line, pill)
                        .combinedClickable(role = Role.Button, onClick = onEdit).padding(horizontal = 14.dp).testTag("edit-keys-popover"),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    Icon(PhosphorIcons.Regular.PencilSimple, null, tint = c.text, modifier = Modifier.size(16.dp))
                    Text("Edit keys…", fontSize = 14.sp, color = c.text)
                }
            }
        }
    }
}
