package dev.sessiontap.android.domain

import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.Status

data class AlertSettings(
    val permission: Boolean = true,
    val input: Boolean = true,
    val finished: Boolean = true,
)

enum class NotifyChannel { Attention, Completed }

/** Everything needed to render one agent notification. */
data class AgentNotice(
    val channel: NotifyChannel,
    val title: String,
    val event: String,
    val text: String,
    val body: String,
    val footer: String,
    val publicTitle: String,
    /** Offer "Open terminal": needs attention, a live terminal, and the `control` scope. */
    val openTerminal: Boolean = false,
)

sealed interface NotifyDecision {
    data class Post(val notice: AgentNotice) : NotifyDecision
    data object Cancel : NotifyDecision
    data object None : NotifyDecision
}

object NotificationRules {
    /**
     * Decides what a change of effective status means for the agent's notification.
     * [prev] is the persisted effective status, or null when the agent is new.
     */
    fun evaluate(
        prev: Status?,
        view: AgentView,
        hubName: String,
        settings: AlertSettings,
        muted: Boolean,
        canControl: Boolean = false,
    ): NotifyDecision {
        val next = effectiveStatus(view)
        if (prev == next) return NotifyDecision.None
        // Leaving blocked cancels, unless the same change also posts (which replaces it).
        val fallback = if (prev == Status.Blocked) NotifyDecision.Cancel else NotifyDecision.None
        val provider = providerLabel(view.provider).name
        val (channel, event, summary) = when {
            next == Status.Blocked -> {
                val cause = blockCause(view)
                val kind = cause?.kind
                val allowed = when (kind) {
                    ReasonKind.Approval -> settings.permission
                    ReasonKind.Input -> settings.input
                    else -> settings.permission || settings.input
                }
                if (!allowed) return fallback
                val event = when (kind) {
                    ReasonKind.Approval -> "$provider needs your permission"
                    ReasonKind.Input -> "$provider needs your input"
                    else -> "$provider needs your attention"
                }
                val summary = listOfNotNull(
                    cause?.child?.let { "${it.agentType} subagent" },
                    cause?.summary?.takeIf { it.isNotBlank() },
                ).joinToString(" · ")
                Triple(NotifyChannel.Attention, event, summary)
            }
            next == Status.Stopped && view.reason?.kind == ReasonKind.Completed -> {
                if (!settings.finished) return fallback
                Triple(NotifyChannel.Completed, "$provider finished", view.reason.summary)
            }
            else -> return fallback
        }
        if (muted) return fallback
        val text = listOf(hubName, event, location(view)).filter { it.isNotEmpty() }.joinToString(" · ")
        return NotifyDecision.Post(
            AgentNotice(
                channel = channel,
                title = sessionTitle(view),
                event = event,
                text = text,
                body = summary,
                footer = usageFooter(view.usage),
                publicTitle = event,
                openTerminal = channel == NotifyChannel.Attention && canControl && view.terminal != null,
            ),
        )
    }
}
