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
)

/** The single status notification for every profile (spec: one service, one notification). */
object StatusSummary {
    data class Text(val title: String, val text: String, val alert: Boolean)

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
            "Failed" -> "error: ${p.runMessage ?: "unknown"} (retrying)" to true
            else -> {
                gateFailure(p.lastGate)?.let { return "not syncing: $it" to true }
                p.safetyStop?.let { return "safety stop: ${it.substringAfter(' ')}" to true }
                val mode = if (p.pullOnly) "pull-only" else "syncing"
                val parts = mutableListOf(mode)
                p.lastActivityMs?.let { parts += "last change ${clock(it, now)}" }
                if (p.conflicted > 0) parts += "${p.conflicted} conflicted"
                parts.joinToString(" · ") to (p.conflicted > 0)
            }
        }
    }

    fun summarize(lines: List<ProfileLine>, now: Long = System.currentTimeMillis()): Text {
        if (lines.isEmpty()) return Text("LiveSync", "No vault set up", false)
        if (lines.size == 1) {
            val (text, alert) = phrase(lines[0], now)
            return Text("LiveSync · ${lines[0].name}", text, alert)
        }
        val parts = lines.map { it.name to phrase(it, now) }
        val alert = parts.any { it.second.second }
        return Text(
            "LiveSync · ${lines.size} vaults",
            parts.joinToString("\n") { "${it.first}: ${it.second.first}" },
            alert,
        )
    }

    /** `14:02` today, `3 Oct 14:02` otherwise. */
    fun clock(ms: Long, now: Long): String {
        val day = SimpleDateFormat("yyyyMMdd", Locale.ROOT)
        val sameDay = day.format(Date(ms)) == day.format(Date(now))
        val fmt = if (sameDay) "HH:mm" else "d MMM HH:mm"
        return SimpleDateFormat(fmt, Locale.getDefault()).format(Date(ms))
    }
}
