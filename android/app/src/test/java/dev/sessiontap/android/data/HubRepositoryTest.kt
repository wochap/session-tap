package dev.sessiontap.android.data

import androidx.datastore.preferences.core.PreferenceDataStoreFactory
import androidx.test.core.app.ApplicationProvider
import dev.sessiontap.android.Fixtures
import dev.sessiontap.android.Fixtures.child
import dev.sessiontap.android.Fixtures.view
import dev.sessiontap.android.domain.AgentNotice
import dev.sessiontap.android.net.AgentEntry
import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.HubEnvelope
import dev.sessiontap.android.net.HubInfo
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.Status
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35], application = android.app.Application::class)
class HubRepositoryTest {
    @get:Rule
    val tmp = TemporaryFolder()

    private class FakeNotifier : AgentNotifier {
        val events = mutableListOf<String>()
        override fun post(hub: HubEntity, key: AgentKey, notice: AgentNotice) { events += "post ${key.invocationId} ${notice.event}" }
        override fun cancel(key: AgentKey) { events += "cancel ${key.invocationId}" }
        override fun cancelHub(hubId: String) { events += "cancelHub $hubId" }
    }

    private val hubId = "h".repeat(64)
    private lateinit var db: SessionTapDb
    private lateinit var settings: SettingsStore
    private lateinit var notifier: FakeNotifier
    private lateinit var scope: CoroutineScope
    private lateinit var repo: HubRepository
    private var now = Fixtures.NOW.toEpochMilli()

    @Before
    fun setUp() = runBlocking {
        db = SessionTapDb.inMemory(ApplicationProvider.getApplicationContext())
        scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
        settings = SettingsStore(PreferenceDataStoreFactory.create(scope = scope) { tmp.newFile("alerts.preferences_pb") })
        notifier = FakeNotifier()
        repo = newRepo()
        repo.savePaired(hubId, "MacBook", listOf("10.0.2.2:8932"), listOf("read", "manage"), "10.0.2.2:8932")
    }

    private fun newRepo() = HubRepository(db.hubs(), settings, notifier, scope) { now }

    @After
    fun tearDown() {
        scope.cancel()
        db.close()
    }

    private fun snapshot(vararg views: AgentView) =
        HubEnvelope.Snapshot(1, emptyList(), views.map { AgentEntry("host", it) })

    private fun update(v: AgentView) = HubEnvelope.Update(2, "host", "d", 1, listOf("status"), v)

    private suspend fun stored() = db.hubs().agentsForHub(hubId).associate { it.invocationId to it.effective }

    @Test
    fun snapshotReplacesAgentSet() = runBlocking {
        repo.applyEnvelope(hubId, snapshot(view(Status.Running, id = "a"), view(Status.Idle, id = "b")))
        assertEquals(mapOf("a" to "Running", "b" to "Idle"), stored())
        repo.applyEnvelope(hubId, snapshot(view(Status.Idle, id = "b"), view(Status.Running, id = "c")))
        assertEquals(mapOf("b" to "Idle", "c" to "Running"), stored())
        assertEquals(emptyList<String>(), notifier.events)
    }

    @Test
    fun updateDiffsAgainstPersistedEffectiveStatus() = runBlocking {
        repo.applyEnvelope(hubId, snapshot(view(Status.Running, id = "a")))
        repo.applyEnvelope(hubId, update(view(Status.Blocked, Fixtures.approval(), id = "a")))
        repo.applyEnvelope(hubId, update(view(Status.Blocked, Fixtures.approval("other"), id = "a")))
        repo.applyEnvelope(hubId, update(view(Status.Running, id = "a")))
        repo.applyEnvelope(hubId, update(view(Status.Running, children = listOf(child(Status.Blocked, ReasonKind.Input)), id = "a")))
        repo.applyEnvelope(hubId, update(view(Status.Stopped, Fixtures.completed(), id = "a")))
        assertEquals(
            listOf(
                "post a Claude Code needs your permission",
                "cancel a",
                "post a Claude Code needs your input",
                "post a Claude Code finished",
            ),
            notifier.events,
        )
        assertEquals(mapOf("a" to "Stopped"), stored())
    }

    @Test
    fun blockWhileDisconnectedNotifiesOnceFromSnapshot() = runBlocking {
        repo.applyEnvelope(hubId, snapshot(view(Status.Running, id = "a")))
        repo.applyEnvelope(hubId, snapshot(view(Status.Blocked, Fixtures.approval(), id = "a")))
        repo.applyEnvelope(hubId, snapshot(view(Status.Blocked, Fixtures.approval(), id = "a")))
        assertEquals(listOf("post a Claude Code needs your permission"), notifier.events)
    }

    @Test
    fun restartWithAgentStillBlockedDoesNotRenotify() = runBlocking {
        repo.applyEnvelope(hubId, snapshot(view(Status.Blocked, Fixtures.input(), id = "a")))
        notifier.events.clear()
        repo = newRepo()
        repo.applyEnvelope(hubId, snapshot(view(Status.Blocked, Fixtures.input(), id = "a")))
        assertEquals(emptyList<String>(), notifier.events)
    }

    @Test
    fun blockedAgentMissingFromSnapshotIsCancelled() = runBlocking {
        repo.applyEnvelope(hubId, snapshot(view(Status.Blocked, Fixtures.input(), id = "a")))
        notifier.events.clear()
        repo.applyEnvelope(hubId, snapshot())
        assertEquals(listOf("cancel a"), notifier.events)
        assertTrue(stored().isEmpty())
    }

    @Test
    fun mutedHubStillUpdatesState() = runBlocking {
        settings.mute(hubId, now + 3_600_000)
        repo.applyEnvelope(hubId, snapshot(view(Status.Running, id = "a")))
        repo.applyEnvelope(hubId, update(view(Status.Blocked, Fixtures.approval(), id = "a")))
        assertEquals(emptyList<String>(), notifier.events)
        assertEquals(mapOf("a" to "Blocked"), stored())
        now += 3_600_001
        repo.applyEnvelope(hubId, update(view(Status.Running, id = "a")))
        repo.applyEnvelope(hubId, update(view(Status.Blocked, Fixtures.approval(), id = "a")))
        assertEquals(listOf("cancel a", "post a Claude Code needs your permission"), notifier.events)
    }

    @Test
    fun finishedToggleOff() = runBlocking {
        settings.setFinished(false)
        repo.applyEnvelope(hubId, snapshot(view(Status.Running, id = "a")))
        repo.applyEnvelope(hubId, update(view(Status.Stopped, Fixtures.completed(), id = "a")))
        assertEquals(emptyList<String>(), notifier.events)
    }

    @Test
    fun repairMergesEndpointsAndUnpairDeletesEverything() = runBlocking {
        repo.applyEnvelope(hubId, snapshot(view(Status.Running, id = "a")))
        val pairedAt = db.hubs().hub(hubId)!!.pairedAt
        now += 1_000
        repo.savePaired(hubId, "MacBook", listOf("192.168.1.37:8932", "100.64.0.7:8932"), listOf("read"), "100.64.0.7:8932")
        val hubs = db.hubs().allHubs()
        assertEquals(1, hubs.size)
        assertEquals(listOf("100.64.0.7:8932", "192.168.1.37:8932", "10.0.2.2:8932"), hubs[0].endpoints)
        assertEquals(listOf("read"), hubs[0].scopes)
        assertEquals(pairedAt, hubs[0].pairedAt)
        assertEquals(setOf("a"), stored().keys)
        repo.unpair(hubId)
        assertTrue(db.hubs().allHubs().isEmpty())
        assertTrue(stored().isEmpty())
        assertEquals(listOf("cancelHub $hubId"), notifier.events)
    }

    @Test
    fun differentHubIdIsANewHub() = runBlocking {
        val before = db.hubs().hub(hubId)!!
        val other = "b".repeat(64)
        repo.savePaired(other, "MacBook", listOf("10.0.2.2:8932"), listOf("read"), "10.0.2.2:8932")
        assertEquals(2, db.hubs().allHubs().size)
        assertEquals(before, db.hubs().hub(hubId))
        assertEquals(listOf("10.0.2.2:8932"), db.hubs().hub(other)!!.endpoints)
    }

    @Test
    fun onConnectedMergesReportedEndpoints() = runBlocking {
        repo.savePaired(hubId, "MacBook", listOf("192.168.1.20:8932", "macbook.tailnet.ts.net:8932"), listOf("read"), "192.168.1.20:8932")
        db.hubs().upsertHub(db.hubs().hub(hubId)!!.copy(endpoints = listOf("192.168.1.20:8932", "macbook.tailnet.ts.net:8932")))
        val info = HubInfo(hubId, "MacBook", 1, listOf("read"), listOf("192.168.1.37:8932", "macbook.tailnet.ts.net:8932", "", "bad"))
        repo.onConnected(hubId, "macbook.tailnet.ts.net:8932", info)
        assertEquals(
            listOf("macbook.tailnet.ts.net:8932", "192.168.1.37:8932", "192.168.1.20:8932"),
            db.hubs().hub(hubId)!!.endpoints,
        )

        val many = (1..10).map { "10.1.0.$it:8932" }
        repo.onConnected(hubId, "macbook.tailnet.ts.net:8932", info.copy(endpoints = many))
        assertEquals(listOf("macbook.tailnet.ts.net:8932") + many.take(7), db.hubs().hub(hubId)!!.endpoints)
    }

    @Test
    fun collapseStateRoundTripsAndClears() = runBlocking {
        assertEquals(emptyMap<String, Boolean>(), settings.collapsed.first())
        settings.setCollapsed("stale", false)
        settings.setCollapsed("h_x", true)
        assertEquals(mapOf("stale" to false, "h_x" to true), settings.collapsed.first())
        settings.clearCollapsed("h_x")
        assertEquals(mapOf("stale" to false), settings.collapsed.first())
    }

    @Test
    fun unpairClearsOnlyThatHubsCollapseState() = runBlocking {
        settings.setCollapsed("h_$hubId", true)
        settings.setCollapsed("h_other", true)
        settings.setCollapsed("attn", true)
        repo.unpair(hubId)
        assertEquals(mapOf("h_other" to true, "attn" to true), settings.collapsed.first())
    }
}
