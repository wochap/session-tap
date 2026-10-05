package dev.sessiontap.android

import android.app.NotificationManager
import android.content.Intent
import android.net.Uri
import androidx.compose.ui.test.ExperimentalTestApi
import androidx.compose.ui.test.assertIsFocused
import androidx.compose.ui.test.assertCountEquals
import androidx.compose.ui.test.assertTextContains
import androidx.compose.ui.test.onAllNodesWithTag
import androidx.compose.ui.test.hasTestTag
import androidx.compose.ui.test.hasText
import androidx.compose.ui.test.junit4.ComposeTestRule
import androidx.compose.ui.test.junit4.createEmptyComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performScrollToNode
import androidx.compose.ui.test.performTextInput
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeLeft
import androidx.core.app.NotificationCompat
import androidx.test.core.app.ActivityScenario
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import androidx.test.uiautomator.By
import androidx.test.uiautomator.UiDevice
import androidx.test.uiautomator.Until
import org.junit.After
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.FixMethodOrder
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.junit.runners.MethodSorters
import java.util.regex.Pattern

/**
 * End-to-end flow against the live test hubs (android/scripts/test-hub.sh, TEST_HUB=1 and 2)
 * through android/scripts/test-control.py. Methods run in name order and share app state:
 * onboarding on a fresh install, pairing, then the paired screens.
 */
@OptIn(ExperimentalTestApi::class)
@RunWith(AndroidJUnit4::class)
@FixMethodOrder(MethodSorters.NAME_ASCENDING)
class AppFlowTest {
    @get:Rule
    val compose = createEmptyComposeRule()

    private val instrumentation = InstrumentationRegistry.getInstrumentation()
    private val context = instrumentation.targetContext
    private val device = UiDevice.getInstance(instrumentation)
    private var scenario: ActivityScenario<MainActivity>? = null

    @Before
    fun setUp() {
        assumeTrue("test-control.py is not reachable on 10.0.2.2:8930", Control.available())
    }

    @After
    fun tearDown() {
        // A notification content intent can move the activity to another task; closing then times out.
        runCatching { scenario?.close() }
    }

    private fun launch(uri: Uri? = null) {
        val intent = Intent(context, MainActivity::class.java)
        if (uri != null) intent.setAction(Intent.ACTION_VIEW).setData(uri)
        scenario = ActivityScenario.launch(intent)
        waitFor(what = "compose hierarchy") {
            runCatching { compose.onAllNodes(androidx.compose.ui.test.isRoot()).fetchSemanticsNodes().isNotEmpty() }.getOrDefault(false)
        }
    }

    private fun ComposeTestRule.waitTag(tag: String, timeout: Long = 15_000) =
        waitUntilAtLeastOneExists(hasTestTag(tag), timeout)

    private fun ComposeTestRule.waitText(text: String, timeout: Long = 15_000) =
        waitUntilAtLeastOneExists(hasText(text, substring = true), timeout)

    private fun ComposeTestRule.waitGone(tag: String, timeout: Long = 15_000) =
        waitUntilDoesNotExist(hasTestTag(tag), timeout)

    private fun pair(hub: Int, answer: String): String {
        val link = Control.link(hub)
        Control.bg(hub, "answer", answer)
        launch(link)
        val state = if (answer == "y") "pair:Paired" else "pair:Rejected"
        compose.waitTag(state, 60_000)
        return state
    }

    private fun activeTitles(): List<String> =
        context.getSystemService(NotificationManager::class.java).activeNotifications
            .mapNotNull { it.notification.extras.getCharSequence(NotificationCompat.EXTRA_TITLE)?.toString() }

    private fun dumpsysHasTitle(title: String): Boolean =
        shell("dumpsys notification --noredact").lines()
            .any { "android.title=" in it && title in it }

    private fun waitFor(timeoutMs: Long = 15_000, what: String, cond: () -> Boolean) {
        val end = System.currentTimeMillis() + timeoutMs
        while (System.currentTimeMillis() < end) {
            if (cond()) return
            Thread.sleep(250)
        }
        throw AssertionError("timed out waiting for $what")
    }

    // 5.1 onboarding and permission checklist on a fresh install.
    @Test
    fun a01_onboardingAndPermissions() {
        launch()
        compose.waitText("Watch your coding agents from your pocket.")
        compose.onNodeWithText("sessiontap-hub pair").assertExists()
        screenshot("1a-welcome")
        compose.onNodeWithTag("scan-qr").performClick()
        compose.waitText("Before you scan")
        listOf("perm:camera", "perm:notifications", "perm:battery").forEach { compose.onNodeWithTag(it).assertExists() }
        compose.onNodeWithText("Tailscale", substring = true).assertExists()
        screenshot("1b-permissions")

        // Deny the camera: the row explains how to grant it later and does not block.
        compose.onNodeWithText("To scan the pairing code").assertExists()
        allow("perm:camera").performClick()
        device.wait(Until.findObject(By.text(Pattern.compile("Don.t allow", Pattern.CASE_INSENSITIVE))), 10_000)!!.click()
        compose.waitText("Denied · grant it later in Android settings")

        // Grant notifications through the system dialog.
        allow("perm:notifications").performClick()
        device.wait(Until.findObject(By.text(Pattern.compile("^Allow$", Pattern.CASE_INSENSITIVE))), 10_000)!!.click()
        compose.waitUntil(10_000) {
            compose.onAllNodes(hasText("So you hear when an agent is blocked")).fetchSemanticsNodes().isNotEmpty() &&
                dev.sessiontap.android.notify.notificationsAllowed(context)
        }

        compose.onNodeWithTag("continue").performClick()
        // The scan screen asks for the camera again; deny it to see the fallback.
        // The dialog can appear late or after the first resume, so keep dismissing it while waiting.
        waitFor(30_000, what = "app resumed") {
            device.findObject(By.text(Pattern.compile("Don.t allow", Pattern.CASE_INSENSITIVE)))?.click()
            runCatching { compose.onAllNodes(androidx.compose.ui.test.isRoot()).fetchSemanticsNodes().isNotEmpty() }.getOrDefault(false)
        }
        compose.waitText("Scan pairing code")
        compose.onNodeWithText("Camera access is needed to scan the code.").assertExists()
    }

    private fun allow(tag: String) =
        compose.onNode(hasText("Allow").and(androidx.compose.ui.test.hasAnyAncestor(hasTestTag(tag))))

    // 5.2 pairing via the debug deep link: expired, rejected, accepted.
    @Test
    fun a02_pairExpired() {
        launch(Control.link(1, expired = true))
        compose.waitTag("pair:Expired")
        compose.onNodeWithText("Pairing code expired").assertExists()
        screenshot("3b-expired")
    }

    @Test
    fun a03_pairRejected() {
        pair(1, "n")
        screenshot("3c-rejected")
    }

    @Test
    fun a04_pairAccepted() {
        pair(1, "y")
        screenshot("3a-paired")
        compose.onNodeWithTag("view-sessions").performClick()
        compose.waitTag("row:Fix flaky auth tests")
        assertTrue(Control.run(1, "hub", "devices").contains("read,manage"))
    }

    // 6.2 one hub: no hub chips, sections, children, filters.
    @Test
    fun a05_sessionsOneHub() {
        Control.run(1, "snapshot")
        launch()
        compose.waitTag("row:Fix flaky auth tests")
        compose.onNodeWithTag("hubchip:all").assertDoesNotExist()
        compose.onNodeWithTag("section:attn").assertExists()
        compose.onNodeWithTag("row:Provision dev box").assertExists()
        compose.onNodeWithTag("row:Refactor billing webhooks").assertExists()
        compose.onNodeWithText("Explore", substring = true).assertExists() // reason line names the child
        compose.onNodeWithText("test-runner").assertDoesNotExist()
        compose.onNodeWithTag("kids:Refactor billing webhooks").performClick()
        compose.waitText("test-runner")
        screenshot("1e-sessions")
        compose.onNodeWithTag("kids:Refactor billing webhooks").performClick()
        compose.waitUntilDoesNotExist(hasText("test-runner"), 5_000)

        compose.onNodeWithTag("filter:Running").performClick()
        compose.waitGone("row:Migrate to Vite 6")
        compose.onNodeWithTag("row:Fix flaky auth tests").assertExists()
        compose.onNodeWithTag("filter:Attention").performClick()
        compose.waitGone("row:Fix flaky auth tests")
        compose.onNodeWithTag("row:Provision dev box").assertExists()
        compose.onNodeWithTag("filter:Stale").performClick()
        compose.waitTag("empty:stale")
        compose.onNodeWithTag("filter:All").performClick()
        compose.waitTag("row:Triage open issues")

        // Collapsing a section hides its rows.
        compose.onNodeWithTag("section:attn").performClick()
        compose.waitGone("row:Provision dev box")
        compose.onNodeWithTag("section:attn").performClick()
        compose.waitTag("row:Provision dev box")
    }

    // 6.3 detail for blocked-by-child and stopped.
    @Test
    fun a06_detail() {
        launch()
        compose.waitTag("row:Refactor billing webhooks")
        compose.onNodeWithTag("row:Refactor billing webhooks").performClick()
        compose.waitTag("detail")
        compose.onNodeWithTag("status").assertTextContainsAny("Blocked", "Explore")
        compose.onNodeWithText("feat/stripe-v2", substring = true).assertExists()
        compose.onNodeWithText("Context window").performScrollTo().assertExists()
        compose.onNodeWithText("42%").assertExists()
        compose.onNodeWithText("test-runner", substring = true).performScrollTo().assertExists()
        compose.onNodeWithTag("forget").assertDoesNotExist()
        compose.onNodeWithText("Forget becomes available once this session stops.").performScrollTo().assertExists()
        screenshot("1g-detail-child")
        device.pressBack()

        compose.waitTag("row:Migrate to Vite 6")
        compose.onNodeWithTag("row:Migrate to Vite 6").performClick()
        compose.waitTag("detail")
        compose.onNodeWithTag("status").assertTextContainsAny("Completed")
        compose.onNodeWithText("Done · 14 files changed").assertExists()
        compose.onNodeWithTag("forget").performScrollTo().assertExists()
        screenshot("1h-detail-stopped")
    }

    private fun androidx.compose.ui.test.SemanticsNodeInteraction.assertTextContainsAny(vararg parts: String) {
        val text = fetchSemanticsNode().config[androidx.compose.ui.semantics.SemanticsProperties.Text].joinToString { it.text }
        parts.forEach { assertTrue("'$text' lacks '$it'", it in text) }
    }

    // 6.4 swipe to forget: undo sends nothing, expiry forgets on the hub.
    @Test
    fun a07_forgetUndoAndExpiry() {
        val triage = "00000000-0000-4000-8004-000000000001"
        Control.run(1, "snapshot")
        launch()
        compose.waitTag("row:Triage open issues")
        compose.onNodeWithTag("row:Triage open issues").performTouchInput { swipeLeft() }
        compose.waitText("Forgotten on TestHub")
        compose.onNodeWithTag("row:Triage open issues").assertDoesNotExist()
        compose.onNodeWithText("Undo").performClick()
        compose.waitTag("row:Triage open issues")
        Thread.sleep(6_000)
        assertTrue("undo must not forget", Control.listen(1).contains(triage))

        compose.onNodeWithTag("row:Triage open issues").performTouchInput { swipeLeft() }
        compose.waitText("Forgotten on TestHub")
        compose.waitUntilDoesNotExist(hasText("Forgotten on TestHub"), 10_000)
        waitFor(10_000, "hub to drop triage") { !Control.listen(1).contains(triage) }
        compose.onNodeWithTag("row:Triage open issues").assertDoesNotExist()
        // Running agents cannot be swiped away.
        compose.onNodeWithTag("row:Fix flaky auth tests").performTouchInput { swipeLeft() }
        Thread.sleep(500)
        compose.onNodeWithText("Forgotten on TestHub").assertDoesNotExist()
        Control.run(1, "snapshot")
    }

    // 6.6 alert toggles and per-hub mute suppress notifications.
    @Test
    fun a08_alertToggles() {
        launch()
        compose.waitTag("tab:Alerts")
        compose.onNodeWithTag("tab:Alerts").performClick()
        compose.waitTag("toggle:Needs permission")
        screenshot("1j-alerts")

        fun reblock(hub: Int = 1) {
            Control.run(hub, "post", "devbox", "running")
            waitFor(what = "devbox notification cancelled") { !dumpsysHasTitle("Provision dev box") }
            Control.run(hub, "post", "devbox", "approval")
            Thread.sleep(2_000)
        }

        reblock()
        assertTrue("approval notifies", dumpsysHasTitle("Provision dev box"))

        compose.onNodeWithTag("toggle:Needs permission").performClick()
        Thread.sleep(500)
        reblock()
        assertFalse("permission toggle off suppresses", dumpsysHasTitle("Provision dev box"))
        compose.onNodeWithTag("toggle:Needs permission").performClick()
        Thread.sleep(500)

        compose.onNodeWithTag("mute:TestHub").performClick()
        compose.onNodeWithText("Mute for 1 hour").performClick()
        compose.waitText("Muted until")
        reblock()
        assertFalse("muted hub is silent", dumpsysHasTitle("Provision dev box"))
        compose.onNodeWithTag("mute:TestHub").performClick()
        compose.waitText("Alerts on")
        reblock()
        assertTrue("unmuted hub notifies again", dumpsysHasTitle("Provision dev box"))
    }

    // 6.7 the notification content intent opens the session detail.
    @Test
    fun a09_notificationOpensDetail() {
        launch()
        compose.waitTag("tab:Hubs")
        compose.onNodeWithTag("tab:Hubs").performClick()
        compose.waitTag("hub:TestHub")
        waitFor(what = "devbox notification") { "Provision dev box" in activeTitles() }
        val sbn = context.getSystemService(NotificationManager::class.java).activeNotifications
            .first { it.notification.extras.getCharSequence(NotificationCompat.EXTRA_TITLE)?.toString() == "Provision dev box" }
        sbn.notification.contentIntent.send()
        compose.waitTag("detail")
        compose.onNodeWithText("Provision dev box").assertExists()
        compose.onNodeWithTag("status").assertTextContainsAny("Blocked", "permission")
        // Bottom nav attention badge on the Sessions tab.
        device.pressBack()
        compose.waitUntilAtLeastOneExists(
            hasTestTag("tab:Sessions").and(
                androidx.compose.ui.test.SemanticsMatcher.expectValue(
                    androidx.compose.ui.semantics.SemanticsProperties.StateDescription, "2 need attention",
                ),
            ),
            15_000,
        )
    }

    // 6.2 two hubs: chips and per-hub sections.
    @Test
    fun a10_sessionsTwoHubs() {
        pair(2, "y")
        compose.onNodeWithTag("view-sessions").performClick()
        compose.waitTag("hubchip:TestHub2", 30_000)
        compose.onNodeWithTag("hubchip:all").assertExists()
        compose.onNodeWithTag("hubchip:TestHub").assertExists()
        compose.waitUntil(15_000) { compose.onAllNodes(hasTestTag("row:Fix flaky auth tests")).fetchSemanticsNodes().size == 2 }
        assertTrue(compose.onAllNodes(hasTestTag("section:attn")).fetchSemanticsNodes().size == 1)
        assertTrue(compose.onAllNodes(hasText("· live", substring = true), useUnmergedTree = true).fetchSemanticsNodes().isNotEmpty())
        screenshot("3e-two-hubs")
        compose.onNodeWithTag("hubchip:TestHub2").performClick()
        compose.waitUntil(10_000) { compose.onAllNodes(hasTestTag("row:Fix flaky auth tests")).fetchSemanticsNodes().size == 1 }
        compose.onNodeWithTag("hubchip:all").performClick()
        compose.waitUntil(10_000) { compose.onAllNodes(hasTestTag("row:Fix flaky auth tests")).fetchSemanticsNodes().size == 2 }
    }

    // 6.5 hubs screen: revoked state after `sessiontap-hub revoke`, then unpair.
    @Test
    fun a11_hubsRevokedAndUnpair() {
        launch()
        compose.waitTag("tab:Hubs")
        compose.onNodeWithTag("tab:Hubs").performClick()
        compose.waitTag("hub:TestHub2")
        compose.waitUntil(15_000) { compose.onAllNodes(hasTestTag("conn:TestHub2").and(hasText("live"))).fetchSemanticsNodes().isNotEmpty() }
        screenshot("1i-hubs")
        val deviceId = Control.run(2, "hub", "devices").lines().drop(1).first { it.isNotBlank() }.substringBefore(' ')
        Control.run(2, "hub", "revoke", deviceId)
        compose.waitUntil(30_000) { compose.onAllNodes(hasTestTag("conn:TestHub2").and(hasText("revoked"))).fetchSemanticsNodes().isNotEmpty() }
        compose.onNodeWithText("Pair again").assertExists()
        screenshot("3h-revoked")
        compose.onNodeWithTag("unpair:TestHub2").performScrollTo().performClick()
        compose.onNodeWithTag("confirm-unpair").performClick()
        compose.waitGone("hub:TestHub2")
        compose.onNodeWithTag("hub:TestHub").assertExists()
    }
    // Scopes: requested chips while waiting, granted chips on the hub card, narrowing by re-pairing.
    @Test
    fun a12_scopeChips() {
        // Answer only after the waiting screen is checked, so it stays up.
        launch(Control.link(1, scopes = listOf("manage", "control")))
        compose.waitTag("scope-chip-control", 60_000)
        listOf("read", "manage", "watch").forEach { compose.onNodeWithTag("scope-chip-$it").assertExists() }
        compose.onNodeWithTag("scope-control-warning").assertExists()
        compose.onNodeWithText("Control terminal can type into agents, which can run commands on TestHub.").assertExists()
        screenshot("8a-scopes-requested")
        Control.bg(1, "answer", "y")
        compose.waitTag("pair:Paired", 60_000)
        assertTrue(Control.run(1, "hub", "devices").contains("read,manage,watch,control"))
        compose.onNodeWithTag("view-sessions").performClick()
        compose.waitTag("tab:Hubs")
        compose.onNodeWithTag("tab:Hubs").performClick()
        compose.waitTag("hub-scope-control")
        listOf("read", "manage", "watch").forEach { compose.onNodeWithTag("hub-scope-$it").assertExists() }
        screenshot("8b-hub-scopes")

        // Re-pair with read only: Manage leaves the card and the session list keeps updating.
        val link = Control.link(1, scopes = listOf("read"))
        Control.bg(1, "answer", "y")
        launch(link)
        compose.waitTag("pair:Paired", 60_000)
        compose.onNodeWithTag("view-sessions").performClick()
        compose.waitTag("tab:Hubs")
        compose.onNodeWithTag("tab:Hubs").performClick()
        compose.waitTag("hub-scope-read")
        compose.waitGone("hub-scope-manage")
        listOf("watch", "control").forEach { compose.onNodeWithTag("hub-scope-$it").assertDoesNotExist() }
        compose.onNodeWithTag("tab:Sessions").performClick()
        compose.waitTag("row:Triage open issues")
        compose.onNodeWithTag("filter:Running").performClick()
        compose.waitGone("row:Triage open issues")
        Control.run(1, "post", "triage", "running")
        compose.waitTag("row:Triage open issues")
        compose.onNodeWithTag("filter:All").performClick()
        Control.run(1, "snapshot")
    }

    /** Row tags drawn inside the feed, top to bottom. */
    private fun visibleRows(): List<String> {
        val feed = compose.onNodeWithTag("feed").fetchSemanticsNode().boundsInRoot
        val tag = androidx.compose.ui.semantics.SemanticsProperties.TestTag
        val row = androidx.compose.ui.test.SemanticsMatcher("row") { it.config.getOrElseNullable(tag) { null }?.startsWith("row:") == true }
        return compose.onAllNodes(row).fetchSemanticsNodes()
            .filter { it.boundsInRoot.bottom > feed.top && it.boundsInRoot.top < feed.bottom }
            .sortedBy { it.boundsInRoot.top }
            .map { it.config[tag] }
    }

    // Feed top anchor: at the top, an agent moving above the first row stays visible; scrolled down, nothing moves.
    @Test
    fun a12b_feedTopAnchor() {
        Control.run(1, "snapshot")
        // A larger font and density make the short fixture list scroll.
        val scale = shell("settings get system font_scale").trim().toFloatOrNull() ?: 1f
        shell("settings put system font_scale 2.0")
        shell("wm density 720")
        try {
            launch()
            compose.waitTag("row:Fix flaky auth tests")
            compose.waitForIdle()
            val first = visibleRows().first()
            assertTrue("triage must not start first", first != "row:Triage open issues")
            Control.run(1, "post", "triage", "approval")
            runCatching { compose.waitUntil(15_000) { visibleRows().firstOrNull() == "row:Triage open issues" } }
                .onFailure { throw AssertionError("first row not triage: ${visibleRows()}", it) }

            // Scroll to the bottom so only rows below the attention section are on screen.
            compose.onNodeWithTag("feed").performScrollToNode(hasTestTag("row:Migrate to Vite 6"))
            compose.waitForIdle()
            val before = visibleRows()
            screenshot("feed-scrolled")
            assertTrue("feed did not scroll: $before", before.firstOrNull() != "row:Triage open issues")
            // Devbox moves to the top of the attention section, above the screen.
            Control.run(1, "post", "devbox", "approval")
            Thread.sleep(1_500)
            compose.waitForIdle()
            assertTrue("visible rows moved: $before -> ${visibleRows()}", visibleRows() == before)
        } finally {
            shell("settings put system font_scale $scale")
            shell("wm density reset")
            Control.run(1, "snapshot")
        }
    }

    private fun textOf(tag: String): String =
        compose.onNodeWithTag(tag).fetchSemanticsNode().config[androidx.compose.ui.semantics.SemanticsProperties.Text].joinToString { it.text }

    /** Turns direct mode back on; reading the screen through the menu hides the keyboard, which leaves it. */
    private fun directMode() {
        if (compose.onAllNodesWithTag("direct-strip").fetchSemanticsNodes().isEmpty()) compose.onNodeWithTag("keyboard-toggle").performClick()
        compose.waitTag("direct-input")
    }

    /** The pane's visible text, through the top bar's "Copy visible screen". */
    private fun visibleScreen(): String {
        compose.onNodeWithTag("terminal-menu").performClick()
        compose.onNodeWithTag("menu:copy").performClick()
        compose.waitForIdle()
        var text = ""
        val instrumentation = InstrumentationRegistry.getInstrumentation()
        instrumentation.runOnMainSync {
            val clip = instrumentation.targetContext.getSystemService(android.content.ClipboardManager::class.java).primaryClip
            text = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.text?.toString().orEmpty()
        }
        return text
    }

    // Terminal: pair with control, open the fixture agent's terminal, answer its approval, see it end.
    @Test
    fun a13_terminal() {
        val link = Control.link(1, scopes = listOf("control"))
        Control.bg(1, "answer", "y")
        launch(link)
        compose.waitTag("pair:Paired", 60_000)
        try {
            Control.run(1, "terminal", "start")
            compose.onNodeWithTag("view-sessions").performClick()
            val icon = androidx.compose.ui.test.SemanticsMatcher("terminal icon") {
                it.config.getOrElseNullable(androidx.compose.ui.semantics.SemanticsProperties.TestTag) { null }?.startsWith("term:") == true
            }
            compose.waitUntil(30_000) { compose.onAllNodes(icon, useUnmergedTree = true).fetchSemanticsNodes().isNotEmpty() }
            val name = compose.onAllNodes(icon, useUnmergedTree = true).fetchSemanticsNodes().first()
                .config[androidx.compose.ui.semantics.SemanticsProperties.TestTag].removePrefix("term:")
            compose.onNodeWithTag("row:$name").performClick()
            compose.waitTag("detail")
            compose.onNodeWithTag("open-terminal").performScrollTo().performClick()
            compose.waitTag("terminal")
            compose.waitTag("digit:1", 30_000)
            compose.onNodeWithTag("terminal-status").assertTextContainsAny("waiting for you")
            screenshot("9a-terminal-approval")

            // The emulator has a hardware keyboard; force the soft keyboard so direct mode keeps it up.
            val directIme = shell("settings get secure show_ime_with_hard_keyboard").trim()
            shell("settings put secure show_ime_with_hard_keyboard 1")
            try {
                // Direct mode: keystrokes go straight to the pane. Answer the menu with `2`.
                compose.onNodeWithTag("keyboard-toggle").performClick()
                compose.waitTag("direct-strip")
                compose.onAllNodesWithTag("reply").assertCountEquals(0)
                compose.onNodeWithTag("direct-input").performTextInput("2")
                compose.waitUntil(30_000) {
                    val status = compose.onNodeWithTag("terminal-status").fetchSemanticsNode()
                        .config[androidx.compose.ui.semantics.SemanticsProperties.Text].joinToString { it.text }
                    status != "waiting for you"
                }
                compose.onNodeWithTag("digit:1").assertDoesNotExist()
                compose.waitUntil(15_000) { "You chose option 2" in visibleScreen() }
                screenshot("9b-terminal-answered")

                // The fake agent reads a line: type it key by key, then Enter from the key bar.
                directMode()
                compose.onNodeWithTag("direct-input").performTextInput("hi")
                compose.onNodeWithTag("key:enter").performClick()
                compose.waitUntil(15_000) { "Agent got: hi" in visibleScreen() }

                // A latched Ctrl applies to the next typed key and the strip names the combination.
                directMode()
                compose.onNodeWithTag("key:mod:ctrl").performClick()
                compose.onNodeWithTag("mod-chip").assertTextContains("Ctrl · next")
                compose.onNodeWithTag("direct-input").performTextInput("x")
                compose.waitUntil(5_000) { textOf("direct-sub") == "Sent Ctrl+X" }
                compose.onAllNodesWithTag("mod-chip").assertCountEquals(0)
                screenshot("9b1-terminal-direct")
                compose.onNodeWithTag("keyboard-toggle").performClick()
                compose.waitTag("reply")

            } finally {
                shell("settings put secure show_ime_with_hard_keyboard ${directIme.toIntOrNull() ?: 0}")
            }

            // Edit keys: a third row with `$` shows on the terminal key bar.
            compose.onNodeWithTag("terminal-menu").performClick()
            compose.onNodeWithTag("menu:edit-keys").performClick()
            compose.waitTag("key-editor")
            compose.onNodeWithTag("add-row").performClick()
            compose.onNodeWithTag("add-key").performClick()
            compose.onNodeWithTag("quick:$").performScrollTo().performClick()
            screenshot("9d-key-editor")
            compose.onNodeWithTag("keys-back").performClick()
            compose.waitTag("terminal")
            compose.waitTag("key:char:$")
            val top = { tag: String -> compose.onNodeWithTag(tag).fetchSemanticsNode().boundsInRoot.top }
            assertTrue("three rows", top("key:char:$") < top("key:escape") && top("key:escape") < top("key:mod:ctrl"))
            screenshot("9d2-terminal-three-rows")
            compose.onNodeWithTag("terminal-menu").performClick()
            compose.onNodeWithTag("menu:edit-keys").performClick()
            compose.waitTag("key-editor")
            compose.onNodeWithTag("reset-keys").performClick()
            compose.onNodeWithTag("confirm-reset").performClick()
            compose.onNodeWithTag("keys-back").performClick()
            compose.waitTag("terminal")
            compose.waitGone("key:char:$")

            // The emulator has a hardware keyboard; force the soft keyboard so the layout swap happens.
            val imeSetting = shell("settings get secure show_ime_with_hard_keyboard").trim()
            shell("settings put secure show_ime_with_hard_keyboard 1")
            try {
                compose.onNodeWithTag("reply").performClick()
                // With the keyboard open the reply field moves above the key bar.
                fun replyAboveKeys() =
                    compose.onNodeWithTag("reply").fetchSemanticsNode().boundsInRoot.top <
                        compose.onNodeWithTag("key-bar").fetchSemanticsNode().boundsInRoot.top
                runCatching { compose.waitUntil(15_000) { replyAboveKeys() } }.onFailure {
                    screenshot("9b2-fail")
                    throw AssertionError("keyboard: ${shell("dumpsys input_method").lines().filter { l -> "mInputShown" in l || "mServedView" in l || "isInputViewShown" in l }}", it)
                }
                compose.waitForIdle()
                Thread.sleep(1_000)
                compose.waitForIdle()
                assertTrue("keyboard closed after focusing the reply field", replyAboveKeys())
                compose.onNodeWithTag("reply").assertIsFocused()
                compose.onNodeWithTag("reply").performTextInput("see CI run 4821")
                compose.onNodeWithTag("reply").assertTextContains("see CI run 4821")
                screenshot("9b2-terminal-reply-keyboard")
            } finally {
                shell("settings put secure show_ime_with_hard_keyboard ${imeSetting.toIntOrNull() ?: 0}")
            }

            Control.run(1, "terminal", "exit")
            compose.waitTag("end-card", 30_000)
            compose.onNodeWithText("Agent exited").assertExists()
            compose.onNodeWithTag("key-bar").assertDoesNotExist()
            screenshot("9c-terminal-ended")
        } finally {
            Control.run(1, "terminal", "stop")
        }
    }
}
