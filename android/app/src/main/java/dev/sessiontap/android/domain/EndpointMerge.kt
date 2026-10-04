package dev.sessiontap.android.domain

const val MAX_ENDPOINT_HINTS = 8

/**
 * True for a syntactic `host:port` hint: no whitespace or `/`, port 1..65535,
 * host a bracketed IPv6 literal or a part without `:`. No DNS or reachability check.
 */
fun isEndpointHint(value: String): Boolean {
    if (value.isEmpty() || value.any { it.isWhitespace() || it == '/' }) return false
    val colon = value.lastIndexOf(':')
    if (colon <= 0) return false
    val port = value.substring(colon + 1)
    if (port.isEmpty() || port.length > 5 || !port.all { it in '0'..'9' } || port.toInt() !in 1..65535) return false
    val host = value.substring(0, colon)
    return if (host.startsWith('[')) {
        host.length > 2 && host.endsWith(']') && host.indexOf(']') == host.length - 1
    } else {
        ':' !in host && '[' !in host && ']' !in host
    }
}

/**
 * Connected endpoint first, then [incoming] in order, then [stored]: trimmed,
 * invalid entries dropped, deduplicated keeping the first occurrence, capped at [cap].
 */
fun mergeEndpoints(first: String?, incoming: List<String>, stored: List<String>, cap: Int = MAX_ENDPOINT_HINTS): List<String> =
    (listOfNotNull(first) + incoming + stored)
        .map { it.trim() }
        .filter(::isEndpointHint)
        .distinct()
        .take(cap)
