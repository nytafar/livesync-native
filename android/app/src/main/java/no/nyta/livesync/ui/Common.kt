package no.nyta.livesync.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

@Composable
fun SectionCard(title: String, modifier: Modifier = Modifier, content: @Composable () -> Unit) {
    Card(modifier.fillMaxWidth().padding(vertical = 6.dp)) {
        Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
            Text(title, style = MaterialTheme.typography.titleMedium)
            content()
        }
    }
}

enum class Tone { OK, WARN, ERROR, INFO }

@Composable
fun toneColor(t: Tone): Color = when (t) {
    Tone.OK -> Color(0xFF2E7D32)
    Tone.WARN -> Color(0xFFB26A00)
    Tone.ERROR -> MaterialTheme.colorScheme.error
    Tone.INFO -> MaterialTheme.colorScheme.onSurfaceVariant
}

@Composable
fun Banner(text: String, tone: Tone) {
    Card(
        Modifier.fillMaxWidth().padding(vertical = 4.dp),
        colors = CardDefaults.cardColors(containerColor = toneColor(tone).copy(alpha = 0.12f)),
    ) {
        Text(text, Modifier.padding(12.dp), color = toneColor(tone))
    }
}

/** A permission or setting with its state and a button to fix it. */
@Composable
fun CheckRow(label: String, ok: Boolean, detail: String?, action: String?, onAction: () -> Unit) {
    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
        Column(Modifier.weight(1f)) {
            Text(
                (if (ok) "✓ " else "✗ ") + label,
                color = toneColor(if (ok) Tone.OK else Tone.WARN),
                fontWeight = FontWeight.Medium,
            )
            if (detail != null) Text(detail, style = MaterialTheme.typography.bodySmall)
        }
        if (action != null) {
            Spacer(Modifier.width(8.dp))
            OutlinedButton(onClick = onAction) { Text(action) }
        }
    }
}

@Composable
fun KeyValue(key: String, value: String?) {
    Row(Modifier.fillMaxWidth()) {
        Text(key, Modifier.width(120.dp), style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.onSurfaceVariant)
        Text(value ?: "–", style = MaterialTheme.typography.bodySmall)
    }
}

fun fmtTime(ms: Long?): String? {
    if (ms == null) return null
    val ago = (System.currentTimeMillis() - ms) / 1000
    val rel = when {
        ago < 0 -> "just now"
        ago < 90 -> "${ago}s ago"
        ago < 5400 -> "${ago / 60} min ago"
        ago < 172800 -> "${ago / 3600} h ago"
        else -> "${ago / 86400} days ago"
    }
    return SimpleDateFormat("d MMM HH:mm:ss", Locale.getDefault()).format(Date(ms)) + " ($rel)"
}

/** `"<ms> ok: version 12 ..."` → `ok: version 12 ... (14:02:11)`. */
fun fmtStamped(s: String?): String? {
    if (s == null) return null
    val ms = s.substringBefore(' ').toLongOrNull() ?: return s
    return "${s.substringAfter(' ')} — ${fmtTime(ms)}"
}
