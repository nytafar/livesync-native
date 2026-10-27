package no.nyta.livesync.ui

import android.content.Intent
import android.content.res.Configuration
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.BackHandler
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import no.nyta.livesync.LiveSyncApp
import no.nyta.livesync.service.SyncService

class MainActivity : ComponentActivity() {
    /** A vault to open, from an alert's [EXTRA_OPEN_PROFILE]. */
    private var openRequest by mutableStateOf<String?>(null)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        if (savedInstanceState == null) openRequest = intent?.getStringExtra(EXTRA_OPEN_PROFILE)
        enableEdgeToEdge()
        val app = application as LiveSyncApp
        // Make sure the service runs whenever a profile is active (also after
        // a force-stop, which cancels the boot start and the sticky restart).
        if (runCatching { app.profiles.all().any { !it.paused } }.getOrDefault(false)) {
            SyncService.start(this, "app opened")
        }
        setContent {
            LiveSyncTheme {
                Surface(Modifier.fillMaxSize()) {
                    AppRoot(app, openRequest) { openRequest = null }
                }
            }
        }
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        intent.getStringExtra(EXTRA_OPEN_PROFILE)?.let { openRequest = it }
    }

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        // The activity is not recreated (see the manifest): pick the system
        // bars' light or dark style again after a dark-mode switch.
        enableEdgeToEdge()
    }

    companion object {
        const val EXTRA_OPEN_PROFILE = "open_profile"
    }
}

private sealed interface Screen {
    /** The home screen: every vault, and "Add vault". */
    data object Vaults : Screen
    data class Status(val id: String) : Screen
    data class Setup(val editId: String?) : Screen
}

@Composable
private fun AppRoot(app: LiveSyncApp, openRequest: String?, onOpenHandled: () -> Unit) {
    val core = app.core
    if (core.isFailure) {
        Column(Modifier.padding(24.dp)) {
            Text("The sync core could not be loaded.", style = MaterialTheme.typography.titleMedium)
            Text(core.exceptionOrNull().toString())
        }
        return
    }
    var version by remember { mutableIntStateOf(0) }
    val profiles = remember(version) { app.profiles.all() }
    var screen by remember { mutableStateOf<Screen>(if (profiles.isEmpty()) Screen.Setup(null) else Screen.Vaults) }

    // An alert names its vault: open that vault's status.
    LaunchedEffect(openRequest) {
        val id = openRequest ?: return@LaunchedEffect
        version++
        if (app.profiles.get(id) != null) screen = Screen.Status(id)
        onOpenHandled()
    }

    val back: Screen? = when (val s = screen) {
        Screen.Vaults -> null
        is Screen.Status -> Screen.Vaults
        is Screen.Setup -> when {
            s.editId != null -> Screen.Status(s.editId)
            profiles.isEmpty() -> null
            else -> Screen.Vaults
        }
    }
    BackHandler(enabled = back != null) { back?.let { screen = it } }

    when (val s = screen) {
        Screen.Vaults -> if (profiles.isEmpty()) {
            screen = Screen.Setup(null)
        } else {
            VaultListScreen(
                app = app,
                onOpen = { screen = Screen.Status(it) },
                onAdd = { screen = Screen.Setup(null) },
            )
        }
        is Screen.Setup -> SetupScreen(
            app = app,
            existing = s.editId?.let { id -> profiles.firstOrNull { it.id == id } },
            onDone = { id ->
                version++
                screen = Screen.Status(id)
            },
            onCancel = back?.let { b -> { screen = b } },
        )
        is Screen.Status -> if (profiles.none { it.id == s.id }) {
            screen = Screen.Vaults
        } else {
            // A fresh screen per vault: nothing remembered for one vault (a
            // status, an open confirmation such as "Allow these deletions")
            // can act on another.
            key(s.id) {
                StatusScreen(
                    app = app,
                    profileId = s.id,
                    onBack = { screen = Screen.Vaults },
                    onEdit = { screen = Screen.Setup(s.id) },
                    onChanged = { version++ },
                    onRemoved = {
                        // May come after the user left this vault's screen:
                        // refresh the list, and leave the screen only if it
                        // still shows the removed vault.
                        version++
                        if (screen == Screen.Status(s.id)) screen = Screen.Vaults
                    },
                )
            }
        }
    }
}

@Composable
fun LiveSyncTheme(content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    val ctx = LocalContext.current
    val colors = when {
        Build.VERSION.SDK_INT >= 31 && dark -> dynamicDarkColorScheme(ctx)
        Build.VERSION.SDK_INT >= 31 -> dynamicLightColorScheme(ctx)
        dark -> darkColorScheme()
        else -> lightColorScheme()
    }
    MaterialTheme(colorScheme = colors, content = content)
}
