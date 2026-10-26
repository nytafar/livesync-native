package no.nyta.livesync.service

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class StatusSummaryTest {
    private fun line(name: String = "hvelv", state: String = "Running", gate: String? = "1700000000000 ok: version 12", conflicted: Long = 0) =
        ProfileLine(name, false, null, state, null, true, gate, null, conflicted, null, 0)

    @Test
    fun oneProfile() {
        val t = StatusSummary.summarize(listOf(line()))
        assertEquals("LiveSync · hvelv", t.title)
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
    fun nudgesAreDebounced() {
        val g = NudgeGate(quietMs = 1000)
        assertTrue(g.shouldReport(true, false, 0))
        assertFalse(g.shouldReport(true, false, 500))
        assertTrue(g.shouldReport(false, false, 600))
        assertTrue(g.shouldReport(true, false, 700))
        assertTrue(g.shouldReport(true, false, 1800))
    }
}
