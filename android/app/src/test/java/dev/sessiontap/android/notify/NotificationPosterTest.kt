package dev.sessiontap.android.notify

import android.content.Intent
import androidx.test.core.app.ApplicationProvider
import dev.sessiontap.android.data.AgentKey
import dev.sessiontap.android.data.HubEntity
import dev.sessiontap.android.domain.AgentNotice
import dev.sessiontap.android.domain.NotifyChannel
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.Shadows.shadowOf
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35], application = android.app.Application::class)
class NotificationPosterTest {
    private val context = ApplicationProvider.getApplicationContext<android.app.Application>()
    private val hub = HubEntity("hub", "MacBook", listOf("h:1"), listOf("read", "watch", "control"), pairedAt = 0)
    private val key = AgentKey("hub", "term", "inv-1")

    private fun notice(openTerminal: Boolean) = AgentNotice(
        channel = NotifyChannel.Attention,
        title = "Fix flaky auth tests",
        event = "Claude Code needs your permission",
        text = "MacBook · Claude Code needs your permission",
        body = "Bash · rm -rf build",
        footer = "",
        publicTitle = "Claude Code needs your permission",
        openTerminal = openTerminal,
    )

    @Test
    fun controlAddsOpenTerminalAction() {
        Channels.create(context)
        val n = NotificationPoster(context).build(hub, key, notice(openTerminal = true))
        assertEquals(listOf("Open", "Open terminal", "Mute MacBook 1h"), n.actions.map { it.title.toString() })
        val intent: Intent = shadowOf(n.actions[1].actionIntent).savedIntent
        assertEquals("sessiontap://terminal/hub/term/inv-1", intent.data.toString())
        // the lock-screen version gains nothing
        assertNull(n.publicVersion.actions)
    }

    @Test
    fun withoutControlOnlyOpenAndMute() {
        Channels.create(context)
        val n = NotificationPoster(context).build(hub, key, notice(openTerminal = false))
        assertEquals(listOf("Open", "Mute MacBook 1h"), n.actions.map { it.title.toString() })
    }
}
