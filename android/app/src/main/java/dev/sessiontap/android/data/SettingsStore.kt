package dev.sessiontap.android.data

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.booleanPreferencesKey
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.longPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import dev.sessiontap.android.domain.AlertSettings
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map

private val Context.alertStore: DataStore<Preferences> by preferencesDataStore(name = "alerts")

/** Alert toggles and per-hub mutes. Stored only on this phone. */
class SettingsStore(private val store: DataStore<Preferences>) {
    constructor(context: Context) : this(context.alertStore)

    private val permission = booleanPreferencesKey("permission")
    private val input = booleanPreferencesKey("input")
    private val finished = booleanPreferencesKey("finished")
    private fun muteKey(hubId: String) = longPreferencesKey("mute_$hubId")

    val settings: Flow<AlertSettings> = store.data.map {
        AlertSettings(
            permission = it[permission] ?: true,
            input = it[input] ?: true,
            finished = it[finished] ?: true,
        )
    }

    /** hub id -> mute expiry (epoch ms). Expired entries are included; compare against now. */
    val mutes: Flow<Map<String, Long>> = store.data.map { prefs ->
        prefs.asMap().mapNotNull { (key, value) ->
            if (key.name.startsWith("mute_") && value is Long) key.name.removePrefix("mute_") to value else null
        }.toMap()
    }

    suspend fun current(): AlertSettings = settings.first()

    suspend fun isMuted(hubId: String, nowMs: Long): Boolean = (mutes.first()[hubId] ?: 0) > nowMs

    suspend fun setPermission(on: Boolean) = store.edit { it[permission] = on }
    suspend fun setInput(on: Boolean) = store.edit { it[input] = on }
    suspend fun setFinished(on: Boolean) = store.edit { it[finished] = on }

    suspend fun mute(hubId: String, untilMs: Long) = store.edit { it[muteKey(hubId)] = untilMs }
    suspend fun unmute(hubId: String) = store.edit { it.remove(muteKey(hubId)) }
}
