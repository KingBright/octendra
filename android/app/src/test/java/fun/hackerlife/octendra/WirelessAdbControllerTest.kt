package `fun`.hackerlife.octendra

import org.junit.Assert.assertEquals
import org.junit.Assert.assertThrows
import org.junit.Test
import java.io.InputStream

class WirelessAdbControllerTest {
    @Test fun finishesBeforeBrokenTransportEof() {
        val chunks = ArrayDeque(listOf("2000\r\n\r\nfixture", ":0\r\n"))
        val stream = object : InputStream() {
            override fun read(): Int = error("unexpected single-byte read")
            override fun read(bytes: ByteArray): Int {
                check(chunks.isNotEmpty()) { "must not wait for ADB CLSE after the result" }
                val next = chunks.removeFirst().toByteArray()
                next.copyInto(bytes)
                return next.size
            }
        }
        assertEquals("2000\r\n", WirelessAdbController.readBootstrapOutput(stream, "fixture"))
    }
    @Test fun rejectsFailedAndMissingResults() {
        for (text in listOf("\nfixture:1\n", "2000\n")) {
            assertThrows(AgentError::class.java) {
                WirelessAdbController.readBootstrapOutput(text.byteInputStream(), "fixture")
            }
        }
    }
    @Test fun boundsUnterminatedOutput() {
        assertThrows(AgentError::class.java) {
            WirelessAdbController.readBootstrapOutput("x".repeat(4097).byteInputStream(), "fixture")
        }
    }
}
