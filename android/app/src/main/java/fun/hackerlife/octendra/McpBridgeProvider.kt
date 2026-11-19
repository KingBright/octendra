package `fun`.hackerlife.octendra

import android.content.ContentProvider
import android.content.ContentValues
import android.content.Context
import android.content.pm.PackageManager
import android.database.Cursor
import android.net.Uri
import android.os.Binder
import android.os.Build
import android.os.Bundle
import android.os.Process
import android.view.Display
import org.json.JSONArray
import org.json.JSONObject
import java.util.UUID

/**
 * Caller trust policy for the exported Binder MCP ContentProvider (`fun.hackerlife.octendra.mcp`).
 * Mirrors `AiViewCallerPolicy` single-UID / certificate SHA-256 verification and explicitly
 * trusts `fun.hackerlife.telos` as well as owner-signed apps and local ADB shell.
 */
internal object McpCallerPolicy {
    const val AUTHORITY = "fun.hackerlife.octendra.mcp"
    const val OWNER_CERT_SHA256 = "f84d9b71f1a00f4b5e3084420e5cee3327053389983b2428d9bec71755e5539d"
    const val TELOS_PACKAGE = "fun.hackerlife.telos"
    const val TELOS_CERT_SHA256 = "480fe79689e663b4e9d0173deba93de2559b4cfc0f523ae17962656181e80dbe"

    fun isAllowed(
        callingUid: Int,
        selfUid: Int,
        ownCertsSha256: Set<String>,
        configuredPeerFingerprint: String?,
        callerPackageSigners: List<Pair<String, Set<String>>>
    ): Boolean {
        if (callingUid == selfUid || callingUid == 2000 || callingUid == 0) return true
        if (callerPackageSigners.isEmpty()) return false

        val normalizedPeer = configuredPeerFingerprint?.trim()?.lowercase()
            ?.takeIf { it.matches(Regex("[0-9a-f]{64}")) }

        if (callerPackageSigners.any { it.first == TELOS_PACKAGE }) {
            // Binder calls identify UID rather than package; refuse shared-UID spoofing.
            if (callerPackageSigners.size != 1) return false
            val signers = callerPackageSigners.single().second.map { it.lowercase() }.toSet()
            if (signers.isEmpty()) return false
            val telosAllowed = buildSet {
                add(TELOS_CERT_SHA256)
                add(OWNER_CERT_SHA256)
                if (normalizedPeer != null) add(normalizedPeer)
            }
            return signers.all { it in telosAllowed }
        }

        val allowed = HashSet<String>(ownCertsSha256.size + 3)
        ownCertsSha256.forEach { allowed.add(it.lowercase()) }
        allowed.add(OWNER_CERT_SHA256)
        if (normalizedPeer != null) allowed.add(normalizedPeer)

        return callerPackageSigners.all { (_, signers) ->
            signers.isNotEmpty() && signers.all { it.lowercase() in allowed }
        }
    }
}

/** Pure JSON-RPC 2.0 MCP router testable on JVM without Binder or Accessibility dependencies. */
internal class McpRpcHandler(
    private val backend: Backend,
    private val serverVersion: String = BuildConfig.VERSION_NAME
) {
    interface Backend {
        fun deviceId(): String
        fun status(): JSONObject
        fun performUi(tool: String, arguments: JSONObject): JSONObject
    }

    fun handleRpc(rpcJson: String): JSONObject {
        val req = try {
            JSONObject(rpcJson)
        } catch (_: Exception) {
            return rpcError(JSONObject.NULL, -32700, "parse_error")
        }
        val id = req.opt("id") ?: JSONObject.NULL
        val method = req.optString("method", "")
        val params = req.optJSONObject("params") ?: JSONObject()

        return when (method) {
            "initialize" -> rpcResult(
                id,
                obj(
                    "protocolVersion" to "2024-11-05",
                    "capabilities" to obj("tools" to obj("listChanged" to false)),
                    "serverInfo" to obj("name" to "octendra-phone-mcp", "version" to serverVersion)
                )
            )

            "notifications/initialized", "ping" -> rpcResult(id, JSONObject())

            "tools/list" -> rpcResult(id, obj("tools" to toolDefinitions()))

            "resources/list" -> rpcResult(id, obj("resources" to JSONArray()))

            "tools/call" -> {
                val toolName = params.optString("name", "")
                val arguments = params.optJSONObject("arguments") ?: JSONObject()
                try {
                    val toolResult = executeTool(toolName, arguments)
                    rpcResult(id, toolResult)
                } catch (e: Exception) {
                    val err = Policy.error(e)
                    rpcResult(
                        id,
                        obj(
                            "isError" to true,
                            "content" to JSONArray().put(
                                obj("type" to "text", "text" to err.toString())
                            )
                        )
                    )
                }
            }

            else -> rpcError(id, -32601, "method_not_found")
        }
    }

    private fun executeTool(name: String, rawArgs: JSONObject): JSONObject {
        return when (name) {
            "phone_ui_status" -> {
                val st = backend.status()
                obj("content" to JSONArray().put(obj("type" to "text", "text" to st.toString())))
            }

            "phone_ui_observe" -> {
                val status = backend.status()
                if (!status.optBoolean("ready_for_observe", false)) {
                    val reason = status.optString("blocking_reason", "remote_control_stopped_locally")
                    val blocked = obj(
                        "error" to reason,
                        "input_sent" to false,
                        "takeover_required" to true,
                        "takeover_action" to "open_octendra",
                        "readiness" to status
                    )
                    return obj(
                        "isError" to true,
                        "content" to JSONArray().put(obj("type" to "text", "text" to blocked.toString()))
                    )
                }

                val allowed = setOf("view", "context", "target", "query", "observation_id", "idempotency_key", "response_mode")
                val args = JSONObject()
                for (k in allowed) {
                    if (rawArgs.has(k) && !rawArgs.isNull(k)) {
                        args.put(k, rawArgs.get(k))
                    }
                }
                args.put("device", backend.deviceId())

                val requestedView = args.optString("view", "").trim()
                val normalizedView = when (requestedView.lowercase()) {
                    "" -> if (status.optBoolean("background_available", false)) "background" else "active"
                    "host", "main", "screen" -> "active"
                    else -> requestedView
                }
                if (!args.has("observation_id")) {
                    args.put("view", normalizedView)
                }

                val res = backend.performUi("ui_observe", args)
                formatUiResultWithMedia(res)
            }

            "phone_ui_act" -> {
                val allowed = setOf("observation_id", "action", "observe_after", "expect", "idempotency_key", "response_mode")
                val args = JSONObject()
                for (k in allowed) {
                    if (rawArgs.has(k) && !rawArgs.isNull(k)) {
                        args.put(k, rawArgs.get(k))
                    }
                }
                args.put("device", backend.deviceId())
                val res = backend.performUi("ui_act", args)
                formatUiResultWithMedia(res)
            }

            else -> throw AgentError("unknown_mcp_tool")
        }
    }

    private fun formatUiResultWithMedia(rawRes: JSONObject): JSONObject {
        val copy = JSONObject(rawRes.toString())
        val content = JSONArray()
        val img = copy.optJSONObject("image")
            ?: copy.optJSONObject("observation")?.optJSONObject("image")
        if (img != null) {
            val data = img.optString("data", "")
            val mime = img.optString("mime_type", "image/jpeg")
            if (data.isNotBlank()) {
                content.put(obj("type" to "image", "mimeType" to mime, "data" to data))
                img.remove("data")
                img.put("attached_to_mcp_image", true)
            }
        }
        content.put(obj("type" to "text", "text" to copy.toString()))
        val isError = copy.has("error") && !copy.isNull("error")
        return obj("content" to content, "isError" to isError)
    }

    private fun toolDefinitions(): JSONArray {
        val s = JSONObject().put("type", "string")
        val boolSchema = JSONObject().put("type", "boolean")
        return JSONArray().apply {
            put(
                obj(
                    "name" to "phone_ui_status",
                    "description" to "Check on-device Android Accessibility, BackgroundView virtual display (SDK 34+), and foreground app readiness.",
                    "inputSchema" to obj(
                        "type" to "object",
                        "properties" to JSONObject(),
                        "additionalProperties" to false
                    )
                )
            )
            put(
                obj(
                    "name" to "phone_ui_observe",
                    "description" to "Capture Set-of-Marks annotated screen pixels and semantic accessibility elements on this phone (defaults to Android 14+ BackgroundView when ready).",
                    "inputSchema" to obj(
                        "type" to "object",
                        "properties" to obj(
                            "view" to s,
                            "observation_id" to s,
                            "target" to obj("type" to "object"),
                            "query" to obj("type" to "object")
                        ),
                        "additionalProperties" to true
                    )
                )
            )
            put(
                obj(
                    "name" to "phone_ui_act",
                    "description" to "Execute one semantic phone UI action bound to observation_id and return a fresh observation.",
                    "inputSchema" to obj(
                        "type" to "object",
                        "properties" to obj(
                            "observation_id" to s,
                            "action" to obj("type" to "object"),
                            "expect" to obj("type" to "object"),
                            "observe_after" to boolSchema
                        ),
                        "required" to JSONArray().put("observation_id").put("action"),
                        "additionalProperties" to true
                    )
                )
            )
        }
    }

    private fun rpcResult(id: Any, result: JSONObject): JSONObject = obj(
        "jsonrpc" to "2.0",
        "id" to id,
        "result" to result
    )

    private fun rpcError(id: Any, code: Int, message: String): JSONObject = obj(
        "jsonrpc" to "2.0",
        "id" to id,
        "error" to obj("code" to code, "message" to message)
    )
}

/**
 * Zero-UI exported Binder MCP provider allowing Telos Android on the same phone to warm up
 * and call on-device Phone Use capabilities (`phone_ui_status`, `phone_ui_observe`, `phone_ui_act`)
 * with strict package signer and single-UID verification.
 */
class McpBridgeProvider : ContentProvider() {
    override fun onCreate(): Boolean = true

    override fun call(method: String, arg: String?, extras: Bundle?): Bundle {
        val ctx = context ?: throw SecurityException("unavailable")
        verifyCaller(ctx, Binder.getCallingUid())
        val handler = McpRpcHandler(LiveBackend(ctx))
        return when (method) {
            "warmup" -> {
                val status = LiveBackend(ctx).status()
                Bundle().apply {
                    putBoolean("ok", true)
                    putString("health_json", status.toString())
                }
            }

            "mcp_rpc" -> {
                val reqStr = arg ?: extras?.getString("payload")
                    ?: return Bundle().apply {
                        putBoolean("ok", false)
                        putString("error", "missing_mcp_rpc_payload")
                    }
                val response = handler.handleRpc(reqStr)
                Bundle().apply {
                    putBoolean("ok", true)
                    putString("mode", "inline")
                    putString("response_json", response.toString())
                }
            }

            else -> Bundle().apply {
                putBoolean("ok", false)
                putString("error", "unsupported_method:$method")
            }
        }
    }

    private class LiveBackend(private val ctx: Context) : McpRpcHandler.Backend {
        override fun deviceId(): String = currentDeviceId(ctx)

        override fun status(): JSONObject {
            val access = AccessService.instance
            val accessReady = access != null
            val serviceRunning = AgentRuntime.running
            val remoteEnabled = controls(ctx).getBoolean("remote_enabled", false)
            val shellConnected = AgentRuntime.bridge?.connected() == true
            val backgroundAvailable = Build.VERSION.SDK_INT >= 34 && AgentRuntime.bridge?.ui() != null
            val ready = accessReady && serviceRunning && remoteEnabled
            val blockingReason = when {
                !accessReady -> "accessibility_permission_required"
                !serviceRunning || !remoteEnabled -> "remote_control_stopped_locally"
                else -> null
            }
            val fgPkg = access?.foregroundPackage(Display.DEFAULT_DISPLAY)
            return obj(
                "status" to "ok",
                "service" to "octendra-phone-mcp",
                "appVersionName" to BuildConfig.VERSION_NAME,
                "device_id" to deviceId(),
                "accessibility_connected" to accessReady,
                "agent_service_running" to serviceRunning,
                "remote_control_enabled" to remoteEnabled,
                "shell_connected" to shellConnected,
                "background_available" to backgroundAvailable,
                "ready_for_observe" to ready,
                "blocking_reason" to blockingReason,
                "recommended_view" to if (backgroundAvailable) "background" else "active",
                "foreground_package" to fgPkg,
                "active_views" to JSONArray(AiViewHost.shared.describeViews())
            )
        }

        override fun performUi(tool: String, arguments: JSONObject): JSONObject {
            val controller = sharedUiController(ctx)
            val job = obj(
                "id" to UUID.randomUUID().toString(),
                "device_id" to deviceId(),
                "owner" to "local-telos-mcp",
                "tool" to tool,
                "arguments" to arguments
            )
            return controller.perform(job)
        }
    }

    @Suppress("DEPRECATION")
    private fun packageCertsSha256(pm: PackageManager, pkg: String): Set<String> {
        val info = runCatching {
            if (Build.VERSION.SDK_INT >= 33) {
                pm.getPackageInfo(
                    pkg,
                    PackageManager.PackageInfoFlags.of(PackageManager.GET_SIGNING_CERTIFICATES.toLong())
                )
            } else {
                pm.getPackageInfo(pkg, PackageManager.GET_SIGNING_CERTIFICATES)
            }
        }.getOrNull() ?: return emptySet()
        val signers = info.signingInfo?.apkContentsSigners ?: return emptySet()
        return signers.map { sha256(it.toByteArray()).lowercase() }.toSet()
    }

    private fun verifyCaller(ctx: Context, callingUid: Int) {
        val selfUid = Process.myUid()
        if (callingUid == selfUid || callingUid == 2000 || callingUid == 0) return
        val pm = ctx.packageManager
        val packages = pm.getPackagesForUid(callingUid)?.toList().orEmpty()
        val ownCerts = packageCertsSha256(pm, ctx.packageName)
        val configuredPeer = runCatching { controls(ctx).getString("mcp_peer_sha256", null) }.getOrNull()
        val callerCerts = packages.map { it to packageCertsSha256(pm, it) }
        if (!McpCallerPolicy.isAllowed(callingUid, selfUid, ownCerts, configuredPeer, callerCerts)) {
            throw SecurityException("unauthorized_mcp_caller")
        }
    }

    override fun query(
        uri: Uri,
        projection: Array<out String>?,
        selection: String?,
        selectionArgs: Array<out String>?,
        sortOrder: String?
    ): Cursor = throw SecurityException("unsupported_operation")

    override fun getType(uri: Uri): String = "vnd.android.cursor.item/vnd.fun.hackerlife.octendra.mcp"

    override fun insert(uri: Uri, values: ContentValues?): Uri = throw SecurityException("read_only")

    override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?): Int =
        throw SecurityException("read_only")

    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?): Int =
        throw SecurityException("read_only")

    companion object {
        private val controllerLock = Any()

        @Volatile
        private var cachedController: Pair<String, UiController>? = null

        private fun currentDeviceId(ctx: Context): String {
            return runCatching { ConfigStore(ctx.applicationContext).read()?.deviceId }
                .getOrNull()
                ?.takeIf { it.isNotBlank() }
                ?: "local-phone"
        }

        private fun sharedUiController(ctx: Context): UiController = synchronized(controllerLock) {
            val appCtx = ctx.applicationContext
            val devId = currentDeviceId(appCtx)
            val existing = cachedController
            if (existing != null && existing.first == devId) {
                return existing.second
            }
            existing?.second?.let { runCatching { it.close() } }
            val cfg = GatewayConfig(
                gatewayUrl = "https://local.invalid",
                deviceId = devId,
                deviceToken = "",
                name = "local-phone"
            )
            val created = UiController(appCtx, cfg)
            cachedController = devId to created
            created
        }
    }
}
