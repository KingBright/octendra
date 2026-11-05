package `fun`.hackerlife.octendra

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.Context
import android.content.Intent
import android.os.Build
import io.github.muntashirakon.adb.AdbConnection
import io.github.muntashirakon.adb.AdbPairingRequiredException
import io.github.muntashirakon.adb.LocalAdbPairing
import java.io.Closeable
import java.io.IOException
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/** Only bootstraps the existing bridge. User commands continue through its durable channel. */
class WirelessAdbController(private val context: Context) : Closeable {
    private val preferences = controls(context)
    private val executor = Executors.newScheduledThreadPool(2)
    private val busy = AtomicBoolean(false)
    private val automaticPairingPort = AtomicInteger(0)
    private val resourceLock = Any()
    private var resource: Closeable? = null
    @Volatile private var closed = false
    @Volatile private var pairingRequested = false
    @Volatile private var pairingPort = 0
    @Volatile private var connectionPort = 0
    @Volatile private var state = "needs_pairing"
    @Volatile private var lastError: String? = null
    @Volatile private var activationCount = 0
    @Volatile private var connectionCount = 0
    private var identity: WirelessAdbIdentity? = null
    private val notifications = context.getSystemService(NotificationManager::class.java)
    private val connectionDiscovery = LocalAdbDiscovery(context, "adb-tls-connect") { port ->
        connectionPort = port.coerceAtLeast(0)
        if (port > 0) recover()
    }
    private val pairingDiscovery = LocalAdbDiscovery(context, "adb-tls-pairing") { port ->
        pairingPort = port.coerceAtLeast(0)
        if (pairingRequested && port > 0) pairingNotification(true)
        if (port > 0) pairFromSettings()
        AgentRuntime.changed()
    }

    init {
        notifications.createNotificationChannel(NotificationChannel(CHANNEL, "无线 ADB 配对", NotificationManager.IMPORTANCE_HIGH))
        executor.scheduleWithFixedDelay({
            if (enabled() && pairingRequested) { pairingDiscovery.start(); pairFromSettings() }
            recover()
        }, 5, 10, TimeUnit.SECONDS)
    }

    private fun enabled() = !closed && AgentRuntime.running &&
        preferences.getBoolean("remote_enabled", false) && preferences.getBoolean("shell_enabled", false)
    private fun paired() = preferences.getBoolean("wireless_adb_paired", false)
    private fun change(value: String, error: String? = null) {
        state = value
        lastError = error
        AgentRuntime.changed()
    }
    fun status() = obj("paired" to paired(), "state" to state, "last_error" to lastError,
        "pairing_port" to pairingPort.takeIf { it > 0 }, "connection_port" to connectionPort.takeIf { it > 0 },
        "local_only" to true, "auto_recovery_enabled" to (enabled() && paired()),
        "automatic_pairing_available" to (AccessService.instance != null),
        "activation_count" to activationCount, "connection_count" to connectionCount)

    fun updateEnabled() {
        if (!enabled()) {
            cancelResource()
            pairingRequested = false
            runCatching { pairingDiscovery.stop() }
            runCatching { connectionDiscovery.stop() }
            notifications.cancel(NOTIFICATION)
            change("disabled")
            return
        }
        if (paired()) {
            discoverConnection()
            recover()
        } else change("needs_pairing")
    }

    fun requestPairing() {
        demand(enabled(), "start_service_and_enable_shell_first")
        demand(AccessService.instance != null || notifications.areNotificationsEnabled(), "wireless_pairing_notifications_required", "请允许 Octendra 通知，然后在无线调试页面输入配对码。")
        pairingRequested = true
        automaticPairingPort.set(0)
        change("waiting_for_pairing")
        pairingNotification(pairingPort > 0)
        pairingDiscovery.start()
    }

    fun pairFromSettings() {
        val port = pairingPort
        if (!enabled() || !pairingRequested || busy.get() || port !in 1..65535 || automaticPairingPort.get() == port) return
        val code = runCatching { AccessService.instance?.localWirelessPairingCode() }.getOrNull() ?: return
        if (automaticPairingPort.getAndSet(port) == port) { code.fill('\u0000'); return }
        pair(code)
    }

    fun pair(code: CharArray) {
        if (!enabled() || !pairingRequested || pairingPort !in 1..65535 ||
            code.size != 6 || code.any { it !in '0'..'9' }) {
            code.fill('\u0000')
            change("waiting_for_pairing", "pairing_code_or_port_required")
            if (pairingRequested) pairingNotification(pairingPort > 0)
            return
        }
        if (!busy.compareAndSet(false, true)) { code.fill('\u0000'); return }
        val port = pairingPort
        executor.execute {
            var success = false
            try {
                change("pairing")
                val keys = identity ?: WirelessAdbIdentity().also { identity = it }
                LocalAdbPairing().use { pairing ->
                    own(pairing)
                    val deadline = executor.schedule({ runCatching { pairing.close() } }, 30, TimeUnit.SECONDS)
                    try { pairing.pair(port, code, keys.privateKey, keys.certificate) }
                    finally { deadline.cancel(false); release(pairing) }
                }
                demand(enabled(), "wireless_activation_cancelled")
                preferences.edit().putBoolean("wireless_adb_paired", true).apply()
                pairingRequested = false
                pairingDiscovery.stop()
                notifications.cancel(NOTIFICATION)
                change("discovering")
                success = true
            } catch (_: Exception) {
                if (enabled()) {
                    change("waiting_for_pairing", "wireless_pairing_failed")
                    pairingNotification(pairingPort > 0)
                }
            } finally {
                code.fill('\u0000')
                busy.set(false)
            }
            if (success && enabled()) { discoverConnection(); recover(force = true) }
        }
    }

    private fun discoverConnection() {
        try { connectionDiscovery.start() }
        catch (_: RuntimeException) { change("waiting_for_wireless_debugging", "wireless_discovery_unavailable") }
    }

    fun networkChanged() {
        if (!enabled()) return
        connectionPort = 0
        runCatching { connectionDiscovery.stop() }
        if (paired()) discoverConnection()
        if (pairingRequested) {
            pairingPort = 0
            runCatching { pairingDiscovery.stop() }
            runCatching { pairingDiscovery.start() }
        }
    }

    fun recover(force: Boolean = false) {
        if (!enabled() || !paired()) return
        if (!force && AgentRuntime.bridge?.connected() == true) {
            if (!busy.get()) change("ready")
            return
        }
        val port = connectionPort
        if (port !in 1..65535) { discoverConnection(); change("waiting_for_wireless_debugging"); return }
        if (!busy.compareAndSet(false, true)) return
        executor.execute {
            try {
                change("connecting")
                activate(port)
                if (enabled()) change("ready")
            } catch (_: AdbPairingRequiredException) {
                if (enabled()) change("needs_pairing", "wireless_pairing_revoked")
            } catch (_: Exception) {
                if (enabled()) change("waiting_for_wireless_debugging", "wireless_connection_failed")
            } finally { busy.set(false) }
        }
    }

    private fun activate(port: Int) {
        val keys = identity ?: WirelessAdbIdentity().also { identity = it }
        val connection = AdbConnection.Builder("127.0.0.1", port).setApi(Build.VERSION.SDK_INT)
            .setPrivateKey(keys.privateKey).setCertificate(keys.certificate).setDeviceName("Octendra").build()
        connection.use {
            own(connection)
            val deadline = executor.schedule({ runCatching { connection.close() } }, 20, TimeUnit.SECONDS)
            try {
                demand(connection.connect(8, TimeUnit.SECONDS, true), "wireless_adb_connect_timeout")
                demand(readCommand(connection, "id -u").trim() == "2000", "wireless_adb_wrong_uid")
                connectionCount++
                demand(enabled(), "wireless_activation_cancelled")
                val bridge = AgentRuntime.bridge ?: throw AgentError("transport_stopped")
                if (!bridge.connected()) {
                    bridge.open()
                    readCommand(connection, bridge.activationShellCommand())
                    val until = android.os.SystemClock.elapsedRealtime() + 5000
                    while (enabled() && !bridge.connected() && android.os.SystemClock.elapsedRealtime() < until) Thread.sleep(50)
                    demand(enabled() && bridge.connected(), "wireless_shell_start_failed")
                    activationCount++
                }
            } finally { deadline.cancel(false); release(connection) }
        }
    }

    private fun readCommand(connection: AdbConnection, command: String): String {
        connection.open("shell:$command").use { stream ->
            val output = java.io.ByteArrayOutputStream()
            val buffer = ByteArray(1024)
            val input = stream.openInputStream()
            while (true) {
                val count = input.read(buffer)
                if (count < 0) break
                if (output.size() + count > 4096) throw IOException("wireless_activation_output_limit")
                output.write(buffer, 0, count)
            }
            return output.toString("UTF-8")
        }
    }

    private fun own(value: Closeable) = synchronized(resourceLock) {
        if (!enabled()) { value.close(); throw IOException("wireless_activation_cancelled") }
        resource = value
    }
    private fun release(value: Closeable) = synchronized(resourceLock) { if (resource === value) resource = null }
    private fun cancelResource() {
        val current = synchronized(resourceLock) { resource.also { resource = null } }
        runCatching { current?.close() }
    }

    private fun pairingNotification(canPair: Boolean) {
        if (!enabled() || !pairingRequested || !notifications.areNotificationsEnabled()) return
        val open = PendingIntent.getActivity(context, 61, Intent(context, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val builder = Notification.Builder(context, CHANNEL).setSmallIcon(R.drawable.ic_agent)
            .setContentTitle(if (canPair) "输入无线调试配对码" else "打开无线调试并选择配对码")
            .setContentText(if (canPair) "保持系统配对页面打开，在此输入六位配对码。" else "系统设置 → 开发者选项 → 无线调试 → 使用配对码配对。")
            .setContentIntent(open).setVisibility(Notification.VISIBILITY_SECRET).setOnlyAlertOnce(true)
        if (canPair) {
            val reply = PendingIntent.getService(context, 62, Intent(context, AgentService::class.java).setAction(PAIR_ACTION), PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
            val input = RemoteInput.Builder(CODE_INPUT).setLabel("六位配对码").build()
            builder.addAction(Notification.Action.Builder(null, "输入配对码", reply).addRemoteInput(input).build())
        }
        notifications.notify(NOTIFICATION, builder.build())
    }

    override fun close() {
        closed = true
        cancelResource()
        runCatching { connectionDiscovery.stop() }
        runCatching { pairingDiscovery.stop() }
        notifications.cancel(NOTIFICATION)
        executor.shutdownNow()
    }

    companion object {
        const val PAIR_ACTION = "fun.hackerlife.octendra.WIRELESS_PAIR"
        const val CODE_INPUT = "pairing_code"
        private const val CHANNEL = "wireless-adb"
        private const val NOTIFICATION = 102
    }
}
