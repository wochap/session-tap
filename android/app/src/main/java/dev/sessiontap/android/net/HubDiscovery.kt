package dev.sessiontap.android.net

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.ReceiveChannel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import java.net.Inet6Address
import java.net.InetAddress

/** DNS-SD service type announced by hubs with `remote.discovery`. */
const val SERVICE_TYPE = "_sessiontap._tcp"

/** Most discovered candidates tried per browse. */
const val MAX_DISCOVERED = 8

/** How long one browse runs. */
const val BROWSE_MS = 10_000L

/** One platform browse for [SERVICE_TYPE]: reports resolved addresses until cancelled. */
fun interface Browser {
    suspend fun browse(found: (InetAddress, Int) -> Unit)
}

/** `ip:port`, with IPv6 in brackets, or null for link-local addresses. */
fun candidateEndpoint(address: InetAddress, port: Int): String? {
    if (address.isLinkLocalAddress || address.isLoopbackAddress || address.isAnyLocalAddress) return null
    if (address is Inet6Address) return "[${compressV6(address.address)}]:$port"
    val host = address.hostAddress ?: return null
    return "$host:$port"
}

/** RFC 5952 text form, as the hub writes its hints, so candidates dedupe against them. */
private fun compressV6(bytes: ByteArray): String {
    val groups = (0 until 8).map { ((bytes[2 * it].toInt() and 0xff) shl 8) or (bytes[2 * it + 1].toInt() and 0xff) }
    var bestStart = -1
    var bestLength = 1
    var start = -1
    for (i in 0..8) {
        if (i < 8 && groups[i] == 0) {
            if (start < 0) start = i
        } else if (start >= 0) {
            if (i - start > bestLength) {
                bestStart = start
                bestLength = i - start
            }
            start = -1
        }
    }
    val hex = groups.map { it.toString(16) }
    if (bestStart < 0) return hex.joinToString(":")
    val head = hex.subList(0, bestStart).joinToString(":")
    val tail = hex.subList(bestStart + bestLength, 8).joinToString(":")
    return "$head::$tail"
}

/**
 * One bounded browse shared by every reconnecting hub. [request] joins the
 * running browse or starts one when [eligible] (Wi-Fi or Ethernet), and
 * returns its candidates, or null when the app must not browse. Each hub
 * checks candidates against its own pin, so discovery grants no trust.
 */
class HubDiscovery(
    private val scope: CoroutineScope,
    private val browser: Browser,
    private val eligible: () -> Boolean,
    private val browseMs: Long = BROWSE_MS,
    private val maxCandidates: Int = MAX_DISCOVERED,
) {
    private class Browse(val found: List<String> = emptyList(), val done: Boolean = false)

    private val lock = Any()
    private var current: MutableStateFlow<Browse>? = null
    private var job: Job? = null

    /** Browses started so far. */
    @Volatile var browses = 0
        private set

    fun request(): ReceiveChannel<String>? {
        val state = synchronized(lock) {
            current?.takeIf { !it.value.done } ?: run {
                if (!eligible()) return null
                start()
            }
        }
        val out = Channel<String>(Channel.UNLIMITED)
        scope.launch {
            var sent = 0
            try {
                while (true) {
                    val browse = state.first { it.found.size > sent || it.done }
                    browse.found.drop(sent).forEach { out.trySend(it) }
                    sent = browse.found.size
                    if (browse.done) break
                }
            } finally {
                out.close()
            }
        }
        return out
    }

    private fun start(): MutableStateFlow<Browse> {
        val state = MutableStateFlow(Browse())
        current = state
        browses++
        job = scope.launch {
            try {
                withTimeoutOrNull(browseMs) {
                    browser.browse { address, port ->
                        val endpoint = candidateEndpoint(address, port) ?: return@browse
                        synchronized(lock) {
                            val found = state.value.found
                            if (found.size < maxCandidates && endpoint !in found) state.value = Browse(found + endpoint)
                        }
                    }
                }
            } catch (_: Exception) {
                // discovery only adds candidates; stored hints still race
            } finally {
                synchronized(lock) { state.value = Browse(state.value.found, done = true) }
            }
        }
        return state
    }

    fun stop() {
        job?.cancel()
    }
}
