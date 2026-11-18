/* Background display flags and shell-context integration adapted from Mika
 * (wangzan101/Mika, Apache-2.0). See assets/licenses/mika/NOTICE and LICENSE.
 * Octendra modifications: existing UID-authenticated bridge, owned displays only,
 * independent focus required, no model/task framework or public listener. */
package `fun`.hackerlife.octendra

import android.annotation.SuppressLint
import android.app.ActivityOptions
import android.content.ComponentName
import android.content.Intent
import android.content.pm.LauncherApps
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.os.Binder
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.os.SystemClock
import android.os.UserHandle
import android.os.UserManager
import android.view.Display
import android.view.InputDevice
import android.view.InputEvent
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.FutureTask
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

/** Runs in the already-authorized shell UID, never in an unprivileged app. */
@android.annotation.TargetApi(34)
internal class ShellUiService(private val appUid: Int) : IUiShell.Stub(), AutoCloseable {
    private val thread = HandlerThread("octendra-shell-ui").apply { start() }
    private val handler = Handler(thread.looper)
    private val aiMotionExecutor = Executors.newCachedThreadPool()
    private val keepAwake = DisplayKeepAwake(handler)
    private val context by lazy { ShellUiContext.create() }
    private val manager by lazy {
        DisplayManager::class.java.getDeclaredConstructor(android.content.Context::class.java)
            .apply { isAccessible = true }.newInstance(context)
    }
    private val displays = HashMap<Int, VirtualDisplay>()
    private val aiCancelEpochs = ConcurrentHashMap<Int, AtomicLong>()
    private val displayInputLocks = ConcurrentHashMap<Int, Any>()
    private val activeAiMotion = ConcurrentHashMap<Int, MotionEvent>()
    private val activeAiKeys = ConcurrentHashMap<Int, KeyEvent>()
    private val activeHumanMotion = ConcurrentHashMap<Int, MotionEvent>()
    private val activeHumanKeys = ConcurrentHashMap<Int, ConcurrentHashMap<Int, KeyEvent>>()
    @Volatile private var closed = false

    private fun inputLock(displayId: Int): Any =
        displayInputLocks.computeIfAbsent(displayId) { Any() }

    private val inputInstance by lazy {
        val type = runCatching { Class.forName("android.hardware.input.InputManagerGlobal") }
            .getOrElse { Class.forName("android.hardware.input.InputManager") }
        type to type.getMethod("getInstance").invoke(null)
    }
    private val injectInputMethod by lazy {
        inputInstance.first.getMethod("injectInputEvent", InputEvent::class.java, Int::class.javaPrimitiveType)
    }
    private val setDisplayIdMethod by lazy {
        InputEvent::class.java.getMethod("setDisplayId", Int::class.javaPrimitiveType)
    }
    private val atmService by lazy {
        Class.forName("android.app.ActivityTaskManager").getMethod("getService").invoke(null)
    }

    private fun checkCaller() {
        if (Binder.getCallingUid() != appUid) throw SecurityException("app_peer_uid_mismatch")
        demand(!closed, "shell_ui_closed")
    }

    private fun report(block: () -> JSONObject): String {
        checkCaller()
        val task = FutureTask { block() }
        handler.post(task)
        return try { task.get(10, TimeUnit.SECONDS).toString() }
        catch (e: Exception) { task.cancel(false); Policy.error(e).toString() }
    }

    private fun <T> onHandler(block: () -> T): T {
        checkCaller()
        if (Looper.myLooper() == thread.looper) return block()
        val task = FutureTask { block() }
        handler.post(task)
        return try { task.get(10, TimeUnit.SECONDS) }
        catch (e: Exception) { task.cancel(false); throw e }
    }

    private fun aiEpoch(displayId: Int): AtomicLong =
        aiCancelEpochs.computeIfAbsent(displayId) { AtomicLong(0L) }

    private fun sendInput(displayId: Int, ev: InputEvent, mode: Int = 2): Boolean {
        setDisplayIdMethod.invoke(ev, displayId)
        return injectInputMethod.invoke(inputInstance.second, ev, mode) == true
    }

    @SuppressLint("WrongConstant") // Hidden SDK flags; actual own-focus flags are checked below.
    override fun createDisplay(surface: Surface, width: Int, height: Int, dpi: Int): String = report {
        demand(Build.VERSION.SDK_INT >= 34 && width in 1..4096 && height in 1..4096 && dpi in 72..1000, "background_display_unsupported")
        // Trusted + own focus + own display group + no top-focus stealing. Never fall back to a
        // display that could take focus away from the owner's main screen.
        val flags = (1 shl 0) or (1 shl 3) or (1 shl 6) or (1 shl 7) or (1 shl 8) or
            (1 shl 10) or (1 shl 11) or (1 shl 12) or (1 shl 13) or (1 shl 14) or (1 shl 16)
        val display = manager.createVirtualDisplay("Octendra AI", width, height, dpi, surface, flags)
            ?: throw AgentError("background_display_unavailable")
        try {
            val own = Display::class.java.getField("FLAG_OWN_FOCUS").getInt(null)
            val noSteal = Display::class.java.getField("FLAG_STEAL_TOP_FOCUS_DISABLED").getInt(null)
            demand(display.display.flags and own != 0 && display.display.flags and noSteal != 0, "background_focus_unavailable")
            val windows = Class.forName("android.view.WindowManagerGlobal").getMethod("getWindowManagerService").invoke(null)
            windows.javaClass.getMethod("setDisplayImePolicy", Int::class.javaPrimitiveType, Int::class.javaPrimitiveType)
                .invoke(windows, display.display.displayId, 0)
            val id = display.display.displayId
            displays[id] = display
            aiEpoch(id)
            keepAwake.start(id)
            obj(
                "display_id" to id,
                "generation" to UUID.randomUUID().toString(),
                "own_focus" to true,
                "steals_main_focus" to false,
                "width" to width,
                "height" to height
            )
        } catch (e: Exception) {
            display.release()
            throw e
        }
    }

    override fun releaseDisplay(displayId: Int): String = report {
        keepAwake.stop(displayId)
        cancelAiInputInternal(displayId)
        cancelHumanInputInternal(displayId)
        parkTasksOnDisplay(displayId)
        aiCancelEpochs.remove(displayId)
        displays.remove(displayId)?.let {
            val surf = it.surface
            try { it.release() } finally { surf?.release() }
        }
        obj("released" to true)
    }

    override fun pokeKeepAwake(displayId: Int) {
        if (Binder.getCallingUid() != appUid || closed) return
        keepAwake.poke(displayId)
    }

    private fun owned(displayId: Int): VirtualDisplay = displays[displayId] ?: throw AgentError("unknown_background_view")

    private fun userIdOf(handle: UserHandle): Int =
        runCatching { UserHandle::class.java.getMethod("getIdentifier").invoke(handle) as Int }
            .getOrElse { handle.hashCode() }

    private fun userHandleFor(userId: Int): UserHandle? =
        runCatching { UserHandle::class.java.getMethod("of", Int::class.javaPrimitiveType).invoke(null, userId) as UserHandle }
            .getOrNull()

    @SuppressLint("PrivateApi")
    private fun unlockedUserHandles(): List<Pair<Int, UserHandle>> {
        val um = context.getSystemService(UserManager::class.java) ?: return emptyList()
        val seen = LinkedHashMap<Int, UserHandle>()
        for (h in um.userProfiles) {
            seen[userIdOf(h)] = h
        }
        // Also probe UserManager.getProfiles(userId, enabledOnly) for OEM clone profiles (e.g. 999).
        runCatching {
            val baseUser = appUid / 100000
            val m = UserManager::class.java.methods.firstOrNull {
                it.name == "getProfiles" && it.parameterTypes.size == 1 && it.parameterTypes[0] == Int::class.javaPrimitiveType
            }
            val infos = m?.invoke(um, baseUser) as? List<*>
            infos?.filterNotNull()?.forEach { info ->
                val id = info.javaClass.getField("id").getInt(info)
                if (id !in seen) userHandleFor(id)?.let { seen[id] = it }
            }
        }
        return seen.entries.mapNotNull { (id, handle) ->
            val unlocked = runCatching { um.isUserUnlocked(handle) }.getOrDefault(id == appUid / 100000)
            val quiet = runCatching { um.isQuietModeEnabled(handle) }.getOrDefault(false)
            if (unlocked && !quiet) id to handle else null
        }
    }

    override fun listUsers(): String = report {
        val um = context.getSystemService(UserManager::class.java)
        val baseUser = appUid / 100000
        val profiles = unlockedUserHandles().map { (id, handle) ->
            val managed = runCatching {
                UserManager::class.java.getMethod("isManagedProfile", Int::class.javaPrimitiveType).invoke(um, id) as Boolean
            }.getOrDefault(false)
            obj(
                "user_id" to id,
                "unlocked" to true,
                "owner" to (id == baseUser),
                "managed_profile" to managed,
                "quiet_mode" to runCatching { um?.isQuietModeEnabled(handle) ?: false }.getOrDefault(false)
            )
        }
        obj("users" to arr(profiles))
    }

    private fun resolveLaunchIntent(packageName: String, userId: Int, handle: UserHandle): Intent? {
        val la = context.getSystemService(LauncherApps::class.java)
        val activities = runCatching { la?.getActivityList(packageName, handle) }.getOrNull()
        if (!activities.isNullOrEmpty()) {
            return Intent(Intent.ACTION_MAIN)
                .addCategory(Intent.CATEGORY_LAUNCHER)
                .setComponent(activities[0].componentName)
        }
        val baseQuery = Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER).setPackage(packageName)
        val resolved = runCatching {
            val m = context.packageManager.javaClass.methods.firstOrNull {
                it.name == "queryIntentActivitiesAsUser" && it.parameterTypes.size == 3 &&
                    it.parameterTypes[0] == Intent::class.java &&
                    it.parameterTypes[1] == Int::class.javaPrimitiveType &&
                    it.parameterTypes[2] == Int::class.javaPrimitiveType
            }
            @Suppress("UNCHECKED_CAST")
            m?.invoke(context.packageManager, baseQuery, 0, userId) as? List<android.content.pm.ResolveInfo>
        }.getOrNull()
        if (!resolved.isNullOrEmpty()) {
            val ai = resolved[0].activityInfo
            return Intent(Intent.ACTION_MAIN)
                .addCategory(Intent.CATEGORY_LAUNCHER)
                .setComponent(ComponentName(ai.packageName, ai.name))
        }
        return if (userId == appUid / 100000) context.packageManager.getLaunchIntentForPackage(packageName) else null
    }

    private data class TaskSnapshot(
        val taskId: Int,
        val displayId: Int,
        val userId: Int,
        val topPackage: String?,
        val basePackage: String?,
        val isVisible: Boolean
    )

    private fun queryTasks(displayFilter: Int = -1): List<TaskSnapshot> {
        val methods = atmService.javaClass.methods.filter { it.name == "getTasks" }
        val method = methods.firstOrNull { it.parameterTypes.size == 4 } ?: methods.first { it.parameterTypes.size == 3 }
        val raw = (if (method.parameterTypes.size == 4) {
            method.invoke(atmService, 100, false, false, displayFilter)
        } else {
            method.invoke(atmService, 100, false, false)
        }) as? List<*> ?: return emptyList()
        val info = Class.forName("android.app.TaskInfo")
        val taskIdField = runCatching { info.getField("taskId") }.getOrElse { info.getField("id") }
        val displayField = info.getField("displayId")
        val userField = info.getField("userId")
        val topField = info.getField("topActivity")
        val baseField = info.getField("baseActivity")
        val visibleField = runCatching { info.getField("isVisible") }.getOrNull()
        val out = ArrayList<TaskSnapshot>(raw.size)
        for ((index, task) in raw.withIndex()) {
            if (task == null) continue
            val dId = displayField.getInt(task)
            if (displayFilter >= 0 && dId != displayFilter) continue
            out.add(
                TaskSnapshot(
                    taskId = taskIdField.getInt(task),
                    displayId = dId,
                    userId = userField.getInt(task),
                    topPackage = (topField.get(task) as? ComponentName)?.packageName,
                    basePackage = (baseField.get(task) as? ComponentName)?.packageName,
                    isVisible = visibleField?.getBoolean(task) ?: (index == 0)
                )
            )
        }
        return out
    }

    private fun queryMainTopTaskId(): Int {
        val mainTasks = queryTasks(Display.DEFAULT_DISPLAY)
        return mainTasks.firstOrNull()?.taskId ?: -1
    }

    private fun moveTaskToDisplay(taskId: Int, displayId: Int): Boolean {
        val moveRoot = atmService.javaClass.methods.firstOrNull {
            it.name == "moveRootTaskToDisplay" && it.parameterTypes.size == 2
        }
        if (moveRoot != null) {
            return runCatching { moveRoot.invoke(atmService, taskId, displayId); true }.getOrDefault(false)
        }
        val moveStack = atmService.javaClass.methods.firstOrNull {
            it.name == "moveStackToDisplay" && it.parameterTypes.size == 2
        }
        if (moveStack != null) {
            return runCatching { moveStack.invoke(atmService, taskId, displayId); true }.getOrDefault(false)
        }
        return false
    }

    private fun parkTasksOnDisplay(displayId: Int) {
        val tasks = runCatching { queryTasks(displayId) }.getOrDefault(emptyList())
        if (tasks.isEmpty()) return
        val removeTask = atmService.javaClass.methods.firstOrNull {
            it.name == "removeTask" && it.parameterTypes.size == 1 && it.parameterTypes[0] == Int::class.javaPrimitiveType
        }
        for (task in tasks) {
            if (task.taskId > 0 && removeTask != null) {
                runCatching { removeTask.invoke(atmService, task.taskId) }
            }
        }
    }

    private fun pullSpilledTasksInternal(displayId: Int, packageName: String, preMainTopTaskId: Int): List<Int> {
        if (packageName.isBlank()) return emptyList()
        val mainTasks = runCatching { queryTasks(Display.DEFAULT_DISPLAY) }.getOrDefault(emptyList())
        val moved = mutableListOf<Int>()
        for (task in mainTasks) {
            if (task.taskId <= 0 || task.taskId == preMainTopTaskId) break
            val topPkg = task.topPackage
            val basePkg = task.basePackage
            if (UiContract.isSpillExcludedPackage(topPkg) || (basePkg != null && UiContract.isSpillExcludedPackage(basePkg))) {
                continue
            }
            if (topPkg != packageName && basePkg != packageName) {
                continue
            }
            if (moveTaskToDisplay(task.taskId, displayId)) {
                moved.add(task.taskId)
            }
        }
        return moved
    }

    override fun mainTopTaskId(): Int {
        return runCatching {
            onHandler { queryMainTopTaskId() }
        }.getOrDefault(-1)
    }

    override fun pullSpilledTasks(displayId: Int, packageName: String, preMainTopTaskId: Int): String = report {
        owned(displayId)
        if (packageName.isBlank()) return@report obj("pulled_count" to 0, "moved_task_ids" to arr(emptyList<Int>()))
        val pkg = Policy.packageName(packageName)
        val moved = pullSpilledTasksInternal(displayId, pkg, preMainTopTaskId)
        obj("pulled_count" to moved.size, "moved_task_ids" to arr(moved))
    }

    override fun displayTaskInfo(displayId: Int): String = report {
        owned(displayId)
        val top = queryTasks(displayId).firstOrNull()
        obj(
            "package" to (top?.topPackage ?: top?.basePackage ?: ""),
            "user_id" to (top?.userId ?: (appUid / 100000)),
            "task_id" to (top?.taskId ?: -1)
        )
    }

    override fun launchApp(displayId: Int, packageName: String): String =
        launchAppAsUser(displayId, packageName, -1)

    override fun launchAppAsUser(displayId: Int, packageName: String, userId: Int): String = report {
        owned(displayId)
        keepAwake.poke(displayId)
        val name = Policy.packageName(packageName)
        val unlocked = unlockedUserHandles()
        val baseUser = appUid / 100000

        var chosenUserId: Int
        var launch: Intent?
        if (userId >= 0) {
            val pair = unlocked.firstOrNull { it.first == userId }
                ?: throw AgentError("user_profile_locked_or_unavailable")
            chosenUserId = pair.first
            launch = resolveLaunchIntent(name, chosenUserId, pair.second)
                ?: throw AgentError("launch_activity_unavailable")
        } else {
            val primary = unlocked.firstOrNull { it.first == baseUser }
            val primaryIntent = primary?.let { resolveLaunchIntent(name, it.first, it.second) }
            if (primary != null && primaryIntent != null) {
                chosenUserId = primary.first
                launch = primaryIntent
            } else {
                val fallback = unlocked.firstNotNullOfOrNull { (id, handle) ->
                    resolveLaunchIntent(name, id, handle)?.let { id to it }
                } ?: throw AgentError("launch_activity_unavailable")
                chosenUserId = fallback.first
                launch = fallback.second
            }
        }

        // Check main-screen occupancy strictly by (packageName, userId) so clone / work-profile
        // instances remain usable on the background display even if another user profile uses the app on main.
        val mainTasks = queryTasks(Display.DEFAULT_DISPLAY)
        demand(
            !mainTasks.any { task ->
                task.userId == chosenUserId &&
                    (task.topPackage == name || task.basePackage == name) &&
                    task.isVisible
            },
            "app_in_use_on_main",
            "这个应用正在主屏使用；请在主屏继续操作，避免将用户的界面移到后台。"
        )

        val preMainTop = mainTasks.firstOrNull()?.taskId ?: -1
        val options = ActivityOptions.makeBasic().setLaunchDisplayId(displayId).toBundle()
        // ContextWrapper.startActivity delegates to the base system context's
        // package. Use the actual shell UID/package, as in Mika's shell adapter.
        val start = atmService.javaClass.methods.first { it.name == "startActivityAsUser" && it.parameterTypes.size == 12 }
        val result = start.invoke(
            atmService, null, ShellUiContext.SHELL_PACKAGE, null,
            launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK), null, null, null, 0, 0, null, options, chosenUserId
        ) as Int
        demand(result >= 0, "app_launch_rejected")
        SystemClock.sleep(80L)
        val pulled = pullSpilledTasksInternal(displayId, name, preMainTop)
        obj("accepted" to true, "verified" to false, "display_id" to displayId, "user_id" to chosenUserId, "pulled_spilled" to arr(pulled))
    }

    override fun key(displayId: Int, keyCode: Int, modifiers: Int): String = report {
        owned(displayId)
        keepAwake.poke(displayId)
        demand(keyCode in 0..KeyEvent.getMaxKeyCode(), "unsupported_key")
        val epoch = aiEpoch(displayId)
        val gen = epoch.get()
        val at = SystemClock.uptimeMillis()
        fun event(action: Int, flags: Int = 0) =
            KeyEvent(at, SystemClock.uptimeMillis(), action, keyCode, 0, modifiers, -1, 0, flags, InputDevice.SOURCE_KEYBOARD)
        var sent = false
        val lock = inputLock(displayId)
        try {
            val down = event(KeyEvent.ACTION_DOWN)
            val downOk = synchronized(lock) {
                if (closed || !displays.containsKey(displayId) || epoch.get() != gen) {
                    throw AgentError("takeover_by_human")
                }
                activeAiKeys[displayId] = down
                sendInput(displayId, down, 2)
            }
            demand(downOk, "key_injection_rejected")
            sent = true
        } finally {
            var canceled = false
            var released = true
            synchronized(lock) {
                canceled = epoch.get() != gen
                val stillOwned = activeAiKeys.remove(displayId) != null
                if (stillOwned) {
                    released = runCatching {
                        sendInput(displayId, event(KeyEvent.ACTION_UP, if (canceled) KeyEvent.FLAG_CANCELED else 0), 2)
                    }.getOrDefault(false)
                }
            }
            if (canceled) throw AgentError("takeover_by_human")
            if (!released || !sent) throw AgentError("key_outcome_unknown")
        }
        obj("accepted" to true, "verified" to false)
    }

    override fun motion(displayId: Int, kind: String, x: Float, y: Float, toX: Float, toY: Float, scale: Double): String {
        checkCaller()
        val dims = onHandler {
            val display = owned(displayId).display
            keepAwake.poke(displayId)
            val metrics = android.util.DisplayMetrics()
            @Suppress("DEPRECATION")
            display.getRealMetrics(metrics)
            metrics.widthPixels to metrics.heightPixels
        }
        val width = dims.first
        val height = dims.second
        fun validPoint(px: Float, py: Float) = px.isFinite() && py.isFinite() && px >= 0f && py >= 0f && px <= width - 1f && py <= height - 1f
        if (!validPoint(x, y) || !validPoint(toX, toY)) return Policy.error(AgentError("invalid_coordinates")).toString()

        val epoch = aiEpoch(displayId)
        val startGen = epoch.get()
        val lock = inputLock(displayId)

        fun ensureNotCanceled() {
            if (closed || !displays.containsKey(displayId) || epoch.get() != startGen) {
                throw AgentError("takeover_by_human")
            }
        }

        fun sendAi(ev: MotionEvent): Boolean {
            return try {
                synchronized(lock) {
                    ensureNotCanceled()
                    if (ev.actionMasked == MotionEvent.ACTION_UP || ev.actionMasked == MotionEvent.ACTION_CANCEL) {
                        activeAiMotion.remove(displayId)?.recycle()
                    } else {
                        activeAiMotion.put(displayId, MotionEvent.obtain(ev))?.recycle()
                    }
                    sendInput(displayId, ev, 2)
                }
            } finally {
                ev.recycle()
            }
        }

        fun cancelAiStrokeIfStillOwned(ev: MotionEvent) {
            try {
                synchronized(lock) {
                    if (epoch.get() == startGen && activeAiMotion.remove(displayId) != null) {
                        runCatching { sendInput(displayId, ev, 2) }
                    }
                }
            } finally {
                ev.recycle()
            }
        }

        fun sleepInterruptibly(ms: Long) {
            var remaining = ms
            while (remaining > 0) {
                ensureNotCanceled()
                val slice = minOf(25L, remaining)
                SystemClock.sleep(slice)
                remaining -= slice
            }
            ensureNotCanceled()
        }

        fun singleStroke(x1: Float, y1: Float, x2: Float, y2: Float, durationMs: Long, steps: Int) {
            ensureNotCanceled()
            val downAt = SystemClock.uptimeMillis()
            var downSent = false
            var upSent = false
            try {
                demand(
                    sendAi(
                        MotionEvent.obtain(downAt, downAt, MotionEvent.ACTION_DOWN, x1, y1, 1f, 1f, 0, 1f, 1f, 0, 0).apply {
                            source = InputDevice.SOURCE_TOUCHSCREEN
                        }
                    ),
                    "gesture_rejected"
                )
                downSent = true
                val stepCount = steps.coerceAtLeast(1)
                val stepSleep = (durationMs / stepCount).coerceAtLeast(1L)
                for (i in 1..stepCount) {
                    sleepInterruptibly(stepSleep)
                    if (x1 != x2 || y1 != y2 || durationMs >= 300L) {
                        val t = i.toFloat() / stepCount.toFloat()
                        val mx = x1 + (x2 - x1) * t
                        val my = y1 + (y2 - y1) * t
                        val now = SystemClock.uptimeMillis()
                        demand(
                            sendAi(
                                MotionEvent.obtain(downAt, now, MotionEvent.ACTION_MOVE, mx, my, 1f, 1f, 0, 1f, 1f, 0, 0).apply {
                                    source = InputDevice.SOURCE_TOUCHSCREEN
                                }
                            ),
                            "gesture_outcome_unknown"
                        )
                    }
                }
                ensureNotCanceled()
                val upAt = SystemClock.uptimeMillis()
                demand(
                    sendAi(
                        MotionEvent.obtain(downAt, upAt, MotionEvent.ACTION_UP, x2, y2, 0f, 1f, 0, 1f, 1f, 0, 0).apply {
                            source = InputDevice.SOURCE_TOUCHSCREEN
                        }
                    ),
                    "gesture_outcome_unknown"
                )
                upSent = true
            } finally {
                if (downSent && !upSent) {
                    val now = SystemClock.uptimeMillis()
                    cancelAiStrokeIfStillOwned(
                        MotionEvent.obtain(downAt, now, MotionEvent.ACTION_CANCEL, x2, y2, 0f, 1f, 0, 1f, 1f, 0, 0).apply {
                            source = InputDevice.SOURCE_TOUCHSCREEN
                        }
                    )
                }
            }
        }

        val future = FutureTask<JSONObject> {
            when (kind) {
                "click" -> singleStroke(x, y, x, y, 45L, 1)
                "double_click" -> {
                    singleStroke(x, y, x, y, 40L, 1)
                    sleepInterruptibly(75L)
                    singleStroke(x, y, x, y, 40L, 1)
                }
                "triple_click" -> {
                    singleStroke(x, y, x, y, 40L, 1)
                    sleepInterruptibly(65L)
                    singleStroke(x, y, x, y, 40L, 1)
                    sleepInterruptibly(65L)
                    singleStroke(x, y, x, y, 40L, 1)
                }
                "long_press" -> singleStroke(x, y, x, y, 650L, 10)
                "drag" -> singleStroke(x, y, toX, toY, 320L, 12)
                "pinch" -> {
                    demand(scale.isFinite() && scale > 0.0, "invalid_pinch_scale")
                    val radius = minOf(x, y, width - 1f - x, height - 1f - y) * 0.5f / maxOf(1.0, scale).toFloat()
                    demand(radius >= 1f, "pinch_outside_display")
                    val props = arrayOf(
                        MotionEvent.PointerProperties().apply { id = 0; toolType = MotionEvent.TOOL_TYPE_FINGER },
                        MotionEvent.PointerProperties().apply { id = 1; toolType = MotionEvent.TOOL_TYPE_FINGER }
                    )
                    fun coords(lx: Float, rx: Float) = arrayOf(
                        MotionEvent.PointerCoords().apply { this.x = lx; this.y = y; pressure = 1f; size = 1f },
                        MotionEvent.PointerCoords().apply { this.x = rx; this.y = y; pressure = 1f; size = 1f }
                    )
                    val downAt = SystemClock.uptimeMillis()
                    var downSent = false
                    var upSent = false
                    try {
                        val c0 = coords(x - radius, x + radius)
                        demand(sendAi(MotionEvent.obtain(downAt, downAt, MotionEvent.ACTION_DOWN, 1, props, c0, 0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0)), "gesture_rejected")
                        downSent = true
                        val ptrDown = MotionEvent.ACTION_POINTER_DOWN or (1 shl MotionEvent.ACTION_POINTER_INDEX_SHIFT)
                        demand(sendAi(MotionEvent.obtain(downAt, SystemClock.uptimeMillis(), ptrDown, 2, props, c0, 0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0)), "gesture_outcome_unknown")
                        for (step in 1..10) {
                            sleepInterruptibly(35L)
                            val curR = radius + (radius * scale.toFloat() - radius) * (step / 10f)
                            val cm = coords(x - curR, x + curR)
                            demand(sendAi(MotionEvent.obtain(downAt, SystemClock.uptimeMillis(), MotionEvent.ACTION_MOVE, 2, props, cm, 0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0)), "gesture_outcome_unknown")
                        }
                        ensureNotCanceled()
                        val cf = coords(x - radius * scale.toFloat(), x + radius * scale.toFloat())
                        val ptrUp = MotionEvent.ACTION_POINTER_UP or (1 shl MotionEvent.ACTION_POINTER_INDEX_SHIFT)
                        demand(sendAi(MotionEvent.obtain(downAt, SystemClock.uptimeMillis(), ptrUp, 2, props, cf, 0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0)), "gesture_outcome_unknown")
                        demand(sendAi(MotionEvent.obtain(downAt, SystemClock.uptimeMillis(), MotionEvent.ACTION_UP, 1, props, cf, 0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0)), "gesture_outcome_unknown")
                        upSent = true
                    } finally {
                        if (downSent && !upSent) {
                            val cCancel = coords(x, x)
                            cancelAiStrokeIfStillOwned(
                                MotionEvent.obtain(downAt, SystemClock.uptimeMillis(), MotionEvent.ACTION_CANCEL, 1, props, cCancel, 0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0)
                            )
                        }
                    }
                }
                else -> throw AgentError("unsupported_action")
            }
            obj("gesture_completed" to true, "verified" to false)
        }
        aiMotionExecutor.execute(future)
        return try { future.get(10, TimeUnit.SECONDS).toString() }
        catch (e: Exception) { future.cancel(true); Policy.error(e).toString() }
    }

    override fun injectMotionEvent(displayId: Int, event: MotionEvent?): Boolean {
        if (event == null) return false
        return try {
            onHandler {
                val display = owned(displayId).display
                keepAwake.poke(displayId)
                val metrics = android.util.DisplayMetrics()
                @Suppress("DEPRECATION")
                display.getRealMetrics(metrics)
                require(event.pointerCount in 1..10)
                for (i in 0 until event.pointerCount) {
                    require(
                        event.getX(i).isFinite() && event.getY(i).isFinite() &&
                            event.getX(i) in 0f..(metrics.widthPixels - 1).toFloat() &&
                            event.getY(i) in 0f..(metrics.heightPixels - 1).toFloat()
                    )
                }
                synchronized(inputLock(displayId)) {
                    val ok = sendInput(displayId, event, 1)
                    if (ok) {
                        activeHumanMotion.remove(displayId)?.recycle()
                        if (event.actionMasked != MotionEvent.ACTION_UP && event.actionMasked != MotionEvent.ACTION_CANCEL) {
                            activeHumanMotion[displayId] = MotionEvent.obtain(event)
                        }
                    }
                    ok
                }
            }
        } catch (_: Exception) {
            false
        }
    }

    override fun injectKeyEvent(displayId: Int, event: KeyEvent?): Boolean {
        if (event == null) return false
        return try {
            onHandler {
                owned(displayId)
                keepAwake.poke(displayId)
                require(event.keyCode in 0..KeyEvent.getMaxKeyCode())
                synchronized(inputLock(displayId)) {
                    val ok = sendInput(displayId, event, 1)
                    if (ok) {
                        val map = activeHumanKeys.computeIfAbsent(displayId) { ConcurrentHashMap() }
                        if (event.action == KeyEvent.ACTION_DOWN) map[event.keyCode] = KeyEvent(event)
                        else map.remove(event.keyCode)
                    }
                    ok
                }
            }
        } catch (_: Exception) {
            false
        }
    }

    override fun cancelAiInput(displayId: Int) {
        if (Binder.getCallingUid() != appUid || closed) return
        synchronized(inputLock(displayId)) {
            aiEpoch(displayId).incrementAndGet()
            cancelAiInputInternal(displayId)
        }
    }

    override fun cancelHumanInput(displayId: Int) {
        if (Binder.getCallingUid() != appUid || closed) return
        synchronized(inputLock(displayId)) {
            cancelHumanInputInternal(displayId)
        }
    }

    private fun cancelAiInputInternal(displayId: Int) {
        activeAiMotion.remove(displayId)?.let { ev ->
            try {
                val cancel = cloneAsCancelMotion(ev, SystemClock.uptimeMillis())
                try { runCatching { sendInput(displayId, cancel, 1) } } finally { cancel.recycle() }
            } finally { ev.recycle() }
        }
        activeAiKeys.remove(displayId)?.let { ev ->
            val now = SystemClock.uptimeMillis()
            val up = KeyEvent(ev.downTime, now, KeyEvent.ACTION_UP, ev.keyCode, 0, ev.metaState, ev.deviceId, ev.scanCode, ev.flags or KeyEvent.FLAG_CANCELED, ev.source)
            runCatching { sendInput(displayId, up, 1) }
        }
    }

    private fun cancelHumanInputInternal(displayId: Int) {
        activeHumanMotion.remove(displayId)?.let { ev ->
            try {
                val cancel = cloneAsCancelMotion(ev, SystemClock.uptimeMillis())
                try { runCatching { sendInput(displayId, cancel, 1) } } finally { cancel.recycle() }
            } finally { ev.recycle() }
        }
        activeHumanKeys.remove(displayId)?.values?.forEach { ev ->
            val now = SystemClock.uptimeMillis()
            val up = KeyEvent(ev.downTime, now, KeyEvent.ACTION_UP, ev.keyCode, 0, ev.metaState, ev.deviceId, ev.scanCode, ev.flags or KeyEvent.FLAG_CANCELED, ev.source)
            runCatching { sendInput(displayId, up, 1) }
        }
    }

    private fun cloneAsCancelMotion(event: MotionEvent, now: Long): MotionEvent {
        val props = Array(event.pointerCount) { idx -> MotionEvent.PointerProperties().also { event.getPointerProperties(idx, it) } }
        val coords = Array(event.pointerCount) { idx -> MotionEvent.PointerCoords().also { event.getPointerCoords(idx, it) } }
        return MotionEvent.obtain(
            event.downTime, now, MotionEvent.ACTION_CANCEL, event.pointerCount, props, coords,
            event.metaState, event.buttonState, event.xPrecision, event.yPrecision,
            event.deviceId, event.edgeFlags, event.source, event.flags
        )
    }

    override fun close() {
        closed = true
        keepAwake.close()
        val task = FutureTask {
            for (id in displays.keys.toList()) {
                cancelAiInputInternal(id)
                cancelHumanInputInternal(id)
                parkTasksOnDisplay(id)
            }
            displays.values.forEach {
                val surf = it.surface
                runCatching { it.release() }
                runCatching { surf?.release() }
            }
            displays.clear()
            aiCancelEpochs.clear()
        }
        if (Looper.myLooper() == thread.looper) task.run() else handler.post(task)
        runCatching { task.get(3, TimeUnit.SECONDS) }
        aiMotionExecutor.shutdownNow()
        thread.quitSafely()
    }
}
