package no.nyta.livesync.ui

import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.TimeZone

/** Plain text for values the engine hands over raw (no Compose; unit-tested). */
object Formats {
    /** Epoch milliseconds as a local date and time: `7 Apr 2026 13:22`. */
    fun dateTime(ms: Long, locale: Locale = Locale.getDefault(), tz: TimeZone = TimeZone.getDefault()): String =
        SimpleDateFormat("d MMM yyyy HH:mm", locale).apply { timeZone = tz }.format(Date(ms))

    /** The milestone's `created`, which is 0 when the gate did not get that far. */
    fun created(ms: Long, locale: Locale = Locale.getDefault(), tz: TimeZone = TimeZone.getDefault()): String =
        if (ms <= 0) "–" else dateTime(ms, locale, tz)

    /** A byte count in decimal units, one decimal: `365.2 MB`. */
    fun size(bytes: Long, locale: Locale = Locale.getDefault()): String = when {
        bytes >= 1_000_000_000 -> String.format(locale, "%.1f GB", bytes / 1e9)
        bytes >= 1_000_000 -> String.format(locale, "%.1f MB", bytes / 1e6)
        bytes >= 1_000 -> String.format(locale, "%.1f kB", bytes / 1e3)
        else -> "$bytes B"
    }

    /** `4481 files · 365.2 MB`. */
    fun filesAndSize(files: Long, bytes: Long, locale: Locale = Locale.getDefault()): String =
        "$files ${if (files == 1L) "file" else "files"} · ${size(bytes, locale)}"

    private val createdInGate = Regex("""\bcreated (\d{10,})\b""")

    /**
     * The engine's gate summary (`ok: version 12 created 1775560933683 locked
     * false`) with the milestone's `created` as a date.
     */
    fun gateText(text: String, locale: Locale = Locale.getDefault(), tz: TimeZone = TimeZone.getDefault()): String =
        createdInGate.replace(text) { m ->
            m.groupValues[1].toLongOrNull()?.let { "created ${dateTime(it, locale, tz)}" } ?: m.value
        }

    /**
     * The status screen's banner for an owed full fetch, or null when there
     * is none to show. A plain owed fetch is normal (a new profile, "Fetch
     * everything again") and any running engine, pull-only included,
     * completes it; only a resolution after a remote lock needs two-way.
     * A rebuild's quiet wait has its own banner.
     */
    fun fetchOwedBanner(needsFullFetch: Boolean, resolveOwed: Boolean, rebuildWait: Boolean, pullOnly: Boolean, running: Boolean): String? {
        if (!needsFullFetch) return null
        if (resolveOwed) {
            return if (pullOnly) {
                "The server is locked after a rebuild elsewhere. Pull-only keeps following it; this phone is marked " +
                    "as resolved when you switch to two-way sync."
            } else {
                "The server is locked after a rebuild elsewhere; this phone is marked as resolved once the full " +
                    "fetch completes, and pushes start then."
            }
        }
        if (rebuildWait) return null
        if (!running) return "A full fetch from the server is owed; it runs when syncing resumes."
        return if (pullOnly) "Fetching everything from the server…" else "Fetching everything from the server; pushes start once it completes."
    }
}
