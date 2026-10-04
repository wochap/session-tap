package dev.sessiontap.android.crypto

import dev.sessiontap.android.net.HubTls
import dev.sessiontap.android.net.TestHub
import dev.sessiontap.android.net.UnreachableException
import dev.sessiontap.android.net.raceEndpoints
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import javax.net.ssl.SSLException

class PinningTest {
    private val hub = TestHub()

    @After
    fun tearDown() = hub.shutdown()

    /** Same vector as `pairing_vector_matches_android` in crates/sessiontap-hub/src/service.rs. */
    @Test
    fun macAndFingerprintMatchHubImplementation() {
        val secret = ByteArray(32) { it.toByte() }
        val mac = pairingMac(secret, "hub-spki".toByteArray(), "device-spki".toByteArray(), ByteArray(32) { 0xaa.toByte() })
        assertEquals("e7bfbc2f5bb0ccb26433c010b012b0d2cb34cf81b76b2f80c7328371011ff41f", mac.hex())
        assertEquals(listOf("781b1751", "7a877c9a", "5199a93c", "45e3a9b7"), fingerprintGroups("device-spki".toByteArray()))
    }

    @Test
    fun acceptsPinnedHubAndRecordsSpki() = runBlocking {
        hub.enqueue { _, _, _ -> }
        val conn = withTimeout(10_000) { raceEndpoints(listOf(hub.endpoint), null) { HubTls.client(hub.hubId, hub.keyManager()) } }
        assertArrayEquals(hub.serverCert.certificate.publicKey.encoded, conn.trust!!.serverSpki)
        conn.close()
    }

    @Test
    fun rejectsMismatchedSpkiBeforeAnyMessage() = runBlocking {
        hub.enqueue { _, _, _ -> fail("no frame may reach a hub with the wrong key") }
        try {
            withTimeout(10_000) { raceEndpoints(listOf(hub.endpoint), null) { HubTls.client("0".repeat(64), hub.keyManager()) } }
            fail("expected the pin to reject the server")
        } catch (e: UnreachableException) {
            assertTrue(e.failures.single().error.let { it is SSLException || it.cause is SSLException })
        }
        assertEquals(0, hub.server.requestCount)
    }
}
