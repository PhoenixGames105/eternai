package dev.eternai.app.tools

import dev.eternai.app.util.optStringOrNull
import org.json.JSONObject

/**
 * Reassembles `choices[0].delta.tool_calls` across SSE chunks.
 *
 * OpenAI-compatible servers stream a tool call in fragments: the first carries
 * `id` and `function.name`, later ones append to `function.arguments`, and the
 * `index` field says which call a fragment belongs to. Concatenating arguments
 * per index is the only way to get valid JSON out of it.
 */
class ToolCallAccumulator {

    private class Slot {
        var id: String = ""
        val name = StringBuilder()
        val arguments = StringBuilder()
    }

    private val slots = mutableMapOf<Int, Slot>()
    private var nextIndex = 0

    val isEmpty: Boolean get() = slots.isEmpty()

    /** Feeds one `delta` object; ignores it when it carries no tool calls. */
    fun absorb(delta: JSONObject) {
        val calls = delta.optJSONArray("tool_calls") ?: return
        for (i in 0 until calls.length()) {
            val call = calls.optJSONObject(i) ?: continue
            // `index` is optional in practice; fall back to arrival order.
            val index = if (call.has("index")) call.getInt("index") else nextIndex++
            val slot = slots.getOrPut(index) { Slot() }
            call.optStringOrNull("id")?.takeIf { it.isNotEmpty() }?.let { slot.id = it }
            val fn = call.optJSONObject("function") ?: continue
            // Servers repeat the name on every fragment (as "" or as the same
            // value), so keep only the first non-empty one.
            fn.optStringOrNull("name").takeIf { !it.isNullOrEmpty() }
                ?.let { if (slot.name.isEmpty()) slot.name.append(it) }
            fn.optStringOrNull("arguments")?.takeIf { it.isNotEmpty() }
                ?.let { slot.arguments.append(it) }
        }
    }

    /**
     * The completed calls, ordered by index.
     *
     * A call with no name cannot be executed, so it is dropped rather than
     * handed to a runner that would only report `unknowntool`.
     */
    fun result(): List<ToolCall> =
        slots.entries.sortedBy { it.key }
            .mapNotNull { (index, slot) ->
                val name = slot.name.toString().trim()
                if (name.isEmpty()) return@mapNotNull null
                ToolCall(
                    id = slot.id.ifEmpty { "call_$index" },
                    name = name,
                    arguments = slot.arguments.toString().ifEmpty { "{}" },
                )
            }
}
