package dev.sessiontap.android.ui.terminal

import android.content.ClipboardManager
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.luminance
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onAllNodesWithText
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeDown
import androidx.compose.ui.test.doubleClick
import androidx.compose.ui.test.assertTextContains
import androidx.test.core.app.ApplicationProvider
import dev.sessiontap.android.net.EndReason
import dev.sessiontap.android.net.InputState
import dev.sessiontap.android.net.InputUnavailable
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.TerminalErrors
import dev.sessiontap.android.ui.components.copyText
import dev.sessiontap.android.ui.theme.SessionTapTheme
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import org.robolectric.annotation.GraphicsMode

/** Compose UI test; debug only, where ui-test-manifest provides the host activity. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35], qualifiers = "w411dp-h891dp-port")
// real font metrics, so the pane has a line height to scroll by
@GraphicsMode(GraphicsMode.Mode.NATIVE)
class TerminalScreenTest {
    @get:Rule
    val rule = createComposeRule()

    private val live = TerminalState(control = true).let { reduce(reduce(it, TerminalEvent.Opening), TerminalEvent.Snapshot(InputState(true))) }

    private fun ui(
        state: TerminalState = live,
        blocked: ReasonKind? = null,
        digits: Boolean = true,
        reply: String = "",
        ctrlArmed: Boolean = false,
    ) = TerminalUi(
        title = "Fix flaky auth tests",
        where = "api · feat/auth-retry · MacBook",
        hubName = "MacBook",
        sourceName = "sandbox",
        word = if (blocked != null) AgentWord.Waiting else AgentWord.Running,
        blocked = blocked,
        blockedOther = false,
        digits = digits,
        state = state,
        reply = reply,
        error = null,
        ctrlArmed = ctrlArmed,
        cols = 120,
        rows = 40,
        startedAt = null,
    )

    private fun pane(text: String = "Do you want to proceed?") = PaneEmulator().apply { reset(snapshotFrame(text)) }

    private fun show(ui: TerminalUi, actions: TerminalActions = TerminalActions(), dark: Boolean = true, emulator: PaneEmulator = pane(), tick: PaneTick = PaneTick()) {
        rule.setContent { SessionTapTheme(dark = dark) { TerminalScreen(ui, emulator, tick, actions, PaddingValues()) } }
    }

    @Test
    fun topBarShowsSessionStatusWhereAndConnection() {
        show(ui(blocked = ReasonKind.Approval))
        rule.onNodeWithTag("terminal-title").assertTextContains("Fix flaky auth tests")
        rule.onNodeWithTag("terminal-status").assertTextContains("waiting for you")
        rule.onNodeWithText("api · feat/auth-retry · MacBook").assertExists()
        rule.onNodeWithTag("conn-chip").assertTextContains("live", substring = true)
        rule.onNodeWithTag("size-chip").assertTextContains("120×40 · 9sp", substring = true)
        rule.onNodeWithText("Also open on desktop").assertExists()
    }

    @Test
    fun lightThemeKeepsTerminalDark() {
        show(ui(), dark = false)
        fun color(tag: String) = rule.onNodeWithTag(tag).fetchSemanticsNode().config[SurfaceColor]
        assertTrue(color("terminal-surface").luminance() < 0.05f)
        assertTrue(color("controls").luminance() > 0.8f)
        // the pane itself draws on the same fixed palette in both themes
        assertEquals(TerminalPalette.BACKGROUND, PaneEmulator().apply { reset(snapshotFrame("x")) }.emulator!!.mColors.mCurrentColors[com.termux.terminal.TextStyle.COLOR_INDEX_BACKGROUND])
    }

    @Test
    fun sizeChipAndDoubleTapToggleFit() {
        show(ui())
        rule.onNodeWithTag("size-chip").performClick()
        rule.onNodeWithTag("size-chip").assertTextContains("120×40 · fit", substring = true)
        rule.onNodeWithTag("pane").performTouchInput { doubleClick() }
        rule.onNodeWithTag("size-chip").assertTextContains("120×40 · 9sp", substring = true)
    }

    @Test
    fun scrolledUpShowsJumpToLiveWithNewLines() {
        val emulator = PaneEmulator().apply { reset(snapshotFrame((1..300).joinToString("\r\n") { "line $it" }, cursor = dev.sessiontap.android.net.TerminalCursor(8, 39))) }
        var tick by mutableStateOf(PaneTick(emulator.version, 0))
        rule.setContent { SessionTapTheme { TerminalScreen(ui(), emulator, tick, TerminalActions(), PaddingValues()) } }
        rule.onNodeWithTag("jump-live").assertDoesNotExist()
        rule.onNodeWithTag("pane").performTouchInput { swipeDown() }
        rule.onNodeWithTag("jump-live").assertExists()
        val lines = emulator.append((1..12).joinToString("") { "\r\nnew $it" }.toByteArray())
        tick = PaneTick(emulator.version, lines.toLong())
        rule.onNodeWithTag("jump-live").assertTextContains("12 new", substring = true)
        rule.onNodeWithTag("jump-live").performClick()
        rule.onNodeWithTag("jump-live").assertDoesNotExist()
    }

    @Test
    fun keyTapsSendNamedKeys() {
        val keys = mutableListOf<String>()
        show(ui(), TerminalActions(onKey = { keys += it }))
        rule.onNodeWithTag("key:down").performClick()
        rule.onNodeWithTag("key:enter").performClick()
        rule.onNodeWithTag("key:ctrl_c").performScrollTo().performClick()
        assertEquals(listOf("down", "enter", "ctrl_c"), keys)
    }

    @Test
    fun ctrlCArmedAsksForSecondTap() {
        show(ui(ctrlArmed = true))
        rule.onNodeWithTag("key:ctrl_c", useUnmergedTree = false).performScrollTo()
        rule.onNodeWithText("Tap again").assertExists()
    }

    @Test
    fun pasteKeyGoesToReplyField() {
        var pasted = 0
        val keys = mutableListOf<String>()
        show(ui(), TerminalActions(onPaste = { pasted++ }, onKey = { keys += it }))
        rule.onNodeWithTag("key:paste").performScrollTo().performClick()
        assertEquals(1, pasted)
        assertTrue(keys.isEmpty())
    }

    @Test
    fun sendAndLongPressSend() {
        val sends = mutableListOf<Boolean>()
        show(ui(reply = "see CI run 4821"), TerminalActions(onSend = { sends += it }))
        rule.onNodeWithTag("send").performClick()
        rule.onNodeWithTag("send").performTouchInput { longClick() }
        assertEquals(listOf(true, false), sends)
    }

    @Test
    fun approvalBannerWithDigitChips() {
        val keys = mutableListOf<String>()
        show(ui(blocked = ReasonKind.Approval), TerminalActions(onKey = { keys += it }))
        rule.onNodeWithTag("banner-approval").assertExists()
        rule.onNodeWithText("Space toggles · Enter confirms").assertExists()
        listOf("1", "2", "3", "4").forEach { rule.onNodeWithTag("digit:$it").assertExists() }
        rule.onNodeWithTag("digit:1").performClick()
        assertEquals(listOf("1"), keys)
    }

    @Test
    fun approvalWithoutDigitsHasNoChips() {
        show(ui(blocked = ReasonKind.Approval, digits = false))
        rule.onNodeWithTag("banner-approval").assertExists()
        rule.onAllNodesWithTag("digit:1").assertCountEquals(0)
    }

    @Test
    fun questionBannerHasNoChips() {
        show(ui(blocked = ReasonKind.Input))
        rule.onNodeWithText("Agent is asking — reply below").assertExists()
        rule.onAllNodesWithTag("digit:1").assertCountEquals(0)
    }

    @Test
    fun pausedBannersGreyControlsAndKeepReply() {
        val keys = mutableListOf<String>()
        val paused = reduce(live, TerminalEvent.Input(InputState(false, InputUnavailable.NotForeground)))
        show(ui(state = paused, blocked = ReasonKind.Approval, reply = "kept"), TerminalActions(onKey = { keys += it }))
        rule.onNodeWithTag("banner-paused").assertExists()
        rule.onAllNodesWithTag("digit:1").assertCountEquals(0)
        rule.onNodeWithTag("conn-chip").assertTextContains("input paused", substring = true)
        rule.onNodeWithTag("key:enter").assertExists().performClick()
        rule.onNodeWithTag("reply").assertTextContains("kept")
        assertTrue(keys.isEmpty())
    }

    @Test
    fun scrollModeBanner() {
        show(ui(state = reduce(live, TerminalEvent.Input(InputState(false, InputUnavailable.PaneInMode)))))
        rule.onNodeWithText("Desktop is scrolling this pane — input paused").assertExists()
    }

    @Test
    fun watchOnlyHasStripAndNoInput() {
        val watch = reduce(reduce(TerminalState(control = false), TerminalEvent.Opening), TerminalEvent.Snapshot(InputState(true)))
        show(ui(state = watch))
        rule.onNodeWithTag("watch-strip").assertExists()
        rule.onNodeWithText("View only — this phone can't type into agents on MacBook").assertExists()
        rule.onAllNodesWithTag("key-bar").assertCountEquals(0)
        rule.onAllNodesWithTag("reply").assertCountEquals(0)
        rule.onNodeWithTag("pane").assertExists()
    }

    @Test
    fun forbiddenInputSwitchesToWatchOnly() {
        show(ui(state = reduce(live, TerminalEvent.InputFailed(TerminalErrors.FORBIDDEN))))
        rule.onNodeWithTag("watch-strip").assertExists()
        rule.onAllNodesWithTag("key-bar").assertCountEquals(0)
    }

    @Test
    fun reconnectingKeepsReplyWithNote() {
        show(ui(state = reduce(live, TerminalEvent.ConnectionLost), reply = "use the backoff helper"))
        rule.onNodeWithTag("reconnect-note").assertExists()
        rule.onNodeWithTag("reply").assertTextContains("use the backoff helper")
        rule.onNodeWithTag("conn-chip").assertTextContains("Reconnecting…", substring = true)
        rule.onNodeWithTag("pane").assertExists()
    }

    @Test
    fun catchingUpChip() {
        show(ui(state = reduce(live, TerminalEvent.Snapshot(InputState(true)))))
        rule.onNodeWithTag("catching-up").assertExists()
    }

    @Test
    fun openingNamesTheHub() {
        show(ui(state = reduce(TerminalState(control = true), TerminalEvent.Opening)))
        rule.onNodeWithText("Connecting to MacBook…").assertExists()
    }

    @Test
    fun endCardsAndCopyLastScreen() {
        val context = ApplicationProvider.getApplicationContext<android.app.Application>()
        val emulator = pane("You chose option 1")
        var backs = 0
        val ended = reduce(live, TerminalEvent.Ended(EndReason.AgentExited, 0))
        show(ui(state = ended), TerminalActions(onBack = { backs++ }, onCopyScreen = { copyText(context, emulator.screenText()) }), emulator = emulator)
        rule.onNodeWithTag("end-card").assertExists()
        rule.onNodeWithText("Agent exited").assertExists()
        rule.onAllNodesWithTag("key-bar").assertCountEquals(0)
        rule.onAllNodesWithTag("reply").assertCountEquals(0)
        rule.onNodeWithTag("conn-chip").assertTextContains("ended", substring = true)
        rule.onNodeWithTag("copy-screen").performClick()
        val clip = context.getSystemService(ClipboardManager::class.java).primaryClip!!
        assertEquals("You chose option 1", clip.getItemAt(0).text.toString())
        rule.onNodeWithTag("end-back").performClick()
        assertEquals(1, backs)
    }

    @Test
    fun otherEndCards() {
        var state by mutableStateOf(reduce(live, TerminalEvent.Ended(EndReason.PaneClosed, 0)))
        rule.setContent { SessionTapTheme { TerminalScreen(ui(state = state), pane(), PaneTick(), TerminalActions(), PaddingValues()) } }
        rule.onNodeWithText("Pane closed on desktop").assertExists()
        state = reduce(live, TerminalEvent.Ended(EndReason.MultiplexerStopped, 0))
        rule.onNodeWithText("tmux server stopped").assertExists()
    }

    @Test
    fun errorScreens() {
        var state by mutableStateOf(reduce(live, TerminalEvent.Revoked))
        rule.setContent { SessionTapTheme { TerminalScreen(ui(state = state), pane("claude-imposter"), PaneTick(), TerminalActions(), PaddingValues()) } }
        rule.onNodeWithText("Terminal access was revoked").assertExists()
        rule.onNodeWithText("Back to session").assertExists()
        rule.onAllNodesWithText("Retry").assertCountEquals(0)
        rule.onAllNodesWithTag("pane").assertCountEquals(0)

        state = reduce(reduce(live, TerminalEvent.ConnectionLost), TerminalEvent.Unreachable)
        rule.onNodeWithText("Can't reach MacBook").assertExists()
        rule.onNodeWithText("Retry").assertExists()

        state = reduce(TerminalState(control = true), TerminalEvent.OpenFailed(TerminalErrors.SOURCE_DISALLOWS_CONTROL))
        rule.onNodeWithText("This source doesn't share its terminals").assertExists()
        rule.onAllNodesWithTag("pane").assertCountEquals(0)

        state = reduce(live, TerminalEvent.Ended(EndReason.IdentityChanged, 0))
        rule.onNodeWithText("Terminal closed for safety").assertExists()
        rule.onAllNodesWithTag("pane").assertCountEquals(0)
        rule.onAllNodesWithText("claude-imposter", substring = true).assertCountEquals(0)
    }
}
