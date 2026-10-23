package `fun`.hackerlife.octendra

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val preferences = controls(context)
        if (ServiceStartPolicy.restore(intent.action, preferences.getBoolean("remote_enabled", false),
                preferences.getBoolean("start_at_boot", false))) {
            // A platform may reject background startup. Never attempt a hidden bypass.
            runCatching { context.startForegroundService(Intent(context, AgentService::class.java)) }
        }
    }
}
