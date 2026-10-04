package dev.sessiontap.android.domain

import dev.sessiontap.android.Fixtures
import dev.sessiontap.android.Fixtures.child
import dev.sessiontap.android.Fixtures.view
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.Status
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class NotificationRulesTest {
    private val on = AlertSettings()

    private fun eval(prev: Status?, v: dev.sessiontap.android.net.AgentView, settings: AlertSettings = on, muted: Boolean = false) =
        NotificationRules.evaluate(prev, v, "MacBook", settings, muted)

    private fun post(d: NotifyDecision) = (d as NotifyDecision.Post).notice

    @Test
    fun approvalRequested() {
        val n = post(eval(Status.Running, view(Status.Blocked, Fixtures.approval())))
        assertEquals(NotifyChannel.Attention, n.channel)
        assertEquals("Claude Code needs your permission", n.event)
        assertEquals("Fix flaky auth tests", n.title)
        assertEquals("MacBook · Claude Code needs your permission · ~/code/api · feat/auth-retry", n.text)
        assertEquals("Bash · rm -rf build", n.body)
        assertEquals("Context 42% · In 15.3k · Out 2.1k", n.footer)
        assertEquals("Claude Code needs your permission", n.publicTitle)
    }

    @Test
    fun inputAndOtherReasons() {
        assertEquals("Codex needs your input", post(eval(Status.Idle, view(Status.Blocked, Fixtures.input(), provider = "codex"))).event)
        assertEquals("Claude Code needs your attention", post(eval(Status.Idle, view(Status.Blocked))).event)
    }

    @Test
    fun childBlockedUsesChildReason() {
        val n = post(eval(Status.Running, view(Status.Running, children = listOf(child(Status.Blocked, ReasonKind.Approval, summary = "find ~/code")))))
        assertEquals("Claude Code needs your permission", n.event)
        assertEquals("Explore subagent · find ~/code", n.body)
    }

    @Test
    fun titleFallsBackToProvider() {
        assertEquals("Qwen Code", post(eval(Status.Running, view(Status.Blocked, Fixtures.input(), provider = "qwen", sessionName = null))).title)
    }

    @Test
    fun newBlockedAgentNotifies() {
        assertTrue(eval(null, view(Status.Blocked, Fixtures.approval())) is NotifyDecision.Post)
    }

    @Test
    fun leavingBlockedCancels() {
        assertEquals(NotifyDecision.Cancel, eval(Status.Blocked, view(Status.Running)))
        assertEquals(NotifyDecision.Cancel, eval(Status.Blocked, view(Status.Stopped)))
    }

    @Test
    fun blockedStraightToFinishedReplacesTheAlert() {
        assertEquals("Claude Code finished", post(eval(Status.Blocked, view(Status.Stopped, Fixtures.completed()))).event)
        assertEquals(NotifyDecision.Cancel, eval(Status.Blocked, view(Status.Stopped, Fixtures.completed()), on.copy(finished = false)))
    }

    @Test
    fun stillBlockedDoesNotRepost() {
        assertEquals(NotifyDecision.None, eval(Status.Blocked, view(Status.Blocked, Fixtures.approval())))
    }

    @Test
    fun reblockNotifiesAgain() {
        assertEquals(NotifyDecision.Cancel, eval(Status.Blocked, view(Status.Running)))
        assertTrue(eval(Status.Running, view(Status.Blocked, Fixtures.approval())) is NotifyDecision.Post)
    }

    @Test
    fun completedFinish() {
        val n = post(eval(Status.Running, view(Status.Stopped, Fixtures.completed("All tests pass"))))
        assertEquals(NotifyChannel.Completed, n.channel)
        assertEquals("Claude Code finished", n.event)
        assertEquals("All tests pass", n.body)
    }

    @Test
    fun rootStopsWhileChildRuns() {
        assertEquals(NotifyDecision.None, eval(Status.Running, view(Status.Stopped, Fixtures.completed(), listOf(child(Status.Running)))))
        assertTrue(eval(Status.Running, view(Status.Stopped, Fixtures.completed(), listOf(child(Status.Stopped)))) is NotifyDecision.Post)
    }

    @Test
    fun lifecycleOnlyStopIgnored() {
        assertEquals(NotifyDecision.None, eval(Status.Running, view(Status.Stopped)))
        assertEquals(NotifyDecision.None, eval(Status.Running, view(Status.Stopped, dev.sessiontap.android.net.Reason(ReasonKind.Failed, "exit 1"))))
    }

    @Test
    fun alreadyStoppedDoesNotRepeat() {
        assertEquals(NotifyDecision.None, eval(Status.Stopped, view(Status.Stopped, Fixtures.completed())))
    }

    @Test
    fun otherTransitionsAreSilent() {
        assertEquals(NotifyDecision.None, eval(Status.Idle, view(Status.Running)))
        assertEquals(NotifyDecision.None, eval(null, view(Status.Idle)))
    }

    @Test
    fun settingsFilter() {
        assertEquals(NotifyDecision.None, eval(Status.Running, view(Status.Blocked, Fixtures.approval()), on.copy(permission = false)))
        assertTrue(eval(Status.Running, view(Status.Blocked, Fixtures.input()), on.copy(permission = false)) is NotifyDecision.Post)
        assertEquals(NotifyDecision.None, eval(Status.Running, view(Status.Blocked, Fixtures.input()), on.copy(input = false)))
        assertEquals(NotifyDecision.None, eval(Status.Running, view(Status.Stopped, Fixtures.completed()), on.copy(finished = false)))
    }

    @Test
    fun muteSuppressesPostsButStillCancels() {
        assertEquals(NotifyDecision.None, eval(Status.Running, view(Status.Blocked, Fixtures.approval()), muted = true))
        assertEquals(NotifyDecision.Cancel, eval(Status.Blocked, view(Status.Running), muted = true))
    }
}
