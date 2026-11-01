package `fun`.hackerlife.octendra

import org.json.JSONObject
import org.junit.Assert.*
import org.junit.Test

class UiContractTest {
    @Test fun imageCoordinatesMapToDisplayEndpoints() {
        assertEquals(Pair(1079.0,2399.0), UiContract.point(obj("x" to 539,"y" to 1199),540,1200,1080,2400))
        for (bad in listOf(obj("x" to 540,"y" to 0),obj("x" to -1,"y" to 0),obj("ref" to "n1","x" to 1,"y" to 2))) {
            try { UiContract.point(bad,540,1200,1080,2400); fail("invalid target accepted") } catch (_: AgentError) { }
        }
    }
    @Test fun insertPreservesTextAndReplacesOnlySelection() {
        assertEquals("ab中文🙂cd",UiContract.insert("abcd",2,2,"中文🙂"))
        assertEquals("a中d",UiContract.insert("abcd",3,1,"中"))
        try { UiContract.insert("abcd",-1,-1,"x"); fail("unknown selection accepted") } catch (_: AgentError) { }
    }
    @Test fun sharedIntentsRejectAmbiguousOrUnknownFields() {
        for (valid in listOf("""{"type":"click","target":{"ref":"n1"}}""", """{"type":"fill","text":""}""",
            """{"type":"scroll","direction":"down"}""", """{"type":"type_text","text":"中文🙂"}""")) UiContract.validate(JSONObject(valid))
        for (invalid in listOf("""{"type":"click","target":{"ref":"n1"},"command":"bad"}""",
            """{"type":"scroll","direction":"forward"}""", """{"type":"pinch","center":{"x":1,"y":1},"scale":0}""")) {
            try { UiContract.validate(JSONObject(invalid)); fail("invalid action accepted") } catch (_: AgentError) { }
        }
    }
}
