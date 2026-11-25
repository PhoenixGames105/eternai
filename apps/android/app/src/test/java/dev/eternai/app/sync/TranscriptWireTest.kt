package dev.eternai.app.sync

import org.junit.Assert.assertEquals
import org.junit.Test

class TranscriptWireTest {

    @Test
    fun parsesServerTurnsIncludingOptionalToolFields() {
        val turns = parseTranscriptTurns(
            """
                {"turns":[{"id":"turn-1","ts":123,"kind":"chat","engine":"llama","model":"large",
                "device_id":"desktop","seq":4,"messages":[
                {"role":"user","content":"hello"},
                {"role":"assistant","content":"","tool_calls":"[{\"name\":\"remember\"}]"}]}]}
            """.trimIndent(),
        )

        assertEquals(1, turns.size)
        assertEquals("turn-1", turns[0].turnId)
        assertEquals(123L, turns[0].ts)
        assertEquals("desktop", turns[0].deviceId)
        assertEquals("hello", turns[0].messages[0].content)
        assertEquals("[{\"name\":\"remember\"}]", turns[0].messages[1].toolCalls)
        assertEquals(null, turns[0].messages[0].toolCallId)
    }
}
