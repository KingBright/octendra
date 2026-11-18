package `fun`.hackerlife.octendra

import android.view.Display
import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test
import java.util.UUID

class UiControllerTest {
    private class FakeBackend(
        var displayState: Int = Display.STATE_ON,
        var imagePresent: Boolean = true,
        var blankOrSecure: Boolean = false
    ) : UiController.Backend {
        var dumpCalls = 0
        var nodeActionCalls = mutableListOf<Pair<String, JSONObject>>()
        var gestureCalls = mutableListOf<Pair<String, JSONObject>>()
        var backgroundMotionCalls = mutableListOf<String>()
        var pulledSpillChecks = 0
        var nativeObsCounter = 0
        var checkedState = true
        var inputValue = ""
        var epoch = 1L
        var bgDisplayId = 5
        var bgGeneration = "gen-bg-1"
        var bgFrameSeq = 1L
        var tiles = listOf(100L, 200L)
        var dumpedPackage: String = "com.example.notes"
        var staleNodePoint = false
        var staleNodeAction = false
        var simulateConcurrentTakeoverInNodeAction: (() -> Unit)? = null

        override fun backgroundAvailable() = true
        override fun shellConnected() = true
        override fun ensureBackground() = bgDisplayId
        override fun isBackgroundCurrent(view: String, generation: String) =
            view == "display:$bgDisplayId" && generation == bgGeneration
        override fun backgroundDisplayId() = bgDisplayId
        override fun backgroundGeneration() = bgGeneration
        override fun backgroundFrameSequence() = bgFrameSeq
        override fun awaitBackgroundFrameAfter(previous: Long, timeoutMs: Long) = true
        override fun pokeBackgroundKeepAwake() {}
        override fun mainTopTaskId() = 11
        override fun pullSpilledTasks(packageName: String, preMainTopTaskId: Int) { pulledSpillChecks++ }
        override fun backgroundSnapshot(): Pair<JSONObject, List<Long>>? {
            if (!imagePresent) return null
            val img = obj(
                "mime_type" to "image/jpeg",
                "width" to 540,
                "height" to 1200,
                "blank_or_secure" to if (blankOrSecure) true else null,
                "data" to "AAAA"
            )
            return img to tiles
        }
        override fun backgroundLaunch(app: String) = obj("accepted" to true)
        override fun backgroundKey(code: Int, modifiers: Int) = obj("accepted" to true)
        override fun backgroundMotion(kind: String, x: Double, y: Double, toX: Double, toY: Double, scale: Double): JSONObject {
            backgroundMotionCalls.add(kind)
            return obj("gesture_completed" to true)
        }
        override fun currentEpoch(displayId: Int) = epoch
        override fun dump(displayId: Int): JSONObject {
            dumpCalls++
            val obsId = "native-${++nativeObsCounter}"
            val nodes = JSONArray(
                listOf(
                    obj(
                        "id" to 0, "class" to "android.widget.Switch", "resource_id" to "id/toggle",
                        "text" to "Auto sync", "description" to null, "hint" to null,
                        "bounds" to arr(listOf(40, 100, 300, 180)),
                        "clickable" to true, "editable" to false, "scrollable" to false,
                        "checkable" to true, "checked" to checkedState, "selected" to false,
                        "focused" to false, "enabled" to true, "visible" to true, "password" to false
                    ),
                    obj(
                        "id" to 1, "class" to "android.widget.EditText", "resource_id" to "id/input",
                        "text" to inputValue, "description" to null, "hint" to "Type note",
                        "bounds" to arr(listOf(40, 220, 800, 320)),
                        "clickable" to true, "editable" to true, "scrollable" to false,
                        "checkable" to false, "checked" to JSONObject.NULL, "selected" to false,
                        "focused" to true, "enabled" to true, "visible" to true, "password" to false
                    )
                )
            )
            return obj(
                "observation_id" to obsId,
                "package" to dumpedPackage,
                "display" to obj("width" to 1080, "height" to 2400, "rotation" to 0, "display_id" to displayId, "state" to displayState),
                "nodes" to nodes,
                "truncated" to false
            )
        }
        override fun checkObservation(args: JSONObject) {}
        override fun mainSnapshot(): JSONObject {
            val tree = dump(0)
            val snap = backgroundSnapshot()
            return obj("tree" to tree, "image" to snap?.first, "tile_hashes" to arr(snap?.second ?: emptyList<Long>()))
        }
        override fun nodePoint(args: JSONObject): Pair<Double, Double> {
            if (staleNodePoint) throw AgentError("node_changed_observe_again")
            return Pair(170.0, 140.0)
        }
        override fun nodeAction(kind: String, args: JSONObject, persistent: Boolean): JSONObject {
            if (staleNodeAction) throw AgentError("node_changed_observe_again")
            simulateConcurrentTakeoverInNodeAction?.let { hook ->
                simulateConcurrentTakeoverInNodeAction = null
                hook()
                throw AgentError("node_changed_observe_again")
            }
            nodeActionCalls.add(kind to args)
            if (kind == "set_checked") {
                val desired = args.getBoolean("checked")
                if (checkedState == desired) {
                    return obj("action" to "set_checked", "accepted" to true, "already_satisfied" to true, "checked" to desired, "input_sent" to false, "verified" to true)
                }
                checkedState = desired
                tiles = listOf(101L, 200L)
                return obj("action" to "set_checked", "accepted" to true, "already_satisfied" to false, "checked" to desired, "input_sent" to true, "verified" to false)
            }
            if (kind == "text") {
                inputValue = args.getString("text")
                tiles = listOf(102L, 200L)
            }
            return obj("action" to kind, "accepted" to true)
        }
        override fun uiGesture(kind: String, args: JSONObject): JSONObject {
            gestureCalls.add(kind to args)
            return obj("gesture_completed" to true)
        }
        override fun uiKey(args: JSONObject, action: JSONObject) = obj("accepted" to true)
        override fun globalAction(action: String) = obj("accepted" to true)
        override fun launchMainApp(app: String) = obj("accepted" to true)
        override fun sleep(ms: Long) {}
        override fun elapsedRealtime() = System.nanoTime() / 1_000_000L
        override fun close() {}
    }

    private fun job(tool: String, args: JSONObject) = obj(
        "id" to UUID.randomUUID().toString(),
        "device_id" to "dev-1",
        "owner" to "owner-1",
        "tool" to tool,
        "arguments" to args
    )

    @Test fun staleRefPathNeverSendsOrReplaysInput() {
        for (type in listOf("click", "double_click")) {
            val backend = FakeBackend().apply { staleNodeAction = true; staleNodePoint = true }
            val controller = UiController("dev-1", backend, AiViewHost())
            val observation = controller.perform(job("ui_observe", obj("device" to "dev-1", "view" to "active")))
            val args = obj("device" to "dev-1", "observation_id" to observation.getString("observation_id"),
                "action" to obj("type" to type, "target" to obj("ref" to "n0")))
            val result = controller.perform(job("ui_act", args))
            assertEquals("stale_observation", result.getString("error"))
            assertFalse(result.getBoolean("input_sent"))
            assertEquals("stale_observation", controller.perform(job("ui_act", args)).getString("error"))
            assertTrue(backend.nodeActionCalls.isEmpty())
            assertTrue(backend.gestureCalls.isEmpty())
        }
    }

    @Test fun semanticFieldsAndZeroIoQueryPreserveObservations() {
        val backend = FakeBackend()
        val controller = UiController("dev-1", backend, AiViewHost())
        val obs = controller.perform(job("ui_observe", obj("device" to "dev-1", "view" to "active")))
        assertEquals(1, backend.dumpCalls)
        val elements = obs.getJSONArray("elements")
        assertEquals(2, elements.length())
        val n0 = elements.getJSONObject(0)
        assertEquals(true, n0.getBoolean("checkable"))
        assertEquals(true, n0.getBoolean("checked"))
        assertEquals(false, n0.getBoolean("selected"))
        assertEquals(false, n0.getBoolean("focused"))
        val n1 = elements.getJSONObject(1)
        assertEquals("Type note", n1.getString("hint"))
        assertTrue(n1.isNull("checked"))
        assertEquals(true, n1.getBoolean("focused"))

        // Zero-I/O query against saved observation_id must not call dump() again
        val filtered = controller.perform(
            job(
                "ui_observe",
                obj(
                    "device" to "dev-1",
                    "observation_id" to obs.getString("observation_id"),
                    "query" to obj("role" to "EditText", "interactive_only" to true)
                )
            )
        )
        assertEquals(1, backend.dumpCalls)
        assertEquals(1, filtered.getJSONArray("elements").length())
        assertEquals("n1", filtered.getJSONArray("elements").getJSONObject(0).getString("ref"))

        // Original observation_id must still be usable for ui_act (not consumed by query!)
        val actRes = controller.perform(
            job(
                "ui_act",
                obj(
                    "device" to "dev-1",
                    "observation_id" to obs.getString("observation_id"),
                    "action" to obj("type" to "fill", "target" to obj("ref" to "n1"), "text" to "updated"),
                    "expect" to obj("value_equals" to "updated", "visual_changed" to true)
                )
            )
        )
        assertFalse(actRes.has("error"))
        assertEquals(true, actRes.getBoolean("expect_met"))
        assertEquals(true, actRes.getBoolean("visual_changed"))
    }

    @Test fun setCheckedIdempotencyAndTripleClickWorkCleanly() {
        val backend = FakeBackend().apply { checkedState = true }
        val controller = UiController("dev-1", backend, AiViewHost())
        val obs1 = controller.perform(job("ui_observe", obj("device" to "dev-1", "view" to "active")))
        val satisfied = controller.perform(
            job(
                "ui_act",
                obj(
                    "device" to "dev-1",
                    "observation_id" to obs1.getString("observation_id"),
                    "action" to obj("type" to "set_checked", "target" to obj("ref" to "n0"), "checked" to true),
                    "expect" to obj("checked_equals" to true)
                )
            )
        )
        assertEquals(true, satisfied.getBoolean("already_satisfied"))
        assertEquals(false, satisfied.getBoolean("input_sent"))
        assertEquals(true, satisfied.getBoolean("expect_met"))

        val obs2Id = satisfied.getJSONObject("observation").getString("observation_id")
        val toggled = controller.perform(
            job(
                "ui_act",
                obj(
                    "device" to "dev-1",
                    "observation_id" to obs2Id,
                    "action" to obj("type" to "set_checked", "target" to obj("ref" to "n0"), "checked" to false),
                    "expect" to obj("checked_equals" to false)
                )
            )
        )
        assertEquals(false, toggled.getBoolean("already_satisfied"))
        assertEquals(true, toggled.getBoolean("input_sent"))
        assertEquals(true, toggled.getBoolean("expect_met"))

        val obs3Id = toggled.getJSONObject("observation").getString("observation_id")
        val triple = controller.perform(
            job(
                "ui_act",
                obj(
                    "device" to "dev-1",
                    "observation_id" to obs3Id,
                    "action" to obj("type" to "triple_click", "target" to obj("ref" to "n1"))
                )
            )
        )
        assertEquals("triple_click", backend.gestureCalls.single().first)
        assertEquals(true, triple.getBoolean("input_sent"))
    }

    @Test fun observeDistinguishesNotRenderedSleepingAndBlankOrSecure() {
        val backend = FakeBackend(displayState = Display.STATE_ON, imagePresent = false, blankOrSecure = false)
        val controller = UiController("dev-1", backend, AiViewHost())

        val notRendered = controller.perform(job("ui_observe", obj("device" to "dev-1", "view" to "background")))
        assertEquals("view_has_not_rendered", notRendered.getJSONObject("image_unavailable").getString("reason"))
        assertEquals("view_has_not_rendered", notRendered.getString("image_status"))

        backend.displayState = Display.STATE_OFF
        val sleeping = controller.perform(job("ui_observe", obj("device" to "dev-1", "view" to "background")))
        assertEquals("display_sleeping", sleeping.getJSONObject("image_unavailable").getString("reason"))
        assertEquals("display_sleeping", sleeping.getString("image_status"))

        backend.displayState = Display.STATE_ON
        backend.imagePresent = true
        backend.blankOrSecure = true
        val secure = controller.perform(job("ui_observe", obj("device" to "dev-1", "view" to "background")))
        assertEquals("blank_or_secure", secure.getString("image_status"))
        assertTrue(secure.getString("takeover_hint").contains("FLAG_SECURE"))
    }

    @Test fun humanTakeoverViaBridgeIncrementsEpochAndRejectsStaleAiActWithFreshObservation() {
        val backend = FakeBackend()
        val host = AiViewHost()
        var aiCanceled = 0
        var aiInvalidated = 0
        val fakeSink = object : AiInputSink {
            override fun injectTouch(displayId: Int, downTimeMs: Long, eventTimeMs: Long, action: Int, pointerCount: Int, pointerIds: IntArray, xs: FloatArray, ys: FloatArray, pressures: FloatArray) = true
            override fun injectKey(displayId: Int, downTimeMs: Long, eventTimeMs: Long, action: Int, androidKeyCode: Int, metaState: Int, flags: Int) = true
            override fun commitText(displayId: Int, text: String) = true
            override fun uptimeMs() = 1000L
        }
        host.register(
            AiViewHost.ActiveView(
                viewId = "display:5",
                displayId = 5,
                generation = "gen-bg-1",
                width = 1080,
                height = 2400,
                density = 440,
                rotation = 0,
                surfaceBridge = object : AiViewHost.SurfaceBridge {
                    override fun attachSurface(surface: Any) = "consumer-1"
                    override fun detachSurface(consumerId: String) {}
                },
                inputSink = fakeSink,
                cancelAiInput = { aiCanceled++ },
                invalidateAiObservations = { aiInvalidated++ },
                resolveAppInfo = { AiViewHost.ViewAppInfo("com.example.notes", "Notes", 0) }
            )
        )
        val controller = UiController("dev-1", backend, host)

        // 1. AI observes background display at resource_epoch = 1
        val aiObs = controller.perform(job("ui_observe", obj("device" to "dev-1", "view" to "background")))
        val aiObsId = aiObs.getString("observation_id")
        assertEquals(1L, aiObs.getJSONObject("source").getLong("resource_epoch"))
        assertEquals(false, aiObs.getBoolean("human_active"))

        // 2. Weavelo attaches video and human dispatches input
        val attach = host.attachVideo("display:5", "gen-bg-1", "fake-surface")
        assertTrue(attach.getBoolean("ok"))
        val dispatch = host.dispatchInput(
            consumerId = "consumer-1",
            viewId = "display:5",
            generation = "gen-bg-1",
            sourceRevision = 1L,
            eventJson = """{"TouchFrame":[{"action":"Down","pointer_id":0,"normalized_x":0.5,"normalized_y":0.5,"pressure":1.0}]}"""
        )
        assertTrue(dispatch.getBoolean("ok"))
        assertTrue(dispatch.getBoolean("accepted"))
        assertEquals(2L, dispatch.getLong("resource_epoch"))
        assertEquals(1, aiCanceled)
        assertEquals(1, aiInvalidated)

        // 3. Subsequent AI ui_act using pre-takeover observation_id MUST return takeover_by_human + fresh observation
        val staleAct = controller.perform(
            job(
                "ui_act",
                obj(
                    "device" to "dev-1",
                    "observation_id" to aiObsId,
                    "action" to obj("type" to "click", "target" to obj("ref" to "n0"))
                )
            )
        )
        assertEquals("takeover_by_human", staleAct.getString("error"))
        assertEquals(false, staleAct.getBoolean("input_sent"))
        assertEquals(true, staleAct.getBoolean("human_active"))
        assertEquals(2L, staleAct.getLong("resource_epoch"))
        assertTrue(staleAct.has("observation"))
        assertTrue(backend.nodeActionCalls.isEmpty())

        // 4. A normal pointer-up releases ownership without disconnecting video.
        val released = host.dispatchInput("consumer-1", "display:5", "gen-bg-1", 1L,
            """{"TouchFrame":[{"action":"Up","pointer_id":0,"normalized_x":0.5,"normalized_y":0.5,"pressure":1.0}]}""")
        assertTrue(released.getBoolean("accepted"))
        assertFalse(host.isHumanActive("display:5"))
        val freshObs = controller.perform(job("ui_observe", obj("device" to "dev-1", "view" to "background")))
        val resumedAct = controller.perform(
            job(
                "ui_act",
                obj(
                    "device" to "dev-1",
                    "observation_id" to freshObs.getString("observation_id"),
                    "action" to obj("type" to "click", "target" to obj("x" to 100, "y" to 200))
                )
            )
        )
        assertFalse(resumedAct.has("error"))
        assertEquals(listOf("click"), backend.backgroundMotionCalls)
        assertEquals(1, backend.pulledSpillChecks)
    }

    @Test fun dualDisplayImeIsolationNeverUsesMainInputConnectionForVirtualDisplays() {
        // Virtual display MUST NEVER use main-display inputMethod even when package & focus match!
        assertFalse(
            ImeIsolationPolicy.canUseInputMethodConnection(
                displayId = 5,
                sdkInt = 35,
                nodeFocused = true,
                imePackage = "com.tencent.mm",
                seenPackage = "com.tencent.mm",
                focusedWindowOnDisplayMatches = true
            )
        )
        // Main display requires SDK >= 33, focus on main display, and matching IME package
        assertTrue(
            ImeIsolationPolicy.canUseInputMethodConnection(
                displayId = Display.DEFAULT_DISPLAY,
                sdkInt = 35,
                nodeFocused = true,
                imePackage = "com.tencent.mm",
                seenPackage = "com.tencent.mm",
                focusedWindowOnDisplayMatches = true
            )
        )
        assertFalse(
            ImeIsolationPolicy.canUseInputMethodConnection(
                displayId = Display.DEFAULT_DISPLAY,
                sdkInt = 35,
                nodeFocused = true,
                imePackage = "com.other.app",
                seenPackage = "com.tencent.mm",
                focusedWindowOnDisplayMatches = true
            )
        )
    }

    @Test fun displayKeepAwakeLifecycleNeverTouchesDefaultDisplayAndStopsOnRelease() {
        val posted = mutableListOf<Pair<Long, Runnable>>()
        val refreshed = mutableListOf<Int>()
        val keepAwake = DisplayKeepAwake(
            refreshIntervalMs = 15_000L,
            scheduler = object : DisplayKeepAwake.Scheduler {
                override fun post(delayMs: Long, runnable: Runnable) { posted.add(delayMs to runnable) }
                override fun remove(runnable: Runnable) { posted.removeAll { it.second === runnable } }
            },
            refresher = { id -> refreshed.add(id); true }
        )

        keepAwake.start(Display.DEFAULT_DISPLAY)
        assertTrue(refreshed.isEmpty())
        assertTrue(posted.isEmpty())

        keepAwake.start(7)
        assertEquals(listOf(7), refreshed)
        assertEquals(1, posted.size)
        assertEquals(15_000L, posted[0].first)

        // Simulate scheduled tick
        posted.removeAt(0).second.run()
        assertEquals(listOf(7, 7), refreshed)
        assertEquals(1, posted.size)

        keepAwake.stop(7)
        assertFalse(keepAwake.isActive(7))
        assertTrue(posted.isEmpty())
        keepAwake.close()
    }

    @Test fun concurrentTakeoverDuringActSurfacesTakeoverByHumanBlankPackageSkipsSpillAndWindowSelectorWorks() {
        val backend = FakeBackend().apply { dumpedPackage = "" }
        val host = AiViewHost()
        val sink = object : AiInputSink {
            override fun injectTouch(displayId: Int, downTimeMs: Long, eventTimeMs: Long, action: Int, pointerCount: Int, pointerIds: IntArray, xs: FloatArray, ys: FloatArray, pressures: FloatArray) = true
            override fun injectKey(displayId: Int, downTimeMs: Long, eventTimeMs: Long, action: Int, androidKeyCode: Int, metaState: Int, flags: Int) = true
            override fun commitText(displayId: Int, text: String) = true
            override fun uptimeMs() = 1000L
        }
        host.register(
            AiViewHost.ActiveView(
                viewId = "display:5",
                displayId = 5,
                generation = "gen-bg-1",
                width = 1080,
                height = 2400,
                density = 440,
                rotation = 0,
                surfaceBridge = object : AiViewHost.SurfaceBridge {
                    override fun attachSurface(surface: Any) = "consumer-1"
                    override fun detachSurface(consumerId: String) {}
                },
                inputSink = sink,
                cancelAiInput = {},
                invalidateAiObservations = {},
                resolveAppInfo = { AiViewHost.ViewAppInfo("", "AI", 0) }
            )
        )
        host.attachVideo("display:5", "gen-bg-1", "surf")
        val controller = UiController("dev-1", backend, host)

        // target: {"window": "background"} resolves background view cleanly
        val obs = controller.perform(job("ui_observe", obj("device" to "dev-1", "target" to obj("window" to "background"))))
        assertEquals("display:5", obs.getString("view"))

        // Action with blank packageName must NOT invoke pullSpilledTasks
        val clickRes = controller.perform(
            job(
                "ui_act",
                obj(
                    "device" to "dev-1",
                    "observation_id" to obs.getString("observation_id"),
                    "action" to obj("type" to "click", "target" to obj("x" to 100, "y" to 200))
                )
            )
        )
        assertFalse(clickRes.has("error"))
        assertEquals(0, backend.pulledSpillChecks)

        // Concurrent human takeover occurring right as nodeAction runs -> surfaces takeover_by_human (not stale_observation)
        val obs2Id = clickRes.getJSONObject("observation").getString("observation_id")
        backend.simulateConcurrentTakeoverInNodeAction = {
            host.dispatchInput("consumer-1", "display:5", "gen-bg-1", 1L, """{"Text":"hi"}""")
        }
        val raceRes = controller.perform(
            job(
                "ui_act",
                obj(
                    "device" to "dev-1",
                    "observation_id" to obs2Id,
                    "action" to obj("type" to "click", "target" to obj("ref" to "n0"))
                )
            )
        )
        assertEquals("takeover_by_human", raceRes.getString("error"))
        assertTrue(raceRes.has("observation"))
    }
}
