package dev.sessiontap.android.domain

import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.QuickPick

/** What this device may do with an agent's live terminal. */
enum class TerminalLevel { None, View, Control }

/**
 * The hub's effective scopes combined with the agent's `terminal` descriptor.
 * [available] is false when the agent has no live terminal (headless, not in a
 * multiplexer, or its process has exited); a finished turn keeps it available.
 * [digits] says whether digit quick-pick chips apply.
 */
data class TerminalAccess(val available: Boolean, val level: TerminalLevel, val digits: Boolean) {
    /** The device can open the terminal at all. */
    val canOpen: Boolean get() = available && level != TerminalLevel.None

    companion object {
        val Unavailable = TerminalAccess(available = false, level = TerminalLevel.None, digits = false)

        fun of(canWatch: Boolean, canControl: Boolean, view: AgentView?): TerminalAccess {
            val descriptor = view?.terminal ?: return Unavailable
            val level = when {
                canControl -> TerminalLevel.Control
                canWatch -> TerminalLevel.View
                else -> TerminalLevel.None
            }
            return TerminalAccess(
                available = true,
                level = level,
                digits = level == TerminalLevel.Control && descriptor.quickPick == QuickPick.Digits,
            )
        }
    }
}
