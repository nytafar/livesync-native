package no.nyta.livesync.profile

import java.util.Locale

/**
 * Ticking top-level folders in a folder list that can also be typed. The
 * typed list stays the source of truth: a tap adds or removes exactly that
 * folder, and typed entries further down (`inputs/corpora`) are left alone.
 * Entries are compared as the engine normalises them: `/` and `\` both
 * separate, empty segments drop out, case does not matter, and nothing is
 * trimmed (the typed list trims when it is parsed, see
 * [ProfileRules.parseFolders]).
 */
object ScopePick {
    /**
     * The engine's `normalise_prefix`, exactly: no trimming, so a name with a
     * space at either end never matches the trimmed entry the list saves.
     */
    private fun key(s: String) =
        s.split('/', '\\').filter { it.isNotEmpty() }.joinToString("/").lowercase(Locale.ROOT)

    /**
     * Whether a tap may add [name] to the typed list and have it saved as
     * that same folder: the list splits on commas and line breaks and trims
     * each entry, and the engine reads a backslash as a separator, so a name
     * with one of those (or spaces at either end) would become another.
     */
    fun pickable(name: String): Boolean =
        name.isNotEmpty() && name == name.trim() && name.none { it == ',' || it == '\n' || it == '\r' || it == '\\' }

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
