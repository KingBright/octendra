package `fun`.hackerlife.octendra

import org.json.JSONObject
import kotlin.math.max
import kotlin.math.min

/** The same semantic intents as octendra-desktop-protocol::ui; no task/lease API. */
object UiContract {
    fun key(action: JSONObject): Pair<Int,Int> {
        val key = action.getString("key").lowercase()
        val name = when (key) {
            "arrowleft", "left" -> "DPAD_LEFT"; "arrowright", "right" -> "DPAD_RIGHT"
            "arrowup", "up" -> "DPAD_UP"; "arrowdown", "down" -> "DPAD_DOWN"
            "backspace" -> "DEL"; "delete" -> "FORWARD_DEL"; "home" -> "MOVE_HOME"; "end" -> "MOVE_END"
            "pageup" -> "PAGE_UP"; "pagedown" -> "PAGE_DOWN"; else -> key.uppercase()
        }
        val code = android.view.KeyEvent.keyCodeFromString("KEYCODE_$name")
        demand(code != android.view.KeyEvent.KEYCODE_UNKNOWN,"unsupported_key")
        var meta = 0; val modifiers = action.optJSONArray("modifiers")
        if (modifiers != null) for (i in 0 until modifiers.length()) meta = meta or when(modifiers.getString(i)) {
            "shift" -> android.view.KeyEvent.META_SHIFT_ON; "ctrl" -> android.view.KeyEvent.META_CTRL_ON
            "alt" -> android.view.KeyEvent.META_ALT_ON; else -> android.view.KeyEvent.META_META_ON
        }
        return Pair(code,meta)
    }
    val actions = listOf("click", "double_click", "long_press", "type_text", "fill", "scroll", "drag", "key",
        "launch_app", "pinch", "navigate_back", "go_home", "app_switcher")
    fun target(value: JSONObject) {
        val keys = value.keys().asSequence().toSet()
        if (keys == setOf("ref")) demand(value.getString("ref").isNotEmpty(), "invalid_target")
        else {
            demand(keys == setOf("x", "y"), "invalid_target")
            demand(value.getDouble("x").isFinite() && value.getDouble("y").isFinite() &&
                value.getDouble("x") >= 0 && value.getDouble("y") >= 0, "invalid_coordinates")
        }
    }
    fun point(value: JSONObject, imageWidth: Int, imageHeight: Int, displayWidth: Int, displayHeight: Int): Pair<Double, Double> {
        target(value); demand(!value.has("ref"), "coordinate_target_required")
        val x = value.getDouble("x"); val y = value.getDouble("y")
        demand(imageWidth > 0 && imageHeight > 0 && displayWidth > 0 && displayHeight > 0 &&
            x <= imageWidth - 1 && y <= imageHeight - 1, "invalid_coordinates")
        return Pair(x * (displayWidth - 1) / max(1, imageWidth - 1), y * (displayHeight - 1) / max(1, imageHeight - 1))
    }
    fun insert(existing: String, start: Int, end: Int, text: String): String {
        demand(start in 0..existing.length && end in 0..existing.length, "editor_selection_unavailable")
        return existing.substring(0, min(start, end)) + text + existing.substring(max(start, end))
    }
    fun validate(action: JSONObject) {
        val type = action.getString("type")
        val required = when (type) {
            "click", "double_click", "secondary_click", "long_press" -> setOf("target")
            "type_text", "fill" -> setOf("text")
            "scroll" -> setOf("direction")
            "drag" -> setOf("from", "to")
            "key" -> setOf("key")
            "launch_app" -> setOf("app")
            "pinch" -> setOf("center", "scale")
            "navigate_back", "go_home", "app_switcher" -> emptySet()
            else -> throw AgentError("unsupported_action")
        }
        val optional = when (type) { "type_text", "fill", "scroll" -> setOf("target"); "key" -> setOf("modifiers"); else -> emptySet() }
        val keys = action.keys().asSequence().toSet()
        demand(keys.containsAll(required) && keys.all { it == "type" || it in required || it in optional }, "invalid_ui_action")
        for (key in listOf("target", "from", "to", "center")) if (action.has(key) && (!action.isNull(key) || key in required)) target(action.getJSONObject(key))
        if (type == "type_text" || type == "fill") demand(action.getString("text").toByteArray().size <= 8192, "text_too_long")
        if (type == "scroll") demand(action.getString("direction") in listOf("up", "down", "left", "right"), "invalid_scroll_direction")
        if (type == "pinch") demand(action.getDouble("scale").isFinite() && action.getDouble("scale") > 0, "invalid_pinch_scale")
        if (type == "key") {
            demand(action.getString("key").length in 1..64, "unsupported_key")
            val modifiers = action.optJSONArray("modifiers")
            if (modifiers != null) {
                demand(modifiers.length() <= 4, "invalid_key_modifiers")
                for (i in 0 until modifiers.length()) demand(modifiers.getString(i) in listOf("shift", "ctrl", "alt", "meta"), "invalid_key_modifiers")
            }
        }
    }
}
