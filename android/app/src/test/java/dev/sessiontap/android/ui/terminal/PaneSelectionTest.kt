package dev.sessiontap.android.ui.terminal

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class PaneSelectionTest {
    private fun buffer(text: String, cols: Int = 40, rows: Int = 6): SelBuffer {
        val pane = PaneEmulator().apply { reset(snapshotFrame(text, cols = cols, rows = rows)) }
        val emu = pane.emulator!!
        return TermuxSelBuffer(emu.screen, emu.mColumns, emu.mRows)
    }

    @Test
    fun pixelToCellFollowsPanScrollAndFont() {
        val g = PaneGeometry(fontWidth = 10f, lineH = 20f, padPx = 6f, offsetX = 0f, top = 0f, rowsUp = 0)
        assertEquals(Cell(0, 0), g.cell(6f, 0f))
        assertEquals(Cell(1, 3), g.cell(36f + 5f, 25f))
        // panned sideways by 100 px: ten columns further right
        assertEquals(Cell(1, 13), g.copy(offsetX = 100f).cell(41f, 25f))
        // three scrollback rows drawn above the screen
        assertEquals(Cell(-3, 0), g.copy(rowsUp = 3).cell(6f, 5f))
        // scrolled by half a line: top moved down
        assertEquals(Cell(-1, 0), g.copy(rowsUp = 1, top = -10f).cell(6f, 5f))
        // a bigger font
        assertEquals(Cell(0, 2), g.copy(fontWidth = 15f, lineH = 30f).cell(6f + 31f, 29f))
        assertEquals(16f, g.copy(offsetX = 10f).cellLeft(2))
        assertEquals(40f, g.copy(rowsUp = 1).rowTop(1))
    }

    @Test
    fun wordRuleKeepsPathsAndDropsTrailingPunctuation() {
        val b = buffer("open ./src/auth/session.rs: done. ~/x@y+z-1 it's,", cols = 80)
        assertEquals("./src/auth/session.rs", PaneSelection.word(b, Cell(0, 12)).text(b))
        assertEquals("done", PaneSelection.word(b, Cell(0, 29)).text(b))
        assertEquals("~/x@y+z-1", PaneSelection.word(b, Cell(0, 35)).text(b))
        assertEquals("it's", PaneSelection.word(b, Cell(0, 44)).text(b))
        // a blank selects just that cell
        assertEquals(PaneSelection(Cell(0, 4), Cell(0, 4)), PaneSelection.word(b, Cell(0, 4)))
    }

    @Test
    fun lineModeJoinsSoftWrapsAndKeepsHardBreaks() {
        val b = buffer("abcdefghijklmno\r\nxy   \r\nlast", cols = 10)
        assertTrue(b.wraps(0))
        assertEquals("abcdefghijklmno", PaneSelection(Cell(0, 0), Cell(1, 4)).text(b))
        assertEquals("fghijklmno\nxy\nla", PaneSelection(Cell(0, 5), Cell(3, 1)).text(b))
    }

    @Test
    fun crossedHandlesSwapRoles() {
        val sel = PaneSelection(Cell(2, 5), Cell(0, 3))
        assertEquals(Cell(0, 3), sel.start)
        assertEquals(Cell(2, 5), sel.end)
        assertEquals(PaneSelection(Cell(0, 3), Cell(2, 5)), sel.normalized())
    }

    @Test
    fun lineRunsFollowEditorRule() {
        val b = buffer("one\r\n\r\nthree four", cols = 20)
        val runs = PaneSelection(Cell(0, 1), Cell(2, 4)).runs(b)
        // printed text plus one cell for each hard break, then up to the end column
        assertEquals(listOf(SelRun(0, 1, 3), SelRun(1, 0, 0), SelRun(2, 0, 4)), runs)
    }

    @Test
    fun blockModeCopiesAColumn() {
        val text = (0 until 6).joinToString("\r\n") { r -> "row$r " + "abcdefghijkl".drop(r) }
        val b = buffer(text, cols = 30, rows = 8)
        val sel = PaneSelection(Cell(1, 4), Cell(4, 8), SelectMode.Block)
        assertEquals(listOf(SelRun(1, 4, 8), SelRun(2, 4, 8), SelRun(3, 4, 8), SelRun(4, 4, 8)), sel.runs(b))
        assertEquals(" bcde\n cdef\n defg\n efgh", sel.text(b))
    }

    @Test
    fun blockDoesNotJoinSoftWraps() {
        val b = buffer("abcdefghijklmno", cols = 10)
        assertEquals("ab\nkl", PaneSelection(Cell(0, 0), Cell(1, 1), SelectMode.Block).text(b))
    }

    @Test
    fun modeToggleKeepsCells() {
        val sel = PaneSelection(Cell(3, 9), Cell(1, 2))
        val back = sel.toggleMode().toggleMode()
        assertEquals(sel, back)
        assertEquals(Cell(1, 2), sel.toggleMode().start)
        assertEquals(Cell(3, 9), sel.toggleMode().end)
        assertEquals(PaneSelection(Cell(3, 2), Cell(1, 9), SelectMode.Block).start, Cell(1, 2))
    }

    @Test
    fun wideCharactersAreSelectedWhole() {
        val b = buffer("a中b")
        assertEquals(listOf("a", "中", "", "b"), b.glyphs(0).take(4))
        // an end on the second column snaps outward
        assertEquals("中", PaneSelection(Cell(0, 2), Cell(0, 2)).text(b))
        assertEquals(listOf(SelRun(0, 1, 2)), PaneSelection(Cell(0, 2), Cell(0, 1)).runs(b))
        assertEquals("中", PaneSelection.word(b, Cell(0, 2)).let { it.copy(anchor = Cell(0, 1)) }.copy(focus = Cell(0, 1)).text(b))
    }

    @Test
    fun selectAllCoversScrollback() {
        val b = buffer((1..20).joinToString("\r\n") { "line $it" }, rows = 5)
        assertEquals(15, b.scrollback)
        val all = PaneSelection.all(b)
        assertEquals(Cell(-15, 0), all.start)
        assertEquals(SelectMode.Line, all.mode)
        val text = all.text(b)
        assertTrue(text.startsWith("line 1\nline 2\n"))
        assertTrue(text.trimEnd().endsWith("line 20"))
        assertFalse(all.contains(b, Cell(-16, 0)))
    }
}
