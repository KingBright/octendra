package `fun`.hackerlife.octendra

import android.content.Context
import android.graphics.Bitmap
import android.hardware.display.DisplayManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.SystemClock
import android.view.Display
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import org.json.JSONObject

/**
 * One logical AI view, owned by the existing shell helper and local app UID.
 * AiSurfaceHub drains VirtualDisplay frames directly on the GPU for zero-copy streaming;
 * CPU readback happens only on explicit sparse ui_observe snapshot() calls.
 */
class BackgroundView(private val context: Context) : AutoCloseable {
    private val thread = HandlerThread("octendra-ui-frames").apply { start() }
    private val handler = Handler(thread.looper)
    private var hub: AiSurfaceHub? = null
    private var shell: IUiShell? = null
    private var descriptor: JSONObject? = null
    private var displayListener: DisplayManager.DisplayListener? = null
    @Volatile private var lastLaunchedPackage: String = ""
    @Volatile private var lastLaunchedUserId: Int = 0
    @Volatile private var closed = false

    val frameSequence: Long get() = hub?.frameSequence ?: 0L
    val displayId: Int get() = descriptor?.getInt("display_id") ?: throw AgentError("background_view_lost")
    val generation: String get() = descriptor?.getString("generation") ?: throw AgentError("background_view_lost")

    fun available() = Build.VERSION.SDK_INT >= 34 && AgentRuntime.bridge?.ui() != null

    fun current(view: String, generation: String): Boolean = descriptor != null && view == "display:$displayId" &&
        generation == this.generation && shell?.asBinder()?.isBinderAlive == true &&
        shell?.asBinder() == AgentRuntime.bridge?.ui()?.asBinder()

    @Suppress("DEPRECATION")
    fun ensure(): Int {
        demand(!closed && Build.VERSION.SDK_INT >= 34, "background_display_unsupported")
        val service = AgentRuntime.bridge?.awaitUi()
            ?: throw AgentError("shell_ui_activation_required", "当前 shell 辅助进程尚未提供后台显示能力。")
        if (descriptor != null && service.asBinder() == shell?.asBinder()) return displayId
        release()
        val displayManager = context.getSystemService(DisplayManager::class.java)
        val display = displayManager.getDisplay(Display.DEFAULT_DISPLAY)
        val metrics = android.util.DisplayMetrics(); display.getRealMetrics(metrics)
        // Normalize ROTATION_90 / ROTATION_270 main-screen metrics back to upright ROTATION_0
        // so creating a background AI VirtualDisplay while watching landscape video does not
        // permanently lock the virtual display into sideways landscape geometry.
        val rotated = display.rotation == Surface.ROTATION_90 || display.rotation == Surface.ROTATION_270
        val targetWidth = if (rotated) metrics.heightPixels else metrics.widthPixels
        val targetHeight = if (rotated) metrics.widthPixels else metrics.heightPixels

        val surfaceHub = AiSurfaceHub(targetWidth, targetHeight)
        hub = surfaceHub
        shell = service
        try {
            val created = JSONObject(service.createDisplay(surfaceHub.inputSurface, targetWidth, targetHeight, metrics.densityDpi))
            if (created.has("error")) throw AgentError(created.getString("error"), created.optString("message", created.getString("error")))
            demand(created.optBoolean("own_focus") && !created.optBoolean("steals_main_focus", true), "background_focus_unavailable")
            descriptor = created
            val dId = displayId
            val gen = generation
            val viewId = "display:$dId"
            registerHost(viewId, dId, gen, targetWidth, targetHeight, metrics.densityDpi, surfaceHub, service)
            val listener = object : DisplayManager.DisplayListener {
                override fun onDisplayAdded(id: Int) = Unit
                override fun onDisplayRemoved(id: Int) {
                    if (id == dId) AiViewHost.shared.unregister(viewId, gen)
                }
                override fun onDisplayChanged(id: Int) {
                    if (id != dId || closed) return
                    val d = displayManager.getDisplay(dId) ?: return
                    val m = android.util.DisplayMetrics(); d.getRealMetrics(m)
                    if (m.widthPixels > 1 && m.heightPixels > 1) {
                        surfaceHub.updateSize(m.widthPixels, m.heightPixels)
                        // AiSurfaceHub renders via SurfaceTexture.getTransformMatrix into upright (w, h),
                        // so stream rotation relative to (width, height) remains Surface.ROTATION_0 (0).
                        AiViewHost.shared.updateGeometry(viewId, gen, m.widthPixels, m.heightPixels, m.densityDpi, Surface.ROTATION_0)
                    }
                }
            }
            displayListener = listener
            displayManager.registerDisplayListener(listener, handler)
            return dId
        } catch (e: Exception) {
            release()
            throw e
        }
    }

    private fun registerHost(
        viewId: String,
        dId: Int,
        gen: String,
        width: Int,
        height: Int,
        density: Int,
        surfaceHub: AiSurfaceHub,
        service: IUiShell
    ) {
        val surfaceBridge = object : AiViewHost.SurfaceBridge {
            override fun attachSurface(surface: Any): String = surfaceHub.attachConsumer(surface as Surface)
            override fun detachSurface(consumerId: String) = surfaceHub.detachConsumer(consumerId)
        }
        val inputSink = object : AiInputSink {
            override fun injectTouch(
                displayId: Int,
                downTimeMs: Long,
                eventTimeMs: Long,
                action: Int,
                pointerCount: Int,
                pointerIds: IntArray,
                xs: FloatArray,
                ys: FloatArray,
                pressures: FloatArray
            ): Boolean {
                val props = Array(pointerCount) { idx ->
                    MotionEvent.PointerProperties().apply {
                        id = pointerIds[idx]
                        toolType = MotionEvent.TOOL_TYPE_FINGER
                    }
                }
                val coords = Array(pointerCount) { idx ->
                    MotionEvent.PointerCoords().apply {
                        x = xs[idx]
                        y = ys[idx]
                        pressure = pressures[idx]
                        size = 1f
                    }
                }
                val ev = MotionEvent.obtain(
                    downTimeMs, eventTimeMs, action, pointerCount, props, coords,
                    0, 0, 1f, 1f, 0, 0, InputDevice.SOURCE_TOUCHSCREEN, 0
                )
                return try {
                    service.injectMotionEvent(displayId, ev)
                } finally {
                    ev.recycle()
                }
            }

            override fun injectKey(
                displayId: Int,
                downTimeMs: Long,
                eventTimeMs: Long,
                action: Int,
                androidKeyCode: Int,
                metaState: Int,
                flags: Int
            ): Boolean {
                val ev = KeyEvent(
                    downTimeMs, eventTimeMs, action, androidKeyCode, 0, metaState,
                    -1, 0, flags, InputDevice.SOURCE_KEYBOARD
                )
                return service.injectKeyEvent(displayId, ev)
            }

            override fun commitText(displayId: Int, text: String): Boolean =
                AccessService.instance?.commitTextOnDisplay(displayId, text) == true

            override fun uptimeMs(): Long = SystemClock.uptimeMillis()
        }
        val resolveAppInfo = {
            var pkg = AccessService.instance?.foregroundPackage(dId).orEmpty()
            var uId = lastLaunchedUserId
            runCatching {
                val info = JSONObject(service.displayTaskInfo(dId))
                val taskPkg = info.optString("package", "")
                if (pkg.isBlank() && taskPkg.isNotBlank()) pkg = taskPkg
                if (info.has("user_id")) uId = info.getInt("user_id")
            }
            if (pkg.isBlank()) pkg = lastLaunchedPackage
            val label = if (pkg.isNotBlank()) {
                runCatching {
                    val ai = context.packageManager.getApplicationInfo(pkg, 0)
                    context.packageManager.getApplicationLabel(ai).toString()
                }.getOrDefault(pkg)
            } else {
                "AI 工作区"
            }
            AiViewHost.ViewAppInfo(pkg, label, uId)
        }
        AiViewHost.shared.register(
            AiViewHost.ActiveView(
                viewId = viewId,
                displayId = dId,
                generation = gen,
                width = width,
                height = height,
                density = density,
                rotation = Surface.ROTATION_0,
                surfaceBridge = surfaceBridge,
                inputSink = inputSink,
                cancelAiInput = { runCatching { service.cancelAiInput(dId) } },
                invalidateAiObservations = { AccessService.instance?.invalidateDisplay(dId) },
                resolveAppInfo = resolveAppInfo,
                supportsInput = { service.asBinder().isBinderAlive }
            )
        )
    }

    fun awaitFrameAfter(previous: Long, timeoutMs: Long): Boolean =
        hub?.awaitFrameAfter(previous, timeoutMs) ?: false

    fun snapshot(): JSONObject? = snapshotWithTiles()?.first

    internal fun snapshotWithTiles(): Pair<JSONObject, List<Long>>? {
        val activeHub = hub ?: return null
        val bitmap = activeHub.snapshotBitmap(750L) ?: return null
        return try {
            val tiles = UiImage.tileHashes(bitmap)
            UiImage.encode(bitmap) to tiles
        } finally {
            bitmap.recycle()
        }
    }

    fun pokeKeepAwake() {
        val id = descriptor?.optInt("display_id", -1) ?: -1
        if (id >= 0) runCatching { shell?.pokeKeepAwake(id) }
    }

    fun mainTopTaskId(): Int = runCatching { shell?.mainTopTaskId() ?: -1 }.getOrDefault(-1)

    fun pullSpilledTasks(packageName: String, preMainTopTaskId: Int) {
        val id = descriptor?.optInt("display_id", -1) ?: -1
        if (id >= 0) runCatching { shell?.pullSpilledTasks(id, packageName, preMainTopTaskId) }
    }

    fun listUsers(): JSONObject = result { it.listUsers() }

    fun launch(app: String, userId: Int = -1): JSONObject {
        val res = result { it.launchAppAsUser(displayId, app, userId) }
        lastLaunchedPackage = app
        if (res.has("user_id")) lastLaunchedUserId = res.optInt("user_id", 0)
        return res
    }

    fun key(code: Int, modifiers: Int = 0): JSONObject = result { it.key(displayId, code, modifiers) }

    fun motion(kind: String, x: Double, y: Double, toX: Double = x, toY: Double = y, scale: Double = 1.0): JSONObject =
        result { it.motion(displayId, kind, x.toFloat(), y.toFloat(), toX.toFloat(), toY.toFloat(), scale) }

    private fun result(call: (IUiShell) -> String): JSONObject {
        val service = shell ?: throw AgentError("background_view_lost")
        val result = JSONObject(call(service))
        if (result.has("error")) throw AgentError(result.getString("error"), result.optString("message", result.getString("error")))
        return result
    }

    private fun release() {
        val id = descriptor?.optInt("display_id", -1) ?: -1
        val gen = descriptor?.optString("generation", "") ?: ""
        if (id >= 0) {
            AiViewHost.shared.unregister("display:$id", gen.ifEmpty { null })
        }
        displayListener?.let { listener ->
            runCatching { context.getSystemService(DisplayManager::class.java).unregisterDisplayListener(listener) }
        }
        displayListener = null
        if (id >= 0) runCatching { shell?.releaseDisplay(id) }
        descriptor = null
        shell = null
        lastLaunchedPackage = ""
        lastLaunchedUserId = 0
        hub?.close()
        hub = null
    }

    override fun close() {
        closed = true
        release()
        thread.quitSafely()
    }
}
