package dev.sessiontap.android.domain

import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.ChildView
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.Status
import java.time.Duration
import java.time.Instant
import java.time.OffsetDateTime

/** Port of Quickshell `SAgents.qml effectiveStatus`. */
fun effectiveStatus(view: AgentView): Status {
    val children = view.children.orEmpty()
    if (view.status == Status.Blocked || children.any { it.status == Status.Blocked }) return Status.Blocked
    if (view.status == Status.Running || children.any { it.status == Status.Running }) return Status.Running
    return view.status
}

val STALE_AFTER: Duration = Duration.ofHours(24)

fun parseInstant(value: String): Instant? =
    runCatching { OffsetDateTime.parse(value).toInstant() }.getOrNull()

fun isStale(view: AgentView, now: Instant): Boolean {
    val updated = parseInstant(view.updatedAt) ?: return false
    return Duration.between(updated, now) > STALE_AFTER
}

/** What makes an agent blocked: the first blocked child, else the root itself. */
data class BlockCause(val kind: ReasonKind?, val summary: String?, val child: ChildView?)

fun blockCause(view: AgentView): BlockCause? {
    val child = view.children.orEmpty().firstOrNull { it.status == Status.Blocked }
    if (child != null) return BlockCause(child.reason?.kind, child.reason?.summary, child)
    if (view.status == Status.Blocked) return BlockCause(view.reason?.kind, view.reason?.summary, null)
    return null
}
