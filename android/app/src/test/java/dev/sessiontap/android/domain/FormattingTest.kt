package dev.sessiontap.android.domain

import org.junit.Assert.assertEquals
import org.junit.Test

class FormattingTest {
    @Test
    fun humanize() {
        assertEquals("2", humanizeTokens(2))
        assertEquals("999", humanizeTokens(999))
        assertEquals("15.3k", humanizeTokens(15_300))
        assertEquals("2.4M", humanizeTokens(2_400_000))
    }

    @Test
    fun shortenHomeDirectory() {
        assertEquals("~/code/api", shortenHome("/home/me/code/api"))
        assertEquals("~", shortenHome("/home/me"))
        assertEquals("~/x", shortenHome("/Users/me/x"))
        assertEquals("/work/tokenizer", shortenHome("/work/tokenizer"))
        assertEquals("/home/meow", shortenHome("/home/meow", "/home/me"))
    }

    @Test
    fun providers() {
        assertEquals(ProviderLabel("Claude Code", "CC"), providerLabel("claude"))
        assertEquals(ProviderLabel("Codex", "CX"), providerLabel("codex"))
        assertEquals(ProviderLabel("Qwen Code", "QW"), providerLabel("qwen"))
        assertEquals(ProviderLabel("Pi", "PI"), providerLabel("pi"))
        assertEquals("Open Code", providerLabel("open-code").name)
    }

    @Test
    fun usageFooterSkipsMissing() {
        assertEquals("In 1.2k", usageFooter(dev.sessiontap.android.net.Usage(inputTokens = 1_200)))
        assertEquals("", usageFooter(null))
    }
}
