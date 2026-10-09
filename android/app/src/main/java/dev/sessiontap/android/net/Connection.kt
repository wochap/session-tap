package dev.sessiontap.android.net

import dev.sessiontap.android.crypto.PinnedTrustManager
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.ReceiveChannel
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import okhttp3.OkHttpClient
import okhttp3.Request as HttpRequest
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import kotlinx.coroutines.sync.Semaphore
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import java.util.concurrent.atomic.AtomicLong

class RpcException(val code: String, message: String) : Exception("$code: $message")

/** Why a connection ended. 4401 means the hub revoked this device. */
data class CloseInfo(val code: Int, val reason: String, val error: Throwable? = null)

const val CLOSE_REVOKED = 4401

/** Close code: an open stream's scope was withdrawn by re-pairing. */
const val CLOSE_SCOPE_WITHDRAWN = 4403

/** One open WebSocket to a hub with request id correlation. */
class Connection internal constructor(val endpoint: String, val trust: PinnedTrustManager?) {
    internal lateinit var socket: WebSocket
    @Volatile internal var won = false
    private val nextId = AtomicLong(1)
    private val pending = ConcurrentHashMap<Long, CompletableDeferred<Incoming.Response>>()
    val stream = Channel<HubEnvelope>(Channel.UNLIMITED)
    /** Frames per terminal stream id. A frame may arrive before its open call returns, so channels are made on demand. */
    private val terminals = ConcurrentHashMap<Long, Channel<TerminalFrame>>()
    val closed = CompletableDeferred<CloseInfo>()

    suspend fun call(method: String, params: JsonObject? = null, timeoutMs: Long = 15_000): JsonElement? {
        val id = nextId.getAndIncrement()
        val reply = CompletableDeferred<Incoming.Response>()
        pending[id] = reply
        try {
            val text = ProtocolJson.encodeToString(Request.serializer(), Request(id, method, params))
            if (!socket.send(text)) throw RpcException("closed", "connection closed")
            val response = withTimeout(timeoutMs) { reply.await() }
            response.error?.let { throw RpcException(it.code, it.message) }
            return response.result
        } finally {
            pending.remove(id)
        }
    }

    fun close() {
        socket.close(1000, null)
    }

    internal fun onText(text: String) {
        when (val frame = parseIncoming(text)) {
            is Incoming.Response -> pending[frame.id]?.complete(frame)
            is Incoming.Stream -> stream.trySend(frame.envelope)
            is Incoming.Terminal -> terminalChannel(frame.stream).trySend(frame.frame)
            null -> {}
        }
    }

    private fun terminalChannel(id: Long): Channel<TerminalFrame> =
        terminals.computeIfAbsent(id) { Channel<TerminalFrame>(Channel.UNLIMITED).also { if (closed.isCompleted) it.close() } }

    /** Frames of terminal stream [id], in order; closes when the connection closes. */
    fun terminalFrames(id: Long): ReceiveChannel<TerminalFrame> = terminalChannel(id)

    /** Drops the frames of a stream the app no longer shows. */
    fun releaseTerminal(id: Long) {
        terminals.remove(id)?.close()
    }

    internal fun onClosed(info: CloseInfo) {
        closed.complete(info)
        stream.close()
        terminals.values.forEach { it.close() }
        val failure = RpcException("closed", info.reason.ifEmpty { "connection closed" })
        pending.values.forEach { it.completeExceptionally(failure) }
    }
}

/** Outcome of trying one endpoint hint. */
data class EndpointFailure(val endpoint: String, val error: Throwable)

class UnreachableException(val failures: List<EndpointFailure>) :
    Exception("no endpoint answered: " + failures.joinToString { "${it.endpoint} (${it.error.message})" })

/** Most endpoints one race dials at once, below the hub's per-address limit of 4 unauthenticated connections. */
const val MAX_CONCURRENT_DIALS = 4

/**
 * Opens a WebSocket to every endpoint, the preferred one first, and keeps the
 * first that completes TLS with the pinned key. The rest are cancelled.
 * [candidates] (from discovery) join the race as they arrive; ones already in
 * [endpoints] are skipped, and at most [MAX_DISCOVERED] are tried. At most
 * [maxConcurrent] dials run at once. A candidate with another key fails in
 * the TLS handshake before the client certificate is sent.
 */
suspend fun raceEndpoints(
    endpoints: List<String>,
    preferred: String?,
    headStartMs: Long = 300,
    candidates: ReceiveChannel<String>? = null,
    maxConcurrent: Int = MAX_CONCURRENT_DIALS,
    clientFor: () -> Pair<OkHttpClient, PinnedTrustManager?>,
): Connection = coroutineScope {
    val ordered = endpoints.map { it.trim() }.distinct().sortedBy { if (it == preferred) 0 else 1 }
    val winner = CompletableDeferred<Connection>()
    val decided = AtomicBoolean(false)
    val failures = ConcurrentHashMap<String, Throwable>()
    val tried = CopyOnWriteArrayList<String>()
    val sockets = ConcurrentHashMap<String, WebSocket>()
    val jobs = CopyOnWriteArrayList<Job>()
    val dialing = AtomicInteger(0)
    val sourceOpen = AtomicBoolean(candidates != null)
    val permits = Semaphore(maxConcurrent)
    fun exhausted() {
        if (dialing.get() == 0 && !sourceOpen.get() && !decided.get()) {
            winner.completeExceptionally(UnreachableException(tried.map { EndpointFailure(it, failures[it]!!) }))
        }
    }
    fun failed(endpoint: String, error: Throwable) {
        failures[endpoint] = error
        dialing.decrementAndGet()
        exhausted()
    }
    fun dial(endpoint: String, waitMs: Long) {
        tried += endpoint
        dialing.incrementAndGet()
        jobs += launch {
            if (waitMs > 0) delay(waitMs)
            permits.acquire()
            val released = AtomicBoolean(false)
            fun release() {
                if (released.compareAndSet(false, true)) permits.release()
            }
            if (decided.get()) {
                release()
                return@launch
            }
            val (client, trust) = clientFor()
            val conn = Connection(endpoint, trust)
            val listener = object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {
                    release()
                    if (decided.compareAndSet(false, true)) {
                        conn.won = true
                        winner.complete(conn)
                    } else {
                        webSocket.close(1000, null)
                    }
                }
                override fun onMessage(webSocket: WebSocket, text: String) = conn.onText(text)
                override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                    webSocket.close(1000, null)
                    conn.onClosed(CloseInfo(code, reason))
                }
                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) = conn.onClosed(CloseInfo(code, reason))
                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    release()
                    if (conn.won) {
                        conn.onClosed(CloseInfo(1006, t.message ?: "failure", t))
                    } else {
                        failed(endpoint, t)
                    }
                }
            }
            val url = "wss://$endpoint/"
            conn.socket = client.newWebSocket(HttpRequest.Builder().url(url).build(), listener)
            sockets[endpoint] = conn.socket
        }
    }
    ordered.forEachIndexed { index, endpoint ->
        dial(endpoint, if (index > 0 && ordered[0] == preferred) headStartMs else 0)
    }
    if (candidates != null) {
        jobs += launch {
            var taken = 0
            for (candidate in candidates) {
                if (decided.get() || taken >= MAX_DISCOVERED) break
                val endpoint = candidate.trim()
                if (endpoint in tried) continue
                taken++
                dial(endpoint, 0)
            }
            sourceOpen.set(false)
            exhausted()
        }
    } else {
        exhausted()
    }
    try {
        val conn = winner.await()
        sockets.forEach { (endpoint, socket) -> if (endpoint != conn.endpoint) socket.cancel() }
        jobs.forEach { it.cancel() }
        candidates?.cancel()
        conn
    } catch (e: Throwable) {
        sockets.values.forEach { it.cancel() }
        jobs.forEach { it.cancel() }
        candidates?.cancel()
        throw e
    }
}

/** Starts a coroutine that hands stream envelopes, in order, to [handler]. */
fun CoroutineScope.consumeStream(conn: Connection, handler: suspend (HubEnvelope) -> Unit): Job = launch {
    for (envelope in conn.stream) handler(envelope)
}
