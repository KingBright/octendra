package `fun`.hackerlife.octendra

import android.os.Handler
import android.os.IBinder
import android.os.SystemClock
import android.view.Display

/**
 * Per-display-group keep-awake for owned virtual displays.
 * Never acquires main-screen wake locks (e.g. SCREEN_BRIGHT_WAKE_LOCK) or invokes
 * display-unscoped PowerManager activity APIs that could wake Display.DEFAULT_DISPLAY.
 */
internal class DisplayKeepAwake(
    private val refreshIntervalMs: Long = DEFAULT_INTERVAL_MS,
    private val scheduler: Scheduler,
    private val refresher: Refresher
) : AutoCloseable {
    interface Scheduler {
        fun post(delayMs: Long, runnable: Runnable)
        fun remove(runnable: Runnable)
    }

    fun interface Refresher {
        fun refreshDisplay(displayId: Int): Boolean
    }

    private val lock = Any()
    private val activeDisplays = LinkedHashSet<Int>()
    @Volatile private var closed = false
    private var scheduled = false

    private val tickRunnable: Runnable = Runnable {
        val snapshot: List<Int>
        synchronized(lock) {
            scheduled = false
            if (closed || activeDisplays.isEmpty()) return@Runnable
            snapshot = activeDisplays.toList()
        }
        for (displayId in snapshot) {
            if (displayId > Display.DEFAULT_DISPLAY) {
                runCatching { refresher.refreshDisplay(displayId) }
            }
        }
        synchronized(lock) {
            if (!closed && activeDisplays.isNotEmpty() && !scheduled) {
                scheduled = true
                scheduler.post(refreshIntervalMs, tickRunnable)
            }
        }
    }

    constructor(handler: Handler, refreshIntervalMs: Long = DEFAULT_INTERVAL_MS) : this(
        refreshIntervalMs = refreshIntervalMs,
        scheduler = object : Scheduler {
            override fun post(delayMs: Long, runnable: Runnable) {
                handler.postDelayed(runnable, delayMs)
            }

            override fun remove(runnable: Runnable) {
                handler.removeCallbacks(runnable)
            }
        },
        refresher = PlatformPerDisplayRefresher()
    )

    fun start(displayId: Int) {
        if (displayId <= Display.DEFAULT_DISPLAY || closed) return
        val shouldPost: Boolean
        synchronized(lock) {
            if (closed) return
            activeDisplays.add(displayId)
            shouldPost = !scheduled
            if (shouldPost) scheduled = true
        }
        runCatching { refresher.refreshDisplay(displayId) }
        if (shouldPost) {
            scheduler.post(refreshIntervalMs, tickRunnable)
        }
    }

    fun poke(displayId: Int) {
        if (displayId <= Display.DEFAULT_DISPLAY || closed) return
        val isActive = synchronized(lock) { !closed && displayId in activeDisplays }
        if (isActive) {
            runCatching { refresher.refreshDisplay(displayId) }
        }
    }

    fun stop(displayId: Int) {
        synchronized(lock) {
            activeDisplays.remove(displayId)
            if (activeDisplays.isEmpty() && scheduled) {
                scheduled = false
                scheduler.remove(tickRunnable)
            }
        }
    }

    fun isActive(displayId: Int): Boolean = synchronized(lock) { !closed && displayId in activeDisplays }

    fun activeDisplayIds(): Set<Int> = synchronized(lock) { activeDisplays.toSet() }

    override fun close() {
        synchronized(lock) {
            closed = true
            activeDisplays.clear()
            if (scheduled) {
                scheduled = false
                scheduler.remove(tickRunnable)
            }
        }
    }

    @android.annotation.SuppressLint("PrivateApi")
    private class PlatformPerDisplayRefresher : Refresher {
        private val powerBinderAndMethod: Pair<Any, java.lang.reflect.Method>? by lazy {
            runCatching {
                val sm = Class.forName("android.os.ServiceManager").getMethod("getService", String::class.java)
                val binder = sm.invoke(null, "power") as? IBinder ?: return@runCatching null
                val ipm = Class.forName("android.os.IPowerManager\$Stub")
                    .getMethod("asInterface", IBinder::class.java).invoke(null, binder) ?: return@runCatching null
                // Strictly match the display-scoped IPowerManager.userActivity(int displayId, long time, int event, int flags)
                // overload. Never invoke a display-unscoped overload that affects the main display.
                val method = ipm.javaClass.methods.firstOrNull { m ->
                    m.name == "userActivity" &&
                        m.parameterTypes.size == 4 &&
                        m.parameterTypes[0] == Int::class.javaPrimitiveType &&
                        m.parameterTypes[1] == Long::class.javaPrimitiveType &&
                        m.parameterTypes[2] == Int::class.javaPrimitiveType &&
                        m.parameterTypes[3] == Int::class.javaPrimitiveType
                } ?: return@runCatching null
                ipm to method
            }.getOrNull()
        }

        override fun refreshDisplay(displayId: Int): Boolean {
            if (displayId <= Display.DEFAULT_DISPLAY) return false
            val (ipm, method) = powerBinderAndMethod ?: return false
            method.invoke(ipm, displayId, SystemClock.uptimeMillis(), 0, 0)
            return true
        }
    }

    companion object {
        const val DEFAULT_INTERVAL_MS = 15_000L
    }
}
