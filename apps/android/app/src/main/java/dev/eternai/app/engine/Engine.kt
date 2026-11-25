package dev.eternai.app.engine

import dev.eternai.app.tools.ToolCall
import dev.eternai.core.MessageDto
import org.json.JSONArray

/** A chunk of a reply as it streams in. */
data class Chunk(val text: String)

/** Why a reply ended. */
enum class Finish { Stop, Error, Cancelled }

data class Reply(
    val text: String,
    val finish: Finish,
    val engine: String,
    val model: String,
    val error: String? = null,
    /** Tool calls the model asked for in this round; empty for a plain answer. */
    val toolCalls: List<ToolCall> = emptyList(),
)

/**
 * The only thing the chat UI knows about where an answer comes from.
 *
 * M7b drops in a llama.cpp-backed implementation without touching the UI or the sync
 * layer, which is the whole reason this interface exists: the routing decision is already
 * shared Rust code, so the app must not care which engine answered.
 */
interface Engine {
    /** Stable name for the model chip and for the `engine` field of a turn. */
    val name: String

    /** Model identifier reported to the journal, or empty when unknown. */
    val model: String

    /**
     * Streams a completion. [onChunk] is called on a background dispatcher with each
     * text delta; the return value carries the assembled text and any tool calls the
     * model asked for. Implementations must not throw for expected failures (network
     * down, model busy) — they return `Finish.Error` so the caller can fall through to
     * the other engine.
     *
     * [tools] is the OpenAI `tools` array to advertise, or null to advertise none. An
     * engine that cannot do tool calling ignores it and never returns calls, which is
     * why [ToolLoop] probes rather than assumes.
     */
    suspend fun generate(
        messages: List<MessageDto>,
        tools: JSONArray? = null,
        onChunk: (Chunk) -> Unit,
    ): Reply
}
