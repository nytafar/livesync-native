package no.nyta.livesync.service

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class FirstSyncTest {
    @Test
    fun aFirstFetchIsOwedUntilTheEngineReachedTheLiveFeed() {
        assertTrue(FirstSyncWaits.owesFirstFetch(hasIndex = false, heartbeatMs = null))
        assertTrue("an interrupted first fetch", FirstSyncWaits.owesFirstFetch(hasIndex = true, heartbeatMs = null))
        assertFalse(FirstSyncWaits.owesFirstFetch(hasIndex = true, heartbeatMs = 1_700_000_000_000))
    }

    @Test
    fun aFailedEngineDoesNotHoldTheWakelockThroughItsBackoff() {
        assertFalse(FirstSyncWaits.settled(hasIndex = false, heartbeatMs = null, failed = false))
        assertTrue("held by another engine, retrying later", FirstSyncWaits.settled(hasIndex = false, heartbeatMs = null, failed = true))
        assertTrue(FirstSyncWaits.settled(hasIndex = true, heartbeatMs = 1_700_000_000_000, failed = false))
    }

    @Test
    fun theLockGoesWhenEveryProfileCaughtUpOrStopped() {
        val w = FirstSyncWaits()
        assertTrue(w.settle(emptySet()) { false })
        w.add("a")
        w.add("b")
        val done = mutableSetOf<String>()
        assertFalse(w.settle(setOf("a", "b")) { it in done })
        done += "a"
        assertFalse("b is still fetching", w.settle(setOf("a", "b")) { it in done })
        // b was paused or removed before it caught up.
        assertTrue(w.settle(setOf("a")) { it in done })
        assertTrue(w.isEmpty)
        // A profile added later holds it again.
        w.add("c")
        assertFalse(w.settle(setOf("a", "c")) { it in done })
    }
}
