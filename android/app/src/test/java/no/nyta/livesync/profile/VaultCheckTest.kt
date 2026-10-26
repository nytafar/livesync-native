package no.nyta.livesync.profile

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

class VaultCheckTest {
    @get:Rule
    val tmp = TemporaryFolder()

    private val root get() = File(tmp.root, "emulated/0").apply { mkdirs() }

    @Test
    fun aNewFolderUnderSharedStorageIsUsable() {
        val r = VaultCheck.inspect(File(root, "Documents/Obsidian/new").path, root)
        assertTrue(r.errors.toString(), r.errors.isEmpty())
        assertFalse(r.exists)
        assertTrue(r.writable)
    }

    @Test
    fun foldersOutsideSharedStorageOrInAndroidDataAreRefused() {
        assertFalse(VaultCheck.inspect(File(tmp.root, "elsewhere").path, root).errors.isEmpty())
        assertFalse(VaultCheck.inspect(File(root, "Android/data/md.obsidian/files/v").path, root).errors.isEmpty())
        assertFalse(VaultCheck.inspect(File(root, "Documents/.hidden").path, root).errors.isEmpty())
        assertFalse(VaultCheck.inspect(root.path, root).errors.isEmpty())
    }

    @Test
    fun existingFilesAndAnEnabledLiveSyncPluginAreReported() {
        val v = File(root, "Documents/Obsidian/v")
        File(v, "notes").mkdirs()
        File(v, "notes/a.md").writeText("a")
        File(v, "b.md").writeText("b")
        File(v, ".obsidian/plugins/obsidian-livesync").mkdirs()
        File(v, ".obsidian/workspace.json").writeText("{}")
        var r = VaultCheck.inspect(v.path, root)
        assertEquals("dot-folders are not counted", 2, r.files)
        assertTrue(r.isObsidianVault)
        assertFalse("installed but not enabled", r.liveSyncPluginEnabled)
        File(v, ".obsidian/community-plugins.json").writeText("""["dataview","obsidian-livesync"]""")
        r = VaultCheck.inspect(v.path, root)
        assertTrue(r.liveSyncPluginEnabled)
        assertTrue(r.warnings.any { it.contains("LiveSync plugin") })
        assertTrue("warnings do not block", r.errors.isEmpty())
        assertEquals(listOf("notes"), VaultCheck.topFolders(v))
    }
}
