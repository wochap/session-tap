package dev.sessiontap.android.net

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.toList
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import okhttp3.tls.HandshakeCertificates
import okhttp3.tls.HeldCertificate
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger
import javax.net.ssl.SSLServerSocket
import javax.net.ssl.SSLSocket

class DiscoveryTest {
    private val hub = TestHub()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val closeables = mutableListOf<AutoCloseable>()

    @After
    fun tearDown() {
        scope.cancel()
        hub.shutdown()
        closeables.forEach { runCatching { it.close() } }
    }

    private class Recorder : HubClientListener {
        val connected = Channel<String>(Channel.UNLIMITED)
        override suspend fun onConnected(hubId: String, endpoint: String, info: HubInfo) { connected.send(endpoint) }
        override suspend fun onEnvelope(hubId: String, envelope: HubEnvelope) {}
        override suspend fun onRevoked(hubId: String) {}
    }

    private fun serveInfo() = hub.enqueue { ws, id, method ->
        when (method) {
            "hub.info" -> ws.send(hub.info(id))
            "listen" -> ws.send("""{"id":$id,"result":{}}""")
        }
    }

    private fun candidates(vararg endpoints: String): Channel<String> =
        Channel<String>(Channel.UNLIMITED).also { channel -> endpoints.forEach { channel.trySend(it) }; channel.close() }

    /** A TLS server with another key that records whether a client certificate or any data arrived. */
    private inner class StrangerHub {
        val cert: HeldCertificate = HeldCertificate.Builder().ecdsa256().commonName("friend").build()
        private val socket: SSLServerSocket
        val sawClientCert = AtomicBoolean(false)
        val sawData = AtomicBoolean(false)
        val handshakes = AtomicInteger(0)

        init {
            val certs = HandshakeCertificates.Builder().heldCertificate(cert).addTrustedCertificate(hub.clientCert.certificate).build()
            socket = certs.sslContext().serverSocketFactory.createServerSocket(0, 50, InetAddress.getLoopbackAddress()) as SSLServerSocket
            socket.wantClientAuth = true
            closeables += socket
            Thread {
                while (!socket.isClosed) {
                    val client = runCatching { socket.accept() as SSLSocket }.getOrNull() ?: break
                    Thread {
                        client.use {
                            handshakes.incrementAndGet()
                            runCatching {
                                it.startHandshake()
                                if (runCatching { it.session.peerCertificates }.getOrNull()?.isNotEmpty() == true) sawClientCert.set(true)
                                if (it.inputStream.read() >= 0) sawData.set(true)
                            }
                        }
                    }.start()
                }
            }.start()
        }

        val endpoint: String get() = "127.0.0.1:${socket.localPort}"
    }

    @Test
    fun newNetworkConnectsThroughDiscoveredCandidate() = runBlocking {
        serveInfo()
        val recorder = Recorder()
        val asked = AtomicInteger(0)
        val client = HubClient(
            hubId = hub.hubId,
            endpoints = { listOf("127.0.0.1:1") }, // stale hint from another network
            lastGood = "127.0.0.1:1",
            listener = recorder,
            scope = scope,
            clientFor = { HubTls.client(hub.hubId, hub.keyManager()) },
            discover = { asked.incrementAndGet(); candidates(hub.endpoint) },
        )
        client.start()
        assertEquals(hub.endpoint, withTimeout(15_000) { recorder.connected.receive() })
        withTimeout(5_000) { client.state.first { it is ConnState.Live } }
        // connected hubs start no browse
        delay(300)
        assertEquals(1, asked.get())
        client.stop()
    }

    @Test
    fun pinMismatchSendsNoClientCertificateOrMessage() = runBlocking {
        val stranger = StrangerHub()
        val error = runCatching {
            withTimeout(10_000) {
                raceEndpoints(emptyList(), null, candidates = candidates(stranger.endpoint)) { HubTls.client(hub.hubId, hub.keyManager()) }
            }
        }.exceptionOrNull()
        assertTrue("$error", error is UnreachableException)
        assertEquals(listOf(stranger.endpoint), (error as UnreachableException).failures.map { it.endpoint })
        delay(200)
        assertEquals(1, stranger.handshakes.get())
        assertFalse(stranger.sawClientCert.get())
        assertFalse(stranger.sawData.get())
    }

    @Test
    fun otherHubOnTheNetworkGetsNothing() = runBlocking {
        serveInfo()
        val friend = StrangerHub()
        val conn = withTimeout(10_000) {
            raceEndpoints(emptyList(), null, candidates = candidates(friend.endpoint, hub.endpoint)) {
                HubTls.client(hub.hubId, hub.keyManager())
            }
        }
        assertEquals(hub.endpoint, conn.endpoint)
        conn.close()
        delay(200)
        assertFalse(friend.sawClientCert.get())
        assertFalse(friend.sawData.get())
    }

    @Test
    fun manyFakeRecordsAreCappedAndDialedFourAtATime() = runBlocking {
        // a listener that accepts and never answers, so every dial stays pending
        val silent = ServerSocket(0, 100, InetAddress.getByName("0.0.0.0"))
        closeables += silent
        val accepted = CopyOnWriteArrayList<Socket>()
        Thread { while (!silent.isClosed) runCatching { accepted += silent.accept() } }.start()
        closeables += AutoCloseable { accepted.forEach { it.close() } }
        val fakes = (1..50).map { "127.0.0.$it:${silent.localPort}" }
        val dials = AtomicInteger(0)
        val race = async {
            runCatching {
                raceEndpoints(emptyList(), null, candidates = candidates(*fakes.toTypedArray())) {
                    dials.incrementAndGet()
                    HubTls.client(hub.hubId, hub.keyManager())
                }
            }
        }
        delay(1_500)
        assertEquals(4, accepted.size)
        assertEquals(4, dials.get())
        race.cancel()

        val discovery = HubDiscovery(
            scope = scope,
            browser = { found -> (1..50).forEach { found(InetAddress.getByName("192.168.5.$it"), 8932) }; awaitCancellation() },
            eligible = { true },
            browseMs = 200,
        )
        val found = withTimeout(5_000) { discovery.request()!!.toList() }
        assertEquals(MAX_DISCOVERED, found.size)
    }

    @Test
    fun oneBrowseServesEveryReconnectingHub() = runBlocking {
        val discovery = HubDiscovery(
            scope = scope,
            browser = { found ->
                delay(50)
                found(InetAddress.getByName("192.168.5.20"), 8932)
                found(InetAddress.getByName("fe80::1"), 8932)
                found(InetAddress.getByName("2001:db8::5"), 8932)
                found(InetAddress.getByName("192.168.5.20"), 8932)
                awaitCancellation()
            },
            eligible = { true },
            browseMs = 300,
        )
        val first = discovery.request()!!
        val second = discovery.request()!!
        val expected = listOf("192.168.5.20:8932", "[2001:db8::5]:8932")
        assertEquals(expected, withTimeout(5_000) { first.toList() })
        assertEquals(expected, withTimeout(5_000) { second.toList() })
        assertEquals(1, discovery.browses)
        // the next attempt browses again
        withTimeout(5_000) { discovery.request()!!.toList() }
        assertEquals(2, discovery.browses)
    }

    @Test
    fun cellularDoesNotBrowse() {
        val discovery = HubDiscovery(scope = scope, browser = { error("must not browse") }, eligible = { false })
        assertNull(discovery.request())
        assertEquals(0, discovery.browses)
    }

    @Test
    fun noHubFoundMarksDiscoveryMissed() = runBlocking {
        val client = HubClient(
            hubId = hub.hubId,
            endpoints = { listOf("127.0.0.1:1") },
            lastGood = null,
            listener = Recorder(),
            scope = scope,
            clientFor = { HubTls.client(hub.hubId, hub.keyManager()) },
            initialBackoffMs = 10_000,
            discover = { candidates() },
        )
        client.start()
        val state = withTimeout(10_000) { client.state.first { it is ConnState.Reconnecting } } as ConnState.Reconnecting
        assertTrue(state.discoveryMissed)
        client.stop()
    }

    @Test
    fun candidateEndpointsSkipLinkLocal() {
        assertEquals("192.168.5.20:8932", candidateEndpoint(InetAddress.getByName("192.168.5.20"), 8932))
        assertEquals("[2001:db8::5]:8932", candidateEndpoint(InetAddress.getByName("2001:db8::5"), 8932))
        assertEquals("[::1:0:0:0:2]:8932", candidateEndpoint(InetAddress.getByName("0:0:0:1:0:0:0:2"), 8932))
        assertEquals("[fd00::]:8932", candidateEndpoint(InetAddress.getByName("fd00::"), 8932))
        assertNull(candidateEndpoint(InetAddress.getByName("fe80::1"), 8932))
        assertNull(candidateEndpoint(InetAddress.getByName("169.254.3.4"), 8932))
    }
}
