package no.nyta.livesync.profile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class ScopePickTest {
    @Test
    fun aTapAddsOrRemovesExactlyThatFolderInAnyCase() {
        val typed = listOf("notes", "inputs/corpora")
        assertTrue(ScopePick.ticked(typed, "Notes"))
        assertFalse(ScopePick.ticked(typed, "inputs"))
        assertEquals(listOf("inputs/corpora"), ScopePick.toggle(typed, "Notes"))
        assertEquals(listOf("notes", "inputs/corpora", "inputs"), ScopePick.toggle(typed, "inputs"))
        assertEquals(listOf("Archive"), ScopePick.toggle(emptyList(), "Archive"))
        assertEquals(emptyList<String>(), ScopePick.toggle(listOf("/Archive/"), "archive"))
    }

    @Test
    fun typedEntriesFurtherDownAreShownAndKept() {
        val typed = listOf("Inputs/corpora", "inputs/media", "inputsx")
        assertEquals(listOf("Inputs/corpora", "inputs/media"), ScopePick.nestedIn(typed, "inputs"))
        assertEquals(typed + "inputs", ScopePick.toggle(typed, "inputs"))
    }

    @Test
    fun typedEntriesMatchAsTheEngineNormalisesThem() {
        // The engine reads `Notes\` and `/notes//` as `notes`.
        assertTrue(ScopePick.ticked(listOf("Notes\\"), "notes"))
        assertTrue(ScopePick.ticked(listOf("/notes//"), "Notes"))
        assertEquals(emptyList<String>(), ScopePick.toggle(listOf("Notes\\"), "notes"))
        assertEquals(listOf("Inputs\\corpora"), ScopePick.nestedIn(listOf("Inputs\\corpora"), "inputs"))
    }

    @Test
    fun aNameTheFolderListCannotHoldCannotBePicked() {
        // The typed list splits on commas, so "Work, Personal" would become two folders.
        assertFalse(ScopePick.pickable("Work, Personal"))
        // Trimmed when parsed, or split by the engine: another folder.
        assertFalse(ScopePick.pickable(" Notes "))
        assertFalse(ScopePick.pickable("a\\b"))
        assertTrue(ScopePick.pickable("Work & Personal"))
        assertTrue(ScopePick.pickable("My Notes"))
        val typed = ProfileRules.parseFolders(ScopePick.toggle(emptyList(), "Work & Personal").joinToString("\n"))
        assertTrue(ScopePick.ticked(typed, "Work & Personal"))
    }

    @Test
    fun rootFilesSyncUnlessOnlyFoldersAreListed() {
        assertTrue(ScopePick.rootSyncs(ScopeMode.ALL))
        assertTrue(ScopePick.rootSyncs(ScopeMode.EXCLUDE))
        assertFalse(ScopePick.rootSyncs(ScopeMode.INCLUDE))
    }

    @Test
    fun aTickMeansWhatIsSaved() {
        // Typed with slack around the slashes: saved without it, and ticked as such.
        val saved = ProfileRules.parseFolders("/ Notes\n inputs / corpora \nArchive\\")
        assertEquals(listOf("Notes", "inputs/corpora", "Archive"), saved)
        assertTrue(ScopePick.ticked(saved, "Notes"))
        assertTrue(ScopePick.ticked(saved, "archive"))
        assertEquals(listOf("inputs/corpora"), ScopePick.nestedIn(saved, "inputs"))
        // A name with a space at either end is not the trimmed entry.
        assertFalse(ScopePick.ticked(saved, " Notes"))
        assertFalse(ScopePick.ticked(listOf(" Notes"), "Notes"))
    }
}
