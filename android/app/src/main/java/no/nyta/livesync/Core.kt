package no.nyta.livesync

import no.nyta.livesync.core.FolderList
import no.nyta.livesync.core.GateResult
import no.nyta.livesync.core.LiveSync
import no.nyta.livesync.core.ProfileConfig
import no.nyta.livesync.core.ProfileStatus
import no.nyta.livesync.core.ScopeKind
import no.nyta.livesync.profile.Profile
import no.nyta.livesync.profile.ProfileStore
import no.nyta.livesync.profile.ScopeMode
import no.nyta.livesync.secret.SecretStore
import java.io.File

/**
 * Serialises everything that decides which engines run: the service's
 * reconcile and teardown, and removing a profile. Each takes it for its whole
 * read-decide-act sequence, so none acts on settings another has made stale.
 * Never taken on the main thread (a stop can wait for tens of seconds).
 */
object EngineLifecycle {
    val lock = Any()
}

/**
 * The Rust engine host plus the glue from a [Profile] (and its secret) to the
 * engine's [ProfileConfig]. One per process. Every call may block (opening or
 * stopping an engine waits for disk and network): call it off the main thread.
 */
class Core(
    private val host: LiveSync,
    private val profiles: ProfileStore,
    private val secrets: SecretStore,
    private val scratchDir: File,
) {
    /** Thrown when a profile cannot be started because its password is gone. */
    class MissingSecretException(message: String) : Exception(message)

    val engine: LiveSync get() = host

    fun configFor(p: Profile, password: String? = null): ProfileConfig {
        val pw = password ?: secrets.get(p.id, SecretStore.PASSWORD)
            ?: throw MissingSecretException("No stored password for \"${p.name}\"; enter it again in Settings")
        return toConfig(p, profiles.stateDir(p.id).path, pw)
    }

    fun connectTest(p: Profile, password: String): GateResult =
        host.connectTest(toConfig(p, profiles.stateDir(p.id).path, password), scratchDir.path)

    /**
     * The server's top-level folders (files and size each) for the scope
     * picker, with the vault's local-only folders marked. Reads metadata only
     * and never writes to the server. Throws `LiveSyncException`.
     */
    fun listFolders(p: Profile, password: String): FolderList =
        host.listFolders(toConfig(p, profiles.stateDir(p.id).path, password))

    fun status(p: Profile): ProfileStatus = host.status(p.id, p.vaultPath, profiles.stateDir(p.id).path)

    fun log(message: String, level: String = "info") {
        try {
            host.log(level, message)
        } catch (e: Exception) {
            // Logging must never take the caller down.
        }
    }

    companion object {
        fun toConfig(p: Profile, stateDir: String, password: String) = ProfileConfig(
            id = p.id,
            vaultPath = p.vaultPath,
            stateDir = stateDir,
            url = p.url.trim(),
            database = p.database.trim(),
            username = p.username.trim(),
            password = password,
            scopeKind = when (p.scopeMode) {
                ScopeMode.ALL -> ScopeKind.ALL
                ScopeMode.INCLUDE -> ScopeKind.INCLUDE
                ScopeMode.EXCLUDE -> ScopeKind.EXCLUDE
            },
            scopeFolders = if (p.scopeMode == ScopeMode.ALL) emptyList() else p.scopeFolders,
            pullOnly = p.pullOnly,
            deviceName = p.deviceName,
            ignoreRegex = "",
        )
    }
}
