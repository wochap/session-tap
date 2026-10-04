package dev.sessiontap.android.net

import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonClassDiscriminator
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.decodeFromJsonElement
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.longOrNull

/** Shared JSON settings: tolerant of fields newer hubs may add. */
val ProtocolJson = Json {
    ignoreUnknownKeys = true
    explicitNulls = false
    encodeDefaults = false
}

/** Payload of the `sessiontap-hub pair` QR code. */
@Serializable
data class QrPayload(
    val v: Int,
    val hub: String,
    val id: String,
    val ep: List<String>,
    val sc: List<String> = emptyList(),
    val s: String,
    val exp: Long,
)

@Serializable
data class Request(val id: Long, val method: String, val params: JsonObject? = null)

@Serializable
data class RpcError(val code: String, val message: String = "")

@Serializable
data class HubInfo(
    @SerialName("hub_id") val hubId: String,
    @SerialName("hub_name") val hubName: String,
    val protocol: Int,
    val scopes: List<String> = emptyList(),
)

@Serializable
data class PairBegin(val nonce: String)

@Serializable
data class PairComplete(
    @SerialName("device_id") val deviceId: String,
    @SerialName("hub_name") val hubName: String,
)

/** One frame received from the hub. */
sealed interface Incoming {
    data class Response(val id: Long, val result: JsonElement?, val error: RpcError?) : Incoming
    data class Stream(val envelope: HubEnvelope) : Incoming
}

fun parseIncoming(text: String): Incoming? {
    val obj = runCatching { ProtocolJson.parseToJsonElement(text).jsonObject }.getOrNull() ?: return null
    if (obj["event"] == JsonPrimitive("stream")) {
        val data = obj["data"] ?: return null
        return runCatching { Incoming.Stream(ProtocolJson.decodeFromJsonElement<HubEnvelope>(data)) }.getOrNull()
    }
    val id = (obj["id"] as? JsonPrimitive)?.longOrNull ?: return null
    val error = obj["error"]?.let { runCatching { ProtocolJson.decodeFromJsonElement<RpcError>(it) }.getOrNull() }
    return Incoming.Response(id, obj["result"], error)
}

@OptIn(ExperimentalSerializationApi::class)
@Serializable
@JsonClassDiscriminator("type")
sealed class HubEnvelope {
    abstract val hubRevision: Long

    @Serializable
    @SerialName("snapshot")
    data class Snapshot(
        @SerialName("hub_revision") override val hubRevision: Long,
        val sources: List<SourceInfo> = emptyList(),
        val agents: List<AgentEntry> = emptyList(),
    ) : HubEnvelope()

    @Serializable
    @SerialName("update")
    data class Update(
        @SerialName("hub_revision") override val hubRevision: Long,
        @SerialName("source_id") val sourceId: String,
        @SerialName("delivery_id") val deliveryId: String = "",
        @SerialName("source_revision") val sourceRevision: Long = 0,
        val changed: List<String> = emptyList(),
        val view: AgentView,
    ) : HubEnvelope()
}

@Serializable
data class SourceInfo(
    @SerialName("source_id") val sourceId: String,
    @SerialName("display_name") val displayName: String? = null,
    val revision: Long = 0,
)

@Serializable
data class AgentEntry(@SerialName("source_id") val sourceId: String, val view: AgentView)

@Serializable
enum class Status {
    @SerialName("running") Running,
    @SerialName("idle") Idle,
    @SerialName("blocked") Blocked,
    @SerialName("stopped") Stopped,
}

@Serializable
enum class ReasonKind {
    @SerialName("input") Input,
    @SerialName("approval") Approval,
    @SerialName("completed") Completed,
    @SerialName("failed") Failed,
}

@Serializable
data class Reason(val kind: ReasonKind, val summary: String = "")

@Serializable
data class ChildReason(val kind: ReasonKind? = null, val summary: String? = null)

@Serializable
data class ChildView(
    @SerialName("agent_id") val agentId: String,
    @SerialName("agent_type") val agentType: String,
    val status: Status,
    val reason: ChildReason? = null,
    @SerialName("started_at") val startedAt: String,
    @SerialName("updated_at") val updatedAt: String = "",
)

@Serializable
data class ProviderSession(
    val id: String,
    val name: String? = null,
    @SerialName("start_reason") val startReason: String? = null,
)

@Serializable
data class ProviderMetadata(
    val model: String? = null,
    val effort: String? = null,
    @SerialName("permission_mode") val permissionMode: String? = null,
    @SerialName("current_turn_id") val currentTurnId: String? = null,
)

@Serializable
data class Usage(
    @SerialName("input_tokens") val inputTokens: Long? = null,
    @SerialName("output_tokens") val outputTokens: Long? = null,
    @SerialName("context_tokens") val contextTokens: Long? = null,
    @SerialName("context_window_percent") val contextWindowPercent: Int? = null,
)

@Serializable
data class Repository(
    val root: String,
    val branch: String? = null,
    val head: String? = null,
    val dirty: Boolean? = null,
)

/** The hub's `PublicAgentView`. */
@Serializable
data class AgentView(
    @SerialName("invocation_id") val invocationId: String,
    val provider: String,
    val status: Status,
    val reason: Reason? = null,
    val cwd: String = "",
    @SerialName("created_at") val createdAt: String = "",
    @SerialName("updated_at") val updatedAt: String = "",
    val session: ProviderSession? = null,
    val metadata: ProviderMetadata? = null,
    val usage: Usage? = null,
    val repository: Repository? = null,
    val children: List<ChildView>? = null,
)
