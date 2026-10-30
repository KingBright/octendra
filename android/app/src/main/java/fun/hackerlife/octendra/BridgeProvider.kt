package `fun`.hackerlife.octendra

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.os.Binder
import android.os.Process
import android.os.ParcelFileDescriptor
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicBoolean
import org.json.JSONObject

/** Shell-only activation metadata and one authenticated, unnamed duplex FD. */
class BridgeProvider : ContentProvider() {
    private val provisioner = Executors.newSingleThreadExecutor()
    private val provisioning = AtomicBoolean(false)
    override fun onCreate() = true
    override fun query(uri: Uri, projection: Array<out String>?, selection: String?, selectionArgs: Array<out String>?, sortOrder: String?): Cursor {
        val caller = Binder.getCallingUid()
        if (caller != 2000 && caller != 0 && caller != Process.myUid()) throw SecurityException("shell_or_owner_uid_required")
        val ctx = context ?: throw SecurityException("unavailable")
        if (uri.path == "/provision/status") {
            val prefs = controls(ctx)
            return MatrixCursor(arrayOf("state", "request_sha256", "error_code", "start", "running")).apply {
                addRow(arrayOf<Any?>(prefs.getString("provision_state", "not_started"), prefs.getString("provision_request", ""),
                    prefs.getString("provision_error", ""), prefs.getString("provision_start", ""), if (AgentRuntime.running) 1 else 0))
            }
        }
        if (uri.path != "/status" || !controls(ctx).getBoolean("shell_enabled", false)) throw SecurityException("local_shell_consent_required")
        val bridge = AgentRuntime.bridge ?: throw SecurityException("start_agent_first")
        val name = bridge.socketName ?: throw SecurityException("bridge_not_listening")
        return MatrixCursor(arrayOf("socket", "uid", "connected", "apk")).apply {
            addRow(arrayOf(name, Process.myUid(), if (bridge.connected()) 1 else 0, ctx.applicationInfo.sourceDir))
        }
    }
    override fun openFile(uri: Uri, mode: String): ParcelFileDescriptor {
        val caller = Binder.getCallingUid()
        if (caller != 2000 && caller != 0) throw SecurityException("shell_uid_required")
        if (uri.path == "/provision" && mode == "w") {
            val ctx = context ?: throw SecurityException("unavailable")
            if (!provisioning.compareAndSet(false, true)) throw SecurityException("provisioning_in_progress")
            try {
            val pipe = ParcelFileDescriptor.createReliablePipe()
            controls(ctx).edit().putString("provision_state", "receiving").putString("provision_request", "").putString("provision_error", "").commit()
            provisioner.submit {
                try {
                    val bytes = ParcelFileDescriptor.AutoCloseInputStream(pipe[0]).use { Transport.bounded(it, 8192) }
                    controls(ctx).edit().putString("provision_request", sha256(bytes)).putString("provision_state", "applying").commit()
                    ManagedProvisioning.apply(ctx, JSONObject(String(bytes, Charsets.UTF_8)))
                    controls(ctx).edit().putString("provision_state", "configured").commit()
                } catch (error: Exception) {
                    controls(ctx).edit().putString("provision_state", "failed")
                        .putString("provision_error", Policy.error(error).optString("error", "configuration_failed")).commit()
                } finally { provisioning.set(false) }
            }
            return pipe[1]
            } catch (error: Exception) { provisioning.set(false); throw error }
        }
        val parts = uri.pathSegments
        if (mode != "rw" || parts.size != 2 || parts[0] != "connect") throw SecurityException("invalid_bridge_endpoint")
        val bridge = AgentRuntime.bridge ?: throw SecurityException("start_agent_first")
        return bridge.attach(parts[1], caller)
    }
    override fun getType(uri: Uri) = "vnd.android.cursor.item/vnd.fun.hackerlife.octendra.bridge"
    override fun insert(uri: Uri, values: ContentValues?): Uri? = throw SecurityException("read_only")
    override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?) = throw SecurityException("read_only")
    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?) = throw SecurityException("read_only")
}
