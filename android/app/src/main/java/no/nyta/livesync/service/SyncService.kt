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
import no.nyta.livesync.Core
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

    /** Profiles that could not be started, with the reason (worker thread only). */
    private val problems = mutableMapOf<String, String>()
    private var lastText: StatusSummary.Text = StatusSummary.Text("LiveSync", "Starting…", false)
    private var firstSyncLock: PowerManager.WakeLock? = null

    private val app get() = application as LiveSyncApp

    private val refresh = object : Runnable {
        override fun run() {
            updateNotification()
            // Uptime-based: does not wake a sleeping phone just to redraw.
            handler.postDelayed(this, REFRESH_MS)
        }
    }

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        current = this
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
    private fun reconcile(core: Core) {
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
                // No stored password, or the Keystore key is gone.
                problems[p.id] = e.message ?: "no password"
                continue
            }
            if (p.id !in active && !java.io.File(app.profiles.stateDir(p.id), "index.sqlite").exists()) {
                holdFirstSyncWakeLock()
            }
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
    private fun holdFirstSyncWakeLock() {
        val lock = firstSyncLock ?: getSystemService(PowerManager::class.java)
            .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "livesync:first-sync")
            .also {
                it.setReferenceCounted(false)
                firstSyncLock = it
            }
        lock.acquire(FIRST_SYNC_WAKELOCK_MS)
        app.core.getOrNull()?.log("holding a wakelock for up to ${FIRST_SYNC_WAKELOCK_MS / 60_000} min for a first fetch")
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
                ProfileLine(
                    name = p.name,
                    paused = p.paused,
                    startProblem = problems[p.id],
                    runState = st?.runState?.name?.lowercase()?.replaceFirstChar { it.uppercase() } ?: "Stopped",
                    runMessage = st?.runMessage,
                    pullOnly = p.pullOnly,
                    lastGate = st?.lastGate,
                    lastActivityMs = listOfNotNull(st?.lastPullMs, st?.lastPushMs).maxOrNull(),
                    conflicted = st?.conflicted ?: 0,
                    safetyStop = st?.safetyStop,
                    needsAttention = st?.attention?.size?.toLong() ?: 0,
                )
            }
        } catch (e: Exception) {
            return
        }
        lastText = StatusSummary.summarize(lines)
        notifyNow()
    }

    private fun notifyNow() {
        if (NotificationManagerCompat.from(this).areNotificationsEnabled()) {
            try {
                NotificationManagerCompat.from(this).notify(NOTIFICATION_ID, buildNotification())
            } catch (e: SecurityException) {
                // Notification permission revoked meanwhile.
            }
        }
    }

    private fun buildNotification(): Notification {
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
            .setContentTitle(lastText.title)
            .setContentText(lastText.text.lineSequence().firstOrNull() ?: "")
            .setStyle(NotificationCompat.BigTextStyle().bigText(lastText.text))
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
        ServiceCompat.startForeground(this, NOTIFICATION_ID, buildNotification(), type)
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
        val core = app.core.getOrNull()
        handler.post {
            // A newer instance (a quick pause then resume) owns the engines now;
            // stopping them here would stop its work.
            if (current !== this) return@post
            core?.log("service destroyed; stopping engines")
            try {
                core?.engine?.stopAll()
            } catch (e: Exception) {
                core?.log("stopping engines: ${e.message}", "error")
            }
            if (current === this) current = null
        }
        worker.quitSafely()
        super.onDestroy()
    }

    companion object {
        /** The newest instance; an older one's teardown leaves the engines alone. */
        @Volatile
        private var current: SyncService? = null

        const val NOTIFICATION_ID = 1
        const val REFRESH_MS = 60_000L
        const val FIRST_SYNC_WAKELOCK_MS = 20 * 60_000L
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
