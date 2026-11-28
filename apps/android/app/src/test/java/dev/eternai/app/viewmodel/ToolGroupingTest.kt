package dev.eternai.app.viewmodel

import dev.eternai.app.sync.ConversationTurn
import dev.eternai.core.MessageDto
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * The chat folds a turn's tool calls into one collapsible bubble.
 *
 * These lock the two things the UI depends on: a turn always owns exactly one
 * group whose id still starts with the turn id (delete keys off that prefix), and
 * a stored turn projects the same way a live one streams.
 */
class ToolGroupingTest {

    private fun msg(role: String, content: String, name: String? = null) =
        MessageDto(role, content, name, null, null)

    /** Successes reach the model as the tool's own payload, unwrapped. */
    private fun ok(name: String, payload: String) = msg("tool", payload, name)

    private fun fail(name: String, message: String) =
        msg("tool", """{"ok":false,"retryable":false,"error":{"kind":"execution","message":"$message"}}""", name)

    @Test
    fun `a run of tool calls becomes one bubble`() {
        val turn = ConversationTurn(
            "t1",
            listOf(
                msg("user", "what is in my folder?"),
                ok("list_dir", """["a.txt","b.txt"]"""),
                ok("read_file", """{"path":"a.txt","text":"hi"}"""),
                msg("assistant", "two files"),
            ),
        )

        val bubbles = projectBubbles(listOf(turn))
        val groups = bubbles.filter { it.role == ROLE_TOOLS }

        assertEquals(1, groups.size)
        assertEquals(2, groups[0].steps.size)
        assertEquals("read a file", groups[0].steps[1].label)
        assertEquals(listOf("user", "assistant", ROLE_TOOLS), bubbles.map { it.role })
    }

    @Test
    fun `tool calls on both sides of a reply still share one bubble`() {
        // A multi-round turn interleaves tools and partial replies. Two groups would
        // collide on the same id and crash the list, so a turn owns exactly one.
        val turn = ConversationTurn(
            "t1",
            listOf(
                msg("user", "check two things"),
                ok("read_file", """{"text":"a"}"""),
                msg("assistant", "first done"),
                ok("web_search", "[]"),
                msg("assistant", "both done"),
            ),
        )

        val bubbles = projectBubbles(listOf(turn))
        val groups = bubbles.filter { it.role == ROLE_TOOLS }

        assertEquals(1, groups.size)
        assertEquals(2, groups[0].steps.size)
        assertEquals(bubbles.size, bubbles.map { it.id + it.role }.distinct().size)
    }

    @Test
    fun `the group id keeps the turn id as its prefix`() {
        val turn = ConversationTurn("t1", listOf(msg("user", "hi"), ok("recall", "null")))

        val group = projectBubbles(listOf(turn)).first { it.role == ROLE_TOOLS }

        // deleteBubble() takes the turn id from the part before the first ':'.
        assertEquals("t1", group.id.substringBefore(':'))
        assertEquals(toolGroupId("t1"), group.id)
    }

    @Test
    fun `a failed step is flagged and shown in its label`() {
        val turn = ConversationTurn(
            "t1",
            listOf(msg("user", "hi"), ok("web_search", "[]"), fail("read_file", "no such file")),
        )

        val steps = projectBubbles(listOf(turn)).first { it.role == ROLE_TOOLS }.steps

        assertFalse(steps[0].failed)
        assertTrue(steps[1].failed)
        assertEquals("read a file failed", steps[1].label)
        assertEquals("no such file", steps[1].detail)
    }

    @Test
    fun `a failure detail is the error message alone`() {
        val payload = """{"ok":false,"retryable":false,
            |"error":{"kind":"execution","message":"fetch failed: HTTP 404 Not Found"}}""".trimMargin()

        assertEquals("fetch failed: HTTP 404 Not Found", toolDetail(payload))
    }

    @Test
    fun `tool calls from separate turns do not merge`() {
        val turns = listOf(
            ConversationTurn("t1", listOf(msg("user", "one"), ok("recall", "1"))),
            ConversationTurn("t2", listOf(msg("user", "two"), ok("recall", "2"))),
        )

        val groups = projectBubbles(turns).filter { it.role == ROLE_TOOLS }

        assertEquals(2, groups.size)
        assertEquals(listOf("t1:tools", "t2:tools"), groups.map { it.id })
    }

    @Test
    fun `appending steps reuses the same bubble`() {
        val started = appendToolStep(emptyList(), "t1", step("read a file"))
        val grew = appendToolStep(started, "t1", step("searched the web"))

        assertEquals(1, grew.size)
        assertEquals(2, grew[0].steps.size)
        assertEquals(started[0].id, grew[0].id)
    }

    @Test
    fun `appending leaves the other bubbles alone`() {
        val user = ChatViewModel.Bubble("t1:0", "user", "hi")
        val assistant = ChatViewModel.Bubble("t1", "assistant", "", streaming = true)

        val after = appendToolStep(listOf(user, assistant), "t1", step("read a file"))

        assertEquals(listOf("t1:0", "t1", "t1:tools"), after.map { it.id })
        assertEquals(listOf("user", "assistant", ROLE_TOOLS), after.map { it.role })
    }

    @Test
    fun `payloads are flattened to one scannable line`() {
        assertEquals(
            "path: PLAN.md",
            toolDetail("""{"path":"PLAN.md","content":"a whole file of text"}"""),
        )
        assertEquals(
            "path: . · entries: a.txt, b.txt",
            toolDetail("""{"path":".","entries":["a.txt","b.txt"]}"""),
        )
        assertEquals(
            "url: https://x.dev · chars: 5372",
            toolDetail("""{"status":"ok","url":"https://x.dev","chars":5372,"text":"the page"}"""),
        )
        assertEquals(
            "url: https://x.dev · chars: 12000 · truncated: true",
            toolDetail("""{"status":"ok","url":"https://x.dev","chars":12000,"truncated":true,"text":"the page"}"""),
        )
        assertEquals("plain", toolDetail("plain"))
    }

    @Test
    fun `a large result is counted rather than dumped`() {
        val entries = (1..40).joinToString(",") { "\"file$it.txt\"" }

        assertEquals("path: . · entries: 40", toolDetail("""{"path":".","entries":[$entries]}"""))
    }

    @Test
    fun `a long payload is truncated`() {
        val detail = toolDetail("""{"path":"${"x".repeat(400)}"}""")

        assertEquals(241, detail.length)
        assertTrue(detail.endsWith("…"))
    }

    private fun step(label: String) = ChatViewModel.ToolStep("read_file", label, "", false)
}
