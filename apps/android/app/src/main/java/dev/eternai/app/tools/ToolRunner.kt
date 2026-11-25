package dev.eternai.app.tools

import org.json.JSONArray
import org.json.JSONObject

/** One tool call the model asked for, as it appears in an OpenAI delta. */
data class ToolCall(
    val id: String,
    val name: String,
    val arguments: String,
)

/** The outcome of executing one call, in the shape both runners return. */
data class ToolResult(
    val id: String,
    val name: String,
    val ok: Boolean,
    val retryable: Boolean,
    /** Serialised `content` on success, serialised `error` object on failure. */
    val payload: String,
) {
    /**
     * The body of the `role:"tool"` message fed back to the model. Successes
     * pass the content through untouched; failures are wrapped so the model
     * can see why the call was rejected and correct itself.
     */
    fun asToolMessage(): String =
        if (ok) {
            payload
        } else {
            JSONObject()
                .put("ok", false)
                .put("retryable", retryable)
                .put("error", JSONObject(payload))
                .toString()
        }

    fun needsRetry(): Boolean = !ok && retryable
}

/** An assistant turn that asked for tools: any text produced, plus the calls. */
data class ToolRequest(val text: String, val calls: List<ToolCall>)

/**
 * Executes model tool calls wherever the harness happens to be.
 *
 * The gateway and the on-device runtime return byte-identical result objects
 * (`{"ok":true,"content":...}` / `{"ok":false,"retryable":...,"error":{...}}`),
 * so [dev.eternai.app.engine.ToolLoop] does not care which one sits behind an
 * implementation.
 */
interface ToolRunner {
    /** Label for diagnostics ("gateway" or "edge"). */
    val label: String

    /**
     * Turn the next calls belong to. Set before each turn so `remember` entries
     * and gateway logs carry the same provenance as the desktop's own turns.
     */
    var turnId: String

    /** The OpenAI `tools` array to embed in a chat request, or null for none. */
    suspend fun definitions(): JSONArray?

    /** Executes [calls] in order. Implementations must not throw per-call. */
    suspend fun execute(calls: List<ToolCall>): List<ToolResult>
}

/** Parses the gateway's `{"results":[...]}` envelope into [ToolResult]s. */
internal fun parseToolResults(envelope: JSONObject, calls: List<ToolCall>): List<ToolResult> {
    val results = envelope.optJSONArray("results") ?: JSONArray()
    return (0 until results.length()).map { i ->
        val obj = results.optJSONObject(i) ?: JSONObject()
        // The gateway echoes `id` only when the client supplied one; fall back
        // to positional order so a tool message always matches its call.
        val call = calls.getOrNull(i)
        ToolResult(
            id = obj.optString("id").ifEmpty { call?.id ?: "" },
            name = obj.optString("name").ifEmpty { call?.name ?: "" },
            ok = obj.optBoolean("ok", false),
            retryable = obj.optBoolean("retryable", false),
            payload = if (obj.has("content")) {
                obj.get("content").toString()
            } else {
                obj.optJSONObject("error")?.toString() ?: JSONObject().put("message", "unknown error").toString()
            },
        )
    }
}
