package dev.eternai.app.engine

import dev.eternai.core.MessageDto
import org.json.JSONArray
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The edge model only ever sees `(role, content)` pairs, so everything the
 * harness knows about tools has to survive being flattened into text. These
 * checks pin that encoding down: a replayed conversation must read to the model
 * the way the protocol asks it to write.
 */
class EdgePromptTest {

    private val tools = JSONArray(
        """[{"type":"function","function":{"name":"read_file",
           "description":"Read a text file",
           "parameters":{"properties":{"path":{"type":"string"}}}}}]""",
    )

    private fun msg(
        role: String,
        content: String,
        name: String? = null,
        toolCalls: String? = null,
        toolCallId: String? = null,
    ) = MessageDto(role, content, name, toolCalls, toolCallId)

    @Test
    fun instructionsAreAppendedToAnExistingSystemMessage() {
        val rendered = EdgePrompt.render(listOf(msg("system", "You are helpful."), msg("user", "hi")), tools)
        assertEquals(2, rendered.size)
        assertEquals("system", rendered[0].first)
        assertTrue(rendered[0].second, rendered[0].second.startsWith("You are helpful."))
        assertTrue(rendered[0].second, rendered[0].second.contains("read_file"))
        assertEquals("user", rendered[1].first)
    }

    @Test
    fun personalityAndToolInstructionsRemainInTheSystemPrompt() {
        val personality = PersonalityPrompt.message("warm and concise")!!
        val rendered = EdgePrompt.render(listOf(personality, msg("user", "hi")), tools)

        assertEquals("system", rendered[0].first)
        assertTrue(rendered[0].second.contains("style only"))
        assertTrue(rendered[0].second.contains("read_file"))
        assertTrue(rendered[0].second.contains("ETOOL"))
        assertEquals("user", rendered[1].first)
    }

    @Test
    fun aConversationWithoutASystemMessageGetsOne() {
        val rendered = EdgePrompt.render(listOf(msg("user", "hi")), tools)
        assertEquals("system", rendered[0].first)
        assertTrue(rendered[0].second.contains("ETOOL"))
        assertEquals("user", rendered[1].first)
    }

    @Test
    fun noToolsMeansNoInstructions() {
        val rendered = EdgePrompt.render(listOf(msg("user", "hi")), null)
        assertEquals(listOf("user"), rendered.map { it.first })
        assertEquals("hi", rendered[0].second)
    }

    @Test
    fun anAssistantToolRequestBecomesAProtocolLine() {
        val calls = """[{"id":"a","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a.txt\"}"}}]"""
        val rendered = EdgePrompt.render(listOf(msg("assistant", "Checking.", toolCalls = calls)), null)
        val text = rendered.single().second
        assertTrue(text, text.startsWith("Checking."))
        assertTrue(text, text.contains("ETOOL"))
        assertTrue(text, text.contains("\"tool\":\"read_file\""))
    }

    @Test
    fun aToolResultIsAttributedToTheUser() {
        // Most chat templates reject a `tool` role, and a message the template
        // refuses to render is a message the model never sees.
        val rendered = EdgePrompt.render(
            listOf(msg("tool", "the file's contents", name = "read_file", toolCallId = "a")),
            null,
        )
        assertEquals("user", rendered.single().first)
        assertEquals("[tool read_file result] the file's contents", rendered.single().second)
    }

    @Test
    fun aToolResultWithoutANameFallsBackToTheCallId() {
        val rendered = EdgePrompt.render(listOf(msg("tool", "x", toolCallId = "call_9")), null)
        assertEquals("[tool call_9 result] x", rendered.single().second)
    }
}
