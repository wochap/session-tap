package dev.sessiontap.android.service

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.net.nsd.NsdManager
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import dev.sessiontap.android.MainActivity
import dev.sessiontap.android.R
import dev.sessiontap.android.SessionTapApp
import dev.sessiontap.android.crypto.DeviceKey
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.data.HubOps
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.net.HubClient
import dev.sessiontap.android.net.HubClientListener
import dev.sessiontap.android.net.HubDiscovery
import dev.sessiontap.android.net.HubEnvelope
import dev.sessiontap.android.net.HubInfo
import dev.sessiontap.android.net.HubTls
import dev.sessiontap.android.net.RpcException
import dev.sessiontap.android.net.Status
import dev.sessiontap.android.net.TerminalHub
import dev.sessiontap.android.notify.Channels
import dev.sessiontap.android.notify.NotificationPoster
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filterNotNull
import kotlinx.coroutines.launch
import java.util.concurrent.ConcurrentHashMap

/** Foreground service holding one authenticated connection per paired hub. */
class HubService : Service(), HubClientListener, HubOps {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val clients = ConcurrentHashMap<String, HubClient>()
    private lateinit var app: SessionTapApp
    private lateinit var discovery: HubDiscovery
    private val network = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) = clients.values.forEach { it.kick() }
        override fun onLost(network: Network) = clients.values.forEach { it.kick() }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        app = application as SessionTapApp
        startInForeground(summaryText(0, 0))
        app.repository.ops = this
        val connectivity = getSystemService(ConnectivityManager::class.java)
        discovery = HubDiscovery(
            scope = scope,
            browser = NsdBrowser(getSystemService(NsdManager::class.java), connectivity),
            eligible = { onLocalNetwork(connectivity) },
        )
        connectivity.registerDefaultNetworkCallback(network)
        scope.launch {
            app.repository.hubs.filterNotNull().collectLatest { hubs -> sync(hubs) }
        }
        scope.launch {
            combine(app.repository.hubs.filterNotNull(), app.repository.agents) { hubs, agents ->
                val active = hubs.filter { !it.revoked }.map { it.hubId }.toSet()
                hubs.size to agents.count { it.key.hubId in active && it.effective == Status.Blocked }
            }.distinctUntilChanged().collectLatest { (hubs, attention) ->
                if (hubs > 0) updateSummary(summaryText(hubs, attention))
            }
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startInForeground(summaryText(app.repository.hubs.value?.size ?: 0, 0))
        return START_STICKY
    }

    override fun onDestroy() {
        runCatching { getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(network) }
        clients.values.forEach { it.stop() }
        clients.clear()
        discovery.stop()
        if (app.repository.ops === this) app.repository.ops = null
        scope.cancel()
        super.onDestroy()
    }

    private fun sync(hubs: List<HubEntity>) {
        if (hubs.isEmpty()) {
            stopSelf()
            return
        }
        val wanted = hubs.filter { !it.revoked }.associateBy { it.hubId }
        (clients.keys - wanted.keys).forEach { id -> clients.remove(id)?.stop() }
        hubs.filter { it.revoked }.forEach { app.repository.setConnState(it.hubId, ConnState.Revoked) }
        wanted.values.forEach { hub ->
            if (clients.containsKey(hub.hubId)) return@forEach
            val client = HubClient(
                hubId = hub.hubId,
                endpoints = { app.repository.hubs.value?.firstOrNull { it.hubId == hub.hubId }?.endpoints ?: hub.endpoints },
                lastGood = hub.lastGoodEndpoint,
                listener = this,
                scope = scope,
                clientFor = { HubTls.client(hub.hubId, DeviceKey.keyManager()) },
                // only reconnecting clients ask, so connected hubs start no browse
                discover = discovery::request,
            )
            clients[hub.hubId] = client
            scope.launch { client.state.collectLatest { app.repository.setConnState(hub.hubId, it) } }
            client.start()
        }
    }

    override suspend fun onConnected(hubId: String, endpoint: String, info: HubInfo) =
        app.repository.onConnected(hubId, endpoint, info)

    override suspend fun onEnvelope(hubId: String, envelope: HubEnvelope) =
        app.repository.applyEnvelope(hubId, envelope)

    override suspend fun onRevoked(hubId: String) {
        clients.remove(hubId)
        app.repository.markRevoked(hubId)
        app.repository.setConnState(hubId, ConnState.Revoked)
    }

    override suspend fun forget(key: AgentKey) {
        val client = clients[key.hubId] ?: throw RpcException("offline", "hub is not connected")
        client.forget(key.sourceId, key.invocationId)
    }

    override fun reconnectAll() = clients.values.forEach { it.kick() }

    override fun terminalHub(hubId: String): TerminalHub? = clients[hubId]

    private fun summaryText(hubs: Int, attention: Int): String {
        val watching = "Watching $hubs ${if (hubs == 1) "hub" else "hubs"}"
        return if (attention > 0) "$watching · $attention needs attention" else watching
    }

    private fun buildNotification(text: String): Notification {
        val open = PendingIntent.getActivity(
            this,
            0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(this, Channels.SERVICE)
            .setSmallIcon(R.drawable.ic_stat_caret)
            .setContentTitle(text)
            .setOngoing(true)
            .setSilent(true)
            .setShowWhen(false)
            .setPriority(NotificationCompat.PRIORITY_MIN)
            .setContentIntent(open)
            .build()
    }

    private fun startInForeground(text: String) {
        val type = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE else 0
        ServiceCompat.startForeground(this, NotificationPoster.SERVICE_ID, buildNotification(text), type)
    }

    private fun updateSummary(text: String) {
        startInForeground(text)
    }

    companion object {
        fun start(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, HubService::class.java))
        }
    }
}
