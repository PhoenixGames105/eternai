package dev.eternai.app.sync

import android.util.Log
import dev.eternai.app.EternApplication
import dev.eternai.app.net.GatewayException
import dev.eternai.core.EternException
import dev.eternai.core.WindowResult
import dev.eternai.core.disposeWindow
import dev.eternai.core.parsePushAck
import dev.eternai.core.parseWindowProposal
import dev.eternai.core.pushRequestJson
import dev.eternai.core.windowQueryString
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import org.json.JSONObject
import java.io.File

/**
 * Moves offline conversations to the desktop and pulls the desktop's window back.
 *
 * The outbox is the single source of truth for what still owes the gateway: it is only
 * cleared after the gateway has acked the batch, so a crash or a dead battery mid-flush
 * loses nothing. The window pull *replaces* the local context rather than merging into it
 * — merging is how two sides drift apart.
 */
class OutboxSync(private val app: EternApplication) {

    private val prefs = app.prefs
    private val gateway = app.gateway
    private val outbox = app.outbox
    private val pendingDeletes = app.pendingDeletes
    private val memory = LocalMemoryStore(File(app.filesDir, "memory/MEMORY.md"))

    /** Serialises flushes so two reconnects can't push the same batch twice. */
    private val mutex = Mutex()

    /**
     * Pushes every pending turn. Returns a report even when the gateway is unreachable —
     * offline is the normal case here, so a failure must be data, not an exception.
     */
    suspend fun flush(): FlushReport = mutex.withLock {
        val device = prefs.device ?: return FlushReport(0, 0, 0, "not paired")

        val all = runCatching { outbox.readAll() }
            .getOrElse { return FlushReport(0, 0, 0, "outbox unreadable: ${describe(it)}") }

        if (all.skipped > 0) {
            Log.w(TAG, "outbox had ${all.skipped} torn line(s); they were skipped")
        }
        if (all.turns.isEmpty()) return FlushReport(0, 0, 0, null)

        val request = pushRequestJson(device.deviceId, all.turns)
        if (request.truncated) {
            Log.w(TAG, "batch truncated to ${request.count} newest turn(s)")
        }

        try {
            val ack = parsePushAck(gateway.post(PATH_PUSH, device, request.json))
            // Only now is the local copy expendable.
            outbox.clear()
            Log.i(TAG, "pushed ${request.count} turn(s), accepted ${ack.accepted}, dup ${ack.duplicates}")
            FlushReport(
                delivered = request.count.toInt(),
                failed = 0,
                remaining = 0,
                // Replays are successful idempotent delivery. Treating them as
                // failure skips transcript/window pulls and leaves the edge
                // context stale even though the gateway already has the turns.
                error = null,
            )
        } catch (e: GatewayException) {
            if (e.status == 401) {
                // The token will never work again (revoked or re-paired). Say so loudly;
                // retrying silently would hide a pairing problem for days.
                return FlushReport(0, request.count.toInt(), request.count.toInt(), "device token rejected (401) — re-pair")
            }
            FlushReport(0, request.count.toInt(), request.count.toInt(), describe(e))
        } catch (e: EternException) {
            FlushReport(0, request.count.toInt(), request.count.toInt(), describe(e))
        }
    }

    /**
     * Pulls the desktop's view of the last `edge_ctx` tokens and installs it as the local
     * context. Call after a successful [flush] so the window includes what was just pushed.
     */
    suspend fun pullWindow(
        edgeCtx: Long = DEFAULT_EDGE_CTX,
        responseReserve: Long = DEFAULT_RESPONSE_RESERVE,
        overhead: Long = DEFAULT_EDGE_OVERHEAD,
    ): WindowPull {
        val device = prefs.device ?: return WindowPull.Error("not paired")

        val query = windowQueryString(edgeCtx, responseReserve, overhead, device.deviceId)

        val body =
            try {
                gateway.get("$PATH_WINDOW?$query", device)
            } catch (e: GatewayException) {
                return WindowPull.Error(describe(e))
            }

        val proposal =
            try {
                parseWindowProposal(body)
            } catch (e: EternException) {
                return WindowPull.Error("window unreadable: ${describe(e)}")
            }

        // The phone's own budget decides the final trim; the desktop's proposal is only a
        // candidate. `null` token costs means the shared char/4 estimate is used.
        val result: WindowResult =
            try {
                disposeWindow(proposal.turns, edgeCtx, responseReserve, overhead, null)
            } catch (e: EternException) {
                return WindowPull.Error("window dispose failed: ${describe(e)}")
            }

        return try {
            app.context.setWindow(result.turns)
            proposal.windowSeq?.let { prefs.lastSeenWindowSeq = it }
            Log.i(
                TAG,
                "window installed: ${result.turns.size} turn(s), ${result.tokensUsed}/${result.budget} tokens" +
                    if (result.truncated) " (trimmed)" else "",
            )
            WindowPull.Installed(proposal.turns.size, result.turns.size, result.estimated)
        } catch (e: EternException) {
            WindowPull.Error("window install failed: ${describe(e)}")
        }
    }

    /** Flush then pull: the whole reconnect handshake in one call. */
    suspend fun syncNow(): SyncOutcome {
        val report = flush()
        if (!report.ok) return SyncOutcome(report, null, null, null, null, null)
        // Replayed before the pulls below, so a turn deleted offline is already gone
        // from the desktop store by the time we ask it for a transcript and a window.
        val deletes = drainDeletes()
        val memoryReport = syncMemory()
        val personalityReport = syncPersonality()
        val transcript = pullTranscript()
        val pull = pullWindow()
        return SyncOutcome(report, pull, memoryReport, personalityReport, transcript, deletes)
    }

    /**
     * Deletes one exchange: always locally, and on the desktop as soon as we can.
     *
     * When paired and reachable the gateway call goes out immediately and the local
     * copy goes with it. Offline — or unpaired — the id is queued in
     * [PendingDeletes] and replayed by [drainDeletes] on the next successful sync.
     * Either way the phone stops showing and stops prompting with the turn right
     * away; waiting for a connection to hide a message the user just deleted would
     * be the wrong trade.
     *
     * Returns a human-readable account of what happened, suitable for a notice.
     */
    suspend fun deleteTurn(turnId: String): DeleteOutcome {
        if (turnId.isBlank()) return DeleteOutcome.Failed("no turn id")

        val removed = app.context.deleteTurn(turnId)
        // No window-watermark reset is needed: `pullWindow` and `pullTranscript`
        // are unconditional and *replace* what we hold, so the deleted turn can
        // only come back if the desktop still has it — which is what the rest of
        // this function is for.

        val device = prefs.device
            ?: return DeleteOutcome.Queued(
                removed,
                "not paired, so only the phone's copy was removed",
            )

        val body = JSONObject().put("turn_id", turnId).toString()
        return try {
            val response = JSONObject(gateway.post(PATH_DELETE_TURN, device, body))
            if (response.has("error")) {
                val message = response.optJSONObject("error")?.optString("message") ?: "gateway refused"
                DeleteOutcome.Queued(removed, "desktop still holds it: $message")
            } else {
                DeleteOutcome.Removed(removed, response.optLong("deleted", 0L))
            }
        } catch (e: GatewayException) {
            // 404 means the desktop already lacks the turn — nothing to queue.
            if (e.status == 404) return DeleteOutcome.Removed(removed, 0L)
            pendingDeletes.add(turnId)
            DeleteOutcome.Queued(removed, describe(e))
        } catch (e: Exception) {
            pendingDeletes.add(turnId)
            DeleteOutcome.Queued(removed, describe(e))
        }
    }

    /**
     * Replays queued deletions against the desktop.
     *
     * Each id is sent on its own and dropped from the queue only when the gateway
     * accepts it (a 404 counts: the turn is already gone). An id the gateway
     * refuses with 4xx other than 404 is dropped too — a 403 will never succeed on
     * retry, and a queue that never drains would re-send the same batch forever.
     * Transport failures keep their place for the next sync.
     */
    private suspend fun drainDeletes(): DeletesReport {
        val queued = pendingDeletes.peek()
        if (queued.isEmpty()) return DeletesReport(0, 0, 0, null)
        val device = prefs.device ?: return DeletesReport(0, 0, queued.size, "not paired")

        var done = 0
        var error: String? = null
        val stillQueued = mutableListOf<String>()

        for ((index, turnId) in queued.withIndex()) {
            val body = JSONObject().put("turn_id", turnId).toString()
            // 4xx counts as settled: a 404 means the desktop already lacks the
            // turn, and a 403 on a foreign turn will never succeed on retry.
            // Keeping either in the queue would re-send it on every sync forever.
            val settled = try {
                gateway.post(PATH_DELETE_TURN, device, body)
                true
            } catch (e: GatewayException) {
                error = describe(e)
                e.status in 400..499
            } catch (e: Exception) {
                error = describe(e)
                false
            }

            if (settled) {
                done++
            } else {
                // The link died mid-drain; keep this id and everything after it,
                // in order, for the next sync.
                stillQueued.addAll(queued.drop(index))
                break
            }
        }

        pendingDeletes.replaceAll(stillQueued)
        if (done > 0) {
            Log.i(TAG, "replayed $done queued deletion(s), ${stillQueued.size} still queued")
        }
        return DeletesReport(done, stillQueued.size, queued.size, error)
    }

    /** Pulls the wider UI transcript independently of the edge model's small window. */
    private suspend fun pullTranscript(): TranscriptPull {
        val device = prefs.device ?: return TranscriptPull.Error("not paired")
        return try {
            val body = gateway.get(PATH_TRANSCRIPT, device)
            val turns = parseTranscriptTurns(body)
            app.context.setHistory(turns)
            TranscriptPull.Installed(turns.size)
        } catch (e: Exception) {
            TranscriptPull.Error(describe(e))
        }
    }

    private suspend fun syncMemory(): MemorySyncReport {
        val device = prefs.device ?: return MemorySyncReport("not paired")
        return try {
            val local = memory.read()
            val response = JSONObject(
                gateway.post(
                    PATH_MEMORY_SYNC,
                    device,
                    JSONObject().put("content", local).toString(),
                ),
            )
            if (!response.has("content")) {
                throw IllegalStateException("gateway memory response has no content")
            }
            memory.merge(response.getString("content"))
            MemorySyncReport()
        } catch (e: Exception) {
            MemorySyncReport(describe(e))
        }
    }

    suspend fun syncPersonality(): PersonalitySyncReport {
        val device = prefs.device ?: return PersonalitySyncReport("not paired")
        return try {
            val response = JSONObject(
                gateway.post(
                    PATH_PERSONALITY_SYNC,
                    device,
                    JSONObject()
                        .put("value", prefs.personality)
                        .put("updated_at", prefs.personalityUpdatedAt)
                        .toString(),
                ),
            )
            if (!response.has("value") || !response.has("updated_at")) {
                throw IllegalStateException("gateway personality response is incomplete")
            }
            val value = response.getString("value")
            val updatedAt = response.getLong("updated_at")
            prefs.savePersonality(value, updatedAt)
            PersonalitySyncReport()
        } catch (e: Exception) {
            PersonalitySyncReport(describe(e))
        }
    }

    /**
     * Asks the desktop to delete this device's stored turns.
     *
     * Without this, clearing the phone is pointless once the desktop holds the
     * garbage: the next window pull would reinstall the same poisoned turns. The
     * gateway restricts a device token to its own turns, so a lost phone cannot
     * wipe the rest of the transcript.
     *
     * Returns the number of turns deleted, or a message explaining why not.
     */
    suspend fun clearRemoteHistory(): Result<Long> {
        val device = prefs.device ?: return Result.failure(IllegalStateException("not paired"))
        return runCatching {
            val body = gateway.post(PATH_CLEAR, device, """{"confirm":true}""")
            val json = JSONObject(body)
            if (json.has("error")) error(json.optJSONObject("error")?.optString("message") ?: "gateway refused")
            json.optLong("cleared", 0L)
        }
    }

    private fun describe(t: Throwable): String =
        when (t) {
            is GatewayException -> "gateway ${t.status}: ${t.message}"
            else -> t.message ?: t.toString()
        }

    data class FlushReport(
        val delivered: Int,
        val failed: Int,
        val remaining: Int,
        val error: String? = null,
    ) {
        val ok: Boolean get() = error == null && failed == 0
    }

    sealed class WindowPull {
        data class Installed(val proposed: Int, val installed: Int, val estimated: Boolean) : WindowPull()

        data class Error(val message: String) : WindowPull()
    }

    sealed class TranscriptPull {
        data class Installed(val turns: Int) : TranscriptPull()

        data class Error(val message: String) : TranscriptPull()
    }

    data class SyncOutcome(
        val flush: FlushReport,
        val window: WindowPull?,
        val memory: MemorySyncReport?,
        val personality: PersonalitySyncReport?,
        val transcript: TranscriptPull?,
        val deletes: DeletesReport? = null,
    )

    /** Outcome of replaying the offline delete queue. */
    data class DeletesReport(
        val replayed: Int,
        val remaining: Int,
        val queued: Int,
        val error: String? = null,
    )

    /** Outcome of one user-initiated deletion. */
    sealed class DeleteOutcome {
        /** True when the turn left the phone's own context. */
        abstract val removedLocally: Boolean

        /** The desktop confirmed it. */
        data class Removed(override val removedLocally: Boolean, val deleted: Long) : DeleteOutcome()

        /** The phone forgot it; the desktop will be told on the next sync. */
        data class Queued(override val removedLocally: Boolean, val reason: String) : DeleteOutcome()

        data class Failed(val message: String) : DeleteOutcome() {
            override val removedLocally: Boolean get() = false
        }
    }
    data class MemorySyncReport(val error: String? = null) {
        val ok: Boolean get() = error == null
    }
    data class PersonalitySyncReport(val error: String? = null) {
        val ok: Boolean get() = error == null
    }

    companion object {
        private const val TAG = "EternSync"
        const val PATH_PUSH = "/etern/sync/push"
        const val PATH_WINDOW = "/etern/sync/window"
        const val PATH_TRANSCRIPT = "/etern/sync/transcript"
        const val PATH_MEMORY_SYNC = "/etern/memory/sync"
        const val PATH_PERSONALITY_SYNC = "/etern/personality/sync"
        const val PATH_CLEAR = "/etern/history/clear"
        const val PATH_DELETE_TURN = "/etern/history/turn/delete"
        const val DEFAULT_EDGE_CTX = 8192L
        const val DEFAULT_RESPONSE_RESERVE = 700L
        const val DEFAULT_EDGE_OVERHEAD = 400L
    }
}
