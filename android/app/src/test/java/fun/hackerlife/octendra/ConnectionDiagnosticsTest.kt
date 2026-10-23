package `fun`.hackerlife.octendra

import org.junit.Assert.*
import org.junit.Test

class ConnectionDiagnosticsTest {
    @Test fun independentlyTracksStalledHeartbeatAndContinuingPollWithoutWallClock() {
        var elapsed = 0L; val events = mutableListOf<String>()
        val diagnostics = ConnectionDiagnostics({ elapsed }, events::add)
        diagnostics.begin(ConnectionDiagnostics.Channel.HEARTBEAT)
        elapsed = 20000
        diagnostics.begin(ConnectionDiagnostics.Channel.POLL); diagnostics.success(ConnectionDiagnostics.Channel.POLL)
        elapsed = 46000; diagnostics.reportStalls()
        assertTrue(events.contains("channel=heartbeat event=stalled elapsed_ms=46000"))
        val snapshot = diagnostics.snapshot()
        val heartbeat = snapshot.getValue("heartbeat") as Map<*, *>
        val poll = snapshot.getValue("poll") as Map<*, *>
        assertEquals(46000L, heartbeat["active_request_age_ms"])
        assertEquals(26000L, poll["last_success_age_ms"])
    }

    @Test fun transportFailureAndRecoveryClearInFlightStateAndRetainCounts() {
        var elapsed = 100L; val events = mutableListOf<String>()
        val diagnostics = ConnectionDiagnostics({ elapsed }, events::add)
        diagnostics.begin(ConnectionDiagnostics.Channel.POLL)
        elapsed = 400; diagnostics.failure(ConnectionDiagnostics.Channel.POLL, java.net.UnknownHostException("secret-config.example"))
        var state = diagnostics.snapshot().getValue("poll") as Map<*, *>
        assertEquals("dns_failure", state["last_error"]); assertEquals(-1L, state["active_request_age_ms"])
        diagnostics.begin(ConnectionDiagnostics.Channel.POLL); elapsed = 700; diagnostics.success(ConnectionDiagnostics.Channel.POLL)
        state = diagnostics.snapshot().getValue("poll") as Map<*, *>
        assertEquals("none", state["last_error"]); assertEquals(1L, state["successes"]); assertEquals(1L, state["failures"])
        assertFalse(events.joinToString().contains("secret-config"))
    }

    @Test fun onlyFixedCodesLeaveDiagnosticsEvenWhenExceptionTextContainsCredentials() {
        val sensitive = "Bearer synthetic-secret https://private.example/?token=synthetic-secret"
        val cases = listOf(
            java.net.UnknownHostException(sensitive) to "dns_failure",
            java.net.SocketTimeoutException(sensitive) to "network_timeout",
            javax.net.ssl.SSLHandshakeException(sensitive) to "tls_handshake",
            java.net.ConnectException(sensitive) to "connect_failure",
            java.net.SocketException(sensitive) to "socket_failure",
            SecurityException(sensitive) to "android_permission_denied",
            AgentError("config_unreadable", sensitive) to "config_unreadable",
            AgentError(sensitive) to "agent_error",
            IllegalStateException(sensitive) to "request_failure",
            HttpFailure(403) to "http_403"
        )
        for ((error, code) in cases) assertEquals(code, ConnectionDiagnostics.errorCode(error))
        assertEquals(ConnectionDiagnostics.Channel.FILE_SOURCE,
            ConnectionDiagnostics.channel("/device/file-source/private-operation-id?token=synthetic-secret"))
    }
}
