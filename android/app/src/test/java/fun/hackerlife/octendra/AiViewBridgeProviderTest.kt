package `fun`.hackerlife.octendra

import android.view.KeyEvent
import android.view.MotionEvent
import org.json.JSONArray
import org.junit.Assert.*
import org.junit.Test

class AiViewBridgeProviderTest {
    @Test fun callerVerificationAllowsOwnOwnerShellAndRejectsUntrusted() {
        val own = setOf("aaaa".repeat(16))
        val owner = AiViewCallerPolicy.OWNER_CERT_SHA256
        val peer = "bbbb".repeat(16)
        val evil = "cccc".repeat(16)

        assertTrue(AiViewCallerPolicy.isAllowed(10001, 10001, own, null, emptyList()))
        assertTrue(AiViewCallerPolicy.isAllowed(2000, 10001, own, null, emptyList()))
        assertTrue(AiViewCallerPolicy.isAllowed(10055, 10001, own, null, listOf(own)))
        assertTrue(AiViewCallerPolicy.isAllowed(10055, 10001, own, null, listOf(setOf(owner))))
        assertTrue(AiViewCallerPolicy.isAllowed(10055, 10001, own, peer, listOf(setOf(peer))))

        assertFalse(AiViewCallerPolicy.isAllowed(10055, 10001, own, peer, emptyList()))
        assertFalse(AiViewCallerPolicy.isAllowed(10055, 10001, own, peer, listOf(setOf(evil))))
        // Multi-signer package with an untrusted co-signer must be rejected
        assertFalse(AiViewCallerPolicy.isAllowed(10055, 10001, own, peer, listOf(setOf(owner, evil))))
    }

    @Test fun providerBridgeContractSupportsDescribeAttachInputReleaseAndNonDestructiveDetach() {
        val touchEvents = mutableListOf<Triple<Int, Int, IntArray>>()
        val keyEvents = mutableListOf<Triple<Int, Int, Int>>() // (action, keyCode, flags)
        val committedTexts = mutableListOf<String>()
        var surfaceDetached = 0
        var aiCanceled = 0

        val sink = object : AiInputSink {
            override fun injectTouch(displayId: Int, downTimeMs: Long, eventTimeMs: Long, action: Int, pointerCount: Int, pointerIds: IntArray, xs: FloatArray, ys: FloatArray, pressures: FloatArray): Boolean {
                touchEvents.add(Triple(action, pointerCount, pointerIds.copyOf()))
                return true
            }
            override fun injectKey(displayId: Int, downTimeMs: Long, eventTimeMs: Long, action: Int, androidKeyCode: Int, metaState: Int, flags: Int): Boolean {
                keyEvents.add(Triple(action, androidKeyCode, flags))
                return true
            }
            override fun commitText(displayId: Int, text: String): Boolean {
                committedTexts.add(text)
                return true
            }
            override fun uptimeMs() = 42L
        }

        val host = AiViewHost()
        host.register(
            AiViewHost.ActiveView(
                viewId = "display:9",
                displayId = 9,
                generation = "gen-9",
                width = 1080,
                height = 2400,
                density = 440,
                rotation = 0,
                surfaceBridge = object : AiViewHost.SurfaceBridge {
                    override fun attachSurface(surface: Any) = "consumer-9"
                    override fun detachSurface(consumerId: String) { surfaceDetached++ }
                },
                inputSink = sink,
                cancelAiInput = { aiCanceled++ },
                invalidateAiObservations = {},
                resolveAppInfo = { AiViewHost.ViewAppInfo("com.tencent.mm", "WeChat", 999) }
            )
        )

        // 1. describe_views
        val views = JSONArray(host.describeViews())
        assertEquals(1, views.length())
        val v = views.getJSONObject(0)
        assertEquals("display:9", v.getString("view_id"))
        assertEquals(9, v.getInt("display_id"))
        assertEquals("gen-9", v.getString("generation"))
        assertEquals(1L, v.getLong("source_revision"))
        assertEquals(1L, v.getLong("resource_epoch"))
        assertEquals("com.tencent.mm", v.getString("package"))
        assertEquals("WeChat", v.getString("label"))
        assertEquals(999, v.getInt("user_id"))
        assertEquals(1080, v.getInt("width"))
        assertEquals(2400, v.getInt("height"))
        assertEquals(440, v.getInt("density"))
        assertEquals(0, v.getInt("rotation"))
        assertFalse(v.getBoolean("human_active"))
        assertTrue(v.getBoolean("supports_input"))

        // 2. attach_video
        val attached = host.attachVideo("display:9", "gen-9", "surface-token")
        assertTrue(attached.getBoolean("ok"))
        assertEquals("consumer-9", attached.getString("consumer_id"))

        // 3. dispatch_input: source_revision mismatch rejected
        val revMismatch = host.dispatchInput("consumer-9", "display:9", "gen-9", 99L, """{"Text":"hi"}""")
        assertFalse(revMismatch.getBoolean("ok"))
        assertEquals("source_revision_mismatch", revMismatch.getString("error"))

        // 4. dispatch_input: multi-touch TouchFrame (pointer 0 Down, pointer 1 Down), Key down, and UTF-8 Text
        val touchRes = host.dispatchInput(
            "consumer-9", "display:9", "gen-9", 1L,
            """{"TouchFrame":[{"action":"Down","pointer_id":0,"normalized_x":0.25,"normalized_y":0.5,"pressure":1.0},{"action":"Down","pointer_id":1,"normalized_x":0.75,"normalized_y":0.5,"pressure":1.0}]}"""
        )
        assertTrue(touchRes.getBoolean("ok"))
        assertTrue(touchRes.getBoolean("accepted"))
        assertEquals(2L, touchRes.getLong("resource_epoch"))
        assertTrue(host.isHumanActive("display:9"))
        assertEquals(1, aiCanceled)

        val keyDown = host.dispatchInput(
            "consumer-9", "display:9", "gen-9", 1L,
            """{"Key":{"key_code":0,"pressed":true,"modifiers":0}}"""
        )
        assertEquals(3L, keyDown.getLong("resource_epoch"))
        assertEquals(KeyEvent.KEYCODE_A, keyEvents.last().second)

        val textRes = host.dispatchInput(
            "consumer-9", "display:9", "gen-9", 1L,
            """{"Text":"你好世界🙂"}"""
        )
        assertEquals(4L, textRes.getLong("resource_epoch"))
        assertEquals(listOf("你好世界🙂"), committedTexts)

        // 5. release_input cancels held touches (ACTION_CANCEL) and held keys (FLAG_CANCELED)
        val rel = host.releaseInput("consumer-9")
        assertTrue(rel.getBoolean("ok"))
        assertFalse(host.isHumanActive("display:9"))
        assertEquals(MotionEvent.ACTION_CANCEL, touchEvents.last().first)
        assertEquals(KeyEvent.ACTION_UP, keyEvents.last().first)
        assertEquals(KeyEvent.FLAG_CANCELED, keyEvents.last().third and KeyEvent.FLAG_CANCELED)

        // 6. detach_video only detaches consumer without closing the AI view
        val detached = host.detachVideo("consumer-9")
        assertTrue(detached.getBoolean("ok"))
        assertEquals(1, surfaceDetached)
        assertEquals(1, JSONArray(host.describeViews()).length())

        // 7. Re-attach new consumer and test MouseMoveAbsolute, MouseDown(0), MouseScroll, ModifiersChanged, and android_key_code
        val reattached = host.attachVideo("display:9", "gen-9", "surface-token-2")
        assertTrue(reattached.getBoolean("ok"))
        val cid2 = reattached.getString("consumer_id")
        assertTrue(host.dispatchInput(cid2, "display:9", "gen-9", 1L, """{"MouseMoveAbsolute":{"x":32768,"y":32768}}""").getBoolean("accepted"))
        assertTrue(host.dispatchInput(cid2, "display:9", "gen-9", 1L, """{"MouseDown":0}""").getBoolean("accepted"))
        assertTrue(host.dispatchInput(cid2, "display:9", "gen-9", 1L, """{"ModifiersChanged":1}""").getBoolean("accepted"))
        assertTrue(host.dispatchInput(cid2, "display:9", "gen-9", 1L, """{"Key":{"key_code":999,"android_key_code":66,"pressed":true,"modifiers":0}}""").getBoolean("accepted"))
        assertTrue(host.releaseInput(cid2).getBoolean("ok"))
        assertEquals(MotionEvent.ACTION_CANCEL, touchEvents.last().first)
        assertEquals(KeyEvent.KEYCODE_ENTER, keyEvents.last().second)

        // Wrong generation is rejected as stale_view_or_consumer
        val badGen = host.dispatchInput(cid2, "display:9", "wrong-gen", 1L, """{"Text":"x"}""")
        assertFalse(badGen.getBoolean("ok"))
        assertEquals("stale_view_or_consumer", badGen.getString("error"))
    }
}
