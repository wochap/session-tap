package dev.sessiontap.android.ui.keys

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.detectDragGesturesAfterLongPress
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.consumeWindowInsets
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Rect
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.layout.boundsInRoot
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.zIndex
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Bold
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.bold.ArrowUp
import com.adamglin.phosphoricons.bold.DotsSix
import com.adamglin.phosphoricons.regular.ArrowCounterClockwise
import com.adamglin.phosphoricons.regular.ArrowLeft
import com.adamglin.phosphoricons.regular.Keyboard
import com.adamglin.phosphoricons.regular.Plus
import com.adamglin.phosphoricons.regular.Trash
import dev.sessiontap.android.data.KeyLayoutStore
import dev.sessiontap.android.domain.KeyLayout
import dev.sessiontap.android.domain.KeySpec
import dev.sessiontap.android.net.TerminalKeys
import dev.sessiontap.android.ui.components.PrimaryButton
import dev.sessiontap.android.ui.components.SecondaryButton
import dev.sessiontap.android.ui.terminal.KeyFace
import dev.sessiontap.android.ui.terminal.KeyGrid
import dev.sessiontap.android.ui.terminal.KeyGridActions
import dev.sessiontap.android.ui.terminal.Modifiers
import dev.sessiontap.android.ui.terminal.keyLook
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St
import kotlinx.coroutines.launch

/** Loads and saves the global key layout around [KeyEditorScreen]. */
@Composable
fun KeyEditorRoute(store: KeyLayoutStore, onBack: () -> Unit, contentPadding: PaddingValues) {
    val stored by store.layout.collectAsStateWithLifecycle(null)
    val scope = rememberCoroutineScope()
    // Edits show at once; the store catches up.
    var local by remember { mutableStateOf<KeyLayout?>(null) }
    val layout = local ?: stored ?: return
    KeyEditorScreen(
        layout = layout,
        onChange = { next ->
            local = next
            scope.launch { store.save(next) }
        },
        onReset = {
            local = KeyLayout.DEFAULT
            scope.launch { store.reset() }
        },
        onBack = onBack,
        contentPadding = contentPadding,
    )
}

/** What the picker sheet is for. */
private sealed interface Sheet {
    data class Add(val row: Int) : Sheet
    data class Replace(val row: Int, val index: Int) : Sheet
}

private data class Drag(val row: Int, val index: Int, val start: Offset, val offset: Offset = Offset.Zero) {
    val point: Offset get() = start + offset
}

private val PICKER_KEYS: List<KeySpec> = listOf(
    TerminalKeys.ESCAPE, TerminalKeys.TAB, TerminalKeys.BACK_TAB, TerminalKeys.UP, TerminalKeys.DOWN, TerminalKeys.LEFT,
    TerminalKeys.RIGHT, TerminalKeys.HOME, TerminalKeys.END, TerminalKeys.PAGE_UP, TerminalKeys.PAGE_DOWN, TerminalKeys.ENTER,
    TerminalKeys.SPACE, TerminalKeys.BACKSPACE, TerminalKeys.DELETE,
).map { KeySpec.Named(it) } + listOf(KeySpec.CtrlC, KeySpec.Paste)

private val QUICK_CHARS = listOf("/", "-", "$", ":", "|", "~")

/** The key layout editor: rows with drag to reorder, a picker sheet, and a live preview. */
@Composable
fun KeyEditorScreen(
    layout: KeyLayout,
    onChange: (KeyLayout) -> Unit,
    onReset: () -> Unit,
    onBack: () -> Unit,
    contentPadding: PaddingValues,
) {
    val c = St.colors
    var sheet by remember { mutableStateOf<Sheet?>(null) }
    var confirm by remember { mutableStateOf(false) }
    var drag by remember { mutableStateOf<Drag?>(null) }
    val cells = remember { mutableStateMapOf<Pair<Int, Int>, Rect>() }
    val rowBoxes = remember { mutableStateMapOf<Int, Rect>() }

    /** Where a drop at the drag's point lands: before a key, or at the end of a row. */
    fun target(d: Drag): Pair<Int, Int>? {
        cells.entries.firstOrNull { (at, rect) -> at != d.row to d.index && rect.contains(d.point) && at.first < layout.rows.size && at.second < layout.rows[at.first].size }
            ?.let { return it.key }
        return rowBoxes.entries.firstOrNull { (r, rect) -> r < layout.rows.size && rect.contains(d.point) }?.let { it.key to layout.rows[it.key].size }
    }

    fun drop(d: Drag) {
        val (toRow, at) = target(d) ?: return
        val toIndex = if (toRow == d.row && d.index < at) at - 1 else at
        onChange(layout.move(d.row, d.index, toRow, toIndex))
    }

    BackHandler(sheet != null) { sheet = null }
    Box(Modifier.fillMaxSize().background(c.bg).padding(contentPadding).consumeWindowInsets(contentPadding).imePadding().testTag("key-editor")) {
        Column(Modifier.fillMaxSize()) {
            Row(Modifier.fillMaxWidth().padding(start = 4.dp, end = 8.dp, top = 2.dp, bottom = 6.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                IconButton(onClick = onBack, modifier = Modifier.testTag("keys-back")) { Icon(PhosphorIcons.Regular.ArrowLeft, "Back", tint = c.text, modifier = Modifier.size(22.dp)) }
                Text("Terminal keys", fontSize = 18.sp, fontWeight = FontWeight.Medium, modifier = Modifier.weight(1f))
                val isDefault = layout == KeyLayout.DEFAULT
                TextButton(onClick = { confirm = true }, enabled = !isDefault, modifier = Modifier.alpha(if (isDefault) 0.4f else 1f).testTag("reset-keys")) {
                    Text("Reset", fontSize = 14.sp, fontWeight = FontWeight.Medium, color = c.accInk)
                }
            }
            Column(Modifier.weight(1f).verticalScroll(rememberScrollState(), enabled = drag == null).padding(start = 12.dp, end = 12.dp, bottom = 16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("One layout for every agent on every hub. Row 1 sits on top; the bottom row is closest to the reply field.", fontSize = 13.sp, color = c.mute, modifier = Modifier.padding(start = 4.dp, end = 4.dp, bottom = 4.dp))
                val over = drag?.let(::target)
                layout.rows.forEachIndexed { r, row ->
                    val shape = RoundedCornerShape(14.dp)
                    val ringed = drag != null && over?.first == r
                    Column(
                        Modifier.fillMaxWidth().clip(shape).background(c.surf).border(if (ringed) 1.5.dp else 0.dp, if (ringed) c.acc else c.surf, shape)
                            .onGloballyPositioned { rowBoxes[r] = it.boundsInRoot() }.padding(start = 8.dp, end = 8.dp, top = 8.dp, bottom = 10.dp).testTag("row:$r"),
                        verticalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        Row(Modifier.padding(start = 4.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                            Text("Row ${r + 1}", fontSize = 13.sp, fontWeight = FontWeight.Medium)
                            Text("· ${row.size} of ${KeyLayout.MAX_PER_ROW}", fontSize = 12.sp, color = c.mute)
                            Spacer(Modifier.weight(1f))
                            IconButton(onClick = { onChange(layout.deleteRow(r)) }, enabled = layout.canDeleteRow, modifier = Modifier.size(40.dp).alpha(if (layout.canDeleteRow) 1f else 0.35f).testTag("delete-row:$r")) {
                                Icon(PhosphorIcons.Regular.Trash, "Delete row ${r + 1}", tint = c.mute, modifier = Modifier.size(18.dp))
                            }
                        }
                        Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                            for (i in 0 until KeyLayout.MAX_PER_ROW) {
                                Box(Modifier.weight(1f).height(48.dp)) {
                                    val key = row.getOrNull(i)
                                    when {
                                        key != null -> EditCell(
                                            key = key,
                                            lifted = drag?.let { it.row == r && it.index == i } == true,
                                            offset = drag?.takeIf { it.row == r && it.index == i }?.offset ?: Offset.Zero,
                                            selected = (sheet as? Sheet.Replace)?.let { it.row == r && it.index == i } == true,
                                            insert = drag != null && over == r to i,
                                            onClick = { sheet = Sheet.Replace(r, i) },
                                            modifier = Modifier.onGloballyPositioned { cells[r to i] = it.boundsInRoot() }
                                                .pointerInput(r, i, layout) {
                                                    detectDragGesturesAfterLongPress(
                                                        onDragStart = { at -> drag = Drag(r, i, (cells[r to i]?.topLeft ?: Offset.Zero) + at) },
                                                        onDrag = { change, amount ->
                                                            change.consume()
                                                            drag = drag?.let { it.copy(offset = it.offset + amount) }
                                                        },
                                                        onDragEnd = {
                                                            drag?.let(::drop)
                                                            drag = null
                                                        },
                                                        onDragCancel = { drag = null },
                                                    )
                                                }
                                                .testTag("edit:$r:$i"),
                                        )
                                        i == row.size -> AddCell(insert = drag != null && over == r to i) { sheet = Sheet.Add(r) }
                                        else -> {}
                                    }
                                }
                            }
                        }
                    }
                }
                Row(
                    Modifier.fillMaxWidth().height(44.dp).clip(RoundedCornerShape(22.dp)).border(1.dp, c.line, RoundedCornerShape(22.dp))
                        .alpha(if (layout.canAddRow) 1f else 0.45f)
                        .clickable(enabled = layout.canAddRow, role = Role.Button) { onChange(layout.addRow()) }.testTag("add-row"),
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.CenterHorizontally),
                ) {
                    Icon(PhosphorIcons.Regular.Plus, null, tint = c.text, modifier = Modifier.size(18.dp))
                    Text(if (layout.canAddRow) "Add row" else "Add row · max ${KeyLayout.MAX_ROWS}", fontSize = 15.sp, fontWeight = FontWeight.Medium)
                }
                Column(Modifier.padding(start = 4.dp, end = 4.dp, top = 6.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    Text("Long-press a key, then drag it within a row or to another row. Tap a key to replace or remove it.", fontSize = 12.5.sp, color = c.mute)
                    Text("Up to ${KeyLayout.MAX_PER_ROW} keys per row and ${KeyLayout.MAX_ROWS} rows. Ctrl and Alt are sticky: tap for the next key, hold to lock.", fontSize = 12.5.sp, color = c.mute)
                }
            }
            Preview(layout)
        }
        sheet?.let { s -> PickerSheet(s, layout, onPick = { key ->
            onChange(
                when (s) {
                    is Sheet.Add -> layout.add(s.row, key)
                    is Sheet.Replace -> layout.replace(s.row, s.index, key)
                },
            )
            sheet = null
        }, onRemove = {
            (s as? Sheet.Replace)?.let { onChange(layout.remove(it.row, it.index)) }
            sheet = null
        }, onClose = { sheet = null }) }
    }
    if (confirm) {
        val n = layout.rows.size
        AlertDialog(
            onDismissRequest = { confirm = false },
            containerColor = c.surf2,
            shape = RoundedCornerShape(28.dp),
            icon = { Icon(PhosphorIcons.Regular.ArrowCounterClockwise, null, tint = c.accInk, modifier = Modifier.size(24.dp)) },
            title = { Text("Reset to default keys?", fontSize = 20.sp, fontWeight = FontWeight.Medium) },
            text = { Text("Your $n row${if (n == 1) "" else "s"} go back to the 2-row default for every agent and hub. Custom keys are removed.", fontSize = 14.sp, color = c.mute) },
            dismissButton = { TextButton(onClick = { confirm = false }) { Text("Cancel", color = c.accInk) } },
            confirmButton = {
                TextButton(onClick = { confirm = false; onReset() }, modifier = Modifier.testTag("confirm-reset")) { Text("Reset", color = c.accInk) }
            },
        )
    }
}

@Composable
private fun EditCell(key: KeySpec, lifted: Boolean, offset: Offset, selected: Boolean, insert: Boolean, onClick: () -> Unit, modifier: Modifier) {
    val c = St.colors
    val look = keyLook(key, selected = selected || lifted)
    val shape = RoundedCornerShape(8.dp)
    Box(Modifier.fillMaxSize().zIndex(if (lifted) 1f else 0f)) {
        Column(
            modifier.fillMaxSize()
                .graphicsLayer {
                    translationX = offset.x
                    translationY = offset.y - if (lifted) 6.dp.toPx() else 0f
                    val scale = if (lifted) 1.06f else 1f
                    scaleX = scale
                    scaleY = scale
                    shadowElevation = if (lifted) 12.dp.toPx() else 0f
                    this.shape = shape
                    clip = true
                }
                .background(look.bg).border(look.ringWidth, look.ring, shape)
                .clickable(role = Role.Button, onClick = onClick),
            horizontalAlignment = Alignment.CenterHorizontally,
            verticalArrangement = Arrangement.spacedBy(1.dp, Alignment.CenterVertically),
        ) {
            Icon(PhosphorIcons.Bold.DotsSix, null, tint = c.mute, modifier = Modifier.size(12.dp))
            Box(Modifier.height(16.dp), contentAlignment = Alignment.Center) { KeyFace(key, look.content, iconSize = 15.dp, fontSize = 12f) }
        }
        if (insert) InsertBar()
    }
}

@Composable
private fun BoxScope.InsertBar() {
    Box(Modifier.align(Alignment.CenterStart).padding(vertical = 4.dp).width(3.dp).fillMaxHeight().background(St.colors.acc, RoundedCornerShape(2.dp)))
}

@Composable
private fun AddCell(insert: Boolean, onClick: () -> Unit) {
    val c = St.colors
    val shape = RoundedCornerShape(8.dp)
    Box(Modifier.fillMaxSize()) {
        Box(
            Modifier.fillMaxSize().clip(shape).border(1.dp, c.line, shape).padding(4.dp).border(1.dp, c.text.copy(alpha = 0.22f), RoundedCornerShape(5.dp))
                .clickable(role = Role.Button, onClick = onClick).testTag("add-key"),
            contentAlignment = Alignment.Center,
        ) { Icon(PhosphorIcons.Regular.Plus, "Add key", tint = c.mute, modifier = Modifier.size(18.dp)) }
        if (insert) InsertBar()
    }
}

@Composable
private fun Preview(layout: KeyLayout) {
    val c = St.colors
    Column(Modifier.fillMaxWidth().background(c.surf).testTag("key-preview")) {
        Box(Modifier.fillMaxWidth().height(1.dp).background(c.line))
        Text("PREVIEW", fontSize = 11.sp, letterSpacing = 0.66.sp, color = c.mute, modifier = Modifier.padding(start = 16.dp, top = 10.dp, bottom = 2.dp))
        KeyGrid(layout, enabled = true, mods = Modifiers(), ctrlArmed = false, actions = KeyGridActions(), modifier = Modifier.padding(start = 8.dp, end = 8.dp, top = 6.dp, bottom = 4.dp), interactive = false)
        Row(Modifier.padding(start = 8.dp, end = 8.dp, top = 4.dp, bottom = 8.dp).alpha(0.5f), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            Box(Modifier.size(44.dp).border(1.dp, c.line, CircleShape), contentAlignment = Alignment.Center) { Icon(PhosphorIcons.Regular.Keyboard, null, tint = c.text, modifier = Modifier.size(20.dp)) }
            Box(Modifier.weight(1f).height(44.dp).clip(RoundedCornerShape(22.dp)).background(c.surf2).padding(horizontal = 16.dp), contentAlignment = Alignment.CenterStart) {
                Text("Reply to agent…", fontSize = 14.sp, color = c.mute)
            }
            Box(Modifier.size(44.dp).border(1.5.dp, c.acc, CircleShape), contentAlignment = Alignment.Center) { Icon(PhosphorIcons.Bold.ArrowUp, null, tint = c.acc, modifier = Modifier.size(18.dp)) }
        }
    }
}

@Composable
private fun PickerSheet(sheet: Sheet, layout: KeyLayout, onPick: (KeySpec) -> Unit, onRemove: () -> Unit, onClose: () -> Unit) {
    val c = St.colors
    val current = (sheet as? Sheet.Replace)?.let { layout.rows[it.row][it.index] }
    val used = layout.rows.flatten().toSet()
    var char by remember { mutableStateOf("") }
    Box(Modifier.fillMaxSize()) {
        Box(Modifier.fillMaxSize().background(c.bg.copy(alpha = 0.6f)).clickable(interactionSource = remember { MutableInteractionSource() }, indication = null, onClick = onClose))
        Column(
            Modifier.align(Alignment.BottomCenter).fillMaxWidth().fillMaxHeight(0.88f).clip(RoundedCornerShape(topStart = 28.dp, topEnd = 28.dp)).background(c.surf2)
                .verticalScroll(rememberScrollState()).padding(start = 16.dp, end = 16.dp, top = 8.dp, bottom = 30.dp).testTag("key-picker"),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Box(Modifier.align(Alignment.CenterHorizontally).padding(vertical = 6.dp).size(32.dp, 4.dp).alpha(0.5f).background(c.mute, RoundedCornerShape(2.dp)))
            Text(
                when (sheet) {
                    is Sheet.Add -> "Add key to row ${sheet.row + 1}"
                    is Sheet.Replace -> "Replace ${current?.label}"
                },
                fontSize = 18.sp,
                fontWeight = FontWeight.Medium,
                modifier = Modifier.padding(horizontal = 4.dp),
            )
            Group("Keys", PICKER_KEYS, current, used, onPick, "Outlined keys are already in your bar.")
            Group("Function", TerminalKeys.FUNCTION.map { KeySpec.Named(it) }, current, used, onPick, null)
            Group("Modifiers · sticky", listOf(KeySpec.Ctrl, KeySpec.Alt), current, used, onPick, "Tap once to apply to the next key; hold to lock.")
            GroupLabel("Character…")
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                val field = RoundedCornerShape(22.dp)
                Box(Modifier.weight(1f).height(44.dp).clip(field).background(c.bg).border(1.dp, c.line, field).padding(horizontal = 16.dp), contentAlignment = Alignment.CenterStart) {
                    if (char.isEmpty()) Text("Any single character", fontFamily = Mono, fontSize = 15.sp, color = c.mute)
                    BasicTextField(
                        char,
                        { typed -> char = typed.lastOrNull()?.toString()?.takeIf(TerminalKeys::isChar).orEmpty() },
                        singleLine = true,
                        textStyle = TextStyle(fontFamily = Mono, fontSize = 15.sp, fontWeight = FontWeight.Medium, color = c.text),
                        cursorBrush = SolidColor(c.acc),
                        modifier = Modifier.fillMaxWidth().testTag("char-input"),
                    )
                }
                PrimaryButton("Use", { if (char.isNotEmpty()) onPick(KeySpec.Char(char)) }, Modifier.alpha(if (char.isEmpty()) 0.45f else 1f).testTag("use-char"), height = 44.dp)
            }
            Row(Modifier.padding(top = 6.dp), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                QUICK_CHARS.forEach { ch ->
                    val shape = RoundedCornerShape(10.dp)
                    Box(
                        Modifier.size(44.dp).clip(shape).background(c.bg).border(1.dp, c.line, shape).clickable(role = Role.Button) { onPick(KeySpec.Char(ch)) }.testTag("quick:$ch"),
                        contentAlignment = Alignment.Center,
                    ) { Text(ch, fontFamily = Mono, fontSize = 15.sp, fontWeight = FontWeight.Medium) }
                }
            }
            Row(Modifier.fillMaxWidth().padding(top = 16.dp), verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (current != null) {
                    TextButton(onClick = onRemove, modifier = Modifier.testTag("remove-key")) {
                        Icon(PhosphorIcons.Regular.Trash, null, tint = c.block, modifier = Modifier.size(18.dp))
                        Spacer(Modifier.width(8.dp))
                        Text("Remove ${current.label}", fontSize = 14.sp, fontWeight = FontWeight.Medium, color = c.block)
                    }
                }
                Spacer(Modifier.weight(1f))
                SecondaryButton("Cancel", onClose, Modifier.testTag("picker-cancel"), height = 44.dp)
            }
        }
    }
}

@Composable
private fun GroupLabel(label: String) {
    Text(label.uppercase(), fontSize = 11.sp, letterSpacing = 0.66.sp, color = St.colors.mute, modifier = Modifier.padding(start = 4.dp, end = 4.dp, top = 12.dp, bottom = 4.dp))
}

@Composable
private fun Group(label: String, keys: List<KeySpec>, current: KeySpec?, used: Set<KeySpec>, onPick: (KeySpec) -> Unit, note: String?) {
    val c = St.colors
    GroupLabel(label)
    keys.chunked(6).forEach { line ->
        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            for (i in 0 until 6) {
                val key = line.getOrNull(i)
                if (key == null) {
                    Spacer(Modifier.weight(1f))
                    continue
                }
                val isCurrent = key == current
                val inBar = key in used
                val shape = RoundedCornerShape(10.dp)
                val content = when {
                    isCurrent || inBar -> c.accInk
                    key == KeySpec.CtrlC -> c.block
                    else -> c.text
                }
                Box(
                    Modifier.weight(1f).height(44.dp).clip(shape).background(if (isCurrent) c.accTint else c.bg)
                        .border(if (isCurrent) 1.5.dp else 1.dp, if (isCurrent || inBar) c.acc else c.line, shape)
                        .clickable(role = Role.Button) { onPick(key) }.testTag("pick:${key.id}"),
                    contentAlignment = Alignment.Center,
                ) { KeyFace(key, content) }
            }
        }
    }
    note?.let { Text(it, fontSize = 12.sp, color = c.mute, modifier = Modifier.padding(start = 4.dp, end = 4.dp, top = 4.dp)) }
}
