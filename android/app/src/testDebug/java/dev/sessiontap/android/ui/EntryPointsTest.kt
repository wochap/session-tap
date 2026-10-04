package dev.sessiontap.android.ui

import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import dev.sessiontap.android.Fixtures
import dev.sessiontap.android.Fixtures.view
import dev.sessiontap.android.data.AgentItem
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.domain.effectiveStatus
import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.ConnState
import dev.sessiontap.android.net.QuickPick
import dev.sessiontap.android.net.Status
import dev.sessiontap.android.net.TerminalDescriptor
import dev.sessiontap.android.ui.detail.DetailScreen
import dev.sessiontap.android.ui.sessions.SessionsScreen
import dev.sessiontap.android.ui.theme.SessionTapTheme
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/** Compose UI test; debug only, where ui-test-manifest provides the host activity. */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35])
class EntryPointsTest {
    @get:Rule
    val rule = createComposeRule()

    private val term = TerminalDescriptor(QuickPick.Digits)

    private fun hub(vararg scopes: String) = HubEntity("hub", "MacBook", listOf("h:1"), scopes.toList(), pairedAt = 0)
    private fun item(v: AgentView) = AgentItem(AgentKey("hub", "host", v.invocationId), v, effectiveStatus(v))

    private fun detail(hub: HubEntity, v: AgentView, onTerminal: () -> Unit = {}) = rule.setContent {
        SessionTapTheme {
            DetailScreen(item(v), hub, ConnState.Live("h:1"), Fixtures.NOW, {}, {}, PaddingValues(), onTerminal)
        }
    }

    @Test
    fun controlOffersOpenTerminal() {
        var opened = 0
        detail(hub("read", "watch", "control"), view(Status.Blocked, Fixtures.approval(), terminal = term)) { opened++ }
        rule.onNodeWithTag("open-terminal").performScrollTo().performClick()
        assertEquals(1, opened)
        rule.onAllNodesWithTag("view-terminal").assertCountEquals(0)
    }

    @Test
    fun watchOffersViewTerminal() {
        detail(hub("read", "watch"), view(Status.Running, terminal = term))
        rule.onNodeWithTag("view-terminal").assertExists()
        rule.onNodeWithText("View terminal").assertExists()
        rule.onAllNodesWithTag("open-terminal").assertCountEquals(0)
    }

    @Test
    fun noTerminalScopeShowsRepairHint() {
        detail(hub("read", "manage"), view(Status.Running, terminal = term))
        rule.onNodeWithTag("terminal-no-access").assertExists()
        rule.onAllNodesWithTag("open-terminal").assertCountEquals(0)
        rule.onAllNodesWithTag("view-terminal").assertCountEquals(0)
    }

    @Test
    fun headlessAgentHasNoTerminalEntry() {
        detail(hub("read", "watch", "control"), view(Status.Running))
        rule.onAllNodesWithTag("open-terminal").assertCountEquals(0)
        rule.onAllNodesWithTag("view-terminal").assertCountEquals(0)
        rule.onAllNodesWithTag("terminal-no-access").assertCountEquals(0)
    }

    @Test
    fun sessionRowsShowTerminalIconOnlyWhenOpenable() {
        val withTerm = view(Status.Running, terminal = term, id = "a", sessionName = "With terminal")
        val headless = view(Status.Running, id = "b", sessionName = "Headless")
        rule.setContent {
            SessionTapTheme {
                SessionsScreen(
                    hubs = listOf(hub("read", "watch")),
                    conn = mapOf("hub" to ConnState.Live("h:1")),
                    agents = listOf(item(withTerm), item(headless)),
                    hidden = emptySet(),
                    now = Fixtures.NOW,
                    onOpen = {},
                    onForget = {},
                    onPair = {},
                    onHubs = {},
                    onAlerts = {},
                    onRetry = {},
                    onOpenTailscale = {},
                    contentPadding = PaddingValues(),
                )
            }
        }
        rule.onNodeWithTag("row:With terminal").assertExists()
        rule.onNodeWithTag("row:Headless").assertExists()
        rule.onNodeWithTag("term:With terminal", useUnmergedTree = true).assertExists()
        rule.onAllNodesWithTag("term:Headless", useUnmergedTree = true).assertCountEquals(0)
    }
}
