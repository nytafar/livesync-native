package no.nyta.livesync.secret

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.security.SecureRandom
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** The same AES-GCM sealing as the Keystore cipher, with software keys. */
private class SoftCipher : SecretCipher {
    val keys = mutableMapOf<String, SecretKey>()
    private fun key(alias: String) = keys.getOrPut(alias) { KeyGenerator.getInstance("AES").apply { init(256) }.generateKey() }
    override fun encrypt(alias: String, plain: ByteArray): Pair<ByteArray, ByteArray> {
        val iv = ByteArray(12).also { SecureRandom().nextBytes(it) }
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.ENCRYPT_MODE, key(alias), GCMParameterSpec(128, iv))
        return iv to c.doFinal(plain)
    }
    override fun decrypt(alias: String, iv: ByteArray, sealed: ByteArray): ByteArray {
        val c = Cipher.getInstance("AES/GCM/NoPadding")
        c.init(Cipher.DECRYPT_MODE, key(alias), GCMParameterSpec(128, iv))
        return c.doFinal(sealed)
    }
    override fun deleteKey(alias: String) {
        keys.remove(alias)
    }
}

class SecretStoreTest {
    @get:Rule
    val tmp = TemporaryFolder()

    @Test
    fun secretsAreSealedPerProfileAndForgotten() {
        val cipher = SoftCipher()
        val s = SecretStore(tmp.root, cipher)
        s.put("p1", SecretStore.PASSWORD, "hunter2-æøå")
        s.put("p2", SecretStore.PASSWORD, "other")
        assertEquals("hunter2-æøå", s.get("p1", SecretStore.PASSWORD))
        assertEquals("other", s.get("p2", SecretStore.PASSWORD))
        val raw = tmp.root.listFiles()!!.filter { it.name.startsWith("p1.") }.single().readBytes()
        assertFalse("no plaintext on disk", String(raw, Charsets.ISO_8859_1).contains("hunter2"))
        assertTrue(cipher.keys.containsKey("livesync-profile-p1"))
        s.removeProfile("p1")
        assertNull(s.get("p1", SecretStore.PASSWORD))
        assertFalse(cipher.keys.containsKey("livesync-profile-p1"))
        assertEquals("other", s.get("p2", SecretStore.PASSWORD))
    }

    @Test
    fun aLostKeyOrCorruptFileGivesNullNotACrash() {
        val cipher = SoftCipher()
        val s = SecretStore(tmp.root, cipher)
        s.put("p1", SecretStore.PASSWORD, "pw")
        cipher.keys.clear() // as after a Keystore reset
        assertNull(s.get("p1", SecretStore.PASSWORD))
        tmp.root.listFiles()!!.single().writeBytes(byteArrayOf(1))
        assertNull(s.get("p1", SecretStore.PASSWORD))
    }

    @Test(expected = IllegalArgumentException::class)
    fun profileIdsCannotEscapeTheFolder() {
        SecretStore(tmp.root, SoftCipher()).put("../x", SecretStore.PASSWORD, "pw")
    }
}
