package dev.sessiontap.android.ui

import dev.sessiontap.android.Fixtures
import dev.sessiontap.android.Fixtures.child
import dev.sessiontap.android.Fixtures.view
import dev.sessiontap.android.data.AgentItem
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.domain.effectiveStatus
import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.Status
import dev.sessiontap.android.ui.sessions.FeedInput
import dev.sessiontap.android.ui.sessions.FeedItem
import dev.sessiontap.android.ui.sessions.Filter
import dev.sessiontap.android.ui.sessions.buildFeed
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class FeedTest {
    private val mac = HubEntity("m".repeat(64), "MacBook", listOf("1.2.3.4:8932"), listOf("read", "manage"), pairedAt = 1)
    private val desk = HubEntity("d".repeat(64), "Desktop", listOf("1.2.3.5:8932"), listOf("read"), pairedAt = 2)

    private fun item(hub: HubEntity, v: AgentView, source: String = "host") = AgentItem(AgentKey(hub.hubId, source, v.invocationId), v, effectiveStatus(v))

    private fun feed(hubs: List<HubEntity>, agents: List<AgentItem>, filter: Filter = Filter.All, conn: Map<String, ConnState>? = null) = buildFeed(
        FeedInput(hubs, conn ?: hubs.associate { it.hubId to ConnState.Live("x") }, agents, emptySet(), null, filter, emptyMap(), emptySet(), Fixtures.NOW),
    )

    @Test
    fun singleHubHasNoHubSections() {
        val f = feed(listOf(mac), listOf(item(mac, view(Status.Running, id = "a"))))
        assertTrue(f.none { it is FeedItem.Section })
        assertEquals(1, f.count { it is FeedItem.Row })
    }

    @Test
    fun multiHubSectionsAndAttention() {
        val blocked = item(mac, view(Status.Running, children = listOf(child(Status.Blocked, ReasonKind.Approval)), id = "a"))
        val f = feed(listOf(mac, desk), listOf(blocked, item(desk, view(Status.Idle, id = "b"))))
        val sections = f.filterIsInstance<FeedItem.Section>().map { it.title }
        assertEquals(listOf("Needs attention", "MacBook", "Desktop"), sections)
        val row = f.filterIsInstance<FeedItem.Row>().first().row
        assertEquals("Explore subagent needs approval", row.reason)
        assertEquals("MacBook", row.hubTag)
    }

    @Test
    fun staleIsCollapsedAndGrouped() {
        val old = item(mac, view(Status.Idle, id = "s", updatedAt = Fixtures.NOW.minusSeconds(30 * 3600)))
        val f = feed(listOf(mac), listOf(old, item(mac, view(Status.Running, id = "a"))))
        val stale = f.filterIsInstance<FeedItem.Section>().single()
        assertEquals("Stale", stale.title)
        assertTrue(stale.collapsed)
        assertEquals(1, f.count { it is FeedItem.Row })
        val opened = feed(listOf(mac), listOf(old), Filter.Stale)
        assertTrue(opened.filterIsInstance<FeedItem.Row>().single().row.stale)
    }

    @Test
    fun emptyFilterState() {
        val f = feed(listOf(mac), listOf(item(mac, view(Status.Running, id = "a"))), Filter.Stale)
        assertEquals("No stale sessions", f.filterIsInstance<FeedItem.Empty>().single().title)
    }

    @Test
    fun swipeOnlyForStoppedWithManage() {
        val f = feed(listOf(mac, desk), listOf(item(mac, view(Status.Stopped, id = "a")), item(desk, view(Status.Stopped, id = "b")), item(mac, view(Status.Idle, id = "c"))))
        val rows = f.filterIsInstance<FeedItem.Row>().map { it.row }.associateBy { it.key.invocationId }
        assertTrue(rows["a"]!!.swipeable)
        assertTrue(!rows["b"]!!.swipeable)
        assertTrue(!rows["c"]!!.swipeable)
    }

    @Test
    fun offlineSingleHubShowsErrorCard() {
        val f = feed(listOf(mac), listOf(item(mac, view(Status.Running, id = "a"))), conn = mapOf(mac.hubId to ConnState.Reconnecting(0, 5, "timeout")))
        assertEquals("Can't reach MacBook", (f.first() as FeedItem.Error).title)
    }
}
