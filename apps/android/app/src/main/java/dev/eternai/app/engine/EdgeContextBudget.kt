package dev.eternai.app.engine

import dev.eternai.core.MessageDto
import kotlin.math.min

internal data class EdgeContextFit(
    val messages: List<MessageDto>,
    val promptTokens: Int,
    val maxOutputTokens: Int,
    val removedTurns: Int,
)

/**
 * Fits complete older conversation turns before reducing the requested reply cap.
 * The token counter must count the fully rendered prompt, including tool schemas.
 */
internal object EdgeContextBudget {
    const val SAFETY_TOKENS = 32
    private const val MIN_PROMPT_RESERVE = 256
    private const val OUTPUT_RESERVE_DIVISOR = 4

    fun fit(
        messages: List<MessageDto>,
        contextSize: Int,
        requestedOutputTokens: Int,
        countPromptTokens: (List<MessageDto>) -> Int,
    ): EdgeContextFit {
        require(contextSize > SAFETY_TOKENS + 1) { "context window is too small" }

        val promptReserve = min(MIN_PROMPT_RESERVE, contextSize - SAFETY_TOKENS - 1)
        val requestedOutput = min(
            requestedOutputTokens.coerceAtLeast(1),
            contextSize - SAFETY_TOKENS - promptReserve,
        )
        // max_tokens is a ceiling, not a reason to discard nearly all history.
        // Reserve at most a quarter of the edge context for generation; when the
        // prompt is full, the actual output cap is reduced to the space remaining.
        val outputReserve = min(requestedOutput, contextSize / OUTPUT_RESERVE_DIVISOR)
        val promptLimit = contextSize - SAFETY_TOKENS - outputReserve

        var fitted = messages
        var promptTokens = countPromptTokens(fitted)
        var removedTurns = 0
        while (promptTokens > promptLimit) {
            val withoutOldest = dropOldestTurn(fitted) ?: break
            fitted = withoutOldest
            promptTokens = countPromptTokens(fitted)
            removedTurns++
        }

        val outputTokens = min(
            requestedOutput,
            contextSize - promptTokens - SAFETY_TOKENS,
        )
        require(outputTokens > 0) {
            "current prompt exceeds the edge context window; shorten it and try again"
        }

        return EdgeContextFit(fitted, promptTokens, outputTokens, removedTurns)
    }

    private fun dropOldestTurn(messages: List<MessageDto>): List<MessageDto>? {
        val firstUser = messages.indexOfFirst { it.role == "user" }
        if (firstUser < 0) return null

        val nextUser = messages.indexOfFirstFrom(firstUser + 1) { it.role == "user" }
        if (nextUser < 0) return null

        return messages.take(firstUser) + messages.drop(nextUser)
    }

    private inline fun <T> List<T>.indexOfFirstFrom(startIndex: Int, predicate: (T) -> Boolean): Int {
        for (index in startIndex until size) {
            if (predicate(this[index])) return index
        }
        return -1
    }
}
