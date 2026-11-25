package dev.eternai.app.engine

import dev.eternai.app.tools.TextToolProtocol
import dev.eternai.core.MessageDto
import org.json.JSONArray
import org.json.JSONObject

/**
 * Flattens a conversation into the `(role, content)` pairs the JNI layer takes.
 *
 * llama.cpp's template API has no notion of tools: `llama_chat_apply_template`
 * accepts only role and content strings. Tool traffic therefore has to become
 * ordinary text, and it is encoded in the same [TextToolProtocol] shape the
 * model is asked to emit, so a replayed conversation reads to the model exactly
 * like the one that produced it.
 *
 * Tool results are attributed to `user` rather than `tool`: most chat templates
 * only handle system/user/assistant and reject anything else, and a result the
 * template refuses to render is a result the model never sees.
 */
object EdgePrompt {

    /** Renders [messages], appending the tool instructions to the system prompt. */
    fun render(messages: List<MessageDto>, tools: JSONArray?): List<Pair<String, String>> {
        val instructions = tools?.let { TextToolProtocol.instructions(it) }.orEmpty()
        val out = mutableListOf<Pair<String, String>>()
        var injected = instructions.isEmpty()

        for (m in messages) {
            val toolCallId = m.toolCallId
            val toolCalls = m.toolCalls
            val content = when {
                toolCallId != null -> "[tool ${m.name ?: toolCallId} result] ${m.content}"
                !toolCalls.isNullOrEmpty() -> withToolCalls(m.content, toolCalls)
                else -> m.content
            }
            val role = if (toolCallId != null) "user" else m.role

            if (!injected && role == "system") {
                out += "system" to (content + "\n" + instructions)
                injected = true
                continue
            }
            out += role to content
        }

        if (!injected) {
            // No system message to attach to (a fresh conversation, or a window
            // that started mid-exchange), so lead with one.
            out.add(0, "system" to instructions.trim())
        }
        return out
    }

    /** Rewrites an assistant message so its tool calls appear as protocol lines. */
    private fun withToolCalls(text: String, toolCallsJson: String): String {
        val calls = try {
            JSONArray(toolCallsJson)
        } catch (e: org.json.JSONException) {
            return text
        }
        val sb = StringBuilder(text)
        for (i in 0 until calls.length()) {
            val call = calls.optJSONObject(i) ?: continue
            val fn = call.optJSONObject("function") ?: continue
            val args = try {
                JSONObject(fn.optString("arguments").ifEmpty { "{}" })
            } catch (e: org.json.JSONException) {
                JSONObject()
            }
            val line = JSONObject()
                .put("tool", fn.optString("name"))
                .put("arguments", args)
                .toString()
            if (sb.isNotEmpty()) sb.append('\n')
            sb.append("ETOOL ").append(line)
        }
        return sb.toString()
    }
}
