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
import dev.sessiontap.android.net.ReasonKind
import dev.sessiontap.android.net.Status
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
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
    fun repairReplacesEndpointsAndUnpairDeletesEverything() = runBlocking {
        repo.applyEnvelope(hubId, snapshot(view(Status.Running, id = "a")))
        repo.savePaired(hubId, "MacBook", listOf("100.64.0.7:8932"), listOf("read"), "100.64.0.7:8932")
        val hubs = db.hubs().allHubs()
        assertEquals(1, hubs.size)
        assertEquals(listOf("100.64.0.7:8932"), hubs[0].endpoints)
        assertEquals(listOf("read"), hubs[0].scopes)
        repo.unpair(hubId)
        assertTrue(db.hubs().allHubs().isEmpty())
        assertTrue(stored().isEmpty())
        assertEquals(listOf("cancelHub $hubId"), notifier.events)
    }
}
