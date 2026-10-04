package dev.sessiontap.android.domain

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class EndpointMergeTest {
    @Test
    fun connectedThenIncomingThenStored() {
        assertEquals(
            listOf("macbook.tailnet.ts.net:8932", "192.168.1.37:8932", "192.168.1.20:8932"),
            mergeEndpoints(
                "macbook.tailnet.ts.net:8932",
                listOf("192.168.1.37:8932", "macbook.tailnet.ts.net:8932"),
                listOf("192.168.1.20:8932", "macbook.tailnet.ts.net:8932"),
            ),
        )
    }

    @Test
    fun trimsAndDeduplicates() {
        assertEquals(
            listOf("a:1", "b:2"),
            mergeEndpoints(" a:1 ", listOf("a:1", "b:2 "), listOf("\tb:2", "a:1")),
        )
    }

    @Test
    fun capDropsStoredFirstAndKeepsFirst() {
        val incoming = (1..7).map { "10.0.0.$it:8932" }
        val stored = listOf("old1:1", "old2:2")
        val merged = mergeEndpoints("phone.ts.net:8932", incoming, stored)
        assertEquals(listOf("phone.ts.net:8932") + incoming, merged)

        val many = (1..12).map { "h$it:1" }
        val capped = mergeEndpoints("keep:1", many, emptyList())
        assertEquals(8, capped.size)
        assertEquals("keep:1", capped.first())
    }

    @Test
    fun ignoresMalformedEntries() {
        assertEquals(
            listOf("ok:1"),
            mergeEndpoints(null, listOf("", "  ", "host", "host:", "host:0", "host:65536", "a/b:1", "ok:1", "a b:1", "fd00::1:8932"), emptyList()),
        )
        assertFalse(isEndpointHint("[fd00::1]"))
        assertFalse(isEndpointHint(":8932"))
    }

    @Test
    fun acceptsIpv6AndHostNames() {
        assertTrue(isEndpointHint("[fd00::1]:8932"))
        assertTrue(isEndpointHint("macbook.tailnet.ts.net:8932"))
        assertTrue(isEndpointHint("192.168.1.20:65535"))
        assertEquals(listOf("[fd00::1]:8932"), mergeEndpoints(null, emptyList(), listOf("[fd00::1]:8932")))
    }

    @Test
    fun nullFirstAndEmptyIncoming() {
        assertEquals(listOf("a:1", "b:2"), mergeEndpoints(null, listOf("a:1"), listOf("b:2")))
        assertEquals(listOf("c:3", "a:1"), mergeEndpoints("c:3", emptyList(), listOf("a:1")))
        assertEquals(emptyList<String>(), mergeEndpoints(null, emptyList(), emptyList()))
    }
}
