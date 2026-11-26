package dev.eternai.app.engine

import android.content.Context
import android.net.Uri
import android.os.Build
import android.util.Log
import dev.eternai.app.data.EdgeBackend
import dev.eternai.app.data.Prefs
import dev.eternai.app.util.optStringOrNull
import dev.eternai.core.MessageDto
import dev.eternai.app.tools.ToolCallAccumulator
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import org.json.JSONArray
import org.json.JSONObject
import java.io.File
import java.io.RandomAccessFile
import java.util.concurrent.TimeUnit
import kotlin.coroutines.coroutineContext

/**
 * The on-device model, served by llama.cpp's own HTTP server embedded in this
 * process and reached over loopback.
 *
 * This replaced the hand-written JNI engine, which kept producing incoherent
 * output on-device. Rather than re-implement prompt rendering, sampling and the
 * OpenAI wire format, [LlamaNative.nativeStartServer] runs upstream's
 * `llama_server` verbatim on a background thread and this class talks to it
 * exactly like [GatewayEngine] talks to the desktop - same request shape, same
 * SSE parsing - so the two paths cannot drift apart.
 *
 * The server owns one model for the life of the process. It starts lazily on the
 * first edge prompt (cold-starting a multi-hundred-MB model would otherwise slow
 * the far more common "at home, on the desktop" path) and restarts if the model
 * or its settings changed underneath it.
 */
class ServerEdgeEngine(
    context: Context,
    private val prefs: Prefs,
) : Engine {

    /** Needed later to stream the log into a SAF-selected document. */
    private val appContext: Context = context.applicationContext

    private val modelsDir = File(context.filesDir, "models").apply { mkdirs() }

    /**
     * Directory the loader unpacked our .so files into. The ggml CPU backends are
     * dlopen'd from here (see [LlamaNative.nativeInit]).
     */
    private val nativeLibDir: String = context.applicationInfo.nativeLibraryDir

    /**
     * The Fold 5's Adreno driver fails to create the Vulkan FP16 matvec
     * pipeline. Disable Vulkan FP16 before backend registration so ggml uses
     * its FP32 shader variants instead.
     */
    private val disableVulkanF16: Boolean =
        Build.MANUFACTURER.equals("samsung", ignoreCase = true) &&
            Build.MODEL.startsWith("SM-F946", ignoreCase = true)

    /** File the embedded server writes its logs to; the debug screen tails it. */
    val logFile: File = File(context.filesDir, "logs").apply { mkdirs() }
        .let { File(it, "llama-server.log") }

    /** Preserves the log from the last GPU start that failed, for the debug view. */
    private val gpuFailureLogFile: File
        get() = File(logFile.parentFile, "llama-server-gpu-failure.log")

    /**
     * Backends this APK can actually start.
     *
     * With `GGML_BACKEND_DL=ON` each backend is a separate `libggml-<name>.so`
     * that ggml dlopen's out of [nativeLibDir], and a build only carries the one
     * it was configured for - building with `-OpenCL` deletes the Vulkan lib and
     * vice versa. Offering a backend whose library was never packaged would just
     * produce a start failure, so the picker hides it.
     */
    val availableBackends: List<EdgeBackend>
        get() = EdgeBackend.entries.filter { backend ->
            when (backend) {
                EdgeBackend.Cpu -> true
                else -> File(nativeLibDir, "libggml-${backend.id}.so").isFile
            }
        }

    private val startMutex = Mutex()

    @Volatile
    private var modelName: String = ""

    /** Signature of the settings the running server was started with. */
    @Volatile
    private var startedSignature: String? = null

    /** True once [LlamaNative.nativeInit] has registered the ggml backends. */
    @Volatile
    private var backendsLoaded = false

    private val client: OkHttpClient by lazy {
        // Loopback only, but generation can be slow on CPU, so no read timeout.
        OkHttpClient.Builder()
            .connectTimeout(5, TimeUnit.SECONDS)
            .readTimeout(0, TimeUnit.MILLISECONDS)
            .writeTimeout(30, TimeUnit.SECONDS)
            .build()
    }

    private val json = "application/json; charset=utf-8".toMediaType()

    val modelFile: File?
        get() = prefs.edgeModelPath?.let { File(it) }?.takeIf { it.isFile }

    val isRunning: Boolean
        get() = LlamaNative.ensureLoaded() && LlamaNative.nativeServerRunning()

    /** True once the server is up *and* its model is loaded and serving. */
    suspend fun isReady(): Boolean = isRunning && healthOk()

    /**
     * Stops the embedded server; the next prompt restarts it.
     *
     * Takes [startMutex] on purpose. Without it a model/thinking/engine change
     * could stop the server while [ensureStarted] was mid-flight, which surfaced
     * as a bogus "llama-server exited during startup". Waiting means the switch
     * lands after the current start or stop finishes, never between them.
     */
    suspend fun unload() = startMutex.withLock {
        withContext(Dispatchers.IO) {
            if (LlamaNative.ensureLoaded()) {
                LlamaNative.nativeStopServer()
            }
        }
        startedSignature = null
        modelName = ""
    }

    /**
     * Returns the tail of the server's log file for the debug viewer.
     *
     * Reading is bounded so a multi-megabyte log from a long session cannot
     * stall the UI thread that renders it.
     */
    fun readLogTail(maxBytes: Long = 64L * 1024): String {
        if (!logFile.isFile && !gpuFailureLogFile.isFile) return "no log yet"
        return try {
            val halfLimit = (maxBytes / 2).coerceAtLeast(1)
            val current = readFileTail(logFile, halfLimit)
            val failedGpu = readFileTail(gpuFailureLogFile, halfLimit)
            buildString {
                if (current.isNotEmpty()) {
                    append("=== Current server run ===\n")
                    append(current)
                }
                if (failedGpu.isNotEmpty()) {
                    append("\n=== Previous GPU startup failure ===\n")
                    append(failedGpu)
                }
            }
        } catch (t: Exception) {
            "reading log failed: " + (t.message ?: t.javaClass.simpleName)
        }
    }

    private fun readFileTail(file: File, maxBytes: Long): String {
        if (!file.isFile) return ""
        val length = file.length()
        if (length <= maxBytes) return file.readText()
        return RandomAccessFile(file, "r").use { raf ->
            raf.seek(length - maxBytes)
            val buf = ByteArray(maxBytes.toInt())
            raf.readFully(buf)
            String(buf, Charsets.UTF_8).substringAfter('\n')
        }
    }

    /**
     * Copies the entire log to a user-chosen document (SAF) and returns the
     * number of bytes written, or a message describing why it could not.
     *
     * The tail viewer is fine for a quick glance, but a log that explains a bad
     * generation is usually longer than 64 KB, and the file lives in app-private
     * storage the user cannot reach with a file manager.
     */
    suspend fun exportLog(uri: Uri): Result<Long> = withContext(Dispatchers.IO) {
        runCatching {
            val source = logFile
            val failedGpu = gpuFailureLogFile
            if (!source.isFile && !failedGpu.isFile) error("no log has been written yet")
            val target = appContext.contentResolver.openOutputStream(uri, "wt")
                ?: error("could not open the chosen file for writing")
            var bytesWritten = 0L
            target.use { out ->
                if (failedGpu.isFile) {
                    val failedHeader =
                        "=== Previous GPU startup failure ===\n".toByteArray(Charsets.UTF_8)
                    out.write(failedHeader)
                    bytesWritten += failedHeader.size
                    bytesWritten += failedGpu.inputStream().use { it.copyTo(out) }
                    val currentHeader = "\n=== Current server run ===\n".toByteArray(Charsets.UTF_8)
                    out.write(currentHeader)
                    bytesWritten += currentHeader.size
                }
                if (source.isFile) {
                    bytesWritten += source.inputStream().use { it.copyTo(out) }
                }
            }
            bytesWritten
        }
    }

    override val name: String = "edge"

    override val model: String
        get() = modelName.ifEmpty { modelFile?.nameWithoutExtension.orEmpty() }

    /**
     * Starts the server if it is not already running with the current settings
     * and waits for it to report a loaded model.
     */
    private suspend fun ensureStarted(): String? = startMutex.withLock {
        val file = modelFile ?: return@withLock "no edge model installed"
        if (!LlamaNative.ensureLoaded()) return@withLock "native library unavailable"

        // The CPU backends are separate dlopen-able libs; register them before
        // the server thread calls llama_backend_init(). Idempotent.
        if (!backendsLoaded) {
            backendsLoaded = withContext(Dispatchers.IO) {
                LlamaNative.nativeInit(nativeLibDir, disableVulkanF16)
            }
            if (!backendsLoaded) {
                Log.w(TAG, "no ggml backend registered from $nativeLibDir")
            }
        }

        val signature = signature(file)
        if (LlamaNative.nativeServerRunning()) {
            if (startedSignature == signature && healthOk()) return@withLock null
            // Settings changed (or the model never loaded): restart cleanly.
            withContext(Dispatchers.IO) { LlamaNative.nativeStopServer() }
        }

        val backend = activeBackend()
        if (backend.isGpu && gpuFailureLogFile.exists() && !gpuFailureLogFile.delete()) {
            Log.w(TAG, "could not clear the previous GPU failure log")
        }
        val error = startAndWait(file, buildArgs(file, backend), signature)
        if (error == null) return@withLock null

        // GPU start failed: remember it, fall back to CPU so the phone
        // always has an edge engine.
        if (backend.isGpu) {
            Log.w(TAG, "${backend.label} start failed ($error); falling back to CPU")
            prefs.edgeGpuFailedBackend = backend.id
            withContext(Dispatchers.IO) { LlamaNative.nativeStopServer() }
            try {
                if (logFile.isFile) {
                    logFile.copyTo(gpuFailureLogFile, overwrite = true)
                }
                gpuFailureLogFile.appendText(
                    "\n=== ${backend.label} fallback reason ===\n$error\n",
                    Charsets.UTF_8,
                )
            } catch (t: Exception) {
                Log.e(TAG, "could not preserve failed ${backend.label} server log", t)
            }
            val cpuSignature = signature(file)
            val cpuError = startAndWait(file, buildArgs(file, EdgeBackend.Cpu), cpuSignature)
            if (cpuError == null) return@withLock null
            return@withLock "${backend.label} failed ($error); CPU fallback also failed: $cpuError"
        }
        return@withLock error
    }

    /**
     * Starts the server with [args] and waits for it to report a loaded model.
     * Returns null on success or an error message on failure.
     */
    private suspend fun startAndWait(file: File, args: List<String>, signature: String): String? {
        val started = withContext(Dispatchers.IO) {
            LlamaNative.nativeStartServer(args.toTypedArray(), logFile.absolutePath)
        }
        if (!started) return "failed to start llama-server"

        // Wait for /health to report the model is loaded. Loading a GGUF takes
        // seconds, so poll rather than assume. A run is only declared dead once
        // the native side says llama_server() actually returned - the running
        // flag alone cannot distinguish "thread not scheduled yet" from "died",
        // and misreading it produced a startup error for a perfectly healthy
        // server. The log tail is appended so a report explains itself.
        val deadline = System.currentTimeMillis() + STARTUP_TIMEOUT_MS
        while (System.currentTimeMillis() < deadline) {
            if (LlamaNative.nativeServerReturned()) {
                return "llama-server exited during startup: " + readLogTail(2048).trim()
            }
            if (healthOk()) {
                modelName = propsModel()
                startedSignature = signature
                if (activeBackend().isGpu) {
                    // A healthy GPU start clears any remembered failure so a
                    // driver update can re-enable the GPU path.
                    prefs.edgeGpuFailedBackend = null
                }
                Log.i(TAG, "embedded server ready: $model (${file.length() / (1024 * 1024)} MB)")
                return null
            }
            delay(500)
        }
        return "timed out loading ${file.name}: " + readLogTail(2048).trim()
    }

    override suspend fun generate(
        messages: List<MessageDto>,
        tools: JSONArray?,
        onChunk: (Chunk) -> Unit,
    ): Reply {
        val file = modelFile
            ?: return Reply("", Finish.Error, name, "", "no edge model installed")

        ensureStarted()?.let { return Reply("", Finish.Error, name, model, it) }

        val contextFit = try {
            withContext(Dispatchers.IO) {
                EdgeContextBudget.fit(
                    messages = messages,
                    contextSize = prefs.edgeContextSize,
                    requestedOutputTokens = prefs.edgeMaxTokens,
                ) { candidate ->
                    countPromptTokens(candidate, tools)
                }
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Log.e(TAG, "could not fit edge prompt to context", e)
            return Reply(
                "", Finish.Error, name, model,
                e.message ?: "could not fit prompt to the edge context window",
            )
        }
        if (contextFit.removedTurns > 0) {
            Log.i(
                TAG,
                "trimmed ${contextFit.removedTurns} old turn(s); prompt=${contextFit.promptTokens}, " +
                    "max_tokens=${contextFit.maxOutputTokens}",
            )
        }

        val body = JSONObject()
            .put("model", model.ifEmpty { "etern" })
            .put("stream", true)
            .put("messages", toWire(contextFit.messages))
        tools?.let { body.put("tools", it) }
        body.put("max_tokens", contextFit.maxOutputTokens)
        val preset = EdgeSampling.forThinking(prefs.edgeThinking)
        body.put("temperature", preset.temperature.toDouble())
        body.put("top_p", preset.topP.toDouble())
        body.put("top_k", preset.topK)
        body.put("min_p", preset.minP.toDouble())
        body.put("repeat_penalty", preset.repetitionPenalty.toDouble())
        body.put("presence_penalty", preset.presencePenalty.toDouble())

        val text = StringBuilder()
        val calls = ToolCallAccumulator()
        return try {
            withContext(Dispatchers.IO) {
                val request = Request.Builder()
                    .url("http://127.0.0.1:${prefs.edgeServerPort}/v1/chat/completions")
                    .header("accept", "text/event-stream")
                    .post(body.toString().toRequestBody(json))
                    .build()
                client.newCall(request).execute().use { resp ->
                    if (!resp.isSuccessful) {
                        return@withContext Reply(
                            "", Finish.Error, name, model, "server HTTP ${resp.code}",
                        )
                    }
                    val source = resp.body?.source()
                        ?: return@withContext Reply("", Finish.Error, name, model, "no body")
                    while (true) {
                        val line = source.readUtf8Line() ?: break
                        if (!line.startsWith("data:")) continue
                        val payload = line.substring(5).trim()
                        if (payload.isEmpty()) continue
                        if (payload == "[DONE]") break
                        val choice = JSONObject(payload).optJSONArray("choices")?.optJSONObject(0)
                            ?: continue
                        val delta = choice.optJSONObject("delta")
                        delta?.optStringOrNull("content")?.takeIf { it.isNotEmpty() }?.let {
                            text.append(it)
                            onChunk(Chunk(it))
                        }
                        delta?.let { calls.absorb(it) }
                    }
                }
            }
            coroutineContext.ensureActive()
            Reply(text.toString(), Finish.Stop, name, model, toolCalls = calls.result())
        } catch (e: CancellationException) {
            // The server keeps decoding; a fresh turn supersedes it. Interrupting
            // mid-request would need a cancel endpoint the loopback server does
            // not expose per-request, so just report the cancellation upward.
            Reply(text.toString(), Finish.Cancelled, name, model)
        } catch (e: java.io.IOException) {
            Log.w(TAG, "server stream broke: ${e.message}")
            Reply(text.toString(), Finish.Error, name, model, "loopback: ${e.message}")
        } catch (e: org.json.JSONException) {
            Log.w(TAG, "server sent a non-JSON chunk: ${e.message}")
            Reply(text.toString(), Finish.Error, name, model, "malformed stream: ${e.message}")
        }
    }

    private fun countPromptTokens(messages: List<MessageDto>, tools: JSONArray?): Int {
        val templateRequest = JSONObject()
            .put("model", model.ifEmpty { "etern" })
            .put("messages", toWire(messages))
        tools?.let { templateRequest.put("tools", it) }

        val templateResponse = postJson("/apply-template", templateRequest)
        val prompt = templateResponse.optStringOrNull("prompt")
            ?: error("llama.cpp did not return a rendered prompt")
        val tokenResponse = postJson(
            "/tokenize",
            JSONObject()
                .put("content", prompt)
                .put("add_special", false)
                .put("parse_special", true),
        )
        return tokenResponse.optJSONArray("tokens")?.length()
            ?: error("llama.cpp did not return prompt tokens")
    }

    private fun postJson(path: String, body: JSONObject): JSONObject {
        val request = Request.Builder()
            .url("http://127.0.0.1:${prefs.edgeServerPort}$path")
            .post(body.toString().toRequestBody(json))
            .build()
        return client.newCall(request).execute().use { response ->
            val payload = response.body?.string().orEmpty()
            if (!response.isSuccessful) {
                throw java.io.IOException("llama.cpp $path returned HTTP ${response.code}: $payload")
            }
            JSONObject(payload)
        }
    }

    // ------------------------------------------------------------------ health

    private suspend fun healthOk(): Boolean = withContext(Dispatchers.IO) {
        try {
            val req = Request.Builder()
                .url("http://127.0.0.1:${prefs.edgeServerPort}/health")
                .build()
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return@use false
                val status = JSONObject(resp.body?.string().orEmpty()).optString("status")
                status == "ok"
            }
        } catch (e: Exception) {
            false
        }
    }

    private suspend fun propsModel(): String = withContext(Dispatchers.IO) {
        try {
            val req = Request.Builder()
                .url("http://127.0.0.1:${prefs.edgeServerPort}/props")
                .build()
            client.newCall(req).execute().use { resp ->
                if (!resp.isSuccessful) return@use ""
                JSONObject(resp.body?.string().orEmpty()).optString("model_path")
                    .let { File(it).nameWithoutExtension }
            }
        } catch (e: Exception) {
            ""
        }
    }

    // -------------------------------------------------------------------- args

    /**
     * The server command line (without argv[0]). Only loopback is bound, so no
     * API key; the built-in web UI is disabled because this fork ships no
     * compatible UI assets and the app talks to the OpenAI endpoint directly.
     */
    private fun buildArgs(file: File, backend: EdgeBackend = activeBackend()): List<String> {
        // Even in GPU mode the CPU runs the sampler and any op the backend
        // cannot handle, so it still needs workers - just not all of them.
        val threads = prefs.edgeCpuThreads.coerceIn(1, 8)
        val args = mutableListOf(
            "-m", file.absolutePath,
            "--host", "127.0.0.1",
            "--port", prefs.edgeServerPort.toString(),
            "-c", prefs.edgeContextSize.toString(),
            "-t", threads.toString(),
            "-np", "1",
            "--no-webui",
            "--no-log-prefix",
            // The P100 fork emits a compact I32 attention mask when flash-attn
            // is active with a single sequence, but only the CUDA backend can
            // read it; the CPU backend interprets the bytes as f16 and silently
            // disables masking, producing garbage. Flash-attn defaults to AUTO
            // which resolves to enabled on CPU, so force it off.
            "-fa", "off",
        )
        if (prefs.edgeThinking) {
            args += listOf("--reasoning", "on")
        } else {
            args += listOf("--reasoning", "off")
        }
        if (backend.isGpu) {
            // llama.cpp expects the registered device name (Vulkan0 /
            // GPUOpenCL), not the backend type. The latter is rejected while
            // parsing arguments.
            args += listOf("-dev", backend.deviceName!!, "-ngl", prefs.edgeGpuLayers.toString())
        } else {
            // llama.cpp defaults GPU offload to "auto". Since nativeInit
            // registers every packaged backend (including the GPU ones), CPU
            // mode must explicitly exclude GPU devices to avoid silently
            // selecting one and crashing on unsupported device shaders.
            args += listOf("-dev", "none", "-ngl", "0")
        }
        return args
    }

    /**
     * The backend to start with: the user's choice, downgraded to CPU when that
     * GPU cannot be used.
     *
     * Two things disqualify a GPU backend: its previous start failed (so the
     * next launch skips the failure timeout), and its library is not in this
     * APK - a build carries either the Vulkan or the OpenCL backend, never both,
     * and a stored preference from an older build must not stall startup on a
     * device that was never packaged.
     */
    private fun activeBackend(): EdgeBackend {
        val wanted = prefs.edgeBackend
        if (!wanted.isGpu) return EdgeBackend.Cpu
        if (!isPackaged(wanted)) return EdgeBackend.Cpu
        return if (prefs.edgeGpuFailedBackend == wanted.id) EdgeBackend.Cpu else wanted
    }

    private fun isPackaged(backend: EdgeBackend): Boolean =
        backend == EdgeBackend.Cpu ||
            File(nativeLibDir, "libggml-${backend.id}.so").isFile

    private fun signature(file: File): String =
        listOf(
            file.absolutePath, file.length(),
            prefs.edgeContextSize, prefs.edgeThinking, prefs.edgeServerPort,
            activeBackend(), prefs.edgeGpuLayers, prefs.edgeCpuThreads,
        ).joinToString("|")

    private fun toWire(messages: List<MessageDto>): JSONArray {
        val arr = JSONArray()
        for (m in messages) {
            val o = JSONObject().put("role", m.role).put("content", m.content)
            m.name?.let { o.put("name", it) }
            m.toolCalls?.let { o.put("tool_calls", JSONArray(it)) }
            m.toolCallId?.let { o.put("tool_call_id", it) }
            arr.put(o)
        }
        return arr
    }

    private companion object {
        const val TAG = "EternServerEngine"
        const val STARTUP_TIMEOUT_MS = 120_000L
    }
}
