package dev.sessiontap.android.domain

import dev.sessiontap.android.net.TerminalKeys
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.builtins.serializer
import kotlinx.serialization.json.Json

/** One key-bar slot: a named key, a character, Ctrl+C, Paste, or a modifier. */
sealed interface KeySpec {
    /** Stored form: a named key id, `char:<c>`, `ctrl_c`, `paste`, `mod:ctrl`, or `mod:alt`. */
    val id: String

    data class Named(val key: String) : KeySpec {
        override val id get() = key
    }

    data class Char(val char: String) : KeySpec {
        override val id get() = "char:$char"
    }

    data object CtrlC : KeySpec {
        override val id = TerminalKeys.CTRL_C
    }

    data object Paste : KeySpec {
        override val id = "paste"
    }

    data object Ctrl : KeySpec {
        override val id = "mod:ctrl"
    }

    data object Alt : KeySpec {
        override val id = "mod:alt"
    }

    val isModifier: Boolean get() = this == Ctrl || this == Alt

    /** Label shown on the key (arrows and Backspace show an icon instead). */
    val label: String
        get() = when (this) {
            is Named -> when (key) {
                TerminalKeys.BACK_TAB -> "S-Tab"
                TerminalKeys.BACKSPACE -> "Bksp"
                else -> TerminalKeys.label(key)
            }
            is Char -> char
            CtrlC -> "Ctrl+C"
            Paste -> "Paste"
            Ctrl -> "Ctrl"
            Alt -> "Alt"
        }

    /** What the key does, for its long-press popover. */
    val description: String
        get() = when (this) {
            is Named -> when (key) {
                TerminalKeys.ESCAPE -> "sends Esc"
                TerminalKeys.TAB -> "sends Tab"
                TerminalKeys.BACK_TAB -> "Shift+Tab · cycles mode"
                TerminalKeys.UP, TerminalKeys.DOWN, TerminalKeys.LEFT, TerminalKeys.RIGHT -> "arrow key"
                TerminalKeys.ENTER -> "sends Enter"
                TerminalKeys.SPACE -> "sends a space"
                TerminalKeys.BACKSPACE -> "deletes left"
                TerminalKeys.DELETE -> "deletes right"
                TerminalKeys.HOME -> "line start"
                TerminalKeys.END -> "line end"
                TerminalKeys.PAGE_UP -> "page up"
                TerminalKeys.PAGE_DOWN -> "page down"
                else -> "function key"
            }
            is Char -> "types the character"
            CtrlC -> "interrupt · tap twice"
            Paste -> "clipboard → reply field"
            Ctrl, Alt -> "tap: next key · hold: lock"
        }

    companion object {
        /** The parsed spec, or null for an unknown id. */
        fun parse(id: String): KeySpec? = when {
            id == TerminalKeys.CTRL_C -> CtrlC
            id == Paste.id -> Paste
            id == Ctrl.id -> Ctrl
            id == Alt.id -> Alt
            id in TerminalKeys.NAMED -> Named(id)
            id.startsWith("char:") -> id.removePrefix("char:").takeIf(TerminalKeys::isChar)?.let(::Char)
            else -> null
        }
    }
}

/** The key bar: 1 to [MAX_ROWS] rows of 1 to [MAX_PER_ROW] keys, shared by every agent. */
data class KeyLayout(val rows: List<List<KeySpec>>) {
    init {
        require(valid(rows)) { "layout must have 1-$MAX_ROWS rows of at most $MAX_PER_ROW keys" }
    }

    val canAddRow: Boolean get() = rows.size < MAX_ROWS
    val canDeleteRow: Boolean get() = rows.size > 1
    fun canAddTo(row: Int): Boolean = rows.getOrNull(row)?.let { it.size < MAX_PER_ROW } == true

    /** A new empty row on top, or this layout when full. */
    fun addRow(): KeyLayout = if (canAddRow) KeyLayout(listOf(emptyList<KeySpec>()) + rows) else this

    fun deleteRow(row: Int): KeyLayout =
        if (canDeleteRow && row in rows.indices) KeyLayout(rows.filterIndexed { i, _ -> i != row }) else this

    /** Appends [key] to [row], or this layout when the row is full. */
    fun add(row: Int, key: KeySpec): KeyLayout = if (canAddTo(row)) edit(row) { it + key } else this

    fun replace(row: Int, index: Int, key: KeySpec): KeyLayout =
        edit(row) { keys -> keys.mapIndexed { i, k -> if (i == index) key else k } }

    fun remove(row: Int, index: Int): KeyLayout = edit(row) { keys -> keys.filterIndexed { i, _ -> i != index } }

    /** Moves a key to [toIndex] of [toRow]; refused when that row is full. */
    fun move(fromRow: Int, fromIndex: Int, toRow: Int, toIndex: Int): KeyLayout {
        val key = rows.getOrNull(fromRow)?.getOrNull(fromIndex) ?: return this
        if (toRow !in rows.indices) return this
        if (toRow != fromRow && !canAddTo(toRow)) return this
        val without = rows.mapIndexed { i, r -> if (i == fromRow) r.filterIndexed { j, _ -> j != fromIndex } else r }
        val target = without[toRow]
        val at = toIndex.coerceIn(0, target.size)
        return KeyLayout(without.mapIndexed { i, r -> if (i == toRow) r.take(at) + key + r.drop(at) else r })
    }

    fun encode(): String = Json.encodeToString(CODEC, rows.map { row -> row.map { it.id } })

    private fun edit(row: Int, change: (List<KeySpec>) -> List<KeySpec>): KeyLayout =
        if (row !in rows.indices) this else KeyLayout(rows.mapIndexed { i, r -> if (i == row) change(r) else r })

    companion object {
        const val MAX_PER_ROW = 7
        const val MAX_ROWS = 4
        private val CODEC = ListSerializer(ListSerializer(String.serializer()))

        fun valid(rows: List<List<KeySpec>>): Boolean = rows.size in 1..MAX_ROWS && rows.all { it.size <= MAX_PER_ROW }

        val DEFAULT = KeyLayout(
            listOf(
                listOf(
                    KeySpec.Named(TerminalKeys.ESCAPE),
                    KeySpec.Named(TerminalKeys.TAB),
                    KeySpec.Named(TerminalKeys.BACK_TAB),
                    KeySpec.Named(TerminalKeys.UP),
                    KeySpec.CtrlC,
                    KeySpec.Paste,
                    KeySpec.Named(TerminalKeys.BACKSPACE),
                ),
                listOf(
                    KeySpec.Ctrl,
                    KeySpec.Alt,
                    KeySpec.Named(TerminalKeys.LEFT),
                    KeySpec.Named(TerminalKeys.DOWN),
                    KeySpec.Named(TerminalKeys.RIGHT),
                    KeySpec.Named(TerminalKeys.SPACE),
                    KeySpec.Named(TerminalKeys.ENTER),
                ),
            ),
        )

        /** The stored layout, or [DEFAULT] when it is missing or cannot be read. */
        fun decode(text: String?): KeyLayout {
            if (text == null) return DEFAULT
            val ids = runCatching { Json.decodeFromString(CODEC, text) }.getOrNull() ?: return DEFAULT
            val rows = ids.map { row -> row.map { KeySpec.parse(it) ?: return DEFAULT } }
            return if (valid(rows)) KeyLayout(rows) else DEFAULT
        }
    }
}
