package `fun`.hackerlife.octendra

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.AtomicFile
import java.io.File
import java.security.KeyStore
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import org.json.JSONObject

class ConfigStore(private val context: Context) {
    private val file = AtomicFile(File(context.noBackupFilesDir, "gateway-config.enc"))
    private fun keyStore() = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    private fun existingKeys(ks: KeyStore): List<SecretKey> =
        ConfigCipher.READ_ALIASES.mapNotNull { ks.getKey(it, null) as? SecretKey }
    private fun writeKey(): SecretKey {
        existingKeys(keyStore()).firstOrNull()?.let { return it }
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore").apply {
            init(KeyGenParameterSpec.Builder(ConfigCipher.KEY_ALIAS, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM).setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256).build())
        }.generateKey()
    }
    @Synchronized fun read(): GatewayConfig? {
        if (!file.baseFile.exists()) return null
        try {
            val plain = ConfigCipher.decrypt(file.readFully(), context.packageName.toByteArray(), existingKeys(keyStore()))
            return GatewayConfig.parse(JSONObject(String(plain, Charsets.UTF_8)), encryptedLocalConfig = true)
        } catch (_: Exception) { throw AgentError("config_unreadable", "配置无法解密，请重新导入设备配置。") }
    }
    @Synchronized fun write(config: GatewayConfig) {
        writeEncrypted(ConfigMigration.regularSave(read(), config))
    }
    @Synchronized fun migrate(config: GatewayConfig, expectedSource: GatewayConfig) {
        writeEncrypted(ConfigMigration.approved(read(), expectedSource, config))
    }
    private fun writeEncrypted(config: GatewayConfig) {
        val bytes = ConfigCipher.encrypt(config.json().toString().toByteArray(), context.packageName.toByteArray(), writeKey())
        val stream = file.startWrite()
        try { stream.write(bytes); file.finishWrite(stream) } catch (e: Exception) { file.failWrite(stream); throw e }
    }
    fun erase() { file.delete() }
}
fun controls(context: Context) = context.getSharedPreferences("local-controls", Context.MODE_PRIVATE)
fun exposedRoot(context: Context, config: GatewayConfig): File = File(context.filesDir, "shared/${config.identity().take(20)}").apply { mkdirs() }.canonicalFile
