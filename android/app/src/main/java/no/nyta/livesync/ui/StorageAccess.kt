package no.nyta.livesync.ui

import android.Manifest

/**
 * How the app reaches the vault folder in shared storage, by Android release.
 *
 * - Android 11 and later (API 30+): all-files access (MANAGE_EXTERNAL_STORAGE,
 *   spec D3), granted in Settings.
 * - Android 9 and 10: the legacy READ/WRITE_EXTERNAL_STORAGE runtime grant. The
 *   manifest caps both at maxSdkVersion 29, so Android 11+ never sees them. On
 *   Android 10 they reach all of shared storage only in the legacy storage
 *   model (requestLegacyExternalStorage).
 */
enum class StorageAccess(val label: String) {
    ALL_FILES("All-files access"),
    LEGACY("Storage access"),
    ;

    /** The label inside a sentence ("Grant all-files access first."). */
    val phrase: String get() = label.lowercase()

    companion object {
        /** The first API level with all-files access; the legacy permissions stop one below. */
        const val ALL_FILES_SDK = 30

        fun forSdk(sdk: Int): StorageAccess = if (sdk >= ALL_FILES_SDK) ALL_FILES else LEGACY

        val LEGACY_PERMISSIONS = arrayOf(
            Manifest.permission.READ_EXTERNAL_STORAGE,
            Manifest.permission.WRITE_EXTERNAL_STORAGE,
        )
    }
}
