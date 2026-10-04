package dev.sessiontap.android.net

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class HubClientTest {
    private val hub = TestHub()
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    @After
    fun tearDown() {
        scope.cancel()
        hub.shutdown()
    }

    private class Recorder : HubClientListener {
        val connected = Channel<String>(Channel.UNLIMITED)
        val envelopes = Channel<HubEnvelope>(Channel.UNLIMITED)
        val revoked = Channel<String>(Channel.UNLIMITED)
        override suspend fun onConnected(hubId: String, endpoint: String, info: HubInfo) { connected.send(info.hubName) }
        override suspend fun onEnvelope(hubId: String, envelope: HubEnvelope) { envelopes.send(envelope) }
        override suspend fun onRevoked(hubId: String) { revoked.send(hubId) }
    }

    private fun client(listener: HubClientListener) = HubClient(
        hubId = hub.hubId,
        endpoints = { listOf(hub.endpoint) },
        lastGood = null,
        listener = listener,
        scope = scope,
        clientFor = { HubTls.client(hub.hubId, hub.keyManager()) },
        initialBackoffMs = 50,
    )

    @Test
    fun correlatesResponsesById() = runBlocking {
        hub.enqueue { ws, id, method ->
            // Answer the first request only after the second one arrives, so replies come back out of order.
            if (method == "slow") Thread { Thread.sleep(300); ws.send("""{"id":$id,"result":{"which":"slow"}}""") }.start()
            else ws.send("""{"id":$id,"result":{"which":"fast"}}""")
        }
        val conn = withTimeout(10_000) { raceEndpoints(listOf(hub.endpoint), null) { HubTls.client(hub.hubId, hub.keyManager()) } }
        val slow = async { conn.call("slow") }
        val fast = async { conn.call("fast") }
        assertEquals("fast", fast.await()!!.jsonObject["which"]!!.jsonPrimitive.content)
        assertEquals("slow", slow.await()!!.jsonObject["which"]!!.jsonPrimitive.content)
        conn.close()
    }

    @Test
    fun listensAndReconnectsAfterDrop() = runBlocking {
        repeat(2) { round ->
            hub.enqueue { ws, id, method ->
                when (method) {
                    "hub.info" -> ws.send(hub.info(id))
                    "listen" -> {
                        ws.send("""{"id":$id,"result":{}}""")
                        ws.send(hub.snapshot(round + 1L, "running"))
                        if (round == 0) ws.close(1001, "going away")
                    }
                }
            }
        }
        val rec = Recorder()
        val c = client(rec)
        c.start()
        withTimeout(15_000) {
            assertEquals("TestHub", rec.connected.receive())
            assertEquals(1L, rec.envelopes.receive().hubRevision)
            assertEquals("TestHub", rec.connected.receive())
            assertEquals(2L, rec.envelopes.receive().hubRevision)
            c.state.first { it is ConnState.Live }
        }
        c.stop()
    }

    @Test
    fun revokedOnCloseCode4401() = runBlocking {
        hub.enqueue { ws, id, method ->
            when (method) {
                "hub.info" -> ws.send(hub.info(id))
                "listen" -> {
                    ws.send("""{"id":$id,"result":{}}""")
                    ws.close(4401, "device revoked")
                }
            }
        }
        val rec = Recorder()
        val c = client(rec)
        c.start()
        withTimeout(10_000) {
            assertEquals(hub.hubId, rec.revoked.receive())
            c.state.first { it == ConnState.Revoked }
        }
        Thread.sleep(300)
        assertEquals(1, hub.server.requestCount)
    }

    @Test
    fun revokedOnUnauthorizedError() = runBlocking {
        hub.enqueue { ws, id, _ -> ws.send("""{"id":$id,"error":{"code":"unauthorized","message":"this device is not paired"}}""") }
        val rec = Recorder()
        val c = client(rec)
        c.start()
        withTimeout(10_000) { assertEquals(hub.hubId, rec.revoked.receive()) }
        assertTrue(c.state.value == ConnState.Revoked)
    }

    @Test
    fun backoffDoublesAndCaps() = runBlocking {
        // No queued responses: every attempt fails the upgrade.
        val rec = Recorder()
        val c = HubClient(hub.hubId, { listOf("127.0.0.1:1") }, null, rec, scope, { HubTls.client(hub.hubId, hub.keyManager()) }, initialBackoffMs = 10, maxBackoffMs = 40)
        c.start()
        val waits = mutableListOf<Long>()
        withTimeout(10_000) {
            var last = 0
            while (waits.size < 4) {
                val s = c.state.first { it is ConnState.Reconnecting && it.failures > last } as ConnState.Reconnecting
                last = s.failures
                waits += s.retryAt - System.currentTimeMillis()
            }
        }
        c.stop()
        assertTrue(waits.toString(), waits.last() <= 40)
        assertTrue(waits.toString(), waits[2] > waits[0])
    }
}
