package dev.eternai.app.engine

import android.content.Context
import android.util.Log
import dev.eternai.app.data.Prefs
import dev.eternai.app.tools.TextToolProtocol
import dev.eternai.core.MessageDto
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import org.json.JSONArray
import java.io.File
import java.io.FileOutputStream
import java.io.InputStream

/**
 * The on-device model: llama.cpp running in-process over JNI.
 *
 * One model is kept resident for the life of the process. Loading a GGUF takes seconds
 * and hundreds of MB, so it happens lazily on the first offline prompt rather than at
 * cold start, which would slow down the far more common "at home, using the desktop"
 * path. The mutex serialises generation: two concurrent decodes would share one KV
 * sequence and corrupt each other.
 */
class LlamaEdgeEngine(
    context: Context,
    private val prefs: Prefs,
) : Engine {

    private val modelsDir = File(context.filesDir, "models").apply { mkdirs() }

    /** Where the user is expected to place GGUFs; shown in settings. */
    val modelsDirectory: String get() = modelsDir.absolutePath

    private var handle: Long = 0L
    private val loadMutex = Mutex()
    private val genMutex = Mutex()

    @Volatile
    private var modelName: String = ""

    /** GGUF the user picked in settings, or null when nothing is installed. */
    val modelFile: File?
        get() = prefs.edgeModelPath?.let { File(it) }?.takeIf { it.isFile }

    /** Lists GGUFs the user has placed in the app's model directory. */
    fun installedModels(): List<File> =
        modelsDir.listFiles { f -> f.isFile && f.name.endsWith(".gguf", ignoreCase = true) }
            ?.sortedBy { it.name }
            .orEmpty()

    /**
     * Copies a picked GGUF into the model directory, reporting bytes as they land.
     *
     * The model directory is private storage that no file manager can reach on
     * Android 11+, so this is the only practical way in for a sideloaded install.
     * The copy goes to a `.part` name and is renamed at the end: a model cancelled
     * halfway (pulled cable, no space) must not show up in the picker as a usable
     * multi-gigabyte file that fails to load.
     */
    suspend fun importModel(
        input: InputStream,
        fileName: String,
        sizeBytes: Long,
        onProgress: (Long) -> Unit,
    ): ImportResult {
        val name = File(fileName).name
        if (!name.endsWith(".gguf", ignoreCase = true)) {
            return ImportResult.Failed("$name is not a .gguf file")
        }
        modelsDir.mkdirs()
        if (sizeBytes > 0 && modelsDir.usableSpace in 1 until sizeBytes) {
            return ImportResult.Failed(
                "not enough space: ${sizeBytes / BYTES_PER_MB} MB needed, " +
                    "${modelsDir.usableSpace / BYTES_PER_MB} MB free",
            )
        }
        val target = uniqueTarget(name)
        val part = File(modelsDir, target.name + PART_SUFFIX)
        return try {
            input.use { src ->
                FileOutputStream(part).use { dst ->
                    val buffer = ByteArray(COPY_BUFFER_BYTES)
                    var copied = 0L
                    while (true) {
                        val read = src.read(buffer)
                        if (read < 0) break
                        dst.write(buffer, 0, read)
                        copied += read
                        onProgress(copied)
                    }
                    dst.flush()
                }
            }
            if (!part.renameTo(target)) {
                part.delete()
                ImportResult.Failed("could not finish writing ${target.name}")
            } else {
                ImportResult.Installed(target)
            }
        } catch (e: CancellationException) {
            part.delete()
            throw e
        } catch (e: Throwable) {
            Log.e(TAG, "importing $name failed", e)
            part.delete()
            ImportResult.Failed("import failed: ${e.message}")
        }
    }

    /** Never overwrites: `foo.gguf`, `foo (1).gguf`, ... */
    private fun uniqueTarget(name: String): File {
        val stem = name.removeSuffix(".gguf").removeSuffix(".GGUF")
        var candidate = File(modelsDir, name)
        var n = 1
        while (candidate.exists()) {
            candidate = File(modelsDir, "$stem ($n).gguf")
            n++
        }
        return candidate
    }

    override val name: String = "edge"

    override val model: String
        get() = modelName.ifEmpty { modelFile?.nameWithoutExtension.orEmpty() }

    val isReady: Boolean
        get() = handle != 0L

    /** Drops the resident model; the next prompt reloads it. */
    suspend fun unload() = loadMutex.withLock {
        if (handle != 0L) {
            LlamaNative.nativeFree(handle)
            handle = 0L
        }
        modelName = ""
    }

    private suspend fun ensureLoaded(): Boolean {
        if (handle != 0L) return true
        if (!LlamaNative.ensureLoaded()) return false
        return loadMutex.withLock {
            if (handle != 0L) return@withLock true
            val file = modelFile ?: return@withLock false
            withContext(Dispatchers.IO) {
                val thinking = prefs.edgeThinking
                val preset = EdgeSampling.forThinking(thinking)
                val h = LlamaNative.nativeLoad(
                    file.absolutePath,
                    prefs.edgeContextSize,
                    threadCount(),
                    prefs.edgeMaxTokens,
                    preset.temperature,
                    preset.topK,
                    preset.topP,
                    preset.minP,
                    preset.repetitionPenalty,
                    preset.presencePenalty,
                    thinking,
                )
                if (h == 0L) {
                    Log.e(TAG, "failed to load ${file.name}")
                    false
                } else {
                    handle = h
                    modelName = runCatching { LlamaNative.nativeModelName(h) }.getOrDefault("")
                    Log.i(TAG, "edge model ready: $model (${file.length() / (1024 * 1024)} MB)")
                    true
                }
            }
        }
    }

    override suspend fun generate(
        messages: List<MessageDto>,
        tools: JSONArray?,
        onChunk: (Chunk) -> Unit,
    ): Reply {
        val file = modelFile
            ?: return Reply("", Finish.Error, name, "", "no edge model installed")

        if (!ensureLoaded()) {
            return Reply("", Finish.Error, name, "", "failed to load ${file.name}")
        }

        return genMutex.withLock {
            val prompt = EdgePrompt.render(messages, tools)
            val roles = prompt.map { it.first }.toTypedArray()
            val contents = prompt.map { it.second }.toTypedArray()
            val text = StringBuilder()
            val h = handle
            try {
                val code = withContext(Dispatchers.IO) {
                    LlamaNative.nativeGenerate(h, roles, contents) { piece ->
                        text.append(piece)
                        onChunk(Chunk(piece))
                    }
                }
                // The template cannot carry tool schemas, so a tool request
                // arrives as text and is parsed back out here.
                val call = tools?.let { TextToolProtocol.parse(text.toString()) }
                val base = when (code) {
                    LlamaNative.FINISH_STOP -> Reply(text.toString(), Finish.Stop, name, model)
                    LlamaNative.FINISH_LENGTH ->
                        Reply(text.toString(), Finish.Stop, name, model, "stopped: context limit")
                    LlamaNative.FINISH_ABORTED ->
                        Reply(text.toString(), Finish.Cancelled, name, model)
                    else -> Reply(text.toString(), Finish.Error, name, model, "engine error")
                }
                if (call == null) {
                    base
                } else {
                    base.copy(
                        text = TextToolProtocol.strip(base.text),
                        toolCalls = listOf(call),
                    )
                }
            } catch (e: CancellationException) {
                LlamaNative.nativeAbort(h)
                Reply(text.toString(), Finish.Cancelled, name, model)
            } catch (e: Throwable) {
                Log.e(TAG, "edge generation crashed", e)
                Reply(text.toString(), Finish.Error, name, model, "edge: ${e.message}")
            }
        }
    }

    /**
     * Big-core count. A Z Fold 5 has 1x3.0 + 2x2.5 + 2x2.4 + 3x1.8 cores; using every
     * core makes the UI stutter and heats the device, so generation is capped at 6.
     */
    private fun threadCount(): Int = Runtime.getRuntime().availableProcessors().coerceIn(2, 6)

    private companion object {
        const val TAG = "EternEdgeEngine"
        const val PART_SUFFIX = ".part"
        const val COPY_BUFFER_BYTES = 128 * 1024
        const val BYTES_PER_MB = 1024L * 1024L
    }
}

/** Outcome of copying a picked GGUF into the model directory. */
sealed class ImportResult {
    data class Installed(val file: File) : ImportResult()
    data class Failed(val reason: String) : ImportResult()
}
