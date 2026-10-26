package no.nyta.livesync

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.os.Build
import android.os.Environment
import no.nyta.livesync.core.LiveSync
import no.nyta.livesync.core.VaultAccess
import no.nyta.livesync.profile.ProfileStore
import no.nyta.livesync.secret.KeystoreCipher
import no.nyta.livesync.secret.SecretStore
import java.io.File

class LiveSyncApp : Application() {
    val profiles: ProfileStore by lazy {
        ProfileStore(File(filesDir, "profiles.json"), File(filesDir, "state"))
    }
    val secrets: SecretStore by lazy { SecretStore(File(filesDir, "secrets"), KeystoreCipher()) }

    /** The native core, or the reason it could not be loaded. */
    val core: Result<Core> by lazy {
        runCatching {
            // Load the library through the platform loader first, so JNA's
            // lookup by name finds it already mapped.
            System.loadLibrary("livesync_ffi")
            val host = LiveSync(File(filesDir, "logs").path)
            // Without all-files access the vault looks empty; never let that
            // read as "everything was deleted".
            host.setVaultAccess(object : VaultAccess {
                override fun problem(vaultPath: String): String? = try {
                    if (Environment.isExternalStorageManager()) null else "all-files access is not granted"
                } catch (e: Exception) {
                    "cannot check all-files access: ${e.message}"
                }
            })
            Core(host, profiles, secrets, File(cacheDir, "connect-test"))
        }
    }

    override fun onCreate() {
        super.onCreate()
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_STATUS, getString(R.string.channel_status), NotificationManager.IMPORTANCE_LOW)
                .apply {
                    description = getString(R.string.channel_status_description)
                    setShowBadge(false)
                }
        )
    }

    companion object {
        const val CHANNEL_STATUS = "status"

        fun deviceNameDefault(): String = "${Build.MANUFACTURER} ${Build.MODEL}".trim()
    }
}
