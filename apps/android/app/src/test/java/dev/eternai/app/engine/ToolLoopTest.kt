package dev.eternai.app.engine

import dev.eternai.app.tools.ToolCall
import dev.eternai.app.tools.ToolResult
import dev.eternai.app.tools.ToolRunner
import dev.eternai.core.MessageDto
import kotlinx.coroutines.runBlocking
import org.json.JSONArray
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Behaviour of the tool loop, with the engine and the runtime replaced by
 * scripts. What matters here is the transcript the loop builds: a model that
 * cannot see its own tool request next to its result will keep re-asking, and
 * a failure that is swallowed will never be corrected.
 */
class ToolLoopTest {

    // -- fakes ---------------------------------------------------------------

    /** Replies from a script, one per round, and records what it was asked. */
    private class FakeEngine(private val replies: List<Reply>) : Engine {
        override val name = "fake"
        override val model = "fake-1b"
        val prompts = mutableListOf<List<MessageDto>>()

        override suspend fun generate(
            messages: List<MessageDto>,
            tools: JSONArray?,
            onChunk: (Chunk) -> Unit,
        ): Reply {
            prompts += messages
            val reply = replies[minOf(prompts.size - 1, replies.lastIndex)]
            reply.text.takeIf { it.isNotEmpty() }?.let { onChunk(Chunk(it)) }
            return reply
        }
    }

    /** Returns a scripted result batch per `execute` call and records the calls. */
    private class FakeRunner(private val batches: List<List<ToolResult>>) : ToolRunner {
        override val label = "fake"
        override var turnId: String = ""
        val batchesSeen = mutableListOf<List<ToolCall>>()
        var definitions: JSONArray? = JSONArray()

        override suspend fun definitions(): JSONArray? = definitions

        override suspend fun execute(calls: List<ToolCall>): List<ToolResult> {
            batchesSeen += calls
            return batches[minOf(batchesSeen.size - 1, batches.lastIndex)]
        }
    }

    private fun reply(text: String, vararg calls: ToolCall) =
        Reply(text = text, finish = Finish.Stop, engine = "fake", model = "fake-1b", toolCalls = calls.toList())

    private fun call(id: String = "call-1", name: String = "remember") =
        ToolCall(id = id, name = name, arguments = """{"text":"buy milk"}""")

    private fun success(id: String = "call-1", name: String = "remember") =
        ToolResult(id = id, name = name, ok = true, retryable = false, payload = """{"saved":true}""")

    private fun failure(id: String = "call-1", name: String = "remember", retryable: Boolean) =
        ToolResult(
            id = id,
            name = name,
            ok = false,
            retryable = retryable,
            payload = """{"code":"network","message":"gateway unreachable"}""",
        )

    private fun history() = listOf(MessageDto(role = "user", content = "remind me to buy milk", name = null, toolCalls = null, toolCallId = null))

    // -- tests ---------------------------------------------------------------

    @Test
    fun plainAnswerRunsOneRoundWithNoSteps() = runBlocking {
        val engine = FakeEngine(listOf(reply("noted")))
        val runner = FakeRunner(emptyList())
        val streamed = StringBuilder()

        val outcome = ToolLoop(engine, runner, "turn-1").run(history(), onChunk = { streamed.append(it) })

        assertEquals("noted", outcome.text)
        assertEquals(1, outcome.rounds)
        assertEquals(0, outcome.steps.size)
        assertFalse(outcome.aborted)
        assertEquals("noted", streamed.toString())
        assertEquals(0, runner.batchesSeen.size)
        assertEquals("fake-1b", outcome.reply?.model)
    }

    @Test
    fun toolRequestPrecedesItsResultInTranscript() = runBlocking {
        val engine = FakeEngine(listOf(reply("let me save that", call()), reply("saved it")))
        val runner = FakeRunner(listOf(listOf(success())))
        val toolEvents = mutableListOf<ToolResult>()

        val outcome = ToolLoop(engine, runner, "turn-2").run(
            history(),
            onTool = { toolEvents += it },
            onChunk = {},
        )

        // assistant tool_calls, then the tool's answer, in that order.
        assertEquals(2, outcome.steps.size)
        assertEquals(listOf(success()), toolEvents)
        val request = outcome.steps[0]
        assertEquals("assistant", request.role)
        assertEquals("let me save that", request.content)
        val requested = JSONArray(request.toolCalls)
        assertEquals(1, requested.length())
        assertEquals("call-1", requested.getJSONObject(0).getString("id"))
        assertEquals("remember", requested.getJSONObject(0).getJSONObject("function").getString("name"))
        assertEquals("""{"text":"buy milk"}""", requested.getJSONObject(0).getJSONObject("function").getString("arguments"))

        val result = outcome.steps[1]
        assertEquals("tool", result.role)
        assertEquals("call-1", result.toolCallId)
        assertEquals("remember", result.name)
        assertEquals("""{"saved":true}""", result.content)

        // The second generation sees both, so the model knows what it did.
        assertEquals(2, engine.prompts.size)
        val second = engine.prompts[1]
        assertEquals(listOf("user", "assistant", "tool"), second.map { it.role })
        assertEquals("call-1", second.last().toolCallId)
        assertNull(second.last().toolCalls)
    }

    @Test
    fun retryableFailureIsRetriedOnceAndSuccessFedBack() = runBlocking {
        val engine = FakeEngine(listOf(reply("", call()), reply("done")))
        val runner = FakeRunner(listOf(listOf(failure(retryable = true)), listOf(success())))

        val outcome = ToolLoop(engine, runner, "turn-3").run(history(), onChunk = { })

        assertEquals(2, runner.batchesSeen.size)
        assertEquals(listOf("call-1"), runner.batchesSeen[1].map { it.id })
        assertEquals("done", outcome.text)
        // Only the successful attempt reaches the transcript.
        assertEquals("""{"saved":true}""", outcome.steps[1].content)
        assertEquals(2, outcome.rounds)
    }

    @Test
    fun secondFailureIsReportedToModelNotRetriedAgain() = runBlocking {
        val engine = FakeEngine(listOf(reply("", call()), reply("could not save")))
        val runner = FakeRunner(listOf(listOf(failure(retryable = true))))

        val outcome = ToolLoop(engine, runner, "turn-4").run(history(), onChunk = { })

        assertEquals(2, runner.batchesSeen.size)
        val fed = outcome.steps[1].content
        assertTrue(fed, fed.contains("\"ok\":false"))
        assertTrue(fed, fed.contains("gateway unreachable"))
        assertTrue(fed, fed.contains("\"retryable\":true"))
        assertEquals("could not save", outcome.text)
    }

    @Test
    fun nonRetryableFailureIsNotRetried() = runBlocking {
        val engine = FakeEngine(listOf(reply("", call()), reply("sorry")))
        val runner = FakeRunner(listOf(listOf(failure(retryable = false))))

        ToolLoop(engine, runner, "turn-5").run(history(), onChunk = { })

        assertEquals(1, runner.batchesSeen.size)
    }

    @Test
    fun budgetStopsALoopThatNeverSettles() = runBlocking {
        val engine = FakeEngine(List(ToolLoop.MAX_ROUNDS + 2) { reply("", call(id = "call-$it")) })
        val runner = FakeRunner(List(ToolLoop.MAX_ROUNDS + 2) { listOf(success(id = "call-$it")) })

        val outcome = ToolLoop(engine, runner, "turn-6").run(history(), onChunk = { })

        assertTrue(outcome.aborted)
        assertEquals(ToolLoop.MAX_ROUNDS, outcome.rounds)
        assertNull(outcome.reply)
        assertEquals(ToolLoop.MAX_ROUNDS, engine.prompts.size)
        // Every round still leaves its mark so the journal shows the attempt.
        assertEquals(ToolLoop.MAX_ROUNDS * 2, outcome.steps.size)
        assertTrue(outcome.text.contains("${ToolLoop.MAX_ROUNDS} tool rounds"))
    }

    @Test
    fun toolResultsMatchTheirCallsByIdAcrossABatch() = runBlocking {
        val calls = listOf(call(id = "a", name = "remember"), call(id = "b", name = "read_file"))
        val engine = FakeEngine(listOf(reply("", *calls.toTypedArray()), reply("both done")))
        // The runner answers out of order; the loop must not re-pair them.
        val runner = FakeRunner(listOf(listOf(success(id = "b", name = "read_file"), success(id = "a", name = "remember"))))

        val outcome = ToolLoop(engine, runner, "turn-7").run(history(), onChunk = { })

        assertEquals(listOf("b", "a"), outcome.steps.drop(1).map { it.toolCallId })
        assertEquals(listOf("read_file", "remember"), outcome.steps.drop(1).map { it.name })
    }

    @Test
    fun retryMergesBackIntoOriginalOrder() = runBlocking {
        val calls = listOf(call(id = "a", name = "remember"), call(id = "b", name = "read_file"))
        val engine = FakeEngine(listOf(reply("", *calls.toTypedArray()), reply("ok")))
        val first = listOf(failure(id = "a", retryable = true), success(id = "b", name = "read_file"))
        val second = listOf(success(id = "a", name = "remember"))
        val runner = FakeRunner(listOf(first, second))

        val outcome = ToolLoop(engine, runner, "turn-8").run(history(), onChunk = { })

        assertEquals(2, runner.batchesSeen.size)
        assertEquals(listOf("a", "b"), outcome.steps.drop(1).map { it.toolCallId })
        assertEquals("""{"saved":true}""", outcome.steps[1].content)
        assertEquals("""{"saved":true}""", outcome.steps[2].content)
    }

    @Test
    fun toolCallJsonRoundTripsThroughTheEdgePrompt() = runBlocking {
        val calls = listOf(call(id = "a"), call(id = "b", name = "web_search"))
        val json = calls.toJsonArray()
        assertEquals(2, json.length())
        assertEquals("function", json.getJSONObject(1).getString("type"))
        assertEquals("web_search", json.getJSONObject(1).getJSONObject("function").getString("name"))
    }
}
