package no.nyta.livesync.secret

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.io.IOException
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** Encrypts and decrypts under a named key. */
interface SecretCipher {
    /** Returns the IV and the ciphertext. */
    fun encrypt(alias: String, plain: ByteArray): Pair<ByteArray, ByteArray>
    fun decrypt(alias: String, iv: ByteArray, sealed: ByteArray): ByteArray
    fun deleteKey(alias: String)
}

/**
 * AES-256-GCM keys in the Android Keystore, one per profile. The key never
 * leaves the Keystore; it is usable after the first unlock since boot, so the
 * service can start from BOOT_COMPLETED.
 */
class KeystoreCipher : SecretCipher {
    private val keyStore: KeyStore = KeyStore.getInstance(PROVIDER).apply { load(null) }

    private fun key(alias: String): SecretKey {
        (keyStore.getEntry(alias, null) as? KeyStore.SecretKeyEntry)?.let { return it.secretKey }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, PROVIDER)
        gen.init(
            KeyGenParameterSpec.Builder(alias, KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return gen.generateKey()
    }

    override fun encrypt(alias: String, plain: ByteArray): Pair<ByteArray, ByteArray> {
        val c = Cipher.getInstance(TRANSFORMATION)
        c.init(Cipher.ENCRYPT_MODE, key(alias))
        return c.iv to c.doFinal(plain)
    }

    override fun decrypt(alias: String, iv: ByteArray, sealed: ByteArray): ByteArray {
        val c = Cipher.getInstance(TRANSFORMATION)
        c.init(Cipher.DECRYPT_MODE, key(alias), GCMParameterSpec(128, iv))
        return c.doFinal(sealed)
    }

    override fun deleteKey(alias: String) {
        if (keyStore.containsAlias(alias)) keyStore.deleteEntry(alias)
    }

    companion object {
        const val PROVIDER = "AndroidKeyStore"
        const val TRANSFORMATION = "AES/GCM/NoPadding"
    }
}

/**
 * Per-profile secrets (the CouchDB password; later the E2EE passphrase),
 * sealed under a Keystore key whose alias derives from the profile ID
 * (spec "Credentials are stored per profile"). The file holds only
 * `version | iv length | iv | ciphertext`.
 */
class SecretStore(private val dir: File, private val cipher: SecretCipher) {
    fun alias(profileId: String) = "livesync-profile-$profileId"

    private fun file(profileId: String, name: String): File {
        require(profileId.matches(Regex("[A-Za-z0-9_-]+"))) { "bad profile id" }
        return File(dir, "$profileId.$name.bin")
    }

    fun put(profileId: String, name: String, value: String) {
        dir.mkdirs()
        val (iv, sealed) = cipher.encrypt(alias(profileId), value.toByteArray(Charsets.UTF_8))
        val out = ByteArray(2 + iv.size + sealed.size)
        out[0] = VERSION
        out[1] = iv.size.toByte()
        iv.copyInto(out, 2)
        sealed.copyInto(out, 2 + iv.size)
        val f = file(profileId, name)
        val tmp = File(dir, f.name + ".tmp")
        tmp.writeBytes(out)
        if (!tmp.renameTo(f)) throw IOException("cannot store the secret")
    }

    /** The secret, or null when none is stored or it can no longer be opened. */
    fun get(profileId: String, name: String): String? {
        val f = file(profileId, name)
        if (!f.exists()) return null
        val b = f.readBytes()
        if (b.size < 3 || b[0] != VERSION) return null
        val ivLen = b[1].toInt()
        if (b.size < 2 + ivLen) return null
        val iv = b.copyOfRange(2, 2 + ivLen)
        val sealed = b.copyOfRange(2 + ivLen, b.size)
        return try {
            String(cipher.decrypt(alias(profileId), iv, sealed), Charsets.UTF_8)
        } catch (e: Exception) {
            null
        }
    }

    /** Forget every secret of a profile and its key. */
    fun removeProfile(profileId: String) {
        dir.listFiles()?.filter { it.name.startsWith("$profileId.") }?.forEach { it.delete() }
        cipher.deleteKey(alias(profileId))
    }

    companion object {
        const val PASSWORD = "couchdb-password"
        private const val VERSION: Byte = 1
    }
}
