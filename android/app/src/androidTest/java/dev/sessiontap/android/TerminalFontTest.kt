package dev.sessiontap.android

import android.graphics.Paint
import androidx.core.content.res.ResourcesCompat
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith

/** The terminal grid needs Nerd Font icons at one cell, the same advance as a letter. */
@RunWith(AndroidJUnit4::class)
class TerminalFontTest {
    @Test
    fun nerdGlyphsAreOneCellWide() {
        val context = InstrumentationRegistry.getInstrumentation().targetContext
        val paint = Paint().apply {
            typeface = ResourcesCompat.getFont(context, R.font.jetbrains_mono_nerd)
            textSize = 100f
        }
        val cell = paint.measureText("M")
        // U+E0B0 Powerline separator, U+F07B Font Awesome folder.
        for (glyph in listOf("", "")) {
            assertTrue("missing glyph U+%04X".format(glyph[0].code), paint.hasGlyph(glyph))
            assertEquals("advance of U+%04X".format(glyph[0].code), cell, paint.measureText(glyph), 0.01f)
        }
    }
}
