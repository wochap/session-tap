package dev.sessiontap.android.ui.terminal

import com.termux.terminal.TerminalBuffer
import com.termux.terminal.WcWidth
import kotlin.math.floor

/** A buffer cell in Termux external coordinates: negative rows are scrollback, `0..rows-1` the screen. */
data class Cell(val row: Int, val col: Int)

enum class SelectMode { Line, Block }

/** One highlighted stretch of a row, columns inclusive. */
data class SelRun(val row: Int, val from: Int, val to: Int)

/**
 * A selection between two handle cells. Line mode orders them like a text
 * editor; Block mode uses their bounding rectangle. Toggling the mode keeps both
 * cells, and dragging one past the other swaps the start and end roles.
 */
data class PaneSelection(val anchor: Cell, val focus: Cell, val mode: SelectMode = SelectMode.Line) {
    /** Start handle: earlier cell in Line mode, top-left corner in Block mode. */
    val start: Cell
        get() = when (mode) {
            SelectMode.Line -> if (before(anchor, focus)) anchor else focus
            SelectMode.Block -> Cell(minOf(anchor.row, focus.row), minOf(anchor.col, focus.col))
        }

    /** End handle: later cell in Line mode, bottom-right corner in Block mode. */
    val end: Cell
        get() = when (mode) {
            SelectMode.Line -> if (before(anchor, focus)) focus else anchor
            SelectMode.Block -> Cell(maxOf(anchor.row, focus.row), maxOf(anchor.col, focus.col))
        }

    fun toggleMode() = copy(mode = if (mode == SelectMode.Line) SelectMode.Block else SelectMode.Line)

    /** Same selection with anchor at the start handle and focus at the end handle, ready for a handle drag. */
    fun normalized() = copy(anchor = start, focus = end)

    /** Whether [cell] is highlighted. */
    fun contains(buffer: SelBuffer, cell: Cell): Boolean = runs(buffer).any { it.row == cell.row && cell.col in it.from..it.to }

    /** Highlighted runs, one per row at most. */
    fun runs(buffer: SelBuffer): List<SelRun> {
        val s = buffer.snapStart(start)
        val e = buffer.snapEnd(end)
        if (mode == SelectMode.Block) return (s.row..e.row).map { SelRun(it, s.col, e.col) }
        val last = buffer.cols - 1
        return (s.row..e.row).mapNotNull { row ->
            val from = if (row == s.row) s.col else 0
            if (row == e.row) return@mapNotNull SelRun(row, from, e.col).takeIf { from <= e.col }
            // Editor rule: up to the last printed column, plus one cell for a hard line break.
            val to = if (buffer.wraps(row)) last else minOf(buffer.lastPrinted(row) + 1, last)
            SelRun(row, from, maxOf(from, to))
        }
    }

    /** Plain text: Line joins soft-wrapped rows and keeps printed newlines; Block gives one line per row. Trailing blanks are trimmed per line. */
    fun text(buffer: SelBuffer): String {
        val s = buffer.snapStart(start)
        val e = buffer.snapEnd(end)
        return when (mode) {
            SelectMode.Line -> buffer.text(s.col, s.row, e.col, e.row, join = true).lines().joinToString("\n") { it.trimEnd() }
            SelectMode.Block -> (s.row..e.row).joinToString("\n") { buffer.text(s.col, it, e.col, it, join = false).trimEnd() }
        }
    }

    companion object {
        private fun before(a: Cell, b: Cell) = a.row < b.row || (a.row == b.row && a.col <= b.col)

        /** The whole buffer, scrollback included, in Line mode. */
        fun all(buffer: SelBuffer) = PaneSelection(Cell(-buffer.scrollback, 0), Cell(buffer.rows - 1, buffer.cols - 1))

        /** The word under [cell], or the cell alone when it holds no word character. */
        fun word(buffer: SelBuffer, cell: Cell): PaneSelection {
            val c = buffer.clamp(cell)
            val glyphs = buffer.glyphs(c.row)
            val lead = buffer.lead(c.row, c.col)
            if (!isWord(glyphs[lead])) return PaneSelection(Cell(c.row, lead), Cell(c.row, lead))
            var from = lead
            while (from > 0 && isWord(glyphs[buffer.lead(c.row, from - 1)])) from = buffer.lead(c.row, from - 1)
            var to = lead
            while (to + 1 < glyphs.size && isWord(glyphs[to + 1])) to++
            while (to > lead && glyphs[to] in TRAILING) to--
            return PaneSelection(Cell(c.row, from), Cell(c.row, to))
        }

        private val TRAILING = setOf(".", ":", ",", "'")

        /** Letters, digits, and `_ . / ~ : @ + - '`. A wide continuation column (`""`) counts with its glyph. */
        fun isWord(glyph: String): Boolean {
            if (glyph.isEmpty()) return true
            val cp = glyph.codePointAt(0)
            return Character.isLetterOrDigit(cp) || cp.toChar() in "_./~:@+-'"
        }
    }
}

/**
 * Read access to the emulator's buffer for selection. Glyph arrays have one
 * entry per column: the text at a character's first column, `""` on the second
 * column of a wide character, and `" "` for blanks.
 */
interface SelBuffer {
    val cols: Int
    val rows: Int
    val scrollback: Int
    fun glyphs(row: Int): Array<String>
    fun wraps(row: Int): Boolean
    fun text(x1: Int, y1: Int, x2: Int, y2: Int, join: Boolean): String

    fun clamp(cell: Cell) = Cell(cell.row.coerceIn(-scrollback, rows - 1), cell.col.coerceIn(0, cols - 1))

    /** The first column of the character covering [col]. */
    fun lead(row: Int, col: Int): Int {
        val g = glyphs(row)
        var c = col.coerceIn(0, cols - 1)
        while (c > 0 && g[c].isEmpty()) c--
        return c
    }

    /** Last column of the character covering [col]. */
    fun tail(row: Int, col: Int): Int {
        val g = glyphs(row)
        var c = lead(row, col)
        while (c + 1 < cols && g[c + 1].isEmpty()) c++
        return c
    }

    fun snapStart(cell: Cell) = clamp(cell).let { Cell(it.row, lead(it.row, it.col)) }
    fun snapEnd(cell: Cell) = clamp(cell).let { Cell(it.row, tail(it.row, it.col)) }

    /** Last column holding a printed character, or -1 for a blank row. */
    fun lastPrinted(row: Int): Int {
        val g = glyphs(row)
        for (c in g.indices.reversed()) if (g[c] != " " && g[c].isNotEmpty()) return tail(row, c)
        return -1
    }
}

/** [SelBuffer] over a Termux [TerminalBuffer]. */
class TermuxSelBuffer(private val buffer: TerminalBuffer, override val cols: Int, override val rows: Int) : SelBuffer {
    override val scrollback: Int get() = buffer.activeTranscriptRows
    private val cache = HashMap<Int, Array<String>>()

    override fun glyphs(row: Int): Array<String> = cache.getOrPut(row) {
        val line = buffer.allocateFullLineIfNecessary(buffer.externalToInternalRow(row))
        val out = Array(cols) { " " }
        val text = line.mText
        val used = line.spaceUsed
        var i = 0
        var col = 0
        while (i < used && col < cols) {
            val cp = Character.codePointAt(text, i)
            val n = Character.charCount(cp)
            val w = WcWidth.width(cp)
            if (w <= 0) {
                // A combining mark joins the previous glyph.
                if (col > 0) out[col - 1] += String(Character.toChars(cp))
            } else {
                out[col] = String(Character.toChars(cp))
                for (k in 1 until w) if (col + k < cols) out[col + k] = ""
                col += w
            }
            i += n
        }
        out
    }

    override fun wraps(row: Int): Boolean {
        buffer.allocateFullLineIfNecessary(buffer.externalToInternalRow(row))
        return buffer.getLineWrap(row)
    }

    override fun text(x1: Int, y1: Int, x2: Int, y2: Int, join: Boolean): String = buffer.getSelectedText(x1, y1, x2, y2, join)
}

/** Pixel↔cell mapping for the pane at its current font, pan, and scroll. */
data class PaneGeometry(
    val fontWidth: Float,
    val lineH: Float,
    val padPx: Float,
    val offsetX: Float,
    /** Y of external row `-rowsUp`, the first drawn row. */
    val top: Float,
    val rowsUp: Int,
) {
    fun cell(x: Float, y: Float): Cell = Cell(floor((y - top) / lineH).toInt() - rowsUp, floor((x + offsetX - padPx) / fontWidth).toInt())

    fun cellLeft(col: Int): Float = col * fontWidth + padPx - offsetX

    fun rowTop(row: Int): Float = top + (row + rowsUp) * lineH
}
