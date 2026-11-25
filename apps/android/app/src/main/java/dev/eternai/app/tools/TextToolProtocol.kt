package dev.eternai.app.tools

import org.json.JSONArray
import org.json.JSONObject

/**
 * Tool calling for a model whose chat template cannot render schemas.
 *
 * llama.cpp's `llama_chat_apply_template` takes only role and content — there is
 * no `tools` parameter anywhere in its template API — so the edge model cannot
 * be given schemas natively the way the 200k model is. Instead the schemas are
 * written into the system prompt as text and the model is asked to answer a
 * tool request with a single JSON object on its own line. That keeps one tool
 * contract across both models (same names, same arguments, same results) and
 * confines the deviation to how the request is *encoded*, which is the part a
 * template limitation actually affects.
 */
object TextToolProtocol {

    /** The marker the model is told to use; also what [parse] looks for. */
    private const val MARKER = "ETOOL"

    /**
     * Prompt text describing [definitions] (an OpenAI `tools` array).
     *
     * Deliberately terse: PLAN §6 counts the schema block as a real token cost
     * on an 8k context, so only name, one-line description and required
     * argument names are included.
     */
    fun instructions(definitions: JSONArray): String {
        if (definitions.length() == 0) return ""
        val sb = StringBuilder()
        sb.append(
            "You can call tools. Available tools:\n",
        )
        for (i in 0 until definitions.length()) {
            val fn = definitions.optJSONObject(i)?.optJSONObject("function") ?: continue
            val name = fn.optString("name")
            if (name.isEmpty()) continue
            sb.append("- ").append(name)
            fn.optString("description").takeIf { it.isNotEmpty() }?.let { sb.append(": ").append(it) }
            val props = fn.optJSONObject("parameters")?.optJSONObject("properties")
            if (props != null && props.length() > 0) {
                sb.append(" args{")
                val names = props.keys().asSequence().toList()
                names.forEachIndexed { index, key ->
                    if (index > 0) sb.append(",")
                    sb.append(key)
                }
                sb.append("}")
            }
            sb.append('\n')
        }
        sb.append(
            "To call a tool, reply with exactly one line and nothing else:\n" +
                "$MARKER {\"tool\":\"<name>\",\"arguments\":{...}}\n" +
                "Wait for the result before continuing. Only call a tool if you need it.\n",
        )
        return sb.toString()
    }

    /**
     * Extracts a tool request from [text], or null when the model just answered.
     *
     * Tolerates a missing marker, a code fence, and prose around the object,
     * because small models rarely obey formatting exactly. Arguments must parse
     * as a JSON object; anything else is treated as no request rather than
     * guessed at, since a wrong argument is worse than no call.
     */
    fun parse(text: String): ToolCall? {
        val start = text.indexOf('{')
        if (start < 0) return null
        val json = extractObject(text, start) ?: return null
        val obj = try {
            JSONObject(json)
        } catch (e: org.json.JSONException) {
            return null
        }
        val name = obj.optString("tool").ifEmpty { obj.optString("name") }
        if (name.isEmpty()) return null
        val args = obj.optJSONObject("arguments") ?: JSONObject()
        return ToolCall(
            id = obj.optString("id").ifEmpty { "text_0" },
            name = name,
            arguments = args.toString(),
        )
    }

    /**
     * The answer with the tool request removed, so the user sees prose rather
     * than a protocol line.
     */
    fun strip(text: String): String {
        val start = text.indexOf(MARKER)
        if (start < 0) return text.trim()
        return (text.substring(0, start) + text.substring(start + MARKER.length))
            .replace(Regex("\\{\\s*\"tool\"\\s*:[^{}]*(\\{[^{}]*\\}[^{}]*)?}"), "")
            .trim()
    }

    /** Balanced-brace scan from [start]; null when the object never closes. */
    private fun extractObject(text: String, start: Int): String? {
        var depth = 0
        var inString = false
        var escaped = false
        for (i in start until text.length) {
            val c = text[i]
            when {
                escaped -> escaped = false
                c == '\\' && inString -> escaped = true
                c == '"' -> inString = !inString
                inString -> Unit
                c == '{' -> depth++
                c == '}' -> {
                    depth--
                    if (depth == 0) return text.substring(start, i + 1)
                }
            }
        }
        return null
    }
}
