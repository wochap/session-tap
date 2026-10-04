package dev.sessiontap.android.net

import dev.sessiontap.android.crypto.PinnedTrustManager
import dev.sessiontap.android.domain.Scopes
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.coroutines.channels.ReceiveChannel
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.jsonPrimitive
import kotlinx.serialization.json.longOrNull
import kotlinx.serialization.json.buildJsonObject
import okhttp3.OkHttpClient
import kotlin.math.min

/** Connection state shown per hub. */
sealed interface ConnState {
    data object Connecting : ConnState
    data class Live(val endpoint: String) : ConnState
    /** Connected, but the hub grants no `read` scope, so there is no session stream. */
    data class NoAccess(val endpoint: String) : ConnState
    /** Waiting until [retryAt] (epoch ms). After a few failures the hub counts as offline. */
    data class Reconnecting(val retryAt: Long, val failures: Int, val lastError: String?) : ConnState {
        val offline: Boolean get() = failures >= OFFLINE_AFTER_FAILURES
    }
    data object Revoked : ConnState

    companion object {
        const val OFFLINE_AFTER_FAILURES = 3
    }
}

/** An open terminal stream on one connection. */
class TerminalStream internal constructor(val id: Long, val frames: ReceiveChannel<TerminalFrame>, internal val conn: Connection)

/** The terminal calls of one hub, on its current connection. */
interface TerminalHub {
    val state: StateFlow<ConnState>
    suspend fun openTerminal(sourceId: String, invocationId: String): TerminalStream
    suspend fun sendInput(stream: TerminalStream, input: TerminalInput)
    suspend fun closeTerminal(stream: TerminalStream)
    /** Retry now instead of waiting for the backoff. */
    fun kick()
}

interface HubClientListener {
    suspend fun onConnected(hubId: String, endpoint: String, info: HubInfo)
    suspend fun onEnvelope(hubId: String, envelope: HubEnvelope)
    suspend fun onRevoked(hubId: String)
}

/**
 * Keeps one authenticated connection to a hub: races endpoint hints, calls
 * `hub.info` and `listen` (when the hub grants `read`), and reconnects with
 * exponential backoff capped at 60s. A scope-withdrawn close reconnects at once.
 */
class HubClient(
    val hubId: String,
    private val endpoints: () -> List<String>,
    private var lastGood: String?,
    private val listener: HubClientListener,
    private val scope: CoroutineScope,
    private val clientFor: () -> Pair<OkHttpClient, PinnedTrustManager?>,
    private val now: () -> Long = System::currentTimeMillis,
    private val initialBackoffMs: Long = 1_000,
    private val maxBackoffMs: Long = MAX_BACKOFF_MS,
) : TerminalHub {
    private val _state = MutableStateFlow<ConnState>(ConnState.Connecting)
    override val state: StateFlow<ConnState> = _state
    private val wake = Channel<Unit>(Channel.CONFLATED)
    private var job: Job? = null
    @Volatile private var current: Connection? = null
    @Volatile private var backoffMs = initialBackoffMs

    fun start() {
        if (job != null) return
        job = scope.launch { run() }
    }

    fun stop() {
        job?.cancel()
        job = null
        current?.close()
        current = null
    }

    /** Network changed: retry now with a fresh backoff. */
    override fun kick() {
        backoffMs = initialBackoffMs
        wake.trySend(Unit)
    }

    suspend fun forget(sourceId: String, invocationId: String) {
        val conn = current ?: throw RpcException("offline", "hub is not connected")
        conn.call(
            "forget",
            buildJsonObject {
                put("source_id", JsonPrimitive(sourceId))
                put("invocation_id", JsonPrimitive(invocationId))
            },
        )
    }

    override suspend fun openTerminal(sourceId: String, invocationId: String): TerminalStream {
        val conn = current ?: throw RpcException("offline", "hub is not connected")
        val result = conn.call(
            "terminal.open",
            buildJsonObject {
                put("source_id", JsonPrimitive(sourceId))
                put("invocation_id", JsonPrimitive(invocationId))
            },
        )
        val id = (result as? JsonObject)?.get("stream")?.jsonPrimitive?.longOrNull
            ?: throw RpcException("bad_response", "terminal.open answered without a stream")
        return TerminalStream(id, conn.terminalFrames(id), conn)
    }

    /** Sends on the stream's own connection; a stream from a dropped connection is offline. */
    override suspend fun sendInput(stream: TerminalStream, input: TerminalInput) {
        val conn = stream.conn.takeIf { it === current } ?: throw RpcException("offline", "hub is not connected")
        val params = buildJsonObject {
            put("stream", JsonPrimitive(stream.id))
            when (input) {
                is TerminalInput.Keys -> put("keys", JsonArray(input.keys.map(::JsonPrimitive)))
                is TerminalInput.Paste -> put(
                    "paste",
                    buildJsonObject {
                        put("text", JsonPrimitive(input.text))
                        put("enter", JsonPrimitive(input.enter))
                    },
                )
            }
        }
        conn.call("terminal.input", params)
    }

    override suspend fun closeTerminal(stream: TerminalStream) {
        stream.conn.releaseTerminal(stream.id)
        if (stream.conn !== current) return
        stream.conn.call("terminal.close", buildJsonObject { put("stream", JsonPrimitive(stream.id)) })
    }

    private suspend fun run() {
        var failures = 0
        while (true) {
            _state.value = ConnState.Connecting
            var error: String? = null
            try {
                val conn = raceEndpoints(endpoints(), lastGood, clientFor = clientFor)
                current = conn
                try {
                    lastGood = conn.endpoint
                    val info = ProtocolJson.decodeFromJsonElement(HubInfo.serializer(), conn.call("hub.info")!!)
                    listener.onConnected(hubId, conn.endpoint, info)
                    val streamJob = scope.launch { for (envelope in conn.stream) listener.onEnvelope(hubId, envelope) }
                    if (Scopes.READ in info.scopes) {
                        conn.call("listen")
                        _state.value = ConnState.Live(conn.endpoint)
                    } else {
                        _state.value = ConnState.NoAccess(conn.endpoint)
                    }
                    failures = 0
                    backoffMs = initialBackoffMs
                    val close = conn.closed.await()
                    streamJob.join()
                    if (close.code == CLOSE_REVOKED) {
                        revoked()
                        return
                    }
                    if (close.code == CLOSE_SCOPE_WITHDRAWN) {
                        // re-paired with other scopes: reconnect at once and re-read hub.info
                        backoffMs = initialBackoffMs
                        continue
                    }
                    error = close.reason.ifEmpty { close.error?.message }
                } finally {
                    current = null
                    conn.close()
                }
            } catch (e: CancellationException) {
                throw e
            } catch (e: RpcException) {
                if (e.code == "unauthorized") {
                    revoked()
                    return
                }
                error = e.message
            } catch (e: Throwable) {
                error = e.message
            }
            failures++
            val wait = backoffMs
            _state.value = ConnState.Reconnecting(now() + wait, failures, error)
            withTimeoutOrNull(wait) { wake.receive() }
            backoffMs = min(backoffMs * 2, maxBackoffMs)
        }
    }

    private suspend fun revoked() {
        _state.value = ConnState.Revoked
        listener.onRevoked(hubId)
    }

    companion object {
        const val MAX_BACKOFF_MS = 60_000L
    }
}
