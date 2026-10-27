package no.nyta.livesync.ui

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class DeviceChecksTest {
    private fun checks(samsung: Boolean, battery: Boolean, ticked: Boolean) =
        DeviceChecks(allFiles = true, notifications = true, battery = battery, samsung = samsung, samsungNeverSleepingConfirmed = ticked)

    @Test
    fun theBatteryExemptionCoversOneUiSleeping() {
        // One UI hides battery-exempt apps from "Never sleeping apps": the tick cannot be earned there.
        assertTrue(checks(samsung = true, battery = true, ticked = false).samsungSleepOk)
        assertTrue(checks(samsung = true, battery = false, ticked = true).samsungSleepOk)
        assertFalse(checks(samsung = true, battery = false, ticked = false).samsungSleepOk)
        assertTrue(checks(samsung = false, battery = false, ticked = false).samsungSleepOk)
    }
}
