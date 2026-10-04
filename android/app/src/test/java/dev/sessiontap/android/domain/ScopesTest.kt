package dev.sessiontap.android.domain

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ScopesTest {
    @Test
    fun labelsKnownScopesAndKeepsUnknownRaw() {
        assertEquals("Read", Scopes.label("read"))
        assertEquals("Manage", Scopes.label("manage"))
        assertEquals("Watch terminal", Scopes.label("watch"))
        assertEquals("Control terminal", Scopes.label("control"))
        assertEquals("teleport", Scopes.label("teleport"))
    }

    @Test
    fun ordersCanonicallyWithUnknownLast() {
        assertEquals(
            listOf("read", "manage", "watch", "control", "teleport"),
            Scopes.ordered(listOf("control", "teleport", "read", "watch", "manage", "read")),
        )
    }

    @Test
    fun onlyControlWarns() {
        val chips = Scopes.chips(listOf("control", "watch", "read"))
        assertEquals(listOf("Read", "Watch terminal", "Control terminal"), chips.map { it.label })
        assertEquals(listOf(false, false, true), chips.map { it.warning })
        assertEquals(listOf("read", "watch", "control"), Scopes.chips(listOf("control", "watch", "read"), short = true).map { it.label })
        assertFalse(Scopes.isWarning("manage"))
        assertTrue(Scopes.isWarning("control"))
    }
}
