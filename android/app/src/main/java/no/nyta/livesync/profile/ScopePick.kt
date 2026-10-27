package no.nyta.livesync.profile

import java.util.Locale

/**
 * Ticking top-level folders in a folder list that can also be typed. The
 * typed list stays the source of truth: a tap adds or removes exactly that
 * folder, and typed entries further down (`inputs/corpora`) are left alone.
 * Matching is case-insensitive, as the engine's.
 */
object ScopePick {
    private fun key(s: String) = s.trim().trim('/').lowercase(Locale.ROOT)

    /** Whether [name] itself is in [folders] (any case). */
    fun ticked(folders: List<String>, name: String): Boolean = folders.any { key(it) == key(name) }

    /** [folders] with [name] added, or removed when it is there (any case). */
    fun toggle(folders: List<String>, name: String): List<String> =
        if (ticked(folders, name)) folders.filterNot { key(it) == key(name) } else folders + name

    /** The listed entries that lie further down inside [name], as typed. */
    fun nestedIn(folders: List<String>, name: String): List<String> {
        val k = key(name) + "/"
        return folders.filter { key(it).startsWith(k) }
    }

    /** Whether a file at the vault root syncs under this scope (only a list of folders leaves it out). */
    fun rootSyncs(mode: ScopeMode): Boolean = mode != ScopeMode.INCLUDE
}
