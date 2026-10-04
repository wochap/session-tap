package dev.sessiontap.android.service

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import dev.sessiontap.android.SessionTapApp
import kotlinx.coroutines.launch

/** Restarts the connection service after boot when hubs are paired. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED && intent.action != Intent.ACTION_MY_PACKAGE_REPLACED) return
        val app = context.applicationContext as SessionTapApp
        val pending = goAsync()
        app.scope.launch {
            try {
                if (app.db.hubs().allHubs().isNotEmpty()) HubService.start(app)
            } finally {
                pending.finish()
            }
        }
    }
}
