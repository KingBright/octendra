package `fun`.hackerlife.octendra

import android.content.Context
import android.content.Intent
import org.json.JSONObject

/** Configuration supplied by a user-selected file or an OS-authorized USB host. */
object ManagedProvisioning {
    @Synchronized fun apply(context: Context, imported: JSONObject): GatewayConfig {
        val store = ConfigStore(context)
        val source = store.read()
        val migration = ConfigMigration.requested(source, imported)
        val candidate = migration ?: ConfigMigration.regularSave(source, GatewayConfig.parse(imported))
        // Keep ongoing work intact. The installer can observe and retry after it finishes.
        demand(!AgentRuntime.running || source == candidate, "connection_running_configuration_changed")
        if (migration != null) store.migrate(migration, source!!) else store.write(candidate)
        val prefs = controls(context)
        prefs.edit()
            .putBoolean("remote_enabled", imported.optBoolean("connect", true))
            .putBoolean("start_at_boot", imported.optBoolean("start_at_boot", if (source == null) true else prefs.getBoolean("start_at_boot", true)))
            .putBoolean("shell_enabled", imported.optBoolean("allow_exec", if (source == null) true else prefs.getBoolean("shell_enabled", true)))
            .commit()
        if (controls(context).getBoolean("remote_enabled", false)) {
            try {
                context.startForegroundService(Intent(context, AgentService::class.java))
            } catch (_: RuntimeException) {
                // Preserve the configured connection. A normal activity/boot/update can resume it.
                controls(context).edit().putString("provision_start", "platform_foreground_required").commit()
                AgentRuntime.changed()
                return candidate
            }
        } else {
            context.stopService(Intent(context, AgentService::class.java))
        }
        controls(context).edit().putString("provision_start", "requested").commit()
        AgentRuntime.changed()
        return candidate
    }
}
