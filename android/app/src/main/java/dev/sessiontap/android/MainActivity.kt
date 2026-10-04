package dev.sessiontap.android

import android.content.Intent
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.viewModels
import androidx.compose.runtime.LaunchedEffect
import androidx.navigation.compose.rememberNavController
import dev.sessiontap.android.domain.decodeBase64Url
import dev.sessiontap.android.ui.Routes
import dev.sessiontap.android.ui.SessionTapRoot
import dev.sessiontap.android.ui.pairing.PairViewModel
import dev.sessiontap.android.ui.theme.SessionTapTheme
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.first

class MainActivity : ComponentActivity() {
    private val pairVm: PairViewModel by viewModels()
    private val intents = MutableSharedFlow<Intent>(extraBufferCapacity = 4)

    override fun onCreate(savedInstanceState: Bundle?) {
        enableEdgeToEdge()
        super.onCreate(savedInstanceState)
        val app = application as SessionTapApp
        val first = intent
        setContent {
            SessionTapTheme {
                val nav = rememberNavController()
                SessionTapRoot(app, pairVm, nav)
                LaunchedEffect(Unit) {
                    // The NavHost appears once the hub list loads; navigating earlier throws.
                    nav.currentBackStackEntryFlow.first()
                    if (savedInstanceState == null) debugPair(first)?.let { nav.navigate(Routes.PAIR) }
                    intents.collect { next ->
                        if (debugPair(next) != null) nav.navigate(Routes.PAIR) else nav.handleDeepLink(next)
                    }
                }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        intents.tryEmit(intent)
    }

    /** Debug builds only: `sessiontap://pair?p=<base64url payload>` starts pairing like a scan. */
    private fun debugPair(intent: Intent?): Unit? {
        if (!BuildConfig.DEBUG) return null
        val data = intent?.data ?: return null
        if (data.scheme != "sessiontap" || data.host != "pair") return null
        val payload = data.getQueryParameter("p")?.let(::decodeBase64Url)?.decodeToString() ?: return null
        pairVm.start(payload)
        return Unit
    }
}
