package `fun`.hackerlife.octendra

import org.junit.Assert.*
import org.junit.Test

class NetworkRecoveryStateTest {
    @Test fun replacementSurvivesLateLossOfOldRoute() {
        val state = NetworkRecoveryState()
        assertFalse(state.available(1))
        assertTrue(state.validated(1))
        assertTrue(state.available(2))
        assertFalse(state.lost(1))
        assertTrue(state.validated(2))
        assertFalse(state.validated(1))
        assertTrue(state.lost(2))
    }
    @Test fun repeatedSignalsDoNotCreateRetryWakeLoops() {
        val state = NetworkRecoveryState()
        assertFalse(state.available(10))
        assertTrue(state.validated(10))
        assertFalse(state.available(10))
        assertFalse(state.validated(10))
        assertFalse(state.lost(11))
        assertTrue(state.lost(10))
        assertFalse(state.lost(10))
        assertFalse(state.available(10))
        assertTrue(state.validated(10))
    }
    @Test fun validationBeforeAvailabilityCannotDeclareRecovery() {
        val state = NetworkRecoveryState()
        assertFalse(state.validated(5))
        assertFalse(state.lost(5))
        assertFalse(state.available(5))
        assertTrue(state.validated(5))
    }
}
