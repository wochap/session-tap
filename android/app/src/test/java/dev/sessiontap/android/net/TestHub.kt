package dev.sessiontap.android.net

import dev.sessiontap.android.crypto.SingleKeyManager
import dev.sessiontap.android.crypto.sha256
import dev.sessiontap.android.crypto.hex
import kotlinx.serialization.json.int
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import okhttp3.mockwebserver.MockResponse
import okhttp3.mockwebserver.MockWebServer
import okhttp3.tls.HandshakeCertificates
import okhttp3.tls.HeldCertificate

/** A MockWebServer speaking the hub's TLS + WebSocket protocol. */
class TestHub {
    val serverCert: HeldCertificate = HeldCertificate.Builder().ecdsa256().commonName("hub").build()
    val clientCert: HeldCertificate = HeldCertificate.Builder().ecdsa256().commonName("device").build()
    val server = MockWebServer()
    val hubId: String = sha256(serverCert.certificate.publicKey.encoded).hex()
    val endpoint: String get() = "${server.hostName}:${server.port}"

    init {
        val certs = HandshakeCertificates.Builder()
            .heldCertificate(serverCert)
            .addTrustedCertificate(clientCert.certificate)
            .build()
        server.useHttps(certs.sslSocketFactory(), false)
        server.requestClientAuth()
        server.start()
    }

    fun keyManager() = SingleKeyManager("device", clientCert.keyPair.private, arrayOf(clientCert.certificate))

    /** Queues one connection whose frames go to [handle]; reply with [WebSocket.send]. */
    fun enqueue(handle: (WebSocket, id: Int, method: String) -> Unit) {
        server.enqueue(
            MockResponse().withWebSocketUpgrade(object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {}
                override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
                    webSocket.close(1000, null)
                }
                override fun onMessage(webSocket: WebSocket, text: String) {
                    val obj = ProtocolJson.parseToJsonElement(text).jsonObject
                    handle(webSocket, obj["id"]!!.jsonPrimitive.int, obj["method"]!!.jsonPrimitive.content)
                }
            }),
        )
    }

    fun info(id: Int, endpoints: List<String> = listOf(endpoint)): String {
        val ep = endpoints.joinToString(",") { "\"$it\"" }
        return """{"id":$id,"result":{"hub_id":"$hubId","hub_name":"TestHub","protocol":1,"scopes":["read","manage"],"endpoints":[$ep]}}"""
    }

    fun snapshot(revision: Long, vararg statuses: String): String {
        val agents = statuses.mapIndexed { i, s ->
            """{"source_id":"host","view":{"invocation_id":"inv-$i","provider":"claude","status":"$s","cwd":"/home/me","created_at":"2026-10-03T14:00:00Z","updated_at":"2026-10-03T14:00:00Z"}}"""
        }
        return """{"event":"stream","data":{"type":"snapshot","hub_revision":$revision,"sources":[],"agents":[${agents.joinToString(",")}]}}"""
    }

    fun shutdown() = server.shutdown()
}
