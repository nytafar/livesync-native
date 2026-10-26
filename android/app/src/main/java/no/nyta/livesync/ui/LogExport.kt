package no.nyta.livesync.ui

import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.core.content.FileProvider
import no.nyta.livesync.BuildConfig
import no.nyta.livesync.LiveSyncApp
import no.nyta.livesync.core.coreVersion
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * "Export logs": the app's state, every profile's status and the log files,
 * in one text file in Download/LiveSync/ (easy to `adb pull`) and offered to
 * the share sheet. Passwords are never part of it (they are not in the
 * profiles, the status or the log).
 */
object LogExport {
    fun export(ctx: Context): File {
        val app = ctx.applicationContext as LiveSyncApp
        val stamp = SimpleDateFormat("yyyyMMdd-HHmmss", Locale.ROOT).format(Date())
        val text = buildString {
            appendLine("LiveSync for Android ${BuildConfig.VERSION_NAME} (${BuildConfig.VERSION_CODE}), core ${runCatching { coreVersion() }.getOrDefault("?")}")
            appendLine("Exported $stamp; Android ${Build.VERSION.RELEASE} (API ${Build.VERSION.SDK_INT}); ${Build.MANUFACTURER} ${Build.MODEL}")
            appendLine("All-files access: ${Perms.allFilesAccess()}; battery exempt: ${Perms.batteryExempt(ctx)}; notifications: ${Perms.notificationsAllowed(ctx)}")
            appendLine()
            val core = app.core.getOrNull()
            if (core == null) appendLine("Core failed to load: ${app.core.exceptionOrNull()}")
            for (p in app.profiles.all()) {
                appendLine("=== Profile ${p.id} \"${p.name}\"")
                appendLine("vault ${p.vaultPath}; remote ${p.url} db ${p.database} user ${p.username}")
                appendLine("scope ${p.scopeMode} ${p.scopeFolders}; pullOnly ${p.pullOnly}; paused ${p.paused}; device ${p.deviceName}")
                core?.let {
                    val st = runCatching { it.status(p) }.getOrNull()
                    appendLine("run state ${st?.runState} ${st?.runMessage ?: ""}")
                    appendLine(st?.statusJson ?: "(no status)")
                    if (!st?.sessionJson.isNullOrEmpty()) {
                        appendLine("session:")
                        appendLine(st?.sessionJson)
                    }
                }
                appendLine()
            }
            appendLine("=== Log")
            for (f in core?.engine?.logFiles() ?: emptyList()) {
                appendLine("--- $f")
                append(runCatching { File(f).readText() }.getOrDefault("(unreadable)\n"))
            }
        }
        val name = "livesync-logs-$stamp.txt"
        val shared = File(Perms.sharedRoot(), "Download/LiveSync")
        val out = runCatching {
            shared.mkdirs()
            File(shared, name).also { it.writeText(text) }
        }.getOrNull()
        val cacheCopy = File(ctx.cacheDir, "exports").apply { mkdirs() }.let { File(it, name) }
        cacheCopy.writeText(text)
        return out ?: cacheCopy
    }

    fun share(ctx: Context, file: File) {
        val cacheCopy = File(File(ctx.cacheDir, "exports"), file.name)
        val uri = FileProvider.getUriForFile(ctx, "${ctx.packageName}.files", cacheCopy)
        val send = Intent(Intent.ACTION_SEND)
            .setType("text/plain")
            .putExtra(Intent.EXTRA_STREAM, uri)
            .putExtra(Intent.EXTRA_SUBJECT, file.name)
            .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        ctx.startActivity(Intent.createChooser(send, "Share logs").addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
    }
}
