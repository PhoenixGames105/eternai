package dev.eternai.app.sync

import dev.eternai.core.MessageDto
import dev.eternai.core.TurnDto
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.FileOutputStream
import java.nio.file.Files
import java.nio.file.StandardCopyOption

/** Durable copy of the last server transcript, available when the phone starts offline. */
class TranscriptCache(private val file: File) {

    fun read(): List<TurnDto> {
        if (!file.exists()) return emptyList()
        return parseTranscriptTurns(file.readText())
    }

    fun write(turns: List<TurnDto>) {
        val parent = file.absoluteFile.parentFile
            ?: error("transcript cache has no parent directory")
        Files.createDirectories(parent.toPath())

        val temporary = File(parent, "${file.name}.tmp")
        val bytes = serializeTranscriptTurns(turns).toByteArray(Charsets.UTF_8)
        FileOutputStream(temporary).use { output ->
            output.write(bytes)
            output.fd.sync()
        }
        Files.move(
            temporary.toPath(),
            file.toPath(),
            StandardCopyOption.ATOMIC_MOVE,
            StandardCopyOption.REPLACE_EXISTING,
        )
    }
}

internal fun serializeTranscriptTurns(turns: List<TurnDto>): String {
    val serialized = JSONArray()
    turns.forEach { turn ->
        val messages = JSONArray()
        turn.messages.forEach { message ->
            messages.put(
                JSONObject()
                    .put("role", message.role)
                    .put("content", message.content)
                    .putOptional("name", message.name)
                    .putOptional("tool_calls", message.toolCalls)
                    .putOptional("tool_call_id", message.toolCallId),
            )
        }
        serialized.put(
            JSONObject()
                .put("id", turn.turnId)
                .put("ts", turn.ts)
                .put("kind", turn.kind)
                .put("engine", turn.engine)
                .put("model", turn.model)
                .put("device_id", turn.deviceId)
                .put("seq", turn.seq)
                .put("messages", messages),
        )
    }
    return JSONObject().put("turns", serialized).toString()
}

private fun JSONObject.putOptional(key: String, value: String?): JSONObject =
    put(key, value ?: JSONObject.NULL)
