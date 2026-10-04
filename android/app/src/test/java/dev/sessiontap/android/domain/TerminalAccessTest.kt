package dev.sessiontap.android.domain

import dev.sessiontap.android.Fixtures.view
import dev.sessiontap.android.net.QuickPick
import dev.sessiontap.android.net.Status
import dev.sessiontap.android.net.TerminalDescriptor
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class TerminalAccessTest {
    private val digits = view(Status.Blocked, terminal = TerminalDescriptor(QuickPick.Digits))
    private val plain = view(Status.Running, terminal = TerminalDescriptor(QuickPick.None))
    private val headless = view(Status.Running)

    @Test
    fun controlWithDigits() {
        assertEquals(TerminalAccess(true, TerminalLevel.Control, digits = true), TerminalAccess.of(canWatch = true, canControl = true, view = digits))
    }

    @Test
    fun controlWithoutQuickPick() {
        assertEquals(TerminalAccess(true, TerminalLevel.Control, digits = false), TerminalAccess.of(canWatch = true, canControl = true, view = plain))
    }

    @Test
    fun watchOnlyNeverOffersDigits() {
        val access = TerminalAccess.of(canWatch = true, canControl = false, view = digits)
        assertEquals(TerminalAccess(true, TerminalLevel.View, digits = false), access)
        assertTrue(access.canOpen)
    }

    @Test
    fun noTerminalScope() {
        val access = TerminalAccess.of(canWatch = false, canControl = false, view = digits)
        assertEquals(TerminalAccess(true, TerminalLevel.None, digits = false), access)
        assertFalse(access.canOpen)
    }

    @Test
    fun headlessAgentHasNoTerminal() {
        assertEquals(TerminalAccess.Unavailable, TerminalAccess.of(canWatch = true, canControl = true, view = headless))
        assertEquals(TerminalAccess.Unavailable, TerminalAccess.of(canWatch = true, canControl = true, view = null))
        assertFalse(TerminalAccess.Unavailable.canOpen)
    }
}
