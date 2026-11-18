package `fun`.hackerlife.octendra

import android.content.ContentProvider
import android.content.ContentValues
import android.content.Context
import android.content.pm.PackageManager
import android.database.Cursor
import android.net.Uri
import android.os.Binder
import android.os.IBinder
import android.os.Build
import android.os.Bundle
import android.os.Process
import android.os.SystemClock
import android.view.InputDevice
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicLong

/** Caller verification policy for fun.hackerlife.octendra.aiview. Pure JVM testable. */
internal object AiViewCallerPolicy {
    const val AUTHORITY = "fun.hackerlife.octendra.aiview"
    const val OWNER_CERT_SHA256 = "f84d9b71f1a00f4b5e3084420e5cee3327053389983b2428d9bec71755e5539d"

    fun isAllowed(
        callingUid: Int,
        selfUid: Int,
        ownCertsSha256: Set<String>,
        configuredPeerFingerprint: String?,
        callerPackageSigners: List<Set<String>>
    ): Boolean {
        if (callingUid == selfUid || callingUid == 2000) return true
        if (callerPackageSigners.isEmpty()) return false
        val allowed = HashSet<String>(ownCertsSha256.size + 2)
        ownCertsSha256.forEach { allowed.add(it.lowercase()) }
        allowed.add(OWNER_CERT_SHA256)
        configuredPeerFingerprint?.trim()?.lowercase()
            ?.takeIf { it.matches(Regex("[0-9a-f]{64}")) }
            ?.let { allowed.add(it) }
        return callerPackageSigners.any { signers ->
            signers.isNotEmpty() && signers.all { it.lowercase() in allowed }
        }
    }
}

/** Platform-abstracted input sink for human takeover on an owned virtual display. */
internal interface AiInputSink {
    fun injectTouch(
        displayId: Int,
        downTimeMs: Long,
        eventTimeMs: Long,
        action: Int,
        pointerCount: Int,
        pointerIds: IntArray,
        xs: FloatArray,
        ys: FloatArray,
        pressures: FloatArray
    ): Boolean

    fun injectKey(
        displayId: Int,
        downTimeMs: Long,
        eventTimeMs: Long,
        action: Int,
        androidKeyCode: Int,
        metaState: Int,
        flags: Int
    ): Boolean

    fun commitText(displayId: Int, text: String): Boolean

    fun uptimeMs(): Long
}

/**
 * Stateful Weavelo InputEvent translator for one consumer session on a background display.
 * Handles MouseMoveAbsolute, MouseMove, MouseDown, MouseUp, MouseScroll, TouchFrame, Touch,
 * ModifiersChanged, Key (macOS keycodes + optional android_key_code), and UTF-8 Text.
 */
internal class AiHumanInputSession(
    private val displayId: Int,
    @Volatile var width: Int,
    @Volatile var height: Int,
    private val sink: AiInputSink
) {
    private data class Contact(val slot: Int, var x: Float, var y: Float, var pressure: Float)

    private val lock = Any()
    private val contacts = linkedMapOf<Long, Contact>()
    private val heldKeys = linkedMapOf<Int, Pair<Long, Int>>() // androidKeyCode -> (downTimeMs, metaState)
    private var cursorX = (width.coerceAtLeast(2) / 2f)
    private var cursorY = (height.coerceAtLeast(2) / 2f)
    private var downTimeMs = 0L
    private var modifiers = 0

    fun hasHeldInput(): Boolean = synchronized(lock) { contacts.isNotEmpty() || heldKeys.isNotEmpty() || modifiers != 0 }

    fun updateDimensions(newWidth: Int, newHeight: Int) = synchronized(lock) {
        width = newWidth.coerceAtLeast(2)
        height = newHeight.coerceAtLeast(2)
        cursorX = cursorX.coerceIn(0f, (width - 1).toFloat())
        cursorY = cursorY.coerceIn(0f, (height - 1).toFloat())
    }

    private fun pos(normalized: Double, extent: Int): Float {
        require(normalized.isFinite() && normalized in 0.0..1.0) { "invalid_normalized_coordinate" }
        return (normalized * (extent - 1)).toFloat()
    }

    private fun sendTouch(action: Int): Boolean {
        if (contacts.isEmpty()) return true
        val n = contacts.size
        val ids = IntArray(n)
        val xs = FloatArray(n)
        val ys = FloatArray(n)
        val pressures = FloatArray(n)
        var i = 0
        for (c in contacts.values) {
            ids[i] = c.slot
            xs[i] = c.x
            ys[i] = c.y
            pressures[i] = c.pressure
            i++
        }
        return sink.injectTouch(displayId, downTimeMs, sink.uptimeMs(), action, n, ids, xs, ys, pressures)
    }

    private fun startContact(id: Long, px: Float, py: Float, pressure: Float): Boolean {
        if (id in contacts || contacts.size >= 10) return false
        val slot = (0..9).first { candidate -> contacts.values.none { it.slot == candidate } }
        if (contacts.isEmpty()) downTimeMs = sink.uptimeMs()
        contacts[id] = Contact(slot, px, py, pressure)
        val action = if (contacts.size == 1) MotionEvent.ACTION_DOWN
        else MotionEvent.ACTION_POINTER_DOWN or ((contacts.size - 1) shl MotionEvent.ACTION_POINTER_INDEX_SHIFT)
        return sendTouch(action)
    }

    private fun endContact(id: Long): Boolean {
        val index = contacts.keys.indexOf(id)
        if (index < 0) return false
        val action = if (contacts.size == 1) MotionEvent.ACTION_UP
        else MotionEvent.ACTION_POINTER_UP or (index shl MotionEvent.ACTION_POINTER_INDEX_SHIFT)
        val ok = sendTouch(action)
        contacts.remove(id)
        return ok
    }

    private fun sendKey(androidKeyCode: Int, down: Boolean, modifiers: Int): Boolean {
        val meta = (if (modifiers and 1 != 0) KeyEvent.META_SHIFT_ON else 0) or
            (if (modifiers and 0x0A != 0) KeyEvent.META_CTRL_ON else 0) or
            (if (modifiers and 0x14 != 0) KeyEvent.META_ALT_ON else 0)
        val now = sink.uptimeMs()
        val downAt = if (down) now else (heldKeys[androidKeyCode]?.first ?: now)
        val action = if (down) KeyEvent.ACTION_DOWN else KeyEvent.ACTION_UP
        val ok = sink.injectKey(displayId, downAt, now, action, androidKeyCode, meta, 0)
        if (ok) {
            if (down) heldKeys[androidKeyCode] = downAt to meta
            else heldKeys.remove(androidKeyCode)
        }
        return ok
    }

    private fun pressKey(androidKeyCode: Int): Boolean =
        sendKey(androidKeyCode, true, 0) && sendKey(androidKeyCode, false, 0)

    fun execute(event: JSONObject): Boolean = synchronized(lock) {
        val keys = event.keys()
        if (!keys.hasNext()) return@synchronized false
        val name = keys.next()
        val value = event.get(name)
        when (name) {
            "MouseMoveAbsolute", "MouseMove" -> {
                val p = value as? JSONObject ?: return@synchronized false
                if (name == "MouseMoveAbsolute") {
                    val nx = p.getDouble("x") / 65535.0
                    val ny = p.getDouble("y") / 65535.0
                    cursorX = pos(nx, width)
                    cursorY = pos(ny, height)
                } else {
                    cursorX = (cursorX + p.getInt("dx")).coerceIn(0f, (width - 1).toFloat())
                    cursorY = (cursorY + p.getInt("dy")).coerceIn(0f, (height - 1).toFloat())
                }
                contacts[-1L]?.let {
                    it.x = cursorX; it.y = cursorY
                    sendTouch(MotionEvent.ACTION_MOVE)
                } ?: contacts.isEmpty()
            }

            "MouseDown" -> when ((value as? Number)?.toInt()) {
                0 -> contacts.isEmpty() && startContact(-1L, cursorX, cursorY, 1f)
                1 -> pressKey(KeyEvent.KEYCODE_BACK)
                2 -> pressKey(KeyEvent.KEYCODE_HOME)
                else -> false
            }

            "MouseUp" -> when ((value as? Number)?.toInt()) {
                0 -> endContact(-1L)
                1, 2 -> true
                else -> false
            }

            "MouseScroll" -> {
                if (contacts.isNotEmpty()) return@synchronized false
                val delta = value as? JSONObject ?: return@synchronized false
                if (!startContact(-1L, cursorX, cursorY, 1f)) return@synchronized false
                contacts.getValue(-1L).apply {
                    x = (x + delta.getInt("delta_x") * 3f).coerceIn(0f, (width - 1).toFloat())
                    y = (y + delta.getInt("delta_y") * 3f).coerceIn(0f, (height - 1).toFloat())
                }
                sendTouch(MotionEvent.ACTION_MOVE) && endContact(-1L)
            }

            "TouchFrame" -> {
                val arr = value as? JSONArray ?: return@synchronized false
                executeTouchFrame(arr)
            }

            "Touch" -> {
                val obj = value as? JSONObject ?: return@synchronized false
                executeTouchFrame(JSONArray().put(obj))
            }

            "ModifiersChanged" -> { modifiers = (value as? Number)?.toInt()?.and(0x1f) ?: return@synchronized false; true }

            "Key" -> {
                val info = value as? JSONObject ?: return@synchronized false
                val androidCode = if (info.has("android_key_code") && !info.isNull("android_key_code")) {
                    info.getInt("android_key_code")
                } else {
                    mapMacKeyCode(info.getInt("key_code")) ?: return@synchronized false
                }
                sendKey(androidCode, info.getBoolean("pressed"), info.optInt("modifiers", 0))
            }

            "Text" -> {
                val text = value as? String ?: return@synchronized false
                if (text.isEmpty()) false else sink.commitText(displayId, text)
            }

            else -> false
        }
    }

    private fun executeTouchFrame(frame: JSONArray): Boolean {
        if (frame.length() !in 1..10 || contacts.containsKey(-1L)) return false
        val changes = (0 until frame.length()).map { frame.getJSONObject(it) }
        if (changes.map { it.getLong("pointer_id") }.distinct().size != changes.size) return false
        for (change in changes) {
            val id = change.getLong("pointer_id")
            val action = change.getString("action")
            if (id < 0 || (action == "Down") == contacts.containsKey(id)) return false
            if (action !in listOf("Down", "Move", "Up", "Cancel")) return false
            val pressure = change.optDouble("pressure", 1.0).toFloat()
            if (!pressure.isFinite() || pressure !in 0f..1f) return false
            val px = pos(change.getDouble("normalized_x"), width)
            val py = pos(change.getDouble("normalized_y"), height)
            contacts[id]?.apply { x = px; y = py; this.pressure = pressure }
        }
        if (changes.any { it.getString("action") == "Cancel" }) {
            val ok = sendTouch(MotionEvent.ACTION_CANCEL)
            contacts.clear()
            return ok
        }
        for (change in changes.filter { it.getString("action") == "Down" }) {
            val id = change.getLong("pointer_id")
            val px = pos(change.getDouble("normalized_x"), width)
            val py = pos(change.getDouble("normalized_y"), height)
            val pressure = change.optDouble("pressure", 1.0).toFloat()
            if (!startContact(id, px, py, pressure)) return false
        }
        if (changes.any { it.getString("action") == "Move" } && !sendTouch(MotionEvent.ACTION_MOVE)) return false
        for (change in changes.filter { it.getString("action") == "Up" }) {
            if (!endContact(change.getLong("pointer_id"))) return false
        }
        return true
    }

    fun release() = synchronized(lock) {
        if (contacts.isNotEmpty()) {
            runCatching { sendTouch(MotionEvent.ACTION_CANCEL) }
            contacts.clear()
        }
        for ((code, state) in heldKeys.toList()) {
            runCatching {
                sink.injectKey(displayId, state.first, sink.uptimeMs(), KeyEvent.ACTION_UP, code, state.second, KeyEvent.FLAG_CANCELED)
            }
        }
        heldKeys.clear()
        modifiers = 0
    }

    companion object {
        private val macKeyToAndroid: Map<Int, Int> = mapOf(
            0x35 to KeyEvent.KEYCODE_ESCAPE,
            0x24 to KeyEvent.KEYCODE_ENTER,
            0x30 to KeyEvent.KEYCODE_TAB,
            0x31 to KeyEvent.KEYCODE_SPACE,
            0x33 to KeyEvent.KEYCODE_DEL,
            0x75 to KeyEvent.KEYCODE_FORWARD_DEL,
            0x7B to KeyEvent.KEYCODE_DPAD_LEFT,
            0x7C to KeyEvent.KEYCODE_DPAD_RIGHT,
            0x7D to KeyEvent.KEYCODE_DPAD_DOWN,
            0x7E to KeyEvent.KEYCODE_DPAD_UP,
            0x73 to KeyEvent.KEYCODE_MOVE_HOME,
            0x77 to KeyEvent.KEYCODE_MOVE_END,
            0x74 to KeyEvent.KEYCODE_PAGE_UP,
            0x79 to KeyEvent.KEYCODE_PAGE_DOWN,
            // Letters (macOS virtual keycodes from RemoteTextEdit)
            0x00 to KeyEvent.KEYCODE_A, 0x0B to KeyEvent.KEYCODE_B, 0x08 to KeyEvent.KEYCODE_C,
            0x02 to KeyEvent.KEYCODE_D, 0x0E to KeyEvent.KEYCODE_E, 0x03 to KeyEvent.KEYCODE_F,
            0x05 to KeyEvent.KEYCODE_G, 0x04 to KeyEvent.KEYCODE_H, 0x22 to KeyEvent.KEYCODE_I,
            0x26 to KeyEvent.KEYCODE_J, 0x28 to KeyEvent.KEYCODE_K, 0x25 to KeyEvent.KEYCODE_L,
            0x2E to KeyEvent.KEYCODE_M, 0x2D to KeyEvent.KEYCODE_N, 0x1F to KeyEvent.KEYCODE_O,
            0x23 to KeyEvent.KEYCODE_P, 0x0C to KeyEvent.KEYCODE_Q, 0x0F to KeyEvent.KEYCODE_R,
            0x01 to KeyEvent.KEYCODE_S, 0x11 to KeyEvent.KEYCODE_T, 0x20 to KeyEvent.KEYCODE_U,
            0x09 to KeyEvent.KEYCODE_V, 0x0D to KeyEvent.KEYCODE_W, 0x07 to KeyEvent.KEYCODE_X,
            0x10 to KeyEvent.KEYCODE_Y, 0x06 to KeyEvent.KEYCODE_Z,
            // Digits & symbols
            0x12 to KeyEvent.KEYCODE_1, 0x13 to KeyEvent.KEYCODE_2, 0x14 to KeyEvent.KEYCODE_3,
            0x15 to KeyEvent.KEYCODE_4, 0x17 to KeyEvent.KEYCODE_5, 0x16 to KeyEvent.KEYCODE_6,
            0x1A to KeyEvent.KEYCODE_7, 0x1C to KeyEvent.KEYCODE_8, 0x19 to KeyEvent.KEYCODE_9,
            0x1D to KeyEvent.KEYCODE_0, 0x1B to KeyEvent.KEYCODE_MINUS, 0x18 to KeyEvent.KEYCODE_EQUALS,
            0x21 to KeyEvent.KEYCODE_LEFT_BRACKET, 0x1E to KeyEvent.KEYCODE_RIGHT_BRACKET,
            0x2A to KeyEvent.KEYCODE_BACKSLASH, 0x29 to KeyEvent.KEYCODE_SEMICOLON,
            0x27 to KeyEvent.KEYCODE_APOSTROPHE, 0x2B to KeyEvent.KEYCODE_COMMA,
            0x2F to KeyEvent.KEYCODE_PERIOD, 0x2C to KeyEvent.KEYCODE_SLASH,
            0x32 to KeyEvent.KEYCODE_GRAVE
        )

        fun mapMacKeyCode(macCode: Int): Int? = macKeyToAndroid[macCode]
    }
}

/**
 * Shared coordinator for active background AI views, video consumers, and human takeover state.
 * Pure Kotlin logic (surface attachment and input injection are delegated via callbacks/AiInputSink),
 * allowing full JVM unit testing of the ContentProvider contract and takeover epoch logic.
 */
internal class AiViewHost {
    internal interface SurfaceBridge {
        fun attachSurface(surface: Any): String
        fun detachSurface(consumerId: String)
    }

    internal data class ViewAppInfo(val packageName: String, val label: String, val userId: Int)

    internal class ActiveView(
        val viewId: String,
        val displayId: Int,
        val generation: String,
        @Volatile var width: Int,
        @Volatile var height: Int,
        @Volatile var density: Int,
        @Volatile var rotation: Int,
        val surfaceBridge: SurfaceBridge,
        val inputSink: AiInputSink,
        val cancelAiInput: () -> Unit,
        val invalidateAiObservations: () -> Unit,
        val resolveAppInfo: () -> ViewAppInfo,
        val supportsInput: () -> Boolean = { true }
    ) {
        val lock = Any()
        val sourceRevision = AtomicLong(1L)
        val resourceEpoch = AtomicLong(1L)
        val lastHumanTakeoverEpoch = AtomicLong(0L)
        val humanActive = AtomicBoolean(false)
        @Volatile var activeHumanConsumerId: String? = null
        val consumers = LinkedHashMap<String, AiHumanInputSession>()
    }

    private val registryLock = Any()
    private val views = LinkedHashMap<String, ActiveView>() // viewId -> ActiveView
    private val consumerToView = LinkedHashMap<String, String>() // consumerId -> viewId
    private val consumerDeaths = LinkedHashMap<String, Pair<IBinder, IBinder.DeathRecipient>>()

    fun register(view: ActiveView) = synchronized(registryLock) {
        // Clear any stale entries for the same viewId or displayId
        val staleIds = views.values.filter { it.viewId == view.viewId || it.displayId == view.displayId }.map { it.viewId }
        for (id in staleIds) unregisterLocked(id)
        views[view.viewId] = view
    }

    fun unregister(viewId: String, generation: String? = null) {
        synchronized(registryLock) {
            val existing = views[viewId] ?: return
            if (generation != null && existing.generation != generation) return
            unregisterLocked(viewId)
        }
    }

    private fun unregisterLocked(viewId: String) {
        val removed = views.remove(viewId) ?: return
        val ids = synchronized(removed.lock) {
            val cids = removed.consumers.keys.toList()
            for (session in removed.consumers.values) runCatching { session.release() }
            removed.consumers.clear()
            removed.humanActive.set(false)
            removed.activeHumanConsumerId = null
            cids
        }
        ids.forEach { id ->
            consumerToView.remove(id)
            consumerDeaths.remove(id)?.let { (token, death) -> runCatching { token.unlinkToDeath(death, 0) } }
        }
    }

    fun updateGeometry(viewId: String, generation: String, width: Int, height: Int, density: Int, rotation: Int = 0) {
        val view = synchronized(registryLock) { views[viewId]?.takeIf { it.generation == generation } } ?: return
        synchronized(view.lock) {
            if (view.width != width || view.height != height || view.density != density || view.rotation != rotation) {
                view.width = width
                view.height = height
                view.density = density
                view.rotation = rotation
                view.sourceRevision.incrementAndGet()
                view.consumers.values.forEach { it.updateDimensions(width, height) }
            }
        }
    }

    fun currentResourceEpoch(viewId: String): Long {
        val view = synchronized(registryLock) { views[viewId] } ?: return 1L
        return view.resourceEpoch.get()
    }

    fun isHumanActive(viewId: String): Boolean {
        val view = synchronized(registryLock) { views[viewId] } ?: return false
        return view.humanActive.get()
    }

    /**
     * Checks whether a human took over since [observedEpoch] or is currently active.
     * Returns null if AI may proceed, or (humanActive, currentEpoch) if takeover occurred.
     */
    fun checkTakeoverForAi(viewId: String, generation: String, observedEpoch: Long): Pair<Boolean, Long>? {
        val view = synchronized(registryLock) { views[viewId] } ?: return null
        if (view.generation != generation) return null
        synchronized(view.lock) {
            val humanNow = view.humanActive.get()
            val takeoverEpoch = view.lastHumanTakeoverEpoch.get()
            val currentEpoch = view.resourceEpoch.get()
            return if (humanNow || takeoverEpoch > observedEpoch) (humanNow to currentEpoch) else null
        }
    }

    fun bumpResourceEpochForAi(viewId: String, generation: String): Long {
        val view = synchronized(registryLock) { views[viewId] } ?: return 1L
        if (view.generation != generation) return view.resourceEpoch.get()
        return view.resourceEpoch.incrementAndGet()
    }

    fun describeViews(): String {
        val snapshot = synchronized(registryLock) { views.values.toList() }
        val arr = JSONArray()
        for (view in snapshot) {
            val appInfo = runCatching { view.resolveAppInfo() }.getOrElse { ViewAppInfo("", "AI 工作区", 0) }
            val inputReady = runCatching { view.supportsInput() }.getOrDefault(true)
            synchronized(view.lock) {
                arr.put(
                    obj(
                        "view_id" to view.viewId,
                        "display_id" to view.displayId,
                        "generation" to view.generation,
                        "source_revision" to view.sourceRevision.get(),
                        "resource_epoch" to view.resourceEpoch.get(),
                        "package" to appInfo.packageName,
                        "label" to appInfo.label.ifBlank { appInfo.packageName.ifBlank { "AI 工作区" } },
                        "user_id" to appInfo.userId,
                        "width" to view.width,
                        "height" to view.height,
                        "density" to view.density,
                        "rotation" to view.rotation,
                        "human_active" to view.humanActive.get(),
                        "supports_input" to inputReady
                    )
                )
            }
        }
        return arr.toString()
    }

    fun attachVideo(viewId: String?, generation: String?, surface: Any?, owner: IBinder? = null): JSONObject {
        if (viewId.isNullOrEmpty() || generation.isNullOrEmpty() || surface == null) {
            return obj("ok" to false, "error" to "invalid_attach_arguments")
        }
        val view = synchronized(registryLock) { views[viewId] }
            ?: return obj("ok" to false, "error" to "unknown_view")
        if (view.generation != generation) {
            return obj("ok" to false, "error" to "generation_mismatch")
        }
        val consumerId = try {
            view.surfaceBridge.attachSurface(surface)
        } catch (e: Exception) {
            return obj("ok" to false, "error" to (e.message ?: "attach_surface_failed"))
        }
        synchronized(registryLock) {
            if (views[viewId] !== view || view.generation != generation) {
                runCatching { view.surfaceBridge.detachSurface(consumerId) }
                return obj("ok" to false, "error" to "view_closed")
            }
            synchronized(view.lock) {
                view.consumers[consumerId] = AiHumanInputSession(view.displayId, view.width, view.height, view.inputSink)
                consumerToView[consumerId] = viewId
                if (owner != null) {
                    val death = IBinder.DeathRecipient { detachVideo(consumerId) }
                    try {
                        owner.linkToDeath(death, 0)
                        consumerDeaths[consumerId] = owner to death
                    } catch (_: Exception) {
                        view.consumers.remove(consumerId)
                        consumerToView.remove(consumerId)
                        runCatching { view.surfaceBridge.detachSurface(consumerId) }
                        return obj("ok" to false, "error" to "consumer_disconnected")
                    }
                }
                return obj(
                    "ok" to true,
                    "consumer_id" to consumerId,
                    "view_id" to view.viewId,
                    "display_id" to view.displayId,
                    "generation" to view.generation,
                    "source_revision" to view.sourceRevision.get(),
                    "resource_epoch" to view.resourceEpoch.get(),
                    "width" to view.width,
                    "height" to view.height,
                    "rotation" to view.rotation
                )
            }
        }
    }

    fun detachVideo(consumerId: String?): JSONObject {
        if (consumerId.isNullOrEmpty()) return obj("ok" to false, "error" to "invalid_consumer_id")
        val view = synchronized(registryLock) {
            val vid = consumerToView.remove(consumerId)
            consumerDeaths.remove(consumerId)?.let { (token, death) -> runCatching { token.unlinkToDeath(death, 0) } }
            vid?.let { views[it] }
        }
        if (view != null) {
            synchronized(view.lock) {
                if (view.activeHumanConsumerId == consumerId) {
                    view.humanActive.set(false)
                    view.activeHumanConsumerId = null
                }
                view.consumers.remove(consumerId)?.also { runCatching { it.release() } }
            }
            runCatching { view.surfaceBridge.detachSurface(consumerId) }
        }
        return obj("ok" to true)
    }

    fun dispatchInput(
        consumerId: String?,
        viewId: String?,
        generation: String?,
        sourceRevision: Long?,
        eventJson: String?
    ): JSONObject {
        if (consumerId.isNullOrEmpty() || viewId.isNullOrEmpty() || generation.isNullOrEmpty() ||
            sourceRevision == null || eventJson.isNullOrEmpty()
        ) {
            return obj("ok" to false, "error" to "invalid_input_arguments")
        }
        val eventObj = try {
            JSONObject(eventJson)
        } catch (_: Exception) {
            return obj("ok" to false, "error" to "invalid_event_json")
        }
        val view = synchronized(registryLock) {
            val mapped = consumerToView[consumerId]
            if (mapped != viewId) null else views[viewId]
        } ?: return obj("ok" to false, "error" to "stale_view_or_consumer")

        if (view.generation != generation) {
            return obj("ok" to false, "error" to "stale_view_or_consumer")
        }
        synchronized(view.lock) {
            val currentRev = view.sourceRevision.get()
            if (sourceRevision != currentRev) {
                return obj("ok" to false, "error" to "source_revision_mismatch", "source_revision" to currentRev)
            }
            val session = view.consumers[consumerId]
                ?: return obj("ok" to false, "error" to "stale_view_or_consumer")
            if (view.humanActive.get() && view.activeHumanConsumerId != consumerId) {
                return obj("ok" to false, "error" to "human_input_owned")
            }
            val newEpoch = view.resourceEpoch.incrementAndGet()
            view.humanActive.set(true)
            view.activeHumanConsumerId = consumerId
            view.lastHumanTakeoverEpoch.set(newEpoch)
            runCatching { view.cancelAiInput() }
            runCatching { view.invalidateAiObservations() }
            val accepted = runCatching { session.execute(eventObj) }.getOrDefault(false)
            if (!accepted) runCatching { session.release() }
            if (!session.hasHeldInput()) {
                view.humanActive.set(false)
                view.activeHumanConsumerId = null
            }
            return obj("ok" to true, "accepted" to accepted, "resource_epoch" to newEpoch)
        }
    }

    fun releaseInput(consumerId: String?): JSONObject {
        if (consumerId.isNullOrEmpty()) return obj("ok" to false, "error" to "invalid_consumer_id")
        val view = synchronized(registryLock) {
            val vid = consumerToView[consumerId]
            vid?.let { views[it] }
        } ?: return obj("ok" to true)
        synchronized(view.lock) {
            view.consumers[consumerId]?.let { runCatching { it.release() } }
            if (view.activeHumanConsumerId == consumerId) {
                view.humanActive.set(false)
                view.activeHumanConsumerId = null
            }
        }
        return obj("ok" to true)
    }

    companion object {
        val shared = AiViewHost()
    }
}

/**
 * Exported ContentProvider exposing active Octendra background AI views to authorized peer apps
 * (such as Weavelo) over zero-copy Surface sharing and synchronized human-takeover input.
 */
class AiViewBridgeProvider : ContentProvider() {
    override fun onCreate(): Boolean = true

    override fun call(method: String, arg: String?, extras: Bundle?): Bundle {
        val ctx = context ?: throw SecurityException("unavailable")
        verifyCaller(ctx, Binder.getCallingUid())
        return dispatchCall(AiViewHost.shared, method, arg, extras)
    }

    @Suppress("DEPRECATION")
    private fun packageCertsSha256(pm: PackageManager, pkg: String): Set<String> {
        val info = runCatching {
            if (Build.VERSION.SDK_INT >= 33) {
                pm.getPackageInfo(pkg, PackageManager.PackageInfoFlags.of(PackageManager.GET_SIGNING_CERTIFICATES.toLong()))
            } else {
                pm.getPackageInfo(pkg, PackageManager.GET_SIGNING_CERTIFICATES)
            }
        }.getOrNull() ?: return emptySet()
        val signers = info.signingInfo?.apkContentsSigners ?: return emptySet()
        return signers.map { sha256(it.toByteArray()).lowercase() }.toSet()
    }

    private fun verifyCaller(ctx: Context, callingUid: Int) {
        val selfUid = Process.myUid()
        if (callingUid == selfUid || callingUid == 2000) return
        val pm = ctx.packageManager
        val packages = pm.getPackagesForUid(callingUid)?.toList().orEmpty()
        val ownCerts = packageCertsSha256(pm, ctx.packageName)
        val configuredPeer = runCatching { controls(ctx).getString("ai_view_peer_sha256", null) }.getOrNull()
        val callerCerts = packages.map { packageCertsSha256(pm, it) }
        if (!AiViewCallerPolicy.isAllowed(callingUid, selfUid, ownCerts, configuredPeer, callerCerts)) {
            throw SecurityException("unauthorized_aiview_caller")
        }
    }

    override fun query(uri: Uri, projection: Array<out String>?, selection: String?, selectionArgs: Array<out String>?, sortOrder: String?): Cursor =
        throw SecurityException("unsupported_operation")

    override fun getType(uri: Uri): String = "vnd.android.cursor.item/vnd.fun.hackerlife.octendra.aiview"

    override fun insert(uri: Uri, values: ContentValues?): Uri = throw SecurityException("read_only")

    override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?): Int =
        throw SecurityException("read_only")

    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int =
        throw SecurityException("read_only")

    companion object {
        internal fun dispatchCall(host: AiViewHost, method: String, arg: String?, extras: Bundle?): Bundle {
            return when (method) {
                "describe_views" -> Bundle().apply {
                    putString("views_json", host.describeViews())
                }

                "attach_video" -> {
                    val generation = extras?.getString("generation")
                    val surface: Surface? = if (Build.VERSION.SDK_INT >= 33) {
                        extras?.getParcelable("surface", Surface::class.java)
                    } else {
                        @Suppress("DEPRECATION")
                        extras?.getParcelable("surface")
                    }
                    val owner = extras?.getBinder("owner")
                    Bundle().apply {
                        putString("result_json", if (owner == null) obj("ok" to false, "error" to "missing_owner").toString()
                            else host.attachVideo(arg, generation, surface, owner).toString())
                    }
                }

                "detach_video" -> Bundle().apply {
                    putString("result_json", host.detachVideo(arg).toString())
                }

                "dispatch_input" -> {
                    val viewId = extras?.getString("view_id")
                    val generation = extras?.getString("generation")
                    val rev = if (extras != null && extras.containsKey("source_revision")) extras.getLong("source_revision") else null
                    val eventJson = extras?.getString("event_json")
                    Bundle().apply {
                        putString("result_json", host.dispatchInput(arg, viewId, generation, rev, eventJson).toString())
                    }
                }

                "release_input" -> Bundle().apply {
                    putString("result_json", host.releaseInput(arg).toString())
                }

                else -> Bundle().apply {
                    putString("result_json", obj("ok" to false, "error" to "unsupported_method").toString())
                }
            }
        }
    }
}
