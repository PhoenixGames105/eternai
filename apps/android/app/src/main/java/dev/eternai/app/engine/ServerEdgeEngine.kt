package dev.eternai.app.engine

import android.content.Context
import android.net.Uri
import android.util.Log
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

    /** File the embedded server writes its logs to; the debug screen tails it. */
    val logFile: File = File(context.filesDir, "logs").apply { mkdirs() }
        .let { File(it, "llama-server.log") }

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
        val file = logFile
        if (!file.isFile) return "no log yet"
        return try {
            val length = file.length()
            if (length <= maxBytes) {
                file.readText()
            } else {
                RandomAccessFile(file, "r").use { raf ->
                    raf.seek(length - maxBytes)
                    val buf = ByteArray(maxBytes.toInt())
                    raf.readFully(buf)
                    // Drop the first (likely partial) line.
                    String(buf, Charsets.UTF_8).substringAfter('\n')
                }
            }
        } catch (t: Exception) {
            "reading log failed: " + (t.message ?: t.javaClass.simpleName)
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
            if (!source.isFile) error("no log has been written yet")
            val target = appContext.contentResolver.openOutputStream(uri, "wt")
                ?: error("could not open the chosen file for writing")
            target.use { out -> source.inputStream().use { it.copyTo(out) } }
            source.length()
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
            backendsLoaded = withContext(Dispatchers.IO) { LlamaNative.nativeInit(nativeLibDir) }
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

        val args = buildArgs(file)
        val started = withContext(Dispatchers.IO) {
            LlamaNative.nativeStartServer(args.toTypedArray(), logFile.absolutePath)
        }
        if (!started) return@withLock "failed to start llama-server"

        // Wait for /health to report the model is loaded. Loading a GGUF takes
        // seconds, so poll rather than assume. A run is only declared dead once
        // the native side says llama_server() actually returned - the running
        // flag alone cannot distinguish "thread not scheduled yet" from "died",
        // and misreading it produced a startup error for a perfectly healthy
        // server. The log tail is appended so a report explains itself.
        val deadline = System.currentTimeMillis() + STARTUP_TIMEOUT_MS
        while (System.currentTimeMillis() < deadline) {
            if (LlamaNative.nativeServerReturned()) {
                return@withLock "llama-server exited during startup: " + readLogTail(2048).trim()
            }
            if (healthOk()) {
                modelName = propsModel()
                startedSignature = signature
                Log.i(TAG, "embedded server ready: $model (${file.length() / (1024 * 1024)} MB)")
                return@withLock null
            }
            delay(500)
        }
        return@withLock "timed out loading ${file.name}: " + readLogTail(2048).trim()
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
    private fun buildArgs(file: File): List<String> {
        val threads = Runtime.getRuntime().availableProcessors().coerceIn(2, 6)
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
        return args
    }

    private fun signature(file: File): String =
        listOf(
            file.absolutePath, file.length(),
            prefs.edgeContextSize, prefs.edgeThinking, prefs.edgeServerPort,
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
