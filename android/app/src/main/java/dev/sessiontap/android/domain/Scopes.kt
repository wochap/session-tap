package dev.sessiontap.android.domain

/** One scope a hub grants or a pairing requests, ready for display. */
data class ScopeChip(val name: String, val label: String, val warning: Boolean)

/** Hub scope names, their canonical order, and display labels. */
object Scopes {
    const val READ = "read"
    const val MANAGE = "manage"
    const val WATCH = "watch"
    const val CONTROL = "control"

    val ORDER = listOf(READ, MANAGE, WATCH, CONTROL)

    private val LABELS = mapOf(
        READ to "Read",
        MANAGE to "Manage",
        WATCH to "Watch terminal",
        CONTROL to "Control terminal",
    )

    /** Full label; unknown names stay raw. */
    fun label(name: String): String = LABELS[name] ?: name

    /** Control types into agents, so it gets the warning style. */
    fun isWarning(name: String): Boolean = name == CONTROL

    /** Known scopes in canonical order, then unknown names as given, without duplicates. */
    fun ordered(names: List<String>): List<String> {
        val distinct = names.distinct()
        return ORDER.filter { it in distinct } + distinct.filter { it !in ORDER }
    }

    /** Chips in display order; [short] keeps the raw name as label, as on hub cards. */
    fun chips(names: List<String>, short: Boolean = false): List<ScopeChip> =
        ordered(names).map { ScopeChip(it, if (short) it else label(it), isWarning(it)) }
}
