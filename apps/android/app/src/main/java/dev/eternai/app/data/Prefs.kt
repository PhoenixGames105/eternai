package dev.eternai.app.data

import android.content.Context
import dev.eternai.core.DeviceIdentity

const val MIN_EDGE_MAX_TOKENS = 128
const val MAX_EDGE_MAX_TOKENS = 20_000

/**
 * Compute backend for the embedded edge server.
 *
 * [deviceName] is what llama.cpp's `-dev` expects: the *registered device*
 * name, not the backend type. Passing "vulkan"/"opencl" is rejected during
 * argument parsing. Null means "no GPU device" - the server is started with
 * `-dev none`, which matters because ggml registers every packaged backend
 * and would otherwise auto-select a GPU one.
 */
enum class EdgeBackend(val id: String, val deviceName: String?, val label: String) {
    Cpu("cpu", null, "CPU"),
    Vulkan("vulkan", "Vulkan0", "Vulkan"),
    OpenCl("opencl", "GPUOpenCL", "OpenCL");

    /** True when this backend runs on the GPU and can therefore fail to start. */
    val isGpu: Boolean get() = deviceName != null

    companion object {
        fun fromId(id: String?): EdgeBackend = entries.firstOrNull { it.id == id } ?: Cpu
    }
}

/**
 * Where the phone remembers the gateway and its own credential.
 *
 * The device token is a long-lived bearer credential. M7a stores it in the app's
 * private prefs file (already sandboxed); moving to EncryptedSharedPreferences is
 * tracked for M7 verify. Nothing else about pairing is secret: the pair code is
 * single-use and short-lived.
 */
class Prefs(context: Context) {

    private val sp = context.applicationContext
        .getSharedPreferences("eternai", Context.MODE_PRIVATE)

    init {
        // Earlier diagnostic builds could persist selection of the legacy JNI
        // engine, which does not use llama.cpp's server prompt path. Migrate
        // existing installs once so this update actually exercises the server.
        val editor = sp.edit()
        if (!sp.getBoolean(KEY_EDGE_SERVER_MIGRATED, false)) {
            editor.putBoolean(KEY_EDGE_SERVER, DEFAULT_EDGE_SERVER)
            editor.putBoolean(KEY_EDGE_SERVER_MIGRATED, true)
        }
        // The GPU setting used to be a Vulkan on/off switch. Carry an old "on"
        // over as an explicit Vulkan choice, then drop the legacy keys so a
        // later toggle cannot be shadowed by stale state.
        if (!sp.getBoolean(KEY_EDGE_BACKEND_MIGRATED, false)) {
            if (sp.getBoolean("edge_use_vulkan", false)) {
                editor.putString(KEY_EDGE_BACKEND, EdgeBackend.Vulkan.id)
            }
            if (sp.getBoolean("edge_vulkan_failed", false)) {
                editor.putString(KEY_EDGE_GPU_FAILED, EdgeBackend.Vulkan.id)
            }
            editor.remove("edge_use_vulkan")
            editor.remove("edge_vulkan_failed")
            editor.putBoolean(KEY_EDGE_BACKEND_MIGRATED, true)
        }
        editor.apply()
    }

    /** Pairing target from the QR: host, TLS port and the pinned cert. */
    var host: String?
        get() = sp.getString(KEY_HOST, null)
        set(value) = sp.edit().putString(KEY_HOST, value).apply()

    var port: Long
        get() = sp.getLong(KEY_PORT, DEFAULT_TLS_PORT)
        set(value) = sp.edit().putLong(KEY_PORT, value).apply()

    var certPin: String?
        get() = sp.getString(KEY_PIN, null)
        set(value) = sp.edit().putString(KEY_PIN, value).apply()

    var device: DeviceIdentity?
        get() {
            val id = sp.getString(KEY_DEVICE_ID, null) ?: return null
            val token = sp.getString(KEY_DEVICE_TOKEN, null) ?: return null
            return DeviceIdentity(
                deviceId = id,
                token = token,
                deviceName = sp.getString(KEY_DEVICE_NAME, "").orEmpty(),
            )
        }
        set(value) {
            val e = sp.edit()
            if (value == null) {
                e.remove(KEY_DEVICE_ID).remove(KEY_DEVICE_TOKEN).remove(KEY_DEVICE_NAME)
            } else {
                e.putString(KEY_DEVICE_ID, value.deviceId)
                    .putString(KEY_DEVICE_TOKEN, value.token)
                    .putString(KEY_DEVICE_NAME, value.deviceName)
            }
            e.apply()
        }

    /** Seq for the next outbox turn; the outbox derives it from its contents. */
    var lastSeenWindowSeq: Long
        get() = sp.getLong(KEY_WINDOW_SEQ, 0L)
        set(value) = sp.edit().putLong(KEY_WINDOW_SEQ, value).apply()

    // ---------------------------------------------------------------- edge model

    /**
     * Absolute path of the GGUF the edge engine loads. It is a path rather than the
     * bytes because models are multi-hundred-MB files the user copies into the app's
     * files dir (or grants access to); copying one into private storage again would
     * waste half the phone's free space.
     */
    var edgeModelPath: String?
        get() = sp.getString(KEY_EDGE_MODEL, null)
        set(value) = sp.edit().putString(KEY_EDGE_MODEL, value).apply()

    /**
     * Context the resident model is created with. 8192 is the plan's budget: it is what
     * keeps the KV cache small enough to sit alongside a 3-4 bit quantised 3B model in
     * the memory the OS leaves a foreground app.
     */
    var edgeContextSize: Int
        get() = sp.getInt(KEY_EDGE_CTX, DEFAULT_EDGE_CTX)
        set(value) = sp.edit().putInt(KEY_EDGE_CTX, value).apply()

    var edgeMaxTokens: Int
        get() = sp.getInt(KEY_EDGE_MAX_TOKENS, DEFAULT_EDGE_MAX_TOKENS)
            .coerceIn(MIN_EDGE_MAX_TOKENS, MAX_EDGE_MAX_TOKENS)
        set(value) = sp.edit()
            .putInt(KEY_EDGE_MAX_TOKENS, value.coerceIn(MIN_EDGE_MAX_TOKENS, MAX_EDGE_MAX_TOKENS))
            .apply()

    /**
     * Sampling lives in [dev.eternai.app.engine.EdgeSampling], not here. The model
     * publisher documents paired sets of values per mode, and a half-changed set is
     * what produced incoherent replies once; there is nothing useful to tune one at
     * a time, so the app exposes the mode switch and nothing else.
     */

    /**
     * Whether a reasoning model writes inside its chain-of-thought block before
     * answering. Off by default: on a phone CPU the unprompted reasoning costs
     * minutes of silence per turn and can consume the whole reply budget. The
     * toggle exists because closing the block is a prompt convention, not a
     * guarantee, so a template that ignores it can be switched back.
     */
    var edgeThinking: Boolean
        get() = sp.getBoolean(KEY_EDGE_THINKING, DEFAULT_EDGE_THINKING)
        set(value) = sp.edit().putBoolean(KEY_EDGE_THINKING, value).apply()

    /**
     * Which on-device engine serves edge prompts.
     *
     * True (the default) runs llama.cpp's own HTTP server embedded in the app
     * process, reached over loopback - upstream's prompt path verbatim. False
     * falls back to the hand-written JNI engine in
     * [dev.eternai.app.engine.LlamaEdgeEngine], kept only as a diagnostic
     * escape hatch behind the hidden Model-sheet toggle.
     */
    var edgeUseServer: Boolean
        get() = sp.getBoolean(KEY_EDGE_SERVER, DEFAULT_EDGE_SERVER)
        set(value) = sp.edit().putBoolean(KEY_EDGE_SERVER, value).apply()

    /** Loopback port the embedded server listens on. Fixed so the client URL is stable. */
    val edgeServerPort: Int
        get() = DEFAULT_EDGE_SERVER_PORT

    /**
     * Compute backend for the embedded edge server.
     *
     * CPU is the default. On the Fold 5 neither GPU path beat the CPU in
     * measured throughput (see [EdgeBackend] and the notes in
     * [dev.eternai.app.engine.ServerEdgeEngine]), so the GPU backends are
     * opt-in experiments rather than the recommended setting.
     */
    var edgeBackend: EdgeBackend
        get() = EdgeBackend.fromId(sp.getString(KEY_EDGE_BACKEND, null))
        set(value) = sp.edit().putString(KEY_EDGE_BACKEND, value.id).apply()

    /**
     * How many layers to offload to the GPU when a GPU backend is in use. 99
     * means "all of them", which is what a small edge model wants.
     */
    var edgeGpuLayers: Int
        get() = sp.getInt(KEY_EDGE_GPU_LAYERS, DEFAULT_EDGE_GPU_LAYERS)
        set(value) = sp.edit().putInt(KEY_EDGE_GPU_LAYERS, value).apply()

    /**
     * Backend id whose last start failed, so the next launch skips straight to
     * CPU instead of paying the failure timeout again. Cleared when a start on
     * that backend succeeds, so a driver update can re-enable the GPU path.
     */
    var edgeGpuFailedBackend: String?
        get() = sp.getString(KEY_EDGE_GPU_FAILED, null)
        set(value) = sp.edit().putString(KEY_EDGE_GPU_FAILED, value).apply()

    /**
     * CPU threads for the edge server.
     *
     * Deliberately below `availableProcessors()`. The Fold 5 exposes eight
     * logical CPUs, but llama.cpp's CPU backend peaks at 3-4 threads: measured
     * on Qwen3.5-4B-Q4_K_XL, 4 threads produced 6.2-6.9 t/s while 6 produced
     * 5.1-5.7 t/s and 8 produced 4.0 t/s, because the extra workers spill onto
     * the efficiency cores and contend for the same memory bandwidth the
     * quantised GEMV kernels are already saturating.
     */
    var edgeCpuThreads: Int
        get() = sp.getInt(KEY_EDGE_CPU_THREADS, DEFAULT_EDGE_CPU_THREADS)
        set(value) = sp.edit().putInt(KEY_EDGE_CPU_THREADS, value).apply()

    // -------------------------------------------------------------------- voice

    /**
     * Whether replies are spoken aloud. Off by default: a companion that talks
     * back unprompted is unwelcome in a shared space, and the toggle is one tap
     * away in the chat bar.
     */
    var speakReplies: Boolean
        get() = sp.getBoolean(KEY_SPEAK_REPLIES, false)
        set(value) = sp.edit().putBoolean(KEY_SPEAK_REPLIES, value).apply()

    val personality: String
        get() = sp.getString(KEY_PERSONALITY, "").orEmpty()

    val personalityUpdatedAt: Long
        get() = sp.getLong(KEY_PERSONALITY_UPDATED_AT, 0L)

    fun savePersonality(value: String, updatedAt: Long) {
        require(value.codePointCount(0, value.length) <= MAX_PERSONALITY_CHARS) {
            "personality must be $MAX_PERSONALITY_CHARS characters or fewer"
        }
        require(updatedAt >= 0L) { "personality timestamp cannot be negative" }
        sp.edit()
            .putString(KEY_PERSONALITY, value)
            .putLong(KEY_PERSONALITY_UPDATED_AT, updatedAt)
            .apply()
    }

    val paired: Boolean
        get() = device != null && !host.isNullOrBlank() && !certPin.isNullOrBlank()

    fun forget() {
        device = null
        host = null
        certPin = null
        sp.edit().remove(KEY_WINDOW_SEQ).apply()
    }

    private companion object {
        const val KEY_HOST = "host"
        const val KEY_PORT = "port"
        const val KEY_PIN = "cert_pin"
        const val KEY_DEVICE_ID = "device_id"
        const val KEY_DEVICE_TOKEN = "device_token"
        const val KEY_DEVICE_NAME = "device_name"
        const val KEY_WINDOW_SEQ = "window_seq"
        const val KEY_EDGE_MODEL = "edge_model_path"
        const val KEY_EDGE_CTX = "edge_ctx"
        const val KEY_EDGE_MAX_TOKENS = "edge_max_tokens"
        const val KEY_EDGE_TEMP = "edge_temp"
        const val KEY_EDGE_TOP_K = "edge_top_k"
        const val KEY_EDGE_TOP_P = "edge_top_p"
        const val KEY_EDGE_THINKING = "edge_thinking"
        const val KEY_EDGE_SERVER = "edge_use_server"
        const val KEY_EDGE_SERVER_MIGRATED = "edge_server_migrated"
        const val KEY_EDGE_BACKEND_MIGRATED = "edge_backend_migrated"
        const val KEY_EDGE_BACKEND = "edge_backend"
        const val KEY_EDGE_GPU_LAYERS = "edge_gpu_layers"
        const val KEY_EDGE_GPU_FAILED = "edge_gpu_failed_backend"
        const val KEY_EDGE_CPU_THREADS = "edge_cpu_threads"
        const val KEY_SPEAK_REPLIES = "speak_replies"
        const val KEY_PERSONALITY = "personality"
        const val KEY_PERSONALITY_UPDATED_AT = "personality_updated_at"
        const val DEFAULT_TLS_PORT = 8443L
        const val MAX_PERSONALITY_CHARS = 2000

        // The embedded server is the default edge path; the JNI engine is opt-in.
        const val DEFAULT_EDGE_SERVER = true
        const val DEFAULT_EDGE_SERVER_PORT = 18080

        // Mirrors etern_ffi::DEFAULT_EDGE_CTX; UniFFI does not surface Rust consts.
        const val DEFAULT_EDGE_CTX = 8192

        // Above the 700 the Rust budget reserves for a reply, so the native trim is
        // the one that decides how much history fits. A reasoning model that ignores
        // the closed-block hint needs the headroom or it never reaches an answer.
        const val DEFAULT_EDGE_MAX_TOKENS = 1024
        const val DEFAULT_EDGE_THINKING = false
        const val DEFAULT_EDGE_CPU_THREADS = 4
        const val DEFAULT_EDGE_GPU_LAYERS = 99
    }
}
