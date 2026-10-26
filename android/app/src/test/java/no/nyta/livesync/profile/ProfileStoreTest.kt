package no.nyta.livesync.profile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.nio.file.Files

class ProfileStoreTest {
    @get:Rule
    val tmp = TemporaryFolder()

    private fun store() = ProfileStore(File(tmp.root, "files/profiles.json"), File(tmp.root, "files/state"))

    private fun profile(id: String, vault: String) = Profile(
        id = id, name = id, vaultPath = File(tmp.root, vault).path,
        url = "https://couch.example.net", database = "vault", username = "u",
    )

    @Test
    fun putGetUpdateRemoveRoundTrip() {
        val s = store()
        s.put(profile("a", "shared/Obsidian/a"))
        s.put(profile("b", "shared/Obsidian/b"))
        assertEquals(listOf("a", "b"), store().all().map { it.id })
        store().update("a") { it.copy(pullOnly = false, scopeMode = ScopeMode.INCLUDE, scopeFolders = listOf("Notes")) }
        val a = store().get("a")!!
        assertFalse(a.pullOnly)
        assertEquals(listOf("Notes"), a.scopeFolders)
        s.stateDir("a").mkdirs()
        File(s.stateDir("a"), "index.sqlite").writeText("x")
        val keep = File(tmp.root, "Download/LiveSync")
        assertNull("nothing set aside, nothing kept", s.remove("a", keep))
        assertNull(store().get("a"))
        assertFalse("the sync index goes with the profile", s.stateDir("a").exists())
        assertEquals(listOf("b"), store().all().map { it.id })
    }

    @Test
    fun setAsideEditsSurviveTheRemoval() {
        val s = store()
        s.put(profile("r", "shared/Obsidian/r"))
        val rec = File(s.stateDir("r"), "recovered/notes")
        rec.mkdirs()
        File(rec, "a.md.1700000000000").writeText("the only copy of an edit")
        val kept = s.remove("r", File(tmp.root, "Download/LiveSync"))!!
        assertEquals("the only copy of an edit", File(kept, "notes/a.md.1700000000000").readText())
        assertFalse(s.stateDir("r").exists())
        assertNull(s.get("r"))
    }

    @Test
    fun aRemovalThatCannotKeepTheSetAsideEditsRemovesNothing() {
        val s = store()
        s.put(profile("r", "shared/Obsidian/r"))
        File(s.stateDir("r"), "recovered").mkdirs()
        File(s.stateDir("r"), "recovered/a.md").writeText("edit")
        val blocked = File(tmp.root, "blocked").apply { writeText("a file, not a folder") }
        try {
            s.remove("r", blocked)
            fail("removed although the edits could not be kept")
        } catch (e: java.io.IOException) {
            // expected
        }
        assertTrue(File(s.stateDir("r"), "recovered/a.md").exists())
        assertTrue(s.get("r") != null)
    }

    @Test
    fun overlappingVaultFoldersAreRefused() {
        val s = store()
        s.put(profile("a", "shared/Obsidian/a"))
        for ((id, vault) in listOf(
            "same" to "shared/Obsidian/a",
            "inside" to "shared/Obsidian/a/sub",
            "above" to "shared/Obsidian",
            "case" to "shared/obsidian/A",
            "dots" to "shared/Obsidian/x/../a",
        )) {
            try {
                s.put(profile(id, vault))
                fail("$id was accepted")
            } catch (e: ProfileStore.OverlapException) {
                assertTrue(e.message!!.contains("overlaps"))
            }
        }
        s.put(profile("sibling", "shared/Obsidian/ab"))
        // Re-saving a profile does not overlap with itself.
        s.put(profile("a", "shared/Obsidian/a").copy(paused = true))
        assertEquals(setOf("a", "sibling"), s.all().map { it.id }.toSet())
    }

    @Test
    fun overlapFollowsLinksEvenForFoldersNotCreatedYet() {
        val real = File(tmp.root, "storage/emulated/0").apply { mkdirs() }
        val link = File(tmp.root, "sdcard")
        Files.createSymbolicLink(link.toPath(), real.toPath())
        assertTrue(ProfileStore.overlaps(File(link, "Documents/Obsidian/v").path, File(real, "Documents/Obsidian/v/x").path))
        assertFalse(ProfileStore.overlaps(File(link, "Documents/Obsidian/v").path, File(real, "Documents/Obsidian/w").path))
    }

    @Test
    fun unknownFieldsAreIgnoredAndDefaultsApply() {
        val f = File(tmp.root, "files/profiles.json")
        f.parentFile!!.mkdirs()
        f.writeText("""{"version":1,"future":true,"profiles":[{"id":"x","name":"x","vaultPath":"/v","url":"https://h","database":"d","username":"u","later":1}]}""")
        val p = store().get("x")!!
        assertTrue("pull-only is the default", p.pullOnly)
        assertEquals(ScopeMode.ALL, p.scopeMode)
        assertFalse(p.paused)
    }
}
