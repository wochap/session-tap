package dev.sessiontap.android.ui.terminal

import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.net.Connection
import dev.sessiontap.android.net.EndReason
import dev.sessiontap.android.net.InputState
import dev.sessiontap.android.net.RpcException
import dev.sessiontap.android.net.TerminalCursor
import dev.sessiontap.android.net.TerminalFrame
import dev.sessiontap.android.net.TerminalHub
import dev.sessiontap.android.net.TerminalInput
import dev.sessiontap.android.net.TerminalKeys
import dev.sessiontap.android.net.TerminalStream
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.advanceTimeBy
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Base64

/** A hub whose connection and streams the test drives by hand. */
class FakeTerminalHub : TerminalHub {
    override val state = MutableStateFlow<ConnState>(ConnState.Live("hub:8932"))
    val opens = mutableListOf<Pair<String, String>>()
    val inputs = mutableListOf<TerminalInput>()
    val closes = mutableListOf<Long>()
    var openError: String? = null
    var inputError: String? = null
    var inputAttempts = 0
    var kicks = 0
    lateinit var frames: Channel<TerminalFrame>
    private var nextId = 1L

    override suspend fun openTerminal(sourceId: String, invocationId: String): TerminalStream {
        opens += sourceId to invocationId
        openError?.let { throw RpcException(it, it) }
        frames = Channel(Channel.UNLIMITED)
        return TerminalStream(nextId++, frames, Connection("hub:8932", null))
    }

    override suspend fun sendInput(stream: TerminalStream, input: TerminalInput) {
        inputAttempts++
        inputError?.let { throw RpcException(it, it) }
        inputs += input
    }

    override suspend fun closeTerminal(stream: TerminalStream) {
        closes += stream.id
    }

    override fun kick() {
        kicks++
    }

    fun push(frame: TerminalFrame) = frames.trySend(frame)

    /** The connection drops: streams close and the client starts reconnecting. */
    fun drop() {
        frames.close()
        state.value = ConnState.Connecting
    }
}

fun snapshotFrame(
    text: String,
    cols: Int = 120,
    rows: Int = 40,
    input: InputState = InputState(true),
    // by default the cursor sits after the text, as tmux reports it for a one-line screen
    cursor: TerminalCursor = TerminalCursor(text.length, 0),
) = TerminalFrame.Snapshot(
    seq = 0,
    cols = cols,
    rows = rows,
    cursor = cursor,
    data = Base64.getEncoder().encodeToString(text.toByteArray()),
    input = input,
)

fun outputFrame(text: String) = TerminalFrame.Output(1, Base64.getEncoder().encodeToString(text.toByteArray()))

@OptIn(ExperimentalCoroutinesApi::class)
class TerminalViewModelTest {
    private val key = AgentKey("hub", "term", "inv-1")
    private val hub = FakeTerminalHub()
    private var scopes = listOf("read", "watch", "control")

    private fun TestScope.vm(control: Boolean = true) = TerminalViewModel(
        key = key,
        hub = { hub },
        scopes = { scopes },
        control = control,
        closeScope = this,
        now = { 1_000 },
        scope = backgroundScope,
    ).also {
        it.start()
        runCurrent()
    }

    private fun TestScope.live(control: Boolean = true): TerminalViewModel {
        val vm = vm(control)
        hub.push(snapshotFrame("Do you want to proceed?"))
        runCurrent()
        return vm
    }

    @Test
    fun opensAndRendersSnapshotThenOutput() = runTest {
        val vm = vm()
        assertEquals(listOf("term" to "inv-1"), hub.opens)
        assertEquals(TerminalPhase.Opening, vm.state.value.phase)
        hub.push(snapshotFrame("hello", cols = 132, rows = 38))
        hub.push(outputFrame(" world"))
        runCurrent()
        assertEquals(TerminalPhase.Live(InputMode.Enabled), vm.state.value.phase)
        assertEquals(132, vm.emulator.cols)
        assertEquals(38, vm.emulator.rows)
        assertEquals("hello world", vm.emulator.screenText())
    }

    @Test
    fun frozenQueuesOutputAndAppliesInOrderOnUnfreeze() = runTest {
        val vm = live()
        vm.freeze()
        hub.push(outputFrame("\r\none"))
        hub.push(outputFrame("\r\ntwo"))
        runCurrent()
        assertEquals("Do you want to proceed?", vm.emulator.screenText())
        assertEquals(2, vm.heldLines.value)
        vm.unfreeze()
        assertEquals("Do you want to proceed?\none\ntwo", vm.emulator.screenText())
        assertEquals(0, vm.heldLines.value)
        assertFalse(vm.frozen.value)
    }

    @Test
    fun snapshotWhileFrozenSupersedesQueuedOutput() = runTest {
        val vm = live()
        vm.freeze()
        hub.push(outputFrame(" stale"))
        hub.push(snapshotFrame("fresh"))
        hub.push(outputFrame(" after"))
        runCurrent()
        assertEquals("Do you want to proceed?", vm.emulator.screenText())
        vm.unfreeze()
        assertEquals("fresh after", vm.emulator.screenText())
    }

    @Test
    fun overflowWhileFrozenReopensForFreshSnapshot() = runTest {
        val vm = live()
        vm.freeze()
        val chunk = "x".repeat(64 * 1024)
        repeat(17) { hub.push(outputFrame(chunk)) }
        runCurrent()
        assertEquals(1, hub.opens.size)
        vm.unfreeze()
        runCurrent()
        assertEquals("Do you want to proceed?", vm.emulator.screenText())
        assertEquals(2, hub.opens.size)
        assertEquals(listOf(1L), hub.closes)
        assertTrue(vm.state.value.phase is TerminalPhase.Live)
        hub.push(snapshotFrame("reopened"))
        runCurrent()
        assertEquals("reopened", vm.emulator.screenText())
    }

    @Test
    fun laterSnapshotShowsCatchingUp() = runTest {
        val vm = live()
        hub.push(snapshotFrame("resynced"))
        runCurrent()
        assertEquals(TerminalPhase.Live(InputMode.Enabled, catchingUp = true), vm.state.value.phase)
        advanceTimeBy(1_000)
        assertEquals(TerminalPhase.Live(InputMode.Enabled), vm.state.value.phase)
    }

    @Test
    fun reconnectKeepsReplyAndReopensWithFreshSnapshot() = runTest {
        val vm = live()
        vm.reply.value = "use the backoff helper"
        hub.drop()
        runCurrent()
        assertEquals(TerminalPhase.Reconnecting, vm.state.value.phase)
        vm.sendReply(enter = true)
        runCurrent()
        assertTrue(hub.inputs.isEmpty())
        hub.state.value = ConnState.Live("hub:8932")
        advanceTimeBy(10_000)
        assertEquals(2, hub.opens.size)
        hub.push(snapshotFrame("fresh"))
        runCurrent()
        assertEquals(TerminalPhase.Live(InputMode.Enabled), vm.state.value.phase)
        assertEquals("fresh", vm.emulator.screenText())
        assertEquals("use the backoff helper", vm.reply.value)
        assertTrue(hub.inputs.isEmpty())
    }

    @Test
    fun reconnectWithoutWatchIsRevoked() = runTest {
        val vm = live()
        scopes = listOf("read", "manage")
        hub.drop()
        runCurrent()
        hub.state.value = ConnState.Live("hub:8932")
        advanceTimeBy(10_000)
        assertEquals(TerminalPhase.Error(ErrorKind.Revoked), vm.state.value.phase)
        assertEquals(1, hub.opens.size)
    }

    @Test
    fun reconnectWithWatchOnlyReopensReadOnly() = runTest {
        val vm = live()
        scopes = listOf("read", "watch")
        hub.drop()
        runCurrent()
        hub.state.value = ConnState.Live("hub:8932")
        advanceTimeBy(10_000)
        hub.push(snapshotFrame("fresh"))
        runCurrent()
        assertEquals(TerminalPhase.Live(InputMode.WatchOnly), vm.state.value.phase)
    }

    @Test
    fun revokedDeviceShowsRevoked() = runTest {
        val vm = live()
        hub.drop()
        hub.state.value = ConnState.Revoked
        advanceTimeBy(10_000)
        assertEquals(TerminalPhase.Error(ErrorKind.Revoked), vm.state.value.phase)
    }

    @Test
    fun offlineHubShowsUnreachable() = runTest {
        val vm = live()
        hub.drop()
        hub.state.value = ConnState.Reconnecting(0, ConnState.OFFLINE_AFTER_FAILURES, "timeout")
        advanceTimeBy(10_000)
        assertEquals(TerminalPhase.Error(ErrorKind.Unreachable), vm.state.value.phase)
        hub.state.value = ConnState.Live("hub:8932")
        advanceTimeBy(10_000)
        assertEquals(2, hub.opens.size)
    }

    @Test
    fun sourceRefusalOnOpen() = runTest {
        hub.openError = "source_disallows_control"
        val vm = vm()
        assertEquals(TerminalPhase.Error(ErrorKind.SourceRefused), vm.state.value.phase)
    }

    @Test
    fun endedKeepsLastFrame() = runTest {
        val vm = live()
        hub.push(TerminalFrame.Ended(EndReason.AgentExited))
        runCurrent()
        assertEquals(TerminalPhase.Ended(EndKind.AgentExited, 1_000), vm.state.value.phase)
        assertEquals("Do you want to proceed?", vm.emulator.screenText())
        assertEquals(listOf(1L), hub.closes)
        vm.key(TerminalKeys.ENTER)
        runCurrent()
        assertTrue(hub.inputs.isEmpty())
    }

    @Test
    fun closesOnLeave() = runTest {
        val vm = live()
        vm.leave()
        runCurrent()
        assertEquals(listOf(1L), hub.closes)
    }

    @Test
    fun keysSendNamedKeys() = runTest {
        val vm = live()
        vm.key(TerminalKeys.DOWN)
        vm.key(TerminalKeys.ENTER)
        vm.key("1")
        runCurrent()
        assertEquals(
            listOf(TerminalInput.Keys(listOf("down")), TerminalInput.Keys(listOf("enter")), TerminalInput.Keys(listOf("1"))),
            hub.inputs,
        )
    }

    @Test
    fun ctrlCNeedsSecondTapWithinWindow() = runTest {
        val vm = live()
        vm.key(TerminalKeys.CTRL_C)
        runCurrent()
        assertTrue(vm.ctrlArmed.value)
        advanceTimeBy(3_000)
        assertFalse(vm.ctrlArmed.value)
        assertTrue(hub.inputs.isEmpty())
        vm.key(TerminalKeys.CTRL_C)
        advanceTimeBy(1_000)
        vm.key(TerminalKeys.CTRL_C)
        runCurrent()
        assertEquals(listOf(TerminalInput.Keys(listOf("ctrl_c"))), hub.inputs)
        assertFalse(vm.ctrlArmed.value)
    }

    @Test
    fun pasteFillsReplyOnly() = runTest {
        val vm = live()
        vm.paste("see CI run 4821")
        runCurrent()
        assertEquals("see CI run 4821", vm.reply.value)
        assertTrue(hub.inputs.isEmpty())
    }

    @Test
    fun sendWithAndWithoutEnter() = runTest {
        val vm = live()
        vm.reply.value = "match the gateway, 5s"
        vm.sendReply(enter = true)
        runCurrent()
        assertEquals("", vm.reply.value)
        vm.reply.value = "see CI run 4821"
        vm.sendReply(enter = false)
        runCurrent()
        assertEquals(
            listOf(TerminalInput.Paste("match the gateway, 5s", true), TerminalInput.Paste("see CI run 4821", false)),
            hub.inputs,
        )
    }

    @Test
    fun sendErrorKeepsText() = runTest {
        val vm = live()
        hub.inputError = "not_foreground"
        vm.reply.value = "keep me"
        vm.sendReply(enter = true)
        runCurrent()
        assertEquals("keep me", vm.reply.value)
        assertNotNull(vm.error.value)
        assertEquals(TerminalPhase.Live(InputMode.Paused(dev.sessiontap.android.net.InputUnavailable.NotForeground)), vm.state.value.phase)
    }

    @Test
    fun forbiddenInputDropsToWatchOnly() = runTest {
        val vm = live()
        hub.inputError = "forbidden"
        vm.reply.value = "secret"
        vm.sendReply(enter = true)
        runCurrent()
        assertEquals(TerminalPhase.Live(InputMode.WatchOnly), vm.state.value.phase)
        assertFalse(vm.state.value.control)
        assertTrue(hub.inputs.isEmpty())
    }

    @Test
    fun watchOnlySendsNothing() = runTest {
        val vm = live(control = false)
        vm.key(TerminalKeys.ENTER)
        vm.reply.value = "x"
        vm.sendReply(true)
        runCurrent()
        assertTrue(hub.inputs.isEmpty())
    }

    @Test
    fun queuedKeysKeepOrderAndMergeCharacters() = runTest {
        val vm = live()
        vm.type("wi")
        vm.key(TerminalKeys.ESCAPE)
        vm.type("x\n")
        runCurrent()
        assertEquals(
            listOf(
                TerminalInput.Keys(listOf("w", "i")),
                TerminalInput.Keys(listOf("escape")),
                TerminalInput.Keys(listOf("x")),
                TerminalInput.Keys(listOf("enter")),
            ),
            hub.inputs,
        )
    }

    @Test
    fun failedKeyDropsTheRestOfTheQueue() = runTest {
        val vm = live()
        hub.inputError = "pane_in_mode"
        vm.key(TerminalKeys.DOWN)
        vm.key(TerminalKeys.ENTER)
        runCurrent()
        assertEquals(1, hub.inputAttempts)
        assertNotNull(vm.error.value)
    }

    @Test
    fun latchedCtrlAppliesToOneKey() = runTest {
        val vm = live()
        vm.tapModifier(ModKey.Ctrl)
        assertEquals(ModState.Latched, vm.mods.value.ctrl)
        vm.key(TerminalKeys.LEFT)
        vm.key(TerminalKeys.LEFT)
        runCurrent()
        assertEquals(
            listOf(TerminalInput.Keys(listOf("ctrl+left")), TerminalInput.Keys(listOf("left"))),
            hub.inputs,
        )
        assertEquals(ModState.Off, vm.mods.value.ctrl)
        assertEquals("Ctrl+Left", vm.sentCombo.value)
        advanceTimeBy(2_000)
        assertEquals(null, vm.sentCombo.value)
    }

    @Test
    fun lockedCtrlStaysUntilTapped() = runTest {
        val vm = live()
        vm.lockModifier(ModKey.Ctrl)
        vm.tapModifier(ModKey.Alt)
        vm.key(TerminalKeys.UP)
        vm.type("r")
        vm.tapModifier(ModKey.Ctrl)
        vm.key(TerminalKeys.UP)
        runCurrent()
        assertEquals(
            listOf(
                TerminalInput.Keys(listOf("ctrl+alt+up")),
                TerminalInput.Keys(listOf("ctrl+r")),
                TerminalInput.Keys(listOf("up")),
            ),
            hub.inputs,
        )
        assertEquals(Modifiers(), vm.mods.value)
        assertEquals("Ctrl+R", vm.sentCombo.value)
    }

    @Test
    fun ctrlCDoubleTapIgnoresModifiers() = runTest {
        val vm = live()
        vm.tapModifier(ModKey.Alt)
        vm.key(TerminalKeys.CTRL_C)
        vm.key(TerminalKeys.CTRL_C)
        runCurrent()
        assertEquals(listOf(TerminalInput.Keys(listOf("ctrl_c"))), hub.inputs)
        assertEquals(ModState.Latched, vm.mods.value.alt)
    }
}
