package `fun`.hackerlife.octendra

object ServiceStartPolicy {
    fun resumeVisibleActivity(remoteEnabled: Boolean, running: Boolean): Boolean = remoteEnabled && !running

    fun restore(action: String?, remoteEnabled: Boolean, startAtBoot: Boolean): Boolean =
        remoteEnabled && (action == "android.intent.action.MY_PACKAGE_REPLACED" ||
            (action == "android.intent.action.BOOT_COMPLETED" && startAtBoot))
}
