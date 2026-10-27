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

    @Test
    fun booxFreezingNeedsTheUsersTickEvenWithTheBatteryExemption() {
        // Boox's own freezer runs alongside Android's battery optimisation, and
        // the app cannot read it: only the user's tick clears it.
        val base = DeviceChecks(allFiles = true, notifications = true, battery = true, samsung = false, samsungNeverSleepingConfirmed = false)
        assertTrue(base.booxOk)
        assertFalse(base.copy(boox = true).booxOk)
        assertTrue(base.copy(boox = true, booxConfirmed = true).booxOk)
    }

    @Test
    fun booxStepsOnlyOnAndroid9And10() {
        assertTrue(Perms.booxChecks(sdk = 28, manufacturer = "ONYX", brand = "ONYX"))
        assertTrue(Perms.booxChecks(sdk = 29, manufacturer = "Onyx", brand = "other"))
        assertTrue(Perms.booxChecks(sdk = 28, manufacturer = null, brand = "onyx"))
        // Android 11+ keeps the setup screen it had, on a Boox too.
        assertFalse(Perms.booxChecks(sdk = 30, manufacturer = "ONYX", brand = "ONYX"))
        assertFalse(Perms.booxChecks(sdk = 28, manufacturer = "samsung", brand = "samsung"))
    }
}
