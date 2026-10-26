package no.nyta.livesync.profile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class ProfileRulesTest {
    @Test
    fun urls() {
        assertNull(ProfileRules.urlProblem("https://obsidian.example.net"))
        assertNull(ProfileRules.urlProblem("http://100.79.1.2:5984/"))
        assertNotNull(ProfileRules.urlProblem(""))
        assertNotNull(ProfileRules.urlProblem("obsidian.example.net"))
        assertNotNull(ProfileRules.urlProblem("ftp://h"))
        assertNotNull("credentials belong in their fields", ProfileRules.urlProblem("https://u:p@h"))
        assertTrue(ProfileRules.insecureUrl("http://couch.example.net"))
        assertFalse(ProfileRules.insecureUrl("http://phone.tail1234.ts.net"))
        assertFalse(ProfileRules.insecureUrl("http://100.64.0.1"))
        assertFalse(ProfileRules.insecureUrl("https://couch.example.net"))
    }

    @Test
    fun folders() {
        assertEquals(listOf("Notes", "Projects/Work", "a"), ProfileRules.parseFolders(" Notes/\nProjects/Work, a ,,\n"))
        assertNull(ProfileRules.scopeProblem(ScopeMode.ALL, emptyList()))
        assertNotNull(ProfileRules.scopeProblem(ScopeMode.INCLUDE, emptyList()))
        assertNotNull(ProfileRules.scopeProblem(ScopeMode.EXCLUDE, listOf(".obsidian")))
        assertNotNull(ProfileRules.scopeProblem(ScopeMode.EXCLUDE, listOf("a/../b")))
        assertNull(ProfileRules.scopeProblem(ScopeMode.INCLUDE, listOf("Notes")))
        assertEquals("hvelv", ProfileRules.defaultName("/storage/emulated/0/Documents/Obsidian/hvelv/"))
    }
}
