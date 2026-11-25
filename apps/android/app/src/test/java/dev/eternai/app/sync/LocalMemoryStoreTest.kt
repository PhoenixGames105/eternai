package dev.eternai.app.sync

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class LocalMemoryStoreTest {

    @get:Rule
    val temporaryFolder = TemporaryFolder()

    @Test
    fun mergesDesktopAndPhoneFactsWithoutDuplicates() {
        val file = File(temporaryFolder.newFolder("memory"), "MEMORY.md")
        val memory = LocalMemoryStore(file)
        memory.merge("- phone fact\n- shared fact\n")

        val merged = memory.merge("- desktop fact\n- shared fact\n")

        assertEquals("- phone fact\n- shared fact\n- desktop fact\n", merged)
        assertEquals(merged, memory.merge("- phone fact\n- desktop fact\n"))
        assertEquals(merged, file.readText())
    }

    @Test
    fun refusesAnOverCapMergeWithoutChangingTheFile() {
        val file = File(temporaryFolder.newFolder("memory"), "MEMORY.md")
        val memory = LocalMemoryStore(file)
        memory.merge("- existing fact\n")
        val before = file.readText()

        val error = runCatching { memory.merge("- ${"x".repeat(9000)}\n") }.exceptionOrNull()

        assertTrue(error?.message.orEmpty().contains("exceeding the 2000 token cap"))
        assertEquals(before, file.readText())
    }
}
