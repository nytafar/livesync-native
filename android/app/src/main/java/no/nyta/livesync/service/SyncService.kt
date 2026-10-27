package no.nyta.livesync.service

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.IBinder
import android.os.PowerManager
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import androidx.core.content.edit
import no.nyta.livesync.Core
import no.nyta.livesync.EngineLifecycle
import no.nyta.livesync.LiveSyncApp
import no.nyta.livesync.R
import no.nyta.livesync.core.RunState
import no.nyta.livesync.core.StatusListener
import no.nyta.livesync.ui.MainActivity

/**
 * The always-on host (spec D3): a `specialUse` foreground service, sticky,
 * started at boot, holding one engine per active profile and one status
 * notification for all of them. Every engine call runs on [worker], because
 * starting and stopping engines blocks.
 */
class SyncService : Service() {
    private lateinit var worker: HandlerThread
    private lateinit var handler: Handler
    private var network: NetworkMonitor? = null

    /** Profiles that could not be started, with the reason (shared with the screens). */
    private val problems get() = app.startProblems
    private var lastText: StatusSummary.Text = StatusSummary.Text("LiveSync", "Starting…", false)
    /** What the status notification shows now; unchanged text is not re-posted. */
    private val shown = PostGate<StatusSummary.Text>()
    private var firstSyncLock: PowerManager.WakeLock? = null
    /** Profiles the first-sync wakelock is held for (worker thread). */
    private val firstSync = FirstSyncWaits()

    private val app get() = application as LiveSyncApp

    private val refresh = object : Runnable {
        override fun run() {
            // Re-reads the status; posts only if the text changed (the times
            // in it are clock times, so they do not tick by themselves).
            updateNotification()
            // Uptime-based: does not wake a sleeping phone just to redraw.
            handler.postDelayed(this, REFRESH_MS)
        }
    }

    /** While the first-sync wakelock is held: release it once every first fetch caught up. */
    private val firstSyncCheck = object : Runnable {
        override fun run() {
            if (checkFirstSync()) handler.postDelayed(this, FIRST_SYNC_CHECK_MS)
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        current.set(this)
        worker = HandlerThread("livesync-service").apply { start() }
        handler = Handler(worker.looper)
        goForeground()
        handler.post {
            val core = app.core.getOrNull() ?: return@post
            // Called on a Rust thread: only hand over to the worker, never throw.
            core.engine.setListener(object : StatusListener {
                override fun onStateChanged(profileId: String, state: RunState, message: String?) {
                    handler.post { updateNotification() }
                }
            })
            core.log("service created")
        }
        network = NetworkMonitor(this) { up, metered ->
            handler.post {
                app.core.getOrNull()?.engine?.networkChanged(up, metered)
                if (up) handler.post { updateNotification() }
            }
        }.also { it.start() }
        handler.postDelayed(refresh, REFRESH_MS)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Every startForegroundService() must be answered with startForeground().
        if (!goForeground()) return START_NOT_STICKY
        val action = intent?.action ?: ACTION_RECONCILE
        val profileId = intent?.getStringExtra(EXTRA_PROFILE)
        val reason = intent?.getStringExtra(EXTRA_REASON) ?: if (intent == null) "restarted by the system" else action
        handler.post { handle(action, profileId, reason) }
        return START_STICKY
    }

    private fun handle(action: String, profileId: String?, reason: String) {
        val core = app.core.getOrElse { e ->
            lastText = StatusSummary.Text("LiveSync", "The sync core failed to load: ${e.message}", true)
            notifyNow()
            return
        }
        when (action) {
            ACTION_SYNC_NOW -> {
                // A service started by this very intent has no engines yet.
                if (core.engine.activeProfiles().isEmpty()) reconcile(core)
                val ids = profileId?.let { listOf(it) } ?: core.engine.activeProfiles()
                for (id in ids) {
                    try {
                        core.engine.syncNow(id)
                    } catch (e: Exception) {
                        core.log("sync now for $id: ${e.message}", "warn")
                    }
                }
                core.log("sync now requested")
            }
            else -> {
                core.log("reconcile ($reason)")
                reconcile(core)
            }
        }
        updateNotification()
    }

    /**
     * Make the host's engines match the profile store. The host is the source
     * of truth for what runs (it is shared with the UI and with any earlier
     * instance of this service); starting a profile with unchanged settings
     * is a no-op there.
     */
    private fun reconcile(core: Core) = synchronized(EngineLifecycle.lock) { reconcileLocked(core) }

    private fun reconcileLocked(core: Core) {
        val profiles = try {
            app.profiles.all()
        } catch (e: Exception) {
            core.log("cannot read the profiles: ${e.message}", "error")
            emptyList()
        }
        val wanted = profiles.filter { !it.paused }.associateBy { it.id }
        val active = try {
            core.engine.activeProfiles().toSet()
        } catch (e: Exception) {
            emptySet()
        }
        for (id in active) {
            if (id !in wanted) {
                try {
                    core.engine.stop(id)
                } catch (e: Exception) {
                    core.log("stopping $id: ${e.message}", "error")
                }
                core.log("profile $id stopped")
            }
        }
        problems.keys.retainAll(wanted.keys)
        for (p in wanted.values) {
            val cfg = try {
                core.configFor(p)
            } catch (e: Exception) {
                // No stored password, or the Keystore key is gone. An engine
                // still running on older settings (perhaps two-way, where the
                // profile now says pull-only) must not run on.
                problems[p.id] = e.message ?: "no password"
                if (p.id in active) {
                    try {
                        core.engine.stop(p.id)
                    } catch (x: Exception) {
                        core.log("stopping ${p.id}: ${x.message}", "error")
                    }
                    core.log("profile ${p.id} stopped: its settings cannot be loaded (${e.message})", "error")
                }
                continue
            }
            if (p.id !in active && owesFirstFetch(core, p)) holdFirstSyncWakeLock(p.id)
            try {
                core.engine.start(cfg)
                problems.remove(p.id)
                if (p.id !in active) core.log("profile ${p.id} started (${if (p.pullOnly) "pull-only" else "two-way"})")
            } catch (e: Exception) {
                // LiveSyncException (refused, invalid) or InternalException (a bug).
                problems[p.id] = e.message ?: e.javaClass.simpleName
                core.log("profile ${p.id} not started: ${e.message}", "error")
            }
        }
        if (wanted.isEmpty()) {
            core.log("no active profiles; stopping the service")
            stopForegroundCompat()
            stopSelf()
        }
    }

    /**
     * A profile's first fetch can take minutes (thousands of files). Keep the
     * CPU up for a bounded while so it is not stretched over hours of Doze
     * when the screen goes off. Later syncs need no wakelock.
     */
    private fun holdFirstSyncWakeLock(profileId: String) {
        val lock = firstSyncLock ?: getSystemService(PowerManager::class.java)
            .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "livesync:first-sync")
            .also {
                it.setReferenceCounted(false)
                firstSyncLock = it
            }
        firstSync.add(profileId)
        lock.acquire(FIRST_SYNC_WAKELOCK_MS)
        handler.removeCallbacks(firstSyncCheck)
        handler.postDelayed(firstSyncCheck, FIRST_SYNC_CHECK_MS)
        app.core.getOrNull()?.log(
            "holding a wakelock for up to ${FIRST_SYNC_WAKELOCK_MS / 60_000} min for the first fetch of $profileId",
        )
    }

    private fun owesFirstFetch(core: Core, p: no.nyta.livesync.profile.Profile): Boolean =
        !firstFetchSettled(core, p, countFailed = false)

    /** Caught up, or (with [countFailed]) failed and only retrying later. */
    private fun firstFetchSettled(core: Core, p: no.nyta.livesync.profile.Profile, countFailed: Boolean): Boolean {
        val st = try {
            core.status(p)
        } catch (e: Exception) {
            null
        }
        val hasIndex = st?.hasIndex ?: java.io.File(app.profiles.stateDir(p.id), "index.sqlite").exists()
        val failed = countFailed && st?.runState == RunState.FAILED
        return FirstSyncWaits.settled(hasIndex, st?.daemonHeartbeatMs, failed)
    }

    /**
     * Release the first-sync wakelock once every profile it is held for has
     * caught up (or stopped). Returns whether to check again.
     */
    private fun checkFirstSync(): Boolean {
        val lock = firstSyncLock ?: return false
        val core = app.core.getOrNull() ?: return false
        if (!lock.isHeld) {
            // The cap ran out first.
            firstSync.clear()
            core.log("first-fetch wakelock ended at its ${FIRST_SYNC_WAKELOCK_MS / 60_000} min cap")
            return false
        }
        val active = try {
            core.engine.activeProfiles().toSet()
        } catch (e: Exception) {
            emptySet()
        }
        val done = firstSync.settle(active) { id ->
            val p = try {
                app.profiles.get(id)
            } catch (e: Exception) {
                null
            } ?: return@settle false
            firstFetchSettled(core, p, countFailed = true)
        }
        if (!done) return true
        lock.release()
        core.log("first fetch caught up (or failed); wakelock released")
        return false
    }

    private fun updateNotification() {
        val core = app.core.getOrNull() ?: return
        val lines = try {
            app.profiles.all().map { p ->
                val st = if (p.paused) null else try {
                    core.status(p)
                } catch (e: Exception) {
                    null
                }
                st?.let { alertIfNew(p, it) }
                StatusSummary.line(p, st, problems[p.id])
            }
        } catch (e: Exception) {
            return
        }
        lastText = StatusSummary.summarize(lines)
        notifyNow()
    }

    /**
     * A separate, alerting notification for what needs the user: files pushed
     * back after a rebuild, and a mass-delete safety stop. Each distinct
     * value alerts once (they are stamped by the engine), so a status that
     * stays the same never alerts again.
     */
    private fun alertIfNew(p: no.nyta.livesync.profile.Profile, st: no.nyta.livesync.core.ProfileStatus) {
        val prefs = getSharedPreferences("alerts", MODE_PRIVATE)
        fun once(key: String, stamped: String?, id: Int, title: String, text: () -> String) {
            // Without the engine's timestamp: a stop re-recorded at every pass
            // with the same text is the same stop.
            val value = stamped?.substringAfter(' ') ?: return
            if (prefs.getString(key, null) == value) return
            prefs.edit { putString(key, value) }
            postAlert(id, p.id, title, text())
        }
        val base = alertBase(p.id)
        once("orphans-${p.id}", st.rebuildOrphans, base, "${p.name}: files pushed back after a rebuild") {
            st.rebuildOrphans!!.substringAfter(' ') + " Check them in Obsidian."
        }
        once("safety-${p.id}", st.safetyStop, base + 1, "${p.name}: deletions held back") {
            st.safetyStop!!.substringAfter(' ') + " Open LiveSync to decide."
        }
    }

    private fun postAlert(id: Int, profileId: String, title: String, text: String) {
        if (!NotificationManagerCompat.from(this).areNotificationsEnabled()) return
        // Opens that vault's status screen. The request code is the alert's
        // own ID: extras do not tell PendingIntents apart, so with one code
        // every alert would open the vault of whichever was posted last.
        val open = PendingIntent.getActivity(
            this, id,
            Intent(this, MainActivity::class.java)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
                .putExtra(MainActivity.EXTRA_OPEN_PROFILE, profileId),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val n = NotificationCompat.Builder(this, LiveSyncApp.CHANNEL_ALERTS)
            .setSmallIcon(R.drawable.ic_stat_sync)
            .setContentTitle(title)
            .setContentText(text)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text))
            .setAutoCancel(true)
            .setContentIntent(open)
            .build()
        try {
            NotificationManagerCompat.from(this).notify(id, n)
        } catch (e: SecurityException) {
            // Permission revoked meanwhile.
        }
        app.core.getOrNull()?.log("alert: $title")
    }

    private fun notifyNow() = shown.post(lastText) { text ->
        if (!NotificationManagerCompat.from(this).areNotificationsEnabled()) return@post false
        try {
            NotificationManagerCompat.from(this).notify(NOTIFICATION_ID, buildNotification(text))
            true
        } catch (e: SecurityException) {
            // Notification permission revoked meanwhile.
            false
        }
    }

    private fun buildNotification(text: StatusSummary.Text = lastText): Notification {
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        val syncNow = PendingIntent.getForegroundService(
            this, 1, Intent(this, SyncService::class.java).setAction(ACTION_SYNC_NOW),
            PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
        )
        return NotificationCompat.Builder(this, LiveSyncApp.CHANNEL_STATUS)
            .setSmallIcon(R.drawable.ic_stat_sync)
            .setContentTitle(text.title)
            .setContentText(text.summary)
            .setStyle(NotificationCompat.BigTextStyle().bigText(text.text))
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setSilent(true)
            .setShowWhen(false)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .setContentIntent(open)
            .addAction(0, "Sync now", syncNow)
            .build()
    }

    /** Returns false when Android refused the foreground start. */
    private fun goForeground(): Boolean = try {
        // specialUse exists from Android 14; below it the service needs no type.
        val type = if (Build.VERSION.SDK_INT >= 34) ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE else 0
        shown.post(lastText, always = true) { text ->
            ServiceCompat.startForeground(this, NOTIFICATION_ID, buildNotification(text), type)
            true
        }
        true
    } catch (e: Exception) {
        // ForegroundServiceStartNotAllowedException (Android 12+) and friends:
        // log it, and do not run engines in a service that may be killed any moment.
        app.core.getOrNull()?.log("foreground start refused: ${e.message}", "error")
        stopSelf()
        false
    }

    private fun stopForegroundCompat() {
        ServiceCompat.stopForeground(this, ServiceCompat.STOP_FOREGROUND_REMOVE)
    }

    override fun onDestroy() {
        network?.stop()
        firstSyncLock?.let { if (it.isHeld) it.release() }
        handler.removeCallbacks(refresh)
        handler.removeCallbacks(firstSyncCheck)
        val core = app.core.getOrNull()
        handler.post {
            synchronized(EngineLifecycle.lock) {
                // A newer instance (a quick pause then resume) owns the engines
                // now; stopping them here would stop its work. Its reconcile
                // waits for this lock, so it runs after this teardown or not
                // at all before the check.
                if (current.get() !== this) return@synchronized
                core?.log("service destroyed; stopping engines")
                try {
                    core?.engine?.stopAll()
                } catch (e: Exception) {
                    core?.log("stopping engines: ${e.message}", "error")
                }
                // Only if no newer instance took over meanwhile (its onCreate
                // does not take the lock).
                current.compareAndSet(this, null)
            }
        }
        worker.quitSafely()
        super.onDestroy()
    }

    companion object {
        /** The newest instance; an older one's teardown leaves the engines alone. */
        private val current = java.util.concurrent.atomic.AtomicReference<SyncService?>(null)

        const val NOTIFICATION_ID = 1
        const val REFRESH_MS = 60_000L
        const val FIRST_SYNC_WAKELOCK_MS = 20 * 60_000L
        const val FIRST_SYNC_CHECK_MS = 5_000L
        const val ACTION_RECONCILE = "no.nyta.livesync.RECONCILE"
        const val ACTION_SYNC_NOW = "no.nyta.livesync.SYNC_NOW"
        const val EXTRA_PROFILE = "profile"
        const val EXTRA_REASON = "reason"

        /** Start the service (or make it re-read the profiles). */
        fun start(context: Context, reason: String) {
            val i = Intent(context, SyncService::class.java)
                .setAction(ACTION_RECONCILE)
                .putExtra(EXTRA_REASON, reason)
            try {
                ContextCompat.startForegroundService(context, i)
            } catch (e: Exception) {
                (context.applicationContext as LiveSyncApp).core.getOrNull()
                    ?.log("cannot start the service ($reason): ${e.message}", "error")
            }
        }

        /**
         * Forget a removed profile's alert records and take down its alerts,
         * so a vault added later under another ID starts clean and no alert
         * opens a vault that is gone.
         */
        fun forgetAlerts(context: Context, profileId: String) {
            context.getSharedPreferences("alerts", Context.MODE_PRIVATE).edit {
                remove("orphans-$profileId")
                remove("safety-$profileId")
            }
            val base = alertBase(profileId)
            NotificationManagerCompat.from(context).apply {
                cancel(base)
                cancel(base + 1)
            }
        }

        /** The first of a profile's alert notification IDs (the status notification is 1). */
        private fun alertBase(profileId: String) = 1000 + (profileId.hashCode() and 0xffff) * 4

        fun syncNow(context: Context, profileId: String?) {
            val i = Intent(context, SyncService::class.java).setAction(ACTION_SYNC_NOW)
            if (profileId != null) i.putExtra(EXTRA_PROFILE, profileId)
            try {
                ContextCompat.startForegroundService(context, i)
            } catch (e: Exception) {
                (context.applicationContext as LiveSyncApp).core.getOrNull()
                    ?.log("cannot reach the service: ${e.message}", "error")
            }
        }
    }
}
