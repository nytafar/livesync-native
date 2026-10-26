package no.nyta.livesync.ui

import android.widget.Toast
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import no.nyta.livesync.LiveSyncApp
import no.nyta.livesync.core.GateResult
import no.nyta.livesync.core.ProfileStatus
import no.nyta.livesync.core.RunState
import no.nyta.livesync.profile.Profile
import no.nyta.livesync.service.StatusSummary
import no.nyta.livesync.service.SyncService

private const val LOG_LINES = 200u

/** The status and debug screen (spec test strategy "On device"). */
@Composable
fun StatusScreen(app: LiveSyncApp, profileId: String, onEdit: () -> Unit, onChanged: () -> Unit, onRemoved: () -> Unit) {
    val ctx = LocalContext.current
    val scope = rememberCoroutineScope()
    val core = app.core.getOrThrow()
    val checks = rememberDeviceChecks()
    var profile by remember { mutableStateOf(app.profiles.get(profileId)) }
    var status by remember { mutableStateOf<ProfileStatus?>(null) }
    var logs by remember { mutableStateOf<List<String>>(emptyList()) }
    var confirm by remember { mutableStateOf<Confirm?>(null) }
    var gateResult by remember { mutableStateOf<GateResult?>(null) }
    var busy by remember { mutableStateOf<String?>(null) }
    var showRaw by remember { mutableStateOf(false) }

    LaunchedEffect(profileId) {
        while (true) {
            val p = withContext(Dispatchers.IO) { app.profiles.get(profileId) }
            profile = p
            if (p != null) {
                status = withContext(Dispatchers.IO) { runCatching { core.status(p) }.getOrNull() }
                logs = withContext(Dispatchers.IO) { runCatching { core.engine.recentLogLines(LOG_LINES) }.getOrDefault(logs) }
            }
            delay(2000)
        }
    }
    val p = profile ?: return

    fun update(reason: String, change: (Profile) -> Profile) {
        scope.launch {
            profile = withContext(Dispatchers.IO) {
                runCatching { app.profiles.update(p.id, change) }.getOrNull() ?: app.profiles.get(p.id)
            }
            SyncService.start(ctx, reason)
            onChanged()
        }
    }

    Column(Modifier.fillMaxSize().safeDrawingPadding().verticalScroll(rememberScrollState()).padding(16.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Column(Modifier.weight(1f)) {
                Text("LiveSync · ${p.name}", style = MaterialTheme.typography.headlineSmall)
                Text(p.vaultPath, style = MaterialTheme.typography.bodySmall)
            }
            TextButton(onClick = onEdit) { Text("Settings") }
        }

        if (!checks.essentialsOk || !checks.notifications || (checks.samsung && !checks.samsungNeverSleepingConfirmed)) {
            SetupChecksCard(checks)
        }

        val st = status
        SectionCard("Status") {
            val (phrase, alert) = StatusSummary.phrase(
                no.nyta.livesync.service.ProfileLine(
                    name = p.name, paused = p.paused, startProblem = null,
                    runState = st?.runState?.name?.lowercase()?.replaceFirstChar { it.uppercase() } ?: "Stopped",
                    runMessage = st?.runMessage, pullOnly = p.pullOnly, lastGate = st?.lastGate,
                    lastActivityMs = listOfNotNull(st?.lastPullMs, st?.lastPushMs).maxOrNull(),
                    conflicted = st?.conflicted ?: 0, safetyStop = st?.safetyStop,
                    needsAttention = st?.attention?.size?.toLong() ?: 0,
                )
            )
            Banner(phrase, if (alert) Tone.ERROR else if (st?.runState == RunState.RUNNING) Tone.OK else Tone.INFO)
            st?.safetyStop?.let { Banner("Safety stop: ${it.substringAfter(' ')}", Tone.ERROR) }
            st?.lastError?.let { Banner("Last error: ${fmtStamped(it)}", Tone.WARN) }
            if (st?.needsFullFetch == true) Banner("A full fetch is owed; it runs at the next sync.", Tone.INFO)
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text(if (p.pullOnly) "Pull only (read-only)" else "Two-way")
                    Text(
                        if (p.pullOnly) "Edits on this phone are not sent to the server." else "Edits on this phone are pushed to the server.",
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
                Switch(checked = !p.pullOnly, onCheckedChange = { twoWay ->
                    if (twoWay) confirm = Confirm.TwoWay else update("switched to pull-only") { it.copy(pullOnly = true) }
                })
            }
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("Paused", Modifier.weight(1f))
                Switch(checked = p.paused, onCheckedChange = { paused ->
                    update(if (paused) "paused" else "resumed") { it.copy(paused = paused) }
                })
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                Button(enabled = !p.paused, onClick = {
                    SyncService.syncNow(ctx, p.id)
                    Toast.makeText(ctx, "Reconnecting and rescanning", Toast.LENGTH_SHORT).show()
                }) { Text("Sync now") }
                OutlinedButton(onClick = {
                    busy = "Exporting logs…"
                    scope.launch {
                        val f = withContext(Dispatchers.IO) { runCatching { LogExport.export(ctx) } }
                        busy = null
                        f.onSuccess {
                            Toast.makeText(ctx, "Saved ${it.path}", Toast.LENGTH_LONG).show()
                            runCatching { LogExport.share(ctx, it) }
                        }.onFailure { Toast.makeText(ctx, "Export failed: ${it.message}", Toast.LENGTH_LONG).show() }
                    }
                }) { Text("Export logs") }
            }
            busy?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
        }

        SectionCard("Sync") {
            KeyValue("Engine", st?.let { "${it.runState.name.lowercase()}${it.runMessage?.let { m -> " — $m" } ?: ""}" })
            KeyValue("Last gate", fmtStamped(st?.lastGate))
            KeyValue("Gate passed", fmtTime(st?.lastGateOkMs))
            KeyValue("since", st?.since)
            KeyValue("Last pull", fmtTime(st?.lastPullMs))
            KeyValue("Last push", fmtTime(st?.lastPushMs))
            KeyValue("Registered", fmtTime(st?.registeredMs))
            KeyValue("Alive", fmtTime(st?.daemonHeartbeatMs))
            st?.rebuildDetectedMs?.let { KeyValue("Rebuild seen", fmtTime(it)) }
            KeyValue("Folders", st?.scope ?: p.scopeMode.name.lowercase())
            KeyValue("Remote", st?.remote ?: "${p.url} / ${p.database}")
            KeyValue("Node ID", st?.nodeId)
        }

        SectionCard("Index") {
            if (st?.hasIndex != true) {
                Text("No index yet (it is created at the first start).", style = MaterialTheme.typography.bodySmall)
            } else {
                KeyValue("Tracked files", st.trackedFiles.toString())
                st.counts.forEach { KeyValue(it.state, it.count.toString()) }
                KeyValue("Pending", st.pending.toString())
                KeyValue("Local issues", st.localIssues.toString())
            }
        }

        if (!st?.attention.isNullOrEmpty()) {
            SectionCard("Needs attention (${st!!.attention.size})") {
                Text(
                    "Conflicts are resolved by an official LiveSync client (desktop); dirty files are local edits " +
                        "not pushed (pull-only) or waiting to be pushed.",
                    style = MaterialTheme.typography.bodySmall,
                )
                LazyColumn(Modifier.heightIn(max = 300.dp)) {
                    items(st.attention) { a ->
                        Column(Modifier.padding(vertical = 4.dp)) {
                            Text("${a.kind}: ${a.path}", style = MaterialTheme.typography.bodySmall)
                            if (a.note.isNotEmpty()) {
                                Text(a.note, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                            }
                        }
                    }
                }
            }
        }

        SectionCard("Log (last ${LOG_LINES})") {
            val listState = rememberLazyListState()
            LaunchedEffect(logs.size) { if (logs.isNotEmpty()) listState.scrollToItem(logs.size - 1) }
            LazyColumn(Modifier.heightIn(max = 360.dp).fillMaxWidth().horizontalScroll(rememberScrollState()), state = listState) {
                items(logs) { line ->
                    Text(line, fontFamily = FontFamily.Monospace, fontSize = 10.sp, softWrap = false)
                }
            }
        }

        SectionCard("Recovery") {
            Text(
                "Only when the status asks for it. \"Adopt remote settings\" accepts changed tweak values on the " +
                    "server (\"tweaks changed\" refusal). \"Fetch everything again\" starts over from the server, " +
                    "like the plugin's Fetch; edited files are kept.",
                style = MaterialTheme.typography.bodySmall,
            )
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedButton(enabled = st?.runState == RunState.RUNNING, onClick = { confirm = Confirm.Adopt }) { Text("Adopt remote settings") }
                OutlinedButton(enabled = st?.runState == RunState.RUNNING || st?.runState == RunState.FAILED, onClick = { confirm = Confirm.Fetch }) { Text("Fetch everything again") }
            }
            TextButton(onClick = { showRaw = !showRaw }) { Text(if (showRaw) "Hide raw status" else "Show raw status") }
            if (showRaw) {
                Text(st?.statusJson ?: "", fontFamily = FontFamily.Monospace, fontSize = 10.sp)
                if (!st?.sessionJson.isNullOrEmpty()) Text(st?.sessionJson ?: "", fontFamily = FontFamily.Monospace, fontSize = 10.sp)
            }
            OutlinedButton(onClick = { confirm = Confirm.Remove }) { Text("Remove this vault profile") }
        }
    }

    gateResult?.let { g ->
        AlertDialog(
            onDismissRequest = { gateResult = null },
            title = { Text("Gate") },
            text = { Column { GateCard(g) } },
            confirmButton = { TextButton(onClick = { gateResult = null }) { Text("OK") } },
        )
    }

    confirm?.let { c ->
        AlertDialog(
            onDismissRequest = { confirm = null },
            title = { Text(c.title) },
            text = { Text(c.text) },
            dismissButton = { TextButton(onClick = { confirm = null }) { Text("Cancel") } },
            confirmButton = {
                TextButton(onClick = {
                    confirm = null
                    when (c) {
                        Confirm.TwoWay -> update("switched to two-way") { it.copy(pullOnly = false) }
                        Confirm.Adopt -> scope.launch {
                            gateResult = withContext(Dispatchers.IO) {
                                runCatching { core.engine.adoptTweaks(p.id) }.getOrNull()
                            }
                        }
                        Confirm.Fetch -> scope.launch {
                            busy = "Scheduling a full fetch…"
                            val r = withContext(Dispatchers.IO) { runCatching { core.engine.fetchFromRemote(p.id) } }
                            busy = r.exceptionOrNull()?.let { "Full fetch: ${it.message}" }
                        }
                        Confirm.Remove -> scope.launch {
                            withContext(Dispatchers.IO) {
                                runCatching {
                                    app.profiles.update(p.id) { it.copy(paused = true) }
                                    core.engine.stop(p.id)
                                }
                                runCatching { app.secrets.removeProfile(p.id) }
                                app.profiles.remove(p.id)
                                core.log("profile ${p.id} removed")
                            }
                            SyncService.start(ctx, "profile removed")
                            onRemoved()
                        }
                    }
                }) { Text(c.action) }
            },
        )
    }
}

private enum class Confirm(val title: String, val text: String, val action: String) {
    TwoWay(
        "Switch to two-way sync?",
        "Edits, new files, renames and deletions on this phone will be written to the server and reach your " +
            "other devices. Files edited here while in pull-only mode are pushed now. Deletions are soft deletes, " +
            "and a pass that would delete more than half of the files stops instead.",
        "Switch to two-way",
    ),
    Adopt(
        "Adopt the server's settings?",
        "Accept the changed tweak values (chunk size, hashing, ...) the server now prefers. Do this only when " +
            "you changed them on purpose in the desktop plugin.",
        "Adopt",
    ),
    Fetch(
        "Fetch everything again?",
        "Forgets what was synced and compares every file with the server again. Unchanged files are refreshed, " +
            "edited files are kept (as conflicts if they differ). Use it to clear a lock after a rebuild elsewhere.",
        "Fetch",
    ),
    Remove(
        "Remove this vault profile?",
        "Stops syncing and forgets the sync index and the stored password. The vault folder and its files stay " +
            "on the phone, and nothing changes on the server.",
        "Remove",
    ),
}
