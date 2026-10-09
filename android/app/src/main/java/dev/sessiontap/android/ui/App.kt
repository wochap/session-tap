package dev.sessiontap.android.ui

import android.content.Context
import android.content.Intent
import android.net.ConnectivityManager
import android.net.NetworkCapabilities
import android.provider.Settings
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.calculateEndPadding
import androidx.compose.foundation.layout.calculateStartPadding
import androidx.compose.foundation.layout.systemBars
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Badge
import androidx.compose.material3.BadgedBox
import androidx.compose.material3.Icon
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationBarItemDefaults
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Snackbar
import androidx.compose.material3.SnackbarDuration
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SnackbarResult
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation.NavHostController
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.currentBackStackEntryAsState
import androidx.navigation.navDeepLink
import androidx.compose.ui.semantics.stateDescription
import androidx.compose.ui.semantics.semantics
import com.adamglin.PhosphorIcons
import com.adamglin.phosphoricons.Fill
import com.adamglin.phosphoricons.Regular
import com.adamglin.phosphoricons.fill.Bell as BellFill
import com.adamglin.phosphoricons.fill.DesktopTower as DesktopTowerFill
import com.adamglin.phosphoricons.fill.Rows as RowsFill
import com.adamglin.phosphoricons.regular.Bell
import com.adamglin.phosphoricons.regular.DesktopTower
import com.adamglin.phosphoricons.regular.Rows
import dev.sessiontap.android.SessionTapApp
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.notify.notificationsAllowed
import dev.sessiontap.android.service.HubService
import dev.sessiontap.android.ui.alerts.AlertsScreen
import dev.sessiontap.android.ui.detail.DetailScreen
import dev.sessiontap.android.ui.hubs.HubsScreen
import dev.sessiontap.android.ui.onboarding.PermissionsScreen
import dev.sessiontap.android.ui.onboarding.WelcomeScreen
import dev.sessiontap.android.ui.pairing.PairScreen
import dev.sessiontap.android.ui.pairing.PairState
import dev.sessiontap.android.ui.pairing.PairViewModel
import dev.sessiontap.android.ui.pairing.ScanScreen
import dev.sessiontap.android.ui.pairing.openTailscale
import dev.sessiontap.android.ui.sessions.isAttention
import dev.sessiontap.android.ui.terminal.TerminalRoute
import dev.sessiontap.android.ui.keys.KeyEditorRoute
import dev.sessiontap.android.ui.theme.Mono
import dev.sessiontap.android.ui.theme.St
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import java.net.URLDecoder
import java.net.URLEncoder
import java.time.Instant

object Routes {
    const val WELCOME = "welcome"
    const val PERMS = "perms"
    const val SCAN = "scan"
    const val PAIR = "pair"
    const val SESSIONS = "sessions"
    const val HUBS = "hubs"
    const val ALERTS = "alerts"
    const val DETAIL = "detail/{hub}/{source}/{inv}"
    const val TERMINAL = "terminal/{hub}/{source}/{inv}"
    const val KEYS = "keys"

    fun detail(key: AgentKey) = "detail/${enc(key.hubId)}/${enc(key.sourceId)}/${enc(key.invocationId)}"
    fun terminal(key: AgentKey) = "terminal/${enc(key.hubId)}/${enc(key.sourceId)}/${enc(key.invocationId)}"
    private fun enc(v: String) = URLEncoder.encode(v, "UTF-8")
}

const val UNDO_WINDOW_MS = 5_000L

fun vpnActive(context: Context): Boolean {
    val cm = context.getSystemService(ConnectivityManager::class.java)
    return cm.getNetworkCapabilities(cm.activeNetwork)?.hasTransport(NetworkCapabilities.TRANSPORT_VPN) == true
}

@Composable
fun SessionTapRoot(app: SessionTapApp, pairVm: PairViewModel, nav: NavHostController) {
    val c = St.colors
    val context = LocalContext.current
    val repo = app.repository
    val hubsOrNull by repo.hubs.collectAsStateWithLifecycle()
    val agents by repo.agents.collectAsStateWithLifecycle()
    val conn by repo.connStates.collectAsStateWithLifecycle()
    val hidden by repo.hidden.collectAsStateWithLifecycle()
    val alerts by app.settings.settings.collectAsStateWithLifecycle(initialValue = dev.sessiontap.android.domain.AlertSettings())
    val mutes by app.settings.mutes.collectAsStateWithLifecycle(initialValue = emptyMap())
    // null until the store loads, so a collapsed section never renders open first.
    val collapsed by app.settings.collapsed.collectAsStateWithLifecycle(initialValue = null)
    val pairState by pairVm.state.collectAsStateWithLifecycle()
    val now by produceState(Instant.now()) {
        while (true) {
            delay(1_000)
            value = Instant.now()
        }
    }
    var resumes by remember { mutableIntStateOf(0) }
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { resumes++ }
    val notificationsEnabled = remember(resumes) { notificationsAllowed(context) }
    val vpn = remember(resumes, now.epochSecond / 10) { vpnActive(context) }
    val snackbar = remember { SnackbarHostState() }
    val scope = rememberCoroutineScope()

    val hubs = hubsOrNull ?: run {
        Box(Modifier.fillMaxSize().background(c.bg))
        return
    }
    LaunchedEffect(hubs.isNotEmpty()) { if (hubs.isNotEmpty()) HubService.start(context) }

    fun forget(key: AgentKey) {
        val hubName = hubs.firstOrNull { it.hubId == key.hubId }?.name ?: "hub"
        repo.hide(key)
        scope.launch {
            snackbar.currentSnackbarData?.dismiss()
            val timer = launch {
                delay(UNDO_WINDOW_MS)
                snackbar.currentSnackbarData?.dismiss()
            }
            val result = snackbar.showSnackbar("Forgotten on $hubName", actionLabel = "Undo", duration = SnackbarDuration.Indefinite)
            timer.cancel()
            if (result == SnackbarResult.ActionPerformed) {
                repo.unhide(key)
                return@launch
            }
            try {
                repo.forget(key)
            } catch (e: Exception) {
                repo.unhide(key)
                snackbar.showSnackbar("Couldn't forget: ${e.message}", duration = SnackbarDuration.Short)
            }
        }
    }

    val backStack by nav.currentBackStackEntryAsState()
    val route = backStack?.destination?.route
    val tabs = listOf(Routes.SESSIONS, Routes.HUBS, Routes.ALERTS)
    val attention = agents.count { it.key !in hidden && hubs.any { h -> h.hubId == it.key.hubId } && isAttention(it, now) }

    Scaffold(
        containerColor = c.bg,
        contentWindowInsets = WindowInsets(0),
        snackbarHost = {
            SnackbarHost(snackbar) { data ->
                Snackbar(data, containerColor = c.inv, contentColor = c.invText, actionColor = c.invAcc, shape = androidx.compose.foundation.shape.RoundedCornerShape(8.dp))
            }
        },
        bottomBar = {
            if (route in tabs) {
                NavigationBar(containerColor = c.surf, tonalElevation = 0.dp) {
                    listOf(
                        Triple(Routes.SESSIONS, "Sessions", PhosphorIcons.Regular.Rows to PhosphorIcons.Fill.RowsFill),
                        Triple(Routes.HUBS, "Hubs", PhosphorIcons.Regular.DesktopTower to PhosphorIcons.Fill.DesktopTowerFill),
                        Triple(Routes.ALERTS, "Alerts", PhosphorIcons.Regular.Bell to PhosphorIcons.Fill.BellFill),
                    ).forEach { (r, label, icons) ->
                        val selected = route == r
                        NavigationBarItem(
                            selected = selected,
                            onClick = {
                                if (!selected) nav.navigate(r) {
                                    popUpTo(Routes.SESSIONS) { saveState = true }
                                    launchSingleTop = true
                                    restoreState = true
                                }
                            },
                            icon = {
                                BadgedBox(badge = {
                                    if (r == Routes.SESSIONS && attention > 0) {
                                        Badge(containerColor = c.block, contentColor = c.bg, modifier = Modifier.testTag("badge")) {
                                            Text(attention.toString(), fontFamily = Mono, fontSize = 10.sp, fontWeight = FontWeight.SemiBold)
                                        }
                                    }
                                }) { Icon(if (selected) icons.second else icons.first, null, Modifier.size(20.dp)) }
                            },
                            label = { Text(label, fontSize = 12.sp, fontWeight = FontWeight.Medium) },
                            // The icon slot clears its semantics, so the badge count is exposed on the item.
                            modifier = Modifier.testTag("tab:$label").semantics {
                                if (r == Routes.SESSIONS && attention > 0) stateDescription = "$attention need attention"
                            },
                            colors = NavigationBarItemDefaults.colors(
                                selectedIconColor = c.text,
                                selectedTextColor = c.text,
                                unselectedIconColor = c.mute,
                                unselectedTextColor = c.mute,
                                indicatorColor = c.ind,
                            ),
                        )
                    }
                }
            }
        },
    ) { inner ->
        val sys = WindowInsets.systemBars
        val density = androidx.compose.ui.platform.LocalDensity.current
        val layout = androidx.compose.ui.platform.LocalLayoutDirection.current
        val pad = PaddingValues(
            top = with(density) { sys.getTop(this).toDp() },
            bottom = if (route in tabs) inner.calculateBottomPadding() else with(density) { sys.getBottom(this).toDp() },
            start = inner.calculateStartPadding(layout),
            end = inner.calculateEndPadding(layout),
        )
        // Fixed at first composition: pairing the first hub must not reset the graph under the Paired screen.
        val start = remember { if (hubs.isEmpty()) Routes.WELCOME else Routes.SESSIONS }
        NavHost(nav, startDestination = start, modifier = Modifier.background(c.bg)) {
            composable(Routes.WELCOME) { WelcomeScreen(onScan = { nav.navigate(Routes.PERMS) }, contentPadding = pad) }
            composable(Routes.PERMS) {
                PermissionsScreen(onBack = { nav.popBackStack() }, onContinue = { nav.navigate(Routes.SCAN) }, contentPadding = pad)
            }
            composable(Routes.SCAN) {
                ScanScreen(
                    onClose = { nav.popBackStack() },
                    onScanned = { text ->
                        val ok = pairVm.start(text)
                        if (ok) nav.navigate(Routes.PAIR) { popUpTo(Routes.SCAN) { inclusive = true } }
                        ok
                    },
                    error = (pairState as? PairState.Invalid)?.message,
                    contentPadding = pad,
                )
            }
            composable(Routes.PAIR) {
                PairScreen(
                    state = pairState,
                    nowMs = now.toEpochMilli(),
                    onClose = {
                        pairVm.cancel()
                        if (!nav.popBackStack()) nav.navigate(if (hubs.isEmpty()) Routes.WELCOME else Routes.SESSIONS)
                    },
                    onScanAgain = {
                        pairVm.cancel()
                        nav.navigate(Routes.SCAN) { popUpTo(Routes.PAIR) { inclusive = true } }
                    },
                    onRetry = { pairVm.retry() },
                    onDone = {
                        pairVm.cancel()
                        nav.navigate(Routes.SESSIONS) { popUpTo(0) { inclusive = true } }
                    },
                    contentPadding = pad,
                )
            }
            composable(Routes.SESSIONS) {
                if (hubs.isEmpty()) {
                    LaunchedEffect(Unit) { nav.navigate(Routes.WELCOME) { popUpTo(0) { inclusive = true } } }
                    return@composable
                }
                dev.sessiontap.android.ui.sessions.SessionsScreen(
                    hubs = hubs,
                    conn = conn,
                    agents = agents,
                    hidden = hidden,
                    collapsed = collapsed,
                    now = now,
                    onOpen = { nav.navigate(Routes.detail(it)) },
                    onToggleSection = { key, value -> scope.launch { app.settings.setCollapsed(key, value) } },
                    onForget = ::forget,
                    onPair = { nav.navigate(Routes.SCAN) },
                    onHubs = { nav.navigate(Routes.HUBS) { launchSingleTop = true } },
                    onAlerts = { nav.navigate(Routes.ALERTS) { launchSingleTop = true } },
                    onRetry = { repo.reconnectAll() },
                    onOpenTailscale = { openTailscale(context) },
                    contentPadding = pad,
                )
            }
            composable(
                Routes.DETAIL,
                deepLinks = listOf(navDeepLink { uriPattern = "sessiontap://session/{hub}/{source}/{inv}" }),
            ) { entry ->
                fun arg(name: String) = URLDecoder.decode(entry.arguments?.getString(name).orEmpty(), "UTF-8")
                val key = AgentKey(arg("hub"), arg("source"), arg("inv"))
                val item = agents.firstOrNull { it.key == key && it.key !in hidden }
                DetailScreen(
                    item = item,
                    hub = hubs.firstOrNull { it.hubId == key.hubId },
                    conn = conn[key.hubId],
                    now = now,
                    onBack = { if (!nav.popBackStack()) nav.navigate(Routes.SESSIONS) },
                    onForget = {
                        forget(key)
                        if (!nav.popBackStack()) nav.navigate(Routes.SESSIONS)
                    },
                    contentPadding = pad,
                    onTerminal = { nav.navigate(Routes.terminal(key)) },
                )
            }
            composable(
                Routes.TERMINAL,
                deepLinks = listOf(navDeepLink { uriPattern = "sessiontap://terminal/{hub}/{source}/{inv}" }),
            ) { entry ->
                fun arg(name: String) = URLDecoder.decode(entry.arguments?.getString(name).orEmpty(), "UTF-8")
                val key = AgentKey(arg("hub"), arg("source"), arg("inv"))
                TerminalRoute(
                    app = app,
                    key = key,
                    item = agents.firstOrNull { it.key == key },
                    hub = hubs.firstOrNull { it.hubId == key.hubId },
                    conn = conn[key.hubId],
                    onBack = { if (!nav.popBackStack()) nav.navigate(Routes.detail(key)) },
                    onEditKeys = { nav.navigate(Routes.KEYS) { launchSingleTop = true } },
                    contentPadding = pad,
                )
            }
            composable(Routes.KEYS) {
                KeyEditorRoute(app.keyLayout, onBack = { nav.popBackStack() }, contentPadding = pad)
            }
            composable(Routes.HUBS) {
                HubsScreen(
                    hubs = hubs,
                    conn = conn,
                    now = now,
                    vpnActive = vpn,
                    onPair = { nav.navigate(Routes.SCAN) },
                    onUnpair = { hub -> scope.launch { repo.unpair(hub.hubId) } },
                    contentPadding = pad,
                )
            }
            composable(Routes.ALERTS) {
                AlertsScreen(
                    settings = alerts,
                    mutes = mutes,
                    hubs = hubs,
                    notificationsEnabled = notificationsEnabled,
                    now = now,
                    onPermission = { scope.launch { app.settings.setPermission(it) } },
                    onInput = { scope.launch { app.settings.setInput(it) } },
                    onFinished = { scope.launch { app.settings.setFinished(it) } },
                    onMute = { hub, until -> scope.launch { app.settings.mute(hub.hubId, until) } },
                    onUnmute = { hub -> scope.launch { app.settings.unmute(hub.hubId) } },
                    onOpenSettings = {
                        context.startActivity(
                            Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS).putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName)
                                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
                        )
                    },
                    contentPadding = pad,
                )
            }
        }
    }
}
