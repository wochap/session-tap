package dev.sessiontap.android.ui.terminal

import android.content.Context
import android.text.InputType
import android.view.KeyEvent
import android.view.View
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.provider.Settings
import dev.sessiontap.android.net.TerminalKeys

/**
 * Invisible view that owns the soft keyboard in direct mode. Every key the IME
 * or a hardware keyboard produces becomes one terminal key, sent at once.
 * The IME gets `TYPE_NULL` (no autocorrect, no suggestions); IMEs known to
 * mishandle it get a visible-password field with suggestions off instead.
 */
class DirectInputView(context: Context) : View(context) {
    /** A key name or character, with Ctrl/Alt held on a hardware keyboard. */
    var onKey: (key: String, ctrl: Boolean, alt: Boolean) -> Unit = { _, _, _ -> }

    /** Text the IME committed at once (a word, a paste, a suggestion). */
    var onText: (String) -> Unit = {}

    init {
        isFocusable = true
        isFocusableInTouchMode = true
    }

    override fun onCheckIsTextEditor() = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        outAttrs.inputType = if (charBasedIme()) {
            InputType.TYPE_CLASS_TEXT or InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD or InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        } else {
            InputType.TYPE_NULL
        }
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or EditorInfo.IME_FLAG_NO_EXTRACT_UI or EditorInfo.IME_ACTION_NONE
        return Connection()
    }

    private fun charBasedIme(): Boolean {
        val ime = Settings.Secure.getString(context.contentResolver, Settings.Secure.DEFAULT_INPUT_METHOD).orEmpty()
        return CHAR_BASED_IMES.any { ime.startsWith(it) }
    }

    fun showKeyboard() {
        requestFocus()
        context.getSystemService(InputMethodManager::class.java).showSoftInput(this, 0)
    }

    fun hideKeyboard() {
        context.getSystemService(InputMethodManager::class.java).hideSoftInputFromWindow(windowToken, 0)
        clearFocus()
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        val key = keyName(event) ?: return super.onKeyDown(keyCode, event)
        onKey(key, event.isCtrlPressed, event.isAltPressed)
        return true
    }

    private inner class Connection : BaseInputConnection(this, true) {
        override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
            if (text.isNotEmpty()) onText(text.toString())
            editable?.clear()
            return true
        }

        override fun finishComposingText(): Boolean {
            val pending = editable?.toString().orEmpty()
            super.finishComposingText()
            if (pending.isNotEmpty()) onText(pending)
            editable?.clear()
            return true
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            repeat(beforeLength.coerceAtMost(MAX_DELETE)) { onKey(TerminalKeys.BACKSPACE, false, false) }
            repeat(afterLength.coerceAtMost(MAX_DELETE)) { onKey(TerminalKeys.DELETE, false, false) }
            return true
        }

        override fun sendKeyEvent(event: KeyEvent): Boolean {
            if (event.action == KeyEvent.ACTION_DOWN) {
                keyName(event)?.let { onKey(it, event.isCtrlPressed, event.isAltPressed) }
            }
            return true
        }
    }

    companion object {
        private const val MAX_DELETE = 64

        /** IMEs that misbehave with `TYPE_NULL`. */
        private val CHAR_BASED_IMES = listOf("com.samsung.android.honeyboard", "com.sec.android.inputmethod")

        /** The terminal key for a key event, or null for keys the terminal doesn't take. */
        fun keyName(event: KeyEvent): String? = when (event.keyCode) {
            KeyEvent.KEYCODE_ENTER, KeyEvent.KEYCODE_NUMPAD_ENTER -> TerminalKeys.ENTER
            KeyEvent.KEYCODE_DEL -> TerminalKeys.BACKSPACE
            KeyEvent.KEYCODE_FORWARD_DEL -> TerminalKeys.DELETE
            KeyEvent.KEYCODE_TAB -> if (event.isShiftPressed) TerminalKeys.BACK_TAB else TerminalKeys.TAB
            KeyEvent.KEYCODE_ESCAPE -> TerminalKeys.ESCAPE
            KeyEvent.KEYCODE_SPACE -> TerminalKeys.SPACE
            KeyEvent.KEYCODE_DPAD_UP -> TerminalKeys.UP
            KeyEvent.KEYCODE_DPAD_DOWN -> TerminalKeys.DOWN
            KeyEvent.KEYCODE_DPAD_LEFT -> TerminalKeys.LEFT
            KeyEvent.KEYCODE_DPAD_RIGHT -> TerminalKeys.RIGHT
            KeyEvent.KEYCODE_MOVE_HOME -> TerminalKeys.HOME
            KeyEvent.KEYCODE_MOVE_END -> TerminalKeys.END
            KeyEvent.KEYCODE_PAGE_UP -> TerminalKeys.PAGE_UP
            KeyEvent.KEYCODE_PAGE_DOWN -> TerminalKeys.PAGE_DOWN
            in KeyEvent.KEYCODE_F1..KeyEvent.KEYCODE_F12 -> "f${event.keyCode - KeyEvent.KEYCODE_F1 + 1}"
            else -> {
                val unicode = event.getUnicodeChar(event.metaState and (KeyEvent.META_CTRL_MASK or KeyEvent.META_ALT_MASK).inv())
                if (unicode > 0 && unicode and COMBINING == 0 && !Character.isISOControl(unicode)) {
                    String(Character.toChars(unicode))
                } else {
                    null
                }
            }
        }

        private const val COMBINING = android.view.KeyCharacterMap.COMBINING_ACCENT
    }
}
