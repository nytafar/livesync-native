package no.nyta.livesync.service

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import no.nyta.livesync.LiveSyncApp

/** Starts the service after boot and after the app was updated (sideload). */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_MY_PACKAGE_REPLACED -> {
                val app = context.applicationContext as LiveSyncApp
                val active = try {
                    app.profiles.all().any { !it.paused }
                } catch (e: Exception) {
                    false
                }
                if (active) SyncService.start(context, "boot: ${intent.action}")
            }
        }
    }
}
