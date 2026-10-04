package dev.sessiontap.android.net

import dev.sessiontap.android.crypto.PinnedTrustManager
import okhttp3.ConnectionSpec
import okhttp3.OkHttpClient
import okhttp3.Protocol
import okhttp3.TlsVersion
import java.util.concurrent.TimeUnit
import javax.net.ssl.SSLContext
import javax.net.ssl.X509KeyManager

/** OkHttp clients that pin the hub key and present the device certificate. */
object HubTls {
    val PING_INTERVAL_SECONDS = 90L
    private val base: OkHttpClient by lazy { OkHttpClient() }

    private val tls13: ConnectionSpec = ConnectionSpec.Builder(ConnectionSpec.MODERN_TLS)
        .tlsVersions(TlsVersion.TLS_1_3)
        .build()

    /** One client per connection attempt, so its trust manager records that server's SPKI. */
    fun client(hubId: String, keyManager: X509KeyManager?, root: OkHttpClient = base): Pair<OkHttpClient, PinnedTrustManager> {
        val trust = PinnedTrustManager(hubId)
        val context = SSLContext.getInstance("TLS")
        context.init(keyManager?.let { arrayOf(it) }, arrayOf(trust), null)
        val client = root.newBuilder()
            .sslSocketFactory(context.socketFactory, trust)
            .hostnameVerifier { _, _ -> true } // the SPKI pin replaces hostname checks
            .connectionSpecs(listOf(tls13))
            .protocols(listOf(Protocol.HTTP_1_1))
            .connectTimeout(10, TimeUnit.SECONDS)
            .readTimeout(0, TimeUnit.SECONDS)
            .pingInterval(PING_INTERVAL_SECONDS, TimeUnit.SECONDS)
            .build()
        return client to trust
    }
}
