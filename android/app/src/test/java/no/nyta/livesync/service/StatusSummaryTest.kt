package no.nyta.livesync.service

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class StatusSummaryTest {
    private fun line(name: String = "notes", state: String = "Running", gate: String? = "1700000000000 ok: version 12", conflicted: Long = 0) =
        ProfileLine(name, false, null, state, null, true, gate, null, conflicted, null, 0)

    @Test
    fun oneProfile() {
        val t = StatusSummary.summarize(listOf(line()))
        assertEquals("LiveSync · notes", t.title)
        assertEquals("pull-only", t.text)
        assertFalse(t.alert)
    }

    @Test
    fun refusalsErrorsAndConflictsAlert() {
        val refused = StatusSummary.summarize(listOf(line(gate = "1700000000000 failed: refused: milestone locked")))
        assertTrue(refused.alert)
        assertEquals("not syncing: refused: milestone locked", refused.text)
        val failed = StatusSummary.summarize(listOf(line(state = "Failed").copy(runMessage = "index: disk I/O error")))
        assertTrue(failed.text.startsWith("error: index: disk I/O error"))
        assertTrue(StatusSummary.summarize(listOf(line(conflicted = 2))).text.contains("2 conflicted"))
        val missing = StatusSummary.summarize(listOf(line().copy(startProblem = "No stored password")))
        assertTrue(missing.alert)
    }

    @Test
    fun rebuildWaitsOrphansAndDeletionsElsewhereAreNamed() {
        val wait = StatusSummary.phrase(line().copy(rebuildWait = "1700000000000 waiting 30 s"))
        assertEquals("waiting for a rebuild elsewhere to finish", wait.first)
        assertFalse(wait.second)
        val orphans = StatusSummary.phrase(line().copy(rebuildOrphans = "1700000000000 2 file(s): a.md, b.md"))
        assertTrue(orphans.first.contains("pushed back"))
        assertTrue(orphans.second)
        val kept = StatusSummary.phrase(line().copy(deletedElsewhere = 3))
        assertTrue(kept.first.contains("3 kept after a delete elsewhere"))
        assertTrue(kept.second)
        val stop = StatusSummary.phrase(line().copy(safetyStop = "1700000000000 25 of 30 tracked files are missing"))
        assertEquals("safety stop: 25 of 30 tracked files are missing", stop.first)
    }

    @Test
    fun severalProfilesShareOneNotification() {
        val t = StatusSummary.summarize(listOf(line("a"), line("b").copy(paused = true)))
        assertEquals("LiveSync · 2 vaults", t.title)
        assertEquals("a: pull-only\nb: paused", t.text)
    }

    @Test
    fun onlyRealNetworkTransitionsArePassedOn() {
        val wifi = 1L shl 1
        val cell = 1L shl 0
        val g = NetworkGate()
        assertTrue("a first network", g.seen(1, wifi, validated = false, metered = false))
        assertTrue("it became usable", g.seen(1, wifi, validated = true, metered = false))
        // The phone run: the same validated network reported over and over.
        repeat(4) { assertFalse(g.seen(1, wifi, validated = true, metered = false)) }
        assertFalse("losing validation is not an up", g.seen(1, wifi, validated = false, metered = false))
        assertTrue("validated again", g.seen(1, wifi, validated = true, metered = false))
        assertTrue("switched to mobile data", g.seen(2, cell, validated = true, metered = true))
        assertFalse("a stale loss of the old network", g.lost(1))
        assertTrue(g.lost(2))
        assertFalse("lost twice", g.lost(2))
        assertTrue("back on the same network", g.seen(2, cell, validated = true, metered = true))
        assertTrue("metered changed", g.seen(2, cell, validated = true, metered = false))
    }

    @Test
    fun aSwitchBeneathAVpnIsATransition() {
        val wifi = 1L shl 1
        val cell = 1L shl 0
        val vpn = 1L shl 4
        val g = NetworkGate()
        assertTrue(g.seen(7, vpn or wifi, validated = true, metered = false))
        assertFalse(g.seen(7, vpn or wifi, validated = true, metered = false))
        // Same VPN network, same flags, now over cellular.
        assertTrue(g.seen(7, vpn or cell, validated = true, metered = false))
    }

    @Test
    fun theTextHoldsStillBetweenRefreshes() {
        // Issue #3: the minute refresh re-posted an unchanged notification.
        val now = 1_761_550_000_000L
        val l = line().copy(lastActivityMs = now - 5 * 60_000)
        assertEquals(StatusSummary.summarize(listOf(l), now), StatusSummary.summarize(listOf(l), now + 60_000))
        assertEquals(StatusSummary.summarize(listOf(l), now), StatusSummary.summarize(listOf(l), now + 30 * 60_000))
    }

    @Test
    fun onlyAChangedTextIsPosted() {
        val g = PostGate<StatusSummary.Text>()
        val posts = mutableListOf<String>()
        val post = { t: StatusSummary.Text -> posts += t.text; true }
        val a = StatusSummary.Text("LiveSync · notes", "pull-only · last change 14:02", false)
        g.post(a, post = post)
        repeat(3) { g.post(a.copy(), post = post) }
        assertEquals("an unchanged refresh is not posted", listOf(a.text), posts)
        val b = a.copy(text = "pull-only · last change 14:07")
        g.post(b, post = post)
        g.post(b, post = post)
        assertEquals(listOf(a.text, b.text), posts)
        // startForeground always posts; the record follows it.
        g.post(a, always = true, post = post)
        g.post(b, post = post)
        assertEquals(listOf(a.text, b.text, a.text, b.text), posts)
        // A post that did not land is retried.
        g.post(a) { false }
        g.post(a, post = post)
        assertEquals(b.text, posts[3])
        assertEquals(a.text, posts.last())
    }
}
