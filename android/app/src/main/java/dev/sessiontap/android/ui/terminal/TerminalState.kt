package dev.sessiontap.android.ui.terminal

import dev.sessiontap.android.domain.Scopes
import dev.sessiontap.android.net.EndReason
import dev.sessiontap.android.net.InputState
import dev.sessiontap.android.net.InputUnavailable
import dev.sessiontap.android.net.TerminalErrors

/** Whether the key bar and reply field may send. */
sealed interface InputMode {
    data object Enabled : InputMode
    data class Paused(val reason: InputUnavailable) : InputMode
    data object WatchOnly : InputMode
}

/** Why a terminal ended while keeping its last frame. */
enum class EndKind { AgentExited, PaneClosed, TmuxStopped }

/** Screens that replace the pane entirely. */
enum class ErrorKind { Revoked, Unreachable, Safety, SourceRefused, Unavailable }

/** One sealed state drives the whole terminal screen. */
sealed interface TerminalPhase {
    data object Opening : TerminalPhase
    /** [catchingUp] is set while a later snapshot on the open stream renders. */
    data class Live(val input: InputMode, val catchingUp: Boolean = false) : TerminalPhase
    data object Reconnecting : TerminalPhase
    data class Ended(val kind: EndKind, val atMs: Long) : TerminalPhase
    data class Error(val kind: ErrorKind) : TerminalPhase
}

/** Everything the reducer tracks: the phase plus whether this device has `control`. */
data class TerminalState(
    val phase: TerminalPhase = TerminalPhase.Opening,
    val control: Boolean,
    /** A snapshot has rendered on the current stream. */
    val baselined: Boolean = false,
) {
    /** The pane shows a frame (live, reconnecting, catching up, or ended). */
    val showsPane: Boolean
        get() = when (phase) {
            TerminalPhase.Opening -> false
            is TerminalPhase.Error -> false
            else -> true
        }

    /** The screen is finished with the stream: nothing reopens it. */
    val final: Boolean
        get() = phase is TerminalPhase.Ended || (phase as? TerminalPhase.Error)?.kind.let { it != null && it != ErrorKind.Unreachable }
}

sealed interface TerminalEvent {
    /** `terminal.open` is about to be called (first time or after a reconnect). */
    data object Opening : TerminalEvent
    data class OpenFailed(val code: String) : TerminalEvent
    data class Snapshot(val input: InputState) : TerminalEvent
    data object Rendered : TerminalEvent
    data class Input(val input: InputState) : TerminalEvent
    data class Ended(val reason: EndReason, val atMs: Long) : TerminalEvent
    data class InputFailed(val code: String) : TerminalEvent
    /** The hub connection dropped or the stream's channel closed. */
    data object ConnectionLost : TerminalEvent
    /** Reconnect keeps failing; the hub counts as offline. */
    data object Unreachable : TerminalEvent
    /** The device was revoked (close 4401). */
    data object Revoked : TerminalEvent
    /** Connected again; [scopes] are the effective scopes from `hub.info`. */
    data class Reconnected(val scopes: List<String>) : TerminalEvent
}

private fun TerminalState.inputFor(input: InputState): InputMode = when {
    !control -> InputMode.WatchOnly
    input.available -> InputMode.Enabled
    else -> InputMode.Paused(input.reason ?: InputUnavailable.NotForeground)
}

/** Pure transition function; see design.md decision 4 for the mapping. */
fun reduce(state: TerminalState, event: TerminalEvent): TerminalState {
    if (state.final) return state
    return when (event) {
        TerminalEvent.Opening ->
            if (state.phase is TerminalPhase.Reconnecting || state.phase is TerminalPhase.Error) state.copy(baselined = false)
            else state.copy(phase = TerminalPhase.Opening, baselined = false)
        is TerminalEvent.OpenFailed -> when (event.code) {
            TerminalErrors.SOURCE_DISALLOWS_CONTROL -> state.copy(phase = TerminalPhase.Error(ErrorKind.SourceRefused))
            TerminalErrors.FORBIDDEN -> state.copy(phase = TerminalPhase.Error(ErrorKind.Revoked))
            TerminalErrors.NOT_FOUND, TerminalErrors.TERMINAL_UNAVAILABLE, TerminalErrors.UNSUPPORTED_BACKEND ->
                state.copy(phase = TerminalPhase.Error(ErrorKind.Unavailable))
            else -> state.copy(phase = TerminalPhase.Reconnecting)
        }
        is TerminalEvent.Snapshot -> state.copy(
            phase = TerminalPhase.Live(state.inputFor(event.input), catchingUp = state.baselined),
            baselined = true,
        )
        TerminalEvent.Rendered -> when (val p = state.phase) {
            is TerminalPhase.Live -> state.copy(phase = p.copy(catchingUp = false))
            else -> state
        }
        is TerminalEvent.Input -> when (val p = state.phase) {
            is TerminalPhase.Live -> if (p.input == InputMode.WatchOnly) state else state.copy(phase = p.copy(input = state.inputFor(event.input)))
            else -> state
        }
        is TerminalEvent.Ended -> when (event.reason) {
            EndReason.AgentExited -> state.copy(phase = TerminalPhase.Ended(EndKind.AgentExited, event.atMs))
            EndReason.PaneClosed, EndReason.SessionClosed -> state.copy(phase = TerminalPhase.Ended(EndKind.PaneClosed, event.atMs))
            EndReason.MultiplexerStopped -> state.copy(phase = TerminalPhase.Ended(EndKind.TmuxStopped, event.atMs))
            EndReason.IdentityChanged -> state.copy(phase = TerminalPhase.Error(ErrorKind.Safety))
            EndReason.SourceDisallowsControl -> state.copy(phase = TerminalPhase.Error(ErrorKind.SourceRefused))
            EndReason.SourceUnavailable, EndReason.Closed -> state.copy(phase = TerminalPhase.Reconnecting)
        }
        is TerminalEvent.InputFailed -> when (val p = state.phase) {
            is TerminalPhase.Live -> when (event.code) {
                TerminalErrors.FORBIDDEN -> state.copy(control = false, phase = p.copy(input = InputMode.WatchOnly))
                TerminalErrors.NOT_FOREGROUND -> state.copy(phase = p.copy(input = InputMode.Paused(InputUnavailable.NotForeground)))
                TerminalErrors.PANE_IN_MODE -> state.copy(phase = p.copy(input = InputMode.Paused(InputUnavailable.PaneInMode)))
                TerminalErrors.SOURCE_DISALLOWS_CONTROL -> state.copy(phase = TerminalPhase.Error(ErrorKind.SourceRefused))
                else -> state
            }
            else -> state
        }
        TerminalEvent.ConnectionLost -> state.copy(phase = TerminalPhase.Reconnecting)
        TerminalEvent.Unreachable -> state.copy(phase = TerminalPhase.Error(ErrorKind.Unreachable))
        TerminalEvent.Revoked -> state.copy(phase = TerminalPhase.Error(ErrorKind.Revoked))
        is TerminalEvent.Reconnected -> when {
            Scopes.WATCH !in event.scopes && Scopes.CONTROL !in event.scopes -> state.copy(phase = TerminalPhase.Error(ErrorKind.Revoked))
            else -> state.copy(
                control = Scopes.CONTROL in event.scopes,
                phase = if (state.phase is TerminalPhase.Error) TerminalPhase.Reconnecting else state.phase,
            )
        }
    }
}
