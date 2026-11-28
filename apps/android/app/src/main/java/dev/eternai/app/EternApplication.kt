package dev.eternai.app

import android.app.Application
import android.util.Log
import dev.eternai.app.data.Prefs
import dev.eternai.app.engine.LlamaEdgeEngine
import dev.eternai.app.engine.ServerEdgeEngine
import dev.eternai.app.net.GatewayClient
import dev.eternai.app.net.HealthProbe
import dev.eternai.app.sync.LocalContext
import dev.eternai.app.sync.OutboxSync
import dev.eternai.app.sync.PendingDeletes
import dev.eternai.app.sync.TranscriptCache
import dev.eternai.core.Outbox
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import java.io.File

/**
 * Process-wide singletons.
 *
 * The outbox is opened eagerly because its path is fixed and opening it is how the
 * directory gets created; everything else is lazy so a cold start never blocks on disk
 * or network. A UniFFI `Outbox` is a handle to a Rust object, so it lives here rather
 * than in a Compose ViewModel that Android may recreate.
 */
class EternApplication : Application() {

    lateinit var prefs: Prefs
        private set
    lateinit var gateway: GatewayClient
        private set
    lateinit var probe: HealthProbe
        private set
    lateinit var outbox: Outbox
        private set
    lateinit var context: LocalContext
        private set
    lateinit var sync: OutboxSync
        private set

    /** Turns deleted while offline, replayed against the desktop on the next sync. */
    internal lateinit var pendingDeletes: PendingDeletes
        private set

    /** Embedded llama.cpp server reached over loopback; the default edge engine. */
    lateinit var serverEngine: ServerEdgeEngine
        private set

    /** Hand-written JNI engine, kept as a diagnostic fallback only. */
    lateinit var legacyEdgeEngine: LlamaEdgeEngine
        private set

    /** Survives activities; cancelled only if the process dies. */
    val appScope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val syncMutex = Mutex()
    private var periodicSyncJob: Job? = null
    private var lastSyncWarning: String? = null
    @Volatile
    private var turnInProgress = false

    override fun onCreate() {
        super.onCreate()

        prefs = Prefs(this)
        gateway = GatewayClient(prefs)
        probe = HealthProbe(gateway)
        outbox = Outbox(File(filesDir, OUTBOX_FILE).absolutePath)
        val transcriptCache = TranscriptCache(File(filesDir, TRANSCRIPT_CACHE_FILE))
        context = LocalContext(outbox::appendSyncTurn, transcriptCache::write)
        try {
            context.restoreHistory(transcriptCache.read())
        } catch (e: Exception) {
            Log.e(TAG, "cached transcript could not be restored", e)
        }
        try {
            val pending = outbox.readAll()
            if (pending.skipped > 0) {
                Log.w(TAG, "outbox had ${pending.skipped} torn line(s) during startup")
            }
            context.restoreLocal(pending.turns)
        } catch (e: Exception) {
            Log.e(TAG, "pending turns could not be restored from outbox", e)
        }
        pendingDeletes = PendingDeletes(File(filesDir, PENDING_DELETES_FILE))
        sync = OutboxSync(this)
        // Constructing the engines is cheap; the GGUF itself is only mapped into
        // memory the first time a prompt actually needs it.
        serverEngine = ServerEdgeEngine(this, prefs)
        legacyEdgeEngine = LlamaEdgeEngine(this, prefs)

        if (prefs.paired) startServices()
    }

    /** Called on a successful pair and on cold start when already paired. */
    fun startServices() {
        gateway.invalidate()
        appScope.launch { syncNow() }
        probe.start(appScope, prefs)
        if (periodicSyncJob?.isActive != true) {
            periodicSyncJob = appScope.launch(Dispatchers.IO) {
                while (prefs.paired) {
                    if (probe.state.value.decision.target == "high_power") {
                        try {
                            val outcome = syncIfIdle()
                            if (outcome != null) {
                                val warning = when {
                                    !outcome.flush.ok -> outcome.flush.error ?: "outbox flush failed"
                                    outcome.memory?.error != null ->
                                        "memory sync failed: ${outcome.memory.error}"
                                    outcome.personality?.error != null ->
                                        "personality sync failed: ${outcome.personality.error}"
                                    outcome.transcript is OutboxSync.TranscriptPull.Error ->
                                        outcome.transcript.message
                                    outcome.window is OutboxSync.WindowPull.Error ->
                                        outcome.window.message
                                    else -> null
                                }
                                if (warning != lastSyncWarning) {
                                    if (warning != null) Log.w(TAG, "background sync failed: $warning")
                                    lastSyncWarning = warning
                                }
                            }
                        } catch (e: Exception) {
                            val warning = e.message ?: e.javaClass.simpleName
                            if (warning != lastSyncWarning) {
                                Log.w(TAG, "background sync failed: $warning", e)
                                lastSyncWarning = warning
                            }
                        }
                    }
                    delay(SYNC_INTERVAL_MS)
                }
            }
        }
    }

    /**
     * Hands the outbox to the desktop, then pulls its window back. The local half of the
     * context is only dropped once the desktop has both taken the turns and offered a
     * window that contains them.
     */
    suspend fun syncNow(): OutboxSync.SyncOutcome {
        return syncMutex.withLock { syncLocked() }
    }

    suspend fun beginTurn() {
        syncMutex.withLock { turnInProgress = true }
    }

    suspend fun endTurn() {
        syncMutex.withLock { turnInProgress = false }
    }

    suspend fun appendTurn(turn: dev.eternai.core.SyncTurnDto) {
        syncMutex.withLock { context.append(turn) }
    }

    /**
     * Deletes one exchange everywhere: the phone's context, the outbox, and the
     * desktop's copy — or a tombstone if the desktop is out of reach.
     *
     * Taking the turn out of the outbox matters as much as taking it out of the
     * context. An offline turn that was never pushed would otherwise be flushed
     * to the desktop on the next reconnect, and `onCreate` restores the visible
     * transcript from that same file, so a deleted message would come back after
     * a restart. The outbox has no delete method (it is an append-only insert
     * log acked as one batch), so it is rewritten from what survives.
     *
     * Serialised on [syncMutex] so a periodic flush cannot read the outbox
     * mid-rewrite.
     */
    suspend fun deleteTurn(turnId: String): OutboxSync.DeleteOutcome = syncMutex.withLock {
        dropOutboxTurn(turnId)
        sync.deleteTurn(turnId)
    }

    private fun dropOutboxTurn(turnId: String) {
        val pending = runCatching { outbox.readAll() }.getOrNull() ?: return
        if (pending.turns.none { it.turnId == turnId }) return
        val kept = pending.turns.filter { it.turnId != turnId }
        try {
            outbox.clear()
            kept.forEach { outbox.appendSyncTurn(it) }
            Log.i(TAG, "removed $turnId from the outbox")
        } catch (e: Exception) {
            Log.e(TAG, "outbox could not be rewritten after a delete", e)
        }
    }

    suspend fun clearConversation(): Result<Long> = syncMutex.withLock {
        val paired = prefs.paired
        if (paired) sync.flush()
        val remote = if (paired) sync.clearRemoteHistory() else Result.failure(
            IllegalStateException("not paired"),
        )
        runCatching { outbox.clear() }
        // The whole transcript is gone; tombstones for individual turns have
        // nothing left to protect against.
        runCatching { pendingDeletes.replaceAll(emptyList()) }
        context.clearAll()
        remote
    }

    private suspend fun syncIfIdle(): OutboxSync.SyncOutcome? = syncMutex.withLock {
        if (turnInProgress) null else syncLocked()
    }

    private suspend fun syncLocked(): OutboxSync.SyncOutcome {
        val outcome = sync.syncNow()
        if (outcome.window is OutboxSync.WindowPull.Installed &&
            outcome.transcript is OutboxSync.TranscriptPull.Installed
        ) {
            context.clearLocal()
        }
        return outcome
    }

    private companion object {
        const val TAG = "EternApplication"
        const val SYNC_INTERVAL_MS = 5_000L
    }
}

private const val OUTBOX_FILE = "outbox.jsonl"
private const val PENDING_DELETES_FILE = "pending-deletes.jsonl"
private const val TRANSCRIPT_CACHE_FILE = "transcript.json"
