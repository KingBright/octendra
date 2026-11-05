package `fun`.hackerlife.octendra

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Handler
import android.os.Looper
import java.net.NetworkInterface

/** Each start owns a listener; late callbacks cannot revive an old network generation. */
class LocalAdbDiscovery(context: Context, private val type: String, private val onPort: (Int) -> Unit) {
    private val manager = context.getSystemService(NsdManager::class.java)
    private val main = Handler(Looper.getMainLooper())
    private var current: Discovery? = null

    fun start() { main.post {
        if (current == null) {
            val next = Discovery()
            current = next
            try { manager.discoverServices("_$type._tcp", NsdManager.PROTOCOL_DNS_SD, next) }
            catch (_: RuntimeException) { if (current === next) { current = null; onPort(0) } }
        }
    } }

    fun stop() { main.post {
        val previous = current
        current = null
        onPort(0)
        previous?.stop()
    } }

    private inner class Discovery : NsdManager.DiscoveryListener {
        private var started = false
        private var stopping = false
        private var resolvedName: String? = null
        private val pending = ArrayDeque<NsdServiceInfo>()
        private var resolving = false

        fun stop() {
            if (started && !stopping) {
                stopping = true
                runCatching { manager.stopServiceDiscovery(this) }
            }
        }

        override fun onDiscoveryStarted(serviceType: String) { main.post {
            started = true
            if (current !== this) stop()
        } }
        override fun onDiscoveryStopped(serviceType: String) { main.post {
            if (current === this) { current = null; onPort(0) }
        } }
        override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) { main.post {
            if (current === this) { current = null; onPort(0) }
        } }
        override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) { main.post {
            // The listener is retired locally even if the OS failed its cleanup.
            if (current === this) { current = null; onPort(0) }
        } }
        override fun onServiceFound(info: NsdServiceInfo) { main.post {
            if (current === this) { pending.addLast(info); resolveNext() }
        } }
        override fun onServiceLost(info: NsdServiceInfo) { main.post {
            if (current === this && info.serviceName == resolvedName) { resolvedName = null; onPort(0) }
        } }

        private fun resolveNext() {
            if (current !== this || resolving || pending.isEmpty()) return
            resolving = true
            val listener = object : NsdManager.ResolveListener {
                override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) { finish(null) }
                override fun onServiceResolved(info: NsdServiceInfo) { finish(info) }
                private fun finish(info: NsdServiceInfo?) { main.post {
                    resolving = false
                    if (current !== this@Discovery) return@post
                    if (info != null && info.port in 1..65535 && local(info)) {
                        resolvedName = info.serviceName
                        onPort(info.port)
                    }
                    resolveNext()
                } }
            }
            try { manager.resolveService(pending.removeFirst(), listener) }
            catch (_: RuntimeException) { resolving = false; resolveNext() }
        }
    }

    private fun local(info: NsdServiceInfo): Boolean = runCatching {
        val host = info.host ?: return@runCatching false
        host.isLoopbackAddress || NetworkInterface.getNetworkInterfaces().toList().any { network ->
            network.inetAddresses.toList().any { it == host }
        }
    }.getOrDefault(false)
}
