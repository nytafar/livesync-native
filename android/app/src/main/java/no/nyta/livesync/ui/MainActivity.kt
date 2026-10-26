package no.nyta.livesync.ui

import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
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
import androidx.compose.runtime.getValue
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
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        val app = application as LiveSyncApp
        // Make sure the service runs whenever a profile is active (also after
        // a force-stop, which cancels the boot start and the sticky restart).
        if (runCatching { app.profiles.all().any { !it.paused } }.getOrDefault(false)) {
            SyncService.start(this, "app opened")
        }
        setContent {
            LiveSyncTheme {
                Surface(Modifier.fillMaxSize()) { AppRoot(app) }
            }
        }
    }
}

private sealed interface Screen {
    data object Status : Screen
    data class Setup(val editId: String?) : Screen
}

@Composable
private fun AppRoot(app: LiveSyncApp) {
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
    var screen by remember { mutableStateOf<Screen>(if (profiles.isEmpty()) Screen.Setup(null) else Screen.Status) }
    when (val s = screen) {
        is Screen.Setup -> SetupScreen(
            app = app,
            existing = s.editId?.let { id -> profiles.firstOrNull { it.id == id } },
            onDone = {
                version++
                screen = Screen.Status
            },
            onCancel = if (profiles.isEmpty()) null else ({ screen = Screen.Status }),
        )
        Screen.Status -> {
            val p = profiles.firstOrNull()
            if (p == null) {
                screen = Screen.Setup(null)
            } else {
                StatusScreen(
                    app = app,
                    profileId = p.id,
                    onEdit = { screen = Screen.Setup(p.id) },
                    onChanged = { version++ },
                    onRemoved = {
                        version++
                        screen = Screen.Setup(null)
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
