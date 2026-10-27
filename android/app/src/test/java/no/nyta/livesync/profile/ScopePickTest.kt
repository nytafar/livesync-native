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
    fun rootFilesSyncUnlessOnlyFoldersAreListed() {
        assertTrue(ScopePick.rootSyncs(ScopeMode.ALL))
        assertTrue(ScopePick.rootSyncs(ScopeMode.EXCLUDE))
        assertFalse(ScopePick.rootSyncs(ScopeMode.INCLUDE))
    }
}
