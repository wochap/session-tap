package dev.sessiontap.android.ui.terminal

import com.termux.terminal.TextStyle
import dev.sessiontap.android.net.InputState
import dev.sessiontap.android.net.TerminalCursor
import dev.sessiontap.android.net.TerminalFrame
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Base64

class PaneEmulatorTest {
    private fun snapshot(data: String, cols: Int = 120, rows: Int = 30, cursor: TerminalCursor = TerminalCursor(0, 0)) = TerminalFrame.Snapshot(
        seq = 0,
        cols = cols,
        rows = rows,
        cursor = cursor,
        data = Base64.getEncoder().encodeToString(data.toByteArray()),
        input = InputState(true),
    )

    /** Bytes as a Claude approval screen draws them: SGR colors, box drawing, a numbered menu. */
    private val approval = buildString {
        append("\u001b[38;5;246m╭" + "─".repeat(116) + "╮\u001b[0m\r\n")
        append("│ \u001b[1mBash command\u001b[0m\r\n")
        append("│   sudo apt install -y postgresql-16\r\n")
        append("│ Do you want to proceed?\r\n")
        append("│ \u001b[35m❯ 1. Yes\u001b[0m\r\n")
        append("│   2. Yes, and don't ask again for sudo apt install commands in ~/code/api\r\n")
        append("│   3. No, and tell Claude what to do differently \u001b[2m(esc)\u001b[0m\r\n")
        append("\u001b[38;2;120;200;255m╰" + "─".repeat(116) + "╯\u001b[0m")
    }

    @Test
    fun rendersCapturedApprovalScreenWithoutPty() {
        val pane = PaneEmulator()
        pane.reset(snapshot(approval, cursor = TerminalCursor(4, 4)))
        assertEquals(120, pane.cols)
        assertEquals(30, pane.rows)
        val text = pane.screenText()
        assertTrue(text, text.contains("Do you want to proceed?"))
        assertTrue(text, text.contains("❯ 1. Yes"))
        assertTrue(text, text.lines().first().startsWith("╭──"))
        assertEquals(4, pane.emulator!!.cursorRow)
        assertEquals(4, pane.emulator!!.cursorCol)
        // the magenta menu line uses ANSI 5, which the palette maps
        val style = pane.emulator!!.screen.getStyleAt(4, 4)
        assertEquals(5, TextStyle.decodeForeColor(style))
    }

    @Test
    fun snapshotReplacesAndOutputAppends() {
        val pane = PaneEmulator()
        pane.reset(snapshot("old screen", cols = 120, rows = 40))
        pane.append("\r\nmore".toByteArray())
        assertTrue(pane.screenText().contains("more"))
        pane.reset(snapshot("new screen", cols = 132, rows = 38))
        assertEquals(132, pane.cols)
        assertEquals(38, pane.rows)
        assertEquals("new screen", pane.screenText())
    }

    @Test
    fun outputCountsScrolledLines() {
        val pane = PaneEmulator()
        pane.reset(snapshot("", rows = 5, cursor = TerminalCursor(0, 4)))
        val scrolled = pane.append((1..12).joinToString("") { "line $it\r\n" }.toByteArray())
        assertEquals(12, scrolled)
    }

    @Test
    fun scrollbackIsCappedAt500Lines() {
        val pane = PaneEmulator()
        pane.reset(snapshot("", rows = 10, cursor = TerminalCursor(0, 9)))
        pane.append((1..800).joinToString("") { "line $it\r\n" }.toByteArray())
        assertEquals(SCROLLBACK_LINES, pane.scrollbackRows)
    }

    @Test
    fun ansi16UsesTheHandoffPalette() {
        val pane = PaneEmulator()
        pane.reset(snapshot("x"))
        val colors = pane.emulator!!.mColors.mCurrentColors
        for (i in 0 until 16) assertEquals(TerminalPalette.ansi(i), colors[i])
        assertEquals(TerminalPalette.BACKGROUND, colors[TextStyle.COLOR_INDEX_BACKGROUND])
        assertEquals(TerminalPalette.FOREGROUND, colors[TextStyle.COLOR_INDEX_FOREGROUND])
        // an in-band reset keeps the palette
        pane.append("\u001bc".toByteArray())
        assertEquals(TerminalPalette.ansi(1), pane.emulator!!.mColors.mCurrentColors[1])
    }
}
