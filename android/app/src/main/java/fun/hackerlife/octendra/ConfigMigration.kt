package `fun`.hackerlife.octendra

import org.json.JSONObject

/** An explicit, locally confirmed gateway move keeps the existing device identity and data. */
object ConfigMigration {
    fun regularSave(current: GatewayConfig?, candidate: GatewayConfig): GatewayConfig {
        val clean = candidate.copy(storageIdentity = null)
        return if (current != null && current.deviceId == candidate.deviceId && current.gatewayUrl == candidate.gatewayUrl)
            clean.copy(storageIdentity = current.storageIdentity) else clean
    }

    fun requested(current: GatewayConfig?, imported: JSONObject): GatewayConfig? {
        if (!imported.optBoolean("migrate_existing_connection", false)) return null
        demand(current != null, "migration_source_missing")
        val source = current!!
        demand(Policy.uuid(imported.getString("device_id")) == source.deviceId, "migration_device_identity_changed")
        demand(Policy.origin(imported.getString("source_gateway_url")) == source.gatewayUrl, "migration_source_changed")
        demand(!imported.has("device_token") || imported.getString("device_token") == source.deviceToken, "migration_device_token_changed")
        val target = Policy.origin(imported.getString("gateway_url"))
        demand(target != source.gatewayUrl, "migration_destination_unchanged")
        return source.copy(gatewayUrl = target, name = imported.optString("name", source.name).take(80), storageIdentity = source.identity())
    }

    fun approved(current: GatewayConfig?, expectedSource: GatewayConfig, target: GatewayConfig): GatewayConfig {
        demand(current == expectedSource, "migration_source_changed")
        demand(target.deviceId == expectedSource.deviceId && target.deviceToken == expectedSource.deviceToken &&
            target.storageIdentity == expectedSource.identity() && target.gatewayUrl != expectedSource.gatewayUrl,
            "migration_binding_mismatch")
        return target
    }
}
