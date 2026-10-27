package no.nyta.livesync.service

/**
 * The profiles the first-sync wakelock is held for. The lock can go once
 * every one of them has caught up with the server or no longer runs; the
 * wakelock's own timeout bounds it either way.
 */
class FirstSyncWaits {
    private val waiting = mutableSetOf<String>()

    val isEmpty: Boolean @Synchronized get() = waiting.isEmpty()

    @Synchronized
    fun add(profileId: String) {
        waiting.add(profileId)
    }

    /**
     * Forget the profiles that caught up ([caughtUp]) or are no longer in
     * [active]. Returns true when none is left, so the lock can be released.
     */
    @Synchronized
    fun settle(active: Set<String>, caughtUp: (String) -> Boolean): Boolean {
        waiting.removeAll { it !in active || caughtUp(it) }
        return waiting.isEmpty()
    }

    @Synchronized
    fun clear() = waiting.clear()

    companion object {
        /**
         * Whether a profile's first fetch is still ahead or under way: it has
         * no index yet, or its engine never reached the live feed. The
         * engine's daemon heartbeat is written only once a catch-up through
         * `_changes` finished ("remaining 0"), in both modes.
         */
        fun owesFirstFetch(hasIndex: Boolean, heartbeatMs: Long?): Boolean = !hasIndex || heartbeatMs == null
    }
}
