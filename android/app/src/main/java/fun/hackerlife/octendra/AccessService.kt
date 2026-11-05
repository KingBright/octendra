package `fun`.hackerlife.octendra

import android.accessibilityservice.AccessibilityService
import android.accessibilityservice.AccessibilityServiceInfo
import android.accessibilityservice.GestureDescription
import android.graphics.Bitmap
import android.graphics.Path
import android.graphics.Rect
import android.os.Bundle
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.os.SystemClock
import android.view.Display
import android.view.KeyEvent
import android.view.accessibility.AccessibilityEvent
import android.view.accessibility.AccessibilityNodeInfo
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.util.concurrent.CompletableFuture
import java.util.concurrent.Executors
import java.util.concurrent.FutureTask
import java.util.concurrent.TimeUnit

class AccessService : AccessibilityService() {
    data class NodeRef(val path: List<Int>, val signature: String)
    data class Observation(val id: String, val at: Long, val window: Int, val pkg: String, val nodes: List<NodeRef>, val display: String, val epoch: Long, val displayId: Int)
    private val handler = Handler(Looper.getMainLooper())
    private val pictures = Executors.newSingleThreadExecutor()
    private val observations = HashMap<Int,Observation>()
    private var screenshotRequestedAt = 0L
    private val windowEpochs = HashMap<Int,Long>()
    private fun epoch(displayId: Int) = windowEpochs[displayId] ?: 0L
    override fun onServiceConnected() {
        if (Build.VERSION.SDK_INT >= 33) serviceInfo = serviceInfo.apply { flags = flags or AccessibilityServiceInfo.FLAG_INPUT_METHOD_EDITOR }
        instance = this; AgentRuntime.changed()
    }
    override fun onAccessibilityEvent(event: AccessibilityEvent?) {
        if (event?.packageName?.toString() == "com.android.settings") AgentRuntime.wirelessAdb?.pairFromSettings()
        // Only an invalidation counter, never keystrokes or page contents.
        if (event?.eventType == AccessibilityEvent.TYPE_WINDOW_STATE_CHANGED || event?.eventType == AccessibilityEvent.TYPE_WINDOWS_CHANGED) {
            val all = windowsOnAllDisplays
            val id = (0 until all.size()).firstOrNull { index -> all.valueAt(index).any { it.id == event.windowId } }
                ?.let { all.keyAt(it) } ?: Display.DEFAULT_DISPLAY
            windowEpochs[id] = epoch(id)+1
        }
    }
    override fun onInterrupt() { observations.clear() }
    override fun onDestroy() { if (instance === this) instance = null; observations.clear(); pictures.shutdownNow(); super.onDestroy(); AgentRuntime.changed() }
    private fun enabled() { demand(controls(this).getBoolean("remote_enabled", false) && AgentRuntime.running, "remote_control_stopped_locally") }
    private fun scheduleScreenshot(callback: TakeScreenshotCallback) {
        val at = maxOf(SystemClock.elapsedRealtime(), screenshotRequestedAt + 350)
        val delay = at - SystemClock.elapsedRealtime(); screenshotRequestedAt = at
        handler.postDelayed({
            if (AgentRuntime.running && controls(this).getBoolean("remote_enabled", false))
                takeScreenshot(Display.DEFAULT_DISPLAY,pictures,callback)
            else callback.onFailure(ERROR_TAKE_SCREENSHOT_INTERNAL_ERROR)
        },delay.coerceAtLeast(0))
    }
    private fun <T> main(block: () -> T): T {
        if (Looper.myLooper() == Looper.getMainLooper()) return block()
        val task = FutureTask { enabled(); block() }
        handler.post(task)
        // A single framework accessibility query may itself wait five seconds.
        // Leave room for it and our bounded tree traversal instead of racing it.
        try { return task.get(10, TimeUnit.SECONDS) }
        catch (e: Exception) {
            handler.removeCallbacks(task); task.cancel(false)
            if (e is java.util.concurrent.TimeoutException)
                android.util.Log.w("OctendraUI", "Accessibility main thread timed out: " + Looper.getMainLooper().thread.stackTrace.joinToString(";"))
            throw e
        }
    }
    private fun signature(n: AccessibilityNodeInfo): String {
        val b = Rect(); n.getBoundsInScreen(b)
        return listOf(n.windowId.toString(), n.packageName?.toString() ?: "", n.className?.toString() ?: "",
            n.viewIdResourceName ?: "", if (n.isPassword) "<password>" else n.text?.toString()?.take(200) ?: "",
            n.contentDescription?.toString()?.take(200) ?: "", b.flattenToString(), n.isPassword.toString()).joinToString("\u001f")
    }
    private fun rootFor(displayId: Int): AccessibilityNodeInfo? {
        val candidates = windowsOnAllDisplays[displayId] ?: return null
        return candidates.sortedWith(compareByDescending<android.view.accessibility.AccessibilityWindowInfo> { it.isActive }
            .thenByDescending { it.isFocused }.thenByDescending { it.layer }).firstNotNullOfOrNull { it.root }
    }
    /** Only used during an explicitly requested local ADB pairing; never returned over MCP. */
    fun localWirelessPairingCode(): CharArray? = main {
        if (Build.VERSION.SDK_INT >= 33) clearCache()
        val root = rootFor(Display.DEFAULT_DISPLAY) ?: return@main null
        try {
            if (root.packageName?.toString() != "com.android.settings") return@main null
            val nodes = root.findAccessibilityNodeInfosByViewId("com.android.settings:id/pairing_code")
            try {
                val value = nodes.singleOrNull()?.takeIf { it.isVisibleToUser }?.text?.toString()?.replace(" ", "")
                value?.takeIf { it.length == 6 && it.all { digit -> digit in '0'..'9' } }?.toCharArray()
            } finally { nodes.forEach { it.recycle() } }
        } finally { root.recycle() }
    }
    fun dump(displayId: Int = Display.DEFAULT_DISPLAY): JSONObject = main {
        enabled()
        val manager = getSystemService(android.hardware.display.DisplayManager::class.java)
        observations.keys.removeAll { manager.getDisplay(it) == null }
        // We intentionally do not subscribe to text-change/keystroke events.
        // The framework cache can therefore outlive a changed label. Refresh
        // on explicit observation instead of continuously collecting contents.
        val freshCache = Build.VERSION.SDK_INT >= 33 && clearCache()
        val root = rootFor(displayId)
        if (root == null) {
            demand(displayId != Display.DEFAULT_DISPLAY,"screen_tree_unavailable")
            val id = nonce().take(24)
            observations[displayId] = Observation(id,SystemClock.elapsedRealtime(),-1,"",emptyList(),displayInfo(displayId).toString(),epoch(displayId),displayId)
            return@main obj("observation_id" to id,"display" to displayInfo(displayId),"nodes" to JSONArray(),"truncated" to false,
                "window_id" to -1,"window_epoch" to epoch(displayId),"observed_at_ms" to System.currentTimeMillis())
        }
        val refs = mutableListOf<NodeRef>()
        val nodes = JSONArray()
        var estimate = 0
        var truncated = false
        val epoch = epoch(displayId)
        val deadline = SystemClock.elapsedRealtime() + 3000
        fun visit(n: AccessibilityNodeInfo, path: List<Int>, depth: Int) {
            if (refs.size >= 300 || estimate > 58000 || depth > 35 || SystemClock.elapsedRealtime() >= deadline) { truncated = true; return }
            // API 30-32 have no public cache invalidation API; refresh only the
            // bounded nodes we actually return. Never silently return stale data.
            if (!freshCache && !n.refresh()) { truncated = true; return }
            val bounds = Rect(); n.getBoundsInScreen(bounds)
            val item = obj("id" to refs.size, "class" to n.className?.toString(), "resource_id" to n.viewIdResourceName,
                "text" to if (n.isPassword) "<redacted>" else n.text?.toString()?.take(180),
                "description" to if (n.isPassword) null else n.contentDescription?.toString()?.take(180),
                "bounds" to arr(listOf(bounds.left, bounds.top, bounds.right, bounds.bottom)), "clickable" to n.isClickable,
                "editable" to n.isEditable, "scrollable" to n.isScrollable, "enabled" to n.isEnabled,
                "visible" to n.isVisibleToUser, "password" to n.isPassword)
            refs += NodeRef(path, signature(n)); nodes.put(item); estimate += item.toString().toByteArray().size
            for (i in 0 until n.childCount.coerceAtMost(300)) {
                if (refs.size >= 300 || estimate > 58000) { truncated = true; break }
                val child = n.getChild(i) ?: continue
                try { visit(child, path + i, depth + 1) } finally { child.recycle() }
            }
        }
        try {
            visit(root, emptyList(), 0)
            val id = nonce().take(24)
            observations[displayId] = Observation(id, SystemClock.elapsedRealtime(), root.windowId, root.packageName?.toString() ?: "", refs, displayInfo(displayId).toString(), epoch,displayId)
            obj("observation_id" to id, "observed_at_ms" to System.currentTimeMillis(), "valid_for_ms" to 30000,
                "package" to root.packageName?.toString(), "window_id" to root.windowId, "window_epoch" to epoch,
                "display" to displayInfo(displayId), "nodes" to nodes, "truncated" to truncated,
                "password_text_redacted" to true, "actions_require_observation_id" to true)
        } finally { root.recycle() }
    }
    private fun displayInfo(displayId: Int = Display.DEFAULT_DISPLAY): JSONObject {
        val manager = getSystemService(android.hardware.display.DisplayManager::class.java)
        val display = manager.getDisplay(displayId) ?: throw AgentError("ui_view_lost")
        val metrics = android.util.DisplayMetrics(); display.getRealMetrics(metrics)
        return obj("width" to metrics.widthPixels, "height" to metrics.heightPixels, "rotation" to display.rotation,"display_id" to displayId)
    }
    private fun currentMeta(args: JSONObject, persistent: Boolean = false): Observation {
        val seen = observations.values.firstOrNull { it.id == args.optString("observation_id") }
            ?: throw AgentError("observation_required", "先执行 ui_observe 或 android observe。")
        demand(args.optString("observation_id") == seen.id && (persistent || SystemClock.elapsedRealtime() - seen.at <= 30000), "stale_observation")
        demand(seen.display == displayInfo(seen.displayId).toString() && (!persistent || seen.epoch == epoch(seen.displayId)), "stale_observation")
        return seen
    }
    private fun current(args: JSONObject, persistent: Boolean = false): Pair<Observation, AccessibilityNodeInfo> {
        val seen = currentMeta(args,persistent)
        val root = rootFor(seen.displayId) ?: throw AgentError("screen_tree_unavailable")
        if (root.windowId != seen.window || root.packageName?.toString() != seen.pkg) { root.recycle(); throw AgentError("screen_changed_observe_again") }
        return seen to root
    }
    fun nodeAction(kind: String, args: JSONObject, persistent: Boolean = false): JSONObject = main {
        val (seen, root) = current(args, persistent)
        var node = root
        try {
            if (args.has("node_id")) {
                val index = args.getInt("node_id")
                demand(index in seen.nodes.indices, "unknown_node_id")
                val ref = seen.nodes[index]
                for (childIndex in ref.path) {
                    val next = node.getChild(childIndex) ?: throw AgentError("node_changed_observe_again")
                    node.recycle(); node = next
                }
                demand(node.refresh() && signature(node) == ref.signature, "node_changed_observe_again")
            } else {
                demand(persistent && kind in listOf("text", "insert"), "unknown_node_id")
                val focused = node.findFocus(AccessibilityNodeInfo.FOCUS_INPUT) ?: throw AgentError("focused_editor_unavailable")
                node.recycle(); node = focused
            }
            if (persistent) {
                // A label's ref may belong to its actionable ancestor, as in WebViews.
                fun actionable(n: AccessibilityNodeInfo) = when (kind) {
                    "click" -> n.isClickable; "long_click" -> n.isLongClickable
                    "text", "insert" -> n.isEditable; "scroll" -> n.isScrollable; else -> false
                }
                var depth = 0
                while (!actionable(node) && depth++ < 35) {
                    val parent = node.parent ?: break
                    if (parent.windowId != seen.window || parent.packageName?.toString() != seen.pkg) { parent.recycle(); break }
                    node.recycle(); node = parent
                }
            }
            demand(node.refresh() && node.isEnabled && node.isVisibleToUser, "node_changed_observe_again")
            val accepted = when (kind) {
                "click" -> node.performAction(AccessibilityNodeInfo.ACTION_CLICK)
                "long_click" -> node.performAction(AccessibilityNodeInfo.ACTION_LONG_CLICK)
                "text" -> {
                    demand(node.isEditable, "node_not_editable")
                    val text = args.getString("text"); demand(text.length <= 8192, "text_too_long")
                    node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, Bundle().apply { putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, text) })
                }
                "insert" -> {
                    demand(node.isEditable, "node_not_editable")
                    val text = args.getString("text"); demand(text.toByteArray().size <= 8192, "text_too_long")
                    val input = if (Build.VERSION.SDK_INT >= 33 && node.isFocused && inputMethod?.currentInputEditorInfo?.packageName == seen.pkg)
                        inputMethod?.currentInputConnection else null
                    if (Build.VERSION.SDK_INT >= 33 && input != null) { input.commitText(text, 1, null); true }
                    else {
                        demand(!node.isPassword, "editor_input_connection_required")
                        val merged = UiContract.insert(node.text?.toString() ?: "", node.textSelectionStart, node.textSelectionEnd, text)
                        val accepted = node.performAction(AccessibilityNodeInfo.ACTION_SET_TEXT, Bundle().apply {
                            putCharSequence(AccessibilityNodeInfo.ACTION_ARGUMENT_SET_TEXT_CHARSEQUENCE, merged)
                        })
                        if (accepted) {
                            val end = minOf(node.textSelectionStart, node.textSelectionEnd).coerceAtLeast(0) + text.length
                            node.performAction(AccessibilityNodeInfo.ACTION_SET_SELECTION, Bundle().apply {
                                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_START_INT, end)
                                putInt(AccessibilityNodeInfo.ACTION_ARGUMENT_SELECTION_END_INT, end)
                            })
                        }
                        accepted
                    }
                }
                "scroll" -> node.performAction(if (args.optString("direction", "forward") == "backward") AccessibilityNodeInfo.ACTION_SCROLL_BACKWARD else AccessibilityNodeInfo.ACTION_SCROLL_FORWARD)
                else -> throw AgentError("unsupported_node_action")
            }
            demand(accepted, "android_rejected_node_action")
            observations.remove(seen.displayId)
            obj("action" to kind, "accepted" to true, "verified" to false, "next_action" to "android observe")
        } finally { node.recycle() }
    }
    fun checkObservation(args: JSONObject) = main {
        val seen = currentMeta(args,true)
        if (seen.window < 0) {
            val root = rootFor(seen.displayId)
            try { demand(root == null,"stale_observation") } finally { root?.recycle() }
        } else current(args,true).second.recycle()
    }
    fun nodePoint(args: JSONObject): Pair<Double,Double> = main {
        val (seen,root) = current(args,true); var node = root
        try {
            val index = args.getInt("node_id"); demand(index in seen.nodes.indices,"unknown_node_id")
            val ref = seen.nodes[index]
            for (child in ref.path) {
                val next = node.getChild(child) ?: throw AgentError("node_changed_observe_again")
                node.recycle(); node = next
            }
            demand(node.refresh() && signature(node) == ref.signature && node.isEnabled && node.isVisibleToUser,"node_changed_observe_again")
            val bounds = Rect(); node.getBoundsInScreen(bounds)
            demand(bounds.width()>0 && bounds.height()>0,"invalid_coordinates")
            Pair(bounds.exactCenterX().toDouble(),bounds.exactCenterY().toDouble())
        } finally { node.recycle() }
    }

    fun uiKey(args: JSONObject, action: JSONObject): JSONObject = main {
        val (seen, root) = current(args, true)
        try {
            val key = action.getString("key").lowercase()
            val code = when (key) {
                "enter" -> KeyEvent.KEYCODE_ENTER; "tab" -> KeyEvent.KEYCODE_TAB
                "escape" -> KeyEvent.KEYCODE_ESCAPE; "backspace" -> KeyEvent.KEYCODE_DEL
                "delete" -> KeyEvent.KEYCODE_FORWARD_DEL; "space" -> KeyEvent.KEYCODE_SPACE
                "arrowleft", "left" -> KeyEvent.KEYCODE_DPAD_LEFT; "arrowright", "right" -> KeyEvent.KEYCODE_DPAD_RIGHT
                "arrowup", "up" -> KeyEvent.KEYCODE_DPAD_UP; "arrowdown", "down" -> KeyEvent.KEYCODE_DPAD_DOWN
                "home" -> KeyEvent.KEYCODE_MOVE_HOME; "end" -> KeyEvent.KEYCODE_MOVE_END
                "pageup" -> KeyEvent.KEYCODE_PAGE_UP; "pagedown" -> KeyEvent.KEYCODE_PAGE_DOWN
                else -> if (key.length == 1 && key[0] in 'a'..'z') KeyEvent.KEYCODE_A + (key[0] - 'a')
                    else if (key.length == 1 && key[0] in '0'..'9') KeyEvent.KEYCODE_0 + (key[0] - '0') else throw AgentError("unsupported_key")
            }
            if (Build.VERSION.SDK_INT < 33) throw AgentError("editor_input_connection_required")
            demand(inputMethod?.currentInputEditorInfo?.packageName == seen.pkg, "editor_input_connection_required")
            val connection = inputMethod?.currentInputConnection ?: throw AgentError("editor_input_connection_required")
            var meta = 0; val modifiers = action.optJSONArray("modifiers")
            if (modifiers != null) for (i in 0 until modifiers.length()) meta = meta or when (modifiers.getString(i)) {
                "shift" -> KeyEvent.META_SHIFT_ON; "ctrl" -> KeyEvent.META_CTRL_ON; "alt" -> KeyEvent.META_ALT_ON; else -> KeyEvent.META_META_ON
            }
            val at = SystemClock.uptimeMillis()
            connection.sendKeyEvent(KeyEvent(at, at, KeyEvent.ACTION_DOWN, code, 0, meta))
            connection.sendKeyEvent(KeyEvent(at, at, KeyEvent.ACTION_UP, code, 0, meta))
            observations.remove(seen.displayId)
            obj("accepted" to true, "verified" to false)
        } finally { root.recycle() }
    }

    fun uiGesture(kind: String, args: JSONObject): JSONObject {
        val future = CompletableFuture<Boolean>()
        main {
            val (seen, root) = current(args, true); root.recycle()
            val display = displayInfo(seen.displayId); val width = display.getInt("width"); val height = display.getInt("height")
            fun coordinate(key: String, bound: Int): Float {
                val value = args.getDouble(key)
                demand(value.isFinite() && value >= 0 && value <= bound - 1, "invalid_coordinates")
                return value.toFloat()
            }
            val x = coordinate("x", width); val y = coordinate("y", height)
            val builder = GestureDescription.Builder().setDisplayId(seen.displayId)
            fun stroke(x1: Float, y1: Float, x2: Float, y2: Float, start: Long, duration: Long) {
                val path = Path().apply { moveTo(x1,y1); if (x1 != x2 || y1 != y2) lineTo(x2,y2) }
                builder.addStroke(GestureDescription.StrokeDescription(path,start,duration))
            }
            when (kind) {
                "drag" -> stroke(x,y,coordinate("to_x",width),coordinate("to_y",height),0,400)
                "double_click" -> { stroke(x,y,x,y,0,60); stroke(x,y,x,y,140,60) }
                "long_press" -> stroke(x,y,x,y,0,650)
                "pinch" -> {
                    val scale = args.getDouble("scale"); demand(scale.isFinite() && scale > 0, "invalid_pinch_scale")
                    val radius = minOf(x,y,width-1-x,height-1-y) * 0.5f / maxOf(1.0,scale).toFloat()
                    demand(radius >= 1, "pinch_outside_display")
                    stroke(x-radius,y,x-radius*scale.toFloat(),y,0,450)
                    stroke(x+radius,y,x+radius*scale.toFloat(),y,0,450)
                }
                else -> stroke(x,y,x,y,0,60)
            }
            demand(dispatchGesture(builder.build(), object : GestureResultCallback() {
                override fun onCompleted(gestureDescription: GestureDescription?) { future.complete(true) }
                override fun onCancelled(gestureDescription: GestureDescription?) { future.complete(false) }
            }, handler), "gesture_rejected")
            observations.remove(seen.displayId)
        }
        demand(future.get(7,TimeUnit.SECONDS), "gesture_cancelled_outcome_unknown")
        return obj("gesture_completed" to true, "verified" to false)
    }

    fun uiSnapshot(): JSONObject {
        val observed = dump()
        val id = observed.getString("observation_id")
        val future = CompletableFuture<JSONObject>()
        main {
            scheduleScreenshot(object : TakeScreenshotCallback {
                override fun onSuccess(result: ScreenshotResult) {
                    val hardware = result.hardwareBuffer
                    try {
                        if (future.isCancelled) return
                        enabled()
                        val source = Bitmap.wrapHardwareBuffer(hardware, result.colorSpace) ?: throw AgentError("screenshot_bitmap_unavailable")
                        val bitmap = try { source.copy(Bitmap.Config.ARGB_8888,false) } finally { source.recycle() }
                        try {
                            val display = observed.getJSONObject("display")
                            demand(bitmap.width == display.getInt("width") && bitmap.height == display.getInt("height"), "stale_observation")
                            val image = UiImage.encode(bitmap)
                            checkObservation(obj("observation_id" to id))
                            future.complete(obj("tree" to observed,"image" to image))
                        } finally { bitmap.recycle() }
                    } catch (e: Exception) { future.completeExceptionally(e) }
                    finally { hardware.close() }
                }
                override fun onFailure(errorCode: Int) { future.completeExceptionally(AgentError("screenshot_denied_$errorCode")) }
            })
        }
        try { return future.get(10,TimeUnit.SECONDS) } catch(e: Exception) { future.cancel(false); throw e }
    }
    fun gesture(kind: String, args: JSONObject): JSONObject {
        val future = CompletableFuture<Boolean>()
        main {
            val (seen, root) = current(args); root.recycle()
            val display = displayInfo(); val width = display.getInt("width"); val height = display.getInt("height")
            fun coord(key: String, upper: Int): Float {
                val v = args.getDouble(key)
                demand(v.isFinite() && v >= 0 && v < upper, "gesture_outside_display")
                return v.toFloat()
            }
            val x = coord("x", width); val y = coord("y", height)
            val path = Path().apply { moveTo(x, y); if (kind == "swipe") lineTo(coord("to_x", width), coord("to_y", height)) }
            val duration = args.optLong("duration_ms", if (kind == "swipe") 400 else 80)
            demand(duration in 30..5000, "invalid_gesture_duration")
            val gesture = GestureDescription.Builder().addStroke(GestureDescription.StrokeDescription(path, 0, duration)).build()
            val submitted = dispatchGesture(gesture, object : GestureResultCallback() {
                override fun onCompleted(gestureDescription: GestureDescription?) { future.complete(true) }
                override fun onCancelled(gestureDescription: GestureDescription?) { future.complete(false) }
            }, handler)
            demand(submitted, "gesture_rejected")
            observations.remove(seen.displayId)
        }
        demand(future.get(7, TimeUnit.SECONDS), "gesture_cancelled")
        return obj("action" to kind, "gesture_completed" to true, "verified" to false, "next_action" to "android observe")
    }
    fun global(action: String): JSONObject = main {
        val code = when (action) {
            "back" -> GLOBAL_ACTION_BACK; "home" -> GLOBAL_ACTION_HOME; "recents" -> GLOBAL_ACTION_RECENTS
            "notifications" -> GLOBAL_ACTION_NOTIFICATIONS; "quick_settings" -> GLOBAL_ACTION_QUICK_SETTINGS
            "lock" -> GLOBAL_ACTION_LOCK_SCREEN
            else -> throw AgentError("unsupported_global_action")
        }
        demand(performGlobalAction(code), "global_action_rejected")
        observations.remove(Display.DEFAULT_DISPLAY)
        obj("action" to action, "accepted" to true, "verified" to false, "next_action" to "android observe")
    }
    fun screenshot(root: File, operation: String): JSONObject {
        val observed = dump()
        val directory = File(root, "screenshots").apply { mkdirs() }
        val file = File(directory, "${Policy.uuid(operation)}.png")
        demand(!file.exists(), "screenshot_path_exists")
        val future = CompletableFuture<JSONObject>()
        main {
            scheduleScreenshot(object : TakeScreenshotCallback {
                override fun onSuccess(result: ScreenshotResult) {
                    val hardware = result.hardwareBuffer
                    try {
                        if (future.isCancelled) return
                        enabled()
                        val source = Bitmap.wrapHardwareBuffer(hardware, result.colorSpace) ?: throw AgentError("screenshot_bitmap_unavailable")
                        val bitmap = try { source.copy(Bitmap.Config.ARGB_8888, false) } finally { source.recycle() }
                        try {
                            FileOutputStream(file).use { output -> demand(bitmap.compress(Bitmap.CompressFormat.PNG, 100, output), "screenshot_encode_failed"); output.fd.sync() }
                            val receipt = obj("path" to "screenshots/${file.name}", "width" to bitmap.width, "height" to bitmap.height,
                                "size" to file.length(), "observation_id" to observed.getString("observation_id"), "package" to observed.opt("package"),
                                "captured_at_ms" to System.currentTimeMillis(), "secure_windows_bypassed" to false)
                            if (!future.complete(receipt)) file.delete()
                        } finally { bitmap.recycle() }
                    } catch (e: Exception) { file.delete(); future.completeExceptionally(e) }
                    finally { hardware.close() }
                }
                override fun onFailure(errorCode: Int) { future.completeExceptionally(AgentError("screenshot_denied_$errorCode", "系统拒绝截图，可能是安全窗口、锁屏或请求间隔过短。")) }
            })
        }
        try { return future.get(10, TimeUnit.SECONDS) }
        catch (e: Exception) { future.cancel(false); throw e }
    }
    companion object { @Volatile var instance: AccessService? = null; private set }
}
