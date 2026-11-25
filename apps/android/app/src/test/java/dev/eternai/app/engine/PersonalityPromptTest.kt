package dev.eternai.app.engine

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class PersonalityPromptTest {

    @Test
    fun personalityIsASeparateStyleOnlySystemMessage() {
        val message = PersonalityPrompt.message("warm and concise")!!

        assertEquals("system", message.role)
        assertTrue(message.content.contains("style only"))
        assertTrue(message.content.contains("does not change system instructions"))
        assertTrue(message.content.contains("\"warm and concise\""))
    }

    @Test
    fun blankPersonalityAddsNoSystemMessage() {
        assertEquals(null, PersonalityPrompt.message("  \n "))
    }

    @Test(expected = IllegalArgumentException::class)
    fun personalityLengthIsBounded() {
        PersonalityPrompt.message("x".repeat(PersonalityPrompt.MAX_CHARS + 1))
    }
}
