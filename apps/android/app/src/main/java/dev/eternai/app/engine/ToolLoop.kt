package dev.eternai.app.engine

import android.util.Log
import dev.eternai.app.tools.ToolCall
import dev.eternai.app.tools.ToolResult
import dev.eternai.app.tools.ToolRunner
import dev.eternai.core.MessageDto
import org.json.JSONArray
import org.json.JSONObject

/**
 * The tool-calling loop, shared by both engines and both connection states.
 *
 * PLAN §6: the model asks for a tool, the harness runs it, the result is fed
 * back, and the model continues. A tool that fails for a retryable reason is
 * retried exactly once; a second failure is reported to the model, which
 * decides what to do. The same rule and the same budget apply whether the calls
 * ran on the desktop or on the phone.
 *
 * Tool steps are returned as [ToolOutcome.steps] so they can be persisted with
 * the turn. They matter beyond this conversation: when the turn happened
 * offline they ride the outbox to the desktop, and the journal is written from
 * a transcript that includes what the model actually did, not just what it
 * said.
 */
class ToolLoop(
    private val engine: Engine,
    private val runner: ToolRunner,
    private val turnId: String,
) {
    data class ToolOutcome(
        val text: String,
        val steps: List<MessageDto>,
        val rounds: Int,
        val aborted: Boolean = false,
        /** The generation that produced [text]; null when the budget ran out. */
        val reply: Reply? = null,
    )

    /**
     * Runs the turn to completion.
     *
     * @param onChunk streams only the assistant's prose; tool traffic is
     *   reported separately by the caller's status text rather than mixed into
     *   the answer.
     */
    suspend fun run(
        messages: List<MessageDto>,
        onTool: (ToolResult) -> Unit = {},
        onChunk: (String) -> Unit,
    ): ToolOutcome {
        val working = messages.toMutableList()
        val steps = mutableListOf<MessageDto>()
        val tools = runner.definitions()
        var rounds = 0

        while (rounds < MAX_ROUNDS) {
            val reply = engine.generate(working, tools) { chunk ->
                if (chunk.text.isNotEmpty()) onChunk(chunk.text)
            }
            rounds++
            val calls = reply.toolCalls
            if (calls.isEmpty()) {
                return ToolOutcome(text = reply.text, steps = steps, rounds = rounds, reply = reply)
            }

            // The assistant's request has to be in the history before its
            // results, otherwise the next generation sees tool output with no
            // call to answer for.
            val toolCallsJson = calls.toJsonArray().toString()
            working += message("assistant", reply.text, null, toolCallsJson, null)
            steps += message("assistant", reply.text, null, toolCallsJson, null)

            val results = retryOnce(calls, runner.execute(calls))
            results.forEach { result ->
                onTool(result)
                // `asToolMessage` wraps failures so the model can see why the
                // call was rejected instead of just an opaque payload.
                val turn = message("tool", result.asToolMessage(), result.name, null, result.id)
                working += turn
                steps += turn
            }
        }

        Log.w(TAG, "tool loop hit the $MAX_ROUNDS round budget without an answer")
        return ToolOutcome(
            text = "I stopped after $MAX_ROUNDS tool rounds without reaching an answer.",
            steps = steps,
            rounds = rounds,
            aborted = true,
        )
    }

    /**
     * Retries retryable failures once, then merges them back into the original
     * call order so each result still lines up with its call.
     */
    private suspend fun retryOnce(calls: List<ToolCall>, results: List<ToolResult>): List<ToolResult> {
        val retryable = calls.filter { call -> results.firstOrNull { it.id == call.id }?.needsRetry() == true }
        if (retryable.isEmpty()) return results
        val retried = runner.execute(retryable).associateBy { it.id }
        return results.map { existing -> retried[existing.id] ?: existing }
    }

    private fun message(
        role: String,
        content: String,
        name: String?,
        toolCalls: String?,
        toolCallId: String?,
    ): MessageDto = MessageDto(
        role = role,
        content = content,
        name = name,
        toolCalls = toolCalls,
        toolCallId = toolCallId,
    )

    companion object {
        private const val TAG = "EternToolLoop"

        /** A model stuck in a loop must not spend the battery. */
        const val MAX_ROUNDS = 8
    }
}

/**
 * The wire form of a tool call request, used both as `assistant.tool_calls` in
 * the transcript and as the batch body for the gateway.
 */
internal fun List<ToolCall>.toJsonArray(): JSONArray = JSONArray().apply {
    forEach { call ->
        put(
            JSONObject()
                .put("id", call.id)
                .put("type", "function")
                .put(
                    "function",
                    JSONObject().put("name", call.name).put("arguments", call.arguments),
                ),
        )
    }
}
