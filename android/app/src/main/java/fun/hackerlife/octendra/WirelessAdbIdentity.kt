package `fun`.hackerlife.octendra

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.math.BigInteger
import java.security.KeyPairGenerator
import java.security.KeyStore
import java.security.PrivateKey
import java.util.Date
import javax.security.auth.x500.X500Principal

/** The pairing identity stays in AndroidKeyStore across app and device restarts. */
class WirelessAdbIdentity {
    private val store = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
    init {
        if (!store.containsAlias(ALIAS)) {
            KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_RSA, "AndroidKeyStore").apply {
                initialize(KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_SIGN or KeyProperties.PURPOSE_VERIFY)
                    .setKeySize(2048)
                    // Conscrypt delegates TLS RSA signatures through a private-key Cipher,
                    // including raw/no-padding RSA for PSS; Android KeyStore maps that to SIGN/NONE.
                    .setDigests(KeyProperties.DIGEST_NONE, KeyProperties.DIGEST_SHA256, KeyProperties.DIGEST_SHA512)
                    .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                    .setSignaturePaddings(KeyProperties.SIGNATURE_PADDING_RSA_PKCS1, KeyProperties.SIGNATURE_PADDING_RSA_PSS)
                    .setCertificateSubject(X500Principal("CN=Octendra Wireless ADB"))
                    .setCertificateSerialNumber(BigInteger.ONE)
                    .setCertificateNotBefore(Date(0))
                    .setCertificateNotAfter(Date(4102444800000L))
                    .build())
            }.generateKeyPair()
        }
    }
    val privateKey: PrivateKey get() = store.getKey(ALIAS, null) as PrivateKey
    val certificate: java.security.cert.Certificate get() = store.getCertificate(ALIAS)

    companion object { private const val ALIAS = "octendra.wireless-adb.rsa.tls" }
}
