package `fun`.hackerlife.octendra

import android.content.Context
import org.json.JSONObject
import java.util.UUID

/** Native Android adapter for the shared UI tools; uses the existing device journal. */
class UiController(private val context: Context, private val config: GatewayConfig) : AutoCloseable {
    private data class Seen(val owner: String, val id: String, val native: String, val imageWidth: Int, val imageHeight: Int,
        val displayWidth: Int, val displayHeight: Int, val view: String, val generation: String?)
    private val observations = HashMap<String,Seen>()
    private val background = lazy { BackgroundView(context) }
    private fun screen() = AccessService.instance ?: throw AgentError("accessibility_permission_required", "系统尚未连接 Octendra 无障碍服务。")
    @Synchronized fun perform(job: JSONObject): JSONObject {
        val args = job.getJSONObject("arguments")
        demand(args.getString("device") == config.deviceId, "ui_device_identity_mismatch")
        val tool = job.getString("tool"); val owner = job.getString("owner")
        val allowed = if (tool == "ui_observe") setOf("device", "view", "idempotency_key", "response_mode")
            else setOf("device", "observation_id", "action", "observe_after", "idempotency_key", "response_mode")
        demand(args.keys().asSequence().all { it in allowed }, "invalid_ui_arguments")
        if (tool == "ui_observe") {
            return observe(owner,args.optString("view","active"))
        }
        val action = args.getJSONObject("action"); UiContract.validate(action)
        if (action.getString("type") !in UiContract.actions) return failure("unsupported_action")
        val current = observations.values.firstOrNull { it.owner == owner && it.id == args.getString("observation_id") }
        if (current == null || current.owner != owner || current.id != args.getString("observation_id")) return refreshFailure(owner, "stale_observation")
        if (current.generation != null && !background.value.current(current.view,current.generation)) return refreshFailure(owner,"stale_observation","background")
        // Never reuse an observation after any action with an uncertain effect.
        observations.remove(current.view)
        val native = obj("observation_id" to current.native)
        val result = try { act(action, native, current) } catch (error: Exception) {
            val value = Policy.error(error)
            if (value.optString("error") in listOf("stale_observation", "screen_changed_observe_again", "node_changed_observe_again", "observation_required"))
                return refreshFailure(owner, "stale_observation",current.view)
            if (value.optString("error") in listOf("app_in_use_on_main","app_launch_rejected","unsupported_action","unsupported_key","invalid_coordinates", "editor_input_connection_required","focused_editor_unavailable")) value.put("input_sent",false)
            value
        }
        result.put("operation_id", job.getString("id")).put("effect_confirmed", false)
        if (args.optBoolean("observe_after", true) && result.optString("state") != "outcome_unknown") {
            try { result.put("observation", observe(owner,current.view)) }
            catch (error: Exception) { result.put("observation_error", Policy.error(error).put("next_action", "ui_observe")) }
        }
        return result
    }
    private fun failure(code: String) = obj("error" to code, "input_sent" to false,
        "supported_actions" to arr(UiContract.actions), "next_action" to "ui_observe")
    private fun refreshFailure(owner: String, code: String, view: String = "active"): JSONObject {
        val result = failure(code)
        try { result.put("observation", observe(owner,view)).put("next_action","ui_act") } catch (error: Exception) { result.put("observation_error", Policy.error(error)) }
        return result
    }
    private fun observe(owner: String, view: String): JSONObject {
        try { return observeOnce(owner,view) }
        catch (error: Exception) {
            if (Policy.error(error).optString("error") !in listOf("stale_observation","screen_changed_observe_again")) throw error
            // A launch/window transition can finish while its frame is captured.
            // Only repeat the read, never the action that caused the transition.
            return observeOnce(owner,view)
        }
    }
    private fun observeOnce(owner: String, view: String): JSONObject {
        val isBackground = view != "active" && view != "display:0"
        val displayId = if (!isBackground) 0 else if (view == "background") background.value.ensure()
            else if (background.isInitialized() && view == "display:${background.value.displayId}") {
                val id = background.value.ensure(); demand(view == "display:$id","ui_view_lost"); id
            }
            else throw AgentError("unsupported_view")
        val snapshot = if (!isBackground) screen().uiSnapshot() else {
            val tree = screen().dump(displayId)
            val image = background.value.snapshot()
            screen().checkObservation(obj("observation_id" to tree.getString("observation_id")))
            obj("tree" to tree,"image" to image)
        }
        val tree = snapshot.getJSONObject("tree"); val image = snapshot.optJSONObject("image")
        val display = tree.getJSONObject("display"); val id = UUID.randomUUID().toString()
        val generation = if (isBackground) background.value.generation else null
        if (generation != null) observations.entries.removeAll { it.value.generation != null && it.value.generation != generation }
        observations["display:$displayId"] = Seen(owner, id, tree.getString("observation_id"), image?.getInt("width") ?: 0, image?.getInt("height") ?: 0, display.getInt("width"), display.getInt("height"),"display:$displayId",generation)
        val nodes = tree.getJSONArray("nodes")
        for (i in 0 until nodes.length()) {
            val node = nodes.getJSONObject(i)
            node.put("ref", "n${node.getInt("id")}"); node.remove("id")
            if (image != null) {
                val bounds = node.getJSONArray("bounds")
                for (j in 0..3) bounds.put(j, bounds.getDouble(j) * image.getInt(if (j % 2 == 0) "width" else "height") /
                    display.getInt(if (j % 2 == 0) "width" else "height"))
            } else node.remove("bounds")
        }
        return obj("observation_id" to id, "device" to config.deviceId, "view" to "display:$displayId", "image" to image,
            "source" to obj("kind" to if (isBackground) "ai_display" else "main_display", "display_id" to displayId,
                "generation" to generation,"rotation" to display.getInt("rotation"),"width" to display.getInt("width"),"height" to display.getInt("height")),
            "image_unavailable" to if (image == null) obj("reason" to "view_has_not_rendered","next_action" to "launch_app") else null,
            "elements" to nodes, "package" to tree.opt("package"), "truncated" to tree.optBoolean("truncated"),
            "capabilities" to obj("backend" to "android_accessibility", "actions" to arr(UiContract.actions),
                "semantic_elements" to true, "coordinates" to "image_pixels", "background" to (android.os.Build.VERSION.SDK_INT >= 34 && AgentRuntime.bridge?.ui() != null),
                "same_app_concurrency" to false,"current_view_background" to isBackground),
            "readiness" to obj("available" to true, "accessibility" to true, "own_focus" to isBackground,"shell_connected" to (AgentRuntime.bridge?.connected() == true)),
            "observed_at_ms" to System.currentTimeMillis())
    }
    private fun node(target: JSONObject, native: JSONObject): JSONObject {
        UiContract.target(target)
        val ref = target.getString("ref")
        demand(ref.matches(Regex("n[0-9]+")), "unknown_element_ref")
        return native.put("node_id", ref.drop(1).toInt())
    }
    private fun point(target: JSONObject, seen: Seen): Pair<Double, Double> = if (target.has("ref"))
        screen().nodePoint(node(target,obj("observation_id" to seen.native)))
        else UiContract.point(target, seen.imageWidth, seen.imageHeight, seen.displayWidth, seen.displayHeight)
    private fun act(action: JSONObject, native: JSONObject, seen: Seen): JSONObject {
        val service = screen(); val type = action.getString("type")
        val target = action.optJSONObject("target")
        return when (type) {
            "click", "long_press" -> if (target!!.has("ref")) service.nodeAction(if (type == "click") "click" else "long_click", node(target, native), true)
                else {
                    val (x, y) = point(target, seen)
                    service.uiGesture(type, native.put("x", x).put("y", y))
                }
            "double_click" -> {
                val (x, y) = point(target!!, seen)
                service.uiGesture(type, native.put("x", x).put("y", y))
            }
            "type_text", "fill" -> {
                if (target != null) node(target, native)
                service.nodeAction(if (type == "fill") "text" else "insert", native.put("text", action.getString("text")), true)
            }
            "scroll" -> if (target?.has("ref") == true && action.getString("direction") in listOf("up", "down")) {
                service.nodeAction("scroll", node(target, native).put("direction", if (action.getString("direction") == "down") "forward" else "backward"), true)
            } else {
                val center = if (target != null) point(target, seen) else Pair(seen.displayWidth / 2.0, seen.displayHeight / 2.0)
                val vertical = action.getString("direction") in listOf("up", "down")
                val distance = (if (vertical) seen.displayHeight else seen.displayWidth) * 0.25
                val sign = if (action.getString("direction") in listOf("down", "right")) 1 else -1
                val dx = if (vertical) 0.0 else distance * sign; val dy = if (vertical) distance * sign else 0.0
                service.uiGesture("drag", native.put("x", (center.first + dx).coerceIn(0.0, seen.displayWidth - 1.0))
                    .put("y", (center.second + dy).coerceIn(0.0, seen.displayHeight - 1.0))
                    .put("to_x", (center.first - dx).coerceIn(0.0, seen.displayWidth - 1.0))
                    .put("to_y", (center.second - dy).coerceIn(0.0, seen.displayHeight - 1.0)))
            }
            "drag" -> {
                val (x,y) = point(action.getJSONObject("from"), seen); val (tx,ty) = point(action.getJSONObject("to"), seen)
                service.uiGesture("drag", native.put("x",x).put("y",y).put("to_x",tx).put("to_y",ty))
            }
            "pinch" -> {
                val (x,y) = point(action.getJSONObject("center"), seen)
                service.uiGesture("pinch", native.put("x",x).put("y",y).put("scale",action.getDouble("scale")))
            }
            "key" -> if (seen.generation == null) service.uiKey(native, action) else {
                service.checkObservation(native)
                val (code,modifiers) = UiContract.key(action); background.value.key(code,modifiers)
            }
            "launch_app" -> {
                // Explicit app launch uses the live view binding, not transient page contents.
                if (seen.generation == null) PackageActions(context).launch(obj("package" to action.getString("app")))
                else background.value.launch(action.getString("app"))
            }
            "navigate_back", "go_home", "app_switcher" -> {
                service.checkObservation(native)
                if (seen.generation == null) service.global(when(type) { "navigate_back" -> "back"; "go_home" -> "home"; else -> "recents" })
                else background.value.key(when(type) { "navigate_back" -> android.view.KeyEvent.KEYCODE_BACK;
                    "go_home" -> android.view.KeyEvent.KEYCODE_HOME; else -> android.view.KeyEvent.KEYCODE_APP_SWITCH })
            }
            else -> throw AgentError("unsupported_action")
        }.put("input_sent", true)
    }
    override fun close() { observations.clear(); if (background.isInitialized()) background.value.close() }
}
