package dev.sessiontap.android.ui.terminal

import dev.sessiontap.android.net.EndReason
import dev.sessiontap.android.net.InputState
import dev.sessiontap.android.net.InputUnavailable
import dev.sessiontap.android.net.TerminalErrors
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class TerminalStateTest {
    private val control = TerminalState(control = true)
    private val watch = TerminalState(control = false)
    private val available = InputState(true)

    private fun run(start: TerminalState, vararg events: TerminalEvent) = events.fold(start, ::reduce)
    private fun live(start: TerminalState = control) = run(start, TerminalEvent.Opening, TerminalEvent.Snapshot(available))

    @Test
    fun opensThenGoesLive() {
        assertEquals(TerminalPhase.Opening, run(control, TerminalEvent.Opening).phase)
        assertEquals(TerminalPhase.Live(InputMode.Enabled), live().phase)
        assertTrue(live().showsPane)
        assertFalse(run(control, TerminalEvent.Opening).showsPane)
    }

    @Test
    fun watchScopeIsWatchOnly() {
        assertEquals(TerminalPhase.Live(InputMode.WatchOnly), live(watch).phase)
        // input frames never enable typing without control
        assertEquals(TerminalPhase.Live(InputMode.WatchOnly), reduce(live(watch), TerminalEvent.Input(available)).phase)
    }

    @Test
    fun snapshotInputAvailabilityPauses() {
        val s = run(control, TerminalEvent.Opening, TerminalEvent.Snapshot(InputState(false, InputUnavailable.PaneInMode)))
        assertEquals(TerminalPhase.Live(InputMode.Paused(InputUnavailable.PaneInMode)), s.phase)
    }

    @Test
    fun inputFramesPauseAndResume() {
        val paused = reduce(live(), TerminalEvent.Input(InputState(false, InputUnavailable.NotForeground)))
        assertEquals(TerminalPhase.Live(InputMode.Paused(InputUnavailable.NotForeground)), paused.phase)
        val scrolling = reduce(live(), TerminalEvent.Input(InputState(false, InputUnavailable.PaneInMode)))
        assertEquals(TerminalPhase.Live(InputMode.Paused(InputUnavailable.PaneInMode)), scrolling.phase)
        assertEquals(TerminalPhase.Live(InputMode.Enabled), reduce(paused, TerminalEvent.Input(available)).phase)
    }

    @Test
    fun inputErrorsPauseOrDropToWatchOnly() {
        assertEquals(TerminalPhase.Live(InputMode.Paused(InputUnavailable.NotForeground)), reduce(live(), TerminalEvent.InputFailed(TerminalErrors.NOT_FOREGROUND)).phase)
        assertEquals(TerminalPhase.Live(InputMode.Paused(InputUnavailable.PaneInMode)), reduce(live(), TerminalEvent.InputFailed(TerminalErrors.PANE_IN_MODE)).phase)
        val forbidden = reduce(live(), TerminalEvent.InputFailed(TerminalErrors.FORBIDDEN))
        assertEquals(TerminalPhase.Live(InputMode.WatchOnly), forbidden.phase)
        assertFalse(forbidden.control)
        assertEquals(TerminalPhase.Live(InputMode.Enabled), reduce(live(), TerminalEvent.InputFailed(TerminalErrors.BAD_REQUEST)).phase)
    }

    @Test
    fun laterSnapshotCatchesUpUntilRendered() {
        val catching = reduce(live(), TerminalEvent.Snapshot(available))
        assertEquals(TerminalPhase.Live(InputMode.Enabled, catchingUp = true), catching.phase)
        assertEquals(TerminalPhase.Live(InputMode.Enabled), reduce(catching, TerminalEvent.Rendered).phase)
    }

    @Test
    fun endReasonsMapToEndCards() {
        val cases = mapOf(
            EndReason.AgentExited to EndKind.AgentExited,
            EndReason.PaneClosed to EndKind.PaneClosed,
            EndReason.SessionClosed to EndKind.PaneClosed,
            EndReason.MultiplexerStopped to EndKind.TmuxStopped,
        )
        cases.forEach { (reason, kind) ->
            val ended = reduce(live(), TerminalEvent.Ended(reason, 42))
            assertEquals(TerminalPhase.Ended(kind, 42), ended.phase)
            assertTrue(ended.showsPane)
            assertTrue(ended.final)
            // nothing reopens input on an ended terminal
            assertEquals(ended, reduce(ended, TerminalEvent.Input(available)))
            assertEquals(ended, reduce(ended, TerminalEvent.ConnectionLost))
            assertEquals(ended, reduce(ended, TerminalEvent.Snapshot(available)))
        }
    }

    @Test
    fun identityChangedIsSafetyWithoutPane() {
        val s = reduce(live(), TerminalEvent.Ended(EndReason.IdentityChanged, 1))
        assertEquals(TerminalPhase.Error(ErrorKind.Safety), s.phase)
        assertFalse(s.showsPane)
    }

    @Test
    fun sourceDisallowsControlRefuses() {
        assertEquals(TerminalPhase.Error(ErrorKind.SourceRefused), run(control, TerminalEvent.Opening, TerminalEvent.OpenFailed(TerminalErrors.SOURCE_DISALLOWS_CONTROL)).phase)
        assertEquals(TerminalPhase.Error(ErrorKind.SourceRefused), reduce(live(), TerminalEvent.Ended(EndReason.SourceDisallowsControl, 1)).phase)
    }

    @Test
    fun dropsReconnectAndReopen() {
        for (drop in listOf(TerminalEvent.ConnectionLost, TerminalEvent.Ended(EndReason.SourceUnavailable, 1))) {
            val lost = reduce(live(), drop)
            assertEquals(TerminalPhase.Reconnecting, lost.phase)
            assertTrue(lost.showsPane)
            val back = run(lost, TerminalEvent.Reconnected(listOf("read", "watch", "control")), TerminalEvent.Opening)
            assertEquals(TerminalPhase.Reconnecting, back.phase)
            assertEquals(TerminalPhase.Live(InputMode.Enabled), reduce(back, TerminalEvent.Snapshot(available)).phase)
        }
    }

    @Test
    fun reconnectWithoutWatchIsRevoked() {
        val s = run(live(), TerminalEvent.ConnectionLost, TerminalEvent.Reconnected(listOf("read", "manage")))
        assertEquals(TerminalPhase.Error(ErrorKind.Revoked), s.phase)
        assertTrue(s.final)
    }

    @Test
    fun reconnectWithWatchOnlyReopensWatchOnly() {
        val s = run(live(), TerminalEvent.ConnectionLost, TerminalEvent.Reconnected(listOf("read", "watch")), TerminalEvent.Opening, TerminalEvent.Snapshot(available))
        assertEquals(TerminalPhase.Live(InputMode.WatchOnly), s.phase)
    }

    @Test
    fun revokedDevice() {
        assertEquals(TerminalPhase.Error(ErrorKind.Revoked), reduce(live(), TerminalEvent.Revoked).phase)
        assertEquals(TerminalPhase.Error(ErrorKind.Revoked), run(control, TerminalEvent.Opening, TerminalEvent.OpenFailed(TerminalErrors.FORBIDDEN)).phase)
    }

    @Test
    fun unreachableRecoversOnReconnect() {
        val off = run(live(), TerminalEvent.ConnectionLost, TerminalEvent.Unreachable)
        assertEquals(TerminalPhase.Error(ErrorKind.Unreachable), off.phase)
        assertFalse(off.final)
        assertEquals(TerminalPhase.Reconnecting, reduce(off, TerminalEvent.Reconnected(listOf("watch", "control"))).phase)
    }

    @Test
    fun missingTerminalIsUnavailable() {
        assertEquals(TerminalPhase.Error(ErrorKind.Unavailable), run(control, TerminalEvent.Opening, TerminalEvent.OpenFailed(TerminalErrors.TERMINAL_UNAVAILABLE)).phase)
    }

    @Test
    fun connectionChipLabels() {
        assertEquals("live", connLabel(TerminalPhase.Live(InputMode.Enabled)))
        assertEquals("input paused", connLabel(TerminalPhase.Live(InputMode.Paused(InputUnavailable.NotForeground))))
        assertEquals("connecting", connLabel(TerminalPhase.Opening))
        assertEquals("Reconnecting…", connLabel(TerminalPhase.Reconnecting))
        assertEquals("ended", connLabel(TerminalPhase.Ended(EndKind.AgentExited, 0)))
        assertEquals("offline", connLabel(TerminalPhase.Error(ErrorKind.Unreachable)))
        assertEquals("closed", connLabel(TerminalPhase.Error(ErrorKind.Revoked)))
    }
}
