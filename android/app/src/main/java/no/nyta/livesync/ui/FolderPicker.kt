package no.nyta.livesync.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import java.io.File

/**
 * A plain folder browser over shared storage (java.io, which all-files
 * access allows). The Storage Access Framework picker would hand back a
 * content URI, which the engine cannot use.
 */
@Composable
fun FolderPickerDialog(start: File, root: File, onPick: (File) -> Unit, onDismiss: () -> Unit) {
    var dir by remember { mutableStateOf(if (start.isDirectory) start else root) }
    var newName by remember { mutableStateOf("") }
    val children = remember(dir) {
        (dir.listFiles() ?: emptyArray())
            .filter { it.isDirectory && !it.name.startsWith(".") }
            .sortedBy { it.name.lowercase() }
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("Choose the vault folder") },
        text = {
            Column {
                Text(dir.path, style = MaterialTheme.typography.bodySmall)
                HorizontalDivider(Modifier.padding(vertical = 4.dp))
                LazyColumn(Modifier.heightIn(max = 320.dp)) {
                    if (dir.canonicalPath != root.canonicalPath) {
                        item {
                            Text(
                                "⬑ ..",
                                Modifier.fillMaxWidth().clickable { dir = dir.parentFile ?: root }.padding(10.dp),
                            )
                        }
                    }
                    items(children, key = { it.path }) { f ->
                        Text(
                            "📁 ${f.name}",
                            Modifier.fillMaxWidth().clickable { dir = f }.padding(10.dp),
                        )
                    }
                    if (children.isEmpty()) {
                        item { Text("(no folders)", Modifier.padding(10.dp), style = MaterialTheme.typography.bodySmall) }
                    }
                }
                HorizontalDivider(Modifier.padding(vertical = 4.dp))
                Row(verticalAlignment = Alignment.CenterVertically) {
                    OutlinedTextField(
                        value = newName,
                        onValueChange = { newName = it },
                        label = { Text("New folder here") },
                        singleLine = true,
                        modifier = Modifier.weight(1f),
                    )
                    TextButton(
                        enabled = newName.isNotBlank() && !newName.contains('/') && !newName.startsWith("."),
                        onClick = {
                            val f = File(dir, newName.trim())
                            if (f.isDirectory || f.mkdirs()) {
                                dir = f
                                newName = ""
                            }
                        },
                    ) { Text("Create") }
                }
            }
        },
        confirmButton = { TextButton(onClick = { onPick(dir) }) { Text("Use this folder") } },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    )
}
