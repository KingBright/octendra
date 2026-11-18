package `fun`.hackerlife.octendra

import org.json.JSONArray
import org.json.JSONObject
import kotlin.math.abs
import kotlin.math.max
import kotlin.math.min
import kotlin.math.roundToInt

/** The same semantic intents as octendra-desktop-protocol::ui; no task/lease API. */
object UiContract {
    internal inline fun <T : Any> currentPathChild(index: Int, childCount: Int, getChild: (Int) -> T?): T {
        if (index !in 0 until childCount) throw AgentError("node_changed_observe_again")
        return try {
            getChild(index) ?: throw AgentError("node_changed_observe_again")
        } catch (_: IndexOutOfBoundsException) {
            throw AgentError("node_changed_observe_again")
        }
    }

    private const val DISPLAY_STATE_OFF = 1
    private const val DISPLAY_STATE_DOZE = 3
    private const val DISPLAY_STATE_DOZE_SUSPEND = 4
    private const val DISPLAY_STATE_ON_SUSPEND = 6

    private val spillExcludedExact = setOf(
        "android",
        "com.android.systemui",
        "com.android.keyguard",
        "com.android.permissioncontroller",
        "com.google.android.permissioncontroller",
        "com.android.packageinstaller",
        "com.google.android.packageinstaller",
        "com.miui.packageinstaller",
        "com.samsung.android.packageinstaller",
        "com.oplus.appdetail",
        "com.coloros.appinstaller",
        "com.vivo.appinstaller",
        "com.android.credentialmanager",
        "com.android.biometric",
        "com.android.vpndialogs",
        "com.android.managedprovisioning",
        "fun.hackerlife.octendra",
        "fun.hackerlife.octendra.debug",
        "fun.hackerlife.weavelo",
        "fun.hackerlife.weavelo.debug"
    )

    fun isSpillExcludedPackage(packageName: String?): Boolean {
        if (packageName.isNullOrBlank()) return true
        val lower = packageName.lowercase()
        if (lower in spillExcludedExact) return true
        return lower.contains("permissioncontroller") ||
            lower.contains("packageinstaller") ||
            lower.contains("appinstaller") ||
            lower.contains("keyguard") ||
            lower.contains("lockscreen")
    }

    fun isDisplaySleeping(displayState: Int): Boolean =
        displayState == DISPLAY_STATE_OFF ||
            displayState == DISPLAY_STATE_DOZE ||
            displayState == DISPLAY_STATE_DOZE_SUSPEND ||
            displayState == DISPLAY_STATE_ON_SUSPEND

    fun classifyDisplayObservation(hasImage: Boolean, displayState: Int, blankOrSecure: Boolean): String = when {
        !hasImage && isDisplaySleeping(displayState) -> "display_sleeping"
        !hasImage -> "view_has_not_rendered"
        blankOrSecure -> "blank_or_secure"
        else -> "ready"
    }

    fun key(action: JSONObject): Pair<Int, Int> {
        val key = action.getString("key").lowercase()
        val name = when (key) {
            "arrowleft", "left" -> "DPAD_LEFT"; "arrowright", "right" -> "DPAD_RIGHT"
            "arrowup", "up" -> "DPAD_UP"; "arrowdown", "down" -> "DPAD_DOWN"
            "backspace" -> "DEL"; "delete" -> "FORWARD_DEL"; "home" -> "MOVE_HOME"; "end" -> "MOVE_END"
            "pageup" -> "PAGE_UP"; "pagedown" -> "PAGE_DOWN"; else -> key.uppercase()
        }
        val code = android.view.KeyEvent.keyCodeFromString("KEYCODE_$name")
        demand(code != android.view.KeyEvent.KEYCODE_UNKNOWN, "unsupported_key")
        var meta = 0; val modifiers = action.optJSONArray("modifiers")
        if (modifiers != null) for (i in 0 until modifiers.length()) meta = meta or when (modifiers.getString(i)) {
            "shift" -> android.view.KeyEvent.META_SHIFT_ON; "ctrl" -> android.view.KeyEvent.META_CTRL_ON
            "alt" -> android.view.KeyEvent.META_ALT_ON; else -> android.view.KeyEvent.META_META_ON
        }
        return Pair(code, meta)
    }

    val actions = listOf(
        "click", "double_click", "triple_click", "long_press", "set_checked",
        "type_text", "fill", "scroll", "drag", "key",
        "launch_app", "pinch", "navigate_back", "go_home", "app_switcher"
    )

    fun target(value: JSONObject) {
        val keys = value.keys().asSequence().toSet()
        if (keys == setOf("ref")) demand(value.getString("ref").isNotEmpty() && value.getString("ref").length <= 256, "invalid_target")
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

    fun insertInEditor(existing: String, showingHint: Boolean, start: Int, end: Int, text: String): Pair<String, Int> {
        val base = if (showingHint) "" else existing
        val (s, e) = if (base.isEmpty() && start == -1 && end == -1) 0 to 0 else start to end
        val merged = insert(base, s, e, text)
        val cursor = min(s, e) + text.length
        return merged to cursor
    }

    fun shouldEmitNode(
        visible: Boolean, left: Int, top: Int, right: Int, bottom: Int,
        displayWidth: Int, displayHeight: Int, clickable: Boolean, longClickable: Boolean,
        editable: Boolean, scrollable: Boolean, checkable: Boolean, password: Boolean,
        text: String?, description: String?
    ): Boolean {
        if (!visible || displayWidth <= 0 || displayHeight <= 0) return false
        if (right <= left || bottom <= top) return false
        if (right <= 0 || bottom <= 0 || left >= displayWidth || top >= displayHeight) return false
        return clickable || longClickable || editable || scrollable || checkable || password ||
            !text.isNullOrBlank() || !description.isNullOrBlank()
    }

    fun scaleBounds(
        left: Int, top: Int, right: Int, bottom: Int,
        displayWidth: Int, displayHeight: Int, imageWidth: Int, imageHeight: Int
    ): List<Int> {
        val cl = left.coerceIn(0, displayWidth)
        val ct = top.coerceIn(0, displayHeight)
        val cr = right.coerceIn(cl, displayWidth)
        val cb = bottom.coerceIn(ct, displayHeight)
        fun scaleX(v: Int) = ((v.toDouble() * imageWidth) / max(1, displayWidth)).roundToInt().coerceIn(0, imageWidth)
        fun scaleY(v: Int) = ((v.toDouble() * imageHeight) / max(1, displayHeight)).roundToInt().coerceIn(0, imageHeight)
        return listOf(scaleX(cl), scaleY(ct), scaleX(cr), scaleY(cb))
    }

    fun validate(action: JSONObject) {
        val type = action.getString("type")
        val required = when (type) {
            "click", "double_click", "triple_click", "secondary_click", "long_press" -> setOf("target")
            "set_checked" -> setOf("target", "checked")
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
        if (type == "set_checked") demand(action.opt("checked") is Boolean, "invalid_ui_action")
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

    fun validateQuery(query: JSONObject) {
        val allowed = setOf("text", "role", "interactive_only", "max_elements")
        demand(query.keys().asSequence().all { it in allowed }, "invalid_ui_query")
        if (query.has("text") && !query.isNull("text")) {
            val t = query.opt("text")
            demand(t is String && t.length <= 256, "invalid_ui_query")
        }
        if (query.has("role") && !query.isNull("role")) {
            val r = query.opt("role")
            demand(r is String && r.trim().isNotEmpty() && r.length <= 128, "invalid_ui_query")
        }
        if (query.has("interactive_only") && !query.isNull("interactive_only")) {
            demand(query.opt("interactive_only") is Boolean, "invalid_ui_query")
        }
        if (query.has("max_elements") && !query.isNull("max_elements")) {
            val n = query.opt("max_elements")
            demand(n is Int && n in 1..1000 || (n is Long && n in 1L..1000L), "invalid_ui_query")
        }
    }

    fun filterElements(elements: JSONArray, query: JSONObject): JSONArray {
        validateQuery(query)
        val textFilter = if (query.has("text") && !query.isNull("text")) query.getString("text").trim().lowercase() else null
        val roleFilter = if (query.has("role") && !query.isNull("role")) query.getString("role").trim().lowercase() else null
        val interactiveOnly = query.optBoolean("interactive_only", false)
        val maxElements = if (query.has("max_elements") && !query.isNull("max_elements")) query.getInt("max_elements") else Int.MAX_VALUE
        val out = JSONArray()
        for (i in 0 until elements.length()) {
            if (out.length() >= maxElements) break
            val el = elements.getJSONObject(i)
            if (interactiveOnly && !(el.optBoolean("clickable") || el.optBoolean("editable") ||
                    el.optBoolean("scrollable") || el.optBoolean("checkable") ||
                    el.optBoolean("long_clickable") || el.optBoolean("interactive"))) {
                continue
            }
            if (roleFilter != null) {
                val cls = if (el.isNull("class")) "" else el.optString("class", "")
                val role = if (el.isNull("role")) "" else el.optString("role", "")
                if (!cls.lowercase().contains(roleFilter) && !role.lowercase().contains(roleFilter)) continue
            }
            if (!textFilter.isNullOrEmpty()) {
                val matched = listOf("text", "description", "hint", "resource_id", "name", "value", "class").any { field ->
                    !el.isNull(field) && el.optString(field, "").lowercase().contains(textFilter)
                }
                if (!matched) continue
            }
            out.put(JSONObject(el.toString()))
        }
        return out
    }

    fun validateExpect(expect: JSONObject) {
        val allowed = setOf("target", "value_equals", "text_contains", "checked_equals", "element_gone", "visual_changed")
        demand(expect.keys().asSequence().all { it in allowed }, "invalid_ui_expect")
        val hasCondition = listOf("value_equals", "text_contains", "checked_equals", "element_gone", "visual_changed")
            .any { expect.has(it) && !expect.isNull(it) }
        demand(hasCondition, "invalid_ui_expect")
        if (expect.has("target") && !expect.isNull("target")) target(expect.getJSONObject("target"))
        if (expect.has("value_equals") && !expect.isNull("value_equals")) {
            val v = expect.opt("value_equals")
            demand(v is String && v.length <= 8192, "invalid_ui_expect")
        }
        if (expect.has("text_contains") && !expect.isNull("text_contains")) {
            val t = expect.opt("text_contains")
            demand(t is String && t.isNotEmpty() && t.length <= 1024, "invalid_ui_expect")
        }
        if (expect.has("checked_equals") && !expect.isNull("checked_equals")) {
            demand(expect.opt("checked_equals") is Boolean, "invalid_ui_expect")
        }
        if (expect.has("element_gone") && !expect.isNull("element_gone")) {
            demand(expect.opt("element_gone") is Boolean, "invalid_ui_expect")
        }
        if (expect.has("visual_changed") && !expect.isNull("visual_changed")) {
            demand(expect.opt("visual_changed") is Boolean, "invalid_ui_expect")
        }
    }

    private fun optStr(el: JSONObject, key: String): String =
        if (el.isNull(key)) "" else el.optString(key, "")

    private fun elementSignatureScore(pre: JSONObject, post: JSONObject, strictText: Boolean): Int {
        val preCls = optStr(pre, "class")
        val postCls = optStr(post, "class")
        if (preCls.isNotEmpty() && postCls.isNotEmpty() && preCls != postCls) return -1
        val preRes = optStr(pre, "resource_id")
        val postRes = optStr(post, "resource_id")
        if (preRes.isNotEmpty() && preRes != postRes) return -1

        val preText = optStr(pre, "text")
        val postText = optStr(post, "text")
        val preDesc = optStr(pre, "description")
        val postDesc = optStr(post, "description")
        val preHint = optStr(pre, "hint")
        val postHint = optStr(post, "hint")

        if (strictText) {
            if (preText.isNotEmpty() && preText != postText) return -1
            if (preDesc.isNotEmpty() && preDesc != postDesc) return -1
            if (preHint.isNotEmpty() && preHint != postHint) return -1
        }

        var score = 0
        if (preRes.isNotEmpty() && preRes == postRes) score += 40
        if (preCls.isNotEmpty() && preCls == postCls) score += 20
        if (preDesc.isNotEmpty() && preDesc == postDesc) score += 25
        if (preHint.isNotEmpty() && preHint == postHint) score += 20
        if (preText.isNotEmpty() && preText == postText) score += 25
        if (optStr(pre, "ref").isNotEmpty() && optStr(pre, "ref") == optStr(post, "ref")) score += 10

        val preB = pre.optJSONArray("bounds")
        val postB = post.optJSONArray("bounds")
        if (preB != null && postB != null && preB.length() == 4 && postB.length() == 4) {
            val pcx = (preB.getInt(0) + preB.getInt(2)) / 2
            val pcy = (preB.getInt(1) + preB.getInt(3)) / 2
            val qcx = (postB.getInt(0) + postB.getInt(2)) / 2
            val qcy = (postB.getInt(1) + postB.getInt(3)) / 2
            val dist = abs(pcx - qcx) + abs(pcy - qcy)
            if (dist <= 8) score += 25
            else if (dist <= 64) score += 12
            else if (preRes.isEmpty() && preDesc.isEmpty() && preText.isEmpty() && preHint.isEmpty()) return -1
        }
        return if (score > 0) score else -1
    }

    private fun findElementByTarget(target: JSONObject?, elements: JSONArray): JSONObject? {
        if (target == null) return null
        if (target.has("ref")) {
            val ref = target.getString("ref")
            for (i in 0 until elements.length()) {
                val el = elements.getJSONObject(i)
                if (optStr(el, "ref") == ref) return el
            }
            return null
        }
        val x = target.optDouble("x", -1.0)
        val y = target.optDouble("y", -1.0)
        if (!x.isFinite() || !y.isFinite() || x < 0 || y < 0) return null
        var best: JSONObject? = null
        var bestArea = Long.MAX_VALUE
        for (i in 0 until elements.length()) {
            val el = elements.getJSONObject(i)
            val b = el.optJSONArray("bounds") ?: continue
            if (b.length() != 4) continue
            val l = b.getInt(0); val t = b.getInt(1); val r = b.getInt(2); val bt = b.getInt(3)
            if (x >= l && x <= r && y >= t && y <= bt) {
                val area = (r - l).toLong() * (bt - t).toLong()
                if (area < bestArea) {
                    bestArea = area
                    best = el
                }
            }
        }
        return best
    }

    private fun matchPostElement(preEl: JSONObject, postElements: JSONArray, strictText: Boolean): JSONObject? {
        var best: JSONObject? = null
        var bestScore = -1
        for (i in 0 until postElements.length()) {
            val candidate = postElements.getJSONObject(i)
            val score = elementSignatureScore(preEl, candidate, strictText)
            if (score > bestScore) {
                bestScore = score
                best = candidate
            }
        }
        return best
    }

    fun evaluateExpect(
        expect: JSONObject,
        preElements: JSONArray,
        postElements: JSONArray,
        actionTarget: JSONObject?,
        visualChanged: Boolean?,
        actionType: String? = null
    ): Boolean {
        validateExpect(expect)
        val explicitTarget = if (expect.has("target") && !expect.isNull("target")) expect.getJSONObject("target") else null
        val targetObj = explicitTarget ?: actionTarget
        val preEl = findElementByTarget(targetObj, preElements)

        if (expect.has("visual_changed") && !expect.isNull("visual_changed")) {
            val desired = expect.getBoolean("visual_changed")
            if (visualChanged == null || visualChanged != desired) return false
        }
        if (expect.has("element_gone") && !expect.isNull("element_gone")) {
            val desiredGone = expect.getBoolean("element_gone")
            if (preEl == null) return false
            val matched = matchPostElement(preEl, postElements, strictText = true)
            if ((matched == null) != desiredGone) return false
        }
        if (expect.has("checked_equals") && !expect.isNull("checked_equals")) {
            val desiredChecked = expect.getBoolean("checked_equals")
            if (preEl == null) return false
            val matched = matchPostElement(preEl, postElements, strictText = false) ?: return false
            if (matched.isNull("checked") || matched.opt("checked") != desiredChecked) return false
        }
        if (expect.has("value_equals") && !expect.isNull("value_equals")) {
            val desiredValue = expect.getString("value_equals")
            if (preEl == null) return false
            val matched = matchPostElement(preEl, postElements, strictText = false) ?: return false
            if (optStr(matched, "text") != desiredValue) return false
        }
        if (expect.has("text_contains") && !expect.isNull("text_contains")) {
            val needle = expect.getString("text_contains")
            val targetOnly = explicitTarget != null || actionType in listOf("fill", "type_text", "set_checked")
            if (targetOnly && preEl != null) {
                val matched = matchPostElement(preEl, postElements, strictText = false) ?: return false
                val hit = optStr(matched, "text").contains(needle) ||
                    optStr(matched, "description").contains(needle) ||
                    optStr(matched, "hint").contains(needle)
                if (!hit) return false
            } else {
                var found = false
                for (i in 0 until postElements.length()) {
                    val el = postElements.getJSONObject(i)
                    if (optStr(el, "text").contains(needle) ||
                        optStr(el, "description").contains(needle) ||
                        optStr(el, "hint").contains(needle)) {
                        found = true
                        break
                    }
                }
                if (!found) return false
            }
        }
        return true
    }
}

/**
 * Conditional clipboard save/restore guard for fallback text insertion on non-password fields.
 * Never touches clipboard for password fields and only restores the previous clip if the
 * clipboard still matches our temporary write when the action finishes.
 */
object ClipboardGuard {
    fun <T> withTemporaryClip(
        isPassword: Boolean,
        temporaryText: String,
        readCurrentText: () -> String?,
        savePrevious: () -> T?,
        writeTemporary: (String) -> Unit,
        restorePrevious: (T?) -> Unit,
        action: () -> Boolean
    ): Boolean {
        demand(!isPassword, "clipboard_forbidden_for_password")
        val previous = savePrevious()
        writeTemporary(temporaryText)
        return try {
            action()
        } finally {
            runCatching {
                if (readCurrentText() == temporaryText) {
                    restorePrevious(previous)
                }
            }
        }
    }
}
