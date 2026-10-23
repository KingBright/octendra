package `fun`.hackerlife.octendra

import org.junit.Assert.*
import org.junit.Test
import javax.crypto.KeyGenerator
import javax.crypto.Cipher
import java.nio.ByteBuffer

class ConfigCipherTest {
    private fun key() = KeyGenerator.getInstance("AES").apply { init(128) }.generateKey()
    private val associatedData = "fun.hackerlife.octendra".toByteArray()
    private val plain = "existing-device-config-synthetic-fixture".toByteArray()

    @Test fun readsTheExistingV1EnvelopeWithOriginalKeyAfterBrandUpgrade() {
        val original = key()
        // Independently produce the exact format written by the installed 0.11.1.
        val old = Cipher.getInstance("AES/GCM/NoPadding")
        old.init(Cipher.ENCRYPT_MODE, original); old.updateAAD(associatedData)
        val encrypted = old.doFinal(plain)
        val envelope = ByteBuffer.allocate(1 + old.iv.size + encrypted.size)
            .put(1.toByte()).put(old.iv).put(encrypted).array()
        val before = envelope.clone()
        assertArrayEquals(plain, ConfigCipher.decrypt(envelope, associatedData, listOf(original)))
        assertArrayEquals(before, envelope)
        assertEquals(listOf("octendra-gateway-v1", "remote-hosts-gateway-v1"), ConfigCipher.READ_ALIASES)
    }

    @Test fun aWrongNewAliasCannotHideTheValidOriginalAlias() {
        val original = key(); val unrelatedNewKey = key()
        val envelope = ConfigCipher.encrypt(plain, associatedData, original)
        assertArrayEquals(plain, ConfigCipher.decrypt(envelope, associatedData, listOf(unrelatedNewKey, original)))
    }

    @Test fun roundTripKeepsPackageBindingAndRejectsUnknownKeysOrTampering() {
        val original = key(); val envelope = ConfigCipher.encrypt(plain, associatedData, original)
        assertArrayEquals(plain, ConfigCipher.decrypt(envelope, associatedData, listOf(original)))
        for ((bytes, aad, keys) in listOf(
            Triple(envelope, associatedData, emptyList()),
            Triple(envelope, "another.package".toByteArray(), listOf(original)),
            Triple(envelope, associatedData, listOf(key())),
            Triple(envelope.clone().apply { this[lastIndex] = (this[lastIndex].toInt() xor 1).toByte() }, associatedData, listOf(original))
        )) {
            try { ConfigCipher.decrypt(bytes, aad, keys); fail("must reject") }
            catch (_: java.security.GeneralSecurityException) { }
        }
    }

    @Test fun invalidEnvelopeFailsWithoutLookingForReplacementCredentials() {
        for (bytes in listOf(byteArrayOf(), ByteArray(40).apply { this[0] = 2 })) {
            try { ConfigCipher.decrypt(bytes, associatedData, listOf(key())); fail("must reject") }
            catch (_: IllegalArgumentException) { }
        }
    }
}
