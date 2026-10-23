package `fun`.hackerlife.octendra

import java.nio.ByteBuffer
import javax.crypto.Cipher
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** The v1 envelope predates the brand name. Reading never creates or replaces a key. */
object ConfigCipher {
    const val KEY_ALIAS = "octendra-gateway-v1"
    val READ_ALIASES = listOf(KEY_ALIAS, "remote-hosts-gateway-v1")

    fun decrypt(bytes: ByteArray, associatedData: ByteArray, keys: List<SecretKey>): ByteArray {
        require(bytes.size in 30..8192 && bytes[0] == 1.toByte()) { "invalid_config_envelope" }
        for (key in keys) {
            try {
                val cipher = Cipher.getInstance("AES/GCM/NoPadding")
                cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(128, bytes.copyOfRange(1, 13)))
                cipher.updateAAD(associatedData)
                return cipher.doFinal(bytes.copyOfRange(13, bytes.size))
            } catch (_: java.security.GeneralSecurityException) { /* Try another existing upgrade key. */ }
        }
        throw java.security.GeneralSecurityException("config_key_unavailable_or_invalid")
    }

    fun encrypt(plain: ByteArray, associatedData: ByteArray, key: SecretKey): ByteArray {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, key)
        cipher.updateAAD(associatedData)
        val encrypted = cipher.doFinal(plain)
        return ByteBuffer.allocate(1 + cipher.iv.size + encrypted.size)
            .put(1.toByte()).put(cipher.iv).put(encrypted).array()
    }
}
