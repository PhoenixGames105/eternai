package dev.eternai.app.sync

import dev.eternai.core.MessageDto
import dev.eternai.core.TurnDto
import org.junit.Assert.assertEquals
import org.junit.Test
import java.nio.file.Files

class TranscriptCacheTest {

    @Test
    fun persistsAndRestoresTranscriptIncludingToolMessages() {
        val directory = Files.createTempDirectory("etern-transcript-cache").toFile()
        val file = directory.resolve("transcript.json")
        val turns = listOf(
            TurnDto(
                "desktop-turn",
                123,
                "chat",
                "llama",
                "large",
                "desktop",
                7,
                listOf(
                    MessageDto("user", "remember 22", null, null, null),
                    MessageDto("assistant", "", null, """[{"id":"call"}]""", null),
                    MessageDto("tool", "saved", "remember", null, "call"),
                ),
            ),
        )

        TranscriptCache(file).write(turns)

        assertEquals(turns, TranscriptCache(file).read())
        directory.deleteRecursively()
    }
}
