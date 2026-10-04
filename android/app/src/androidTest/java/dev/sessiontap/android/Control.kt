package dev.sessiontap.android

import android.net.Uri
import androidx.test.platform.app.InstrumentationRegistry
import java.io.ByteArrayOutputStream
import java.io.FileInputStream
import java.net.Socket
import java.net.URLEncoder

/**
 * Talks to android/scripts/test-control.py on the host (10.0.2.2:8930), which drives
 * android/scripts/test-hub.sh. Raw sockets because the app forbids cleartext HTTP.
 */
object Control {
    private const val HOST = "10.0.2.2"
    private const val PORT = 8930

    fun run(hub: Int, vararg args: String): String = get("/run", hub, args)
    fun bg(hub: Int, vararg args: String) { get("/bg", hub, args) }
    fun listen(hub: Int): String = get("/listen", hub, emptyArray())

    fun available(): Boolean = runCatching { Socket(HOST, PORT).close() }.isSuccess

    fun link(hub: Int, expired: Boolean = false): Uri =
        Uri.parse((if (expired) run(hub, "link", "expired") else run(hub, "link")).trim().lines().last())

    private fun get(path: String, hub: Int, args: Array<out String>): String {
        val query = (listOf("hub=$hub") + args.map { "arg=" + URLEncoder.encode(it, "UTF-8") }).joinToString("&")
        Socket(HOST, PORT).use { s ->
            s.soTimeout = 130_000
            s.getOutputStream().write("GET $path?$query HTTP/1.0\r\nHost: $HOST\r\n\r\n".toByteArray())
            val text = s.getInputStream().readBytes().decodeToString()
            val status = text.substringBefore("\r\n")
            val body = text.substringAfter("\r\n\r\n")
            check(" 200 " in status) { "$path $args: $status $body" }
            return body
        }
    }
}

/** Runs a shell command as the shell user and returns its output. */
fun shell(cmd: String): String {
    val pfd = InstrumentationRegistry.getInstrumentation().uiAutomation.executeShellCommand(cmd)
    val out = ByteArrayOutputStream()
    FileInputStream(pfd.fileDescriptor).use { it.copyTo(out) }
    pfd.close()
    return out.toString()
}

/** Saves a screenshot to /data/local/tmp/sessiontap-<name>.png for `adb pull`. */
fun screenshot(name: String) {
    shell("screencap -p /data/local/tmp/sessiontap-$name.png")
}
