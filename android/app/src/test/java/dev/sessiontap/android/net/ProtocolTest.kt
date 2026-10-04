package dev.sessiontap.android.net

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/** Fixtures copied from docs/hub.md. */
class ProtocolTest {
    private val snapshotLine =
        """{"type":"snapshot","hub_revision":57,"sources":[{"source_id":"host","display_name":"Host machine","revision":120}],"agents":[{"source_id":"host","view":{"invocation_id":"...","provider":"codex","status":"idle"}}]}"""
    private val updateLine =
        """{"type":"update","hub_revision":58,"source_id":"sandbox","delivery_id":"...","source_revision":42,"changed":["status","reason"],"view":{"invocation_id":"...","provider":"codex","status":"blocked","reason":{"kind":"input","summary":"..."}}}"""
    private val qr =
        """{"v":1,"hub":"MacBook","id":"<hub_id>","ep":["100.64.0.7:8932","192.168.1.20:8932","macbook.tailnet.ts.net:8932"],"sc":["read","manage"],"s":"<base64url secret>","exp":1767225600}"""

    @Test
    fun parsesSnapshotStreamEvent() {
        val frame = parseIncoming("""{"event":"stream","data":$snapshotLine}""") as Incoming.Stream
        val snapshot = frame.envelope as HubEnvelope.Snapshot
        assertEquals(57, snapshot.hubRevision)
        assertEquals("Host machine", snapshot.sources.single().displayName)
        assertEquals(Status.Idle, snapshot.agents.single().view.status)
        assertEquals("host", snapshot.agents.single().sourceId)
    }

    @Test
    fun parsesUpdateStreamEvent() {
        val frame = parseIncoming("""{"event":"stream","data":$updateLine}""") as Incoming.Stream
        val update = frame.envelope as HubEnvelope.Update
        assertEquals("sandbox", update.sourceId)
        assertEquals(listOf("status", "reason"), update.changed)
        assertEquals(ReasonKind.Input, update.view.reason?.kind)
        assertEquals(Status.Blocked, update.view.status)
    }

    @Test
    fun parsesResponsesAndErrors() {
        val ok = parseIncoming("""{"id":1,"result":{"hub_id":"ab","hub_name":"MacBook","protocol":1,"scopes":["read"]}}""") as Incoming.Response
        assertEquals(1, ok.id)
        assertNull(ok.error)
        val info = ProtocolJson.decodeFromJsonElement(HubInfo.serializer(), ok.result!!)
        assertEquals("MacBook", info.hubName)
        val err = parseIncoming("""{"id":2,"error":{"code":"forbidden","message":"needs manage"}}""") as Incoming.Response
        assertEquals("forbidden", err.error?.code)
    }

    @Test
    fun parsesFullPublicAgentView() {
        val json = """{"invocation_id":"7f3c","provider":"claude","status":"running","cwd":"/home/me/x","created_at":"2026-10-03T14:00:00.123456Z","updated_at":"2026-10-03T14:01:00Z",
            "session":{"id":"s1","name":"Fix"},"metadata":{"model":"opus","effort":"high","permission_mode":"default"},
            "usage":{"input_tokens":15300,"output_tokens":2100,"context_tokens":84000,"context_window_percent":42},
            "repository":{"root":"/home/me/x","branch":"main","head":"a41f9c2","dirty":true},
            "children":[{"agent_id":"c1","agent_type":"Explore","status":"blocked","reason":{"kind":"approval","summary":"find"},"started_at":"2026-10-03T14:00:30Z","updated_at":"2026-10-03T14:00:40Z"}],
            "future_field":1}"""
        val view = ProtocolJson.decodeFromString(AgentView.serializer(), json)
        assertEquals(42, view.usage?.contextWindowPercent)
        assertEquals("default", view.metadata?.permissionMode)
        assertEquals(ReasonKind.Approval, view.children!!.single().reason?.kind)
        assertTrue(view.repository!!.dirty!!)
    }

    @Test
    fun parsesQrPayloadShape() {
        val payload = ProtocolJson.decodeFromString(QrPayload.serializer(), qr)
        assertEquals("MacBook", payload.hub)
        assertEquals(3, payload.ep.size)
        assertEquals(listOf("read", "manage"), payload.sc)
    }

    @Test
    fun ignoresGarbage() {
        assertNull(parseIncoming("not json"))
        assertNull(parseIncoming("""{"event":"stream","data":{"type":"weird"}}"""))
    }
}
