package dev.sessiontap.android.ui.terminal

import android.graphics.Paint
import android.graphics.Typeface
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.rememberUpdatedState
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.input.pointer.PointerEventPass
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.input.pointer.positionChange
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.platform.LocalViewConfiguration
import androidx.compose.ui.platform.ViewConfiguration
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.SemanticsPropertyKey
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.res.ResourcesCompat
import androidx.core.graphics.withTranslation
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.regular.Copy
import com.termux.view.TerminalRenderer
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St
import kotlinx.coroutines.launch
import kotlin.math.abs
import kotlin.math.sign
import dev.sessiontap.android.R
import kotlin.math.ceil
import kotlin.math.roundToInt

/** Reading size in portrait. */
val READ_SIZE = 9.sp

/** How long a manual sideways pan suspends auto-pan to the cursor. */
private const val MANUAL_PAN_HOLD_MS = 3_000L

/** Mono advance width as a fraction of the font size, measured once per typeface. */
private fun advanceRatio(typeface: Typeface): Float = TerminalRenderer(100, typeface).fontWidth / 100f

/**
 * Viewing state of the pane: font size (read size, fit-to-width, or pinched),
 * sideways offset, and the vertical scroll position measured from the live
 * bottom. While scrolled up, new output keeps the view still and counts lines.
 */
@Stable
class PaneViewState(fit: Boolean) {
    /** True while the font size follows fit-to-width. */
    var fit by mutableStateOf(fit)
    /** Font size in px when not fitting; null means the read size. */
    var customPx by mutableStateOf<Float?>(null)
    var offsetX by mutableFloatStateOf(0f)
    /** Pixels scrolled up from the live bottom. */
    var scrollPx by mutableFloatStateOf(0f)
    var newLines by mutableIntStateOf(0)
    var lastManualPan by mutableLongStateOf(0L)

    /** Active selection; non-null means selection mode, with the view frozen. */
    var selection by mutableStateOf<PaneSelection?>(null)
    var toolbar by mutableStateOf(true)
    /** A handle is being dragged. */
    var dragging by mutableStateOf(false)
    /** Lines held back while frozen, from the ViewModel. */
    var heldLines by mutableIntStateOf(0)
    /** Scroll position when selection mode began, or null when it began at the live bottom. */
    private var preScroll: Float? = null
    var onFreeze: () -> Unit = {}
    var onUnfreeze: () -> Unit = {}

    val scrolledUp: Boolean get() = scrollPx > 0.5f
    val selecting: Boolean get() = selection != null

    /** Enters selection mode with [sel], or moves the current selection there. */
    fun select(sel: PaneSelection) {
        if (selection == null) {
            preScroll = scrollPx.takeIf { scrolledUp }
            onFreeze()
        }
        selection = sel
        toolbar = true
    }

    /** Leaves selection mode: held output is applied and the view returns to live, or to where it was scrolled before. */
    fun endSelection() {
        if (selection == null) return
        selection = null
        dragging = false
        val before = preScroll
        preScroll = null
        if (before == null) jumpToLive() else scrollPx = before
        onUnfreeze()
    }

    fun toggleFit() {
        fit = !fit
        customPx = null
    }

    fun jumpToLive() {
        if (selecting) {
            preScroll = null
            endSelection()
        }
        scrollPx = 0f
        newLines = 0
    }
}

@Composable
fun rememberPaneViewState(fit: Boolean) = remember(fit) { PaneViewState(fit) }

/** Size label for the chip: real pane size plus the current font mode. */
fun sizeLabel(cols: Int, rows: Int, fit: Boolean): String = "$cols×$rows · ${if (fit) "fit" else "${READ_SIZE.value.roundToInt()}sp"}"


/** Selection gesture timing (design 13, gesture rules). */
private const val LONG_PRESS_MS = 450L
private const val DOUBLE_TAP_MS = 260L

/** Handle tip to finger distance, hit area, and auto-scroll edge band. */
private val HANDLE_REACH = 22.dp
private val HANDLE_HIT = 44.dp
private val EDGE_BAND = 36.dp

/** Loupe: ±7 cells at 18 sp. */
private const val LOUPE_CELLS = 7
private val LOUPE_SIZE = 18.sp

/** The pane's selected text, for tests and accessibility. */
val SelectionText = SemanticsPropertyKey<String>("SelectionText")

private enum class End { Start, End }

/**
 * Draws the emulator with Termux's renderer on a Compose canvas. The pane keeps
 * the desktop's column count; when it is wider than the view, it pans
 * sideways and follows the cursor unless the user panned in the last few seconds.
 * Long-press enters selection mode (see [PaneViewState.select]); [onCopy] gets
 * the selected text and its line count.
 */
@Composable
fun PaneView(
    emulator: PaneEmulator,
    tick: PaneTick,
    view: PaneViewState,
    dimmed: Boolean,
    modifier: Modifier = Modifier,
    onCopy: (text: String, lines: Int) -> Unit = { _, _ -> },
) {
    val context = LocalContext.current
    val density = LocalDensity.current
    val haptics = LocalHapticFeedback.current
    val scope = rememberCoroutineScope()
    val typeface = remember { ResourcesCompat.getFont(context, R.font.jetbrains_mono_nerd) ?: Typeface.MONOSPACE }
    val ratio = remember(typeface) { advanceRatio(typeface) }
    val readPx = with(density) { READ_SIZE.toPx() }
    val padPx = with(density) { 6.dp.toPx() }
    val reachPx = with(density) { HANDLE_REACH.toPx() }
    val hitPx = with(density) { HANDLE_HIT.toPx() }
    val edgePx = with(density) { EDGE_BAND.toPx() }
    var lastScrolled by remember { mutableLongStateOf(tick.scrolled) }
    var finger by remember { mutableStateOf<Offset?>(null) }
    var dragEnd by remember { mutableStateOf<End?>(null) }
    var edgeScrolling by remember { mutableIntStateOf(0) }
    BoxWithConstraints(modifier.fillMaxSize()) {
        val viewW = constraints.maxWidth.toFloat()
        val viewH = constraints.maxHeight.toFloat()
        val cols = emulator.cols.coerceAtLeast(1)
        val fitPx = ((viewW - 2 * padPx) / (cols * ratio)).coerceAtLeast(1f)
        val textPx = if (view.fit) fitPx else (view.customPx ?: readPx)
        val renderer = remember(textPx.roundToInt(), typeface) { TerminalRenderer(textPx.roundToInt().coerceAtLeast(1), typeface) }
        val selPaint = remember(textPx.roundToInt(), typeface) { glyphPaint(typeface, textPx.roundToInt().coerceAtLeast(1).toFloat()) }
        val lineH = renderer.fontLineSpacing.toFloat()
        val paneW = cols * renderer.fontWidth + 2 * padPx
        val paneH = emulator.rows * lineH
        val maxX = (paneW - viewW).coerceAtLeast(0f)
        val maxScroll = emulator.scrollbackRows * lineH + (paneH - viewH).coerceAtLeast(0f)

        // Output while scrolled up keeps the view still and counts the new lines.
        LaunchedEffect(tick.scrolled) {
            val delta = (tick.scrolled - lastScrolled).toInt()
            lastScrolled = tick.scrolled
            if (delta > 0 && view.scrolledUp) {
                view.scrollPx = (view.scrollPx + delta * lineH).coerceAtMost(maxScroll)
                view.newLines += delta
            }
        }
        // Follow the cursor sideways unless the user panned recently or is selecting.
        LaunchedEffect(tick.version, textPx, viewW) {
            val emu = emulator.emulator ?: return@LaunchedEffect
            if (view.selecting) return@LaunchedEffect
            if (System.currentTimeMillis() - view.lastManualPan < MANUAL_PAN_HOLD_MS) return@LaunchedEffect
            val cursorX = emu.cursorCol * renderer.fontWidth + padPx
            if (cursorX < view.offsetX || cursorX > view.offsetX + viewW - 2 * renderer.fontWidth) {
                view.offsetX = (cursorX - viewW * 0.75f).coerceIn(0f, maxX)
            }
        }
        view.offsetX = view.offsetX.coerceIn(0f, maxX)
        view.scrollPx = view.scrollPx.coerceIn(0f, maxScroll)
        if (!view.scrolledUp) view.newLines = 0

        val rows = emulator.rows
        val rowsUp = ceil((viewH + view.scrollPx) / lineH - rows).toInt().coerceIn(0, emulator.scrollbackRows)
        val top = viewH - (rows + rowsUp) * lineH + view.scrollPx
        val geo = PaneGeometry(renderer.fontWidth, lineH, padPx, view.offsetX, top, rowsUp)
        val geoState = rememberUpdatedState(geo)
        val buffer = emulator.emulator?.let { TermuxSelBuffer(it.screen, it.mColumns, it.mRows) }
        val bufferState = rememberUpdatedState(buffer)
        val maxScrollState = rememberUpdatedState(maxScroll)

        fun tipOf(end: End, sel: PaneSelection, buf: SelBuffer, g: PaneGeometry): Offset {
            val cell = if (end == End.Start) buf.snapStart(sel.start) else buf.snapEnd(sel.end)
            return Offset(g.cellLeft(cell.col) + g.fontWidth / 2, g.rowTop(cell.row) + g.lineH / 2)
        }

        fun knobOf(end: End, sel: PaneSelection, buf: SelBuffer, g: PaneGeometry): Offset {
            val tip = tipOf(end, sel, buf, g)
            // Both knobs hang below their row, so the toolbar above the selection stays clear of them.
            val x = if (end == End.Start) tip.x - g.fontWidth / 2 else tip.x + g.fontWidth / 2
            return Offset(x, tip.y + g.lineH / 2 + reachPx)
        }

        val base = LocalViewConfiguration.current
        val config = remember(base) {
            object : ViewConfiguration by base {
                override val longPressTimeoutMillis = LONG_PRESS_MS
                override val doubleTapTimeoutMillis = DOUBLE_TAP_MS
            }
        }
        val selection = view.selection
        Box(
            Modifier
                .fillMaxSize()
                .semantics { if (selection != null && buffer != null) this[SelectionText] = selection.text(buffer) }
                // Handle drags claim the pointer before the pane's pan and tap detectors see it.
                .pointerInput(Unit) {
                    awaitEachGesture {
                        val down = awaitFirstDown(requireUnconsumed = false, pass = PointerEventPass.Initial)
                        val sel = view.selection ?: return@awaitEachGesture
                        val buf = bufferState.value ?: return@awaitEachGesture
                        val g0 = geoState.value
                        val end = End.entries.firstOrNull { (knobOf(it, sel, buf, g0) - down.position).getDistance() <= hitPx / 2 } ?: return@awaitEachGesture
                        down.consume()
                        // The dragged end becomes the focus; the other stays put.
                        val norm = sel.normalized()
                        val fixed = if (end == End.Start) norm.focus else norm.anchor
                        val tip0 = tipOf(end, sel, buf, g0)
                        var total = Offset.Zero
                        var current = if (end == End.Start) norm.anchor else norm.focus
                        view.selection = PaneSelection(fixed, current, sel.mode)
                        view.dragging = true
                        dragEnd = end
                        finger = down.position

                        fun update() {
                            val b = bufferState.value ?: return
                            val target = b.clamp(geoState.value.cell(tip0.x + total.x, tip0.y + total.y))
                            if (target != current) {
                                current = target
                                view.selection = view.selection?.copy(focus = target)
                                haptics.performHapticFeedback(HapticFeedbackType.TextHandleMove)
                            }
                        }

                        val auto = scope.launch {
                            var last = withFrameNanos { it }
                            while (true) {
                                val now = withFrameNanos { it }
                                val dt = (now - last) / 1e9f
                                last = now
                                val y = finger?.y ?: continue
                                val depth = when {
                                    y < edgePx -> (edgePx - y) / edgePx
                                    y > viewH - edgePx -> -(y - (viewH - edgePx)) / edgePx
                                    else -> 0f
                                }.coerceIn(-1f, 1f)
                                edgeScrolling = sign(depth).toInt()
                                if (depth != 0f) {
                                    // Faster the deeper the handle sits in the band.
                                    val speed = (300f + 2400f * abs(depth)) * dt * sign(depth)
                                    view.scrollPx = (view.scrollPx + speed).coerceIn(0f, maxScrollState.value)
                                    update()
                                }
                            }
                        }
                        try {
                            while (true) {
                                val event = awaitPointerEvent(PointerEventPass.Initial)
                                val change = event.changes.firstOrNull { it.id == down.id } ?: break
                                if (!change.pressed) {
                                    change.consume()
                                    break
                                }
                                total += change.positionChange()
                                finger = change.position
                                change.consume()
                                update()
                            }
                        } finally {
                            auto.cancel()
                            edgeScrolling = 0
                            view.dragging = false
                            dragEnd = null
                            finger = null
                        }
                    }
                },
        ) {
            CompositionLocalProvider(LocalViewConfiguration provides config) {
                Canvas(
                    Modifier
                        .fillMaxSize()
                        .clipToBounds()
                        .alpha(if (dimmed) 0.5f else 1f)
                        .testTag("pane")
                        .semantics { stateDescription = sizeLabel(emulator.cols, emulator.rows, view.fit) }
                        .pointerInput(maxX, maxScroll, fitPx, readPx) {
                            detectTransformGestures { _, pan, zoom, _ ->
                                if (zoom != 1f) {
                                    val px = ((if (view.fit) fitPx else view.customPx ?: readPx) * zoom).coerceIn(fitPx, maxOf(fitPx, readPx) * 2f)
                                    view.fit = false
                                    view.customPx = px
                                }
                                if (pan.x != 0f) {
                                    view.offsetX = (view.offsetX - pan.x).coerceIn(0f, maxX)
                                    view.lastManualPan = System.currentTimeMillis()
                                }
                                if (pan.y != 0f) view.scrollPx = (view.scrollPx + pan.y).coerceIn(0f, maxScroll)
                            }
                        }
                        .pointerInput(Unit) {
                            detectTapGestures(
                                onDoubleTap = { view.toggleFit() },
                                onLongPress = { pos ->
                                    val buf = bufferState.value ?: return@detectTapGestures
                                    view.select(PaneSelection.word(buf, geoState.value.cell(pos.x, pos.y)))
                                    haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                                },
                                onTap = { pos ->
                                    val sel = view.selection ?: return@detectTapGestures
                                    val buf = bufferState.value ?: return@detectTapGestures
                                    if (sel.contains(buf, geoState.value.cell(pos.x, pos.y))) view.toolbar = !view.toolbar else view.endSelection()
                                },
                            )
                        },
                ) {
                    drawRect(Color(TerminalPalette.BACKGROUND))
                    val emu = emulator.emulator ?: return@Canvas
                    tick.version // redraw on every frame
                    val sel = view.selection
                    drawIntoCanvas { canvas ->
                        canvas.nativeCanvas.withTranslation(padPx - view.offsetX, top) {
                            renderer.render(emu, this, -rowsUp, -1, -1, -1, -1)
                            if (sel != null && buffer != null) drawHighlight(this, sel, buffer, renderer.fontWidth, lineH, rowsUp, selPaint)
                        }
                    }
                }
            }
            if (selection != null && buffer != null) {
                SelectionOverlay(selection, buffer, geo, viewW, viewH, dragEnd, finger, edgeScrolling, typeface, ::knobOf, ::tipOf)
                if (view.toolbar && !view.dragging) {
                    SelectionToolbar(
                        sel = selection,
                        buffer = buffer,
                        geo = geo,
                        viewH = viewH,
                        onCopy = {
                            val text = selection.text(buffer)
                            onCopy(text, text.lines().size)
                            view.endSelection()
                        },
                        onSelectAll = { view.select(PaneSelection.all(buffer)) },
                        onMode = { view.selection = selection.copy(mode = it) },
                    )
                }
            }
        }
    }
}

private fun glyphPaint(typeface: Typeface, size: Float) = Paint().apply {
    this.typeface = typeface
    textSize = size
    isAntiAlias = true
}

/**
 * Paints selected runs with one fixed pair, terminal foreground behind
 * terminal background text, whatever each cell's own colors are (design 13k).
 */
private fun drawHighlight(canvas: android.graphics.Canvas, sel: PaneSelection, buffer: SelBuffer, fontWidth: Float, lineH: Float, rowsUp: Int, paint: Paint) {
    val rows = buffer.rows
    for (run in sel.runs(buffer)) {
        if (run.row < -rowsUp || run.row >= rows) continue
        val y = (run.row + rowsUp) * lineH
        paint.color = TerminalPalette.FOREGROUND
        canvas.drawRect(run.from * fontWidth, y, (run.to + 1) * fontWidth, y + lineH, paint)
        paint.color = TerminalPalette.BACKGROUND
        drawGlyphs(canvas, buffer.glyphs(run.row), run.from, run.to, 0, fontWidth, y, lineH, paint)
    }
}

/** Draws columns [from]..[to] of a row's glyphs, column [origin] at x = 0. */
private fun drawGlyphs(canvas: android.graphics.Canvas, glyphs: Array<String>, from: Int, to: Int, origin: Int, fontWidth: Float, y: Float, lineH: Float, paint: Paint) {
    val baseline = y + (lineH - (paint.descent() - paint.ascent())) / 2 - paint.ascent()
    for (c in from.coerceAtLeast(0)..to.coerceAtMost(glyphs.size - 1)) {
        val g = glyphs[c]
        if (g.isEmpty() || g == " ") continue
        canvas.drawText(g, (c - origin) * fontWidth, baseline, paint)
    }
}

/** Handles, the loupe while dragging, and the edge auto-scroll glow and chip. */
@Composable
private fun SelectionOverlay(
    sel: PaneSelection,
    buffer: SelBuffer,
    geo: PaneGeometry,
    viewW: Float,
    viewH: Float,
    dragEnd: End?,
    finger: Offset?,
    edgeScrolling: Int,
    typeface: Typeface,
    knobOf: (End, PaneSelection, SelBuffer, PaneGeometry) -> Offset,
    tipOf: (End, PaneSelection, SelBuffer, PaneGeometry) -> Offset,
) {
    val c = St.colors
    val density = LocalDensity.current
    val knobR = with(density) { 7.dp.toPx() }
    val stem = with(density) { 2.dp.toPx() }
    val edgePx = with(density) { EDGE_BAND.toPx() }
    val loupePaint = remember(typeface, density) { glyphPaint(typeface, with(density) { LOUPE_SIZE.toPx() }) }
    Canvas(Modifier.fillMaxSize().testTag("selection-overlay")) {
        // Edge glow while auto-scrolling.
        if (edgeScrolling != 0) {
            val glow = c.acc.copy(alpha = 0.35f)
            if (edgeScrolling > 0) drawRect(Brush.verticalGradient(listOf(glow, Color.Transparent), 0f, edgePx), size = Size(viewW, edgePx))
            else drawRect(Brush.verticalGradient(listOf(Color.Transparent, glow), viewH - edgePx, viewH), topLeft = Offset(0f, viewH - edgePx), size = Size(viewW, edgePx))
        }
        for (end in End.entries) {
            val tip = tipOf(end, sel, buffer, geo)
            val knob = knobOf(end, sel, buffer, geo)
            val x = knob.x
            val rowTop = tip.y - geo.lineH / 2
            drawLine(c.acc, Offset(x, rowTop), Offset(x, knob.y), strokeWidth = stem)
            drawCircle(c.acc, knobR, knob)
        }
        // Loupe: the dragged end's row, ±7 cells, magnified above the finger.
        val f = finger
        if (dragEnd != null && f != null) {
            val cell = if (dragEnd == End.Start) buffer.snapStart(sel.start) else buffer.snapEnd(sel.end)
            val cw = loupePaint.measureText("M")
            val lh = loupePaint.fontSpacing
            val pad = 8.dp.toPx()
            val w = cw * (2 * LOUPE_CELLS + 1) + 2 * pad
            val h = lh + 2 * pad
            val left = (f.x - w / 2).coerceIn(0f, (viewW - w).coerceAtLeast(0f))
            val topY = (f.y - h - 56.dp.toPx()).coerceAtLeast(0f)
            drawRoundRect(c.surf2, Offset(left, topY), Size(w, h), CornerRadius(10.dp.toPx()))
            drawRoundRect(c.line, Offset(left, topY), Size(w, h), CornerRadius(10.dp.toPx()), style = Stroke(1.dp.toPx()))
            val origin = cell.col - LOUPE_CELLS
            val run = sel.runs(buffer).firstOrNull { it.row == cell.row }
            val glyphs = buffer.glyphs(cell.row)
            drawIntoCanvas { canvas ->
                canvas.nativeCanvas.withTranslation(left + pad, topY + pad) {
                    clipRect(0f, 0f, w - 2 * pad, lh)
                    drawRect(0f, 0f, w - 2 * pad, lh, Paint().apply { color = TerminalPalette.BACKGROUND })
                    loupePaint.color = TerminalPalette.FOREGROUND
                    if (run != null) {
                        val a = maxOf(run.from, origin)
                        val b = minOf(run.to, origin + 2 * LOUPE_CELLS)
                        if (a <= b) drawRect((a - origin) * cw, 0f, (b - origin + 1) * cw, lh, loupePaint)
                    }
                    for (col in origin..origin + 2 * LOUPE_CELLS) {
                        if (col < 0 || col >= glyphs.size) continue
                        val inRun = run != null && col in run.from..run.to
                        loupePaint.color = if (inRun) TerminalPalette.BACKGROUND else TerminalPalette.FOREGROUND
                        drawGlyphs(this, glyphs, col, col, origin, cw, 0f, lh, loupePaint)
                    }
                }
            }
            // Accent marker on the handle's edge.
            val edgeCol = if (dragEnd == End.Start) cell.col - origin else cell.col - origin + 1
            val mx = left + pad + edgeCol * cw
            drawLine(c.acc, Offset(mx, topY + pad - 3.dp.toPx()), Offset(mx, topY + pad + lh + 3.dp.toPx()), strokeWidth = 2.dp.toPx())
        }
    }
    if (edgeScrolling > 0) {
        Box(Modifier.fillMaxSize()) {
            Text(
                "Scrolling back",
                fontFamily = Mono,
                fontSize = 11.sp,
                color = c.accInk,
                modifier = Modifier.align(Alignment.TopCenter).padding(top = 8.dp).clip(RoundedCornerShape(12.dp)).background(c.ind).border(1.dp, c.acc, RoundedCornerShape(12.dp)).padding(horizontal = 9.dp, vertical = 3.dp).testTag("scrolling-back"),
            )
        }
    }
}

/**
 * Nocturne pill with Copy · Select all · Line/Block: 10 dp above the selection,
 * below the end handle when that would cross the chip row, otherwise pinned under
 * the chip row; it keeps 54 dp clear at the bottom for the Jump to live pill.
 */
@Composable
private fun SelectionToolbar(sel: PaneSelection, buffer: SelBuffer, geo: PaneGeometry, viewH: Float, onCopy: () -> Unit, onSelectAll: () -> Unit, onMode: (SelectMode) -> Unit) {
    val c = St.colors
    val density = LocalDensity.current
    var height by remember { mutableIntStateOf(0) }
    var width by remember { mutableIntStateOf(0) }
    BoxWithConstraints(Modifier.fillMaxSize()) {
        val viewW = constraints.maxWidth.toFloat()
        val chipRow = with(density) { EDGE_BAND.toPx() }
        val gap = with(density) { 10.dp.toPx() }
        val reach = with(density) { (HANDLE_REACH + HANDLE_HIT / 2 + 4.dp).toPx() }
        val bottomClear = with(density) { 54.dp.toPx() }
        val start = buffer.snapStart(sel.start)
        val end = buffer.snapEnd(sel.end)
        val above = geo.rowTop(start.row) - gap - height
        val below = geo.rowTop(end.row) + geo.lineH + reach
        val y = when {
            above >= chipRow && above + height <= viewH - bottomClear -> above
            below >= chipRow && below + height <= viewH - bottomClear -> below
            else -> chipRow
        }.coerceIn(0f, (viewH - bottomClear - height).coerceAtLeast(0f))
        val mid = (geo.cellLeft(start.col) + geo.cellLeft(end.col + 1)) / 2
        val x = (mid - width / 2).coerceIn(0f, (viewW - width).coerceAtLeast(0f))
        val shape = RoundedCornerShape(20.dp)
        Row(
            Modifier
                .offset { IntOffset(x.roundToInt(), y.roundToInt()) }
                .onSizeChanged {
                    height = it.height
                    width = it.width
                }
                .height(40.dp)
                .clip(shape)
                .background(c.surf2)
                .border(1.dp, c.line, shape)
                .padding(horizontal = 4.dp)
                .testTag("selection-toolbar"),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            ToolbarButton("Copy", "sel:copy", onCopy, icon = PhosphorIcons.Regular.Copy)
            ToolbarButton("Select all", "sel:all", onSelectAll)
            Spacer(Modifier.width(4.dp))
            val seg = RoundedCornerShape(12.dp)
            Row(Modifier.height(28.dp).clip(seg).border(1.dp, c.line, seg)) {
                SelectMode.entries.forEach { m ->
                    val on = sel.mode == m
                    Box(
                        Modifier.fillMaxHeight().background(if (on) c.accTint else Color.Transparent)
                            .clickable(role = Role.RadioButton) { onMode(m) }
                            .semantics { selected = on }
                            .padding(horizontal = 10.dp)
                            .testTag("sel:mode:${m.name.lowercase()}"),
                        contentAlignment = Alignment.Center,
                    ) {
                        Text(m.name, fontSize = 12.sp, fontWeight = FontWeight.Medium, color = if (on) c.acc else c.mute)
                    }
                }
            }
        }
    }
}

@Composable
private fun ToolbarButton(label: String, tag: String, onClick: () -> Unit, icon: ImageVector? = null) {
    val c = St.colors
    Row(
        Modifier.height(32.dp).clip(RoundedCornerShape(16.dp)).clickable(role = Role.Button, onClick = onClick).padding(horizontal = 10.dp).testTag(tag),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(5.dp),
    ) {
        if (icon != null) Icon(icon, null, tint = c.text, modifier = Modifier.size(14.dp))
        Text(label, fontSize = 13.sp, fontWeight = FontWeight.Medium, color = c.text)
    }
}
