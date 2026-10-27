package no.nyta.livesync.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.Locale
import java.util.TimeZone

class FormatsTest {
    private val en = Locale.ENGLISH
    private val oslo = TimeZone.getTimeZone("Europe/Oslo")

    @Test
    fun createdIsALocalDateTime() {
        // The phone run's milestone: 2026-04-07T11:22:13.683Z, summer time in Oslo.
        assertEquals("7 Apr 2026 13:22", Formats.created(1775560933683, en, oslo))
        assertEquals("7 Apr 2026 11:22", Formats.created(1775560933683, en, TimeZone.getTimeZone("UTC")))
        assertEquals("–", Formats.created(0, en, oslo))
    }

    @Test
    fun theGateSummaryShowsCreatedAsADate() {
        assertEquals(
            "ok: version 12 created 7 Apr 2026 13:22 locked false",
            Formats.gateText("ok: version 12 created 1775560933683 locked false", en, oslo),
        )
        val failed = "failed: refused: milestone locked"
        assertEquals(failed, Formats.gateText(failed, en, oslo))
    }

    @Test
    fun aPlainOwedFetchIsNoWarningForPullOnly() {
        assertNull(Formats.fetchOwedBanner(false, false, false, pullOnly = true, running = true))
        val pull = Formats.fetchOwedBanner(true, false, false, pullOnly = true, running = true)!!
        assertEquals("Fetching everything from the server…", pull)
        assertTrue(Formats.fetchOwedBanner(true, false, false, pullOnly = false, running = true)!!.contains("pushes start"))
        assertTrue(Formats.fetchOwedBanner(true, false, false, pullOnly = true, running = false)!!.contains("when syncing resumes"))
        // The rebuild wait has its own banner.
        assertNull(Formats.fetchOwedBanner(true, false, true, pullOnly = true, running = true))
    }

    @Test
    fun anOwedResolutionNamesTwoWayForPullOnly() {
        val pull = Formats.fetchOwedBanner(true, true, false, pullOnly = true, running = true)!!
        assertTrue(pull, pull.contains("switch to two-way"))
        val two = Formats.fetchOwedBanner(true, true, true, pullOnly = false, running = true)!!
        assertTrue(two, two.contains("marked as resolved"))
    }
}
