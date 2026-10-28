package `fun`.hackerlife.octendra

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class ConfigMigrationTest {
    private val old = GatewayConfig("https://old.example", "11111111-1111-4111-8111-111111111111", "a".repeat(64), "Phone")
    private fun request() = JSONObject().put("migrate_existing_connection", true)
        .put("source_gateway_url", old.gatewayUrl).put("gateway_url", "https://new.example").put("device_id", old.deviceId)
    private fun rejected(block: () -> Unit) { try { block(); fail("must reject") } catch (_: AgentError) { } }

    @Test fun migrationKeepsExactStorageAndDeviceCredentialWithoutExportingToken() {
        val input = request()
        assertFalse(input.has("device_token"))
        val target = ConfigMigration.requested(old, input)!!
        assertEquals("https://new.example", target.gatewayUrl)
        assertEquals(old.deviceId, target.deviceId)
        assertEquals(old.deviceToken, target.deviceToken)
        assertEquals(old.identity(), target.identity())
        assertEquals(old.identity().take(20), target.identity().take(20))
        assertEquals(old.identity().take(24), target.identity().take(24))
        assertEquals(target, ConfigMigration.approved(old, old, target))
    }
    @Test fun encryptedRoundTripAndNormalSaveKeepApprovedNamespace() {
        val target = ConfigMigration.requested(old, request())!!
        val restored = GatewayConfig.parse(target.json(), encryptedLocalConfig = true)
        assertEquals(target, restored)
        assertEquals(target.identity(), ConfigMigration.regularSave(restored, target.copy(storageIdentity = null)).identity())
        val next = ConfigMigration.requested(restored, request().put("source_gateway_url", restored.gatewayUrl).put("gateway_url", "https://next.example"))!!
        assertEquals(old.identity(), next.identity())
    }
    @Test fun ordinaryImportsCannotChooseExistingPrivateNamespace() {
        val target = ConfigMigration.requested(old, request())!!
        val imported = GatewayConfig.parse(target.json())
        assertNull(imported.storageIdentity)
        assertNotEquals(old.identity(), ConfigMigration.regularSave(old, imported).identity())
        assertNull(ConfigMigration.requested(old, target.json()))
        val tampered = target.copy(deviceId = "22222222-2222-4222-8222-222222222222")
        assertNull(ConfigMigration.regularSave(old, tampered).storageIdentity)
    }
    @Test fun sourceDeviceCredentialAndApprovalMustStillMatch() {
        rejected { ConfigMigration.requested(null, request()) }
        rejected { ConfigMigration.requested(old, request().put("device_id", "22222222-2222-4222-8222-222222222222")) }
        rejected { ConfigMigration.requested(old, request().put("source_gateway_url", "https://other.example")) }
        rejected { ConfigMigration.requested(old, request().put("device_token", "b".repeat(64))) }
        rejected { ConfigMigration.requested(old, request().put("gateway_url", old.gatewayUrl)) }
        val target = ConfigMigration.requested(old, request())!!
        rejected { ConfigMigration.approved(old.copy(name = "Changed"), old, target) }
        rejected { ConfigMigration.approved(old, old, target.copy(storageIdentity = "b".repeat(64))) }
        rejected { ConfigMigration.approved(old, old, target.copy(deviceToken = "b".repeat(64))) }
    }
    @Test fun privateStorageIdentifierMustBeACompleteHash() {
        rejected { GatewayConfig.parse(old.json().put("storage_identity", "../../outside"), encryptedLocalConfig = true) }
        rejected { GatewayConfig.parse(old.json().put("storage_identity", "a".repeat(63)), encryptedLocalConfig = true) }
    }
}
