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
import kotlinx.serialization.json.JsonObject
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
        val infos = Channel<HubInfo>(Channel.UNLIMITED)
        override suspend fun onConnected(hubId: String, endpoint: String, info: HubInfo) {
            infos.send(info)
            connected.send(info.hubName)
        }
        override suspend fun onEnvelope(hubId: String, envelope: HubEnvelope) { envelopes.send(envelope) }
        override suspend fun onRevoked(hubId: String) { revoked.send(hubId) }
    }

    private fun client(listener: HubClientListener, initialBackoffMs: Long = 50) = HubClient(
        hubId = hub.hubId,
        endpoints = { listOf(hub.endpoint) },
        lastGood = null,
        listener = listener,
        scope = scope,
        clientFor = { HubTls.client(hub.hubId, hub.keyManager()) },
        initialBackoffMs = initialBackoffMs,
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
    fun scopeWithdrawnReconnectsAtOnce() = runBlocking {
        hub.enqueue { ws, id, method ->
            when (method) {
                "hub.info" -> ws.send(hub.info(id))
                "listen" -> {
                    ws.send("""{"id":$id,"result":{}}""")
                    ws.close(4403, "scope withdrawn")
                }
            }
        }
        hub.enqueue { ws, id, method ->
            when (method) {
                "hub.info" -> ws.send(hub.info(id, scopes = listOf("read")))
                "listen" -> ws.send("""{"id":$id,"result":{}}""")
            }
        }
        val rec = Recorder()
        // a normal retry would wait 30s; the second connection must come at once
        val c = client(rec, initialBackoffMs = 30_000)
        c.start()
        withTimeout(10_000) {
            assertEquals(listOf("read", "manage"), rec.infos.receive().scopes)
            assertEquals(listOf("read"), rec.infos.receive().scopes)
            c.state.first { it is ConnState.Live }
        }
        c.stop()
        assertEquals(2, hub.server.requestCount)
    }

    @Test
    fun noListenWithoutReadScope() = runBlocking {
        val methods = Channel<String>(Channel.UNLIMITED)
        hub.enqueue { ws, id, method ->
            methods.trySend(method)
            when (method) {
                "hub.info" -> ws.send(hub.info(id, scopes = listOf("manage")))
                "listen" -> ws.send("""{"id":$id,"result":{}}""")
            }
        }
        val rec = Recorder()
        val c = client(rec)
        c.start()
        withTimeout(10_000) { c.state.first { it is ConnState.NoAccess } }
        Thread.sleep(300)
        c.stop()
        assertEquals("hub.info", methods.receive())
        assertTrue(methods.tryReceive().isFailure)
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
    fun hubInfoEndpointsReachOnConnected() = runBlocking {
        val reported = listOf("192.168.1.37:8932", "macbook.tailnet.ts.net:8932")
        hub.enqueue { ws, id, method ->
            when (method) {
                "hub.info" -> ws.send(hub.info(id, reported))
                "listen" -> ws.send("""{"id":$id,"result":{}}""")
            }
        }
        val rec = Recorder()
        val c = client(rec)
        c.start()
        withTimeout(10_000) { assertEquals(reported, rec.infos.receive().endpoints) }
        c.stop()
    }

    @Test
    fun pinMismatchReportsNoEndpoints() = runBlocking {
        hub.enqueue { ws, id, method -> if (method == "hub.info") ws.send(hub.info(id, listOf("evil.example:8932"))) }
        val wrongId = "0".repeat(64)
        val rec = Recorder()
        val c = HubClient(wrongId, { listOf(hub.endpoint) }, null, rec, scope, { HubTls.client(wrongId, hub.keyManager()) }, initialBackoffMs = 50)
        c.start()
        withTimeout(10_000) { c.state.first { it is ConnState.Reconnecting } }
        c.stop()
        assertTrue(rec.infos.tryReceive().isFailure)
        assertEquals(0, hub.server.requestCount)
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

    @Test
    fun terminalFramesRouteByStreamAndCloseOnLeave() = runBlocking {
        val requests = Channel<JsonObject>(Channel.UNLIMITED)
        hub.enqueueRequests { ws, req ->
            requests.trySend(req)
            val id = req["id"]!!.jsonPrimitive.content
            when (req["method"]!!.jsonPrimitive.content) {
                "hub.info" -> ws.send(hub.info(id.toInt(), scopes = listOf("read", "watch", "control")))
                "listen" -> ws.send("""{"id":$id,"result":{}}""")
                "terminal.open" -> {
                    val stream = if (req["params"]!!.jsonObject["invocation_id"]!!.jsonPrimitive.content == "a") 1 else 2
                    ws.send("""{"id":$id,"result":{"stream":$stream}}""")
                    ws.send("""{"type":"terminal","stream":$stream,"frame":{"type":"output","seq":1,"data":"${java.util.Base64.getEncoder().encodeToString("s$stream".toByteArray())}"}}""")
                }
                else -> ws.send("""{"id":$id,"result":{}}""")
            }
        }
        val c = client(Recorder())
        c.start()
        withTimeout(10_000) {
            c.state.first { it is ConnState.Live }
            val a = c.openTerminal("host", "a")
            val b = c.openTerminal("host", "b")
            assertEquals("s1", (a.frames.receive() as TerminalFrame.Output).bytes.decodeToString())
            assertEquals("s2", (b.frames.receive() as TerminalFrame.Output).bytes.decodeToString())
            c.sendInput(a, TerminalInput.Keys(listOf("down", "1")))
            c.sendInput(a, TerminalInput.Paste("see CI run 4821", enter = false))
            c.closeTerminal(a)
            val methods = mutableListOf<JsonObject>()
            while (methods.none { it["method"]!!.jsonPrimitive.content == "terminal.close" }) methods += requests.receive()
            val inputs = methods.filter { it["method"]!!.jsonPrimitive.content == "terminal.input" }.map { it["params"]!!.jsonObject }
            assertEquals("""{"stream":1,"keys":["down","1"]}""", inputs[0].toString())
            assertEquals("""{"stream":1,"paste":{"text":"see CI run 4821","enter":false}}""", inputs[1].toString())
            val close = methods.last { it["method"]!!.jsonPrimitive.content == "terminal.close" }
            assertEquals("""{"stream":1}""", close["params"].toString())
            assertTrue(a.frames.isClosedForReceive)
        }
        c.stop()
    }

    @Test
    fun terminalCallsFailOfflineWithoutConnection() = runBlocking {
        val c = HubClient(hub.hubId, { listOf("127.0.0.1:1") }, null, Recorder(), scope, { HubTls.client(hub.hubId, hub.keyManager()) }, initialBackoffMs = 10_000)
        val error = runCatching { c.openTerminal("host", "a") }.exceptionOrNull() as RpcException
        assertEquals("offline", error.code)
    }

    @Test
    fun terminalFramesCloseWhenConnectionDrops() = runBlocking {
        hub.enqueueRequests { ws, req ->
            val id = req["id"]!!.jsonPrimitive.content
            when (req["method"]!!.jsonPrimitive.content) {
                "hub.info" -> ws.send(hub.info(id.toInt(), scopes = listOf("read", "watch")))
                "terminal.open" -> {
                    ws.send("""{"id":$id,"result":{"stream":5}}""")
                    ws.close(1001, "going away")
                }
                else -> ws.send("""{"id":$id,"result":{}}""")
            }
        }
        val c = client(Recorder(), initialBackoffMs = 30_000)
        c.start()
        withTimeout(10_000) {
            c.state.first { it is ConnState.Live }
            val s = c.openTerminal("host", "a")
            for (frame in s.frames) error("unexpected frame $frame")
            val sendError = runCatching { c.sendInput(s, TerminalInput.Keys(listOf("enter"))) }.exceptionOrNull() as RpcException
            // the dropped connection either is already gone (offline) or fails the send (closed)
            assertTrue(sendError.code, sendError.code in setOf("offline", "closed"))
        }
        c.stop()
    }
}
