package dev.sessiontap.android.ui.sessions

import androidx.compose.foundation.layout.width
import androidx.compose.ui.Modifier
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.getBoundsInRoot
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.unit.dp
import androidx.compose.foundation.layout.Box
import dev.sessiontap.android.data.AgentItem
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.net.AgentView
import dev.sessiontap.android.net.ProviderMetadata
import dev.sessiontap.android.net.Status
import dev.sessiontap.android.ui.theme.SessionTapTheme
import java.time.Instant
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35], qualifiers = "w411dp-h891dp-port")
class SessionRowTest {
    @get:Rule
    val rule = createComposeRule()

    private val now = Instant.parse("2026-10-09T12:00:00Z")

    private fun row(metadata: ProviderMetadata?, hubTag: String? = null): RowModel {
        val view = AgentView(
            invocationId = "i1",
            provider = "claude",
            status = Status.Idle,
            cwd = "/work",
            createdAt = "2026-10-09T11:00:00Z",
            updatedAt = "2026-10-09T11:55:00Z",
            metadata = metadata,
        )
        return rowModel(AgentItem(AgentKey("h", "s", "i1"), view, Status.Idle), null, hubTag, false, now)
    }

    private fun show(row: RowModel, width: Int = 411) {
        rule.setContent {
            SessionTapTheme(dark = true) {
                Box(Modifier.width(width.dp)) { SessionRow(row, onOpen = {}, onToggleKids = {}, onForget = {}) }
            }
        }
    }

    @Test
    fun rowModelPrefersLabelThenRawModel() {
        assertEquals("opus-5.5", row(ProviderMetadata(model = "claude-opus-5-5", modelLabel = "opus-5.5")).model)
        assertEquals("gpt-5.5", row(ProviderMetadata(model = "gpt-5.5")).model)
        assertEquals(null, row(ProviderMetadata(effort = "high")).model)
        assertEquals(null, row(null).model)
    }

    @Test
    fun timeSitsAtTrailingEdge() {
        val r = row(null)
        show(r)
        val rowBounds = rule.onNodeWithTag("row:${r.name}").getBoundsInRoot()
        val time = rule.onNodeWithTag("time:${r.name}", useUnmergedTree = true).getBoundsInRoot()
        // Row has 14.dp horizontal padding; the time ends at the content edge.
        assertEquals((rowBounds.right - 14.dp).value, time.right.value, 1f)
    }

    @Test
    fun chipShowsLabel() {
        val r = row(ProviderMetadata(model = "claude-opus-5-5", modelLabel = "opus-5.5"))
        show(r)
        rule.onNodeWithTag("model:${r.name}", useUnmergedTree = true).assertIsDisplayed()
        rule.onNode(androidx.compose.ui.test.hasText("opus-5.5"), useUnmergedTree = true).assertExists()
    }

    @Test
    fun chipFallsBackToRawModel() {
        val r = row(ProviderMetadata(model = "gpt-5.5"))
        show(r)
        rule.onNode(androidx.compose.ui.test.hasText("gpt-5.5"), useUnmergedTree = true).assertExists()
    }

    @Test
    fun noChipWithoutModel() {
        val r = row(null)
        show(r)
        rule.onNodeWithTag("model:${r.name}", useUnmergedTree = true).assertDoesNotExist()
    }

    @Test
    fun longNameKeepsTimeVisible() {
        val base = row(ProviderMetadata(model = "claude-opus-5-5", modelLabel = "opus-5.5"), hubTag = "work-hub")
        val r = base.copy(name = "A very long session name that cannot possibly fit on a narrow phone screen", terminal = true)
        show(r, width = 280)
        val rowBounds = rule.onNodeWithTag("row:${r.name}").getBoundsInRoot()
        val time = rule.onNodeWithTag("time:${r.name}", useUnmergedTree = true).getBoundsInRoot()
        assertEquals((rowBounds.right - 14.dp).value, time.right.value, 1f)
        assertTrue(time.right > time.left)
        rule.onNodeWithTag("model:${r.name}", useUnmergedTree = true).assertIsDisplayed()
    }
}
