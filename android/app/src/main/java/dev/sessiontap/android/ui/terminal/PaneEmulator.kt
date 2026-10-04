package dev.sessiontap.android.ui.terminal

import com.termux.terminal.TerminalEmulator
import com.termux.terminal.TerminalOutput
import com.termux.terminal.TerminalSession
import com.termux.terminal.TerminalSessionClient
import com.termux.terminal.TextStyle
import dev.sessiontap.android.net.TerminalFrame

/** Lines of scrollback kept above the screen. */
const val SCROLLBACK_LINES = 500

/**
 * A Termux [TerminalEmulator] fed from relay frames instead of a PTY. A
 * snapshot replaces the emulator; output appends. Nothing is ever written back:
 * replies the emulator would send (device status reports) are dropped.
 */
class PaneEmulator(private val palette: IntArray = TerminalPalette.argb) {
    var emulator: TerminalEmulator? = null
        private set

    /** Bumped on every change so the UI redraws. */
    var version: Long = 0
        private set

    val cols: Int get() = emulator?.mColumns ?: 0
    val rows: Int get() = emulator?.mRows ?: 0

    /** Scrollback rows currently held above the screen. */
    val scrollbackRows: Int get() = emulator?.screen?.activeTranscriptRows ?: 0

    fun reset(snapshot: TerminalFrame.Snapshot) {
        val emu = TerminalEmulator(NoOutput, snapshot.cols.coerceAtLeast(1), snapshot.rows.coerceAtLeast(1), SCROLLBACK_LINES + snapshot.rows.coerceAtLeast(1), NoClient)
        applyPalette(emu)
        val bytes = snapshot.bytes
        emu.append(bytes, bytes.size)
        val cursor = "\u001b[0m\u001b[${snapshot.cursor.y + 1};${snapshot.cursor.x + 1}H\u001b[?25${if (snapshot.cursor.visible) 'h' else 'l'}"
        emu.append(cursor.toByteArray(), cursor.length)
        emu.clearScrollCounter()
        emulator = emu
        version++
    }

    /** Appends output; returns how many lines scrolled off the top. */
    fun append(bytes: ByteArray): Int {
        val emu = emulator ?: return 0
        emu.clearScrollCounter()
        emu.append(bytes, bytes.size)
        val scrolled = emu.scrollCounter
        emu.clearScrollCounter()
        version++
        return scrolled
    }

    /** The visible screen as text, trailing blanks trimmed. */
    fun screenText(): String {
        val emu = emulator ?: return ""
        return emu.screen.getSelectedText(0, 0, emu.mColumns - 1, emu.mRows - 1).lines().joinToString("\n") { it.trimEnd() }.trimEnd()
    }

    private fun applyPalette(emu: TerminalEmulator) {
        palette.copyInto(emu.mColors.mCurrentColors, 0, 0, minOf(palette.size, emu.mColors.mCurrentColors.size))
    }

    private object NoOutput : TerminalOutput() {
        override fun write(data: ByteArray?, offset: Int, count: Int) {}
        override fun titleChanged(oldTitle: String?, newTitle: String?) {}
        override fun onCopyTextToClipboard(text: String?) {}
        override fun onPasteTextFromClipboard() {}
        override fun onBell() {}
        override fun onColorsChanged() {}
    }

    private object NoClient : TerminalSessionClient {
        override fun onTextChanged(changedSession: TerminalSession) {}
        override fun onTitleChanged(changedSession: TerminalSession) {}
        override fun onSessionFinished(finishedSession: TerminalSession) {}
        override fun onCopyTextToClipboard(session: TerminalSession, text: String?) {}
        override fun onPasteTextFromClipboard(session: TerminalSession?) {}
        override fun onBell(session: TerminalSession) {}
        override fun onColorsChanged(session: TerminalSession) {}
        override fun onTerminalCursorStateChange(state: Boolean) {}
        override fun getTerminalCursorStyle(): Int = TerminalEmulator.DEFAULT_TERMINAL_CURSOR_STYLE
        override fun logError(tag: String?, message: String?) {}
        override fun logWarn(tag: String?, message: String?) {}
        override fun logInfo(tag: String?, message: String?) {}
        override fun logDebug(tag: String?, message: String?) {}
        override fun logVerbose(tag: String?, message: String?) {}
        override fun logStackTraceWithMessage(tag: String?, message: String?, e: Exception?) {}
        override fun logStackTrace(tag: String?, e: Exception?) {}
    }
}

/** The handoff's ANSI-16 palette plus foreground, background, and cursor; the same in both app themes. */
object TerminalPalette {
    /** oklch (l, c, h) for colors 0-15. */
    private val ANSI = listOf(
        Triple(0.34, 0.02, 275.0), Triple(0.68, 0.16, 22.0), Triple(0.76, 0.13, 150.0), Triple(0.83, 0.12, 85.0),
        Triple(0.72, 0.11, 255.0), Triple(0.72, 0.12, 288.0), Triple(0.78, 0.09, 205.0), Triple(0.84, 0.01, 275.0),
        Triple(0.52, 0.02, 275.0), Triple(0.76, 0.15, 22.0), Triple(0.84, 0.12, 150.0), Triple(0.90, 0.11, 90.0),
        Triple(0.80, 0.10, 255.0), Triple(0.80, 0.11, 288.0), Triple(0.86, 0.08, 205.0), Triple(0.97, 0.005, 275.0),
    )

    /** Nocturne ground mixed 55% toward black. */
    const val BACKGROUND = 0xFF06070E.toInt()
    val FOREGROUND: Int = oklchArgb(0.93, 0.006, 275.0)

    /** Colors indexed like Termux's `mCurrentColors`: 0-15 ANSI, then the defaults at their fixed slots. */
    val argb: IntArray by lazy {
        val base = com.termux.terminal.TerminalColors.COLOR_SCHEME.mDefaultColors.copyOf()
        ANSI.forEachIndexed { i, (l, c, h) -> base[i] = oklchArgb(l, c, h) }
        base[TextStyle.COLOR_INDEX_FOREGROUND] = FOREGROUND
        base[TextStyle.COLOR_INDEX_BACKGROUND] = BACKGROUND
        base[TextStyle.COLOR_INDEX_CURSOR] = FOREGROUND
        // An in-band reset (ESC c, OSC 104) restores the scheme defaults, so they carry this palette too.
        base.copyInto(com.termux.terminal.TerminalColors.COLOR_SCHEME.mDefaultColors)
        base
    }

    fun ansi(index: Int): Int = argb[index]

    private fun oklchArgb(l: Double, c: Double, h: Double): Int {
        val color = dev.sessiontap.android.ui.theme.oklch(l, c, h)
        fun byte(v: Float) = Math.round(v * 255)
        return (0xFF shl 24) or (byte(color.red) shl 16) or (byte(color.green) shl 8) or byte(color.blue)
    }
}
