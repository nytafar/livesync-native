package no.nyta.livesync.service

import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/** What the notification needs to know about one profile. */
data class ProfileLine(
    val name: String,
    val paused: Boolean,
    /** Could not be started (missing password, refused by the host). */
    val startProblem: String?,
    /** Stopped, Starting, Running or Failed. */
    val runState: String,
    val runMessage: String?,
    val pullOnly: Boolean,
    /** The engine's `last_gate`: `"<ms> ok: ..."` or `"<ms> failed: ..."`. */
    val lastGate: String?,
    val lastActivityMs: Long?,
    val conflicted: Long,
    val safetyStop: String?,
    val needsAttention: Long,
    /** Waiting for a rebuilding device to finish uploading. */
    val rebuildWait: String? = null,
    /** Files pushed back after a rebuild or full fetch. */
    val rebuildOrphans: String? = null,
    /** Rows kept although another device deleted them. */
    val deletedElsewhere: Long = 0,
)

/** The single status notification for every profile (spec: one service, one notification). */
object StatusSummary {
    fun line(p: no.nyta.livesync.profile.Profile, st: no.nyta.livesync.core.ProfileStatus?, startProblem: String?) =
        ProfileLine(
            name = p.name,
            paused = p.paused,
            startProblem = startProblem,
            runState = st?.runState?.name?.lowercase()?.replaceFirstChar { it.uppercase() } ?: "Stopped",
            runMessage = st?.runMessage,
            pullOnly = p.pullOnly,
            lastGate = st?.lastGate,
            lastActivityMs = listOfNotNull(st?.lastPullMs, st?.lastPushMs).maxOrNull(),
            conflicted = st?.conflicted ?: 0,
            safetyStop = st?.safetyStop,
            needsAttention = st?.attention?.size?.toLong() ?: 0,
            rebuildWait = st?.rebuildWait,
            rebuildOrphans = st?.rebuildOrphans,
            deletedElsewhere = st?.deletedElsewhere ?: 0,
        )

    /**
     * The notification: [title], the one-line [summary] shown collapsed, and
     * the full [text] (one line per vault) shown expanded.
     */
    data class Text(val title: String, val text: String, val alert: Boolean, val summary: String = text)

    /**
     * Whether an engine failed because another engine (another profile's, a
     * Termux `lsync`, another app) holds this vault folder: the engine's
     * vault lock refusal names the holder.
     */
    fun heldElsewhere(runMessage: String?): Boolean =
        runMessage?.contains("another LiveSync engine is using") == true

    fun gateFailure(lastGate: String?): String? {
        val g = lastGate ?: return null
        val rest = g.substringAfter(' ', "")
        return if (rest.startsWith("failed:")) rest.removePrefix("failed:").trim() else null
    }

    fun phrase(p: ProfileLine, now: Long = System.currentTimeMillis()): Pair<String, Boolean> {
        if (p.paused) return "paused" to false
        p.startProblem?.let { return "not started: $it" to true }
        return when (p.runState) {
            "Stopped" -> "stopped" to false
            "Starting" -> "starting…" to false
            "Failed" -> if (heldElsewhere(p.runMessage)) {
                "not started: ${p.runMessage} (retrying)" to true
            } else {
                "error: ${p.runMessage ?: "unknown"} (retrying)" to true
            }
            else -> {
                gateFailure(p.lastGate)?.let { return "not syncing: $it" to true }
                p.safetyStop?.let { return "safety stop: ${it.substringAfter(' ')}" to true }
                p.rebuildWait?.let { return "waiting for a rebuild elsewhere to finish" to false }
                val mode = if (p.pullOnly) "pull-only" else "syncing"
                val parts = mutableListOf(mode)
                p.lastActivityMs?.let { parts += "last change ${clock(it, now)}" }
                if (p.conflicted > 0) parts += "${p.conflicted} conflicted"
                if (p.deletedElsewhere > 0) parts += "${p.deletedElsewhere} kept after a delete elsewhere"
                if (p.rebuildOrphans != null) parts += "files pushed back after a rebuild"
                parts.joinToString(" · ") to (p.conflicted > 0 || p.deletedElsewhere > 0 || p.rebuildOrphans != null)
            }
        }
    }

    fun summarize(lines: List<ProfileLine>, now: Long = System.currentTimeMillis()): Text {
        if (lines.isEmpty()) return Text("LiveSync", "No vault set up", false)
        if (lines.size == 1) {
            val (text, alert) = phrase(lines[0], now)
            return Text("LiveSync · ${lines[0].name}", text, alert)
        }
        val parts = lines.map { it to phrase(it, now) }
        val alert = parts.any { it.second.second }
        return Text(
            "LiveSync · ${lines.size} vaults",
            parts.joinToString("\n") { "${it.first.name}: ${it.second.first}" },
            alert,
            counts(parts.map { (l, ph) -> l to ph.second }),
        )
    }

    /**
     * One collapsed line for several vaults: the ones that need attention by
     * name, the rest counted ("Work needs attention · 2 syncing · 1 paused").
     */
    private fun counts(lines: List<Pair<ProfileLine, Boolean>>): String {
        val attention = lines.filter { it.second }.map { it.first.name }
        val rest = lines.filter { !it.second }.map { it.first }
        val buckets = linkedMapOf("syncing" to 0, "starting" to 0, "stopped" to 0, "paused" to 0)
        for (l in rest) {
            val b = when {
                l.paused -> "paused"
                l.runState == "Running" -> "syncing"
                l.runState == "Starting" -> "starting"
                else -> "stopped"
            }
            buckets[b] = buckets.getValue(b) + 1
        }
        val parts = mutableListOf<String>()
        if (attention.isNotEmpty()) {
            parts += attention.joinToString(", ") + if (attention.size == 1) " needs attention" else " need attention"
        }
        buckets.forEach { (what, n) -> if (n > 0) parts += "$n $what" }
        return parts.joinToString(" · ")
    }

    /** `14:02` today, `3 Oct 14:02` otherwise. */
    fun clock(ms: Long, now: Long): String {
        val day = SimpleDateFormat("yyyyMMdd", Locale.ROOT)
        val sameDay = day.format(Date(ms)) == day.format(Date(now))
        val fmt = if (sameDay) "HH:mm" else "d MMM HH:mm"
        return SimpleDateFormat(fmt, Locale.getDefault()).format(Date(ms))
    }
}

/**
 * Posts the status notification only when what it says changes: every post
 * wakes the CPU, and a refresh with the same text is a wasted one. A post
 * and its record happen under one lock, so with posts from two threads (the
 * worker, and `startForeground` on the main thread) the record always
 * matches the post that landed last.
 */
class PostGate<T : Any> {
    private var shown: T? = null

    /**
     * Run [post] for [t] unless [t] is already shown ([always]: regardless).
     * [post] returns whether it showed [t]; if not, what is shown is unknown
     * and the next call posts again.
     */
    @Synchronized fun post(t: T, always: Boolean = false, post: (T) -> Boolean) {
        if (!always && t == shown) return
        shown = null
        if (post(t)) shown = t
    }
}
