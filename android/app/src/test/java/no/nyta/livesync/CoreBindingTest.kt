package no.nyta.livesync

import no.nyta.livesync.core.GateOutcome
import no.nyta.livesync.core.LiveSync
import no.nyta.livesync.core.LiveSyncException
import no.nyta.livesync.core.RunState
import no.nyta.livesync.core.ScopeKind
import no.nyta.livesync.core.StatusListener
import no.nyta.livesync.core.checkScope
import no.nyta.livesync.core.coreVersion
import no.nyta.livesync.core.pathsOverlap
import no.nyta.livesync.profile.Profile
import no.nyta.livesync.profile.ScopeMode
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import java.util.concurrent.CopyOnWriteArrayList

/**
 * The generated Kotlin bindings against the real library, built for the host
 * (scripts/build-apk.sh builds it and sets livesync.requireHostLib=true, so
 * there these tests cannot be skipped). Records, enums, errors and the
 * callback interface all cross JNA here exactly as on the phone.
 */
class CoreBindingTest {
    @get:Rule
    val tmp = TemporaryFolder()

    @Before
    fun needsHostLib() {
        val lib = File(System.getProperty("livesync.hostLib") ?: "")
        if (System.getProperty("livesync.requireHostLib") == "true") {
            assertTrue("host library missing: $lib", lib.exists())
        }
        assumeTrue("host library not built: $lib", lib.exists())
    }

    private fun profile(vault: String) = Profile(
        id = "p1", name = "v", vaultPath = File(tmp.root, vault).path,
        // Nothing listens on port 9: the connect fails fast, no network needed.
        url = "http://127.0.0.1:9", database = "vault", username = "u",
        scopeMode = ScopeMode.EXCLUDE, scopeFolders = listOf("Private"),
    )

    @Test
    fun pureFunctionsCrossTheBoundary() {
        assertTrue(coreVersion().isNotEmpty())
        assertTrue(pathsOverlap("/a/b", "/A/b/c"))
        assertFalse(pathsOverlap("/a/b", "/a/bc"))
        assertEquals(listOf("notes", "work/x"), checkScope(ScopeKind.INCLUDE, listOf("Notes/", "work/x")))
        try {
            checkScope(ScopeKind.INCLUDE, listOf(".obsidian"))
            fail("dot-folder accepted")
        } catch (e: LiveSyncException.Invalid) {
            assertTrue(e.message!!.contains("dot-path"))
        }
    }

    @Test
    fun hostRunsAProfileAndReportsState() {
        val host = LiveSync(File(tmp.root, "logs").path)
        val seen = CopyOnWriteArrayList<RunState>()
        host.setListener(object : StatusListener {
            override fun onStateChanged(profileId: String, state: RunState, message: String?) {
                seen += state
            }
        })
        val p = profile("vault")
        val state = File(tmp.root, "state/p1").path
        val cfg = Core.toConfig(p, state, "secret-pw")
        assertEquals(ScopeKind.EXCLUDE, cfg.scopeKind)

        val gate = host.connectTest(cfg, File(tmp.root, "cache").path)
        assertEquals(gate.message, GateOutcome.UNREACHABLE, gate.outcome)

        host.start(cfg)
        assertEquals(listOf("p1"), host.activeProfiles())
        val deadline = System.currentTimeMillis() + 10_000
        while (RunState.RUNNING !in seen && System.currentTimeMillis() < deadline) Thread.sleep(50)
        assertTrue("listener saw $seen", RunState.RUNNING in seen)
        val st = host.status("p1", p.vaultPath, state)
        assertEquals(RunState.RUNNING, st.runState)
        assertTrue(st.pullOnly)
        host.syncNow("p1")
        host.networkChanged(true, false)

        // A second profile on the same folder is refused.
        try {
            host.start(cfg.copy(id = "p2", stateDir = File(tmp.root, "state/p2").path))
            fail("overlap accepted")
        } catch (e: LiveSyncException.Busy) {
            assertTrue(e.message!!.contains("overlaps"))
        }
        try {
            host.adoptTweaks("nope")
            fail()
        } catch (e: LiveSyncException.NotRunning) {
            // expected
        }
        try {
            host.allowMassDeleteOnce("p1")
            fail("a pull-only profile allowed mass deletes")
        } catch (e: LiveSyncException.Invalid) {
            // expected
        }
        assertFalse(st.massDeleteAllowed)
        assertEquals(0L, st.pushed)

        host.stop("p1")
        assertTrue(host.activeProfiles().isEmpty())
        assertEquals(RunState.STOPPED, seen.last())
        val stopped = host.status("p1", p.vaultPath, state)
        assertEquals(RunState.STOPPED, stopped.runState)
        assertTrue("the index exists after a start", stopped.hasIndex)
        assertTrue(stopped.lastError.orEmpty().isNotEmpty() || stopped.lastGate.orEmpty().contains("failed"))

        host.log("info", "binding test line")
        assertTrue(host.recentLogLines(50u).any { it.contains("binding test line") })
        val logText = host.logFiles().joinToString("") { File(it).readText() }
        assertFalse("the password never reaches the log", logText.contains("secret-pw"))
        host.close()
    }
}
