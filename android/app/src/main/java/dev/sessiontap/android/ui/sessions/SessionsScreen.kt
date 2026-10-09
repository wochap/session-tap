package dev.sessiontap.android.ui.sessions

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.IntrinsicSize
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.SwipeToDismissBox
import androidx.compose.material3.SwipeToDismissBoxValue
import androidx.compose.material3.Text
import androidx.compose.material3.rememberSwipeToDismissBoxState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.Saver
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.ColorFilter
import androidx.compose.ui.graphics.ColorMatrix
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.Paint
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.regular.Bell
import com.adamglin.phosphoricons.regular.Broom
import com.adamglin.phosphoricons.regular.CaretDown
import com.adamglin.phosphoricons.regular.CheckCircle
import com.adamglin.phosphoricons.regular.Cube
import com.adamglin.phosphoricons.regular.Desktop
import com.adamglin.phosphoricons.regular.DesktopTower
import com.adamglin.phosphoricons.regular.DotsThreeVertical
import com.adamglin.phosphoricons.regular.Eraser
import com.adamglin.phosphoricons.regular.Funnel
import com.adamglin.phosphoricons.regular.PauseCircle
import com.adamglin.phosphoricons.regular.Plugs
import com.adamglin.phosphoricons.regular.QrCode
import com.adamglin.phosphoricons.regular.TerminalWindow
import com.adamglin.phosphoricons.regular.TreeStructure
import dev.sessiontap.android.data.AgentItem
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.ui.components.Conn
import dev.sessiontap.android.ui.components.ConnDot
import dev.sessiontap.android.ui.components.PrimaryButton
import dev.sessiontap.android.ui.components.ProviderMark
import dev.sessiontap.android.ui.components.SecondaryButton
import dev.sessiontap.android.ui.components.StCard
import dev.sessiontap.android.ui.components.StChip
import dev.sessiontap.android.ui.components.StatusGlyph
import dev.sessiontap.android.ui.components.connOf
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St
import dev.sessiontap.android.ui.theme.repoColor
import java.time.Instant

/** Expanded rows survive tab switches; each key is flattened to its three id strings. */
private val AgentKeySetSaver = Saver<Set<AgentKey>, ArrayList<String>>(
    save = { keys -> ArrayList(keys.flatMap { listOf(it.hubId, it.sourceId, it.invocationId) }) },
    restore = { flat -> flat.chunked(3).map { AgentKey(it[0], it[1], it[2]) }.toSet() },
)

private val GRAYSCALE = ColorFilter.colorMatrix(ColorMatrix().apply { setToSaturation(0f) })

@Composable
fun SessionsScreen(
    hubs: List<HubEntity>,
    conn: Map<String, ConnState>,
    agents: List<AgentItem>,
    hidden: Set<AgentKey>,
    collapsed: Map<String, Boolean>?,
    now: Instant,
    onOpen: (AgentKey) -> Unit,
    onToggleSection: (String, Boolean) -> Unit,
    onForget: (AgentKey) -> Unit,
    onPair: () -> Unit,
    onHubs: () -> Unit,
    onAlerts: () -> Unit,
    onRetry: () -> Unit,
    onOpenTailscale: () -> Unit,
    contentPadding: PaddingValues,
) {
    val c = St.colors
    var hubSel by rememberSaveable { mutableStateOf<String?>(null) }
    var filter by rememberSaveable { mutableStateOf(Filter.All) }
    var expanded by rememberSaveable(stateSaver = AgentKeySetSaver) { mutableStateOf(emptySet<AgentKey>()) }
    var menu by remember { mutableStateOf(false) }
    if (hubSel != null && hubs.none { it.hubId == hubSel }) hubSel = null
    val multi = hubs.size > 1
    // Collapse state still loading: show nothing rather than flash sections open.
    val feed = if (collapsed == null) emptyList() else buildFeed(FeedInput(hubs, conn, agents, hidden, hubSel, filter, collapsed, expanded, now))
    val counts = filterCounts(agents, hidden, hubSel, now)

    Column(Modifier.fillMaxSize().background(c.bg).padding(top = contentPadding.calculateTopPadding())) {
        Row(Modifier.fillMaxWidth().padding(start = 16.dp, end = 8.dp, top = 10.dp, bottom = 6.dp), verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text(if (multi) "SessionTap" else hubs.firstOrNull()?.name ?: "SessionTap", fontSize = 22.sp, fontWeight = FontWeight.Medium, letterSpacing = (-0.33).sp)
                Row(horizontalArrangement = Arrangement.spacedBy(10.dp), verticalAlignment = Alignment.CenterVertically) {
                    if (multi) {
                        val byConn = hubs.groupBy { connOf(conn[it.hubId] ?: if (it.revoked) ConnState.Revoked else null) }
                        listOf(Conn.Live to "live", Conn.Reconnecting to "reconnecting", Conn.Offline to "offline", Conn.Revoked to "revoked").forEach { (k, label) ->
                            val n = byConn[k]?.size ?: 0
                            if (n > 0) Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                                ConnDot(k)
                                Text("$n $label", fontSize = 12.sp, color = c.mute)
                            }
                        }
                    } else hubs.firstOrNull()?.let { hub ->
                        val state = conn[hub.hubId] ?: if (hub.revoked) ConnState.Revoked else null
                        val endpoint = (state as? ConnState.Live)?.endpoint ?: hub.lastGoodEndpoint ?: hub.endpoints.firstOrNull().orEmpty()
                        val count = agents.count { it.key.hubId == hub.hubId && it.key !in hidden }
                        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(5.dp)) {
                            ConnDot(connOf(state))
                            Text("${connText(hub, state, now)} · $endpoint · $count sessions", fontSize = 12.sp, color = c.mute, maxLines = 1, overflow = TextOverflow.Ellipsis)
                        }
                    }
                }
            }
            Box {
                IconButton(onClick = { menu = true }, modifier = Modifier.testTag("menu")) {
                    Icon(PhosphorIcons.Regular.DotsThreeVertical, "Menu", tint = c.text, modifier = Modifier.size(22.dp))
                }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }, containerColor = c.surf2, shape = RoundedCornerShape(14.dp)) {
                    MenuItem("Pair another hub", PhosphorIcons.Regular.QrCode) { menu = false; onPair() }
                    MenuItem("Manage hubs", PhosphorIcons.Regular.DesktopTower) { menu = false; onHubs() }
                    MenuItem("Notification settings", PhosphorIcons.Regular.Bell) { menu = false; onAlerts() }
                }
            }
        }
        if (multi) {
            Row(Modifier.horizontalScroll(rememberScrollState()).padding(start = 16.dp, end = 16.dp, top = 6.dp, bottom = 2.dp), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                val count = { id: String? -> agents.count { it.key !in hidden && (id == null || it.key.hubId == id) && !dev.sessiontap.android.domain.isStale(it.view, now) } }
                StChip(hubSel == null, { hubSel = null }, 32.dp, 10.dp, Modifier.testTag("hubchip:all")) { ChipLabel("All", count(null).toString(), hubSel == null) }
                hubs.forEach { hub ->
                    val sel = hubSel == hub.hubId
                    StChip(sel, { hubSel = hub.hubId }, 32.dp, 10.dp, Modifier.testTag("hubchip:${hub.name}")) {
                        ConnDot(connOf(conn[hub.hubId] ?: if (hub.revoked) ConnState.Revoked else null))
                        ChipLabel(hub.name, count(hub.hubId).toString(), sel)
                    }
                }
            }
        }
        val hasAgents = agents.any { a -> hubs.any { it.hubId == a.key.hubId } }
        if (hasAgents) {
            Row(Modifier.horizontalScroll(rememberScrollState()).padding(horizontal = 16.dp, vertical = 6.dp), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                listOf(
                    Triple(Filter.All, "All", ""),
                    Triple(Filter.Attention, "Needs attention", counts.attention.toString()),
                    Triple(Filter.Running, "Running", counts.running.toString()),
                    Triple(Filter.Stale, "Stale", counts.stale.toString()),
                ).forEach { (f, label, n) ->
                    StChip(filter == f, { filter = f }, 28.dp, 14.dp, Modifier.testTag("filter:${f.name}")) {
                        if (f == Filter.Attention && counts.attention > 0) Box(Modifier.size(6.dp).background(c.block, RoundedCornerShape(3.dp)))
                        ChipLabel(label, n, filter == f, small = true)
                    }
                }
            }
        }
        val listState = rememberLazyListState()
        // Read while idle: after a change lands, key anchoring has already moved the first visible index.
        var atTop by remember { mutableStateOf(true) }
        LaunchedEffect(listState) {
            snapshotFlow { listState.firstVisibleItemIndex == 0 && listState.firstVisibleItemScrollOffset == 0 }.collect { atTop = it }
        }
        val wasAtTop = atTop
        LaunchedEffect(feed.map { it.id }) { if (wasAtTop) listState.scrollToItem(0) }
        LazyColumn(Modifier.weight(1f).testTag("feed"), state = listState, contentPadding = PaddingValues(bottom = contentPadding.calculateBottomPadding() + 12.dp)) {
            items(feed, key = { it.id }) { item ->
                when (item) {
                    is FeedItem.Section -> SectionHeader(item) { onToggleSection(item.key, !item.collapsed) }
                    is FeedItem.Source -> Row(Modifier.padding(start = 30.dp, end = 16.dp, top = 8.dp, bottom = 2.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        Icon(if (item.host) PhosphorIcons.Regular.Desktop else PhosphorIcons.Regular.Cube, null, tint = c.mute, modifier = Modifier.size(12.dp))
                        Text(item.title, fontFamily = Mono, fontSize = 11.sp, color = c.mute)
                    }
                    is FeedItem.Note -> Text(item.text, fontSize = 12.sp, color = c.mute, modifier = Modifier.padding(start = 30.dp, end = 16.dp, top = 2.dp, bottom = 6.dp))
                    is FeedItem.Error -> ErrorCard(item, onRetry, onOpenTailscale, onPair)
                    is FeedItem.Empty -> EmptyState(item)
                    is FeedItem.Row -> SessionRow(
                        row = item.row,
                        onOpen = { onOpen(item.row.key) },
                        onToggleKids = { expanded = if (item.row.expanded) expanded - item.row.key else expanded + item.row.key },
                        onForget = { onForget(item.row.key) },
                    )
                    is FeedItem.Kids -> KidsBlock(item.row)
                }
            }
        }
    }
}

@Composable
private fun MenuItem(label: String, icon: androidx.compose.ui.graphics.vector.ImageVector, onClick: () -> Unit) {
    DropdownMenuItem(
        text = { Text(label, fontSize = 14.sp) },
        leadingIcon = { Icon(icon, null, Modifier.size(18.dp)) },
        onClick = onClick,
        modifier = Modifier.widthIn(min = 220.dp),
    )
}

@Composable
private fun ChipLabel(label: String, count: String, selected: Boolean, small: Boolean = false) {
    val c = St.colors
    Text(label, fontSize = if (small) 12.5.sp else 13.sp, fontWeight = FontWeight.Medium, color = if (selected) c.accInk else c.text)
    if (count.isNotEmpty()) Text(count, fontFamily = Mono, fontSize = 11.sp, color = c.mute)
}

@Composable
private fun SectionHeader(item: FeedItem.Section, onToggle: () -> Unit) {
    val c = St.colors
    val rot by animateFloatAsState(if (item.collapsed) -90f else 0f, label = "chev")
    Row(
        Modifier.fillMaxWidth().clickable(onClick = onToggle).padding(start = 16.dp, end = 12.dp, top = 14.dp, bottom = 6.dp).testTag(item.id),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        if (item.attention) Box(Modifier.size(6.dp).background(c.block, RoundedCornerShape(3.dp)))
        item.conn?.let { ConnDot(it) }
        Text(item.title, fontSize = 12.5.sp, fontWeight = FontWeight.Medium, color = when {
            item.attention -> c.block
            item.muted -> c.mute
            else -> c.text
        })
        if (item.meta.isNotEmpty()) Text(item.meta, fontSize = 12.sp, color = c.mute, maxLines = 1)
        Box(Modifier.weight(1f).height(1.dp).padding(horizontal = 6.dp).background(Brush.horizontalGradient(listOf(c.line, Color.Transparent))))
        Text(item.count.toString(), fontFamily = Mono, fontSize = 11.sp, color = c.mute)
        Icon(PhosphorIcons.Regular.CaretDown, null, tint = c.mute, modifier = Modifier.size(14.dp).rotate(rot))
    }
}

@Composable
internal fun SessionRow(row: RowModel, onOpen: () -> Unit, onToggleKids: () -> Unit, onForget: () -> Unit) {
    val c = St.colors
    val content = @Composable {
        val bg = if (row.blocked) Brush.linearGradient(listOf(c.blockTint, c.blockTint)) else Brush.linearGradient(listOf(c.bg, c.bg))
        Row(
            Modifier.fillMaxWidth().background(c.bg).background(bg).clickable(onClick = onOpen)
                .staleFilter(row.stale)
                .padding(horizontal = 14.dp, vertical = 10.dp)
                .testTag("row:${row.name}"),
            horizontalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            Box(Modifier.height(20.dp), contentAlignment = Alignment.Center) {
                StatusGlyph(row.glyph)
            }
            Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    Row(Modifier.weight(1f), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        Text(row.name, fontSize = 14.sp, fontWeight = FontWeight.Medium, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f, fill = false))
                        ProviderMark(row.mark)
                        if (row.terminal) {
                            Icon(PhosphorIcons.Regular.TerminalWindow, "Terminal available", tint = c.mute, modifier = Modifier.size(14.dp).testTag("term:${row.name}"))
                        }
                        row.model?.let {
                            Text(
                                it, fontFamily = Mono, fontSize = 11.sp, color = c.mute, maxLines = 1, overflow = TextOverflow.Ellipsis,
                                modifier = Modifier.widthIn(max = 110.dp).clip(RoundedCornerShape(4.dp)).background(c.line).padding(horizontal = 6.dp).testTag("model:${row.name}"),
                            )
                        }
                        row.hubTag?.let {
                            Text(it, fontSize = 11.sp, color = c.mute, modifier = Modifier.clip(RoundedCornerShape(4.dp)).background(c.line).padding(horizontal = 6.dp))
                        }
                    }
                    Text(row.time, fontFamily = Mono, fontSize = 11.sp, color = c.mute, maxLines = 1, softWrap = false, modifier = Modifier.testTag("time:${row.name}"))
                }
                Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    Box(Modifier.size(6.dp).clip(RoundedCornerShape(2.dp)).background(if (row.stale) c.dim else repoColor(row.repoKey)))
                    Text(row.branch, fontFamily = Mono, fontSize = 11.5.sp, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.widthIn(max = 150.dp))
                    Text("·", color = c.dim, fontSize = 12.sp)
                    Text(row.activity, fontSize = 12.sp, color = c.mute, maxLines = 1, overflow = TextOverflow.Ellipsis, fontFamily = if (row.activityMono) Mono else null)
                }
                row.reason?.let { Text(it, fontSize = 12.sp, color = c.block, maxLines = 1, overflow = TextOverflow.Ellipsis) }
            }
            if (row.children.isNotEmpty()) {
                Row(
                    Modifier.align(Alignment.CenterVertically).height(26.dp).clip(RoundedCornerShape(13.dp))
                        .background(if (row.expanded) c.line else Color.Transparent)
                        .clickable(onClick = onToggleKids)
                        .padding(start = 8.dp, end = 6.dp)
                        .testTag("kids:${row.name}"),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    Icon(PhosphorIcons.Regular.TreeStructure, null, tint = c.mute, modifier = Modifier.size(12.dp))
                    Text(row.children.size.toString(), fontFamily = Mono, fontSize = 11.sp, fontWeight = FontWeight.Medium, color = c.mute)
                    Icon(PhosphorIcons.Regular.CaretDown, null, tint = c.mute, modifier = Modifier.size(11.dp).rotate(if (row.expanded) 180f else 0f))
                }
            }
        }
    }
    if (!row.swipeable) {
        content()
        return
    }
    val state = rememberSwipeToDismissBoxState()
    LaunchedEffect(state.currentValue) {
        if (state.currentValue == SwipeToDismissBoxValue.EndToStart) {
            onForget()
            state.snapTo(SwipeToDismissBoxValue.Settled)
        }
    }
    SwipeToDismissBox(
        state = state,
        enableDismissFromStartToEnd = false,
        backgroundContent = {
            Row(
                Modifier.fillMaxSize().background(c.block.copy(alpha = 0.22f).compositeOver(c.bg)).padding(end = 22.dp),
                horizontalArrangement = Arrangement.End,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Icon(PhosphorIcons.Regular.Eraser, null, tint = c.block, modifier = Modifier.size(17.dp))
                Spacer(Modifier.width(6.dp))
                Text("Forget", color = c.block, fontSize = 13.sp, fontWeight = FontWeight.Medium)
            }
        },
    ) { content() }
}

private fun Color.compositeOver(background: Color): Color {
    val a = alpha
    return Color(red * a + background.red * (1 - a), green * a + background.green * (1 - a), blue * a + background.blue * (1 - a), 1f)
}

/** Desaturates stale rows like the handoff's `grayscale(1) opacity(.5)`. */
private fun Modifier.staleFilter(stale: Boolean): Modifier = if (!stale) this else drawWithContent {
    val paint = Paint().apply { colorFilter = GRAYSCALE; alpha = 0.5f }
    drawIntoCanvas {
        it.saveLayer(Rect(Offset.Zero, size), paint)
        drawContent()
        it.restore()
    }
}

@Composable
private fun KidsBlock(row: RowModel) {
    val c = St.colors
    val bg = if (row.blocked) c.blockTint else Color.Transparent
    Box(Modifier.fillMaxWidth().background(c.bg).background(bg).staleFilter(row.stale).padding(start = 40.dp, end = 14.dp, bottom = 10.dp)) {
        Row(Modifier.height(IntrinsicSize.Min)) {
            Box(Modifier.width(1.dp).fillMaxHeight().background(c.line))
            Column(Modifier.padding(start = 10.dp)) {
                Text("${row.children.size} agents", fontSize = 11.sp, color = c.mute, modifier = Modifier.padding(bottom = 4.dp))
                row.children.forEach { k ->
                    Row(Modifier.padding(vertical = 4.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        StatusGlyph(k.glyph, small = true)
                        Text(k.name, fontSize = 12.5.sp, fontWeight = FontWeight.Medium)
                        Text(k.summary, fontSize = 12.sp, color = if (k.blocked) c.block else c.mute, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                        Text(k.elapsed, fontFamily = Mono, fontSize = 11.sp, color = c.mute)
                    }
                }
            }
        }
    }
}

@Composable
private fun ErrorCard(item: FeedItem.Error, onRetry: () -> Unit, onOpenTailscale: () -> Unit, onPair: () -> Unit) {
    val c = St.colors
    StCard(Modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, top = 8.dp, bottom = 4.dp), padding = PaddingValues(horizontal = 16.dp, vertical = 14.dp)) {
        Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Icon(PhosphorIcons.Regular.Plugs, null, tint = c.mute, modifier = Modifier.size(18.dp))
                Text(item.title, fontSize = 15.sp, fontWeight = FontWeight.Medium)
            }
            if (item.body.isNotEmpty()) Text(item.body, fontSize = 13.sp, color = c.mute)
            if (item.detail.isNotEmpty()) Text(item.detail, fontFamily = Mono, fontSize = 11.5.sp, color = c.mute)
            Row(Modifier.padding(top = 6.dp), horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (item.revokedHubId != null) {
                    PrimaryButton("Pair again", onPair, height = 32.dp)
                } else {
                    PrimaryButton("Retry now", onRetry, height = 32.dp)
                    SecondaryButton("Open Tailscale", onOpenTailscale, height = 32.dp)
                }
            }
        }
    }
}

@Composable
private fun EmptyState(item: FeedItem.Empty) {
    val c = St.colors
    val icon = when (item.kind) {
        "none" -> PhosphorIcons.Regular.TerminalWindow
        "attention" -> PhosphorIcons.Regular.CheckCircle
        "running" -> PhosphorIcons.Regular.PauseCircle
        "stale" -> PhosphorIcons.Regular.Broom
        else -> PhosphorIcons.Regular.Funnel
    }
    Column(Modifier.fillMaxWidth().padding(start = 28.dp, end = 28.dp, top = 56.dp).testTag(item.id), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Icon(icon, null, tint = c.mute, modifier = Modifier.size(32.dp))
        Text(item.title, fontSize = 18.sp, fontWeight = FontWeight.Medium, modifier = Modifier.padding(top = 6.dp))
        if (item.body.isNotEmpty()) Text(item.body, fontSize = 13.5.sp, color = c.mute)
        item.cmd?.let {
            StCard(Modifier.padding(top = 10.dp), shape = RoundedCornerShape(12.dp), padding = PaddingValues(horizontal = 14.dp, vertical = 10.dp)) {
                Text(it, fontFamily = Mono, fontSize = 13.sp)
            }
        }
    }
}
