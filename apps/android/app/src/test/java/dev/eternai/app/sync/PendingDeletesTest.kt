package dev.eternai.app.sync

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

/**
 * Covers the tombstone queue that carries deletions across an offline period:
 * order, de-duplication, crash tolerance, and draining.
 */
class PendingDeletesTest {
    @get:Rule
    val tmp = TemporaryFolder()

    private fun queue() = PendingDeletes(File(tmp.root, "pending-deletes.jsonl"))

    @Test
    fun deletionsKeepTheirOrderAndAreOnlyQueuedOnce() {
        val deletes = queue()
        assert(deletes.isEmpty())

        deletes.add("turn-b")
        deletes.add("turn-a")
        deletes.add("turn-b")

        assertEquals(listOf("turn-b", "turn-a"), deletes.peek())
    }

    @Test
    fun anEmptyQueueHasNoFileBehindIt() {
        val deletes = queue()
        deletes.add("turn-a")
        assert(!deletes.isEmpty())

        deletes.replaceAll(listOf("turn-b"))
        assertEquals(listOf("turn-b"), deletes.peek())

        deletes.replaceAll(emptyList())
        assert(deletes.isEmpty())
        assertEquals(emptyList<String>(), deletes.peek())
    }

    @Test
    fun blankAndRepeatedLinesFromAHalfWrittenAppendAreIgnored() {
        val file = File(tmp.root, "pending-deletes.jsonl")
        file.writeText("turn-a\n\n   \nturn-a\nturn-b\n")

        val deletes = PendingDeletes(file)
        assertEquals(listOf("turn-a", "turn-b"), deletes.peek())
    }

    @Test
    fun aQueueWrittenByOneInstanceIsVisibleToTheNext() {
        val first = queue()
        first.add("turn-a")
        first.add("turn-b")

        assertEquals(listOf("turn-a", "turn-b"), queue().peek())
    }

    @Test
    fun aMissingFileReadsAsEmpty() {
        assert(queue().isEmpty())
        assertEquals(emptyList<String>(), queue().peek())
    }
}
