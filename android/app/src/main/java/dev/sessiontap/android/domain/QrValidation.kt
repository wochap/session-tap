package dev.sessiontap.android.domain

import dev.sessiontap.android.net.ProtocolJson
import dev.sessiontap.android.net.QrPayload
import java.util.Base64

const val QR_VERSION = 1

/** A QR payload that passed validation, with its decoded secret. */
data class ValidPayload(val payload: QrPayload, val secret: ByteArray)

sealed interface QrResult {
    data class Valid(val value: ValidPayload) : QrResult
    data object Expired : QrResult
    data class Invalid(val why: String) : QrResult
}

fun decodeBase64Url(value: String): ByteArray? =
    runCatching { Base64.getUrlDecoder().decode(value.trimEnd('=')) }.getOrNull()

fun encodeBase64Url(bytes: ByteArray): String = Base64.getUrlEncoder().withoutPadding().encodeToString(bytes)

/** Parses and validates a scanned QR string before any network call. */
fun validateQr(text: String, nowEpochSeconds: Long): QrResult {
    val payload = runCatching { ProtocolJson.decodeFromString<QrPayload>(text.trim()) }.getOrNull()
        ?: return QrResult.Invalid("not a SessionTap pairing code")
    if (payload.v != QR_VERSION) return QrResult.Invalid("unsupported pairing code version ${payload.v}")
    if (!Regex("^[0-9a-f]{64}$").matches(payload.id)) return QrResult.Invalid("malformed hub id")
    val endpoints = payload.ep.filter { it.isNotBlank() }
    if (endpoints.isEmpty()) return QrResult.Invalid("no endpoints")
    if (endpoints.any { parseEndpoint(it) == null }) return QrResult.Invalid("malformed endpoint")
    val secret = decodeBase64Url(payload.s)
    if (secret == null || secret.size != 32) return QrResult.Invalid("malformed secret")
    if (payload.exp <= nowEpochSeconds) return QrResult.Expired
    return QrResult.Valid(ValidPayload(payload.copy(ep = endpoints), secret))
}

/** Splits `host:port` or `[v6]:port`. */
fun parseEndpoint(value: String): Pair<String, Int>? {
    val v = value.trim()
    val (host, port) = if (v.startsWith("[")) {
        val end = v.indexOf("]:")
        if (end < 0) return null
        v.substring(1, end) to v.substring(end + 2)
    } else {
        val i = v.lastIndexOf(':')
        if (i <= 0) return null
        v.substring(0, i) to v.substring(i + 1)
    }
    val p = port.toIntOrNull() ?: return null
    if (host.isEmpty() || p !in 1..65535) return null
    return host to p
}
