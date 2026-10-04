package dev.sessiontap.android

import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.ChildReason
import dev.sessiontap.android.net.ChildView
import dev.sessiontap.android.net.Reason
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.Repository
import dev.sessiontap.android.net.Status
import dev.sessiontap.android.net.Usage
import java.time.Instant

object Fixtures {
    val NOW: Instant = Instant.parse("2026-10-03T14:32:00Z")

    fun view(
        status: Status,
        reason: Reason? = null,
        children: List<ChildView>? = null,
        updatedAt: Instant = NOW.minusSeconds(60),
        provider: String = "claude",
        id: String = "7f3c2a1e-0000-4000-8000-000000000001",
        sessionName: String? = "Fix flaky auth tests",
    ) = AgentView(
        invocationId = id,
        provider = provider,
        status = status,
        reason = reason,
        cwd = "/home/me/code/api",
        createdAt = NOW.minusSeconds(600).toString(),
        updatedAt = updatedAt.toString(),
        session = sessionName?.let { dev.sessiontap.android.net.ProviderSession("sess-1", it) },
        usage = Usage(inputTokens = 15_300, outputTokens = 2_100, contextWindowPercent = 42),
        repository = Repository("/home/me/code/api", "feat/auth-retry", "a41f9c2d", true),
        children = children,
    )

    fun child(status: Status, kind: ReasonKind? = null, type: String = "Explore", summary: String? = null) = ChildView(
        agentId = "child-$type",
        agentType = type,
        status = status,
        reason = if (kind != null || summary != null) ChildReason(kind, summary) else null,
        startedAt = NOW.minusSeconds(120).toString(),
        updatedAt = NOW.minusSeconds(30).toString(),
    )

    fun approval(summary: String = "Bash · rm -rf build") = Reason(ReasonKind.Approval, summary)
    fun input(summary: String = "Which version tag?") = Reason(ReasonKind.Input, summary)
    fun completed(summary: String = "Done") = Reason(ReasonKind.Completed, summary)
}
