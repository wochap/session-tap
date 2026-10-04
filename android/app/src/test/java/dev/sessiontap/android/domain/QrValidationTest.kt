package dev.sessiontap.android.domain

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class QrValidationTest {
    private val id = "a".repeat(64)
    private val secret = encodeBase64Url(ByteArray(32) { it.toByte() })
    private val now = 1_767_225_000L

    private fun qr(v: Int = 1, hubId: String = id, ep: String = """["100.64.0.7:8932","192.168.1.20:8932"]""", s: String = secret, exp: Long = now + 100) =
        """{"v":$v,"hub":"MacBook","id":"$hubId","ep":$ep,"sc":["read","manage"],"s":"$s","exp":$exp}"""

    @Test
    fun valid() {
        val result = validateQr(qr(), now) as QrResult.Valid
        assertEquals(32, result.value.secret.size)
        assertEquals(2, result.value.payload.ep.size)
        assertTrue(validateQr(qr(ep = """["[fd7a::1]:8932"]"""), now) is QrResult.Valid)
    }

    @Test
    fun expired() {
        assertEquals(QrResult.Expired, validateQr(qr(exp = now - 1), now))
        assertEquals(QrResult.Expired, validateQr(qr(exp = now), now))
    }

    @Test
    fun malformed() {
        listOf(
            "hello",
            qr(v = 2),
            qr(hubId = "XYZ"),
            qr(ep = "[]"),
            qr(ep = """["nohost"]"""),
            qr(ep = """["host:99999"]"""),
            qr(s = encodeBase64Url(ByteArray(16))),
            qr(s = "!!!"),
            """{"v":1}""",
        ).forEach { assertTrue(it, validateQr(it, now) is QrResult.Invalid) }
    }
}
