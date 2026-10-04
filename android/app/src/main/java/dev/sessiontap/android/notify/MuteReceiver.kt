package dev.sessiontap.android.notify

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import dev.sessiontap.android.SessionTapApp
import kotlinx.coroutines.launch

/** "Mute <hub> 1h" notification action. */
class MuteReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val hubId = intent.getStringExtra(EXTRA_HUB) ?: return
        val app = context.applicationContext as SessionTapApp
        val pending = goAsync()
        app.scope.launch {
            try {
                app.settings.mute(hubId, System.currentTimeMillis() + MUTE_MS)
                app.notifier.cancelHub(hubId)
            } finally {
                pending.finish()
            }
        }
    }

    companion object {
        const val ACTION = "dev.sessiontap.android.MUTE_HUB"
        const val EXTRA_HUB = "hub"
        const val MUTE_MS = 60 * 60 * 1000L
    }
}
