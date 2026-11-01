package `fun`.hackerlife.octendra

import android.content.Context
import android.graphics.Bitmap
import android.graphics.PixelFormat
import android.hardware.display.DisplayManager
import android.media.Image
import android.media.ImageReader
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.view.Display
import org.json.JSONObject
import java.nio.ByteBuffer

/** One logical AI view, owned by the existing shell helper and local app UID.
 * ImageReader drains native frames; CPU copies happen only on explicit observe.
 * No preview video or high-frequency pointer traffic passes through MCP. */
class BackgroundView(private val context: Context) : AutoCloseable {
    private val thread = HandlerThread("octendra-ui-frames").apply { start() }
    private val handler = Handler(thread.looper)
    private val frames = java.lang.Object()
    private var latest: Image? = null
    private var reader: ImageReader? = null
    private var shell: IUiShell? = null
    private var descriptor: JSONObject? = null
    private var closed = false
    val displayId: Int get() = descriptor?.getInt("display_id") ?: throw AgentError("background_view_lost")
    val generation: String get() = descriptor?.getString("generation") ?: throw AgentError("background_view_lost")
    fun available() = Build.VERSION.SDK_INT >= 34 && AgentRuntime.bridge?.ui() != null
    fun current(view: String, generation: String): Boolean = descriptor != null && view == "display:$displayId" &&
        generation == this.generation && shell?.asBinder()?.isBinderAlive == true && shell?.asBinder() == AgentRuntime.bridge?.ui()?.asBinder()
    fun ensure(): Int {
        demand(!closed && Build.VERSION.SDK_INT >= 34,"background_display_unsupported")
        val service = AgentRuntime.bridge?.awaitUi() ?: throw AgentError("shell_ui_activation_required","当前 shell 辅助进程尚未提供后台显示能力。")
        if (descriptor != null && service.asBinder() == shell?.asBinder()) return displayId
        release()
        val display = context.getSystemService(DisplayManager::class.java).getDisplay(Display.DEFAULT_DISPLAY)
        val metrics = android.util.DisplayMetrics(); display.getRealMetrics(metrics)
        val candidate = ImageReader.newInstance(metrics.widthPixels,metrics.heightPixels,PixelFormat.RGBA_8888,3)
        candidate.setOnImageAvailableListener({ source ->
            val image = runCatching { source.acquireLatestImage() }.getOrNull() ?: return@setOnImageAvailableListener
            synchronized(frames) {
                if (closed || reader !== source) image.close()
                else { latest?.close(); latest = image; frames.notifyAll() }
            }
        },handler)
        reader = candidate; shell = service
        try {
            val created = JSONObject(service.createDisplay(candidate.surface,metrics.widthPixels,metrics.heightPixels,metrics.densityDpi))
            if (created.has("error")) throw AgentError(created.getString("error"),created.optString("message",created.getString("error")))
            demand(created.optBoolean("own_focus") && !created.optBoolean("steals_main_focus",true),"background_focus_unavailable")
            descriptor = created
            return displayId
        } catch(e: Exception) { release(); throw e }
    }
    fun snapshot(): JSONObject? {
        val bitmap = synchronized(frames) {
            val end = android.os.SystemClock.elapsedRealtime()+2000
            while (latest == null && !closed && android.os.SystemClock.elapsedRealtime()<end) frames.wait(100)
            val image = latest ?: return null // A newly-created empty display has not rendered yet.
            val plane = image.planes[0]
            demand(plane.pixelStride == 4,"unsupported_ui_pixel_format")
            val strideWidth = plane.rowStride/4
            val padded = Bitmap.createBitmap(strideWidth,image.height,Bitmap.Config.ARGB_8888)
            try {
                val buffer = plane.buffer.duplicate().apply { rewind() }
                val required = strideWidth*image.height*4
                if (buffer.remaining() < required) {
                    val filled = ByteBuffer.allocate(required); filled.put(buffer); filled.rewind(); padded.copyPixelsFromBuffer(filled)
                } else padded.copyPixelsFromBuffer(buffer)
                Bitmap.createBitmap(padded,0,0,image.width,image.height).let { if (it === padded) padded.copy(Bitmap.Config.ARGB_8888,false) else it }
            } finally { padded.recycle() }
        }
        return try { UiImage.encode(bitmap) } finally { bitmap.recycle() }
    }
    fun launch(app: String): JSONObject = result { it.launchApp(displayId,app) }
    fun key(code: Int, modifiers: Int = 0): JSONObject = result { it.key(displayId,code,modifiers) }
    private fun result(call: (IUiShell)->String): JSONObject {
        val service = shell ?: throw AgentError("background_view_lost")
        val result = JSONObject(call(service))
        if (result.has("error")) throw AgentError(result.getString("error"),result.optString("message",result.getString("error")))
        return result
    }
    private fun release() {
        val id = descriptor?.optInt("display_id",-1) ?: -1
        if (id >= 0) runCatching { shell?.releaseDisplay(id) }
        descriptor = null; shell = null
        synchronized(frames) { latest?.close(); latest = null; reader?.close(); reader = null; frames.notifyAll() }
    }
    override fun close() { closed = true; release(); thread.quitSafely() }
}
