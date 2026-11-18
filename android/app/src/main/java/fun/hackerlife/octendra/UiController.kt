package `fun`.hackerlife.octendra

import android.content.Context
import android.os.Build
import android.os.SystemClock
import android.view.Display
import android.view.KeyEvent
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID

/** Native Android adapter for the shared UI tools; uses the existing device journal. */
class UiController internal constructor(
    private val deviceId: String,
    private val backend: Backend,
    private val aiViewHost: AiViewHost = AiViewHost.shared
) : AutoCloseable {

    constructor(context: Context, config: GatewayConfig) : this(
        deviceId = config.deviceId,
        backend = AndroidBackend(context),
        aiViewHost = AiViewHost.shared
    )

    internal interface Backend : AutoCloseable {
        fun backgroundAvailable(): Boolean
        fun shellConnected(): Boolean
        fun ensureBackground(): Int
        fun isBackgroundCurrent(view: String, generation: String): Boolean
        fun backgroundDisplayId(): Int
        fun backgroundGeneration(): String
        fun backgroundFrameSequence(): Long
        fun awaitBackgroundFrameAfter(previous: Long, timeoutMs: Long): Boolean
        fun pokeBackgroundKeepAwake()
        fun mainTopTaskId(): Int
        fun pullSpilledTasks(packageName: String, preMainTopTaskId: Int)
        fun backgroundSnapshot(): Pair<JSONObject, List<Long>>?
        fun backgroundLaunch(app: String): JSONObject
        fun backgroundKey(code: Int, modifiers: Int): JSONObject
        fun backgroundMotion(kind: String, x: Double, y: Double, toX: Double, toY: Double, scale: Double): JSONObject
        fun currentEpoch(displayId: Int): Long
        fun dump(displayId: Int): JSONObject
        fun checkObservation(args: JSONObject)
        fun mainSnapshot(): JSONObject
        fun nodePoint(args: JSONObject): Pair<Double, Double>
        fun nodeAction(kind: String, args: JSONObject, persistent: Boolean): JSONObject
        fun uiGesture(kind: String, args: JSONObject): JSONObject
        fun uiKey(args: JSONObject, action: JSONObject): JSONObject
        fun globalAction(action: String): JSONObject
        fun launchMainApp(app: String): JSONObject
        fun sleep(ms: Long)
        fun elapsedRealtime(): Long
    }

    private data class Seen(
        val owner: String,
        val id: String,
        val native: String,
        val imageWidth: Int,
        val imageHeight: Int,
        val displayWidth: Int,
        val displayHeight: Int,
        val view: String,
        val generation: String?,
        val resourceEpoch: Long,
        val packageName: String,
        val elements: JSONArray,
        val tileHashes: List<Long>,
        val response: JSONObject
    )

    private val observations = HashMap<String, Seen>()
    private val observationCache = object : LinkedHashMap<String, Seen>(32, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<String, Seen>?): Boolean = size > 32
    }

    @Synchronized fun perform(job: JSONObject): JSONObject {
        val args = job.getJSONObject("arguments")
        demand(args.getString("device") == deviceId, "ui_device_identity_mismatch")
        val tool = job.getString("tool"); val owner = job.getString("owner")
        val allowed = if (tool == "ui_observe") {
            setOf("device", "view", "context", "target", "query", "observation_id", "idempotency_key", "response_mode")
        } else {
            setOf("device", "observation_id", "action", "observe_after", "expect", "idempotency_key", "response_mode")
        }
        demand(args.keys().asSequence().all { it in allowed }, "invalid_ui_arguments")

        if (tool == "ui_observe") {
            val query = if (args.has("query") && !args.isNull("query")) {
                args.getJSONObject("query").also { UiContract.validateQuery(it) }
            } else null
            val requestedObsId = if (args.has("observation_id") && !args.isNull("observation_id")) {
                args.getString("observation_id").takeIf { it.isNotEmpty() }
            } else null

            if (requestedObsId != null) {
                val cached = observationCache[requestedObsId]?.takeIf { it.owner == owner }
                    ?: return refreshFailure(owner, "stale_observation", resolveRequestedView(args))
                if (cached.generation != null) {
                    if (!backend.isBackgroundCurrent(cached.view, cached.generation)) {
                        return refreshFailure(owner, "stale_observation", "background")
                    }
                    val takeover = aiViewHost.checkTakeoverForAi(cached.view, cached.generation, cached.resourceEpoch)
                    if (takeover != null) {
                        return refreshTakeoverFailure(owner, cached.view, takeover.first, takeover.second)
                    }
                }
                // Zero OS I/O: filter the saved observation in-memory without consuming or overwriting observations.
                val copy = JSONObject(cached.response.toString())
                if (query != null) {
                    copy.put("elements", UiContract.filterElements(cached.elements, query))
                }
                return copy
            }

            val observed = observe(owner, resolveRequestedView(args))
            if (query != null) {
                val copy = JSONObject(observed.toString())
                copy.put("elements", UiContract.filterElements(observed.getJSONArray("elements"), query))
                return copy
            }
            return observed
        }

        val action = args.getJSONObject("action"); UiContract.validate(action)
        val expect = if (args.has("expect") && !args.isNull("expect")) {
            args.getJSONObject("expect").also { UiContract.validateExpect(it) }
        } else null
        demand(expect == null || args.optBoolean("observe_after", true), "expect_requires_observe_after")
        if (action.getString("type") !in UiContract.actions) return failure("unsupported_action")

        val obsId = args.getString("observation_id")
        val current = observations.values.firstOrNull { it.owner == owner && it.id == obsId }
        if (current == null || current.owner != owner || current.id != obsId) {
            val cached = observationCache[obsId]?.takeIf { it.owner == owner }
            if (cached != null && cached.generation != null) {
                val takeover = aiViewHost.checkTakeoverForAi(cached.view, cached.generation, cached.resourceEpoch)
                if (takeover != null) {
                    return refreshTakeoverFailure(owner, cached.view, takeover.first, takeover.second)
                }
            }
            return refreshFailure(owner, "stale_observation")
        }
        if (current.generation != null) {
            if (!backend.isBackgroundCurrent(current.view, current.generation)) {
                return refreshFailure(owner, "stale_observation", "background")
            }
            val takeover = aiViewHost.checkTakeoverForAi(current.view, current.generation, current.resourceEpoch)
            if (takeover != null) {
                observations.remove(current.view)
                return refreshTakeoverFailure(owner, current.view, takeover.first, takeover.second)
            }
        }

        // Never reuse an observation after any action with an uncertain effect.
        observations.remove(current.view)
        val targetDisplayId = if (current.generation == null) Display.DEFAULT_DISPLAY else backend.backgroundDisplayId()
        val preEpoch = runCatching { backend.currentEpoch(targetDisplayId) }.getOrDefault(0L)
        val preFrameSeq = if (current.generation != null) backend.backgroundFrameSequence() else 0L
        val preMainTopTask = if (current.generation != null) backend.mainTopTaskId() else -1
        val actionType = action.getString("type")
        val spillTargetPackage = if (actionType == "launch_app") action.getString("app") else current.packageName

        if (current.generation != null) {
            aiViewHost.bumpResourceEpochForAi(current.view, current.generation)
        }

        val native = obj("observation_id" to current.native)
        val result = try {
            act(action, native, current)
        } catch (error: Exception) {
            val value = Policy.error(error)
            val errCode = value.optString("error")
            if (current.generation != null && (errCode == "takeover_by_human" ||
                    errCode in listOf("stale_observation", "screen_changed_observe_again", "node_changed_observe_again", "observation_required"))
            ) {
                val takeover = aiViewHost.checkTakeoverForAi(current.view, current.generation, current.resourceEpoch)
                if (errCode == "takeover_by_human" || takeover != null) {
                    return refreshTakeoverFailure(
                        owner,
                        current.view,
                        takeover?.first ?: aiViewHost.isHumanActive(current.view),
                        takeover?.second ?: aiViewHost.currentResourceEpoch(current.view)
                    )
                }
            }
            if (errCode in listOf("stale_observation", "screen_changed_observe_again", "node_changed_observe_again", "observation_required")) {
                return refreshFailure(owner, "stale_observation", current.view)
            }
            if (errCode in listOf(
                    "app_in_use_on_main", "app_launch_rejected", "unsupported_action",
                    "unsupported_key", "invalid_coordinates", "editor_input_connection_required", "focused_editor_unavailable"
                )) {
                value.put("input_sent", false)
            }
            value
        }

        result.put("operation_id", job.getString("id")).put("effect_confirmed", false)
        val alreadySatisfied = result.optBoolean("already_satisfied", false)
        if (args.optBoolean("observe_after", true) && result.optString("state") != "outcome_unknown" && !result.has("error")) {
            if (!alreadySatisfied) {
                settleAfterAction(actionType, current, targetDisplayId, preEpoch, preFrameSeq)
            }
            if (current.generation != null && !alreadySatisfied && spillTargetPackage.isNotBlank()) {
                backend.pullSpilledTasks(spillTargetPackage, preMainTopTask)
            }
        }
        if (args.optBoolean("observe_after", true) && result.optString("state") != "outcome_unknown") {
            try {
                val postObs = observe(owner, current.view)
                result.put("observation", postObs)
                val postSeen = observations[current.view]
                val visualChanged = if (current.tileHashes.isNotEmpty() && postSeen != null && postSeen.tileHashes.isNotEmpty()) {
                    current.tileHashes != postSeen.tileHashes
                } else null
                if (visualChanged != null && !result.has("error")) {
                    result.put("visual_changed", visualChanged)
                }
                if (expect != null && !result.has("error")) {
                    val met = UiContract.evaluateExpect(
                        expect = expect,
                        preElements = current.elements,
                        postElements = postObs.getJSONArray("elements"),
                        actionTarget = action.optJSONObject("target"),
                        visualChanged = visualChanged,
                        actionType = actionType
                    )
                    result.put("expect_met", met)
                }
            } catch (error: Exception) {
                result.put("observation_error", Policy.error(error).put("next_action", "ui_observe"))
                if (expect != null && !result.has("error")) {
                    result.put("expect_met", false)
                }
            }
        }
        return result
    }

    private fun resolveRequestedView(args: JSONObject): String {
        if (args.has("view") && !args.isNull("view")) return args.getString("view")
        val target = args.optJSONObject("target")
        if (target != null) {
            if (target.has("view") && !target.isNull("view")) return target.getString("view")
            if (target.has("instance") && !target.isNull("instance")) return target.getString("instance")
            if (target.has("window") && !target.isNull("window")) return target.getString("window")
        }
        return "active"
    }

    private fun settleAfterAction(type: String, seen: Seen, displayId: Int, preEpoch: Long, preFrameSeq: Long) {
        if (type == "launch_app") {
            if (seen.generation != null) {
                backend.awaitBackgroundFrameAfter(preFrameSeq, 900L)
                backend.sleep(160L)
            } else {
                val deadline = backend.elapsedRealtime() + 800L
                while (backend.elapsedRealtime() < deadline) {
                    backend.sleep(100L)
                    if (runCatching { backend.currentEpoch(displayId) }.getOrDefault(preEpoch) != preEpoch) {
                        backend.sleep(140L)
                        break
                    }
                }
            }
            return
        }
        backend.sleep(140L)
        if (seen.generation != null) {
            backend.awaitBackgroundFrameAfter(preFrameSeq, 280L)
        } else {
            val midEpoch = runCatching { backend.currentEpoch(displayId) }.getOrDefault(preEpoch)
            if (midEpoch != preEpoch) backend.sleep(140L)
        }
    }

    private fun failure(code: String) = obj(
        "error" to code,
        "input_sent" to false,
        "supported_actions" to arr(UiContract.actions),
        "next_action" to "ui_observe"
    )

    private fun refreshFailure(owner: String, code: String, view: String = "active"): JSONObject {
        val result = failure(code)
        try { result.put("observation", observe(owner, view)).put("next_action", "ui_act") }
        catch (error: Exception) { result.put("observation_error", Policy.error(error)) }
        return result
    }

    private fun refreshTakeoverFailure(owner: String, view: String, humanActive: Boolean, resourceEpoch: Long): JSONObject {
        val result = failure("takeover_by_human")
            .put("human_active", humanActive)
            .put("resource_epoch", resourceEpoch)
            .put("takeover_state", if (humanActive) "paused_by_human" else "human_took_over")
        try { result.put("observation", observe(owner, view)) }
        catch (error: Exception) { result.put("observation_error", Policy.error(error)) }
        return result
    }

    private fun observe(owner: String, view: String): JSONObject {
        try { return observeOnce(owner, view) }
        catch (error: Exception) {
            if (Policy.error(error).optString("error") !in listOf("stale_observation", "screen_changed_observe_again")) throw error
            // A launch/window transition can finish while its frame is captured.
            // Only repeat the read, never the action that caused the transition.
            return observeOnce(owner, view)
        }
    }

    private fun observeOnce(owner: String, view: String): JSONObject {
        val isBackground = view != "active" && view != "display:0"
        val displayId = if (!isBackground) Display.DEFAULT_DISPLAY else if (view == "background") backend.ensureBackground()
        else if (view == "display:${backend.backgroundDisplayId()}") {
            val id = backend.ensureBackground(); demand(view == "display:$id", "ui_view_lost"); id
        } else throw AgentError("unsupported_view")

        val snapshot: JSONObject
        val tileHashes: List<Long>
        if (!isBackground) {
            snapshot = backend.mainSnapshot()
            val arr = snapshot.optJSONArray("tile_hashes")
            tileHashes = if (arr != null) List(arr.length()) { arr.getLong(it) } else emptyList()
        } else {
            backend.pokeBackgroundKeepAwake()
            val tree = backend.dump(displayId)
            val snap = backend.backgroundSnapshot()
            backend.checkObservation(obj("observation_id" to tree.getString("observation_id")))
            snapshot = obj("tree" to tree, "image" to snap?.first)
            tileHashes = snap?.second ?: emptyList()
        }

        val tree = snapshot.getJSONObject("tree")
        val image = snapshot.optJSONObject("image")
        val display = tree.getJSONObject("display")
        val id = UUID.randomUUID().toString()
        val generation = if (isBackground) backend.backgroundGeneration() else null
        val viewKey = "display:$displayId"
        if (generation != null) observations.entries.removeAll { it.value.generation != null && it.value.generation != generation }

        val nodes = tree.getJSONArray("nodes")
        val dispW = display.getInt("width"); val dispH = display.getInt("height")
        val displayState = display.optInt("state", Display.STATE_ON)
        for (i in 0 until nodes.length()) {
            val node = nodes.getJSONObject(i)
            node.put("ref", "n${node.getInt("id")}"); node.remove("id")
            if (image != null) {
                val bounds = node.getJSONArray("bounds")
                val scaled = UiContract.scaleBounds(
                    bounds.getInt(0), bounds.getInt(1), bounds.getInt(2), bounds.getInt(3),
                    dispW, dispH, image.getInt("width"), image.getInt("height")
                )
                node.put("bounds", arr(scaled))
            } else node.remove("bounds")
        }

        val blankOrSecure = image?.optBoolean("blank_or_secure") == true
        val imageStatus = UiContract.classifyDisplayObservation(image != null, displayState, blankOrSecure)
        val imageUnavailable = when (imageStatus) {
            "display_sleeping" -> obj("reason" to "display_sleeping", "next_action" to if (isBackground) "wake_display" else "wake_device")
            "view_has_not_rendered" -> obj("reason" to "view_has_not_rendered", "next_action" to "launch_app")
            else -> null
        }
        val secureHint = if (imageStatus == "blank_or_secure") {
            "Screen image appears completely black (possibly FLAG_SECURE protected); inspect semantic elements or ask the user to complete sensitive input on device."
        } else null

        val resourceEpoch = if (isBackground) aiViewHost.currentResourceEpoch(viewKey) else 0L
        val humanActive = if (isBackground) aiViewHost.isHumanActive(viewKey) else false
        val pkgName = tree.optString("package", "")

        val response = obj(
            "observation_id" to id,
            "device" to deviceId,
            "view" to viewKey,
            "image" to image,
            "image_status" to imageStatus,
            "source" to obj(
                "kind" to if (isBackground) "ai_display" else "main_display",
                "display_id" to displayId,
                "generation" to generation,
                "resource_epoch" to if (isBackground) resourceEpoch else null,
                "rotation" to display.getInt("rotation"),
                "width" to dispW,
                "height" to dispH
            ),
            "image_unavailable" to imageUnavailable,
            "takeover_hint" to secureHint,
            "human_active" to if (isBackground) humanActive else false,
            "takeover_state" to if (humanActive) "paused_by_human" else null,
            "elements" to nodes,
            "package" to tree.opt("package"),
            "truncated" to tree.optBoolean("truncated"),
            "capabilities" to obj(
                "backend" to "android_accessibility",
                "actions" to arr(UiContract.actions),
                "semantic_elements" to true,
                "coordinates" to "image_pixels",
                "background" to backend.backgroundAvailable(),
                "same_app_concurrency" to false,
                "current_view_background" to isBackground
            ),
            "readiness" to obj(
                "available" to true,
                "accessibility" to true,
                "own_focus" to isBackground,
                "shell_connected" to backend.shellConnected()
            ),
            "observed_at_ms" to System.currentTimeMillis()
        )

        val seen = Seen(
            owner = owner,
            id = id,
            native = tree.getString("observation_id"),
            imageWidth = image?.optInt("width") ?: 0,
            imageHeight = image?.optInt("height") ?: 0,
            displayWidth = dispW,
            displayHeight = dispH,
            view = viewKey,
            generation = generation,
            resourceEpoch = resourceEpoch,
            packageName = pkgName,
            elements = JSONArray(nodes.toString()),
            tileHashes = tileHashes,
            response = response
        )
        observations[viewKey] = seen
        observationCache[id] = seen
        return JSONObject(response.toString())
    }

    private fun node(target: JSONObject, native: JSONObject): JSONObject {
        UiContract.target(target)
        val ref = target.getString("ref")
        demand(ref.matches(Regex("n[0-9]+")), "unknown_element_ref")
        return native.put("node_id", ref.drop(1).toInt())
    }

    private fun point(target: JSONObject, seen: Seen): Pair<Double, Double> = if (target.has("ref"))
        backend.nodePoint(node(target, obj("observation_id" to seen.native)))
    else UiContract.point(target, seen.imageWidth, seen.imageHeight, seen.displayWidth, seen.displayHeight)

    private fun findCheckableElementAt(elements: JSONArray, displayX: Double, displayY: Double, seen: Seen): JSONObject? {
        if (seen.imageWidth <= 0 || seen.imageHeight <= 0) return null
        val imgX = displayX * (seen.imageWidth - 1) / maxOf(1, seen.displayWidth - 1)
        val imgY = displayY * (seen.imageHeight - 1) / maxOf(1, seen.displayHeight - 1)
        var best: JSONObject? = null
        var bestArea = Long.MAX_VALUE
        for (i in 0 until elements.length()) {
            val el = elements.getJSONObject(i)
            if (!el.optBoolean("checkable")) continue
            val b = el.optJSONArray("bounds") ?: continue
            if (b.length() != 4) continue
            val l = b.getInt(0); val t = b.getInt(1); val r = b.getInt(2); val bt = b.getInt(3)
            if (imgX >= l && imgX <= r && imgY >= t && imgY <= bt) {
                val area = (r - l).toLong() * (bt - t).toLong()
                if (area < bestArea) {
                    bestArea = area
                    best = el
                }
            }
        }
        return best
    }

    private fun dispatchGesture(kind: String, native: JSONObject, seen: Seen): JSONObject {
        if (seen.generation != null) {
            backend.checkObservation(native)
            return backend.backgroundMotion(
                kind,
                native.getDouble("x"),
                native.getDouble("y"),
                native.optDouble("to_x", native.getDouble("x")),
                native.optDouble("to_y", native.getDouble("y")),
                native.optDouble("scale", 1.0)
            )
        }
        return backend.uiGesture(kind, native)
    }

    private fun act(action: JSONObject, native: JSONObject, seen: Seen): JSONObject {
        val type = action.getString("type")
        val target = action.optJSONObject("target")
        val result = when (type) {
            "click", "long_press" -> if (target!!.has("ref")) {
                backend.nodeAction(if (type == "click") "click" else "long_click", node(target, native), true)
            } else {
                val (x, y) = point(target, seen)
                dispatchGesture(type, native.put("x", x).put("y", y), seen)
            }

            "double_click", "triple_click" -> {
                val (x, y) = point(target!!, seen)
                dispatchGesture(type, native.put("x", x).put("y", y), seen)
            }

            "set_checked" -> {
                val desired = action.getBoolean("checked")
                if (target!!.has("ref")) {
                    node(target, native)
                    native.put("checked", desired)
                    backend.nodeAction("set_checked", native, true)
                } else {
                    val (x, y) = point(target, seen)
                    val matched = findCheckableElementAt(seen.elements, x, y, seen)
                    if (matched != null && !matched.isNull("checked") && matched.optBoolean("checked") == desired) {
                        obj(
                            "action" to "set_checked",
                            "accepted" to true,
                            "already_satisfied" to true,
                            "checked" to desired,
                            "input_sent" to false,
                            "verified" to true
                        )
                    } else {
                        dispatchGesture("click", native.put("x", x).put("y", y), seen)
                            .put("already_satisfied", false)
                            .put("checked", desired)
                    }
                }
            }

            "type_text", "fill" -> {
                if (target != null && target.has("ref")) {
                    node(target, native)
                    backend.nodeAction(if (type == "fill") "text" else "insert", native.put("text", action.getString("text")), true)
                } else {
                    if (target != null) {
                        val (x, y) = point(target, seen)
                        dispatchGesture("click", native.put("x", x).put("y", y), seen)
                        backend.sleep(160L)
                        val refreshed = backend.dump(if (seen.generation == null) Display.DEFAULT_DISPLAY else backend.backgroundDisplayId())
                        native.put("observation_id", refreshed.getString("observation_id"))
                        native.remove("x"); native.remove("y")
                    }
                    backend.nodeAction(if (type == "fill") "text" else "insert", native.put("text", action.getString("text")), true)
                }
            }

            "scroll" -> if (target?.has("ref") == true && action.getString("direction") in listOf("up", "down")) {
                backend.nodeAction(
                    "scroll",
                    node(target, native).put("direction", if (action.getString("direction") == "down") "forward" else "backward"),
                    true
                )
            } else {
                val center = if (target != null) point(target, seen) else Pair(seen.displayWidth / 2.0, seen.displayHeight / 2.0)
                val vertical = action.getString("direction") in listOf("up", "down")
                val distance = (if (vertical) seen.displayHeight else seen.displayWidth) * 0.25
                val sign = if (action.getString("direction") in listOf("down", "right")) 1 else -1
                val dx = if (vertical) 0.0 else distance * sign; val dy = if (vertical) distance * sign else 0.0
                dispatchGesture(
                    "drag",
                    native.put("x", (center.first + dx).coerceIn(0.0, seen.displayWidth - 1.0))
                        .put("y", (center.second + dy).coerceIn(0.0, seen.displayHeight - 1.0))
                        .put("to_x", (center.first - dx).coerceIn(0.0, seen.displayWidth - 1.0))
                        .put("to_y", (center.second - dy).coerceIn(0.0, seen.displayHeight - 1.0)),
                    seen
                )
            }

            "drag" -> {
                val (x, y) = point(action.getJSONObject("from"), seen); val (tx, ty) = point(action.getJSONObject("to"), seen)
                dispatchGesture("drag", native.put("x", x).put("y", y).put("to_x", tx).put("to_y", ty), seen)
            }

            "pinch" -> {
                val (x, y) = point(action.getJSONObject("center"), seen)
                dispatchGesture("pinch", native.put("x", x).put("y", y).put("scale", action.getDouble("scale")), seen)
            }

            "key" -> if (seen.generation == null) backend.uiKey(native, action) else {
                backend.checkObservation(native)
                val (code, modifiers) = UiContract.key(action); backend.backgroundKey(code, modifiers)
            }

            "launch_app" -> {
                // Explicit app launch uses the live view binding, not transient page contents.
                if (seen.generation == null) backend.launchMainApp(action.getString("app"))
                else backend.backgroundLaunch(action.getString("app"))
            }

            "navigate_back", "go_home", "app_switcher" -> {
                backend.checkObservation(native)
                if (seen.generation == null) backend.globalAction(when (type) {
                    "navigate_back" -> "back"; "go_home" -> "home"; else -> "recents"
                })
                else backend.backgroundKey(
                    when (type) {
                        "navigate_back" -> KeyEvent.KEYCODE_BACK
                        "go_home" -> KeyEvent.KEYCODE_HOME
                        else -> KeyEvent.KEYCODE_APP_SWITCH
                    },
                    0
                )
            }

            else -> throw AgentError("unsupported_action")
        }
        if (!result.has("input_sent")) result.put("input_sent", true)
        return result
    }

    override fun close() {
        observations.clear()
        observationCache.clear()
        backend.close()
    }

    private class AndroidBackend(private val context: Context) : Backend {
        private val background = lazy { BackgroundView(context) }
        private fun screen() = AccessService.instance
            ?: throw AgentError("accessibility_permission_required", "系统尚未连接 Octendra 无障碍服务。")

        override fun backgroundAvailable() = Build.VERSION.SDK_INT >= 34 && AgentRuntime.bridge?.ui() != null
        override fun shellConnected() = AgentRuntime.bridge?.connected() == true
        override fun ensureBackground() = background.value.ensure()
        override fun isBackgroundCurrent(view: String, generation: String) =
            background.isInitialized() && background.value.current(view, generation)
        override fun backgroundDisplayId() = background.value.displayId
        override fun backgroundGeneration() = background.value.generation
        override fun backgroundFrameSequence() = background.value.frameSequence
        override fun awaitBackgroundFrameAfter(previous: Long, timeoutMs: Long) =
            background.value.awaitFrameAfter(previous, timeoutMs)
        override fun pokeBackgroundKeepAwake() { if (background.isInitialized()) background.value.pokeKeepAwake() }
        override fun mainTopTaskId() = if (background.isInitialized()) background.value.mainTopTaskId() else -1
        override fun pullSpilledTasks(packageName: String, preMainTopTaskId: Int) {
            if (background.isInitialized()) background.value.pullSpilledTasks(packageName, preMainTopTaskId)
        }
        override fun backgroundSnapshot() = background.value.snapshotWithTiles()
        override fun backgroundLaunch(app: String) = background.value.launch(app)
        override fun backgroundKey(code: Int, modifiers: Int) = background.value.key(code, modifiers)
        override fun backgroundMotion(kind: String, x: Double, y: Double, toX: Double, toY: Double, scale: Double) =
            background.value.motion(kind, x, y, toX, toY, scale)
        override fun currentEpoch(displayId: Int) = screen().currentEpoch(displayId)
        override fun dump(displayId: Int) = screen().dump(displayId)
        override fun checkObservation(args: JSONObject) = screen().checkObservation(args)
        override fun mainSnapshot() = screen().uiSnapshot()
        override fun nodePoint(args: JSONObject) = screen().nodePoint(args)
        override fun nodeAction(kind: String, args: JSONObject, persistent: Boolean) = screen().nodeAction(kind, args, persistent)
        override fun uiGesture(kind: String, args: JSONObject) = screen().uiGesture(kind, args)
        override fun uiKey(args: JSONObject, action: JSONObject) = screen().uiKey(args, action)
        override fun globalAction(action: String) = screen().global(action)
        override fun launchMainApp(app: String) = PackageActions(context).launch(obj("package" to app))
        override fun sleep(ms: Long) = SystemClock.sleep(ms)
        override fun elapsedRealtime() = SystemClock.elapsedRealtime()
        override fun close() { if (background.isInitialized()) background.value.close() }
    }
}
