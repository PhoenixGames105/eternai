// JNI bridge that runs llama.cpp's own HTTP server inside the app process.
//
// The whole point of this file is to stop re-implementing the inference path.
// The hand-written engine in etern_engine.cpp kept producing incoherent output
// on-device, so instead we embed upstream's server verbatim: it already knows
// how to apply the model's chat template, drive the sampler, and speak the
// OpenAI wire format. The Kotlin layer becomes a thin HTTP client pointed at
// loopback, exactly like it is when talking to the desktop gateway.
//
// Lifecycle:
//   nativeStartServer(args)  -> mark the run live, then spawn a thread running
//                               llama_server(argc, argv)
//   nativeServerRunning()    -> a run is live (loading or serving)
//   nativeServerReturned()   -> no run is in progress; combined with the above
//                               this separates "still loading" from "died"
//   nativeStopServer()       -> retry llama_server_terminate() until the run ends
//
// llama_server() blocks inside start_loop() until terminate() is called, so a
// single background thread is enough; the caller never waits on it directly.
//
// Two rules keep the lifecycle honest, both learned from a false "exited during
// startup" report whose own log showed a perfectly healthy server:
//   1. The spawner, never the thread, publishes g_server_running. Setting it
//      inside the thread left a window where the first readiness poll saw a
//      server that had not been scheduled yet.
//   2. g_server_returned is published last, after llama_server() returns, so a
//      waiter that sees it knows the run is over and can report a real death.

#include <jni.h>
#include <android/log.h>

#include "common/log.h"
#include "ggml-backend.h"

#include <atomic>
#include <chrono>
#include <ctime>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

// Provided by libllama-server-impl.a (declared at file scope in server.cpp).
int  llama_server(int argc, char ** argv);
void llama_server_terminate();
bool llama_server_termination_ready();

// Clears the previous run's shutdown handler. Upstream keeps that handler after
// the run returns and it captures that run's locals by reference, so calling
// llama_server_terminate() before a new run installs its own handler would jump
// into freed stack frames.
void llama_server_reset();

#define STAG "etern-server"
#define SLOGI(...) __android_log_print(ANDROID_LOG_INFO, STAG, __VA_ARGS__)
#define SLOGW(...) __android_log_print(ANDROID_LOG_WARN, STAG, __VA_ARGS__)
#define SLOGE(...) __android_log_print(ANDROID_LOG_ERROR, STAG, __VA_ARGS__)

namespace {

std::thread        g_server_thread;
std::atomic<bool>  g_server_running{false};
std::atomic<bool>  g_server_returned{true};

// Serialises start/stop so a stop cannot interleave with a spawn and so
// g_server_thread is never reassigned while another call is inspecting it.
std::mutex         g_server_mutex;

// Runs the upstream server on its own thread. argv is marshalled here so the
// backing char buffers outlive the blocking llama_server() call.
void run_server(std::vector<std::string> args) {
    std::vector<char *> argv;
    argv.reserve(args.size());
    for (auto & arg : args) {
        argv.push_back(const_cast<char *>(arg.c_str()));
    }

    SLOGI("starting embedded llama-server with %d args", (int) argv.size());

    const int rc = llama_server((int) argv.size(), argv.data());

    SLOGI("embedded llama-server returned %d", rc);
    // g_server_returned is published last: a waiter that sees it can also see
    // the run as finished and stop poking it.
    g_server_running.store(false);
    g_server_returned.store(true);
}

} // namespace

extern "C" {

// Registers the dlopen-able ggml backends - the CPU ISA variants produced by
// GGML_BACKEND_DL + GGML_CPU_ALL_VARIANTS - from the app's nativeLibraryDir.
// Must run before llama_backend_init() (which llama_server() calls on its own
// thread): that only auto-discovers backends when none are registered yet, and
// auto-discovery can't find libraries inside an APK. Mirrors the init order in
// llama.cpp's own Android example.
JNIEXPORT jboolean JNICALL
Java_dev_eternai_app_engine_LlamaNative_nativeInit(JNIEnv * env, jclass, jstring libDir) {
    if (libDir == nullptr) {
        SLOGE("nativeInit called without a native library dir");
        return JNI_FALSE;
    }
    const char * dir = env->GetStringUTFChars(libDir, nullptr);
    if (dir == nullptr) {
        return JNI_FALSE;
    }
    const size_t before = ggml_backend_reg_count();
    ggml_backend_load_all_from_path(dir);
    const size_t after = ggml_backend_reg_count();
    env->ReleaseStringUTFChars(libDir, dir);
    SLOGI("ggml backend registrations: %zu -> %zu", before, after);
    return after > before ? JNI_TRUE : JNI_FALSE;
}

// args is the full command line INCLUDING argv[0] (a program name), mirroring
// how the standalone llama-server binary is invoked. The Kotlin layer builds
// this list from the user's model + sampling preferences.
JNIEXPORT jboolean JNICALL
Java_dev_eternai_app_engine_LlamaNative_nativeStartServer(JNIEnv * env, jclass, jobjectArray args,
                                                          jstring logPath) {
    std::vector<std::string> argv;
    // argv[0] first so a caller that forgot to include one still parses.
    argv.emplace_back("llama-server");

    if (args != nullptr) {
        const jsize n = env->GetArrayLength(args);
        for (jsize i = 0; i < n; ++i) {
            auto elem = (jstring) env->GetObjectArrayElement(args, i);
            if (elem == nullptr) {
                continue;
            }
            const char * s = env->GetStringUTFChars(elem, nullptr);
            if (s != nullptr) {
                argv.emplace_back(s);
                env->ReleaseStringUTFChars(elem, s);
            }
            env->DeleteLocalRef(elem);
        }
    }

    // Route llama/ggml logs into a file the debug screen can tail. Done before
    // the thread starts so the very first lines land in the file too.
    if (logPath != nullptr) {
        const char * path = env->GetStringUTFChars(logPath, nullptr);
        if (path != nullptr) {
            common_log_set_file(common_log_main(), path);
            common_log_set_timestamps(common_log_main(), true);
            SLOGI("server logs -> %s", path);
            // common_log_set_file() opens with "w", so every start truncates the
            // previous run. Without a banner an export of a run that died before
            // it could log anything still shows the *last successful* startup -
            // which is exactly what made an earlier report impossible to read.
            const std::time_t now = std::time(nullptr);
            std::tm tm_utc{};
            gmtime_r(&now, &tm_utc);
            char stamp[32];
            std::strftime(stamp, sizeof(stamp), "%Y-%m-%d %H:%M:%S UTC", &tm_utc);
            common_log_add(common_log_main(), GGML_LOG_LEVEL_INFO,
                           "=== embedded llama-server start %s ===\n", stamp);
            // Echo the backend state into the same file the debug viewer tails:
            // with GGML_BACKEND_DL a zero count means nativeInit never ran and
            // every tensor would fall back to a reference path.
            common_log_add(common_log_main(), GGML_LOG_LEVEL_INFO,
                           "ggml backend registrations: %zu\n", ggml_backend_reg_count());
            env->ReleaseStringUTFChars(logPath, path);
        }
    }

    {
        std::lock_guard<std::mutex> lock(g_server_mutex);
        if (g_server_running.load()) {
            SLOGE("nativeStartServer called while already running");
            return JNI_FALSE;
        }
        if (g_server_thread.joinable()) {
            // Only reachable when a run died on its own and nobody stopped it:
            // reclaim that thread, otherwise the handle could not be reassigned.
            SLOGW("reclaiming a llama-server run that already returned");
            g_server_thread.join();
            llama_server_reset();
        }
        // Publish "running" before the thread exists. Setting it inside
        // run_server() left a window where the caller's first readiness poll saw
        // a server that had not been scheduled yet and reported a startup
        // failure for a run that then loaded the model perfectly.
        g_server_returned.store(false);
        g_server_running.store(true);
        g_server_thread = std::thread(run_server, std::move(argv));
    }
    return JNI_TRUE;
}

JNIEXPORT void JNICALL
Java_dev_eternai_app_engine_LlamaNative_nativeStopServer(JNIEnv *, jclass) {
    std::lock_guard<std::mutex> lock(g_server_mutex);
    if (!g_server_running.load() && !g_server_thread.joinable()) {
        return;
    }

    // llama_server_terminate() is a no-op until the run has installed its own
    // shutdown handler, which upstream does only after the model has loaded. A
    // stop that arrives during loading therefore has to be retried, otherwise
    // the request is silently dropped and the join below blocks forever.
    //
    // The server publishes readiness only after assigning the handler. This
    // avoids concurrently reading its std::function while the server thread
    // installs it during model loading.
    // The loop ends when the handler stops the run, or when the run fails and
    // publishes g_server_returned.
    for (int attempt = 0; !g_server_returned.load(); ++attempt) {
        if (llama_server_termination_ready()) {
            llama_server_terminate();
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(100));
        if (attempt > 0 && attempt % 100 == 0) {
            SLOGW("still waiting for llama-server to stop after %d s", attempt / 10);
        }
    }

    if (g_server_thread.joinable()) {
        g_server_thread.join();
    }
    g_server_running.store(false);
    // Drop the handler the run just installed: its captured locals died with it,
    // and the next run does not install its own until the model has loaded.
    llama_server_reset();
}

JNIEXPORT jboolean JNICALL
Java_dev_eternai_app_engine_LlamaNative_nativeServerRunning(JNIEnv *, jclass) {
    return g_server_running.load() ? JNI_TRUE : JNI_FALSE;
}

// True once the server thread is not currently inside llama_server(): either it
// never started or it has finished. The Kotlin layer uses this to tell "still
// loading" apart from "died", which the running flag alone cannot express.
JNIEXPORT jboolean JNICALL
Java_dev_eternai_app_engine_LlamaNative_nativeServerReturned(JNIEnv *, jclass) {
    return g_server_returned.load() ? JNI_TRUE : JNI_FALSE;
}

} // extern "C"
