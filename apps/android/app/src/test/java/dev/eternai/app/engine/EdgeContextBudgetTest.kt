package dev.eternai.app.engine

import dev.eternai.core.MessageDto
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class EdgeContextBudgetTest {

    private fun message(role: String, content: String) =
        MessageDto(role, content, null, null, null)

    @Test
    fun trimsWholeOldTurnsBeforeReducingReplyCap() {
        val oldUser = message("user", "o".repeat(400))
        val oldAssistant = message("assistant", "a".repeat(400))
        val currentUser = message("user", "c".repeat(100))
        val messages = listOf(
            message("system", "s".repeat(10)),
            oldUser,
            oldAssistant,
            currentUser,
        )

        val fit = EdgeContextBudget.fit(
            messages = messages,
            contextSize = 1_000,
            requestedOutputTokens = 400,
        ) { candidate -> candidate.sumOf { it.content.length } }

        assertEquals(1, fit.removedTurns)
        assertEquals(listOf("system", "user"), fit.messages.map { it.role })
        assertEquals(currentUser, fit.messages.last())
        assertEquals(400, fit.maxOutputTokens)
        assertTrue(
            fit.promptTokens + fit.maxOutputTokens + EdgeContextBudget.SAFETY_TOKENS <= 1_000,
        )
    }

    @Test
    fun oversizedRequestedReplyIsClampedToContext() {
        val messages = listOf(message("user", "q".repeat(700)))

        val fit = EdgeContextBudget.fit(
            messages = messages,
            contextSize = 1_000,
            requestedOutputTokens = 20_000,
        ) { candidate -> candidate.sumOf { it.content.length } }

        assertEquals(268, fit.maxOutputTokens)
        assertTrue(
            fit.promptTokens + fit.maxOutputTokens + EdgeContextBudget.SAFETY_TOKENS <= 1_000,
        )
        assertEquals(messages, fit.messages)
    }

    @Test
    fun oversizedReplyCeilingDoesNotEvictARecentContextFact() {
        val remembered = message("user", "remember 22 for this conversation only")
        val acknowledgment = message("assistant", "I will keep 22 in mind.")
        val question = message("user", "what was the number?")
        val messages = listOf(remembered, acknowledgment, question)

        val fit = EdgeContextBudget.fit(
            messages = messages,
            contextSize = 8_192,
            requestedOutputTokens = 20_000,
        ) { candidate -> candidate.sumOf { it.content.length } }

        assertEquals(messages, fit.messages)
        assertTrue(fit.maxOutputTokens < 20_000)
        assertTrue(fit.promptTokens + fit.maxOutputTokens + EdgeContextBudget.SAFETY_TOKENS <= 8_192)
    }

    @Test
    fun rejectsCurrentPromptThatCannotLeaveRoomForGeneration() {
        val messages = listOf(message("user", "q".repeat(1_000)))

        val error = runCatching {
            EdgeContextBudget.fit(
                messages = messages,
                contextSize = 1_000,
                requestedOutputTokens = 512,
            ) { candidate -> candidate.sumOf { it.content.length } }
        }.exceptionOrNull()

        assertEquals(
            "current prompt exceeds the edge context window; shorten it and try again",
            error?.message,
        )
    }
}
