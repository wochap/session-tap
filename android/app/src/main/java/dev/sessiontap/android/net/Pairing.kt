package dev.sessiontap.android.net

import dev.sessiontap.android.crypto.PinnedTrustManager
import dev.sessiontap.android.crypto.pairingMac
import dev.sessiontap.android.domain.ValidPayload
import dev.sessiontap.android.domain.decodeBase64Url
import dev.sessiontap.android.domain.encodeBase64Url
import kotlinx.coroutines.CancellationException
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import okhttp3.OkHttpClient

sealed interface PairOutcome {
    data class Paired(val hubId: String, val hubName: String, val endpoint: String, val scopes: List<String>) : PairOutcome
    data object Expired : PairOutcome
    data object Rejected : PairOutcome
    data class Failed(val message: String) : PairOutcome
    data class Unreachable(val failures: List<EndpointFailure>) : PairOutcome
}

/** Runs the pairing exchange from a validated QR payload. */
suspend fun pairWithHub(
    valid: ValidPayload,
    deviceName: String,
    deviceSpki: ByteArray,
    clientFor: () -> Pair<OkHttpClient, PinnedTrustManager?>,
    onWaiting: () -> Unit = {},
): PairOutcome {
    val payload = valid.payload
    val conn = try {
        raceEndpoints(payload.ep, null, headStartMs = 0, clientFor = clientFor)
    } catch (e: UnreachableException) {
        return PairOutcome.Unreachable(e.failures)
    }
    try {
        val hubSpki = conn.trust?.serverSpki ?: return PairOutcome.Failed("hub key was not recorded")
        val begin = ProtocolJson.decodeFromJsonElement(PairBegin.serializer(), conn.call("pair.begin")!!)
        val nonce = decodeBase64Url(begin.nonce) ?: return PairOutcome.Failed("malformed nonce")
        val mac = pairingMac(valid.secret, hubSpki, deviceSpki, nonce)
        onWaiting()
        val params = buildJsonObject {
            put("name", JsonPrimitive(deviceName.take(64)))
            put("mac", JsonPrimitive(encodeBase64Url(mac)))
        }
        val done = ProtocolJson.decodeFromJsonElement(
            PairComplete.serializer(),
            conn.call("pair.complete", params, timeoutMs = 300_000)!!,
        )
        val info = ProtocolJson.decodeFromJsonElement(HubInfo.serializer(), conn.call("hub.info")!!)
        return PairOutcome.Paired(info.hubId, done.hubName.ifEmpty { info.hubName }, conn.endpoint, info.scopes)
    } catch (e: CancellationException) {
        throw e
    } catch (e: RpcException) {
        return when (e.code) {
            "pairing_closed" -> PairOutcome.Expired
            "pairing_rejected" -> PairOutcome.Rejected
            else -> PairOutcome.Failed(e.message ?: e.code)
        }
    } catch (e: Throwable) {
        return PairOutcome.Failed(e.message ?: "pairing failed")
    } finally {
        conn.close()
    }
}
