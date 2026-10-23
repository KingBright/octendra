package `fun`.hackerlife.octendra

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log

class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val preferences = controls(context)
        val remoteEnabled = preferences.getBoolean("remote_enabled", false)
        val allowed = ServiceStartPolicy.restore(intent.action, remoteEnabled, preferences.getBoolean("start_at_boot", false))
        if (intent.action in listOf(Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED))
            Log.i("OctendraConnection", "channel=lifecycle event=restore_decision allowed=$allowed remote_enabled=$remoteEnabled")
        if (allowed) {
            // A platform may reject background startup. Never attempt a hidden bypass.
            try { context.startForegroundService(Intent(context, AgentService::class.java)) }
            catch (error: Exception) {
                Log.w("OctendraConnection", "channel=lifecycle event=restore_failed code=${ConnectionDiagnostics.errorCode(error)}")
            }
        }
    }
}
