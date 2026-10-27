package no.nyta.livesync.profile

import android.annotation.SuppressLint
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import java.io.File
import java.io.IOException
import java.util.UUID

/**
 * The profiles (one per vault), in one JSON file in app-private storage.
 * Refuses a profile whose vault folder is the same as, inside or above
 * another profile's: flock does not work on shared storage, and the engine's
 * host lock only catches the same folder, not one inside another, so this
 * check is the guard (spec "Designed for multiple vaults").
 *
 * Two profiles may name the same server and database (different scopes, or
 * different vault folders): each is its own node with its own index, state
 * folder and Keystore alias, all keyed by the profile ID.
 */
class ProfileStore(private val file: File, private val stateRoot: File) {
    class OverlapException(message: String) : Exception(message)

    /** How a vault folder lies against another profile's. */
    enum class Overlap { SAME, INSIDE, ABOVE }

    /** A vault folder that overlaps [other]'s, and how. */
    data class Clash(val other: Profile, val kind: Overlap) {
        val message: String
            get() = when (kind) {
                Overlap.SAME -> "This is already the vault folder of \"${other.name}\"."
                Overlap.INSIDE -> "This folder is inside the vault folder of \"${other.name}\" (${other.vaultPath})."
                Overlap.ABOVE -> "This folder contains the vault folder of \"${other.name}\" (${other.vaultPath})."
            } + " Two vaults cannot share a folder or sit inside each other."
    }

    @Serializable
    private data class Saved(val version: Int = 1, val profiles: List<Profile> = emptyList())

    private val json = Json { ignoreUnknownKeys = true; prettyPrint = true; encodeDefaults = true }
    private val lock = Any()

    /** Every profile, in the order they were added. */
    fun all(): List<Profile> = synchronized(lock) { load().profiles }

    fun get(id: String): Profile? = all().firstOrNull { it.id == id }

    /** The folder for a profile's sync index (outside the vault). */
    fun stateDir(id: String): File = File(stateRoot, id)

    fun newId(): String = UUID.randomUUID().toString().replace("-", "").take(12)

    /** Add a profile, or replace the one with its ID in place. */
    @Throws(OverlapException::class, IOException::class)
    fun put(profile: Profile) = synchronized(lock) {
        val saved = load()
        clash(profile, saved.profiles)?.let { throw OverlapException(it.message) }
        val replaced = saved.profiles.any { it.id == profile.id }
        val profiles = if (replaced) {
            saved.profiles.map { if (it.id == profile.id) profile else it }
        } else {
            saved.profiles + profile
        }
        save(saved.copy(profiles = profiles))
    }

    /**
     * Remove a profile and its sync index. The vault folder is left alone,
     * and so is every other profile.
     * Edits the engine had to set aside (`<state>/recovered/`, possibly their
     * only copy) are first copied into [keepRecoveredIn]; if that fails,
     * nothing is removed. Returns the folder they went to, if there were any.
     */
    @Throws(IOException::class)
    fun remove(id: String, keepRecoveredIn: File): File? = synchronized(lock) {
        val recovered = File(stateDir(id), "recovered")
        val kept = if (recovered.walkTopDown().any { it.isFile }) {
            val target = File(keepRecoveredIn, "recovered-$id-${System.currentTimeMillis()}")
            if (!recovered.copyRecursively(target, overwrite = false) ||
                recovered.walkTopDown().count { it.isFile } != target.walkTopDown().count { it.isFile }
            ) {
                throw IOException("could not keep the set-aside edits in ${target.path}")
            }
            target
        } else {
            null
        }
        val saved = load()
        save(saved.copy(profiles = saved.profiles.filter { it.id != id }))
        stateDir(id).deleteRecursively()
        kept
    }

    fun update(id: String, change: (Profile) -> Profile): Profile? = synchronized(lock) {
        val p = get(id) ?: return null
        val updated = change(p).copy(id = p.id)
        put(updated)
        updated
    }

    /** The first other profile whose vault folder overlaps this one's. */
    fun overlapping(profile: Profile, others: List<Profile> = all()): Profile? = clash(profile, others)?.other

    /** The first other profile whose vault folder overlaps this one's, and how. */
    fun clash(profile: Profile, others: List<Profile> = all()): Clash? {
        for (o in others) {
            if (o.id == profile.id) continue
            relation(profile.vaultPath, o.vaultPath)?.let { return Clash(o, it) }
        }
        return null
    }

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
         * Other names of the primary shared storage, as links (`/sdcard`)
         * point. Only a spelling, used when the link itself did not resolve:
         * on the phone canonicalisation follows it (for any user) first.
         */
        @SuppressLint("SdCardPath")
        private val SHARED_ALIASES = listOf("/sdcard", "/mnt/sdcard", "/storage/self/primary")
        private const val SHARED = "/storage/emulated/0"

        /**
         * Canonical (links resolved, including the longest existing ancestor of a
         * folder that does not exist yet), lower-cased (shared storage folds
         * case, and refusing two spellings elsewhere is harmless), without a
         * trailing slash. A shared-storage alias the links did not resolve
         * (no such link here, or not readable) is spelt `/storage/emulated/0`.
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
            val p = resolved.path.trimEnd('/').lowercase()
            val alias = SHARED_ALIASES.firstOrNull { p == it || p.startsWith("$it/") } ?: return p
            return SHARED + p.removePrefix(alias)
        }

        /** How folder [a] lies against folder [b] (component-wise), or null when apart. */
        fun relation(a: String, b: String): Overlap? {
            val x = comparable(a)
            val y = comparable(b)
            return when {
                x == y -> Overlap.SAME
                y.isEmpty() || x.startsWith("$y/") -> Overlap.INSIDE
                x.isEmpty() || y.startsWith("$x/") -> Overlap.ABOVE
                else -> null
            }
        }

        /** Same folder, or one inside the other (component-wise). */
        fun overlaps(a: String, b: String): Boolean = relation(a, b) != null
    }
}
