package no.nyta.livesync.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import no.nyta.livesync.LiveSyncApp
import no.nyta.livesync.core.Cancel
import no.nyta.livesync.core.GateOutcome
import no.nyta.livesync.core.GateResult
import no.nyta.livesync.profile.Profile
import no.nyta.livesync.profile.ProfileRules
import no.nyta.livesync.profile.ProfileStore
import no.nyta.livesync.profile.ScopeMode
import no.nyta.livesync.profile.ScopePick
import no.nyta.livesync.profile.VaultCheck
import no.nyta.livesync.profile.VaultReport
import no.nyta.livesync.secret.SecretStore
import no.nyta.livesync.service.SyncService
import java.io.File

/**
 * Onboarding (no [existing] profile) and settings (an existing one). The
 * vault folder, server URL and database of an existing profile are fixed: its
 * sync index is bound to them; to change them, remove the profile and add it
 * again.
 */
@Composable
fun SetupScreen(app: LiveSyncApp, existing: Profile?, onDone: () -> Unit, onCancel: (() -> Unit)?) {
    val ctx = LocalContext.current
    val focus = LocalFocusManager.current
    val scope = rememberCoroutineScope()
    val core = app.core.getOrThrow()
    val checks = rememberDeviceChecks()
    val editing = existing != null

    var vaultPath by remember {
        mutableStateOf(existing?.vaultPath ?: File(Perms.defaultVaultParent(), "").path + "/")
    }
    var url by remember { mutableStateOf(existing?.url ?: "https://") }
    var database by remember { mutableStateOf(existing?.database ?: "") }
    var username by remember { mutableStateOf(existing?.username ?: "") }
    var password by remember { mutableStateOf("") }
    var showPassword by remember { mutableStateOf(false) }
    var deviceName by remember { mutableStateOf(existing?.deviceName ?: LiveSyncApp.deviceNameDefault()) }
    var scopeMode by remember { mutableStateOf(existing?.scopeMode ?: ScopeMode.ALL) }
    var foldersText by remember { mutableStateOf(existing?.scopeFolders?.joinToString("\n") ?: "") }
    var pullOnly by remember { mutableStateOf(existing?.pullOnly ?: true) }
    var picking by remember { mutableStateOf(false) }
    var report by remember { mutableStateOf<VaultReport?>(null) }
    var gate by remember { mutableStateOf<GateResult?>(null) }
    var testedKey by remember { mutableStateOf<String?>(null) }
    var testing by remember { mutableStateOf(false) }
    var saving by remember { mutableStateOf(false) }
    var saveError by remember { mutableStateOf<String?>(null) }
    var folderLoad by remember { mutableStateOf<FolderLoad>(FolderLoad.Idle) }
    // The connect-test key (server, user, typed password) the list was made
    // for, so changed credentials hide a stale list, in setup and settings.
    var folderKey by remember { mutableStateOf<String?>(null) }
    var folderGen by remember { mutableIntStateOf(0) }
    // The listing in flight. A listing reads every file's metadata (about
    // 15 MB on a large vault); leaving the screen cancels it in the core,
    // which closes its connection, instead of letting it run to its timeout.
    val folderCancel = remember { arrayOfNulls<Cancel>(1) }
    DisposableEffect(Unit) {
        onDispose { folderCancel[0]?.cancel() }
    }

    LaunchedEffect(vaultPath, checks.allFiles) {
        report = if (vaultPath.trimEnd('/').isEmpty()) null else withContext(Dispatchers.IO) {
            VaultCheck.inspect(vaultPath.trimEnd('/'), Perms.sharedRoot())
        }
    }
    val cleanVault = vaultPath.trimEnd('/')
    val draft = Profile(
        id = existing?.id ?: "new",
        name = existing?.name ?: ProfileRules.defaultName(cleanVault),
        vaultPath = cleanVault,
        url = url.trim(),
        database = database.trim(),
        username = username.trim(),
        scopeMode = scopeMode,
        scopeFolders = ProfileRules.parseFolders(foldersText),
        pullOnly = pullOnly,
        paused = existing?.paused ?: false,
        deviceName = deviceName.trim(),
        createdAtMs = existing?.createdAtMs ?: System.currentTimeMillis(),
    )
    val overlap = remember(cleanVault) { app.profiles.overlapping(draft) }
    val urlProblem = ProfileRules.urlProblem(url)
    val scopeProblem = ProfileRules.scopeProblem(scopeMode, draft.scopeFolders)
    // What a connect test vouches for.
    val testKey = listOf(draft.url, draft.database, draft.username, password).joinToString("\u0000")
    val credentialsChanged = !editing || password.isNotEmpty() || draft.username != existing?.username
    val testPassed = gate?.outcome == GateOutcome.PASSED && testedKey == testKey
    val vaultOk = report?.errors?.isEmpty() == true && overlap == null
    val canTest = urlProblem == null && draft.database.isNotEmpty() && (password.isNotEmpty() || editing) && !testing

    fun loadFolders(pw: String, key: String) {
        val gen = ++folderGen
        val p = draft.copy(id = existing?.id ?: "connect-test")
        val cancel = Cancel()
        folderCancel[0]?.cancel()
        folderCancel[0] = cancel
        folderKey = key
        folderLoad = FolderLoad.Loading
        scope.launch {
            val r = withContext(Dispatchers.IO) { runCatching { core.listFolders(p, pw, cancel) } }
            if (gen != folderGen) return@launch
            folderLoad = r.fold({ FolderLoad.Loaded(it) }, { FolderLoad.Failed(it.message ?: it.toString()) })
        }
    }

    fun reloadFolders() {
        val key = testKey
        val typed = password
        scope.launch {
            val pw = typed.ifEmpty {
                existing?.let { withContext(Dispatchers.IO) { app.secrets.get(it.id, SecretStore.PASSWORD) } } ?: ""
            }
            loadFolders(pw, key)
        }
    }

    // Settings list only when asked ("List the folders on the server"), with
    // the stored password: never again on every open.
    val canSave = checks.allFiles && vaultOk && urlProblem == null && scopeProblem == null &&
        draft.database.isNotEmpty() && (testPassed || !credentialsChanged) && !saving

    Column(
        Modifier.fillMaxSize().safeDrawingPadding().verticalScroll(rememberScrollState()).padding(16.dp),
    ) {
        Text(if (editing) "Settings: ${existing?.name}" else "Set up LiveSync", style = MaterialTheme.typography.headlineSmall)
        Text(
            "Keeps an Obsidian vault folder in two-way sync with your Self-hosted LiveSync CouchDB, " +
                "whether or not Obsidian is open. Disable the LiveSync plugin in this vault on this phone.",
            style = MaterialTheme.typography.bodySmall,
        )
        Spacer(Modifier.height(8.dp))
        SetupChecksCard(checks)

        SectionCard("2. Vault folder") {
            OutlinedTextField(
                value = vaultPath,
                onValueChange = { vaultPath = it },
                label = { Text("Folder in shared storage") },
                singleLine = true,
                enabled = !editing,
                modifier = Modifier.fillMaxWidth(),
            )
            if (!editing) {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedButton(enabled = checks.allFiles, onClick = { picking = true }) { Text("Browse…") }
                }
                if (!checks.allFiles) Text("Grant all-files access first.", style = MaterialTheme.typography.bodySmall)
                Text(
                    "Open the same folder in Obsidian as a device-storage vault (by default under Documents/Obsidian).",
                    style = MaterialTheme.typography.bodySmall,
                )
            } else {
                Text("The vault folder of a profile cannot change; remove the profile and add it again.", style = MaterialTheme.typography.bodySmall)
            }
            overlap?.let { Banner("Overlaps \"${it.name}\" (${it.vaultPath}): vault folders may not be the same or inside each other.", Tone.ERROR) }
            report?.let { r ->
                r.errors.forEach { Banner(it, Tone.ERROR) }
                if (!editing) r.warnings.forEach { Banner(it, Tone.WARN) }
                if (r.errors.isEmpty() && !r.exists) Text("The folder will be created.", style = MaterialTheme.typography.bodySmall)
            }
        }

        SectionCard("3. CouchDB server") {
            OutlinedTextField(
                value = url, onValueChange = { url = it }, label = { Text("Server URL") },
                singleLine = true, enabled = !editing, modifier = Modifier.fillMaxWidth(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                isError = url.length > 8 && urlProblem != null,
                supportingText = { if (url.length > 8 && urlProblem != null) Text(urlProblem) },
            )
            if (urlProblem == null && ProfileRules.insecureUrl(url)) {
                Banner("Plain http:// sends the password unencrypted. Use https:// unless this is a tailnet or LAN address.", Tone.WARN)
            }
            OutlinedTextField(
                value = database, onValueChange = { database = it.trim() }, label = { Text("Database") },
                singleLine = true, enabled = !editing, modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = username, onValueChange = { username = it }, label = { Text("User name") },
                singleLine = true, modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = password, onValueChange = { password = it },
                label = { Text(if (editing) "Password (leave empty to keep)" else "Password") },
                singleLine = true, modifier = Modifier.fillMaxWidth(),
                visualTransformation = if (showPassword) VisualTransformation.None else PasswordVisualTransformation(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
                trailingIcon = {
                    TextButton(onClick = { showPassword = !showPassword }) { Text(if (showPassword) "Hide" else "Show") }
                },
            )
            Text("The password is stored encrypted with a key in this phone's Android Keystore.", style = MaterialTheme.typography.bodySmall)
            OutlinedTextField(
                value = deviceName, onValueChange = { deviceName = it }, label = { Text("Device name (shown to other LiveSync clients)") },
                singleLine = true, modifier = Modifier.fillMaxWidth(),
            )
        }

        SectionCard("4. Connect test") {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Button(enabled = canTest, onClick = {
                    testing = true
                    gate = null
                    val key = testKey
                    scope.launch {
                        val pw = password.ifEmpty { existing?.let { app.secrets.get(it.id, SecretStore.PASSWORD) } ?: "" }
                        val r = withContext(Dispatchers.IO) {
                            runCatching { core.connectTest(draft.copy(id = existing?.id ?: "connect-test"), pw) }
                        }
                        val g = r.getOrElse { e ->
                            GateResult(GateOutcome.FAILED, e.message ?: e.toString(), "", 0, 0, 0, false, 0, false, false, 0, false)
                        }
                        gate = g
                        testedKey = key
                        testing = false
                        // Before anything syncs: what is on the server, to pick
                        // folders from. On mobile data only when asked.
                        if (g.outcome == GateOutcome.PASSED && !Perms.metered(ctx)) loadFolders(pw, key)
                    }
                }) { Text("Test connection") }
                if (testing) {
                    Spacer(Modifier.width(12.dp))
                    CircularProgressIndicator(Modifier.size(24.dp))
                }
            }
            gate?.let { GateCard(it) }
            if (!credentialsChanged) Text("Not needed unless you change the user name or password.", style = MaterialTheme.typography.bodySmall)
        }

        SectionCard("5. Folders to sync") {
            ScopeOption("All folders", scopeMode == ScopeMode.ALL) { scopeMode = ScopeMode.ALL }
            ScopeOption("Only these folders", scopeMode == ScopeMode.INCLUDE) { scopeMode = ScopeMode.INCLUDE }
            ScopeOption("Everything except these folders", scopeMode == ScopeMode.EXCLUDE) { scopeMode = ScopeMode.EXCLUDE }
            if (scopeMode != ScopeMode.ALL) {
                OutlinedTextField(
                    value = foldersText, onValueChange = { foldersText = it },
                    label = { Text("Folders, one per line (vault-relative, case-insensitive)") },
                    modifier = Modifier.fillMaxWidth(), minLines = 2,
                    isError = scopeProblem != null,
                    supportingText = { scopeProblem?.let { Text(it) } },
                )
            }
            // Only a list made with the credentials now on the screen.
            val shown = if (folderKey == testKey) folderLoad else FolderLoad.Idle
            val canList = editing || testPassed
            ServerFolders(
                load = shown,
                mode = scopeMode,
                folders = draft.scopeFolders,
                onToggle = { name -> foldersText = ScopePick.toggle(ProfileRules.parseFolders(foldersText), name).joinToString("\n") },
                onReload = if (canList) ({ reloadFolders() }) else null,
            )
            if (shown !is FolderLoad.Loaded && scopeMode != ScopeMode.ALL) {
                // Without the server's list (not tested yet, offline, encrypted
                // paths): the vault's own folders, as before.
                val top = remember(report) { if (report?.isDirectory == true) VaultCheck.topFolders(File(cleanVault)) else emptyList() }
                if (top.isNotEmpty()) {
                    Text("Folders in the vault now:", style = MaterialTheme.typography.bodySmall)
                    FlowChips(top) { name ->
                        val list = ProfileRules.parseFolders(foldersText)
                        if (list.none { it.equals(name, ignoreCase = true) }) foldersText = (list + name).joinToString("\n")
                    }
                }
            }
            if (shown == FolderLoad.Idle) {
                if (canList) {
                    OutlinedButton(onClick = { reloadFolders() }) { Text("List the folders on the server") }
                    Text(
                        if (Perms.metered(ctx)) {
                            "You are on mobile data: listing reads every file's details from the server, about " +
                                "1 KB per file (some 15 MB for 20,000 files)."
                        } else {
                            "Reads every file's details from the server, about 1 KB per file."
                        },
                        style = MaterialTheme.typography.bodySmall,
                    )
                } else {
                    Text("A passing connect test lists the folders on the server here.", style = MaterialTheme.typography.bodySmall)
                }
            }
            Text(
                "Files outside the chosen folders are never written here and never pushed; nothing is deleted on " +
                    "the server when you narrow the list. Dot-folders (.obsidian, .trash) never sync.",
                style = MaterialTheme.typography.bodySmall,
            )
        }

        SectionCard("6. Mode") {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Column(Modifier.weight(1f)) {
                    Text("Pull only (read-only)")
                    Text(
                        "Start safely: follow the server without ever writing to it. Edits made on this phone stay " +
                            "local until you switch to two-way.",
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
                Switch(checked = pullOnly, onCheckedChange = { pullOnly = it })
            }
        }

        saveError?.let { Banner(it, Tone.ERROR) }
        if (!checks.battery) {
            Banner("Without the battery exemption, Android will stop the sync while the phone sleeps.", Tone.WARN)
        }
        Row(Modifier.fillMaxWidth().padding(vertical = 12.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            if (onCancel != null) OutlinedButton(onClick = onCancel) { Text("Cancel") }
            Button(enabled = canSave, onClick = {
                // A tap while the keyboard is closing can be lost: the IME
                // padding shrinks and the scrolled button moves under the
                // finger, which cancels the press. Close it here, and show
                // "Saving…" so a tap that did land is plain to see.
                focus.clearFocus()
                saving = true
                saveError = null
                scope.launch {
                    val err = withContext(Dispatchers.IO) { save(app, existing, draft, password) }
                    if (err == null) {
                        SyncService.start(ctx, if (editing) "profile edited" else "profile created")
                        // Stays "Saving…" (and disabled) until the screen is gone.
                        onDone()
                    } else {
                        saving = false
                        saveError = err
                    }
                }
            }) {
                if (saving) {
                    CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                    Spacer(Modifier.width(8.dp))
                    Text("Saving…")
                } else {
                    Text(if (editing) "Save and restart" else "Save and start syncing")
                }
            }
        }
        if (!canSave) {
            val why = when {
                !checks.allFiles -> "Grant all-files access."
                !vaultOk -> "Choose a usable vault folder."
                urlProblem != null -> urlProblem
                draft.database.isEmpty() -> "Enter the database name."
                scopeProblem != null -> scopeProblem
                credentialsChanged && !testPassed -> "Run a successful connect test with these settings."
                else -> null
            }
            why?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
        }
    }

    if (picking) {
        FolderPickerDialog(
            start = File(cleanVault).takeIf { it.isDirectory } ?: Perms.defaultVaultParent().takeIf { it.isDirectory } ?: Perms.sharedRoot(),
            root = Perms.sharedRoot(),
            onPick = {
                vaultPath = it.path
                picking = false
            },
            onDismiss = { picking = false },
        )
    }
}

/** Store the secret, then the profile. Returns an error message or null. */
private fun save(app: LiveSyncApp, existing: Profile?, draft: Profile, password: String): String? {
    val id = existing?.id ?: app.profiles.newId()
    val profile = draft.copy(id = id)
    return try {
        if (password.isNotEmpty()) app.secrets.put(id, SecretStore.PASSWORD, password)
        app.profiles.put(profile)
        app.core.getOrNull()?.log("profile $id saved (${profile.vaultPath}, ${if (profile.pullOnly) "pull-only" else "two-way"}, scope ${profile.scopeMode})")
        null
    } catch (e: ProfileStore.OverlapException) {
        if (existing == null) app.secrets.removeProfile(id)
        e.message
    } catch (e: Exception) {
        if (existing == null) app.secrets.removeProfile(id)
        "Could not save: ${e.message}"
    }
}

@Composable
private fun ScopeOption(label: String, selected: Boolean, onClick: () -> Unit) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        RadioButton(selected = selected, onClick = onClick)
        Text(label)
    }
}

@Composable
private fun FlowChips(names: List<String>, onClick: (String) -> Unit) {
    // A simple wrap: rows of up to three chips.
    Column {
        names.take(30).chunked(3).forEach { row ->
            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                row.forEach { n -> AssistChip(onClick = { onClick(n) }, label = { Text(n) }) }
            }
        }
    }
}

@Composable
fun GateCard(g: GateResult) {
    val tone = when (g.outcome) {
        GateOutcome.PASSED -> Tone.OK
        GateOutcome.REFUSED -> Tone.ERROR
        else -> Tone.WARN
    }
    Banner(g.message, tone)
    if (g.outcome == GateOutcome.PASSED) {
        KeyValue("Remote", g.remote)
        KeyValue("Version doc", g.version.toString())
        KeyValue("Documents", g.docCount.toString())
        KeyValue("Created", Formats.created(g.created))
        KeyValue("Locked", if (g.locked) "yes${if (g.fetchToResolve) " (resolved by the first full fetch)" else ""}" else "no")
        KeyValue("Accepted nodes", g.acceptedNodes.toString())
        KeyValue("Max file size", "${g.maxSizeMb} MB")
        KeyValue("Case-sensitive IDs", if (g.caseSensitiveIds) "yes" else "no")
    }
}
