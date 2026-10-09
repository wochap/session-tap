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
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.selects.select
import kotlinx.coroutines.withTimeoutOrNull

/** Redraw tick for the pane: [version] changes on every frame, [scrolled] counts lines that left the screen. */
data class PaneTick(val version: Long = 0, val scrolled: Long = 0)

/**
 * One open terminal screen: opens the agent's stream on the hub's current
 * connection, feeds frames to the emulator, re-opens with a fresh snapshot
 * after a reconnect, and closes the stream on leave. Input goes through one
 * ordered queue; a failure drops what is still queued, and nothing is replayed.
 * A kept reply waits for the user to tap Send.
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
    private val _mods = MutableStateFlow(Modifiers())
    val mods: StateFlow<Modifiers> = _mods
    /** Label of the last modified key sent ("Ctrl+R"), shown briefly. */
    private val _sentCombo = MutableStateFlow<String?>(null)
    val sentCombo: StateFlow<String?> = _sentCombo
    val emulator = PaneEmulator()
    private val queue = Channel<Queued>(Channel.UNLIMITED)
    private var comboJob: Job? = null

    @Volatile private var stream: TerminalStream? = null
    private var client: TerminalHub? = null
    private var ctrlJob: Job? = null
    private var loop: Job? = null

    private val _frozen = MutableStateFlow(false)
    /** Selection mode holds the view still: output is queued instead of applied. */
    val frozen: StateFlow<Boolean> = _frozen
    private val _heldLines = MutableStateFlow(0)
    /** `\n` bytes held while frozen, for the Jump to live pill. */
    val heldLines: StateFlow<Int> = _heldLines
    private val held = ArrayList<ByteArray>()
    private var heldBytes = 0
    private var heldSnapshot: TerminalFrame.Snapshot? = null
    /** Held output passed [HOLD_CAP_BYTES] and was dropped; unfreeze reopens for a fresh snapshot. */
    private var stale = false
    private val reopen = Channel<Unit>(Channel.CONFLATED)

    fun start() {
        if (loop != null) return
        loop = scope.launch { run() }
        scope.launch { drain() }
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
        var reopening = false
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
                else -> if (!reopening && (!first || _state.value.phase is TerminalPhase.Error)) {
                    dispatch(TerminalEvent.Reconnected(scopes()))
                    if (_state.value.final) return
                }
            }
            first = false
            // A reopen after a held-output overflow keeps the pane; the new snapshot shows as catching up.
            if (!reopening) dispatch(TerminalEvent.Opening)
            reopening = false
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
            while (true) {
                val frame = select<TerminalFrame?> {
                    opened.frames.onReceiveCatching { it.getOrNull() }
                    reopen.onReceive {
                        reopening = true
                        null
                    }
                } ?: break
                handle(frame)
                if (_state.value.final || _state.value.phase == TerminalPhase.Reconnecting) break
            }
            stream = null
            if (reopening) {
                closeQuietly(h, opened)
                continue
            }
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

    /** Holds the view still: later output is queued until [unfreeze]. */
    fun freeze() {
        _frozen.value = true
    }

    /** Applies held output in order, or reopens the stream for a fresh snapshot after an overflow. */
    fun unfreeze() {
        if (!_frozen.value) return
        _frozen.value = false
        heldSnapshot?.let(::applySnapshot)
        heldSnapshot = null
        var lines = 0L
        held.forEach { lines += emulator.append(it) }
        if (held.isNotEmpty()) _tick.update { PaneTick(emulator.version, it.scrolled + lines) }
        held.clear()
        heldBytes = 0
        _heldLines.value = 0
        if (stale) {
            stale = false
            reopen.trySend(Unit)
        }
    }

    private fun applySnapshot(frame: TerminalFrame.Snapshot) {
        emulator.reset(frame)
        _tick.update { PaneTick(emulator.version, it.scrolled) }
    }

    private fun hold(bytes: ByteArray) {
        if (stale) return
        if (heldBytes + bytes.size > HOLD_CAP_BYTES) {
            held.clear()
            heldBytes = 0
            heldSnapshot = null
            stale = true
        } else {
            held += bytes
            heldBytes += bytes.size
        }
        _heldLines.update { n -> n + bytes.count { it == '\n'.code.toByte() } }
    }

    private fun handle(frame: TerminalFrame) {
        when (frame) {
            is TerminalFrame.Snapshot -> {
                if (_frozen.value) {
                    // A newer snapshot supersedes anything queued before it.
                    held.clear()
                    heldBytes = 0
                    stale = false
                    heldSnapshot = frame
                } else {
                    applySnapshot(frame)
                }
                val catching = _state.value.baselined
                dispatch(TerminalEvent.Snapshot(frame.input))
                if (catching) {
                    scope.launch {
                        delay(catchUpMs)
                        dispatch(TerminalEvent.Rendered)
                    }
                }
            }
            is TerminalFrame.Output -> if (_frozen.value) hold(frame.bytes) else {
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

    /**
     * Sends one named key or character with the active modifiers, plus [ctrl]/[alt]
     * held on a hardware keyboard; latched modifiers are then cleared.
     */
    fun key(name: String, ctrl: Boolean = false, alt: Boolean = false) {
        if (name == TerminalKeys.CTRL_C) return ctrlC()
        if (!inputEnabled) return
        val m = _mods.value
        if (!m.any && !ctrl && !alt) return send(TerminalInput.Keys(listOf(name)))
        val combo = TerminalKeys.withMods(name, m.ctrl.on || ctrl, m.alt.on || alt)
        _mods.value = m.used()
        send(TerminalInput.Keys(listOf(combo)))
        _sentCombo.value = TerminalKeys.label(combo)
        comboJob?.cancel()
        comboJob = scope.launch {
            delay(SENT_COMBO_MS)
            _sentCombo.value = null
        }
    }

    /** Text the soft keyboard committed: each character is one keystroke, in order. */
    fun type(text: String) {
        text.codePoints().forEach { cp ->
            key(
                when (cp) {
                    '\n'.code, '\r'.code -> TerminalKeys.ENTER
                    '\t'.code -> TerminalKeys.TAB
                    ' '.code -> TerminalKeys.SPACE
                    else -> String(Character.toChars(cp))
                },
            )
        }
    }

    /** A tap latches [mod] for the next key, or turns it off when already on. */
    fun tapModifier(mod: ModKey) = _mods.update { it.set(mod, if (it[mod] == ModState.Off) ModState.Latched else ModState.Off) }

    /** A long-press locks [mod] until it is tapped again. */
    fun lockModifier(mod: ModKey) = _mods.update { it.set(mod, ModState.Locked) }

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
        queue.trySend(Queued(input, onSent))
    }

    /** Sends queued input one request at a time, merging queued character keys. */
    private suspend fun drain() {
        var held: Queued? = null
        while (true) {
            var item = held ?: queue.receive()
            held = null
            while (item.chars) {
                val more = queue.tryReceive().getOrNull() ?: break
                if (!more.chars) {
                    held = more
                    break
                }
                item = item.merge(more)
            }
            try {
                val s = stream
                val h = client
                if (s == null || h == null) throw RpcException("offline", "hub is not connected")
                h.sendInput(s, item.input)
                _error.value = null
                item.onSent()
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                // Later keys must not run ahead of a failed one.
                held = null
                while (queue.tryReceive().isSuccess) Unit
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
        const val SENT_COMBO_MS = 1_500L
        /** Output held while frozen, past which it is dropped and a fresh snapshot is fetched. */
        const val HOLD_CAP_BYTES = 1 shl 20

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

private class Queued(val input: TerminalInput, val onSent: () -> Unit) {
    /** Plain character keys, which may share one request with neighbours. */
    val chars: Boolean get() = input is TerminalInput.Keys && input.keys.all(TerminalKeys::isChar)

    fun merge(other: Queued) = Queued(
        TerminalInput.Keys((input as TerminalInput.Keys).keys + (other.input as TerminalInput.Keys).keys),
    ) {
        onSent()
        other.onSent()
    }
}

enum class ModKey { Ctrl, Alt }

enum class ModState {
    Off, Latched, Locked;

    val on: Boolean get() = this != Off
}

/** Ctrl and Alt for the next key: latched for one key, locked until tapped again. */
data class Modifiers(val ctrl: ModState = ModState.Off, val alt: ModState = ModState.Off) {
    val any: Boolean get() = ctrl.on || alt.on

    operator fun get(mod: ModKey): ModState = if (mod == ModKey.Ctrl) ctrl else alt

    fun set(mod: ModKey, state: ModState) = if (mod == ModKey.Ctrl) copy(ctrl = state) else copy(alt = state)

    /** After one key: latched modifiers clear, locked ones stay. */
    fun used() = Modifiers(
        ctrl = if (ctrl == ModState.Locked) ModState.Locked else ModState.Off,
        alt = if (alt == ModState.Locked) ModState.Locked else ModState.Off,
    )
}
