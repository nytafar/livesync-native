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
                assertTrue(e.message!!, e.message!!.contains("cannot share a folder or sit inside each other"))
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
    fun overlapIsEqualInsideOrAboveInAnySpellingOfSharedStorage() {
        val o = ProfileStore.Overlap.SAME
        val inside = ProfileStore.Overlap.INSIDE
        val above = ProfileStore.Overlap.ABOVE
        val v = "/storage/emulated/0/Documents/Obsidian/Notes"
        for ((a, b, want) in listOf(
            Triple(v, v, o),
            Triple("$v/", v, o),
            Triple("$v//", "$v/", o),
            // Shared storage folds case.
            Triple("/storage/emulated/0/documents/obsidian/NOTES", v, o),
            // The same folder through the links to shared storage.
            Triple("/sdcard/Documents/Obsidian/Notes", v, o),
            Triple("/sdcard/Documents/Obsidian/Notes/", "/storage/self/primary/documents/obsidian/notes", o),
            Triple("/mnt/sdcard/Documents/Obsidian/Notes", "/sdcard/Documents/Obsidian/Notes", o),
            Triple("$v/Work", v, inside),
            Triple("/sdcard/documents/Obsidian/notes/Work/", v, inside),
            Triple("/storage/emulated/0/Documents", v, above),
            Triple("/sdcard/", v, above),
            Triple("/sdcard", "/storage/emulated/0", o),
            Triple("$v/../Notes/Sub", v, inside),
        )) {
            assertEquals("$a against $b", want, ProfileStore.relation(a, b))
        }
        // Component-wise: a longer name beside it is apart.
        for ((a, b) in listOf(
            "/storage/emulated/0/Documents/Obsidian/Notes2" to v,
            "/sdcard/Documents/Obsidian/Note" to v,
            "/storage/emulated/0/Documents/Obsidian/Work" to v,
            "/storage/emulated/10/Documents/Obsidian/Notes" to v,
        )) {
            assertNull("$a against $b", ProfileStore.relation(a, b))
            assertFalse(ProfileStore.overlaps(a, b))
        }
    }

    @Test
    fun theFolderStepSaysHowAFolderOverlaps() {
        val s = store()
        s.put(profile("a", "shared/Obsidian/a").copy(name = "Notes"))
        val same = s.clash(profile("n", "shared/Obsidian/A/"))!!
        assertEquals(ProfileStore.Overlap.SAME, same.kind)
        assertTrue(same.message.startsWith("This is already the vault folder of \"Notes\"."))
        val inside = s.clash(profile("n", "shared/Obsidian/a/sub"))!!
        assertEquals(ProfileStore.Overlap.INSIDE, inside.kind)
        assertTrue(inside.message.startsWith("This folder is inside the vault folder of \"Notes\""))
        val above = s.clash(profile("n", "shared"))!!
        assertEquals(ProfileStore.Overlap.ABOVE, above.kind)
        assertTrue(above.message.startsWith("This folder contains the vault folder of \"Notes\""))
        assertNull(s.clash(profile("n", "shared/Obsidian/b")))
        // A profile never clashes with itself.
        assertNull(s.clash(profile("a", "shared/Obsidian/a")))
    }

    @Test
    fun severalVaultsOnOneDatabaseKeepTheirOwnStateAndOrder() {
        val s = store()
        val notes = profile("n1", "shared/Obsidian/notes").copy(scopeMode = ScopeMode.INCLUDE, scopeFolders = listOf("notes"))
        val work = profile("w1", "shared/Obsidian/work").copy(scopeMode = ScopeMode.INCLUDE, scopeFolders = listOf("work"))
        val other = profile("o1", "shared/Other").copy(database = "other")
        // Same server and database, different vault folders and scopes: allowed.
        s.put(notes)
        s.put(work)
        s.put(other)
        assertEquals(listOf("n1", "w1", "o1"), store().all().map { it.id })
        assertTrue(s.stateDir("n1") != s.stateDir("w1"))
        // Editing one keeps its place in the list and leaves the others alone.
        s.update("n1") { it.copy(pullOnly = false) }
        assertEquals(listOf("n1", "w1", "o1"), store().all().map { it.id })
        assertEquals(work, store().get("w1"))
        assertEquals(other, store().get("o1"))
        for (id in listOf("n1", "w1", "o1")) File(s.stateDir(id), "index.sqlite").apply { parentFile!!.mkdirs() }.writeText(id)
        s.remove("w1", File(tmp.root, "Download/LiveSync"))
        assertEquals(listOf("n1", "o1"), store().all().map { it.id })
        assertEquals("n1", File(s.stateDir("n1"), "index.sqlite").readText())
        assertEquals("o1", File(s.stateDir("o1"), "index.sqlite").readText())
        assertFalse(s.stateDir("w1").exists())
        // The removed vault's folder can be used again, and only it.
        s.put(profile("w2", "shared/Obsidian/work"))
        assertEquals(listOf("n1", "o1", "w2"), store().all().map { it.id })
    }

    @Test
    fun theOneProfileOfAnEarlierVersionComesBackUnchanged() {
        // profiles.json as the single-vault app (0.1.0-m2b) wrote it, with the
        // shape of the phone's profile: two-way, an exclude list, a device name.
        val f = File(tmp.root, "files/profiles.json")
        f.parentFile!!.mkdirs()
        val written = """
            {
                "version": 1,
                "profiles": [
                    {
                        "id": "03d5fce91982",
                        "name": "hvelv",
                        "vaultPath": "/storage/emulated/0/Documents/Obsidian/hvelv",
                        "url": "https://couch.example.net",
                        "database": "vault",
                        "username": "u",
                        "scopeMode": "EXCLUDE",
                        "scopeFolders": [
                            "inputs/corpora",
                            "inputs/media",
                            "contexts/myrvann",
                            "efforts/nyta-seo"
                        ],
                        "pullOnly": false,
                        "paused": false,
                        "deviceName": "samsung SM-S906B",
                        "createdAtMs": 1761550000000
                    }
                ]
            }
        """.trimIndent()
        f.writeText(written)
        val s = store()
        val expected = Profile(
            id = "03d5fce91982", name = "hvelv", vaultPath = "/storage/emulated/0/Documents/Obsidian/hvelv",
            url = "https://couch.example.net", database = "vault", username = "u",
            scopeMode = ScopeMode.EXCLUDE,
            scopeFolders = listOf("inputs/corpora", "inputs/media", "contexts/myrvann", "efforts/nyta-seo"),
            pullOnly = false, paused = false, deviceName = "samsung SM-S906B", createdAtMs = 1761550000000,
        )
        assertEquals(listOf(expected), s.all())
        // Its state folder (index, node ID) is where it was.
        assertEquals(File(tmp.root, "files/state/03d5fce91982"), s.stateDir("03d5fce91982"))
        // Reading never rewrites the file.
        assertEquals(written, f.readText())
        // A second vault leaves the first exactly as it was, first in the list.
        s.put(profile("second", "shared/Obsidian/second"))
        assertEquals(listOf("03d5fce91982", "second"), store().all().map { it.id })
        assertEquals(expected, store().get("03d5fce91982"))
        // And a folder inside it is refused.
        try {
            s.put(profile("x", "x").copy(vaultPath = "/sdcard/Documents/Obsidian/hvelv/inputs"))
            fail("a vault inside the existing one was accepted")
        } catch (e: ProfileStore.OverlapException) {
            assertTrue(e.message!!.contains("inside the vault folder of \"hvelv\""))
        }
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
