package no.nyta.livesync.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import no.nyta.livesync.LiveSyncApp
import no.nyta.livesync.core.ProfileStatus
import no.nyta.livesync.core.RunState
import no.nyta.livesync.profile.Profile
import no.nyta.livesync.service.StatusSummary

/**
 * The home screen: every vault with one line of status, and "Add vault". A
 * tap opens the vault's status screen. It reads the statuses every 2 s only
 * while the activity is started, like the status screen: with the screen
 * off or the app in the background nothing polls.
 */
@Composable
fun VaultListScreen(app: LiveSyncApp, onOpen: (String) -> Unit, onAdd: () -> Unit) {
    val core = app.core.getOrThrow()
    val checks = rememberDeviceChecks()
    var rows by remember { mutableStateOf<List<Pair<Profile, ProfileStatus?>>>(emptyList()) }

    PollWhileStarted(Unit, 2000) {
        rows = withContext(Dispatchers.IO) {
            runCatching { app.profiles.all() }.getOrDefault(emptyList()).map { p ->
                p to runCatching { core.status(p) }.getOrNull()
            }
        }
    }

    Column(Modifier.fillMaxSize().safeDrawingPadding().verticalScroll(rememberScrollState()).padding(16.dp)) {
        Text("LiveSync", style = MaterialTheme.typography.headlineSmall)
        Text(
            "Each vault is a folder on this phone kept in sync with a CouchDB database (or part of one).",
            style = MaterialTheme.typography.bodySmall,
        )
        Spacer(Modifier.height(8.dp))
        if (!checks.essentialsOk || !checks.notifications || !checks.samsungSleepOk || !checks.booxOk) {
            SetupChecksCard(checks)
        }
        rows.forEach { (p, st) ->
            VaultRow(p, st, app.startProblems[p.id]) { onOpen(p.id) }
        }
        Row(Modifier.fillMaxWidth().padding(vertical = 12.dp), horizontalArrangement = Arrangement.End) {
            Button(onClick = onAdd) { Text("Add vault") }
        }
        Text(
            "Vault folders may not be the same, or inside each other. Two vaults may sync different folders of " +
                "the same database; each is its own device to the other LiveSync clients.",
            style = MaterialTheme.typography.bodySmall,
        )
    }
}

@Composable
private fun VaultRow(p: Profile, st: ProfileStatus?, startProblem: String?, onClick: () -> Unit) {
    val (phrase, alert) = StatusSummary.phrase(StatusSummary.line(p, st, startProblem))
    val tone = when {
        alert -> Tone.ERROR
        p.paused -> Tone.INFO
        st?.runState == RunState.RUNNING -> Tone.OK
        else -> Tone.INFO
    }
    Card(Modifier.fillMaxWidth().padding(vertical = 4.dp).clickable(onClick = onClick)) {
        Column(Modifier.padding(16.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(p.name, Modifier.weight(1f), style = MaterialTheme.typography.titleMedium)
                Text(if (p.pullOnly) "pull-only" else "two-way", style = MaterialTheme.typography.labelMedium)
            }
            Text(p.vaultPath, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
            Text(phrase, color = toneColor(tone), style = MaterialTheme.typography.bodyMedium)
        }
    }
}
