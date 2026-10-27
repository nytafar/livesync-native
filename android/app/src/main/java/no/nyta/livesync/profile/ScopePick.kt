package no.nyta.livesync.profile

import java.util.Locale

/**
 * Ticking top-level folders in a folder list that can also be typed. The
 * typed list stays the source of truth: a tap adds or removes exactly that
 * folder, and typed entries further down (`inputs/corpora`) are left alone.
 * Entries are compared as the engine normalises them: `/` and `\` both
 * separate, empty segments drop out, case does not matter.
 */
object ScopePick {
    private fun key(s: String) =
        s.trim().split('/', '\\').filter { it.isNotEmpty() }.joinToString("/").lowercase(Locale.ROOT)

    /**
     * Whether a tap may add [name] to the typed list: the list splits on
     * commas and line breaks, so a name holding one would turn into others.
     */
    fun pickable(name: String): Boolean = name.none { it == ',' || it == '\n' || it == '\r' }

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
