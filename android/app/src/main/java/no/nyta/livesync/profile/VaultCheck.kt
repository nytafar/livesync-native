package no.nyta.livesync.profile

import java.io.File

/** What a chosen vault folder looks like before syncing starts. */
data class VaultReport(
    val path: String,
    val exists: Boolean,
    val isDirectory: Boolean,
    val writable: Boolean,
    /** Non-dot files, counted up to [VaultCheck.COUNT_CAP]. */
    val files: Int,
    val isObsidianVault: Boolean,
    /** The LiveSync plugin is installed and enabled in this vault's Obsidian config. */
    val liveSyncPluginEnabled: Boolean,
    /** Blocking problems; empty when the folder can be used. */
    val errors: List<String>,
    val warnings: List<String>,
)

object VaultCheck {
    const val COUNT_CAP = 20_000

    /**
     * Inspect a folder. [sharedRoot] is the root of shared storage (the vault
     * must be inside it, and not in another app's private `Android/` folder).
     * [access] names the grant that makes it writable on this Android release
     * (`StorageAccess.phrase`): all-files access from Android 11, storage access below.
     */
    fun inspect(path: String, sharedRoot: File, access: String = "all-files access"): VaultReport {
        val dir = File(path)
        val errors = mutableListOf<String>()
        val warnings = mutableListOf<String>()
        val root = ProfileStore.comparable(sharedRoot.path)
        val target = ProfileStore.comparable(path)
        if (!target.startsWith("$root/")) {
            errors += "Choose a folder inside shared storage (${sharedRoot.path})"
        } else {
            val rel = target.removePrefix("$root/")
            if (rel == "android" || rel.startsWith("android/")) {
                errors += "Folders under Android/ belong to other apps and cannot be synced"
            }
            if (rel.split('/').any { it.startsWith(".") }) {
                errors += "The vault folder cannot be a dot-folder"
            }
        }
        val exists = dir.exists()
        val isDir = dir.isDirectory
        if (exists && !isDir) errors += "This is a file, not a folder"
        val writable = if (isDir) dir.canWrite() else dir.parentFile?.let { canCreateIn(it) } ?: false
        if (!writable && errors.isEmpty()) errors += "The folder cannot be written (is $access granted?)"
        val files = if (isDir) countFiles(dir) else 0
        val obsidian = File(dir, ".obsidian").isDirectory
        val pluginOn = obsidian && liveSyncEnabled(dir)
        if (pluginOn) {
            warnings += "The Self-hosted LiveSync plugin is enabled in this vault. Disable it in Obsidian " +
                "(Settings → Community plugins) before syncing here, or both will sync the same folder."
        }
        if (files > 0) {
            warnings += "The folder already holds ${if (files >= COUNT_CAP) "$COUNT_CAP+" else files} file(s). " +
                "Files identical to the remote are adopted; files that differ become conflicts, never overwrites."
        }
        return VaultReport(path, exists, isDir, writable, files, obsidian, pluginOn, errors, warnings)
    }

    private fun canCreateIn(dir: File): Boolean {
        var d: File? = dir
        while (d != null && !d.exists()) d = d.parentFile
        return d?.canWrite() ?: false
    }

    fun countFiles(dir: File): Int {
        var n = 0
        val stack = ArrayDeque<File>()
        stack.addLast(dir)
        while (stack.isNotEmpty() && n < COUNT_CAP) {
            val d = stack.removeLast()
            for (f in d.listFiles() ?: continue) {
                if (f.name.startsWith(".")) continue
                if (f.isDirectory) stack.addLast(f) else n++
            }
        }
        return n
    }

    /** `.obsidian/community-plugins.json` lists the plugin and its folder exists. */
    fun liveSyncEnabled(vault: File): Boolean {
        val list = File(vault, ".obsidian/community-plugins.json")
        val plugin = File(vault, ".obsidian/plugins/obsidian-livesync")
        if (!plugin.isDirectory) return false
        if (!list.exists()) return false
        return try {
            list.readText().contains("\"obsidian-livesync\"")
        } catch (e: Exception) {
            true // Cannot tell: warn.
        }
    }

    /** Top-level non-dot folders, for picking scope folders. */
    fun topFolders(vault: File): List<String> =
        (vault.listFiles() ?: emptyArray())
            .filter { it.isDirectory && !it.name.startsWith(".") }
            .map { it.name }
            .sortedBy { it.lowercase() }
}
