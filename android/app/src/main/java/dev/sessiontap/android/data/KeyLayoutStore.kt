package dev.sessiontap.android.data

import android.content.Context
import androidx.datastore.core.DataStore
import androidx.datastore.preferences.core.Preferences
import androidx.datastore.preferences.core.edit
import androidx.datastore.preferences.core.stringPreferencesKey
import androidx.datastore.preferences.preferencesDataStore
import dev.sessiontap.android.domain.KeyLayout
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.map

private val Context.keyLayoutStore: DataStore<Preferences> by preferencesDataStore(name = "key_layout")

/** The terminal key bar layout. Stored only on this phone; applies to every agent and hub. */
class KeyLayoutStore(private val store: DataStore<Preferences>) {
    constructor(context: Context) : this(context.keyLayoutStore)

    private val rows = stringPreferencesKey("rows")

    val layout: Flow<KeyLayout> = store.data.map { KeyLayout.decode(it[rows]) }

    suspend fun save(layout: KeyLayout) = store.edit { it[rows] = layout.encode() }

    suspend fun reset() = store.edit { it.remove(rows) }
}
