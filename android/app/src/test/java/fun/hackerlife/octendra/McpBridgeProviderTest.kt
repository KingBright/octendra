package `fun`.hackerlife.octendra

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class McpBridgeProviderTest {
    @Test fun callerPolicyAllowsTelosOwnerShellAndRejectsUntrustedOrSharedUid() {
        val own = setOf("aaaa".repeat(16))
        val owner = McpCallerPolicy.OWNER_CERT_SHA256
        val telos = McpCallerPolicy.TELOS_CERT_SHA256
        val evil = "cccc".repeat(16)

        assertTrue(McpCallerPolicy.isAllowed(10001, 10001, own, null, emptyList()))
        assertTrue(McpCallerPolicy.isAllowed(2000, 10001, own, null, emptyList()))
        assertTrue(McpCallerPolicy.isAllowed(10088, 10001, own, null, listOf(McpCallerPolicy.TELOS_PACKAGE to setOf(telos))))
        assertTrue(McpCallerPolicy.isAllowed(10088, 10001, own, null, listOf(McpCallerPolicy.TELOS_PACKAGE to setOf(owner))))
        assertTrue(McpCallerPolicy.isAllowed(10089, 10001, own, null, listOf("fun.hackerlife.peer" to setOf(owner))))

        // Spoofed cert or shared-UID alongside Telos must be denied
        assertFalse(McpCallerPolicy.isAllowed(10088, 10001, own, null, listOf(McpCallerPolicy.TELOS_PACKAGE to setOf(evil))))
        assertFalse(
            McpCallerPolicy.isAllowed(
                10088, 10001, own, null,
                listOf(McpCallerPolicy.TELOS_PACKAGE to setOf(telos), "other.pkg" to setOf(telos))
            )
        )
        assertFalse(McpCallerPolicy.isAllowed(10088, 10001, own, null, listOf(McpCallerPolicy.TELOS_PACKAGE to setOf(telos, evil))))
        assertFalse(McpCallerPolicy.isAllowed(10099, 10001, own, null, emptyList()))
        assertFalse(McpCallerPolicy.isAllowed(10099, 10001, own, null, listOf("evil.app" to setOf(evil))))
    }

    @Test fun mcpRpcSupportsInitializeListObserveImageScrubAndAct() {
        val recordedCalls = mutableListOf<Pair<String, JSONObject>>()
        var ready = true
        var backgroundAvail = true

        val handler = McpRpcHandler(
            backend = object : McpRpcHandler.Backend {
                override fun deviceId() = "64d9bc00-4bd2-4fe2-8c16-be10ec4ffe9c"

                override fun status() = obj(
                    "status" to "ok",
                    "ready_for_observe" to ready,
                    "background_available" to backgroundAvail,
                    "accessibility_connected" to true,
                    "remote_control_enabled" to ready,
                    "agent_service_running" to ready,
                    "blocking_reason" to if (ready) null else "remote_control_stopped_locally"
                )

                override fun performUi(tool: String, arguments: JSONObject): JSONObject {
                    recordedCalls.add(tool to JSONObject(arguments.toString()))
                    val fakeImg = obj(
                        "mime_type" to "image/jpeg",
                        "width" to 540,
                        "height" to 1200,
                        "som_badges" to 2,
                        "data" to "FAKE_BASE64_SCREENSHOT"
                    )
                    return if (tool == "ui_observe") {
                        obj(
                            "observation_id" to "obs-123",
                            "device" to deviceId(),
                            "view" to "display:5",
                            "image" to fakeImg
                        )
                    } else {
                        obj(
                            "accepted" to true,
                            "observation" to obj(
                                "observation_id" to "obs-124",
                                "device" to deviceId(),
                                "image" to fakeImg
                            )
                        )
                    }
                }
            },
            serverVersion = "0.12.13"
        )

        val initResp = handler.handleRpc("""{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}""")
        assertEquals("2024-11-05", initResp.getJSONObject("result").getString("protocolVersion"))
        assertEquals("0.12.13", initResp.getJSONObject("result").getJSONObject("serverInfo").getString("version"))

        val listResp = handler.handleRpc("""{"jsonrpc":"2.0","id":2,"method":"tools/list"}""")
        val tools = listResp.getJSONObject("result").getJSONArray("tools")
        val names = (0 until tools.length()).map { tools.getJSONObject(it).getString("name") }
        assertEquals(listOf("phone_ui_status", "phone_ui_observe", "phone_ui_act"), names)

        // Default view prefers background when background_available=true, and extracts image block while scrubbing duplicate base64
        val obsResp = handler.handleRpc(
            """{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"phone_ui_observe","arguments":{}}}"""
        )
        assertEquals("ui_observe", recordedCalls[0].first)
        assertEquals("background", recordedCalls[0].second.getString("view"))
        val obsContent = obsResp.getJSONObject("result").getJSONArray("content")
        assertEquals(2, obsContent.length())
        assertEquals("image", obsContent.getJSONObject(0).getString("type"))
        assertEquals("FAKE_BASE64_SCREENSHOT", obsContent.getJSONObject(0).getString("data"))
        val obsText = JSONObject(obsContent.getJSONObject(1).getString("text"))
        assertFalse(obsText.getJSONObject("image").has("data"))
        assertTrue(obsText.getJSONObject("image").getBoolean("attached_to_mcp_image"))

        // phone_ui_act attaches post-action image cleanly
        val actResp = handler.handleRpc(
            """{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"phone_ui_act","arguments":{"observation_id":"obs-123","action":{"type":"click","target":{"ref":"n1"}}}}}"""
        )
        assertEquals("ui_act", recordedCalls[1].first)
        val actContent = actResp.getJSONObject("result").getJSONArray("content")
        assertEquals("image", actContent.getJSONObject(0).getString("type"))
        val actText = JSONObject(actContent.getJSONObject(1).getString("text"))
        assertFalse(actText.getJSONObject("observation").getJSONObject("image").has("data"))

        // Not-ready state surfaces takeover metadata instead of failing cryptically
        ready = false
        val blockedResp = handler.handleRpc(
            """{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"phone_ui_observe","arguments":{}}}"""
        )
        assertTrue(blockedResp.getJSONObject("result").getBoolean("isError"))
        val blockedPayload = JSONObject(blockedResp.getJSONObject("result").getJSONArray("content").getJSONObject(0).getString("text"))
        assertEquals("remote_control_stopped_locally", blockedPayload.getString("error"))
        assertEquals("open_octendra", blockedPayload.getString("takeover_action"))
    }
}
