package dev.sessiontap.android.crypto

import android.annotation.SuppressLint
import java.security.MessageDigest
import java.security.cert.CertificateException
import java.security.cert.X509Certificate
import javax.crypto.Mac
import javax.crypto.spec.SecretKeySpec
import javax.net.ssl.X509TrustManager

fun sha256(bytes: ByteArray): ByteArray = MessageDigest.getInstance("SHA-256").digest(bytes)

fun ByteArray.hex(): String = joinToString("") { "%02x".format(it) }

/** DER SubjectPublicKeyInfo of a certificate. */
fun spkiOf(cert: X509Certificate): ByteArray = cert.publicKey.encoded

fun spkiSha256Hex(cert: X509Certificate): String = sha256(spkiOf(cert)).hex()

/** First 16 bytes of sha256(SPKI) as four groups of eight hex characters, as the hub prints. */
fun fingerprintGroups(spki: ByteArray): List<String> =
    sha256(spki).copyOfRange(0, 16).toList().chunked(4).map { it.toByteArray().hex() }

const val PAIR_LABEL = "sessiontap-pair-v1"

/** HMAC-SHA256(secret, label || hub_spki || device_spki || nonce). */
fun pairingMac(secret: ByteArray, hubSpki: ByteArray, deviceSpki: ByteArray, nonce: ByteArray): ByteArray {
    val mac = Mac.getInstance("HmacSHA256")
    mac.init(SecretKeySpec(secret, "HmacSHA256"))
    mac.update(PAIR_LABEL.toByteArray())
    mac.update(hubSpki)
    mac.update(deviceSpki)
    mac.update(nonce)
    return mac.doFinal()
}

/**
 * Trusts exactly one server leaf: the one whose SPKI hash equals the hub ID.
 * Records the leaf SPKI so the pairing MAC can include it.
 */
@SuppressLint("CustomX509TrustManager") // pins one SPKI; stricter than CA validation
class PinnedTrustManager(private val hubId: String) : X509TrustManager {
    @Volatile
    var serverSpki: ByteArray? = null
        private set

    override fun checkServerTrusted(chain: Array<out X509Certificate>?, authType: String?) {
        val leaf = chain?.firstOrNull() ?: throw CertificateException("no server certificate")
        val spki = spkiOf(leaf)
        if (sha256(spki).hex() != hubId) throw CertificateException("hub key does not match the paired hub id")
        serverSpki = spki
    }

    override fun checkClientTrusted(chain: Array<out X509Certificate>?, authType: String?) =
        throw CertificateException("client role unsupported")

    override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
}
