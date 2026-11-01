/* Background display flags and shell-context integration adapted from Mika
 * (wangzan101/Mika, Apache-2.0). See assets/licenses/mika/NOTICE and LICENSE.
 * Octendra modifications: existing UID-authenticated bridge, owned displays only,
 * independent focus required, no model/task framework or public listener. */
package `fun`.hackerlife.octendra

import android.app.ActivityOptions
import android.content.ComponentName
import android.content.Intent
import android.hardware.display.DisplayManager
import android.hardware.display.VirtualDisplay
import android.os.Binder
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.os.SystemClock
import android.view.Display
import android.view.InputDevice
import android.view.InputEvent
import android.view.KeyEvent
import android.view.Surface
import org.json.JSONObject
import java.util.UUID
import java.util.concurrent.FutureTask
import java.util.concurrent.TimeUnit

/** Runs in the already-authorized shell UID, never in an unprivileged app. */
@android.annotation.TargetApi(34)
internal class ShellUiService(private val appUid: Int) : IUiShell.Stub(), AutoCloseable {
    private val thread = HandlerThread("octendra-shell-ui").apply { start() }
    private val handler = Handler(thread.looper)
    private val context by lazy { ShellUiContext.create() }
    private val manager by lazy { DisplayManager::class.java.getDeclaredConstructor(android.content.Context::class.java)
        .apply { isAccessible = true }.newInstance(context) }
    private val displays = HashMap<Int,VirtualDisplay>()
    @Volatile private var closed = false
    private fun report(block: () -> JSONObject): String {
        if (Binder.getCallingUid() != appUid) throw SecurityException("app_peer_uid_mismatch")
        demand(!closed, "shell_ui_closed")
        val task = FutureTask { block() }
        handler.post(task)
        return try { task.get(10,TimeUnit.SECONDS).toString() }
        catch (e: Exception) { task.cancel(false); Policy.error(e).toString() }
    }
    @android.annotation.SuppressLint("WrongConstant") // Hidden SDK flags; actual own-focus flags are checked below.
    override fun createDisplay(surface: Surface, width: Int, height: Int, dpi: Int): String = report {
        demand(Build.VERSION.SDK_INT >= 34 && width in 1..4096 && height in 1..4096 && dpi in 72..1000, "background_display_unsupported")
        // Trusted + own focus + no top-focus stealing. Never fall back to a
        // display that could take focus away from the owner's main screen.
        val flags = (1 shl 0) or (1 shl 3) or (1 shl 6) or (1 shl 7) or (1 shl 8) or
            (1 shl 10) or (1 shl 11) or (1 shl 12) or (1 shl 13) or (1 shl 14) or (1 shl 16)
        val display = manager.createVirtualDisplay("Octendra AI",width,height,dpi,surface,flags)
            ?: throw AgentError("background_display_unavailable")
        try {
            val own = Display::class.java.getField("FLAG_OWN_FOCUS").getInt(null)
            val noSteal = Display::class.java.getField("FLAG_STEAL_TOP_FOCUS_DISABLED").getInt(null)
            demand(display.display.flags and own != 0 && display.display.flags and noSteal != 0, "background_focus_unavailable")
            val windows = Class.forName("android.view.WindowManagerGlobal").getMethod("getWindowManagerService").invoke(null)
            windows.javaClass.getMethod("setDisplayImePolicy",Int::class.javaPrimitiveType,Int::class.javaPrimitiveType)
                .invoke(windows,display.display.displayId,0)
            displays[display.display.displayId] = display
            obj("display_id" to display.display.displayId,"generation" to UUID.randomUUID().toString(),
                "own_focus" to true,"steals_main_focus" to false,"width" to width,"height" to height)
        } catch(e: Exception) { display.release(); throw e }
    }
    override fun releaseDisplay(displayId: Int): String = report {
        displays.remove(displayId)?.let { it.release(); it.surface?.release() }
        obj("released" to true)
    }
    private fun owned(displayId: Int): VirtualDisplay = displays[displayId] ?: throw AgentError("unknown_background_view")
    override fun launchApp(displayId: Int, packageName: String): String = report {
        owned(displayId); val name = Policy.packageName(packageName)
        val manager = Class.forName("android.app.ActivityTaskManager").getMethod("getService").invoke(null)
        val methods = manager.javaClass.methods.filter { it.name == "getTasks" }
        val method = methods.firstOrNull { it.parameterTypes.size == 4 } ?: methods.first { it.parameterTypes.size == 3 }
        val tasks = (if (method.parameterTypes.size == 4) method.invoke(manager,100,false,false,Display.DEFAULT_DISPLAY)
            else method.invoke(manager,100,false,false)) as List<*>
        val info = Class.forName("android.app.TaskInfo")
        val visible = runCatching { info.getField("isVisible") }.getOrNull()
        val mainTasks = tasks.filterNotNull().filter { info.getField("displayId").getInt(it) == Display.DEFAULT_DISPLAY }
        demand(!mainTasks.withIndex().any { (index,task) ->
            val pkg = (info.getField("topActivity").get(task) as? ComponentName)?.packageName
            val base = (info.getField("baseActivity").get(task) as? ComponentName)?.packageName
            info.getField("userId").getInt(task) == appUid/100000 && (pkg == name || base == name) &&
                (visible?.getBoolean(task) ?: (index == 0))
        }, "app_in_use_on_main", "这个应用正在主屏使用；请在主屏继续操作，避免将用户的界面移到后台。")
        val launch = context.packageManager.getLaunchIntentForPackage(name) ?: throw AgentError("launch_activity_unavailable")
        val options = ActivityOptions.makeBasic().setLaunchDisplayId(displayId).toBundle()
        // ContextWrapper.startActivity delegates to the base system context's
        // package. Use the actual shell UID/package, as in Mika's shell adapter.
        val start = manager.javaClass.methods.first { it.name == "startActivityAsUser" && it.parameterTypes.size == 12 }
        val result = start.invoke(manager,null,ShellUiContext.SHELL_PACKAGE,null,
            launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),null,null,null,0,0,null,options,appUid/100000) as Int
        demand(result >= 0,"app_launch_rejected")
        obj("accepted" to true,"verified" to false,"display_id" to displayId)
    }
    override fun key(displayId: Int, keyCode: Int, modifiers: Int): String = report {
        owned(displayId)
        demand(keyCode in 0..KeyEvent.getMaxKeyCode(), "unsupported_key")
        val type = runCatching { Class.forName("android.hardware.input.InputManagerGlobal") }
            .getOrElse { Class.forName("android.hardware.input.InputManager") }
        val input = type.getMethod("getInstance").invoke(null)
        val inject = type.getMethod("injectInputEvent",InputEvent::class.java,Int::class.javaPrimitiveType)
        val setDisplay = InputEvent::class.java.getMethod("setDisplayId",Int::class.javaPrimitiveType)
        val at = SystemClock.uptimeMillis()
        fun event(action: Int) = KeyEvent(at,at,action,keyCode,0,modifiers,-1,0,0,InputDevice.SOURCE_KEYBOARD).also { setDisplay.invoke(it,displayId) }
        var sent = false
        try {
            demand(inject.invoke(input,event(KeyEvent.ACTION_DOWN),2) == true,"key_injection_rejected")
            sent = true
        } finally {
            // Even a rejected/uncertain press may have reached the OS. Pair it.
            val released = runCatching { inject.invoke(input,event(KeyEvent.ACTION_UP),2) == true }.getOrDefault(false)
            if (!released || !sent) throw AgentError("key_outcome_unknown")
        }
        obj("accepted" to true,"verified" to false)
    }
    override fun close() {
        closed = true
        val task = FutureTask { displays.values.forEach { it.release(); it.surface?.release() }; displays.clear() }
        if (Looper.myLooper() == thread.looper) task.run() else handler.post(task)
        runCatching { task.get(3,TimeUnit.SECONDS) }; thread.quitSafely()
    }
}
