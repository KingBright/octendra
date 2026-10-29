package `fun`.hackerlife.octendra

/** A late loss event for the old route must not tear down its replacement. */
class NetworkRecoveryState {
    private var current: Long? = null
    private var validated = false
    @Synchronized fun available(id: Long): Boolean {
        if (current == id) return false
        val replaced = current != null
        current = id; validated = false
        return replaced
    }
    @Synchronized fun lost(id: Long): Boolean {
        if (current != id) return false
        current = null; validated = false
        return true
    }
    @Synchronized fun validated(id: Long): Boolean {
        if (current != id || validated) return false
        validated = true
        return true
    }
}
