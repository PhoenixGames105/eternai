package dev.eternai.app.sync

import android.util.Log
import java.io.File
import java.io.FileOutputStream

/**
 * Turn ids the user deleted while the desktop was out of reach.
 *
 * A deletion that only happens on the phone does not stick: the transcript and
 * the model window are both built from the desktop's database and are *replaced
 * wholesale* on every pull, never merged. So an offline delete has to be
 * remembered and replayed against the gateway the moment the connection is back,
 * or the message reappears on the next sync.
 *
 * The queue is a JSONL file rather than a new method on the Rust `Outbox`. The
 * outbox is a turn *insert* log keyed by seq and acked as one batch — deletions
 * have no seq, must survive a batch that is otherwise acked and cleared, and
 * have to be replayable individually because one can be refused (403) while its
 * neighbours succeed. Keeping them in their own append-only file also avoids
 * regenerating the uniffi bindings for a phone-side concern.
 *
 * Ids are deduplicated on read and written oldest first, so replay order
 * matches the order the user pressed delete.
 */
internal class PendingDeletes(private val file: File) {

    private val lock = Any()

    /** Records a deletion. Safe to call repeatedly for the same turn. */
    @Synchronized
    fun add(turnId: String) {
        if (turnId.isBlank()) return
        val existing = readIds()
        if (turnId in existing) return

        val parent = file.parentFile
        check(parent == null || parent.isDirectory || parent.mkdirs()) {
            "could not create delete queue directory: ${parent?.absolutePath}"
        }
        FileOutputStream(file, true).use {
            it.write((turnId + "\n").toByteArray(Charsets.UTF_8))
            it.fd.sync()
        }
    }

    /** Queued ids, oldest first, deduplicated. */
    @Synchronized
    fun peek(): List<String> = readIds()

    @Synchronized
    fun isEmpty(): Boolean = peek().isEmpty()

    /**
     * Replaces the queue with [remaining] after a drain.
     *
     * Written as a whole file rather than pruned line by line: a drain can drop
     * ids from anywhere in the list, and rewriting is the only way to keep the
     * file small after hundreds of deletes. The write is atomic (temp file +
     * rename) so a crash mid-drain cannot leave a half-written id that would
     * replay as garbage.
     */
    @Synchronized
    fun replaceAll(remaining: List<String>) {
        if (remaining.isEmpty()) {
            if (file.exists() && !file.delete()) {
                Log.w(TAG, "could not delete the empty delete queue")
            }
            return
        }
        val parent = file.parentFile
        if (parent != null && !parent.isDirectory && !parent.mkdirs()) {
            error("could not create delete queue directory: ${parent.absolutePath}")
        }
        val tmp = File(file.parentFile, file.name + ".tmp")
        FileOutputStream(tmp, false).use {
            remaining.forEach { id ->
                it.write((id + "\n").toByteArray(Charsets.UTF_8))
            }
            it.fd.sync()
        }
        if (!tmp.renameTo(file)) {
            // Renames across filesystems fail; fall back to a direct rewrite
            // rather than losing the queue.
            FileOutputStream(file, false).use { out ->
                remaining.forEach { out.write((it + "\n").toByteArray(Charsets.UTF_8)) }
                out.fd.sync()
            }
            tmp.delete()
        }
    }

    private fun readIds(): List<String> {
        if (!file.exists()) return emptyList()
        val seen = LinkedHashSet<String>()
        runCatching { file.readLines(Charsets.UTF_8) }
            .getOrElse {
                Log.w(TAG, "delete queue unreadable: ${it.message}")
                return emptyList()
            }
            .forEach { line ->
                val id = line.trim()
                if (id.isNotEmpty()) seen.add(id)
            }
        return seen.toList()
    }

    private companion object {
        const val TAG = "EternDeletes"
    }
}
