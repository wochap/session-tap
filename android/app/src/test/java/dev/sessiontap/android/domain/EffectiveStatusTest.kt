package dev.sessiontap.android.domain

import dev.sessiontap.android.Fixtures
import dev.sessiontap.android.Fixtures.child
import dev.sessiontap.android.Fixtures.view
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.Status
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class EffectiveStatusTest {
    @Test
    fun rootOnly() {
        Status.entries.forEach { assertEquals(it, effectiveStatus(view(it))) }
    }

    @Test
    fun blockedChildWins() {
        assertEquals(Status.Blocked, effectiveStatus(view(Status.Running, children = listOf(child(Status.Blocked), child(Status.Running, type = "t")))))
        assertEquals(Status.Blocked, effectiveStatus(view(Status.Stopped, children = listOf(child(Status.Blocked)))))
    }

    @Test
    fun runningChildKeepsStoppedRootRunning() {
        assertEquals(Status.Running, effectiveStatus(view(Status.Stopped, Fixtures.completed(), listOf(child(Status.Running)))))
        assertEquals(Status.Running, effectiveStatus(view(Status.Idle, children = listOf(child(Status.Running)))))
    }

    @Test
    fun blockedRootBeatsRunningChild() {
        assertEquals(Status.Blocked, effectiveStatus(view(Status.Blocked, children = listOf(child(Status.Running)))))
    }

    @Test
    fun finishedChildrenFallBackToRoot() {
        assertEquals(Status.Idle, effectiveStatus(view(Status.Idle, children = listOf(child(Status.Stopped)))))
    }

    @Test
    fun staleAfter24Hours() {
        assertTrue(isStale(view(Status.Idle, updatedAt = Fixtures.NOW.minusSeconds(30 * 3600)), Fixtures.NOW))
        assertFalse(isStale(view(Status.Idle, updatedAt = Fixtures.NOW.minusSeconds(23 * 3600)), Fixtures.NOW))
        assertTrue(isStale(view(Status.Blocked, updatedAt = Fixtures.NOW.minusSeconds(25 * 3600)), Fixtures.NOW))
    }

    @Test
    fun blockCausePrefersFirstBlockedChild() {
        val v = view(Status.Blocked, Fixtures.input(), listOf(child(Status.Running, type = "a"), child(Status.Blocked, ReasonKind.Approval, "Explore", "find"), child(Status.Blocked, ReasonKind.Input, "b")))
        val cause = blockCause(v)!!
        assertEquals("Explore", cause.child?.agentType)
        assertEquals(ReasonKind.Approval, cause.kind)
        assertEquals(ReasonKind.Input, blockCause(view(Status.Blocked, Fixtures.input()))!!.kind)
        assertNull(blockCause(view(Status.Running)))
    }
}
