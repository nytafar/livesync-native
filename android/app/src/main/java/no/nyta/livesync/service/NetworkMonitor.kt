package no.nyta.livesync.service

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities

/**
 * Follows the default network and tells the engines when it comes back
 * (reconnect at once, rescan) instead of letting them sit in a backoff sleep
 * (spec pull loop step 5, "Nudge" seam).
 */
class NetworkMonitor(
    context: Context,
    private val onChange: (up: Boolean, metered: Boolean) -> Unit,
) {
    private val cm = context.getSystemService(ConnectivityManager::class.java)
    private val gate = NudgeGate()
    private var registered = false

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            val caps = cm.getNetworkCapabilities(network)
            report(true, caps)
        }

        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
            // A network often becomes usable (validated) a moment after it appears.
            if (caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)) report(true, caps)
        }

        override fun onLost(network: Network) {
            report(false, null)
        }
    }

    private fun report(up: Boolean, caps: NetworkCapabilities?) {
        val metered = caps?.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED) == false
        if (gate.shouldReport(up, metered, System.currentTimeMillis())) onChange(up, metered)
    }

    fun start() {
        if (registered) return
        cm.registerDefaultNetworkCallback(callback)
        registered = true
    }

    fun stop() {
        if (!registered) return
        try {
            cm.unregisterNetworkCallback(callback)
        } catch (e: IllegalArgumentException) {
            // Already unregistered.
        }
        registered = false
    }
}

/**
 * Drops repeats: the same state reported again within [quietMs] is not passed
 * on (capability updates arrive in bursts).
 */
class NudgeGate(private val quietMs: Long = 3_000) {
    private var last: Pair<Boolean, Boolean>? = null
    private var lastAt = 0L

    @Synchronized
    fun shouldReport(up: Boolean, metered: Boolean, now: Long): Boolean {
        val state = up to metered
        if (state == last && now - lastAt < quietMs) return false
        last = state
        lastAt = now
        return true
    }
}
