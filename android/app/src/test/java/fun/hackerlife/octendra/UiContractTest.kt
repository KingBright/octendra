package `fun`.hackerlife.octendra

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class UiContractTest {
    @Test fun imageCoordinatesMapToDisplayEndpoints() {
        assertEquals(Pair(1079.0, 2399.0), UiContract.point(obj("x" to 539, "y" to 1199), 540, 1200, 1080, 2400))
        for (bad in listOf(obj("x" to 540, "y" to 0), obj("x" to -1, "y" to 0), obj("ref" to "n1", "x" to 1, "y" to 2))) {
            try { UiContract.point(bad, 540, 1200, 1080, 2400); fail("invalid target accepted") } catch (_: AgentError) { }
        }
    }

    @Test fun insertPreservesTextAndReplacesOnlySelection() {
        assertEquals("ab中文🙂cd", UiContract.insert("abcd", 2, 2, "中文🙂"))
        assertEquals("a中d", UiContract.insert("abcd", 3, 1, "中"))
        try { UiContract.insert("abcd", -1, -1, "x"); fail("unknown selection accepted") } catch (_: AgentError) { }
    }

    @Test fun sharedIntentsRejectAmbiguousOrUnknownFields() {
        for (valid in listOf(
            """{"type":"click","target":{"ref":"n1"}}""",
            """{"type":"triple_click","target":{"ref":"n2"}}""",
            """{"type":"set_checked","target":{"ref":"n3"},"checked":true}""",
            """{"type":"fill","text":""}""",
            """{"type":"scroll","direction":"down"}""",
            """{"type":"type_text","text":"中文🙂"}"""
        )) UiContract.validate(JSONObject(valid))
        for (invalid in listOf(
            """{"type":"click","target":{"ref":"n1"},"command":"bad"}""",
            """{"type":"set_checked","target":{"ref":"n3"},"checked":"yes"}""",
            """{"type":"set_checked","target":{"ref":"n3"}}""",
            """{"type":"scroll","direction":"forward"}""",
            """{"type":"pinch","center":{"x":1,"y":1},"scale":0}"""
        )) {
            try { UiContract.validate(JSONObject(invalid)); fail("invalid action accepted") } catch (_: AgentError) { }
        }
    }

    @Test fun offscreenInvisibleAndEmptyWrapperNodesArePrunedAndBoundsAreCompact() {
        // Off-screen negative bounds from launcher adjacent pages must be pruned.
        assertFalse(UiContract.shouldEmitNode(true, -1016, 200, -574, 400, 1172, 2748,
            clickable = true, longClickable = false, editable = false, scrollable = false,
            checkable = false, password = false, text = "WeChat", description = null))
        // Invisible nodes must be pruned even if inside display coordinates.
        assertFalse(UiContract.shouldEmitNode(false, 100, 200, 300, 400, 1172, 2748,
            clickable = true, longClickable = false, editable = false, scrollable = false,
            checkable = false, password = false, text = "Hidden", description = null))
        // Empty non-interactive layout wrappers must be pruned.
        assertFalse(UiContract.shouldEmitNode(true, 0, 0, 1172, 2748, 1172, 2748,
            clickable = false, longClickable = false, editable = false, scrollable = false,
            checkable = false, password = false, text = "", description = null))
        // Visible actionable or labelled on-screen nodes must be retained.
        assertTrue(UiContract.shouldEmitNode(true, 80, 200, 280, 400, 1172, 2748,
            clickable = true, longClickable = false, editable = false, scrollable = false,
            checkable = false, password = false, text = null, description = "浏览器"))
        // Partially visible edge bounds clamp cleanly and round to integer image pixels.
        assertEquals(listOf(0, 93, 130, 186), UiContract.scaleBounds(-20, 200, 280, 400, 1172, 2748, 545, 1280))
    }

    @Test fun queryFilteringPreservesOriginalRefsAndSupportsRoleTextInteractiveMax() {
        val elements = JSONArray(
            listOf(
                obj("ref" to "n0", "class" to "android.widget.TextView", "text" to "Settings", "clickable" to false, "editable" to false, "scrollable" to false, "checkable" to false),
                obj("ref" to "n1", "class" to "android.widget.Switch", "text" to "Wi-Fi", "hint" to "Network toggle", "clickable" to true, "editable" to false, "scrollable" to false, "checkable" to true, "checked" to true),
                obj("ref" to "n2", "class" to "android.widget.EditText", "text" to "", "hint" to "Search Wi-Fi", "clickable" to true, "editable" to true, "scrollable" to false, "checkable" to false),
                obj("ref" to "n3", "class" to "android.widget.Button", "text" to "Advanced", "clickable" to true, "editable" to false, "scrollable" to false, "checkable" to false)
            )
        )
        val byText = UiContract.filterElements(elements, obj("text" to "wi-fi", "interactive_only" to true))
        assertEquals(2, byText.length())
        assertEquals("n1", byText.getJSONObject(0).getString("ref"))
        assertEquals("n2", byText.getJSONObject(1).getString("ref"))

        val byRoleAndMax = UiContract.filterElements(elements, obj("role" to "Switch", "max_elements" to 1))
        assertEquals(1, byRoleAndMax.length())
        assertEquals("n1", byRoleAndMax.getJSONObject(0).getString("ref"))
    }

    @Test fun expectEvaluatesByRealElementSignatureAcrossIndexShifts() {
        val pre = JSONArray(
            listOf(
                obj("ref" to "n0", "class" to "android.widget.CheckBox", "resource_id" to "id/wifi", "text" to "Wi-Fi", "bounds" to arr(listOf(10, 10, 100, 60)), "checkable" to true, "checked" to false),
                obj("ref" to "n1", "class" to "android.widget.EditText", "resource_id" to "id/query", "text" to "", "hint" to "Search", "bounds" to arr(listOf(10, 80, 300, 140)), "editable" to true),
                obj("ref" to "n2", "class" to "android.widget.Button", "resource_id" to "id/submit", "text" to "Submit", "bounds" to arr(listOf(10, 160, 200, 220)))
            )
        )
        // Post elements insert a banner at index 0 so ref indices shift!
        val post = JSONArray(
            listOf(
                obj("ref" to "n0", "class" to "android.widget.TextView", "resource_id" to "id/banner", "text" to "Saved", "bounds" to arr(listOf(0, 0, 300, 40))),
                obj("ref" to "n1", "class" to "android.widget.CheckBox", "resource_id" to "id/wifi", "text" to "Wi-Fi", "bounds" to arr(listOf(10, 50, 100, 100)), "checkable" to true, "checked" to true),
                obj("ref" to "n2", "class" to "android.widget.EditText", "resource_id" to "id/query", "text" to "hello", "hint" to "Search", "bounds" to arr(listOf(10, 120, 300, 180)), "editable" to true)
            )
        )
        assertTrue(UiContract.evaluateExpect(obj("checked_equals" to true, "visual_changed" to true), pre, post, obj("ref" to "n0"), visualChanged = true, actionType = "set_checked"))
        assertFalse(UiContract.evaluateExpect(obj("checked_equals" to false), pre, post, obj("ref" to "n0"), visualChanged = true, actionType = "set_checked"))
        assertTrue(UiContract.evaluateExpect(obj("value_equals" to "hello", "text_contains" to "ell"), pre, post, obj("ref" to "n1"), visualChanged = true, actionType = "fill"))
        assertTrue(UiContract.evaluateExpect(obj("element_gone" to true), pre, post, obj("ref" to "n2"), visualChanged = true, actionType = "click"))
        assertFalse(UiContract.evaluateExpect(obj("element_gone" to true), pre, post, obj("ref" to "n0"), visualChanged = true, actionType = "click"))
    }

    @Test fun displayStateClassificationAndSpillExclusionsAreAccurate() {
        assertEquals("display_sleeping", UiContract.classifyDisplayObservation(hasImage = false, displayState = 1, blankOrSecure = false))
        assertEquals("display_sleeping", UiContract.classifyDisplayObservation(hasImage = false, displayState = 3, blankOrSecure = false))
        assertEquals("view_has_not_rendered", UiContract.classifyDisplayObservation(hasImage = false, displayState = 2, blankOrSecure = false))
        assertEquals("blank_or_secure", UiContract.classifyDisplayObservation(hasImage = true, displayState = 2, blankOrSecure = true))
        assertEquals("ready", UiContract.classifyDisplayObservation(hasImage = true, displayState = 2, blankOrSecure = false))

        assertTrue(UiContract.isSpillExcludedPackage("com.google.android.permissioncontroller"))
        assertTrue(UiContract.isSpillExcludedPackage("com.android.packageinstaller"))
        assertTrue(UiContract.isSpillExcludedPackage("com.android.systemui"))
        assertTrue(UiContract.isSpillExcludedPackage("com.android.keyguard"))
        assertFalse(UiContract.isSpillExcludedPackage("com.tencent.mm"))
    }

    @Test fun clipboardGuardProtectsPasswordsAndConditionallyRestoresClip() {
        var clip: String? = "user-secret-note"
        try {
            ClipboardGuard.withTemporaryClip(
                isPassword = true,
                temporaryText = "pw",
                readCurrentText = { clip },
                savePrevious = { clip },
                writeTemporary = { clip = it },
                restorePrevious = { clip = it },
                action = { true }
            )
            fail("password clipboard must be rejected")
        } catch (_: AgentError) {
            assertEquals("user-secret-note", clip)
        }

        // Unchanged clip after action -> restored cleanly
        assertTrue(
            ClipboardGuard.withTemporaryClip(
                isPassword = false,
                temporaryText = "temp-insert",
                readCurrentText = { clip },
                savePrevious = { clip },
                writeTemporary = { clip = it },
                restorePrevious = { clip = it },
                action = { assertEquals("temp-insert", clip); true }
            )
        )
        assertEquals("user-secret-note", clip)

        // Concurrent user copy during action -> NOT overwritten on restore
        ClipboardGuard.withTemporaryClip(
            isPassword = false,
            temporaryText = "temp-insert",
            readCurrentText = { clip },
            savePrevious = { clip },
            writeTemporary = { clip = it },
            restorePrevious = { clip = it },
            action = { clip = "user-copied-newer-text"; true }
        )
        assertEquals("user-copied-newer-text", clip)
    }

    @Test fun insertInEditorIgnoresHintTextAndHandlesEmptyUnknownSelection() {
        assertEquals("hello" to 5, UiContract.insertInEditor("Enter name", showingHint = true, start = -1, end = -1, text = "hello"))
        assertEquals("abXcd" to 3, UiContract.insertInEditor("abcd", showingHint = false, start = 2, end = 2, text = "X"))
    }
}
