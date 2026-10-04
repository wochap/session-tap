package dev.sessiontap.android.crypto

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.math.BigInteger
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.cert.X509Certificate
import java.security.spec.ECGenParameterSpec
import java.util.Calendar
import java.util.Date
import java.net.Socket
import java.security.Principal
import java.security.PrivateKey
import javax.net.ssl.SSLEngine
import javax.net.ssl.X509ExtendedKeyManager
import javax.security.auth.x500.X500Principal

/** Client identity: one non-exportable P-256 key in AndroidKeyStore shared by all hubs. */
object DeviceKey {
    const val ALIAS = "sessiontap-device"
    private const val PROVIDER = "AndroidKeyStore"

    private fun keyStore(): KeyStore = KeyStore.getInstance(PROVIDER).apply { load(null) }

    /** Creates the key once and returns its self-signed certificate. */
    @Synchronized
    fun certificate(): X509Certificate {
        val ks = keyStore()
        (ks.getCertificate(ALIAS) as? X509Certificate)?.let { return it }
        val start = Date()
        val end = Calendar.getInstance().apply { add(Calendar.YEAR, 100) }.time
        val spec = KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_SIGN)
            .setAlgorithmParameterSpec(ECGenParameterSpec("secp256r1"))
            .setDigests(KeyProperties.DIGEST_SHA256, KeyProperties.DIGEST_NONE)
            .setCertificateSubject(X500Principal("CN=SessionTap device"))
            .setCertificateSerialNumber(BigInteger.ONE)
            .setCertificateNotBefore(start)
            .setCertificateNotAfter(end)
            .build()
        KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, PROVIDER).apply {
            initialize(spec)
            generateKeyPair()
        }
        return ks.getCertificate(ALIAS) as X509Certificate
    }

    fun spki(): ByteArray = spkiOf(certificate())

    fun fingerprint(): List<String> = fingerprintGroups(spki())

    /** Key manager presenting the device certificate as TLS client auth. */
    fun keyManager(): X509ExtendedKeyManager {
        val cert = certificate()
        val key = keyStore().getKey(ALIAS, null) as PrivateKey
        return SingleKeyManager(ALIAS, key, arrayOf(cert))
    }
}

/** Always offers one client key, whatever issuers the server names. */
class SingleKeyManager(
    private val alias: String,
    private val key: PrivateKey,
    private val chain: Array<X509Certificate>,
) : X509ExtendedKeyManager() {
    override fun chooseClientAlias(keyType: Array<out String>?, issuers: Array<out Principal>?, socket: Socket?) = alias
    override fun chooseEngineClientAlias(keyType: Array<out String>?, issuers: Array<out Principal>?, engine: SSLEngine?) = alias
    override fun getClientAliases(keyType: String?, issuers: Array<out Principal>?) = arrayOf(alias)
    override fun getCertificateChain(alias: String?) = chain
    override fun getPrivateKey(alias: String?) = key
    override fun getServerAliases(keyType: String?, issuers: Array<out Principal>?): Array<String>? = null
    override fun chooseServerAlias(keyType: String?, issuers: Array<out Principal>?, socket: Socket?): String? = null
}
