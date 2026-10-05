package dev.sessiontap.android.ui.keys

import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.test.assertIsNotEnabled
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performClick
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performTextInput
import dev.sessiontap.android.domain.KeyLayout
import dev.sessiontap.android.domain.KeySpec
import dev.sessiontap.android.ui.theme.SessionTapTheme
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [35], qualifiers = "w411dp-h891dp-port")
class KeyEditorScreenTest {
    @get:Rule
    val rule = createComposeRule()

    private var layout by mutableStateOf(KeyLayout.DEFAULT)

    private fun show(light: Boolean = false) {
        rule.setContent {
            SessionTapTheme(dark = !light) {
                KeyEditorScreen(layout, onChange = { layout = it }, onReset = { layout = KeyLayout.DEFAULT }, onBack = {}, contentPadding = PaddingValues())
            }
        }
    }

    @Test
    fun addRowThenCharacterAndNamedKey() {
        show()
        rule.onNodeWithTag("add-row").performClick()
        rule.onNodeWithTag("add-key").performClick()
        rule.onNodeWithTag("char-input").performTextInput("$")
        rule.onNodeWithTag("use-char").performClick()
        rule.onNodeWithTag("add-key").performClick()
        rule.onNodeWithTag("pick:page_up").performScrollTo().performClick()
        assertEquals(listOf(KeySpec.Char("$"), KeySpec.Named("page_up")), layout.rows[0])
        rule.onNodeWithTag("key-preview").assertExists()
    }

    @Test
    fun replaceAndRemove() {
        show(light = true)
        rule.onNodeWithTag("edit:0:0").performClick()
        rule.onNodeWithText("Replace Esc").assertExists()
        rule.onNodeWithTag("pick:home").performClick()
        assertEquals(KeySpec.Named("home"), layout.rows[0][0])
        rule.onNodeWithTag("edit:0:0").performClick()
        rule.onNodeWithTag("remove-key").performScrollTo().performClick()
        assertEquals(6, layout.rows[0].size)
    }

    @Test
    fun rowLimitAndReset() {
        show()
        rule.onNodeWithTag("add-row").performClick()
        rule.onNodeWithTag("add-row").performClick()
        rule.onNodeWithTag("add-row").assertIsNotEnabled()
        rule.onNodeWithText("Add row · max 4").assertExists()
        rule.onNodeWithTag("reset-keys").performClick()
        rule.onNodeWithTag("confirm-reset").performClick()
        assertEquals(KeyLayout.DEFAULT, layout)
    }
}
