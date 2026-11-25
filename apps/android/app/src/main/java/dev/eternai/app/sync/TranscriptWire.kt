package dev.eternai.app.sync

import dev.eternai.core.MessageDto
import dev.eternai.core.TurnDto
import org.json.JSONObject

internal fun parseTranscriptTurns(body: String): List<TurnDto> {
    val turns = JSONObject(body).getJSONArray("turns")
    return List(turns.length()) { turnIndex ->
        val turn = turns.getJSONObject(turnIndex)
        val messagesJson = turn.getJSONArray("messages")
        val messages = List(messagesJson.length()) { messageIndex ->
            val message = messagesJson.getJSONObject(messageIndex)
            MessageDto(
                message.getString("role"),
                message.getString("content"),
                message.optionalString("name"),
                message.optionalString("tool_calls"),
                message.optionalString("tool_call_id"),
            )
        }
        TurnDto(
            turn.getString("id"),
            turn.getLong("ts"),
            turn.getString("kind"),
            turn.getString("engine"),
            turn.getString("model"),
            turn.getString("device_id"),
            turn.getLong("seq"),
            messages,
        )
    }
}

private fun JSONObject.optionalString(name: String): String? =
    if (isNull(name)) null else getString(name)
