package dev.sessiontap.android.domain

import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.Usage
import java.time.Duration
import java.time.Instant
import java.util.Locale

/** 2 -> 2, 15300 -> 15.3k, 2400000 -> 2.4M (same as `sessiontap-notify.sh`). */
fun humanizeTokens(n: Long): String = when {
    n >= 1_000_000 -> String.format(Locale.US, "%.1fM", n / 1_000_000.0)
    n >= 1_000 -> String.format(Locale.US, "%.1fk", n / 1_000.0)
    else -> n.toString()
}

/** Display name and two-letter mark for a provider id. */
data class ProviderLabel(val name: String, val mark: String)

fun providerLabel(provider: String): ProviderLabel {
    val p = provider.lowercase(Locale.ROOT)
    return when {
        "codex" in p -> ProviderLabel("Codex", "CX")
        "claude" in p -> ProviderLabel("Claude Code", "CC")
        "qwen" in p -> ProviderLabel("Qwen Code", "QW")
        p == "pi" || p.startsWith("pi-") || p.startsWith("pi_") -> ProviderLabel("Pi", "PI")
        else -> {
            val words = provider.split('-', '_', ' ').filter { it.isNotEmpty() }
            val name = words.joinToString(" ") { w -> w.replaceFirstChar { it.titlecase(Locale.ROOT) } }
            ProviderLabel(name.ifEmpty { "Agent" }, name.filter { it.isLetter() }.take(2).uppercase(Locale.ROOT).ifEmpty { "AG" })
        }
    }
}

/** Replaces a leading home directory with `~`. The home is guessed from the path itself. */
fun shortenHome(cwd: String, home: String? = guessHome(cwd)): String {
    if (home.isNullOrEmpty()) return cwd
    return when {
        cwd == home -> "~"
        cwd.startsWith("$home/") -> "~/" + cwd.removePrefix("$home/")
        else -> cwd
    }
}

/** `/home/<user>`, `/Users/<user>`, or `/root` prefix of a path. */
fun guessHome(path: String): String? {
    if (path == "/root" || path.startsWith("/root/")) return "/root"
    val match = Regex("^(/home/[^/]+|/Users/[^/]+)").find(path) ?: return null
    return match.value
}

fun sessionTitle(view: AgentView): String =
    view.session?.name?.takeIf { it.isNotBlank() } ?: providerLabel(view.provider).name

/** "Context 42% · In 15.3k · Out 2.1k" */
fun usageFooter(usage: Usage?): String {
    if (usage == null) return ""
    val parts = buildList {
        usage.contextWindowPercent?.let { add("Context $it%") }
        usage.inputTokens?.let { add("In ${humanizeTokens(it)}") }
        usage.outputTokens?.let { add("Out ${humanizeTokens(it)}") }
    }
    return parts.joinToString(" · ")
}

/** "~/code/api · feat/auth-retry" */
fun location(view: AgentView): String =
    listOfNotNull(shortenHome(view.cwd).takeIf { it.isNotEmpty() }, view.repository?.branch?.takeIf { it.isNotEmpty() })
        .joinToString(" · ")

/** Compact age: 8s, 14m, 2h, 3d. */
fun relativeAge(from: Instant?, now: Instant): String {
    if (from == null) return ""
    val s = Duration.between(from, now).seconds.coerceAtLeast(0)
    return when {
        s < 60 -> "${s}s"
        s < 3600 -> "${s / 60}m"
        s < 86400 -> "${s / 3600}h"
        else -> "${s / 86400}d"
    }
}
