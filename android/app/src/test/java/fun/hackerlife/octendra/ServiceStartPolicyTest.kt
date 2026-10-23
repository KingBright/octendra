package `fun`.hackerlife.octendra

import org.junit.Assert.*
import org.junit.Test

class ServiceStartPolicyTest {
    private val replacement = "android.intent.action.MY_PACKAGE_REPLACED"
    private val boot = "android.intent.action.BOOT_COMPLETED"

    @Test fun replacingAnEnabledAppRestoresItsExistingConnectionWithoutBootOptIn() {
        assertTrue(ServiceStartPolicy.restore(replacement, remoteEnabled = true, startAtBoot = false))
        assertTrue(ServiceStartPolicy.restore(replacement, remoteEnabled = true, startAtBoot = true))
    }
    @Test fun aUserStoppedSessionNeverRestartsOnUpgradeOrBoot() {
        for (action in listOf(replacement, boot)) for (bootOptIn in listOf(false, true))
            assertFalse(ServiceStartPolicy.restore(action, remoteEnabled = false, startAtBoot = bootOptIn))
    }
    @Test fun bootStillRequiresExplicitExistingOptInAndUnrelatedEventsDoNothing() {
        assertFalse(ServiceStartPolicy.restore(boot, true, false))
        assertTrue(ServiceStartPolicy.restore(boot, true, true))
        assertFalse(ServiceStartPolicy.restore(null, true, true))
        assertFalse(ServiceStartPolicy.restore("android.intent.action.SCREEN_ON", true, true))
    }
}
