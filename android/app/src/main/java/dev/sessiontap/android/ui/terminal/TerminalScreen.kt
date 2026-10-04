package dev.sessiontap.android.ui.terminal

import android.content.res.Configuration
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.defaultMinSize
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.isImeVisible
import androidx.compose.foundation.layout.consumeWindowInsets
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.SemanticsPropertyKey
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Bold
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.bold.ArrowDown as ArrowDownBold
import com.adamglin.phosphoricons.bold.ArrowLeft as ArrowLeftBold
import com.adamglin.phosphoricons.bold.ArrowRight as ArrowRightBold
import com.adamglin.phosphoricons.bold.ArrowUp as ArrowUpBold
import com.adamglin.phosphoricons.bold.Warning
import com.adamglin.phosphoricons.regular.ArrowElbowDownLeft
import com.adamglin.phosphoricons.regular.ArrowLeft
import com.adamglin.phosphoricons.regular.Backspace
import com.adamglin.phosphoricons.regular.ChatCircleDots
import com.adamglin.phosphoricons.regular.CheckCircle
import com.adamglin.phosphoricons.regular.ClipboardText
import com.adamglin.phosphoricons.regular.Copy
import com.adamglin.phosphoricons.regular.Desktop
import com.adamglin.phosphoricons.regular.Eye
import com.adamglin.phosphoricons.regular.LockKey
import com.adamglin.phosphoricons.regular.LockSimple
import com.adamglin.phosphoricons.regular.MouseScroll
import com.adamglin.phosphoricons.regular.PauseCircle
import com.adamglin.phosphoricons.regular.Plugs
import com.adamglin.phosphoricons.regular.Power
import com.adamglin.phosphoricons.regular.ShieldWarning
import com.adamglin.phosphoricons.regular.TerminalWindow
import com.adamglin.phosphoricons.regular.XSquare
import dev.sessiontap.android.net.InputUnavailable
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.TerminalKeys
import dev.sessiontap.android.ui.components.PrimaryButton
import dev.sessiontap.android.ui.components.RunningArc
import dev.sessiontap.android.ui.components.SecondaryButton
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Locale

/** Background a region paints, exposed for tests of the dark terminal surface. */
val SurfaceColor = SemanticsPropertyKey<Color>("SurfaceColor")

/** The agent's effective status as the top bar words it. */
enum class AgentWord(val text: String) { Waiting("waiting for you"), Running("running"), Idle("idle"), Exited("exited") }

/** Everything the screen shows besides the pane itself. */
data class TerminalUi(
    val title: String,
    /** "repo · branch · hub". */
    val where: String,
    val hubName: String,
    val sourceName: String,
    val word: AgentWord,
    /** Why the agent is blocked, when it is. */
    val blocked: ReasonKind?,
    val blockedOther: Boolean,
    val digits: Boolean,
    val state: TerminalState,
    val reply: String,
    val error: String?,
    val ctrlArmed: Boolean,
    val cols: Int,
    val rows: Int,
    /** When the agent started, for the end card's duration. */
    val startedAt: Instant?,
    val unreachableDetail: String? = null,
)

/** Callbacks from the screen; all default to no-ops for previews and tests. */
data class TerminalActions(
    val onBack: () -> Unit = {},
    val onKey: (String) -> Unit = {},
    val onPaste: () -> Unit = {},
    val onReply: (String) -> Unit = {},
    val onSend: (enter: Boolean) -> Unit = {},
    val onRetry: () -> Unit = {},
    val onCopyScreen: () -> Unit = {},
)

private val CLOCK = DateTimeFormatter.ofPattern("HH:mm", Locale.US)

private fun Long.clock(): String = Instant.ofEpochMilli(this).atZone(ZoneId.systemDefault()).format(CLOCK)

/** Connection chip text per phase. */
fun connLabel(phase: TerminalPhase): String = when (phase) {
    TerminalPhase.Opening -> "connecting"
    TerminalPhase.Reconnecting -> "Reconnecting…"
    is TerminalPhase.Live -> if (phase.input is InputMode.Paused) "input paused" else "live"
    is TerminalPhase.Ended -> "ended"
    is TerminalPhase.Error -> if (phase.kind == ErrorKind.Unreachable) "offline" else "closed"
}

@OptIn(ExperimentalLayoutApi::class)
@Composable
fun TerminalScreen(
    ui: TerminalUi,
    emulator: PaneEmulator,
    tick: PaneTick,
    actions: TerminalActions,
    contentPadding: PaddingValues,
) {
    val c = St.colors
    val landscape = LocalConfiguration.current.orientation == Configuration.ORIENTATION_LANDSCAPE
    val view = rememberPaneViewState(fit = landscape)
    val ime = WindowInsets.isImeVisible
    val state = ui.state
    val phase = state.phase
    val sending = actions.copy(
        onSend = { enter -> view.jumpToLive(); actions.onSend(enter) },
        onKey = { key -> view.jumpToLive(); actions.onKey(key) },
    )
    Column(Modifier.fillMaxSize().background(c.bg).padding(contentPadding).consumeWindowInsets(contentPadding).imePadding().testTag("terminal")) {
        TopBar(ui, phase, actions.onBack)
        val surface = Color(TerminalPalette.BACKGROUND)
        Box(Modifier.weight(1f).fillMaxWidth().background(surface).semantics { this[SurfaceColor] = surface }.testTag("terminal-surface")) {
            when {
                phase == TerminalPhase.Opening -> OpeningPane(ui.hubName)
                phase is TerminalPhase.Error -> ErrorPane(phase.kind, ui, actions)
                else -> {
                    PaneView(emulator, tick, view, dimmed = phase is TerminalPhase.Ended)
                    Overlay(ui, phase, view, ime)
                    if (view.scrolledUp) JumpPill(view.newLines) { view.jumpToLive() }
                }
            }
        }
        if (phase !is TerminalPhase.Error) Controls(ui, sending, ime, landscape)
    }
}

@Composable
private fun TopBar(ui: TerminalUi, phase: TerminalPhase, onBack: () -> Unit) {
    val c = St.colors
    val ended = phase is TerminalPhase.Ended
    val word = if (ended) AgentWord.Exited else ui.word
    Column {
        Row(Modifier.fillMaxWidth().padding(start = 4.dp, end = 10.dp, top = 2.dp, bottom = 6.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(4.dp)) {
            IconButton(onClick = onBack, modifier = Modifier.testTag("terminal-back")) { Icon(PhosphorIcons.Regular.ArrowLeft, "Back", tint = c.text, modifier = Modifier.size(22.dp)) }
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(1.dp)) {
                Text(ui.title, fontSize = 15.sp, fontWeight = FontWeight.Medium, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.testTag("terminal-title"))
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    Box(Modifier.size(12.dp), contentAlignment = Alignment.Center) {
                        when (word) {
                            AgentWord.Running -> RunningArc()
                            AgentWord.Waiting -> Box(Modifier.size(7.dp).background(c.block, CircleShape))
                            AgentWord.Idle -> Box(Modifier.size(7.dp).border(1.5.dp, c.mute, CircleShape))
                            AgentWord.Exited -> Box(Modifier.size(6.dp).background(c.dim, CircleShape))
                        }
                    }
                    Text(
                        word.text,
                        fontSize = 11.5.sp,
                        fontWeight = FontWeight.Medium,
                        color = when (word) {
                            AgentWord.Waiting -> c.block
                            AgentWord.Exited -> c.mute
                            else -> c.text
                        },
                        modifier = Modifier.testTag("terminal-status"),
                    )
                    Text("·", fontSize = 11.5.sp, color = c.dim)
                    Text(ui.where, fontFamily = Mono, fontSize = 11.sp, color = c.mute, maxLines = 1, overflow = TextOverflow.Ellipsis)
                }
            }
            ConnChip(phase)
        }
        Box(Modifier.fillMaxWidth().height(1.dp).background(c.line))
    }
}

@Composable
private fun ConnChip(phase: TerminalPhase) {
    val c = St.colors
    val label = connLabel(phase)
    val reconnecting = phase == TerminalPhase.Reconnecting || phase == TerminalPhase.Opening
    Row(
        Modifier.height(28.dp).border(1.dp, c.line, RoundedCornerShape(14.dp)).padding(horizontal = 10.dp).semantics(mergeDescendants = true) {}.testTag("conn-chip"),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(6.dp),
    ) {
        when {
            reconnecting -> Pulse()
            label == "live" -> Box(Modifier.size(6.dp).background(c.ok, CircleShape))
            else -> Box(Modifier.size(6.dp).border(1.25.dp, c.mute, CircleShape))
        }
        Text(label, fontSize = 11.5.sp, color = if (reconnecting) c.run else c.mute)
    }
}

@Composable
private fun Pulse() {
    val t = rememberInfiniteTransition(label = "pulse")
    val a by t.animateFloat(1f, 0.3f, infiniteRepeatable(tween(600), RepeatMode.Reverse), label = "alpha")
    Box(Modifier.size(6.dp).alpha(a).background(St.colors.run, CircleShape))
}

/** Terminal-surface tokens: the same dark values in both app themes. */
private object Term {
    val fg = Color(TerminalPalette.FOREGROUND)
    val chip = Color(0xFF1C1D25)
    val ring = Color(0x24ECECF2)
    val white = Color(TerminalPalette.ansi(7))
    val dimText = Color(TerminalPalette.ansi(8))
}

@Composable
private fun BoxScope.Overlay(ui: TerminalUi, phase: TerminalPhase, view: PaneViewState, ime: Boolean) {
    Row(Modifier.align(Alignment.TopEnd).padding(6.dp), horizontalArrangement = Arrangement.spacedBy(4.dp), verticalAlignment = Alignment.CenterVertically) {
        if (phase is TerminalPhase.Live && phase.catchingUp) TermChip("Catching up…", spin = true, tag = "catching-up")
        if (phase is TerminalPhase.Live && !ime) TermChip("Also open on desktop", icon = PhosphorIcons.Regular.Desktop)
        TermChip(sizeLabel(ui.cols, ui.rows, view.fit), tag = "size-chip", onClick = { view.toggleFit() })
    }
}

@Composable
private fun TermChip(label: String, icon: ImageVector? = null, spin: Boolean = false, tag: String? = null, onClick: (() -> Unit)? = null) {
    val shape = RoundedCornerShape(12.dp)
    var m = Modifier.height(24.dp).clip(shape).background(Term.chip).border(1.dp, Term.ring, shape)
    if (onClick != null) m = m.clickable(role = Role.Button, onClick = onClick)
    if (tag != null) m = m.testTag(tag)
    Row(m.padding(horizontal = 9.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(5.dp)) {
        if (spin) RunningArc()
        if (icon != null) Icon(icon, null, tint = Term.white, modifier = Modifier.size(12.dp))
        Text(label, fontFamily = Mono, fontSize = 11.sp, fontWeight = FontWeight.Medium, color = Term.white)
    }
}

@Composable
private fun BoxScope.JumpPill(newLines: Int, onClick: () -> Unit) {
    val c = St.colors
    val shape = RoundedCornerShape(17.dp)
    Row(
        Modifier.align(Alignment.BottomCenter).padding(bottom = 10.dp).height(34.dp).clip(shape)
            .background(c.ind).border(1.dp, c.acc, shape).clickable(onClick = onClick).padding(horizontal = 14.dp).semantics(mergeDescendants = true) {}.testTag("jump-live"),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(7.dp),
    ) {
        Icon(PhosphorIcons.Bold.ArrowDownBold, null, tint = c.accInk, modifier = Modifier.size(14.dp))
        Text("Jump to live", fontSize = 13.sp, fontWeight = FontWeight.Medium, color = c.accInk)
        if (newLines > 0) Text("· $newLines new", fontFamily = Mono, fontSize = 11.5.sp, color = c.accInk.copy(alpha = 0.8f))
    }
}

@Composable
private fun OpeningPane(hubName: String) {
    val widths = listOf(0.62f, 0.38f, 0f, 0.8f, 0.3f, 0.56f, 0f, 0.72f, 0.44f, 0.66f, 0f, 0.5f, 0.84f, 0.28f)
    Box(Modifier.fillMaxSize().testTag("terminal-opening")) {
        Column(Modifier.fillMaxSize().padding(horizontal = 10.dp, vertical = 14.dp), verticalArrangement = Arrangement.spacedBy(7.dp, Alignment.Bottom)) {
            widths.forEach { w -> Box(Modifier.fillMaxWidth(w.coerceAtLeast(0.001f)).height(6.dp).alpha(if (w == 0f) 0f else 1f).clip(RoundedCornerShape(3.dp)).background(Term.chip)) }
        }
        Column(Modifier.align(Alignment.Center), horizontalAlignment = Alignment.CenterHorizontally, verticalArrangement = Arrangement.spacedBy(10.dp)) {
            Row(
                Modifier.clip(RoundedCornerShape(20.dp)).background(St.colors.surf2).padding(horizontal = 16.dp, vertical = 10.dp),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(10.dp),
            ) {
                RunningArc()
                Text("Connecting to $hubName…", fontSize = 13.5.sp)
            }
            Text("encrypted · pinned", fontFamily = Mono, fontSize = 11.sp, color = Term.dimText)
        }
    }
}

private data class ErrorCopy(val icon: ImageVector, val alert: Boolean, val title: String, val body: String, val detail: String?, val retry: Boolean)

private fun errorCopy(kind: ErrorKind, ui: TerminalUi): ErrorCopy = when (kind) {
    ErrorKind.Revoked -> ErrorCopy(PhosphorIcons.Regular.LockKey, true, "Terminal access was revoked", "${ui.hubName} removed Watch and Control for this phone. Session status keeps updating.", null, false)
    ErrorKind.Unreachable -> ErrorCopy(PhosphorIcons.Regular.Plugs, false, "Can't reach ${ui.hubName}", "The stream stopped and no address answers. Nothing you typed was sent.", ui.unreachableDetail, true)
    ErrorKind.Safety -> ErrorCopy(PhosphorIcons.Regular.ShieldWarning, true, "Terminal closed for safety", "This pane no longer belongs to the agent, so SessionTap stopped showing it.", null, false)
    ErrorKind.SourceRefused -> ErrorCopy(PhosphorIcons.Regular.LockSimple, false, "This source doesn't share its terminals", "${ui.sourceName} on ${ui.hubName} doesn't allow terminal access. Session status keeps updating.", null, false)
    ErrorKind.Unavailable -> ErrorCopy(PhosphorIcons.Regular.TerminalWindow, false, "Terminal isn't available", "The agent has no live terminal right now.", null, false)
}

@Composable
private fun ErrorPane(kind: ErrorKind, ui: TerminalUi, actions: TerminalActions) {
    val c = St.colors
    val e = errorCopy(kind, ui)
    Column(Modifier.fillMaxSize().background(c.bg).padding(28.dp).testTag("terminal-error"), verticalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterVertically)) {
        Icon(e.icon, null, tint = if (e.alert) c.block else c.mute, modifier = Modifier.size(32.dp))
        Text(e.title, fontSize = 20.sp, fontWeight = FontWeight.Medium, modifier = Modifier.padding(top = 6.dp))
        Text(e.body, fontSize = 13.5.sp, color = c.mute)
        e.detail?.let { Text(it, fontFamily = Mono, fontSize = 11.5.sp, color = c.mute) }
        Row(Modifier.padding(top = 14.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            if (e.retry) {
                PrimaryButton("Retry", actions.onRetry, height = 44.dp)
                SecondaryButton("Back to session", actions.onBack, height = 44.dp)
            } else {
                PrimaryButton("Back to session", actions.onBack, height = 44.dp)
            }
        }
    }
}

@Composable
private fun Controls(ui: TerminalUi, actions: TerminalActions, ime: Boolean, landscape: Boolean) {
    val c = St.colors
    val phase = ui.state.phase
    Column(Modifier.fillMaxWidth().background(c.surf).semantics { this[SurfaceColor] = c.surf }.testTag("controls")) {
        CompositionLocalProvider(LocalKey provides actions.onKey) {
            Box(Modifier.fillMaxWidth().height(1.dp).background(c.line))
            when {
                phase is TerminalPhase.Ended -> EndCard(phase, ui, actions)
                !ui.state.control -> WatchStrip(ui.hubName)
                else -> {
                    val live = phase as? TerminalPhase.Live
                    val enabled = live?.input == InputMode.Enabled
                    Banner(ui, live)
                    val note = if (phase == TerminalPhase.Reconnecting) "Reconnecting — your reply stays here and sends only when you tap Send" else null
                    if (landscape) {
                        Row(verticalAlignment = Alignment.Bottom) {
                            KeyBar(enabled, ui.ctrlArmed, actions, Modifier.weight(1f).padding(horizontal = 8.dp, vertical = 6.dp))
                            ReplyRow(ui, enabled, note, actions, ime, Modifier.width(360.dp).padding(top = 6.dp, bottom = 6.dp, end = 8.dp))
                        }
                    } else if (ime) {
                        ReplyRow(ui, enabled, note, actions, ime, Modifier.padding(start = 8.dp, end = 8.dp, top = 8.dp, bottom = 2.dp))
                        KeyBar(enabled, ui.ctrlArmed, actions, Modifier.padding(start = 8.dp, end = 8.dp, top = 4.dp, bottom = 8.dp))
                    } else {
                        KeyBar(enabled, ui.ctrlArmed, actions, Modifier.padding(start = 8.dp, end = 8.dp, top = 8.dp, bottom = 6.dp))
                        ReplyRow(ui, enabled, note, actions, ime, Modifier.padding(start = 8.dp, end = 8.dp, top = 2.dp, bottom = 8.dp))
                    }
                    ui.error?.let { Text(it, fontSize = 12.sp, color = c.block, modifier = Modifier.padding(start = 16.dp, end = 16.dp, bottom = 8.dp).testTag("input-error")) }
                }
            }
        }
    }
}

@Composable
private fun Banner(ui: TerminalUi, live: TerminalPhase.Live?) {
    val input = live?.input ?: return
    when (input) {
        is InputMode.Paused -> when (input.reason) {
            InputUnavailable.NotForeground -> CalmBanner(PhosphorIcons.Regular.PauseCircle, "Agent isn't in the foreground — input paused", "Input comes back when the agent is in front again.", "banner-paused")
            InputUnavailable.PaneInMode -> CalmBanner(PhosphorIcons.Regular.MouseScroll, "Desktop is scrolling this pane — input paused", "Resumes when they leave scroll mode.", "banner-scrolling")
        }
        InputMode.Enabled -> when {
            ui.blocked == ReasonKind.Input -> AskBanner("Agent is asking — reply below", null, null, emptyList(), "banner-question")
            ui.blocked == ReasonKind.Approval -> AskBanner(
                "Agent wants approval",
                if (ui.digits) "Tap a number, or ↑ ↓ then Enter" else "↑ ↓ then Enter",
                "Space toggles · Enter confirms",
                if (ui.digits) listOf("1", "2", "3", "4") else emptyList(),
                "banner-approval",
            )
            ui.blockedOther -> AskBanner("Agent is asking", "Space toggles · Enter confirms", null, emptyList(), "banner-asking")
            else -> {}
        }
        InputMode.WatchOnly -> {}
    }
}

/** Provided by [Controls] so digit chips reach the key callback. */
private val LocalKey = staticCompositionLocalOf<(String) -> Unit> { {} }

@Composable
private fun AskBanner(title: String, sub: String?, hint: String?, chips: List<String>, tag: String) {
    val c = St.colors
    val onKey = LocalKey.current
    val shape = RoundedCornerShape(12.dp)
    FlowRow(
        Modifier.padding(start = 8.dp, end = 8.dp, top = 8.dp).fillMaxWidth().defaultMinSize(minHeight = 44.dp).clip(shape)
            .background(c.blockTint).border(1.dp, c.block.copy(alpha = 0.3f), shape).padding(start = 12.dp, end = 8.dp, top = 8.dp, bottom = 8.dp).testTag(tag),
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterVertically),
    ) {
        Icon(PhosphorIcons.Regular.ChatCircleDots, null, tint = c.block, modifier = Modifier.size(18.dp).align(Alignment.CenterVertically))
        Column(Modifier.weight(1f).align(Alignment.CenterVertically)) {
            Text(title, fontSize = 13.sp, fontWeight = FontWeight.Medium)
            sub?.let { Text(it, fontSize = 12.sp, color = c.mute) }
            hint?.let { Text(it, fontSize = 12.sp, color = c.mute) }
        }
        if (chips.isNotEmpty()) {
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp), modifier = Modifier.align(Alignment.CenterVertically)) {
                chips.forEach { n ->
                    val chipShape = RoundedCornerShape(10.dp)
                    Box(
                        Modifier.size(44.dp).clip(chipShape).background(c.bg).border(1.dp, c.line, chipShape)
                            .clickable(role = Role.Button) { onKey(n) }.testTag("digit:$n"),
                        contentAlignment = Alignment.Center,
                    ) { Text(n, fontFamily = Mono, fontSize = 16.sp, fontWeight = FontWeight.SemiBold, color = c.accInk) }
                }
            }
        }
    }
}

@Composable
private fun CalmBanner(icon: ImageVector, title: String, sub: String, tag: String) {
    val c = St.colors
    val shape = RoundedCornerShape(12.dp)
    Row(
        Modifier.padding(start = 8.dp, end = 8.dp, top = 8.dp).fillMaxWidth().heightIn(min = 44.dp).clip(shape)
            .background(c.bg).border(1.dp, c.line, shape).padding(start = 12.dp, end = 8.dp, top = 8.dp, bottom = 8.dp).testTag(tag),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Icon(icon, null, tint = c.mute, modifier = Modifier.size(18.dp))
        Column(Modifier.weight(1f)) {
            Text(title, fontSize = 13.sp, fontWeight = FontWeight.Medium)
            Text(sub, fontSize = 12.sp, color = c.mute)
        }
    }
}

private data class KeyDef(val id: String, val label: String, val icon: ImageVector?, val width: Dp, val arrow: Boolean = false)

private val KEYS = listOf(
    KeyDef(TerminalKeys.ESCAPE, "Esc", null, 44.dp),
    KeyDef(TerminalKeys.TAB, "Tab", null, 44.dp),
    KeyDef(TerminalKeys.BACK_TAB, "⇧Tab", null, 54.dp),
    KeyDef(TerminalKeys.UP, "", PhosphorIcons.Bold.ArrowUpBold, 50.dp, arrow = true),
    KeyDef(TerminalKeys.DOWN, "", PhosphorIcons.Bold.ArrowDownBold, 50.dp, arrow = true),
    KeyDef(TerminalKeys.LEFT, "", PhosphorIcons.Bold.ArrowLeftBold, 50.dp, arrow = true),
    KeyDef(TerminalKeys.RIGHT, "", PhosphorIcons.Bold.ArrowRightBold, 50.dp, arrow = true),
    KeyDef(TerminalKeys.ENTER, "Enter", PhosphorIcons.Regular.ArrowElbowDownLeft, 76.dp),
    KeyDef(TerminalKeys.SPACE, "Space", null, 64.dp),
    KeyDef(TerminalKeys.BACKSPACE, "", PhosphorIcons.Regular.Backspace, 50.dp),
    KeyDef(TerminalKeys.CTRL_C, "Ctrl+C", null, 68.dp),
    KeyDef(PASTE, "Paste", PhosphorIcons.Regular.ClipboardText, 76.dp),
)

/** Key-bar id of the Paste key; it never reaches the agent. */
const val PASTE = "paste"

private val KEY_NAMES = mapOf(
    TerminalKeys.ESCAPE to "Esc", TerminalKeys.TAB to "Tab", TerminalKeys.BACK_TAB to "Shift+Tab",
    TerminalKeys.UP to "Up", TerminalKeys.DOWN to "Down", TerminalKeys.LEFT to "Left", TerminalKeys.RIGHT to "Right",
    TerminalKeys.BACKSPACE to "Backspace",
)

@Composable
private fun KeyBar(enabled: Boolean, ctrlArmed: Boolean, actions: TerminalActions, modifier: Modifier) {
    val c = St.colors
    val raised = androidx.compose.ui.graphics.lerp(c.surf2, c.text, 0.18f)
    Row(modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).testTag("key-bar"), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
        KEYS.forEach { k ->
            val armed = k.id == TerminalKeys.CTRL_C && ctrlArmed
            val shape = RoundedCornerShape(10.dp)
            val bg = when {
                armed -> c.blockTint
                k.arrow -> raised
                else -> c.surf2
            }
            val color = if (k.id == TerminalKeys.CTRL_C) c.block else c.text
            Row(
                Modifier.height(44.dp).defaultMinSize(minWidth = if (armed) 108.dp else k.width).clip(shape).background(bg)
                    .border(1.dp, if (armed) c.block else c.line, shape)
                    .alpha(if (enabled) 1f else 0.38f)
                    .clickable(enabled = enabled, role = Role.Button) { if (k.id == PASTE) actions.onPaste() else actions.onKey(k.id) }
                    .padding(horizontal = 10.dp)
                    .testTag("key:${k.id}"),
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(6.dp, Alignment.CenterHorizontally),
            ) {
                val icon = if (armed) PhosphorIcons.Bold.Warning else k.icon
                val label = if (armed) "Tap again" else k.label
                if (icon != null) Icon(icon, KEY_NAMES[k.id].takeIf { label.isEmpty() }, tint = color, modifier = Modifier.size(17.dp))
                if (label.isNotEmpty()) Text(label, fontFamily = Mono, fontSize = 13.sp, fontWeight = FontWeight.Medium, color = color)
            }
        }
    }
}

@OptIn(ExperimentalFoundationApi::class)
@Composable
private fun ReplyRow(ui: TerminalUi, enabled: Boolean, note: String?, actions: TerminalActions, focused: Boolean, modifier: Modifier) {
    val c = St.colors
    val haptic = LocalHapticFeedback.current
    Column(modifier, verticalArrangement = Arrangement.spacedBy(4.dp)) {
        note?.let {
            Row(Modifier.padding(horizontal = 6.dp).testTag("reconnect-note"), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                Pulse()
                Text(it, fontSize = 11.5.sp, color = c.mute)
            }
        }
        Row(verticalAlignment = Alignment.Bottom, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            val shape = RoundedCornerShape(22.dp)
            Box(
                Modifier.weight(1f).clip(shape).background(c.surf2).border(if (focused) 1.5.dp else 1.dp, if (focused) c.acc else c.line, shape)
                    .alpha(if (enabled) 1f else 0.45f).padding(horizontal = 16.dp, vertical = 12.dp),
            ) {
                if (ui.reply.isEmpty()) Text("Reply to agent…", fontSize = 14.sp, color = c.mute)
                BasicTextField(
                    ui.reply,
                    actions.onReply,
                    enabled = enabled,
                    maxLines = 4,
                    textStyle = TextStyle(fontSize = 14.sp, lineHeight = 20.sp, color = c.text),
                    cursorBrush = SolidColor(c.acc),
                    modifier = Modifier.fillMaxWidth().testTag("reply"),
                )
            }
            Box(
                Modifier.size(44.dp).clip(CircleShape).background(if (enabled) c.accTint else Color.Transparent).border(1.5.dp, c.acc, CircleShape)
                    .alpha(if (enabled) 1f else 0.45f)
                    .combinedClickable(
                        enabled = enabled,
                        role = Role.Button,
                        onLongClickLabel = "Send without Enter",
                        onLongClick = {
                            haptic.performHapticFeedback(HapticFeedbackType.LongPress)
                            actions.onSend(false)
                        },
                        onClick = { actions.onSend(true) },
                    )
                    .testTag("send"),
                contentAlignment = Alignment.Center,
            ) { Icon(PhosphorIcons.Bold.ArrowUpBold, "Send", tint = c.acc, modifier = Modifier.size(18.dp)) }
        }
    }
}

@Composable
private fun WatchStrip(hubName: String) {
    val c = St.colors
    var open by remember { mutableStateOf(false) }
    Column(Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 14.dp).testTag("watch-strip"), verticalArrangement = Arrangement.spacedBy(6.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Icon(PhosphorIcons.Regular.Eye, null, tint = c.mute, modifier = Modifier.size(19.dp))
            Text("View only — this phone can't type into agents on $hubName", fontSize = 13.sp, modifier = Modifier.weight(1f))
            Text(
                "How to allow",
                fontSize = 13.sp,
                fontWeight = FontWeight.Medium,
                color = c.accInk,
                modifier = Modifier.clickable { open = !open }.padding(horizontal = 4.dp, vertical = 10.dp).testTag("how-to-allow"),
            )
        }
        if (open) Text("Pair this phone again with sessiontap-hub pair --scope control on $hubName.", fontSize = 12.sp, color = c.mute, modifier = Modifier.padding(start = 31.dp))
    }
}

private data class EndCopy(val icon: ImageVector, val ok: Boolean, val title: String, val sub: String)

fun endTitle(kind: EndKind): String = when (kind) {
    EndKind.AgentExited -> "Agent exited"
    EndKind.PaneClosed -> "Pane closed on desktop"
    EndKind.TmuxStopped -> "tmux server stopped"
}

private fun endCopy(kind: EndKind, hub: String): EndCopy = when (kind) {
    EndKind.AgentExited -> EndCopy(PhosphorIcons.Regular.CheckCircle, true, endTitle(kind), "The agent exited. This is its last frame.")
    EndKind.PaneClosed -> EndCopy(PhosphorIcons.Regular.XSquare, false, endTitle(kind), "The tmux pane was closed on $hub, and the agent ended with it.")
    EndKind.TmuxStopped -> EndCopy(PhosphorIcons.Regular.Power, false, endTitle(kind), "The tmux server on $hub shut down, ending every agent it hosted.")
}

@Composable
private fun EndCard(phase: TerminalPhase.Ended, ui: TerminalUi, actions: TerminalActions) {
    val c = St.colors
    val e = endCopy(phase.kind, ui.hubName)
    val after = ui.startedAt?.let { started ->
        val minutes = ((phase.atMs - started.toEpochMilli()) / 60_000).coerceAtLeast(0)
        if (minutes >= 60) " · after ${minutes / 60}h ${minutes % 60}m" else " · after ${minutes}m"
    }.orEmpty()
    Box(Modifier.padding(start = 8.dp, end = 8.dp, top = 8.dp, bottom = 4.dp)) {
        Column(
            Modifier.fillMaxWidth().clip(RoundedCornerShape(20.dp)).background(c.surf2).border(1.dp, c.line, RoundedCornerShape(20.dp)).padding(16.dp).testTag("end-card"),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                Icon(e.icon, null, tint = if (e.ok) c.ok else c.mute, modifier = Modifier.size(20.dp))
                Text(e.title, fontSize = 15.sp, fontWeight = FontWeight.Medium, modifier = Modifier.weight(1f))
                Text(phase.atMs.clock() + after, fontFamily = Mono, fontSize = 11.sp, color = c.mute)
            }
            Text(e.sub, fontSize = 12.5.sp, color = c.mute, modifier = Modifier.padding(start = 30.dp))
            Row(Modifier.padding(top = 10.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                PrimaryButton("Back to session", actions.onBack, Modifier.weight(1f).testTag("end-back"), height = 44.dp)
                SecondaryButton("Copy last screen", actions.onCopyScreen, Modifier.weight(1f).testTag("copy-screen"), height = 44.dp, icon = PhosphorIcons.Regular.Copy)
            }
        }
    }
    Spacer(Modifier.height(4.dp))
}
