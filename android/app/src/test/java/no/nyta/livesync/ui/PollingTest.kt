package no.nyta.livesync.ui

import androidx.lifecycle.Lifecycle
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.atomic.AtomicInteger

class PollingTest {
    @Test
    fun pollsOnlyWhileTheScreenIsStarted() = runBlocking {
        val state = MutableStateFlow(Lifecycle.State.CREATED)
        val reads = AtomicInteger()
        val job = launch(Dispatchers.Default) { pollWhileStarted(state, 10) { reads.incrementAndGet() } }
        delay(100)
        assertEquals("not started yet: no reads", 0, reads.get())

        state.value = Lifecycle.State.STARTED
        delay(100)
        assertTrue("started: it polls", reads.get() >= 3)
        state.value = Lifecycle.State.RESUMED
        delay(50)

        // Screen off or Home: the activity is stopped, the process lives on.
        state.value = Lifecycle.State.CREATED
        delay(30)
        val stopped = reads.get()
        delay(150)
        assertEquals("stopped: no reads for nobody", stopped, reads.get())

        state.value = Lifecycle.State.STARTED
        delay(100)
        assertTrue("back: it polls again", reads.get() > stopped)
        job.cancel()
    }
}
