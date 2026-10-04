package dev.sessiontap.android.data

import android.content.Context
import androidx.room.Dao
import androidx.room.Database
import androidx.room.Entity
import androidx.room.Query
import androidx.room.Room
import androidx.room.RoomDatabase
import androidx.room.Transaction
import androidx.room.TypeConverter
import androidx.room.TypeConverters
import androidx.room.Upsert
import dev.sessiontap.android.net.ProtocolJson
import kotlinx.coroutines.flow.Flow
import kotlinx.serialization.builtins.ListSerializer
import kotlinx.serialization.builtins.MapSerializer
import kotlinx.serialization.builtins.serializer

@Entity(tableName = "hubs", primaryKeys = ["hubId"])
data class HubEntity(
    val hubId: String,
    val name: String,
    val endpoints: List<String>,
    val scopes: List<String>,
    val lastGoodEndpoint: String? = null,
    val pairedAt: Long,
    val lastSeenAt: Long? = null,
    val lastSyncAt: Long? = null,
    val hubRevision: Long = 0,
    val revoked: Boolean = false,
    /** source_id -> display name from the latest snapshot. */
    val sources: Map<String, String> = emptyMap(),
) {
    val canManage: Boolean get() = "manage" in scopes
}

@Entity(tableName = "agents", primaryKeys = ["hubId", "sourceId", "invocationId"])
data class AgentEntity(
    val hubId: String,
    val sourceId: String,
    val invocationId: String,
    val viewJson: String,
    /** Last effective status the notification rules saw. */
    val effective: String,
    val updatedAt: String,
)

class Converters {
    private val list = ListSerializer(String.serializer())
    private val map = MapSerializer(String.serializer(), String.serializer())

    @TypeConverter
    fun fromList(value: List<String>): String = ProtocolJson.encodeToString(list, value)

    @TypeConverter
    fun toList(value: String): List<String> = ProtocolJson.decodeFromString(list, value)

    @TypeConverter
    fun fromMap(value: Map<String, String>): String = ProtocolJson.encodeToString(map, value)

    @TypeConverter
    fun toMap(value: String): Map<String, String> = ProtocolJson.decodeFromString(map, value)
}

@Dao
interface HubDao {
    @Query("SELECT * FROM hubs ORDER BY pairedAt")
    fun hubs(): Flow<List<HubEntity>>

    @Query("SELECT * FROM hubs ORDER BY pairedAt")
    suspend fun allHubs(): List<HubEntity>

    @Query("SELECT * FROM hubs WHERE hubId = :hubId")
    suspend fun hub(hubId: String): HubEntity?

    @Upsert
    suspend fun upsertHub(hub: HubEntity)

    @Query("DELETE FROM hubs WHERE hubId = :hubId")
    suspend fun deleteHubRow(hubId: String)

    @Query("SELECT * FROM agents")
    fun agents(): Flow<List<AgentEntity>>

    @Query("SELECT * FROM agents WHERE hubId = :hubId")
    suspend fun agentsForHub(hubId: String): List<AgentEntity>

    @Query("SELECT * FROM agents WHERE hubId = :hubId AND sourceId = :sourceId AND invocationId = :invocationId")
    suspend fun agent(hubId: String, sourceId: String, invocationId: String): AgentEntity?

    @Upsert
    suspend fun upsertAgents(agents: List<AgentEntity>)

    @Query("DELETE FROM agents WHERE hubId = :hubId")
    suspend fun deleteAgentsForHub(hubId: String)

    @Query("DELETE FROM agents WHERE hubId = :hubId AND sourceId = :sourceId AND invocationId = :invocationId")
    suspend fun deleteAgent(hubId: String, sourceId: String, invocationId: String)

    @Transaction
    suspend fun replaceAgents(hubId: String, agents: List<AgentEntity>) {
        deleteAgentsForHub(hubId)
        upsertAgents(agents)
    }

    @Transaction
    suspend fun deleteHub(hubId: String) {
        deleteAgentsForHub(hubId)
        deleteHubRow(hubId)
    }
}

@Database(entities = [HubEntity::class, AgentEntity::class], version = 1, exportSchema = false)
@TypeConverters(Converters::class)
abstract class SessionTapDb : RoomDatabase() {
    abstract fun hubs(): HubDao

    companion object {
        fun open(context: Context): SessionTapDb =
            Room.databaseBuilder(context, SessionTapDb::class.java, "sessiontap.db")
                .fallbackToDestructiveMigration(dropAllTables = true)
                .build()

        fun inMemory(context: Context): SessionTapDb =
            Room.inMemoryDatabaseBuilder(context, SessionTapDb::class.java).allowMainThreadQueries().build()
    }
}
