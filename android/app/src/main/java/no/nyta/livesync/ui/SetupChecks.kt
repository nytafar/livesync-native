package no.nyta.livesync.ui

import android.Manifest
import android.content.Context
import android.os.Build
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Row
import androidx.compose.material3.Checkbox
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.platform.LocalContext
import androidx.core.content.edit
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect

/** The device-side state the app depends on, re-read whenever the app comes back to the front. */
data class DeviceChecks(
    val allFiles: Boolean,
    val notifications: Boolean,
    val battery: Boolean,
    val samsung: Boolean,
    val samsungNeverSleepingConfirmed: Boolean,
) {
    val essentialsOk get() = allFiles && battery

    /**
     * One UI's own app sleeping is dealt with. With the battery exemption
     * granted, One UI no longer offers the app in its "Never sleeping apps"
     * list (seen on an S22+, One UI on Android 16), so the exemption is what
     * this checks; without it, the user's own tick that they added the app.
     */
    val samsungSleepOk get() = !samsung || battery || samsungNeverSleepingConfirmed
}

private const val PREFS = "ui"
private const val KEY_NEVER_SLEEPING = "samsung_never_sleeping_confirmed"

fun readChecks(ctx: Context) = DeviceChecks(
    allFiles = Perms.allFilesAccess(),
    notifications = Perms.notificationsAllowed(ctx),
    battery = Perms.batteryExempt(ctx),
    samsung = Perms.isSamsung(),
    samsungNeverSleepingConfirmed = ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        .getBoolean(KEY_NEVER_SLEEPING, false),
)

/** Device checks that refresh on every resume (the user comes back from Settings). */
@Composable
fun rememberDeviceChecks(): DeviceChecks {
    val ctx = LocalContext.current
    var tick by remember { mutableIntStateOf(0) }
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) { tick++ }
    return remember(tick) { readChecks(ctx) }
}

/** Onboarding's permission steps (spec D3 and risk 6). */
@Composable
fun SetupChecksCard(checks: DeviceChecks, onChanged: () -> Unit = {}) {
    val ctx = LocalContext.current
    val notifLauncher = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { onChanged() }
    var neverSleeping by remember(checks) { mutableStateOf(checks.samsungNeverSleepingConfirmed) }
    SectionCard("1. Permissions") {
        CheckRow(
            "All-files access",
            checks.allFiles,
            "Needed to read and write the vault folder directly (Obsidian does the same).",
            if (checks.allFiles) null else "Grant",
        ) { Perms.launch(ctx, Perms.allFilesIntent(ctx), Perms.allFilesFallbackIntent()) }
        CheckRow(
            "Notifications",
            checks.notifications,
            "Shows the always-on sync status.",
            if (checks.notifications) null else "Allow",
        ) {
            if (Build.VERSION.SDK_INT >= 33) {
                notifLauncher.launch(Manifest.permission.POST_NOTIFICATIONS)
            } else {
                Perms.launch(ctx, Perms.appDetailsIntent(ctx))
            }
        }
        CheckRow(
            "Not battery-optimised",
            checks.battery,
            "Without this exemption Android stops the sync when the phone sleeps.",
            if (checks.battery) null else "Exempt",
        ) { Perms.launch(ctx, Perms.batteryIntent(ctx), Perms.appDetailsIntent(ctx)) }
        if (checks.samsung && checks.battery) {
            CheckRow(
                "Samsung: app sleeping",
                true,
                "Checked through the battery exemption above. With it, Android exempts LiveSync from app standby " +
                    "and One UI stops offering it in its \"Never sleeping apps\" list, so there is nothing to add " +
                    "there. If the sync still stops while the phone sleeps, check that Apps → LiveSync → Battery " +
                    "is \"Unrestricted\".",
                null,
            ) {}
        } else if (checks.samsung) {
            CheckRow(
                "Samsung: never sleeping",
                neverSleeping,
                "One UI also puts apps to sleep on its own. Grant the battery exemption above, which covers this; " +
                    "or open Settings → Battery (or Device care → Battery) → Background usage limits → Never " +
                    "sleeping apps → + → LiveSync.",
                "Open",
            ) { Perms.launch(ctx, Perms.samsungBatteryIntent(), Perms.appDetailsIntent(ctx)) }
            Row(verticalAlignment = Alignment.CenterVertically) {
                Checkbox(checked = neverSleeping, onCheckedChange = {
                    neverSleeping = it
                    ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit { putBoolean(KEY_NEVER_SLEEPING, it) }
                    onChanged()
                })
                Text("LiveSync is on the \"Never sleeping apps\" list", style = MaterialTheme.typography.bodySmall)
            }
        }
        if (checks.samsung) {
            TextButton(onClick = { Perms.launch(ctx, Perms.dontKillMyAppIntent()) }) {
                Text("More on dontkillmyapp.com")
            }
        }
    }
}
