package dev.eternai.app.sync

import java.io.File
import java.io.FileOutputStream

/** Append-only local memory file shared with the edge tool runtime. */
internal class LocalMemoryStore(private val file: File) {

    @Synchronized
    fun read(): String = if (file.exists()) file.readText(Charsets.UTF_8) else ""

    /**
     * Adds only new lines from the desktop. Existing local facts are never
     * replaced, and a merge that exceeds the shared memory cap is rejected.
     */
    @Synchronized
    fun merge(incoming: String): String {
        val current = read()
        val seen = current.lineSequence()
            .map { it.trim() }
            .filter { it.isNotEmpty() }
            .toMutableSet()
        val additions = incoming.lineSequence()
            .map { it.trim() }
            .filter { it.isNotEmpty() && seen.add(it) }
            .toList()
        if (additions.isEmpty()) return current

        val merged = buildString {
            append(current)
            if (isNotEmpty() && last() != '\n') append('\n')
            additions.forEach {
                append(it)
                append('\n')
            }
        }
        val codePoints = merged.codePointCount(0, merged.length)
        val tokens = (codePoints + 3) / 4
        check(tokens <= MEMORY_CAP_TOKENS) {
            "merged memory would use $tokens tokens, exceeding the $MEMORY_CAP_TOKENS token cap"
        }

        val parent = file.parentFile
        check(parent == null || parent.isDirectory || parent.mkdirs()) {
            "could not create memory directory: ${parent?.absolutePath}"
        }
        val suffix = buildString {
            if (current.isNotEmpty() && !current.endsWith('\n')) append('\n')
            additions.forEach {
                append(it)
                append('\n')
            }
        }
        FileOutputStream(file, true).use {
            it.write(suffix.toByteArray(Charsets.UTF_8))
            it.fd.sync()
        }
        return merged
    }

    private companion object {
        const val MEMORY_CAP_TOKENS = 2_000
    }
}
