package no.nyta.livesync.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.w3c.dom.Element
import java.io.File
import javax.xml.parsers.DocumentBuilderFactory

class StorageAccessTest {
    @Test
    fun allFilesAccessFromAndroid11LegacyPermissionsBelow() {
        assertEquals(StorageAccess.LEGACY, StorageAccess.forSdk(28))
        assertEquals(StorageAccess.LEGACY, StorageAccess.forSdk(29))
        assertEquals(StorageAccess.ALL_FILES, StorageAccess.forSdk(30))
        assertEquals(StorageAccess.ALL_FILES, StorageAccess.forSdk(35))
    }

    @Test
    fun theAndroid11WordingIsUnchanged() {
        assertEquals("All-files access", StorageAccess.ALL_FILES.label)
        assertEquals("all-files access", StorageAccess.ALL_FILES.phrase)
        assertEquals("Storage access", StorageAccess.LEGACY.label)
        assertEquals("storage access", StorageAccess.LEGACY.phrase)
    }

    private val android = "http://schemas.android.com/apk/res/android"

    private fun manifest(): Element {
        val f = DocumentBuilderFactory.newInstance().apply { isNamespaceAware = true }
        return f.newDocumentBuilder().parse(File("src/main/AndroidManifest.xml")).documentElement
    }

    private fun Element.children(tag: String): List<Element> {
        val nodes = getElementsByTagName(tag)
        return (0 until nodes.length).map { nodes.item(it) as Element }
    }

    /** The permissions a device on [sdk] grants from the manifest (maxSdkVersion applied). */
    private fun permissionsOn(sdk: Int): Set<String> = manifest().children("uses-permission")
        .filter { e -> e.getAttributeNS(android, "maxSdkVersion").let { it.isEmpty() || it.toInt() >= sdk } }
        .map { it.getAttributeNS(android, "name").removePrefix("android.permission.") }
        .toSet()

    @Test
    fun android11AndLaterGetExactlyTheReleasePermissions() {
        // The list on main before the Android 9 variant; the legacy storage
        // permissions must stop at API 29 so this stays as it was.
        val release = setOf(
            "INTERNET", "ACCESS_NETWORK_STATE", "MANAGE_EXTERNAL_STORAGE", "FOREGROUND_SERVICE",
            "FOREGROUND_SERVICE_SPECIAL_USE", "RECEIVE_BOOT_COMPLETED", "POST_NOTIFICATIONS",
            "REQUEST_IGNORE_BATTERY_OPTIMIZATIONS", "WAKE_LOCK",
        )
        for (sdk in StorageAccess.ALL_FILES_SDK..36) assertEquals("API $sdk", release, permissionsOn(sdk))
    }

    @Test
    fun android9And10AlsoGetTheLegacyStoragePermissions() {
        for (sdk in 28 until StorageAccess.ALL_FILES_SDK) {
            assertTrue("API $sdk", permissionsOn(sdk).containsAll(setOf("READ_EXTERNAL_STORAGE", "WRITE_EXTERNAL_STORAGE")))
        }
        // Android 10 grants full shared storage to the legacy permissions only
        // in the legacy storage model; Android 11+ ignores the flag at targetSdk 30+.
        val app = manifest().children("application").single()
        assertEquals("true", app.getAttributeNS(android, "requestLegacyExternalStorage"))
    }
}
