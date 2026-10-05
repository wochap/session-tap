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
import java.util.Base64

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
    val endpoints: List<String> = emptyList(),
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
    /** A pushed frame of an open terminal stream. */
    data class Terminal(val stream: Long, val frame: TerminalFrame) : Incoming
}

fun parseIncoming(text: String): Incoming? {
    val obj = runCatching { ProtocolJson.parseToJsonElement(text).jsonObject }.getOrNull() ?: return null
    if (obj["event"] == JsonPrimitive("stream")) {
        val data = obj["data"] ?: return null
        return runCatching { Incoming.Stream(ProtocolJson.decodeFromJsonElement<HubEnvelope>(data)) }.getOrNull()
    }
    if (obj["type"] == JsonPrimitive("terminal")) {
        val stream = (obj["stream"] as? JsonPrimitive)?.longOrNull ?: return null
        val frame = obj["frame"] ?: return null
        return runCatching { Incoming.Terminal(stream, ProtocolJson.decodeFromJsonElement<TerminalFrame>(frame)) }.getOrNull()
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
    /** Present only while a live terminal is available for this agent. */
    val terminal: TerminalDescriptor? = null,
)

@Serializable
enum class QuickPick {
    @SerialName("digits") Digits,
    @SerialName("none") None,
}

/** The agent's public `terminal` descriptor. */
@Serializable
data class TerminalDescriptor(@SerialName("quick_pick") val quickPick: QuickPick = QuickPick.None)

/** Error codes answered by `terminal.open` and `terminal.input`. */
object TerminalErrors {
    const val FORBIDDEN = "forbidden"
    const val NOT_FOUND = "not_found"
    const val TERMINAL_UNAVAILABLE = "terminal_unavailable"
    const val UNSUPPORTED_BACKEND = "unsupported_backend"
    const val NOT_FOREGROUND = "not_foreground"
    const val PANE_IN_MODE = "pane_in_mode"
    const val TERMINAL_ENDED = "terminal_ended"
    const val BAD_REQUEST = "bad_request"
    const val SOURCE_UNAVAILABLE = "source_unavailable"
    const val SOURCE_DISALLOWS_CONTROL = "source_disallows_control"
}

/** Named keys accepted by `terminal.input`; any other key is one printable character. */
object TerminalKeys {
    const val UP = "up"
    const val DOWN = "down"
    const val LEFT = "left"
    const val RIGHT = "right"
    const val ESCAPE = "escape"
    const val TAB = "tab"
    const val BACK_TAB = "back_tab"
    const val ENTER = "enter"
    const val SPACE = "space"
    const val BACKSPACE = "backspace"
    const val CTRL_C = "ctrl_c"
    const val HOME = "home"
    const val END = "end"
    const val PAGE_UP = "page_up"
    const val PAGE_DOWN = "page_down"
    const val DELETE = "delete"
    val FUNCTION = (1..12).map { "f$it" }

    /** Every named key the hub accepts, in picker order. */
    val NAMED = listOf(
        ESCAPE, TAB, BACK_TAB, ENTER, SPACE, BACKSPACE, DELETE,
        UP, DOWN, LEFT, RIGHT, HOME, END, PAGE_UP, PAGE_DOWN,
    ) + FUNCTION

    /** [key] with the `ctrl+`/`alt+` prefixes the hub expects. */
    fun withMods(key: String, ctrl: Boolean, alt: Boolean): String =
        (if (ctrl) "ctrl+" else "") + (if (alt) "alt+" else "") + key

    /** Short human label: "Ctrl+R", "Alt+Left", "PgUp". */
    fun label(key: String): String {
        var rest = key
        val parts = mutableListOf<String>()
        if (rest.length > 5 && rest.startsWith("ctrl+")) { parts += "Ctrl"; rest = rest.removePrefix("ctrl+") }
        if (rest.length > 4 && rest.startsWith("alt+")) { parts += "Alt"; rest = rest.removePrefix("alt+") }
        parts += when (rest) {
            ESCAPE -> "Esc"
            TAB -> "Tab"
            BACK_TAB -> "Shift+Tab"
            ENTER -> "Enter"
            SPACE -> "Space"
            BACKSPACE -> "Backspace"
            DELETE -> "Del"
            UP -> "Up"
            DOWN -> "Down"
            LEFT -> "Left"
            RIGHT -> "Right"
            HOME -> "Home"
            END -> "End"
            PAGE_UP -> "PgUp"
            PAGE_DOWN -> "PgDn"
            CTRL_C -> "Ctrl+C"
            in FUNCTION -> rest.uppercase()
            else -> if (parts.isNotEmpty()) rest.uppercase() else rest
        }
        return parts.joinToString("+")
    }

    /** A single printable character, sent as a keystroke. */
    fun isChar(key: String): Boolean = key.codePointCount(0, key.length) == 1 && !Character.isISOControl(key.codePointAt(0))
}

@Serializable
enum class InputUnavailable {
    @SerialName("not_foreground") NotForeground,
    @SerialName("pane_in_mode") PaneInMode,
}

@Serializable
data class InputState(val available: Boolean, val reason: InputUnavailable? = null)

@Serializable
data class TerminalCursor(val x: Int, val y: Int, val visible: Boolean = true)

@Serializable
enum class EndReason {
    @SerialName("agent_exited") AgentExited,
    @SerialName("pane_closed") PaneClosed,
    @SerialName("session_closed") SessionClosed,
    @SerialName("multiplexer_stopped") MultiplexerStopped,
    @SerialName("identity_changed") IdentityChanged,
    @SerialName("source_unavailable") SourceUnavailable,
    @SerialName("source_disallows_control") SourceDisallowsControl,
    @SerialName("closed") Closed,
}

/** One frame of a terminal stream; `data` is base64 pane bytes. */
@OptIn(ExperimentalSerializationApi::class)
@Serializable
@JsonClassDiscriminator("type")
sealed class TerminalFrame {
    @Serializable
    @SerialName("snapshot")
    data class Snapshot(
        val seq: Long,
        val cols: Int,
        val rows: Int,
        val cursor: TerminalCursor,
        @SerialName("alternate_screen") val alternateScreen: Boolean = false,
        val data: String,
        val input: InputState,
    ) : TerminalFrame() {
        val bytes: ByteArray get() = Base64.getDecoder().decode(data)
    }

    @Serializable
    @SerialName("output")
    data class Output(val seq: Long, val data: String) : TerminalFrame() {
        val bytes: ByteArray get() = Base64.getDecoder().decode(data)
    }

    @Serializable
    @SerialName("input")
    data class Input(val available: Boolean, val reason: InputUnavailable? = null) : TerminalFrame() {
        val state: InputState get() = InputState(available, reason)
    }

    @Serializable
    @SerialName("ended")
    data class Ended(val reason: EndReason) : TerminalFrame()
}

/** Body of one `terminal.input` request: named keys or characters, or a paste. */
sealed interface TerminalInput {
    data class Keys(val keys: List<String>) : TerminalInput
    data class Paste(val text: String, val enter: Boolean) : TerminalInput
}
