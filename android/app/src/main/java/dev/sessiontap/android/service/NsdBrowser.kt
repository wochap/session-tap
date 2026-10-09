package dev.sessiontap.android.service

import android.annotation.SuppressLint
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import dev.sessiontap.android.net.Browser
import dev.sessiontap.android.net.SERVICE_TYPE
import kotlinx.coroutines.awaitCancellation
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import java.net.InetAddress
import java.util.concurrent.Executor
import kotlin.coroutines.resume

/** Whether the default network has Wi-Fi or Ethernet transport. */
fun onLocalNetwork(connectivity: ConnectivityManager): Boolean {
    val caps = connectivity.activeNetwork?.let(connectivity::getNetworkCapabilities) ?: return false
    return caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) || caps.hasTransport(NetworkCapabilities.TRANSPORT_ETHERNET)
}

/**
 * [Browser] over `NsdManager`. API 33+ browses on the default network; API
 * 34+ resolves through service info callbacks, which report every host
 * address; API 31–32 resolves one service at a time, because concurrent
 * resolves fail with `FAILURE_ALREADY_ACTIVE`. The system does the mDNS work,
 * so no multicast lock or location permission is needed.
 */
class NsdBrowser(private val nsd: NsdManager, private val connectivity: ConnectivityManager) : Browser {
    private val direct = Executor { it.run() }

    @SuppressLint("NewApi")
    override suspend fun browse(found: (InetAddress, Int) -> Unit) = coroutineScope {
        val services = Channel<NsdServiceInfo>(Channel.UNLIMITED)
        val discovery = object : NsdManager.DiscoveryListener {
            override fun onServiceFound(info: NsdServiceInfo) { services.trySend(info) }
            override fun onServiceLost(info: NsdServiceInfo) {}
            override fun onDiscoveryStarted(serviceType: String) {}
            override fun onDiscoveryStopped(serviceType: String) { services.close() }
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) { services.close() }
            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}
        }
        val network = connectivity.activeNetwork
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU && network != null) {
            nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, network, direct, discovery)
        } else {
            nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, discovery)
        }
        val callbacks = java.util.Collections.synchronizedList(mutableListOf<NsdManager.ServiceInfoCallback>())
        try {
            launch {
                for (service in services) {
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                        callbacks += watch(service, found)
                    } else {
                        resolve(service)?.let { info -> info.host?.let { found(it, info.port) } }
                    }
                }
            }
            awaitCancellation()
        } finally {
            runCatching { nsd.stopServiceDiscovery(discovery) }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
                callbacks.forEach { runCatching { nsd.unregisterServiceInfoCallback(it) } }
            }
        }
    }

    @SuppressLint("NewApi")
    private fun watch(service: NsdServiceInfo, found: (InetAddress, Int) -> Unit): NsdManager.ServiceInfoCallback {
        val callback = object : NsdManager.ServiceInfoCallback {
            override fun onServiceInfoCallbackRegistrationFailed(errorCode: Int) {}
            override fun onServiceUpdated(info: NsdServiceInfo) = info.hostAddresses.forEach { found(it, info.port) }
            override fun onServiceLost() {}
            override fun onServiceInfoCallbackUnregistered() {}
        }
        nsd.registerServiceInfoCallback(service, direct, callback)
        return callback
    }

    /** Serialized by the caller's loop: one resolve at a time. */
    @Suppress("DEPRECATION")
    private suspend fun resolve(service: NsdServiceInfo): NsdServiceInfo? = suspendCancellableCoroutine { cont ->
        nsd.resolveService(
            service,
            object : NsdManager.ResolveListener {
                override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) { if (cont.isActive) cont.resume(null) }
                override fun onServiceResolved(info: NsdServiceInfo) { if (cont.isActive) cont.resume(info) }
            },
        )
    }
}
