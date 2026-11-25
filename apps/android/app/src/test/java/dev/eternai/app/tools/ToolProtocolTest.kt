package dev.eternai.app.tools

import org.json.JSONArray
import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The tool plumbing has to be right before it can be tried against a real
 * model on a real phone, and these are the parts that can be checked without
 * either: SSE reassembly, the text protocol the edge model speaks, and the
 * decoder for the gateway's result envelope.
 */
class ToolProtocolTest {

    private fun delta(vararg calls: JSONObject): JSONObject =
        JSONObject().put("tool_calls", JSONArray().apply { calls.forEach { put(it) } })

    private fun fragment(index: Int?, id: String?, name: String?, args: String?): JSONObject {
        val fn = JSONObject()
        if (name != null) fn.put("name", name)
        if (args != null) fn.put("arguments", args)
        val obj = JSONObject().put("function", fn)
        if (index != null) obj.put("index", index)
        if (id != null) obj.put("id", id)
        return obj
    }

    @Test
    fun fragmentsOfOneCallAreStitchedIntoValidJson() {
        val acc = ToolCallAccumulator()
        acc.absorb(delta(fragment(0, "call_a", "read_file", null)))
        acc.absorb(delta(fragment(0, null, "", "{\"path\":")))
        acc.absorb(delta(fragment(0, null, "", "\"notes.txt\"}")))

        val calls = acc.result()
        assertEquals(1, calls.size)
        assertEquals("call_a", calls[0].id)
        assertEquals("read_file", calls[0].name)
        assertEquals("""{"path":"notes.txt"}""", calls[0].arguments)
    }

    @Test
    fun interleavedCallsStaySeparatedByIndex() {
        val acc = ToolCallAccumulator()
        acc.absorb(delta(fragment(0, "a", "read_file", "{"), fragment(1, "b", "write_file", "{")))
        acc.absorb(delta(fragment(1, null, null, "\"p\":\"x\"}"), fragment(0, null, null, "\"p\":\"y\"}")))

        val calls = acc.result()
        assertEquals(listOf("read_file", "write_file"), calls.map { it.name })
        assertEquals(listOf("a", "b"), calls.map { it.id })
        assertEquals("""{"p":"y"}""", calls[0].arguments)
        assertEquals("""{"p":"x"}""", calls[1].arguments)
    }

    @Test
    fun aCallWithNoNameIsDroppedRatherThanExecuted() {
        val acc = ToolCallAccumulator()
        acc.absorb(delta(fragment(0, "a", null, "{}")))
        assertTrue(acc.result().isEmpty())
    }

    @Test
    fun deltasWithoutToolCallsAreIgnored() {
        val acc = ToolCallAccumulator()
        acc.absorb(JSONObject().put("content", "hello"))
        assertTrue(acc.isEmpty)
        assertTrue(acc.result().isEmpty())
    }

    @Test
    fun instructionsListNamesAndArgumentKeys() {
        val defs = JSONArray(
            """
            [{"type":"function","function":{"name":"read_file",
              "description":"Read a text file",
              "parameters":{"type":"object","properties":{"path":{"type":"string"}}}}}]
            """.trimIndent(),
        )
        val text = TextToolProtocol.instructions(defs)
        assertTrue(text, text.contains("read_file: Read a text file"))
        assertTrue(text, text.contains("args{path}"))
        assertTrue(text, text.contains("ETOOL"))
    }

    @Test
    fun aMarkedToolLineParsesOutOfSurroundingProse() {
        val reply = "Let me check.\nETOOL {\"tool\":\"remember\",\"arguments\":{\"text\":\"milk\"}}\ndone"
        val call = TextToolProtocol.parse(reply)
        assertEquals("remember", call?.name)
        assertEquals("milk", JSONObject(checkNotNull(call).arguments).getString("text"))
    }

    @Test
    fun aBareJsonObjectWithoutTheMarkerStillParses() {
        val call = TextToolProtocol.parse("""{"name":"read_file","arguments":{"path":"a.txt"}}""")
        assertEquals("read_file", call?.name)
    }

    @Test
    fun proseIsNotMistakenForAToolRequest() {
        assertNull(TextToolProtocol.parse("The capital of France is Paris."))
        assertNull(TextToolProtocol.parse("""{"unrelated": true}"""))
        assertNull(TextToolProtocol.parse("""{"tool":"","arguments":{}}"""))
    }

    @Test
    fun bracesInsideArgumentStringsDoNotEndTheObjectEarly() {
        val call =
            TextToolProtocol.parse("""ETOOL {"tool":"write_file","arguments":{"path":"a","body":"{not json}"}}""")
        assertEquals("write_file", call?.name)
        assertEquals("{not json}", JSONObject(checkNotNull(call).arguments).getString("body"))
    }

    @Test
    fun stripRemovesTheProtocolLineAndKeepsTheProse() {
        val text = "Reading it now.\nETOOL {\"tool\":\"read_file\",\"arguments\":{\"path\":\"a\"}}"
        assertEquals("Reading it now.", TextToolProtocol.strip(text))
    }

    @Test
    fun gatewayResultsKeepTheirIdsAndErrorPayloads() {
        val calls = listOf(ToolCall("a", "read_file", "{}"), ToolCall("b", "remember", "{}"))
        val envelope = JSONObject(
            """
            {"results":[
              {"id":"a","name":"read_file","ok":true,"content":"hello"},
              {"id":"b","name":"remember","ok":false,"retryable":true,
               "error":{"kind":"validation","message":"text is required"}}
            ]}
            """.trimIndent(),
        )
        val results = parseToolResults(envelope, calls)
        assertEquals(listOf("a", "b"), results.map { it.id })
        assertTrue(results[0].ok)
        assertEquals("hello", results[0].payload)
        assertFalse(results[1].ok)
        assertTrue(results[1].needsRetry())
        val fed = JSONObject(results[1].asToolMessage())
        assertFalse(fed.getBoolean("ok"))
        assertEquals("validation", fed.getJSONObject("error").getString("kind"))
    }

    @Test
    fun resultsWithoutIdsFallBackToCallOrder() {
        val calls = listOf(ToolCall("a", "read_file", "{}"))
        val results =
            parseToolResults(JSONObject("""{"results":[{"name":"read_file","ok":true,"content":1}]}"""), calls)
        assertEquals("a", results.single().id)
        assertEquals("1", results.single().payload)
    }
}
