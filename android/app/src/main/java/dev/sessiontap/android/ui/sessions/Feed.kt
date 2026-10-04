package dev.sessiontap.android.ui.sessions

import dev.sessiontap.android.domain.TerminalAccess
import dev.sessiontap.android.data.AgentItem
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.domain.blockCause
import dev.sessiontap.android.domain.isStale
import dev.sessiontap.android.domain.parseInstant
import dev.sessiontap.android.domain.providerLabel
import dev.sessiontap.android.domain.relativeAge
import dev.sessiontap.android.domain.sessionTitle
import dev.sessiontap.android.domain.shortenHome
import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.ChildView
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.Status
import dev.sessiontap.android.ui.components.Conn
import dev.sessiontap.android.ui.components.Glyph
import dev.sessiontap.android.ui.components.connOf
import java.time.Instant

enum class Filter { All, Attention, Running, Stale }

data class ChildModel(val glyph: Glyph, val name: String, val summary: String, val elapsed: String, val blocked: Boolean)

data class RowModel(
    val key: AgentKey,
    val glyph: Glyph,
    val name: String,
    val mark: String,
    val hubTag: String?,
    val time: String,
    val branch: String,
    val repoKey: String?,
    val activity: String,
    val activityMono: Boolean,
    val reason: String?,
    val children: List<ChildModel>,
    val expanded: Boolean,
    val stale: Boolean,
    val blocked: Boolean,
    val swipeable: Boolean,
    /** This device can open the agent's live terminal. */
    val terminal: Boolean = false,
)

sealed interface FeedItem {
    val id: String

    data class Section(
        val key: String,
        val title: String,
        val meta: String,
        val count: Int,
        val conn: Conn?,
        val attention: Boolean,
        val muted: Boolean,
        val collapsed: Boolean,
    ) : FeedItem {
        override val id get() = "section:$key"
    }

    data class Source(val hubId: String, val title: String, val host: Boolean) : FeedItem {
        override val id get() = "source:$hubId:$title"
    }

    data class Note(val hubId: String, val text: String) : FeedItem {
        override val id get() = "note:$hubId"
    }

    data class Error(val title: String, val body: String, val detail: String, val revokedHubId: String?) : FeedItem {
        override val id get() = "error"
    }

    data class Empty(val kind: String, val title: String, val body: String, val cmd: String?) : FeedItem {
        override val id get() = "empty:$kind"
    }

    data class Row(val row: RowModel) : FeedItem {
        override val id get() = "row:${row.key}"
    }

    data class Kids(val row: RowModel) : FeedItem {
        override val id get() = "kids:${row.key}"
    }
}

fun glyphOf(view: AgentView, effective: Status): Glyph = when (effective) {
    Status.Running -> Glyph.Running
    Status.Blocked -> Glyph.Blocked
    Status.Idle -> Glyph.Idle
    Status.Stopped -> when (view.reason?.kind) {
        ReasonKind.Completed -> Glyph.Completed
        ReasonKind.Failed -> Glyph.Failed
        else -> Glyph.Stopped
    }
}

fun childGlyph(child: ChildView): Glyph = when (child.status) {
    Status.Running -> Glyph.Running
    Status.Blocked -> Glyph.Blocked
    Status.Idle -> Glyph.Idle
    Status.Stopped -> when (child.reason?.kind) {
        ReasonKind.Failed -> Glyph.Failed
        ReasonKind.Completed, null -> Glyph.Completed
        else -> Glyph.Stopped
    }
}

private fun needs(kind: ReasonKind?) = when (kind) {
    ReasonKind.Approval -> "approval"
    ReasonKind.Input -> "input"
    else -> "attention"
}

/** Red third line of a blocked row: which child waits, or the root's own reason. */
fun blockedLine(view: AgentView): String? {
    val cause = blockCause(view) ?: return null
    cause.child?.let { return "${it.agentType} subagent needs ${needs(cause.kind)}" }
    val summary = cause.summary?.takeIf { it.isNotBlank() }
    val head = when (cause.kind) {
        ReasonKind.Approval -> "awaiting permission"
        ReasonKind.Input -> "needs input"
        else -> "needs attention"
    }
    return listOfNotNull(head, summary).joinToString(" · ")
}

fun rowModel(item: AgentItem, hub: HubEntity?, hubTag: String?, expanded: Boolean, now: Instant): RowModel {
    val v = item.view
    val provider = providerLabel(v.provider)
    val stale = isStale(v, now)
    val rootSummary = v.reason?.summary?.takeIf { it.isNotBlank() && v.status != Status.Blocked }
    return RowModel(
        key = item.key,
        glyph = glyphOf(v, item.effective),
        name = sessionTitle(v),
        mark = provider.mark,
        hubTag = hubTag,
        time = relativeAge(parseInstant(v.updatedAt), now),
        branch = v.repository?.branch ?: "—",
        repoKey = v.repository?.root,
        activity = rootSummary ?: shortenHome(v.cwd),
        activityMono = rootSummary == null,
        reason = if (item.effective == Status.Blocked) blockedLine(v) else null,
        children = v.children.orEmpty().map { child ->
            ChildModel(
                glyph = childGlyph(child),
                name = child.agentType,
                summary = child.reason?.summary ?: child.status.name.lowercase(),
                elapsed = relativeAge(parseInstant(child.startedAt), parseInstant(child.updatedAt)?.takeIf { child.status == Status.Stopped } ?: now),
                blocked = child.status == Status.Blocked,
            )
        },
        expanded = expanded,
        stale = stale,
        blocked = item.effective == Status.Blocked && !stale,
        swipeable = v.status == Status.Stopped && hub?.canManage == true,
        terminal = TerminalAccess.of(hub?.canWatch == true, hub?.canControl == true, v).canOpen,
    )
}

data class FeedInput(
    val hubs: List<HubEntity>,
    val conn: Map<String, ConnState>,
    val agents: List<AgentItem>,
    val hidden: Set<AgentKey>,
    val hubSel: String?,
    val filter: Filter,
    val collapsed: Map<String, Boolean>,
    val expanded: Set<AgentKey>,
    val now: Instant,
)

fun connText(hub: HubEntity, state: ConnState?, now: Instant): String = when (state) {
    is ConnState.Live -> "live"
    is ConnState.NoAccess -> "no session access"
    ConnState.Connecting -> "connecting"
    is ConnState.Reconnecting -> {
        val secs = ((state.retryAt - now.toEpochMilli()) / 1000).coerceAtLeast(0)
        if (state.offline) "offline · ${relativeAge(hub.lastSeenAt?.let(Instant::ofEpochMilli), now).ifEmpty { "never" }} · retry ${secs}s"
        else "reconnecting ${secs}s"
    }
    ConnState.Revoked -> "revoked"
    null -> if (hub.revoked) "revoked" else "offline"
}

fun isAttention(item: AgentItem, now: Instant) = item.effective == Status.Blocked && !isStale(item.view, now)

/** Turns hub state into the session list, following the handoff's section rules. */
fun buildFeed(input: FeedInput): List<FeedItem> {
    val now = input.now
    val hubs = input.hubs
    val multi = hubs.size > 1
    val hubById = hubs.associateBy { it.hubId }
    val visible = input.agents
        .filter { it.key !in input.hidden && it.key.hubId in hubById }
        .filter { input.hubSel == null || it.key.hubId == input.hubSel }
        .sortedByDescending { parseInstant(it.view.updatedAt) ?: Instant.EPOCH }
    val feed = mutableListOf<FeedItem>()
    fun collapsed(key: String, default: Boolean) = input.collapsed[key] ?: default
    fun rowItems(item: AgentItem, tagged: Boolean): List<FeedItem> {
        val hub = hubById[item.key.hubId]
        val row = rowModel(item, hub, if (tagged && multi) hub?.name else null, item.key in input.expanded, now)
        return if (row.expanded && row.children.isNotEmpty()) listOf(FeedItem.Row(row), FeedItem.Kids(row)) else listOf(FeedItem.Row(row))
    }

    val conns = hubs.map { connOf(input.conn[it.hubId] ?: if (it.revoked) ConnState.Revoked else null) }
    if (hubs.isNotEmpty() && conns.all { it == Conn.Offline || it == Conn.Revoked }) {
        if (multi) {
            feed += FeedItem.Error(
                title = "All hubs offline",
                body = "Can't reach ${hubs.joinToString(", ") { it.name }}. Showing the last state each hub reported.",
                detail = hubs.mapNotNull { (input.conn[it.hubId] as? ConnState.Reconnecting)?.lastError }.distinct().joinToString(" · "),
                revokedHubId = null,
            )
        } else {
            val hub = hubs[0]
            val state = input.conn[hub.hubId]
            if (hub.revoked || state == ConnState.Revoked) {
                feed += FeedItem.Error("${hub.name} revoked this phone", "The hub no longer trusts this device. Pair again to reconnect.", "", hub.hubId)
            } else {
                val seen = relativeAge(hub.lastSeenAt?.let(Instant::ofEpochMilli), now)
                val retry = (state as? ConnState.Reconnecting)?.let { ((it.retryAt - now.toEpochMilli()) / 1000).coerceAtLeast(0) }
                feed += FeedItem.Error(
                    title = "Can't reach ${hub.name}",
                    body = listOfNotNull(
                        retry?.let { "Retrying in ${it}s." },
                        if (seen.isNotEmpty()) "Below is the last state it reported, $seen ago." else null,
                    ).joinToString(" "),
                    detail = listOfNotNull((state as? ConnState.Reconnecting)?.lastError, hub.endpoints.joinToString("   ")).joinToString(" · "),
                    revokedHubId = null,
                )
            }
        }
    }

    if (input.agents.none { it.key.hubId in hubById } && hubs.isNotEmpty()) {
        val where = if (multi) "your hubs" else hubs[0].name
        feed += FeedItem.Empty(
            "none",
            "No sessions on $where yet",
            "Start Claude Code, Codex, Pi or Qwen in any terminal on $where. It shows up here within a second.",
            "$ claude",
        )
        return feed
    }

    val attention = visible.filter { isAttention(it, now) }
    val stale = visible.filter { isStale(it.view, now) }
    if ((input.filter == Filter.All || input.filter == Filter.Attention) && attention.isNotEmpty()) {
        val c = collapsed("attn", false)
        feed += FeedItem.Section("attn", "Needs attention", "", attention.size, null, attention = true, muted = false, collapsed = c)
        if (!c) attention.forEach { feed += rowItems(it, tagged = true) }
    }
    if (input.filter == Filter.All || input.filter == Filter.Running) {
        hubs.filter { input.hubSel == null || it.hubId == input.hubSel }.forEach { hub ->
            val state = input.conn[hub.hubId] ?: if (hub.revoked) ConnState.Revoked else null
            val all = visible.filter { it.key.hubId == hub.hubId && !isStale(it.view, now) && !isAttention(it, now) }
            val list = if (input.filter == Filter.Running) all.filter { it.effective == Status.Running } else all
            if (input.filter == Filter.Running && list.isEmpty()) return@forEach
            if (multi) {
                val key = "h_${hub.hubId}"
                val c = collapsed(key, false)
                feed += FeedItem.Section(key, hub.name, "· " + connText(hub, state, now), list.size, connOf(state), attention = false, muted = false, collapsed = c)
                if (c) return@forEach
            }
            val conn = connOf(state)
            if ((conn == Conn.Offline || conn == Conn.Revoked) && list.isNotEmpty() && multi) {
                feed += FeedItem.Note(hub.hubId, "Last known state · ${relativeAge(hub.lastSeenAt?.let(Instant::ofEpochMilli), now).ifEmpty { "never" }} ago")
            }
            val sources = input.agents.filter { it.key.hubId == hub.hubId }.map { it.key.sourceId }.distinct()
            if (sources.size > 1) {
                sources.forEach { src ->
                    val rows = list.filter { it.key.sourceId == src }
                    if (rows.isEmpty()) return@forEach
                    feed += FeedItem.Source(hub.hubId, hub.sources[src] ?: src, host = src == "host")
                    rows.forEach { feed += rowItems(it, tagged = false) }
                }
            } else {
                list.forEach { feed += rowItems(it, tagged = false) }
            }
        }
    }
    if ((input.filter == Filter.All || input.filter == Filter.Stale) && stale.isNotEmpty()) {
        val default = input.filter != Filter.Stale
        val c = collapsed("stale", default)
        feed += FeedItem.Section("stale", "Stale", "· no update in 24h", stale.size, null, attention = false, muted = true, collapsed = c)
        if (!c) stale.forEach { feed += rowItems(it, tagged = true) }
    }
    if (feed.none { it is FeedItem.Row || it is FeedItem.Section }) {
        feed += when (input.filter) {
            Filter.Attention -> FeedItem.Empty("attention", "Nothing needs you", "Blocked agents across all hubs land here first.", null)
            Filter.Running -> FeedItem.Empty("running", "No agents running", "Idle and finished sessions are under All.", null)
            Filter.Stale -> FeedItem.Empty("stale", "No stale sessions", "Sessions move here after 24h without an update.", null)
            Filter.All -> FeedItem.Empty("all", "No sessions match", "", null)
        }
    }
    return feed
}

data class FilterCounts(val attention: Int, val running: Int, val stale: Int)

fun filterCounts(agents: List<AgentItem>, hidden: Set<AgentKey>, hubSel: String?, now: Instant): FilterCounts {
    val base = agents.filter { it.key !in hidden && (hubSel == null || it.key.hubId == hubSel) }
    return FilterCounts(
        attention = base.count { isAttention(it, now) },
        running = base.count { !isStale(it.view, now) && it.effective == Status.Running },
        stale = base.count { isStale(it.view, now) },
    )
}
