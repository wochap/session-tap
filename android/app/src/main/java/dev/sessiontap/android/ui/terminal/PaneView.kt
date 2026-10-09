package dev.sessiontap.android.ui.terminal

import android.graphics.Typeface
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.Stable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.draw.clipToBounds
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.drawscope.drawIntoCanvas
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.res.ResourcesCompat
import androidx.core.graphics.withTranslation
import com.termux.view.TerminalRenderer
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

    val scrolledUp: Boolean get() = scrollPx > 0.5f

    fun toggleFit() {
        fit = !fit
        customPx = null
    }

    fun jumpToLive() {
        scrollPx = 0f
        newLines = 0
    }
}

@Composable
fun rememberPaneViewState(fit: Boolean) = remember(fit) { PaneViewState(fit) }

/** Size label for the chip: real pane size plus the current font mode. */
fun sizeLabel(cols: Int, rows: Int, fit: Boolean): String = "$cols×$rows · ${if (fit) "fit" else "${READ_SIZE.value.roundToInt()}sp"}"

/**
 * Draws the emulator with Termux's renderer on a Compose canvas. The pane keeps
 * the desktop's column count; when it is wider than the view, it pans
 * sideways and follows the cursor unless the user panned in the last few seconds.
 */
@Composable
fun PaneView(
    emulator: PaneEmulator,
    tick: PaneTick,
    view: PaneViewState,
    dimmed: Boolean,
    modifier: Modifier = Modifier,
) {
    val context = LocalContext.current
    val density = LocalDensity.current
    val typeface = remember { ResourcesCompat.getFont(context, R.font.jetbrains_mono_nerd) ?: Typeface.MONOSPACE }
    val ratio = remember(typeface) { advanceRatio(typeface) }
    val readPx = with(density) { READ_SIZE.toPx() }
    val padPx = with(density) { 6.dp.toPx() }
    var lastScrolled by remember { mutableLongStateOf(tick.scrolled) }
    BoxWithConstraints(modifier.fillMaxSize()) {
        val viewW = constraints.maxWidth.toFloat()
        val viewH = constraints.maxHeight.toFloat()
        val cols = emulator.cols.coerceAtLeast(1)
        val fitPx = ((viewW - 2 * padPx) / (cols * ratio)).coerceAtLeast(1f)
        val textPx = if (view.fit) fitPx else (view.customPx ?: readPx)
        val renderer = remember(textPx.roundToInt(), typeface) { TerminalRenderer(textPx.roundToInt().coerceAtLeast(1), typeface) }
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
        // Follow the cursor sideways unless the user panned recently.
        LaunchedEffect(tick.version, textPx, viewW) {
            val emu = emulator.emulator ?: return@LaunchedEffect
            if (System.currentTimeMillis() - view.lastManualPan < MANUAL_PAN_HOLD_MS) return@LaunchedEffect
            val cursorX = emu.cursorCol * renderer.fontWidth + padPx
            if (cursorX < view.offsetX || cursorX > view.offsetX + viewW - 2 * renderer.fontWidth) {
                view.offsetX = (cursorX - viewW * 0.75f).coerceIn(0f, maxX)
            }
        }
        view.offsetX = view.offsetX.coerceIn(0f, maxX)
        view.scrollPx = view.scrollPx.coerceIn(0f, maxScroll)
        if (!view.scrolledUp) view.newLines = 0

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
                .pointerInput(Unit) { detectTapGestures(onDoubleTap = { view.toggleFit() }) },
        ) {
            drawRect(Color(TerminalPalette.BACKGROUND))
            val emu = emulator.emulator ?: return@Canvas
            tick.version // redraw on every frame
            val transcript = emulator.scrollbackRows
            val rowsUp = ceil((viewH + view.scrollPx) / lineH - emu.mRows).toInt().coerceIn(0, transcript)
            val top = viewH - (emu.mRows + rowsUp) * lineH + view.scrollPx
            drawIntoCanvas { canvas ->
                canvas.nativeCanvas.withTranslation(padPx - view.offsetX, top) {
                    renderer.render(emu, this, -rowsUp, -1, -1, -1, -1)
                }
            }
        }
    }
}
