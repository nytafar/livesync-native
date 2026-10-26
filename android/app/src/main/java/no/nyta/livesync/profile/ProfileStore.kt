package no.nyta.livesync.profile

import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import java.io.File
import java.io.IOException
import java.util.UUID

/**
 * The profiles, in one JSON file in app-private storage. Multi-profile under
 * the hood (the UI creates one). Refuses a profile whose vault folder is the
 * same as, inside or above another profile's: flock does not work on shared
 * storage, so this check is the guard (spec "Designed for multiple vaults").
 */
class ProfileStore(private val file: File, private val stateRoot: File) {
    class OverlapException(message: String) : Exception(message)

    @Serializable
    private data class Saved(val version: Int = 1, val profiles: List<Profile> = emptyList())

    private val json = Json { ignoreUnknownKeys = true; prettyPrint = true; encodeDefaults = true }
    private val lock = Any()

    fun all(): List<Profile> = synchronized(lock) { load().profiles }

    fun get(id: String): Profile? = all().firstOrNull { it.id == id }

    /** The folder for a profile's sync index (outside the vault). */
    fun stateDir(id: String): File = File(stateRoot, id)

    fun newId(): String = UUID.randomUUID().toString().replace("-", "").take(12)

    /** Add or replace a profile (matched by ID). */
    @Throws(OverlapException::class, IOException::class)
    fun put(profile: Profile) = synchronized(lock) {
        val saved = load()
        overlapping(profile, saved.profiles)?.let { other ->
            throw OverlapException(
                "The vault folder overlaps \"${other.name}\" (${other.vaultPath}). " +
                    "Two vaults cannot share a folder or sit inside each other."
            )
        }
        val others = saved.profiles.filter { it.id != profile.id }
        save(saved.copy(profiles = others + profile))
    }

    /** Remove a profile and its sync index. The vault folder is left alone. */
    fun remove(id: String) = synchronized(lock) {
        val saved = load()
        save(saved.copy(profiles = saved.profiles.filter { it.id != id }))
        stateDir(id).deleteRecursively()
    }

    fun update(id: String, change: (Profile) -> Profile): Profile? = synchronized(lock) {
        val p = get(id) ?: return null
        val updated = change(p).copy(id = p.id)
        put(updated)
        updated
    }

    /** The first other profile whose vault folder overlaps this one's. */
    fun overlapping(profile: Profile, others: List<Profile> = all()): Profile? =
        others.firstOrNull { it.id != profile.id && overlaps(it.vaultPath, profile.vaultPath) }

    private fun load(): Saved {
        if (!file.exists()) return Saved()
        return json.decodeFromString(Saved.serializer(), file.readText())
    }

    private fun save(saved: Saved) {
        file.parentFile?.mkdirs()
        val tmp = File(file.parentFile, file.name + ".tmp")
        tmp.writeText(json.encodeToString(Saved.serializer(), saved))
        if (!tmp.renameTo(file)) throw IOException("cannot replace ${file.path}")
    }

    companion object {
        /**
         * Canonical (links resolved, including the longest existing ancestor of a
         * folder that does not exist yet), lower-cased: shared storage folds case.
         */
        fun comparable(path: String): String {
            var base = File(path).absoluteFile.normalize()
            val rest = ArrayDeque<String>()
            while (!base.exists() && base.parentFile != null) {
                rest.addFirst(base.name)
                base = base.parentFile!!
            }
            var resolved = try {
                base.canonicalFile
            } catch (e: IOException) {
                base
            }
            for (seg in rest) resolved = File(resolved, seg)
            return resolved.path.lowercase().trimEnd('/')
        }

        /** Same folder, or one inside the other (component-wise). */
        fun overlaps(a: String, b: String): Boolean {
            val x = comparable(a)
            val y = comparable(b)
            return x == y || x.startsWith("$y/") || y.startsWith("$x/") || x.isEmpty() || y.isEmpty()
        }
    }
}
