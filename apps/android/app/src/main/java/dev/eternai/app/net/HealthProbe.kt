package dev.eternai.app.net

import dev.eternai.app.data.Prefs
import dev.eternai.core.HealthInfo
import dev.eternai.core.RouteDecision
import dev.eternai.core.parseHealth
import dev.eternai.core.probeBackoffMs
import dev.eternai.core.route
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * Watches the gateway and publishes the routing decision.
 *
 * The backoff ladder comes from `probeBackoffMs` in Rust so the phone and any
 * future desktop-side probe share one curve. A probe is a single HTTPS
 * handshake plus `GET /health`; on a home LAN that is a few milliseconds, so
 * polling while online is cheap and detects "walked out the door" quickly.
 */
class HealthProbe(private val gateway: GatewayClient) {

    data class State(
        val decision: RouteDecision,
        val health: HealthInfo?,
        val consecutiveFailures: Long,
        val lastCheckMs: Long,
    )

    private val _state = MutableStateFlow(
        State(route(false, false), null, 0, 0)
    )
    val state: StateFlow<State> = _state.asStateFlow()

    private var job: Job? = null

    /** Online when a prompt sent right now would reach the desktop. */
    val isOnline: Boolean
        get() = _state.value.decision.target == "high_power"

    fun start(scope: CoroutineScope, prefs: Prefs) {
        if (job?.isActive == true) return
        job = scope.launch { run(prefs) }
    }

    fun stop() {
        job?.cancel()
        job = null
    }

    /** Runs until cancelled, probing at the interval the ladder dictates. */
    suspend fun run(prefs: Prefs) = withContext(Dispatchers.IO) {
        // `delay` is cancellation-aware, so it is the loop's only exit for a
        // cancelled scope; no explicit job polling needed.
        while (true) {
            val outcome = check()
            val waitMs = if (outcome.consecutiveFailures == 0L) {
                ONLINE_INTERVAL_MS
            } else {
                probeBackoffMs(outcome.consecutiveFailures)
            }
            delay(waitMs.coerceAtMost(MAX_INTERVAL_MS))
            if (!prefs.paired) return@withContext
        }
    }

    /** One probe. Safe to call from anywhere; never throws. */
    suspend fun check(): State = withContext(Dispatchers.IO) {
        val previous = _state.value
        try {
            val health = parseHealth(gateway.probe())
            if (!health.reachable) {
                // Answered with something that isn't /health — treat as down but
                // keep counting, since a captive portal does this constantly.
                val failures = previous.consecutiveFailures + 1
                publish(failures, null).also {
                    if (previous.consecutiveFailures < FAILURES_BEFORE_OFFLINE &&
                        failures >= FAILURES_BEFORE_OFFLINE
                    ) {
                        android.util.Log.w(TAG, "gateway unreachable after $failures probes")
                    }
                }
            } else {
                if (previous.consecutiveFailures >= FAILURES_BEFORE_OFFLINE) {
                    android.util.Log.i(TAG, "gateway back: ${health.engineState}")
                }
                publish(0, health)
            }
        } catch (e: Exception) {
            val failures = previous.consecutiveFailures + 1
            if (failures == FAILURES_BEFORE_OFFLINE) {
                android.util.Log.i(TAG, "going offline: ${e.message}")
            }
            publish(failures, null)
        }
    }

    private fun publish(failures: Long, health: HealthInfo?): State {
        // Below the threshold a single dropped probe is noise, not failover:
        // one blip must not send every prompt to the edge model.
        val reachable = failures < FAILURES_BEFORE_OFFLINE
        val decision = route(reachable, health?.engineServing ?: false)
        val next = State(decision, health, failures, System.currentTimeMillis())
        _state.value = next
        return next
    }

    companion object {
        private const val TAG = "EternProbe"
        private const val ONLINE_INTERVAL_MS = 5_000L
        private const val MAX_INTERVAL_MS = 60_000L

        /** Probes that must fail before the phone declares itself offline. */
        const val FAILURES_BEFORE_OFFLINE = 3L
    }
}
