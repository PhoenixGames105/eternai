package dev.eternai.app.sync

import dev.eternai.core.EternException
import dev.eternai.core.MessageDto
import dev.eternai.core.SyncTurnDto
import dev.eternai.core.TurnDto
import dev.eternai.core.turnMessages
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

data class ConversationTurn(
    val turnId: String,
    val messages: List<MessageDto>,
)

/**
 * The conversation the edge model actually sees, plus the separately synced transcript.
 *
 * The model *window* is replaced wholesale and remains budget-bounded. It is not the chat
 * transcript: *history* holds the wider server transcript shown in the UI, while *local*
 * retains phone turns until both transcript and context sync complete.
 *
 * The model prompt stays small by construction; transcript history is independent of it.
 */
class LocalContext(
    private val persist: (SyncTurnDto) -> Unit,
    private val persistHistory: (List<TurnDto>) -> Unit = {},
) {

    private val lock = Any()

    private var window: List<TurnDto> = emptyList()
    private var history: List<TurnDto> = emptyList()
    private val local: MutableList<SyncTurnDto> = mutableListOf()
    private val _turns = MutableStateFlow<List<ConversationTurn>>(emptyList())
    val turns: StateFlow<List<ConversationTurn>> = _turns.asStateFlow()

    /** Installs a window proposal, discarding the previous one. */
    fun setWindow(turns: List<TurnDto>) {
        synchronized(lock) {
            window = turns.toList()
        }
    }

    /** Installs server history for display without changing the model's context window. */
    fun setHistory(turns: List<TurnDto>) {
        val snapshot = turns.map { it.copy(messages = it.messages.toList()) }
        persistHistory(snapshot)
        synchronized(lock) {
            history = snapshot
            publishTurns()
        }
    }

    /** Restores the last cached server transcript at process startup. */
    fun restoreHistory(turns: List<TurnDto>) {
        synchronized(lock) {
            history = turns.map { it.copy(messages = it.messages.toList()) }
            publishTurns()
        }
    }

    /** Restores turns that are still waiting in the durable outbox. */
    fun restoreLocal(turns: List<SyncTurnDto>) {
        synchronized(lock) {
            val knownIds = history.mapTo(HashSet<String>()) { it.turnId }
            knownIds.addAll(local.map { it.turnId })
            turns.forEach { turn ->
                if (knownIds.add(turn.turnId)) {
                    local.add(turn.copy(messages = turn.messages.toList()))
                }
            }
            publishTurns()
        }
    }

    fun windowSize(): Int = synchronized(lock) { window.size }

    fun localSize(): Int = synchronized(lock) { local.size }

    /** Complete displayed conversation, oldest first, for rebuilding the edge window. */
    fun turnsForEdgeWindow(): List<TurnDto> = synchronized(lock) {
        val out = history.toMutableList()
        val knownIds = history.mapTo(HashSet<String>()) { it.turnId }
        local.forEachIndexed { index, turn ->
            if (knownIds.add(turn.turnId)) {
                out.add(
                    TurnDto(
                        turn.turnId,
                        turn.ts,
                        "chat",
                        "edge",
                        "",
                        "",
                        index.toLong(),
                        turn.messages.toList(),
                    ),
                )
            }
        }
        out
    }

    /**
     * Records a turn produced on the phone: into the live context *and* the durable
     * outbox. The outbox write is the point of no return — if it throws, the turn is not
     * added to the live context either, so the two can't disagree.
     */
    fun append(turn: SyncTurnDto) {
        persist(turn)
        synchronized(lock) {
            local.add(turn)
            publishTurns()
        }
    }

    /** Messages for the engine prompt, oldest first. */
    fun messages(): List<MessageDto> = synchronized(lock) {
        val out = ArrayList<MessageDto>()
        val windowIds = window.mapTo(HashSet<String>()) { it.turnId }
        window.forEach { out.addAll(it.messages) }
        local.filterNot { it.turnId in windowIds }.forEach { out.addAll(it.messages) }
        out
    }

    /** Clears local pending turns only after transcript and model-window sync both succeed. */
    fun clearLocal() {
        synchronized(lock) {
            local.clear()
            publishTurns()
        }
    }

    /**
     * Drops the whole conversation the edge model can see: the desktop-synced
     * window and the phone's own turns alike.
     *
     * This is the "my context is poisoned" lever. A run of garbage replies is
     * sticky — the window is re-installed on every sync — so clearing the local
     * half alone would be undone by the next pull. The desktop store has to be
     * cleared too (Settings → Danger zone in the desktop app); this only fixes
     * the phone's view of things.
     */
    fun clearAll() {
        synchronized(lock) {
            window = emptyList()
            history = emptyList()
            local.clear()
            persistHistory(emptyList())
            publishTurns()
        }
    }

    /** Throws if the core rejects a turn; callers surface it rather than swallowing. */
    @Throws(EternException::class)
    fun validate(turn: TurnDto): List<MessageDto> = turnMessages(turn)

    private fun publishTurns() {
        _turns.value = buildList {
            history.forEach { add(ConversationTurn(it.turnId, it.messages.toList())) }
            val knownIds = history.mapTo(HashSet<String>()) { it.turnId }
            local.forEach {
                if (knownIds.add(it.turnId)) {
                    add(ConversationTurn(it.turnId, it.messages.toList()))
                }
            }
        }
    }
}
