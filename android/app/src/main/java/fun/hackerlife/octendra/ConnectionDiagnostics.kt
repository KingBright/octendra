package `fun`.hackerlife.octendra

/** Only fixed channel/error codes and monotonic durations leave this class. Never exception text. */
class ConnectionDiagnostics(
    private val clock: () -> Long = { System.nanoTime() / 1_000_000 },
    private val emit: (String) -> Unit = {}
) {
    enum class Channel(val wire: String) { HEALTH("health"), POLL("poll"), HEARTBEAT("heartbeat"), RECEIPT("receipt"), FILE_SOURCE("file_source") }
    private data class State(var started: Long? = null, var succeeded: Long? = null, var successes: Long = 0,
        var failures: Long = 0, var error: String? = null)
    private val channels = Channel.entries.associateWith { State() }

    @Synchronized fun begin(channel: Channel) {
        channels.getValue(channel).started = clock()
        emit("channel=${channel.wire} event=started")
    }
    @Synchronized fun success(channel: Channel) {
        val state = channels.getValue(channel); val now = clock()
        val elapsed = state.started?.let { (now - it).coerceAtLeast(0) } ?: 0
        state.started = null; state.succeeded = now; state.successes++; state.error = null
        emit("channel=${channel.wire} event=success elapsed_ms=$elapsed")
    }
    @Synchronized fun failure(channel: Channel, error: Exception) {
        val state = channels.getValue(channel); val now = clock()
        val elapsed = state.started?.let { (now - it).coerceAtLeast(0) } ?: 0
        state.started = null; state.failures++; state.error = errorCode(error)
        emit("channel=${channel.wire} event=failed code=${state.error} elapsed_ms=$elapsed")
    }
    @Synchronized fun reportStalls() {
        val now = clock()
        for ((channel, state) in channels) state.started?.let {
            if (now - it >= 45000) emit("channel=${channel.wire} event=stalled elapsed_ms=${now - it}")
        }
    }
    @Synchronized fun snapshot(): Map<String, Any> {
        val now = clock()
        return channels.mapKeys { it.key.wire }.mapValues { (_, state) ->
            mapOf("successes" to state.successes, "failures" to state.failures,
                "last_success_age_ms" to (state.succeeded?.let { (now - it).coerceAtLeast(0) } ?: -1L),
                "active_request_age_ms" to (state.started?.let { (now - it).coerceAtLeast(0) } ?: -1L),
                "last_error" to (state.error ?: "none"))
        }
    }
    companion object {
        fun channel(path: String): Channel = when (path) {
            "/healthz" -> Channel.HEALTH
            "/device/poll" -> Channel.POLL
            "/device/heartbeat" -> Channel.HEARTBEAT
            "/device/result" -> Channel.RECEIPT
            else -> Channel.FILE_SOURCE
        }
        fun errorCode(error: Exception): String = when (error) {
            is HttpFailure -> if (error.status in 100..599) "http_${error.status}" else "http_other"
            is java.net.UnknownHostException -> "dns_failure"
            is java.net.SocketTimeoutException -> "network_timeout"
            is javax.net.ssl.SSLHandshakeException -> "tls_handshake"
            is javax.net.ssl.SSLException -> "tls_failure"
            is java.net.ConnectException -> "connect_failure"
            is java.net.SocketException -> "socket_failure"
            is java.io.IOException -> "io_failure"
            is InterruptedException -> "interrupted"
            else -> "request_failure"
        }
    }
}
