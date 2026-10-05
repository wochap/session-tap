package dev.sessiontap.android

import android.app.Application
import dev.sessiontap.android.data.HubRepository
import dev.sessiontap.android.data.KeyLayoutStore
import dev.sessiontap.android.data.SessionTapDb
import dev.sessiontap.android.data.SettingsStore
import dev.sessiontap.android.notify.Channels
import dev.sessiontap.android.notify.NotificationPoster
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob

/** Holds the process-wide singletons. No DI framework. */
class SessionTapApp : Application() {
    val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    lateinit var db: SessionTapDb
        private set
    lateinit var settings: SettingsStore
        private set
    lateinit var keyLayout: KeyLayoutStore
        private set
    lateinit var notifier: NotificationPoster
        private set
    lateinit var repository: HubRepository
        private set

    override fun onCreate() {
        super.onCreate()
        Channels.create(this)
        db = SessionTapDb.open(this)
        settings = SettingsStore(this)
        keyLayout = KeyLayoutStore(this)
        notifier = NotificationPoster(this)
        repository = HubRepository(db.hubs(), settings, notifier, scope)
    }
}
