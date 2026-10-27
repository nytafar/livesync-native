package no.nyta.livesync.profile

import kotlinx.serialization.Serializable
import java.net.URI

/** Folder-scope mode, as the engine's `ScopeKind`. */
@Serializable
enum class ScopeMode { ALL, INCLUDE, EXCLUDE }

/**
 * One synced vault. The password is not here: it lives in the SecretStore
 * under the profile's ID.
 */
@Serializable
data class Profile(
    val id: String,
    /** Shown in the UI; defaults to the vault folder's name. */
    val name: String,
    /** Absolute path of the vault folder in shared storage. */
    val vaultPath: String,
    val url: String,
    val database: String,
    val username: String,
    val scopeMode: ScopeMode = ScopeMode.ALL,
    val scopeFolders: List<String> = emptyList(),
    /** Follow the remote read-only (the safe way to begin). */
    val pullOnly: Boolean = true,
    /** Paused by the user: no engine runs for it. */
    val paused: Boolean = false,
    /** Device name advertised to the other LiveSync clients. */
    val deviceName: String = "",
    val createdAtMs: Long = 0,
)

object ProfileRules {
    /** Problems with a server URL, or null when it is usable. */
    fun urlProblem(url: String): String? {
        val u = url.trim()
        if (u.isEmpty()) return "Enter the server URL"
        val parsed = try {
            URI(u)
        } catch (e: Exception) {
            return "Not a valid URL"
        }
        val scheme = parsed.scheme?.lowercase()
        if (scheme != "https" && scheme != "http") return "The URL must start with https:// or http://"
        if (parsed.host.isNullOrEmpty()) return "The URL has no host name"
        if (parsed.rawUserInfo != null) return "Put the user name and password in their own fields, not in the URL"
        return null
    }

    /** Whether plain http:// is used for a host that is not on the tailnet or the LAN. */
    fun insecureUrl(url: String): Boolean {
        val parsed = try {
            URI(url.trim())
        } catch (e: Exception) {
            return false
        }
        if (parsed.scheme?.lowercase() != "http") return false
        val host = parsed.host ?: return false
        return !(host.endsWith(".ts.net") || host.startsWith("100.") || host.startsWith("192.168.") ||
            host.startsWith("10.") || host == "localhost" || host.startsWith("127."))
    }

    /**
     * Split a folder list typed one per line or separated by commas. Each
     * entry is saved as the engine will read it, so what the picker ticks is
     * what syncs: `/` and `\` separate, empty segments drop out, and spaces
     * around each segment are typing slack (`/ Notes` is `Notes`, never
     * ` Notes`). A folder name that really starts or ends with a space can
     * therefore not be listed (the picker says so).
     */
    fun parseFolders(text: String): List<String> =
        text.split('\n', ',').map { normaliseFolder(it) }.filter { it.isNotEmpty() }

    /** One typed folder entry as it is saved (see [parseFolders]). */
    fun normaliseFolder(entry: String): String =
        entry.split('/', '\\').map { it.trim() }.filter { it.isNotEmpty() }.joinToString("/")

    /** Problems with a folder scope, or null. Mirrors the engine's rules. */
    fun scopeProblem(mode: ScopeMode, folders: List<String>): String? {
        if (mode == ScopeMode.ALL) return null
        if (folders.isEmpty()) return "List at least one folder"
        for (f in folders) {
            val segs = f.split('/', '\\').filter { it.isNotEmpty() }
            if (segs.any { it == "." || it == ".." }) return "\"$f\" contains . or .."
            if (segs.any { it.startsWith(".") }) return "\"$f\" is a dot-folder; dot-folders never sync"
        }
        return null
    }

    fun defaultName(vaultPath: String): String =
        vaultPath.trimEnd('/').substringAfterLast('/').ifEmpty { "vault" }
}
