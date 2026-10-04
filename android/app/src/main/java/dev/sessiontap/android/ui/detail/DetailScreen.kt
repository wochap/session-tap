package dev.sessiontap.android.ui.detail

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.border
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.regular.ArrowLeft
import com.adamglin.phosphoricons.regular.Copy
import com.adamglin.phosphoricons.regular.DotsThreeVertical
import com.adamglin.phosphoricons.regular.Eraser
import com.adamglin.phosphoricons.regular.Eye
import com.adamglin.phosphoricons.regular.TerminalWindow
import dev.sessiontap.android.data.AgentItem
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.domain.TerminalAccess
import dev.sessiontap.android.domain.TerminalLevel
import dev.sessiontap.android.domain.blockCause
import dev.sessiontap.android.domain.humanizeTokens
import dev.sessiontap.android.domain.isStale
import dev.sessiontap.android.domain.parseInstant
import dev.sessiontap.android.domain.providerLabel
import dev.sessiontap.android.domain.relativeAge
import dev.sessiontap.android.domain.sessionTitle
import dev.sessiontap.android.domain.shortenHome
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.Status
import dev.sessiontap.android.ui.components.Glyph
import dev.sessiontap.android.ui.components.PrimaryButton
import dev.sessiontap.android.ui.components.SecondaryButton
import dev.sessiontap.android.ui.components.StCard
import dev.sessiontap.android.ui.components.StatusGlyph
import dev.sessiontap.android.ui.sessions.blockedLine
import dev.sessiontap.android.ui.sessions.childGlyph
import dev.sessiontap.android.ui.sessions.connText
import dev.sessiontap.android.ui.sessions.glyphOf
import dev.sessiontap.android.ui.components.copyText
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St
import dev.sessiontap.android.ui.theme.repoColor
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter
import java.util.Locale

private val STAMP = DateTimeFormatter.ofPattern("MMM d · HH:mm", Locale.US)
private val CLOCK = DateTimeFormatter.ofPattern("HH:mm", Locale.US)

private fun stamp(value: String): String =
    parseInstant(value)?.atZone(ZoneId.systemDefault())?.format(STAMP) ?: value

@Composable
fun DetailScreen(
    item: AgentItem?,
    hub: HubEntity?,
    conn: ConnState?,
    now: Instant,
    onBack: () -> Unit,
    onForget: () -> Unit,
    contentPadding: PaddingValues,
    onTerminal: () -> Unit = {},
) {
    val c = St.colors
    var menu by remember { mutableStateOf(false) }
    val clipboardContext = LocalContext.current
    Column(Modifier.fillMaxSize().background(c.bg).padding(contentPadding)) {
        Row(Modifier.fillMaxWidth().padding(horizontal = 4.dp, vertical = 6.dp), verticalAlignment = Alignment.CenterVertically) {
            IconButton(onClick = onBack) { Icon(PhosphorIcons.Regular.ArrowLeft, "Back", tint = c.text, modifier = Modifier.size(22.dp)) }
            val source = item?.let { hub?.sources?.get(it.key.sourceId) ?: it.key.sourceId }.orEmpty()
            Text(listOfNotNull(hub?.name, source.ifEmpty { null }).joinToString(" · "), fontSize = 13.sp, color = c.mute, modifier = Modifier.weight(1f).padding(start = 4.dp))
            if (item != null) Box {
                IconButton(onClick = { menu = true }) { Icon(PhosphorIcons.Regular.DotsThreeVertical, "Menu", tint = c.text, modifier = Modifier.size(22.dp)) }
                DropdownMenu(expanded = menu, onDismissRequest = { menu = false }, containerColor = c.surf2, shape = RoundedCornerShape(14.dp)) {
                    DropdownMenuItem(
                        text = { Text("Copy cwd", fontSize = 14.sp) },
                        leadingIcon = { Icon(PhosphorIcons.Regular.Copy, null, Modifier.size(18.dp)) },
                        onClick = { menu = false; copyText(clipboardContext, item.view.cwd) },
                    )
                    if (item.view.status == Status.Stopped && hub?.canManage == true) {
                        DropdownMenuItem(
                            text = { Text("Forget session", fontSize = 14.sp, color = c.block) },
                            leadingIcon = { Icon(PhosphorIcons.Regular.Eraser, null, Modifier.size(18.dp), tint = c.block) },
                            onClick = { menu = false; onForget() },
                        )
                    }
                }
            }
        }
        if (item == null) {
            Text("This session is gone.", color = c.mute, modifier = Modifier.padding(20.dp))
            return@Column
        }
        val v = item.view
        val provider = providerLabel(v.provider)
        val stale = isStale(v, now)
        val glyph = glyphOf(v, item.effective)
        val blocked = item.effective == Status.Blocked
        val cause = blockCause(v)
        Column(
            Modifier.weight(1f).verticalScroll(rememberScrollState()).padding(start = 16.dp, end = 16.dp, bottom = 24.dp).testTag("detail"),
            verticalArrangement = Arrangement.spacedBy(18.dp),
        ) {
            Column(Modifier.padding(horizontal = 4.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(sessionTitle(v), fontSize = 24.sp, fontWeight = FontWeight.Medium, letterSpacing = (-0.48).sp)
                FlowRow(horizontalArrangement = Arrangement.spacedBy(6.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    MetaChip(provider.name)
                    v.metadata?.model?.let { MetaChip(it, mono = true) }
                    v.metadata?.effort?.let { MetaChip("effort $it") }
                    v.metadata?.permissionMode?.let { MetaChip(it, mono = true) }
                }
            }
            val title = when {
                blocked && cause?.child != null -> "Blocked · ${blockedLine(v)}"
                blocked -> "Blocked · needs " + when (cause?.kind) {
                    ReasonKind.Approval -> "permission"
                    ReasonKind.Input -> "input"
                    else -> "attention"
                }
                else -> when (glyph) {
                    Glyph.Running -> "Running"
                    Glyph.Idle -> "Idle"
                    Glyph.Completed -> "Completed"
                    Glyph.Failed -> "Failed"
                    else -> "Stopped"
                } + if (stale) " · stale" else ""
            }
            val line = if (blocked) cause?.summary.orEmpty() else v.reason?.summary.orEmpty()
            val since = cause?.child?.let { parseInstant(it.updatedAt) } ?: parseInstant(v.updatedAt)
            val meta = if (blocked) "since ${relativeAge(since, now)} · hub ${hub?.let { connText(it, conn, now) } ?: "?"}" else "updated ${relativeAge(parseInstant(v.updatedAt), now)} ago"
            StCard(color = if (blocked) c.blockTint else c.surf, padding = PaddingValues(horizontal = 16.dp, vertical = 14.dp)) {
                Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                    Box(Modifier.height(20.dp), contentAlignment = Alignment.Center) { StatusGlyph(glyph) }
                    Column(verticalArrangement = Arrangement.spacedBy(3.dp)) {
                        Text(title, fontSize = 15.sp, fontWeight = FontWeight.Medium, color = if (blocked) c.block else c.text, modifier = Modifier.testTag("status"))
                        if (line.isNotEmpty()) Text(line, fontFamily = Mono, fontSize = 12.sp)
                        Text(meta, fontSize = 12.sp, color = c.mute)
                    }
                }
            }
            TerminalEntry(TerminalAccess.of(hub?.canWatch == true, hub?.canControl == true, v), hub?.name ?: "The hub", onTerminal)
            Group("Location") {
                StCard(Modifier.fillMaxWidth()) {
                    Column {
                        LocRow("cwd") { Text(shortenHome(v.cwd), fontFamily = Mono, fontSize = 12.5.sp) }
                        v.repository?.let { repo ->
                            LocRow("branch", top = true) {
                                Box(Modifier.size(6.dp).clip(RoundedCornerShape(2.dp)).background(repoColor(repo.root)))
                                Text(repo.branch ?: "detached", fontFamily = Mono, fontSize = 12.5.sp, maxLines = 1, overflow = TextOverflow.Ellipsis, modifier = Modifier.weight(1f))
                                if (repo.dirty == true) Text("● dirty", fontFamily = Mono, fontSize = 11.sp, color = c.run)
                            }
                            repo.head?.let { LocRow("head", top = true) { Text(it.take(7), fontFamily = Mono, fontSize = 12.5.sp) } }
                        }
                    }
                }
            }
            v.usage?.let { u ->
                Group("Usage") {
                    StCard(Modifier.fillMaxWidth(), padding = PaddingValues(14.dp)) {
                        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                            u.contextWindowPercent?.let { pct ->
                                Row(verticalAlignment = Alignment.Bottom, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                                    Text("Context window", fontSize = 13.sp, modifier = Modifier.weight(1f))
                                    u.contextTokens?.let { Text(humanizeTokens(it), fontFamily = Mono, fontSize = 12.sp, color = c.mute) }
                                    Text("$pct%", fontFamily = Mono, fontSize = 14.sp, fontWeight = FontWeight.Medium)
                                }
                                Box(Modifier.fillMaxWidth().height(6.dp).clip(RoundedCornerShape(3.dp)).background(c.line)) {
                                    Box(Modifier.fillMaxWidth(pct.coerceIn(0, 100) / 100f).fillMaxHeight().clip(RoundedCornerShape(3.dp)).background(if (pct >= 85) c.run else c.text.copy(alpha = 0.7f)))
                                }
                            }
                            Row {
                                listOf("Input" to u.inputTokens, "Output" to u.outputTokens, "Context" to u.contextTokens).forEach { (label, value) ->
                                    Column(Modifier.weight(1f)) {
                                        Text(label, fontSize = 11.sp, color = c.mute)
                                        Text(value?.let(::humanizeTokens) ?: "—", fontFamily = Mono, fontSize = 15.sp, fontWeight = FontWeight.Medium)
                                    }
                                }
                            }
                        }
                    }
                }
            }
            val kids = v.children.orEmpty()
            if (kids.isNotEmpty()) {
                Group("Agents · ${kids.size}") {
                    StCard(Modifier.fillMaxWidth()) {
                        Column {
                            kids.forEachIndexed { i, k ->
                                val kb = k.status == Status.Blocked
                                Column {
                                    if (i > 0) Box(Modifier.fillMaxWidth().height(1.dp).background(c.line))
                                    Row(Modifier.fillMaxWidth().background(if (kb) c.blockTint else c.surf).padding(horizontal = 14.dp, vertical = 11.dp), horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                                        Box(Modifier.height(18.dp), contentAlignment = Alignment.Center) { StatusGlyph(childGlyph(k), small = true) }
                                        Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                                            Row(verticalAlignment = Alignment.Bottom) {
                                                Text(k.agentType, fontSize = 13.5.sp, fontWeight = FontWeight.Medium, modifier = Modifier.weight(1f))
                                                Text(relativeAge(parseInstant(k.startedAt), if (k.status == Status.Stopped) parseInstant(k.updatedAt) ?: now else now), fontFamily = Mono, fontSize = 11.sp, color = c.mute)
                                            }
                                            Text(k.reason?.summary ?: k.status.name.lowercase(), fontSize = 12.sp, color = if (kb) c.block else c.mute)
                                            val started = parseInstant(k.startedAt)?.atZone(ZoneId.systemDefault())?.format(CLOCK)
                                            Text("started ${started ?: "?"} · updated ${relativeAge(parseInstant(k.updatedAt), now)} ago", fontFamily = Mono, fontSize = 11.sp, color = c.mute)
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Column(Modifier.padding(horizontal = 4.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                Stamp("Created", stamp(v.createdAt))
                Stamp("Updated", stamp(v.updatedAt))
                v.session?.id?.let { Stamp("Session", it) }
                Stamp("Invocation", v.invocationId)
            }
            if (v.status == Status.Stopped) {
                if (hub?.canManage == true) {
                    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                        SecondaryButton(
                            "Forget session",
                            onForget,
                            Modifier.fillMaxWidth().testTag("forget"),
                            height = 44.dp,
                            color = c.block,
                            borderColor = c.block.copy(alpha = 0.45f),
                            icon = PhosphorIcons.Regular.Eraser,
                        )
                        Text("Removes it from ${hub.name}'s list. Files and history on disk are untouched.", fontSize = 12.sp, color = c.mute, textAlign = TextAlign.Center, modifier = Modifier.fillMaxWidth())
                    }
                } else {
                    Text("This phone has read-only access to ${hub?.name ?: "the hub"}.", fontSize = 12.sp, color = c.mute, modifier = Modifier.padding(horizontal = 4.dp))
                }
            } else {
                Text("Forget becomes available once this session stops.", fontSize = 12.sp, color = c.mute, modifier = Modifier.padding(horizontal = 4.dp))
            }
        }
    }
}

/** "Open terminal" with control, "View terminal" with watch, a no-access hint otherwise; nothing without a live terminal. */
@Composable
private fun TerminalEntry(access: TerminalAccess, hubName: String, onTerminal: () -> Unit) {
    if (!access.available) return
    when (access.level) {
        TerminalLevel.Control -> PrimaryButton("Open terminal", onTerminal, Modifier.fillMaxWidth().testTag("open-terminal"), height = 48.dp, icon = PhosphorIcons.Regular.TerminalWindow)
        TerminalLevel.View -> SecondaryButton("View terminal", onTerminal, Modifier.fillMaxWidth().testTag("view-terminal"), height = 48.dp, icon = PhosphorIcons.Regular.Eye)
        TerminalLevel.None -> Text(
            "$hubName didn't grant this phone terminal access. To watch or answer agents here, pair again with sessiontap-hub pair --scope watch (or --scope control).",
            fontSize = 12.sp,
            color = St.colors.mute,
            modifier = Modifier.padding(horizontal = 4.dp).testTag("terminal-no-access"),
        )
    }
}

@Composable
private fun MetaChip(label: String, mono: Boolean = false) {
    val c = St.colors
    Text(
        label,
        fontSize = 12.sp,
        color = c.mute,
        fontFamily = if (mono) Mono else null,
        modifier = Modifier.border(1.dp, c.line, RoundedCornerShape(8.dp)).padding(horizontal = 9.dp, vertical = 3.dp),
    )
}

@Composable
private fun Group(title: String, content: @Composable () -> Unit) {
    Column {
        Text(title, fontSize = 12.sp, color = St.colors.mute, modifier = Modifier.padding(start = 4.dp, end = 4.dp, bottom = 6.dp))
        content()
    }
}

@Composable
private fun LocRow(label: String, top: Boolean = false, content: @Composable androidx.compose.foundation.layout.RowScope.() -> Unit) {
    val c = St.colors
    Column {
        if (top) Box(Modifier.fillMaxWidth().height(1.dp).background(c.line))
        Row(Modifier.padding(horizontal = 14.dp, vertical = 11.dp), horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
            Text(label, fontSize = 13.sp, color = c.mute, modifier = Modifier.width(64.dp))
            content()
        }
    }
}

@Composable
private fun Stamp(label: String, value: String) {
    Row {
        Text(label, fontSize = 12.5.sp, color = St.colors.mute, modifier = Modifier.width(80.dp))
        Text(value, fontFamily = Mono, fontSize = 12.sp)
    }
}
