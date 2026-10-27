package no.nyta.livesync.ui

import android.Manifest
import android.annotation.SuppressLint
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Environment
import android.os.PowerManager
import android.provider.Settings
import androidx.annotation.RequiresApi
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import androidx.core.net.toUri

/** The Android permissions and settings the app depends on (spec D3, risk 6). */
object Perms {
    /** Whether the active network costs per byte (mobile data, a metered hotspot); true when unknown. */
    fun metered(ctx: Context): Boolean =
        ctx.getSystemService(android.net.ConnectivityManager::class.java)?.isActiveNetworkMetered ?: true

    /** How this Android release grants the vault folder ([StorageAccess]). */
    fun storageAccess(): StorageAccess = StorageAccess.forSdk(Build.VERSION.SDK_INT)

    /**
     * Whether the app can read and write all of shared storage: all-files
     * access on Android 11+, the legacy storage permissions below it (on
     * Android 10 only in the legacy storage model).
     */
    fun allFilesAccess(ctx: Context): Boolean = when {
        Build.VERSION.SDK_INT >= StorageAccess.ALL_FILES_SDK -> Environment.isExternalStorageManager()
        Build.VERSION.SDK_INT >= 29 -> legacyStorageGranted(ctx) && Environment.isExternalStorageLegacy()
        else -> legacyStorageGranted(ctx)
    }

    private fun legacyStorageGranted(ctx: Context): Boolean = StorageAccess.LEGACY_PERMISSIONS.all {
        ContextCompat.checkSelfPermission(ctx, it) == PackageManager.PERMISSION_GRANTED
    }

    @RequiresApi(30)
    fun allFilesIntent(ctx: Context): Intent =
        Intent(Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION, "package:${ctx.packageName}".toUri())

    @RequiresApi(30)
    fun allFilesFallbackIntent(): Intent = Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION)

    fun batteryExempt(ctx: Context): Boolean =
        ctx.getSystemService(PowerManager::class.java).isIgnoringBatteryOptimizations(ctx.packageName)

    /** Sideloaded always-on sync is the case this intent exists for. */
    @SuppressLint("BatteryLife")
    fun batteryIntent(ctx: Context): Intent =
        Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS, "package:${ctx.packageName}".toUri())

    fun notificationsAllowed(ctx: Context): Boolean =
        if (Build.VERSION.SDK_INT >= 33) {
            ContextCompat.checkSelfPermission(ctx, Manifest.permission.POST_NOTIFICATIONS) ==
                PackageManager.PERMISSION_GRANTED
        } else {
            NotificationManagerCompat.from(ctx).areNotificationsEnabled()
        }

    fun isSamsung(): Boolean = Build.MANUFACTURER.equals("samsung", ignoreCase = true)

    /**
     * Whether to show the Boox steps: an Onyx Boox e-ink device, which freezes
     * background apps on its own, on Android 9 or 10 (this branch's target).
     * Android 11+ keeps the setup screen it had, Boox or not.
     */
    fun booxChecks(
        sdk: Int = Build.VERSION.SDK_INT,
        manufacturer: String? = Build.MANUFACTURER,
        brand: String? = Build.BRAND,
    ): Boolean = sdk < StorageAccess.ALL_FILES_SDK &&
        (manufacturer.equals("onyx", ignoreCase = true) || brand.equals("onyx", ignoreCase = true))

    fun appDetailsIntent(ctx: Context): Intent =
        Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, "package:${ctx.packageName}".toUri())

    /** One UI's battery screen (Device care); not a public API, so callers fall back. */
    fun samsungBatteryIntent(): Intent =
        Intent().setComponent(
            ComponentName("com.samsung.android.lool", "com.samsung.android.sm.battery.ui.BatteryActivity")
        )

    fun dontKillMyAppIntent(): Intent =
        Intent(Intent.ACTION_VIEW, "https://dontkillmyapp.com/samsung".toUri())

    /** Start the first intent that resolves. */
    fun launch(ctx: Context, vararg intents: Intent): Boolean {
        for (i in intents) {
            try {
                ctx.startActivity(i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                return true
            } catch (e: Exception) {
                // Try the next one.
            }
        }
        return false
    }

    fun sharedRoot() = Environment.getExternalStorageDirectory()

    fun defaultVaultParent() = java.io.File(sharedRoot(), "Documents/Obsidian")
}
