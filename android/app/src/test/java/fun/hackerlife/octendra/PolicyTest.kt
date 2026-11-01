package `fun`.hackerlife.octendra

import org.junit.Test
import org.junit.Assert.*
import java.nio.file.Files

class PolicyTest {
    @Test fun wrappedPlatformErrorsRetainTheirActualBoundaryWithoutLeakingMessages() {
        val denied = Policy.error(java.util.concurrent.ExecutionException(java.lang.reflect.InvocationTargetException(SecurityException("private input"))))
        assertEquals("android_permission_denied",denied.getString("error"))
        assertEquals("SecurityException",denied.getString("cause_type"))
        assertFalse(denied.toString().contains("private input"))
        val unsupported = Policy.error(java.lang.reflect.InvocationTargetException(NoSuchMethodException("private symbol")))
        assertEquals("android_api_unavailable",unsupported.getString("error"))
        assertFalse(unsupported.toString().contains("private symbol"))
    }
    private fun denied(f: () -> Unit) { try { f(); fail("must reject") } catch (_: AgentError) {} }
    @Test fun originStrict() {
        assertEquals("https://example.com:8443", Policy.origin(" https://example.com:8443/ "))
        listOf("http://example.com", "https://user:pass@example.com", "https://example.com/a", "https://example.com?q=x", "https://example.com#x", "https://example.com:0", "https://").forEach { denied { Policy.origin(it) } }
    }
    @Test fun relativePaths() {
        assertEquals("a/b.png", Policy.relative("a/b.png"))
        listOf("../key", "/absolute", "a/../key", "a//b", "a\\b", "a/./b", "", "a\u0000b").forEach { denied { Policy.relative(it) } }
    }
    @Test fun symlinkEscape() {
        val root = Files.createTempDirectory("octendra-test").toFile(); val outside = Files.createTempDirectory("octendra-outside").toFile()
        try { Files.createSymbolicLink(root.toPath().resolve("escape"), outside.toPath()); denied { Policy.within(root, "escape/secret") } } finally { root.deleteRecursively(); outside.deleteRecursively() }
    }
    @Test fun sourceCredentialsAndSuffix() {
        assertEquals("https://x.blob.core.windows.net/a", Policy.source("https://x.blob.core.windows.net/a", "https://gateway.example").toString())
        listOf("http://x.blob.core.windows.net/a", "https://blob.core.windows.net.evil.test/a", "https://gateway.example/admin", "https://u:p@x.oaiusercontent.com/a").forEach { denied { Policy.source(it, "https://gateway.example") } }
    }
    @Test fun uuidAndQuote() {
        assertEquals("123e4567-e89b-42d3-a456-426614174000", Policy.uuid("123e4567-e89b-42d3-a456-426614174000"))
        denied { Policy.uuid("x") }; assertEquals("'a'\\''b'", Policy.shellQuote("a'b"))
    }
}
