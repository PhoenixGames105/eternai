package dev.eternai.app.engine

import android.util.Log

/**
 * JNI surface of `libetern_engine.so`.
 *
 * Kept as a bare object of externals so the C++ side has exactly one symbol set to
 * match. Everything above this is [LlamaEdgeEngine].
 */
object LlamaNative {

    /** Result codes from [nativeGenerate]. */
    const val FINISH_STOP = 0
    const val FINISH_LENGTH = 1
    const val FINISH_ABORTED = 2
    const val FINISH_ERROR = 3

    @Volatile
    private var loaded = false

    /** Loads the native library; safe to call repeatedly. */
    @Synchronized
    fun ensureLoaded(): Boolean {
        if (loaded) return true
        return try {
            System.loadLibrary("etern_engine")
            loaded = true
            true
        } catch (e: UnsatisfiedLinkError) {
            Log.e(TAG, "libetern_engine.so failed to load", e)
            false
        }
    }

    external fun nativeLoad(
        path: String,
        nCtx: Int,
        nThreads: Int,
        maxTokens: Int,
        temp: Float,
        topK: Int,
        topP: Float,
        minP: Float,
        repetitionPenalty: Float,
        presencePenalty: Float,
        thinking: Boolean,
    ): Long

    external fun nativeFree(handle: Long)

    external fun nativeModelName(handle: Long): String

    external fun nativeAbort(handle: Long)

    external fun nativeGenerate(
        handle: Long,
        roles: Array<String>,
        contents: Array<String>,
        sink: ChunkSink,
    ): Int

    /** Implemented in Kotlin; called from native for each text delta. */
    fun interface ChunkSink {
        fun onChunk(text: String)
    }

    // ---- Embedded llama-server (upstream HTTP server, loopback) ----

    /**
     * Registers the ggml CPU backends shipped alongside the native engine.
     *
     * The llama.cpp Android build loads its CPU backend as separate `dlopen`-able
     * libraries inside the app's native library directory, so this must run once
     * after [ensureLoaded] and before [nativeStartServer]. Pass
     * `context.applicationInfo.nativeLibraryDir`.
     */
    external fun nativeInit(nativeLibDir: String): Boolean

    /**
     * Starts llama.cpp's own server on a background thread.
     *
     * [args] is the server command line *without* argv[0] (the native side
     * supplies one), e.g. `["-m", path, "--port", "8080", ...]`. [logPath] is a
     * file the debug screen tails. Returns false if a server is already running.
     */
    external fun nativeStartServer(args: Array<String>, logPath: String): Boolean

    /** Signals the server to stop and joins its thread. Safe if not running. */
    external fun nativeStopServer()

    /** True while the embedded server thread is alive. */
    external fun nativeServerRunning(): Boolean

    /**
     * True when no embedded server run is in progress (never started, or already
     * finished). Combined with [nativeServerRunning] this separates "still
     * loading" from "died", which neither flag can express on its own.
     */
    external fun nativeServerReturned(): Boolean

    private const val TAG = "EternLlamaNative"
}
