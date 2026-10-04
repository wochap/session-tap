package dev.sessiontap.android.data

import dev.sessiontap.android.domain.AgentNotice
import dev.sessiontap.android.domain.NotificationRules
import dev.sessiontap.android.domain.NotifyDecision
import dev.sessiontap.android.domain.effectiveStatus
import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.net.HubEnvelope
import dev.sessiontap.android.net.HubInfo
import dev.sessiontap.android.net.ProtocolJson
import dev.sessiontap.android.net.Status
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

data class AgentKey(val hubId: String, val sourceId: String, val invocationId: String) {
    /** Stable notification id for this agent. */
    val notificationId: Int get() = "$hubId|$sourceId|$invocationId".hashCode()
}

data class AgentItem(val key: AgentKey, val view: AgentView, val effective: Status)

/** Posts and cancels agent notifications. */
interface AgentNotifier {
    fun post(hub: HubEntity, key: AgentKey, notice: AgentNotice)
    fun cancel(key: AgentKey)
    fun cancelHub(hubId: String)
}

/** Live hub operations the UI needs; provided by the running service. */
interface HubOps {
    suspend fun forget(key: AgentKey)
    fun reconnectAll()
}

/**
 * Process-wide session state. Snapshots replace a hub's agents, updates upsert
 * one agent, and both pass the persisted effective status to [NotificationRules].
 */
class HubRepository(
    private val dao: HubDao,
    private val settings: SettingsStore,
    private val notifier: AgentNotifier,
    scope: CoroutineScope,
    private val now: () -> Long = System::currentTimeMillis,
) {
    private val mutex = Mutex()

    /** Null until the database has been read once. */
    val hubs: StateFlow<List<HubEntity>?> = dao.hubs().stateIn(scope, SharingStarted.Eagerly, null)

    val agents: StateFlow<List<AgentItem>> = dao.agents()
        .map { rows -> rows.mapNotNull { it.toItem() } }
        .stateIn(scope, SharingStarted.Eagerly, emptyList())

    private val _conn = MutableStateFlow<Map<String, ConnState>>(emptyMap())
    val connStates: StateFlow<Map<String, ConnState>> = _conn

    /** Agents hidden while a forget is pending its undo window. */
    private val _hidden = MutableStateFlow<Set<AgentKey>>(emptySet())
    val hidden: StateFlow<Set<AgentKey>> = _hidden

    @Volatile var ops: HubOps? = null

    fun setConnState(hubId: String, state: ConnState) = _conn.update { it + (hubId to state) }

    suspend fun applyEnvelope(hubId: String, envelope: HubEnvelope) = mutex.withLock {
        val hub = dao.hub(hubId) ?: return@withLock
        val alerts = settings.current()
        val muted = settings.isMuted(hubId, now())
        val decisions = mutableListOf<Pair<AgentKey, NotifyDecision>>()
        when (envelope) {
            is HubEnvelope.Snapshot -> {
                val old = dao.agentsForHub(hubId).associateBy { AgentKey(hubId, it.sourceId, it.invocationId) }
                val rows = envelope.agents.map { entry ->
                    val key = AgentKey(hubId, entry.sourceId, entry.view.invocationId)
                    val prev = old[key]?.effective?.let(::statusOf)
                    decisions += key to NotificationRules.evaluate(prev, entry.view, hub.name, alerts, muted)
                    entry.view.toEntity(key)
                }
                val present = rows.map { AgentKey(hubId, it.sourceId, it.invocationId) }.toSet()
                old.forEach { (key, row) ->
                    if (key !in present && statusOf(row.effective) == Status.Blocked) decisions += key to NotifyDecision.Cancel
                }
                dao.replaceAgents(hubId, rows)
                val sources = envelope.sources.associate { it.sourceId to (it.displayName ?: it.sourceId) }
                dao.upsertHub(hub.copy(hubRevision = envelope.hubRevision, lastSyncAt = now(), lastSeenAt = now(), sources = sources))
                _hidden.update { hidden -> hidden.filter { it.hubId != hubId || it in present }.toSet() }
            }
            is HubEnvelope.Update -> {
                val key = AgentKey(hubId, envelope.sourceId, envelope.view.invocationId)
                val prev = dao.agent(hubId, key.sourceId, key.invocationId)?.effective?.let(::statusOf)
                decisions += key to NotificationRules.evaluate(prev, envelope.view, hub.name, alerts, muted)
                dao.upsertAgents(listOf(envelope.view.toEntity(key)))
                dao.upsertHub(hub.copy(hubRevision = envelope.hubRevision, lastSyncAt = now(), lastSeenAt = now()))
            }
        }
        decisions.forEach { (key, decision) ->
            when (decision) {
                is NotifyDecision.Post -> notifier.post(hub, key, decision.notice)
                NotifyDecision.Cancel -> notifier.cancel(key)
                NotifyDecision.None -> {}
            }
        }
    }

    /** Stores a paired hub; re-pairing replaces endpoints and scopes of the same hub id. */
    suspend fun savePaired(hubId: String, name: String, endpoints: List<String>, scopes: List<String>, endpoint: String) = mutex.withLock {
        val existing = dao.hub(hubId)
        val hub = existing?.copy(name = name, endpoints = endpoints, scopes = scopes, lastGoodEndpoint = endpoint, revoked = false, lastSeenAt = now())
            ?: HubEntity(hubId, name, endpoints, scopes, endpoint, pairedAt = now(), lastSeenAt = now())
        dao.upsertHub(hub)
    }

    suspend fun onConnected(hubId: String, endpoint: String, info: HubInfo) = mutex.withLock {
        val hub = dao.hub(hubId) ?: return@withLock
        dao.upsertHub(hub.copy(name = info.hubName.ifEmpty { hub.name }, scopes = info.scopes, lastGoodEndpoint = endpoint, lastSeenAt = now(), revoked = false))
    }

    suspend fun markRevoked(hubId: String) = mutex.withLock {
        val hub = dao.hub(hubId) ?: return@withLock
        dao.upsertHub(hub.copy(revoked = true))
    }

    /** Deletes the hub, its pinned identity, agents, notifications, and mute. */
    suspend fun unpair(hubId: String) = mutex.withLock {
        dao.deleteHub(hubId)
        notifier.cancelHub(hubId)
        settings.unmute(hubId)
        _conn.update { it - hubId }
        _hidden.update { hidden -> hidden.filter { it.hubId != hubId }.toSet() }
    }

    fun reconnectAll() {
        ops?.reconnectAll()
    }

    fun hide(key: AgentKey) = _hidden.update { it + key }
    fun unhide(key: AgentKey) = _hidden.update { it - key }

    /** Sends `forget`; on success the agent is dropped locally until the next snapshot confirms it. */
    suspend fun forget(key: AgentKey) {
        val ops = ops ?: throw IllegalStateException("not connected")
        ops.forget(key)
        mutex.withLock { dao.deleteAgent(key.hubId, key.sourceId, key.invocationId) }
        unhide(key)
    }

    private fun AgentView.toEntity(key: AgentKey) = AgentEntity(
        hubId = key.hubId,
        sourceId = key.sourceId,
        invocationId = key.invocationId,
        viewJson = ProtocolJson.encodeToString(AgentView.serializer(), this),
        effective = effectiveStatus(this).name,
        updatedAt = updatedAt,
    )

    private fun AgentEntity.toItem(): AgentItem? {
        val view = runCatching { ProtocolJson.decodeFromString(AgentView.serializer(), viewJson) }.getOrNull() ?: return null
        return AgentItem(AgentKey(hubId, sourceId, invocationId), view, effectiveStatus(view))
    }

    private fun statusOf(name: String): Status? = Status.entries.firstOrNull { it.name == name }
}
