package dev.sessiontap.android.domain

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test

class KeyLayoutTest {
    @Test
    fun defaultIsTwoRowsOfSeven() {
        val ids = KeyLayout.DEFAULT.rows.map { row -> row.map { it.id } }
        assertEquals(
            listOf(
                listOf("escape", "tab", "back_tab", "up", "ctrl_c", "paste", "backspace"),
                listOf("mod:ctrl", "mod:alt", "left", "down", "right", "space", "enter"),
            ),
            ids,
        )
    }

    @Test
    fun encodesAndDecodes() {
        val layout = KeyLayout.DEFAULT.addRow()
            .add(0, KeySpec.Char("$"))
            .add(0, KeySpec.Named("page_up"))
            .add(0, KeySpec.Named("f5"))
        val text = layout.encode()
        assertEquals(layout, KeyLayout.decode(text))
        assertTrue(text.contains("\"char:$\""))
    }

    @Test
    fun badStoredValueFallsBackToDefault() {
        for (bad in listOf(null, "", "nope", "[]", "[[\"warp\"]]", "[[\"char:ab\"]]", "[[],[],[],[],[]]", "[[\"up\",\"up\",\"up\",\"up\",\"up\",\"up\",\"up\",\"up\"]]")) {
            assertSame(bad, KeyLayout.DEFAULT, KeyLayout.decode(bad))
        }
    }

    @Test
    fun limits() {
        var layout = KeyLayout.DEFAULT
        assertFalse(layout.canAddTo(0))
        assertSame(layout, layout.add(0, KeySpec.Char("x")))
        layout = layout.addRow().addRow()
        assertEquals(4, layout.rows.size)
        assertFalse(layout.canAddRow)
        assertSame(layout, layout.addRow())
        val single = KeyLayout(listOf(listOf(KeySpec.Paste)))
        assertFalse(single.canDeleteRow)
        assertSame(single, single.deleteRow(0))
    }

    @Test
    fun moveAcrossRows() {
        val moved = KeyLayout.DEFAULT.remove(0, 6).move(1, 5, 0, 0)
        assertEquals(KeySpec.Named("space"), moved.rows[0][0])
        assertEquals(6, moved.rows[1].size)
        // A full target row refuses the move.
        assertSame(KeyLayout.DEFAULT, KeyLayout.DEFAULT.move(1, 5, 0, 0))
        // Within a row is always allowed.
        assertEquals(KeySpec.Named("enter"), KeyLayout.DEFAULT.move(1, 6, 1, 0).rows[1][0])
    }
}
