package no.nyta.livesync.service

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities

/**
 * Follows the default network and tells the engines when it comes back
 * (reconnect at once, rescan) instead of letting them sit in a backoff sleep
 * (spec pull loop step 5, "Nudge" seam). Only real transitions are passed on
 * ([NetworkGate]).
 */
class NetworkMonitor(
    context: Context,
    private val onChange: (up: Boolean, metered: Boolean) -> Unit,
) {
    private val cm = context.getSystemService(ConnectivityManager::class.java)
    private val gate = NetworkGate()
    private var registered = false

    private val callback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            // Usually followed at once by onCapabilitiesChanged; without
            // capabilities yet, that callback reports it.
            cm.getNetworkCapabilities(network)?.let { report(network, it) }
        }

        override fun onCapabilitiesChanged(network: Network, caps: NetworkCapabilities) {
            report(network, caps)
        }

        override fun onLost(network: Network) {
            if (gate.lost(network.networkHandle)) onChange(false, false)
        }
    }

    private fun report(network: Network, caps: NetworkCapabilities) {
        val validated = caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED)
        val metered = !caps.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)
        // A VPN that stays the default network carries its underlying
        // network's transports (VPN plus Wi-Fi or cellular): a switch
        // underneath shows up here.
        var transports = 0L
        for (t in 0..MAX_TRANSPORT) if (caps.hasTransport(t)) transports = transports or (1L shl t)
        if (gate.seen(network.networkHandle, transports, validated, metered)) onChange(true, metered)
    }

    fun start() {
        if (registered) return
        cm.registerDefaultNetworkCallback(callback)
        registered = true
    }

    private companion object {
        /** The highest transport constant (TRANSPORT_SATELLITE, API 35); unknown ones read as absent. */
        const val MAX_TRANSPORT = 10
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
 * Passes on only real transitions of the default network. Capability updates
 * for the same network arrive all the time (signal strength, bandwidth, a
 * VPN's link details), and each one used to nudge every engine and log
 * "network up". A transition is: a different network becoming the default,
 * its transports changing (a VPN's underlying network switching between
 * Wi-Fi and cellular), the current one becoming validated (usable) or
 * changing between metered and unmetered, and losing it.
 */
class NetworkGate {
    private var network: Long? = null
    private var transports = 0L
    private var validated = false
    private var metered = false

    /**
     * A capability report for [handle] with its [transports] bit set; true
     * when it is a transition to pass on as "up".
     */
    @Synchronized
    fun seen(handle: Long, transports: Long, validated: Boolean, metered: Boolean): Boolean {
        val transition = handle != network || transports != this.transports ||
            (validated && !this.validated) || metered != this.metered
        network = handle
        this.transports = transports
        this.validated = validated
        this.metered = metered
        return transition
    }

    /** [handle] was lost; true when it was the current network (a stale loss is ignored). */
    @Synchronized
    fun lost(handle: Long): Boolean {
        if (handle != network) return false
        network = null
        validated = false
        return true
    }
}
