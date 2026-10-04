package dev.sessiontap.android.ui.terminal

import android.content.ClipboardManager
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import dev.sessiontap.android.SessionTapApp
import dev.sessiontap.android.data.AgentItem
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.domain.TerminalAccess
import dev.sessiontap.android.domain.blockCause
import dev.sessiontap.android.domain.parseInstant
import dev.sessiontap.android.domain.sessionTitle
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.net.Status
import dev.sessiontap.android.ui.components.copyText

/** Top bar status word for the agent's effective status. */
fun agentWord(item: AgentItem?): AgentWord = when (item?.effective) {
    Status.Blocked -> AgentWord.Waiting
    Status.Running -> AgentWord.Running
    Status.Idle -> AgentWord.Idle
    Status.Stopped, null -> AgentWord.Exited
}

/** "repo · branch · hub" under the session name. */
fun terminalWhere(item: AgentItem?, hubName: String): String {
    val view = item?.view
    val dir = (view?.repository?.root ?: view?.cwd).orEmpty().trimEnd('/').substringAfterLast('/')
    return listOfNotNull(dir.ifEmpty { null }, view?.repository?.branch, hubName).joinToString(" · ")
}

/** Wires one agent's terminal screen to the hub's connection. */
@Composable
fun TerminalRoute(
    app: SessionTapApp,
    key: AgentKey,
    item: AgentItem?,
    hub: HubEntity?,
    conn: ConnState?,
    onBack: () -> Unit,
    contentPadding: PaddingValues,
) {
    val context = LocalContext.current
    val repo = app.repository
    val vm = viewModel(key = "terminal|${key.hubId}|${key.sourceId}|${key.invocationId}") {
        TerminalViewModel(
            key = key,
            hub = { repo.terminalHub(key.hubId) },
            scopes = { repo.hubs.value?.firstOrNull { it.hubId == key.hubId }?.scopes.orEmpty() },
            control = hub?.canControl == true,
            closeScope = app.scope,
        )
    }
    LaunchedEffect(vm) { vm.start() }
    val state by vm.state.collectAsStateWithLifecycle()
    val tick by vm.tick.collectAsStateWithLifecycle()
    val reply by vm.reply.collectAsStateWithLifecycle()
    val error by vm.error.collectAsStateWithLifecycle()
    val armed by vm.ctrlArmed.collectAsStateWithLifecycle()
    val hubName = hub?.name ?: "hub"
    val access = TerminalAccess.of(hub?.canWatch == true, state.control, item?.view)
    val cause = item?.takeIf { it.effective == Status.Blocked }?.let { blockCause(it.view) }
    val ui = TerminalUi(
        title = item?.let { sessionTitle(it.view) } ?: "Terminal",
        where = terminalWhere(item, hubName),
        hubName = hubName,
        sourceName = hub?.sources?.get(key.sourceId) ?: key.sourceId,
        word = agentWord(item),
        blocked = cause?.kind,
        blockedOther = item?.effective == Status.Blocked && cause?.kind == null,
        // The descriptor disappears when the terminal ends; keep chips off then.
        digits = access.digits,
        state = state,
        reply = reply,
        error = error,
        ctrlArmed = armed,
        cols = vm.emulator.cols,
        rows = vm.emulator.rows,
        startedAt = item?.view?.createdAt?.let(::parseInstant),
        unreachableDetail = (conn as? ConnState.Reconnecting)?.lastError,
    )
    TerminalScreen(
        ui = ui,
        emulator = vm.emulator,
        tick = tick,
        actions = TerminalActions(
            onBack = onBack,
            onKey = vm::key,
            onPaste = {
                val clip = context.getSystemService(ClipboardManager::class.java).primaryClip
                val text = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString().orEmpty()
                vm.paste(text)
            },
            onReply = { vm.reply.value = it },
            onSend = vm::sendReply,
            onRetry = { repo.terminalHub(key.hubId)?.kick() ?: repo.reconnectAll() },
            onCopyScreen = { copyText(context, vm.emulator.screenText()) },
        ),
        contentPadding = contentPadding,
    )
}
