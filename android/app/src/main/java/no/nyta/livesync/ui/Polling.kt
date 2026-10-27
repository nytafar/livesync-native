package no.nyta.livesync.ui

import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.rememberUpdatedState
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map

/**
 * Run [block] at once and then every [periodMs] while [state] is at least
 * STARTED; stop as soon as it drops below (screen off, Home, another app)
 * and start again when it comes back. The foreground service keeps the
 * process alive, so a plain loop in a composable would go on reading the
 * statuses for nobody.
 */
suspend fun pollWhileStarted(state: StateFlow<Lifecycle.State>, periodMs: Long, block: suspend () -> Unit) {
    state.map { it.isAtLeast(Lifecycle.State.STARTED) }.distinctUntilChanged().collectLatest { started ->
        if (started) {
            while (true) {
                block()
                delay(periodMs)
            }
        }
    }
}

/** [pollWhileStarted] for a screen: restarts when [key] changes, ends when the screen leaves. */
@Composable
fun PollWhileStarted(key: Any?, periodMs: Long, block: suspend () -> Unit) {
    val lifecycle = LocalLifecycleOwner.current.lifecycle
    val current by rememberUpdatedState(block)
    LaunchedEffect(key, lifecycle) {
        pollWhileStarted(lifecycle.currentStateFlow, periodMs) { current() }
    }
}
