package no.nyta.livesync.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.selection.toggleable
import androidx.compose.material3.Checkbox
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.dp
import no.nyta.livesync.core.FolderList
import no.nyta.livesync.profile.ScopeMode
import no.nyta.livesync.profile.ScopePick
import java.util.Locale

/** The server's folder list as the scope picker has it. */
sealed interface FolderLoad {
    data object Idle : FolderLoad
    data object Loading : FolderLoad
    data class Failed(val message: String) : FolderLoad
    data class Loaded(val list: FolderList) : FolderLoad
}

/**
 * The server's top-level folders with their file counts and sizes, ticked
 * from the typed folder list ([folders]); a tap adds or removes exactly that
 * folder. Local-only folders are marked. The checkboxes work in the include
 * and exclude modes; in "all" the list is for reading.
 */
@Composable
fun ServerFolders(
    load: FolderLoad,
    mode: ScopeMode,
    folders: List<String>,
    onToggle: (String) -> Unit,
    onReload: (() -> Unit)?,
) {
    when (load) {
        FolderLoad.Idle -> {}
        FolderLoad.Loading -> Row(verticalAlignment = Alignment.CenterVertically) {
            CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
            Text("  Listing the folders on the server…", style = MaterialTheme.typography.bodySmall)
        }
        is FolderLoad.Failed -> {
            Banner("Could not list the folders on the server: ${load.message}. You can still type them.", Tone.WARN)
            onReload?.let { OutlinedButton(onClick = it) { Text("Try again") } }
        }
        is FolderLoad.Loaded -> {
            val l = load.list
            Text(
                when (mode) {
                    ScopeMode.ALL -> "On the server (choose \"Only these\" or \"Everything except\" to pick):"
                    ScopeMode.INCLUDE -> "Tick the folders to sync:"
                    ScopeMode.EXCLUDE -> "Tick the folders to leave out:"
                },
                style = MaterialTheme.typography.bodyMedium,
            )
            if (l.folders.isEmpty()) Text("No folders on the server.", style = MaterialTheme.typography.bodySmall)
            l.folders.forEach { f ->
                val ticked = ScopePick.ticked(folders, f.name)
                val nested = ScopePick.nestedIn(folders, f.name)
                val pickable = ScopePick.pickable(f.name)
                val enabled = mode != ScopeMode.ALL && pickable
                Row(
                    Modifier.fillMaxWidth().toggleable(
                        value = ticked,
                        enabled = enabled,
                        role = Role.Checkbox,
                        onValueChange = { onToggle(f.name) },
                    ),
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Checkbox(checked = ticked, onCheckedChange = null, enabled = enabled)
                    Column(Modifier.padding(start = 8.dp)) {
                        Text("${f.name}/")
                        val detail = when {
                            !f.remote -> "only in this vault folder, not on the server"
                            else -> Formats.filesAndSize(f.files, f.bytes)
                        }
                        Text(detail, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
                        if (!pickable) {
                            Text(
                                "Cannot be picked: the folder list reads a comma as two folders.",
                                style = MaterialTheme.typography.bodySmall,
                            )
                        }
                        if (nested.isNotEmpty() && mode != ScopeMode.ALL) {
                            Text("Also listed inside: ${nested.joinToString(", ")}", style = MaterialTheme.typography.bodySmall)
                        }
                    }
                }
            }
            if (l.rootFiles > 0) {
                val syncs = ScopePick.rootSyncs(mode)
                Text(
                    "Files at the vault root: ${Formats.filesAndSize(l.rootFiles, l.rootBytes)}" +
                        if (syncs) "" else " (not synced with \"Only these folders\")",
                    style = MaterialTheme.typography.bodySmall,
                )
            }
            Text(
                "${l.files} files on the server; listed from ${l.docsRead} documents " +
                    "(${Formats.size(l.bytesFetched)}) in ${String.format(Locale.getDefault(), "%.1f", l.elapsedMs / 1000.0)} s.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            onReload?.let { OutlinedButton(onClick = it) { Text("List again") } }
        }
    }
}
