package dev.sessiontap.android.ui.terminal

import androidx.lifecycle.ViewModel
import androidx.lifecycle.viewModelScope
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.net.RpcException
import dev.sessiontap.android.net.TerminalErrors
import dev.sessiontap.android.net.TerminalFrame
import dev.sessiontap.android.net.TerminalHub
import dev.sessiontap.android.net.TerminalInput
import dev.sessiontap.android.net.TerminalKeys
import dev.sessiontap.android.net.TerminalStream
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/** Redraw tick for the pane: [version] changes on every frame, [scrolled] counts lines that left the screen. */
data class PaneTick(val version: Long = 0, val scrolled: Long = 0)

/**
 * One open terminal screen: opens the agent's stream on the hub's current
 * connection, feeds frames to the emulator, re-opens with a fresh snapshot
 * after a reconnect, and closes the stream on leave. Input is never queued or
 * replayed; a kept reply waits for the user to tap Send.
 */
class TerminalViewModel(
    private val key: AgentKey,
    private val hub: () -> TerminalHub?,
    /** Effective scopes the hub granted on the latest `hub.info`. */
    private val scopes: () -> List<String>,
    control: Boolean,
    private val closeScope: CoroutineScope,
    private val now: () -> Long = System::currentTimeMillis,
    scope: CoroutineScope? = null,
    private val retryMs: Long = 1_000,
    private val catchUpMs: Long = 300,
) : ViewModel() {
    private val scope = scope ?: viewModelScope
    private val _state = MutableStateFlow(TerminalState(control = control))
    val state: StateFlow<TerminalState> = _state
    private val _tick = MutableStateFlow(PaneTick())
    val tick: StateFlow<PaneTick> = _tick
    val reply = MutableStateFlow("")
    private val _error = MutableStateFlow<String?>(null)
    val error: StateFlow<String?> = _error
    private val _ctrlArmed = MutableStateFlow(false)
    val ctrlArmed: StateFlow<Boolean> = _ctrlArmed
    val emulator = PaneEmulator()

    @Volatile private var stream: TerminalStream? = null
    private var client: TerminalHub? = null
    private var ctrlJob: Job? = null
    private var loop: Job? = null

    fun start() {
        if (loop != null) return
        loop = scope.launch { run() }
    }

    private fun dispatch(event: TerminalEvent) = _state.update { reduce(it, event) }

    private suspend fun awaitHub(): TerminalHub {
        while (true) {
            hub()?.let { return it }
            delay(retryMs)
        }
    }

    private suspend fun run() {
        var first = true
        while (scope.isActive && !_state.value.final) {
            val h = awaitHub()
            client = h
            when (h.state.first { it.connected || it is ConnState.Revoked || it is ConnState.Reconnecting && it.offline }) {
                ConnState.Revoked -> {
                    dispatch(TerminalEvent.Revoked)
                    return
                }
                is ConnState.Reconnecting -> {
                    dispatch(TerminalEvent.Unreachable)
                    h.state.first { it.connected || it == ConnState.Revoked || it is ConnState.Connecting }
                    continue
                }
                else -> if (!first || _state.value.phase is TerminalPhase.Error) {
                    dispatch(TerminalEvent.Reconnected(scopes()))
                    if (_state.value.final) return
                }
            }
            first = false
            dispatch(TerminalEvent.Opening)
            val connState = h.state.value
            val opened = try {
                h.openTerminal(key.sourceId, key.invocationId)
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                dispatch(TerminalEvent.OpenFailed(e.code))
                if (_state.value.final) return
                awaitChange(h, connState)
                continue
            }
            stream = opened
            for (frame in opened.frames) {
                handle(frame)
                if (_state.value.final || _state.value.phase == TerminalPhase.Reconnecting) break
            }
            stream = null
            if (_state.value.final) {
                closeQuietly(h, opened)
                return
            }
            if (_state.value.phase == TerminalPhase.Reconnecting) {
                // The stream ended with source_unavailable on a live connection.
                closeQuietly(h, opened)
                delay(retryMs)
            } else {
                // The connection closed under the stream.
                dispatch(TerminalEvent.ConnectionLost)
                awaitChange(h, connState)
            }
        }
    }

    /** Waits until the hub's connection state is a new value (the dropped connection is noticed), or a retry interval passes. */
    private suspend fun awaitChange(h: TerminalHub, before: ConnState) {
        withTimeoutOrNull(retryMs * 5) { h.state.first { it !== before } }
        delay(retryMs / 4)
    }

    private fun handle(frame: TerminalFrame) {
        when (frame) {
            is TerminalFrame.Snapshot -> {
                emulator.reset(frame)
                _tick.update { PaneTick(emulator.version, it.scrolled) }
                val catching = _state.value.baselined
                dispatch(TerminalEvent.Snapshot(frame.input))
                if (catching) {
                    scope.launch {
                        delay(catchUpMs)
                        dispatch(TerminalEvent.Rendered)
                    }
                }
            }
            is TerminalFrame.Output -> {
                val lines = emulator.append(frame.bytes)
                _tick.update { PaneTick(emulator.version, it.scrolled + lines) }
            }
            is TerminalFrame.Input -> dispatch(TerminalEvent.Input(frame.state))
            is TerminalFrame.Ended -> dispatch(TerminalEvent.Ended(frame.reason, now()))
        }
    }

    private suspend fun closeQuietly(h: TerminalHub, s: TerminalStream) {
        runCatching { h.closeTerminal(s) }
    }

    private val inputEnabled: Boolean
        get() = (_state.value.phase as? TerminalPhase.Live)?.input == InputMode.Enabled

    /** Sends one named key (or a single character such as a quick-pick digit). */
    fun key(name: String) {
        if (name == TerminalKeys.CTRL_C) return ctrlC()
        send(TerminalInput.Keys(listOf(name)))
    }

    /** First tap arms, a second tap within 2.5 s sends Ctrl+C. */
    fun ctrlC() {
        if (!inputEnabled) return
        ctrlJob?.cancel()
        if (!_ctrlArmed.value) {
            _ctrlArmed.value = true
            ctrlJob = scope.launch {
                delay(CTRL_C_CONFIRM_MS)
                _ctrlArmed.value = false
            }
        } else {
            _ctrlArmed.value = false
            send(TerminalInput.Keys(listOf(TerminalKeys.CTRL_C)))
        }
    }

    /** Puts clipboard text into the reply field; nothing is sent. */
    fun paste(text: String) {
        if (text.isEmpty()) return
        reply.update { it + text }
    }

    /** Sends the reply as a paste, followed by Enter unless [enter] is false. */
    fun sendReply(enter: Boolean) {
        val text = reply.value
        if (text.isEmpty()) return
        send(TerminalInput.Paste(text, enter)) { reply.update { current -> if (current == text) "" else current } }
    }

    fun clearError() {
        _error.value = null
    }

    private fun send(input: TerminalInput, onSent: () -> Unit = {}) {
        if (!inputEnabled) return
        val s = stream
        val h = client
        scope.launch {
            try {
                if (s == null || h == null) throw RpcException("offline", "hub is not connected")
                h.sendInput(s, input)
                _error.value = null
                onSent()
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                dispatch(TerminalEvent.InputFailed(e.code))
                _error.value = inputError(e)
            }
        }
    }

    /** Stops the stream; called when the user leaves the screen. */
    fun leave() {
        loop?.cancel()
        val s = stream ?: return
        stream = null
        val h = client ?: return
        closeScope.launch { runCatching { h.closeTerminal(s) } }
    }

    override fun onCleared() {
        leave()
    }

    companion object {
        const val CTRL_C_CONFIRM_MS = 2_500L

        fun inputError(e: RpcException): String = when (e.code) {
            TerminalErrors.FORBIDDEN -> "This phone can no longer type into agents on this hub."
            TerminalErrors.NOT_FOREGROUND -> "Not sent: the agent isn't in the foreground."
            TerminalErrors.PANE_IN_MODE -> "Not sent: the desktop is scrolling this pane."
            TerminalErrors.TERMINAL_ENDED -> "Not sent: the terminal ended."
            TerminalErrors.SOURCE_DISALLOWS_CONTROL -> "Not sent: this source doesn't accept input."
            "offline", "closed" -> "Not sent: the hub is not connected."
            else -> "Not sent: ${e.message}"
        }
    }
}

/** The connection can carry requests. */
val ConnState.connected: Boolean get() = this is ConnState.Live || this is ConnState.NoAccess
