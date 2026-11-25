// JNI bridge between the Android app and llama.cpp.
//
// Deliberately thin: the app hands over a chat, this applies the model's own
// chat template, prefills, then streams decoded pieces back through a callback.
// All state lives in a single Engine struct owned by a jlong handle so the Kotlin
// side can keep exactly one model resident (the phone cannot afford two).
//
// Prompt rendering and sampling both come from llama.cpp's own common/ library
// rather than being reimplemented here. The raw C entry point
// llama_chat_apply_template is not a template engine: it sniffs the model's
// template for a handful of known markers and then runs a hand-written applier,
// so a model that ships a real Jinja template (Qwen3.5 among them) silently gets
// a generic ChatML prompt missing its reasoning, vision and tool-call handling.
// The model then degenerates into repeating a single token, which no sampler
// setting can fix.

#include <jni.h>
#include <android/log.h>

#include "llama.h"
#include "chat.h"
#include "sampling.h"

#include <algorithm>
#include <atomic>
#include <mutex>
#include <string>
#include <utility>
#include <vector>
#include <cstring>

#define TAG "etern-engine"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, TAG, __VA_ARGS__)

namespace {

struct Engine {
    llama_model *   model = nullptr;
    llama_context * ctx   = nullptr;
    const llama_vocab * vocab = nullptr;

    // Compiled Jinja templates for this model, built once at load.
    common_chat_templates_ptr chat_templates;

    int32_t n_ctx      = 8192;
    int32_t max_tokens = 1024;
    bool    thinking   = false;

    // Sampling configuration in llama.cpp's own terms. The chain is rebuilt from
    // this for every turn because common_sampler_init is allowed to adjust the
    // parameters it is handed.
    common_params_sampling sampling;

    // True when the context cannot rewind a sequence to an earlier point, which
    // makes KV prefix reuse unsafe. Decided once at load time.
    bool    no_partial_rollback = false;

    // Tokens currently prefilled into the KV cache, in order. The app resends the
    // whole conversation every turn, so the previous prompt is normally a strict
    // prefix of the next one; reusing that prefix is the difference between a
    // one-second and a one-minute turn once the history has grown.
    std::vector<llama_token> prefilled;

    std::mutex    mu;
    std::atomic<bool> abort_requested{false};
};

// Tokens handed to llama_decode at once. A larger chunk prefills a long prompt in
// fewer passes; 1024 keeps the activation buffers modest on a phone while halving
// the pass count of the 512 it replaces.
constexpr int32_t PREFILL_CHUNK = 1024;

struct ChunkSink {
    jobject   obj   = nullptr;
    jmethodID mid   = nullptr;
};

bool abort_now(void * user) {
    auto * e = static_cast<Engine *>(user);
    return e->abort_requested.load(std::memory_order_relaxed);
}

// Renders the chat with the model's own Jinja template. Returns false if the
// model has no usable template or the template threw, in which case the caller
// falls back to plain text.
//
// Going through common/ rather than the raw C API matters twice over: the
// template is actually evaluated (including its enable-thinking switch, which is
// why the old string surgery that closed a reasoning block by hand is gone), and
// the result is split into template parts and user-input parts so that special
// tokens a user typed cannot be mistaken for control tokens.
bool build_prompt(Engine * e,
                  const std::vector<std::pair<std::string, std::string>> & chat,
                  common_chat_params * out) {
    if (!e->chat_templates) return false;

    common_chat_templates_inputs inputs;
    inputs.messages.reserve(chat.size());
    for (auto & m : chat) {
        common_chat_msg msg;
        msg.role    = m.first;
        msg.content = m.second;
        inputs.messages.push_back(std::move(msg));
    }
    inputs.use_jinja            = true;
    inputs.add_generation_prompt = true;
    inputs.enable_thinking      = e->thinking;
    // The template decides whether a BOS belongs at the start. Forcing one on top
    // of whatever it emitted would duplicate it.
    inputs.add_bos = false;
    inputs.add_eos = false;

    try {
        *out = common_chat_templates_apply(e->chat_templates.get(), inputs);
    } catch (const std::exception & ex) {
        LOGE("chat template failed: %s", ex.what());
        return false;
    }
    return !out->prompt.parts.empty();
}

// Last resort for GGUFs that ship without a chat template: a plain "role: text"
// dump, which is better than refusing to answer.
common_chat_prompt fallback_prompt(
        const std::vector<std::pair<std::string, std::string>> & chat) {
    common_chat_prompt prompt;
    for (auto & m : chat) {
        prompt.append(m.first + "\n", /* is_input */ false);
        prompt.append(m.second + "\n\n", /* is_input */ true);
    }
    prompt.append("assistant\n", /* is_input */ false);
    return prompt;
}

// One rendered prompt part expressed as a half-open range of the token vector.
struct PartSpan {
    int32_t begin = 0;
    int32_t end   = 0;
};

// Tokenizes a rendered prompt. Template parts are tokenized with special-token
// parsing enabled; user-input parts are not, so control-token lookalikes inside
// a message stay ordinary text.
//
// The spans of the individual parts are reported so that an oversized prompt can
// be trimmed on a part boundary: cutting a Jinja render in the middle can delete
// the marker that opened a turn and leave the model with an unterminated
// structure, which is exactly the kind of damage that produces repetition.
llama_tokens tokenize_prompt(const llama_vocab * vocab,
                             const common_chat_prompt & prompt,
                             std::vector<PartSpan> * spans) {
    llama_tokens out;
    for (const auto & part : prompt.parts) {
        if (part.text.empty()) continue;
        const llama_tokens part_tokens =
            common_tokenize(vocab, part.text, /* add_special */ false, !part.is_input);
        if (spans) {
            spans->push_back(PartSpan{ (int32_t) out.size(),
                                       (int32_t) (out.size() + part_tokens.size()) });
        }
        out.insert(out.end(), part_tokens.begin(), part_tokens.end());
    }
    return out;
}

// Drops the oldest `drop` tokens, snapping the cut forward to the start of the
// next part when that still leaves the trim effective. The final part holds the
// generation prompt, so the cut is never allowed to fall inside it.
void trim_prompt(llama_tokens & tokens, const std::vector<PartSpan> & spans, int32_t drop) {
    const int32_t n = (int32_t) tokens.size();
    if (drop <= 0 || drop >= n) return;

    // The start of the final part is as far as a cut may go. Dropping into it
    // would remove part of the generation prompt, so when the prompt is that
    // tight the trim overshoots and discards whole earlier turns instead.
    const int32_t ceiling = spans.empty() ? n : std::max<int32_t>(1, spans.back().begin);
    int32_t cut = std::min(drop, ceiling);
    for (const PartSpan & s : spans) {
        if (s.begin >= drop) {
            if (s.begin <= ceiling) cut = s.begin;
            break;
        }
    }
    if (cut <= 0 || cut >= n) return;

    LOGI("trimming %d leading prompt tokens on a part boundary (wanted %d)", cut, drop);
    tokens.erase(tokens.begin(), tokens.begin() + cut);
}

// Renders the prompt as text with the angle brackets that delimit control tokens
// replaced by square ones. The point is to make a bad render visible in logcat
// without the real marker sequences appearing anywhere.
std::string preview_prompt(const common_chat_prompt & prompt, size_t limit) {
    std::string text;
    for (const auto & part : prompt.parts) text += part.text;
    if (text.size() > limit) text.resize(limit);
    for (char & c : text) {
        if (c == '<') c = '[';
        else if (c == '>') c = ']';
    }
    return text;
}

// Same as preview_prompt, but taken from the end of the render, where the
// generation prompt lives.
std::string tail_preview(const common_chat_prompt & prompt, size_t limit) {
    std::string text;
    for (const auto & part : prompt.parts) text += part.text;
    if (text.size() > limit) text = text.substr(text.size() - limit);
    for (char & c : text) {
        if (c == '<') c = '[';
        else if (c == '>') c = ']';
    }
    return text;
}

}  // namespace

extern "C" {

JNIEXPORT jlong JNICALL
Java_dev_eternai_app_engine_LlamaNative_nativeLoad(JNIEnv * env, jclass, jstring path,
                                                   jint n_ctx, jint n_threads,
                                                   jint max_tokens, jfloat temp,
                                                   jint top_k, jfloat top_p,
                                                   jfloat min_p,
                                                   jfloat penalty_repeat,
                                                   jfloat penalty_present,
                                                   jboolean thinking) {
    const char * cpath = env->GetStringUTFChars(path, nullptr);

    auto * e = new Engine();
    e->n_ctx      = n_ctx > 0 ? n_ctx : 8192;
    e->max_tokens = max_tokens > 0 ? max_tokens : 1024;
    e->thinking   = thinking == JNI_TRUE;

    // Everything the Kotlin preset publishes, expressed in llama.cpp's terms.
    // DRY and the other optional samplers stay disabled because the preset does
    // not expose them; penalty_last_n matches what the previous chain used.
    e->sampling.temp           = temp;
    e->sampling.top_k          = top_k;
    e->sampling.top_p          = top_p;
    e->sampling.min_p          = min_p;
    e->sampling.penalty_repeat  = penalty_repeat > 0 ? penalty_repeat : 1.0f;
    e->sampling.penalty_present = penalty_present;
    e->sampling.penalty_last_n = 64;
    e->sampling.no_perf        = true;
    e->sampling.samplers.clear();
    e->sampling.samplers.push_back(COMMON_SAMPLER_TYPE_PENALTIES);
    e->sampling.samplers.push_back(COMMON_SAMPLER_TYPE_TOP_K);
    e->sampling.samplers.push_back(COMMON_SAMPLER_TYPE_TOP_P);
    if (min_p > 0) {
        e->sampling.samplers.push_back(COMMON_SAMPLER_TYPE_MIN_P);
    }
    e->sampling.samplers.push_back(COMMON_SAMPLER_TYPE_TEMPERATURE);

    llama_backend_init();

    llama_model_params mp = llama_model_default_params();
    mp.n_gpu_layers = 0;   // CPU build; a Vulkan build raises this via n_gpu_layers.
    // mmap keeps the weights out of the app's RSS budget on a 12 GB phone.
    mp.load_mode = LLAMA_LOAD_MODE_MMAP;

    e->model = llama_model_load_from_file(cpath, mp);
    env->ReleaseStringUTFChars(path, cpath);
    if (!e->model) {
        LOGE("model load failed");
        delete e;
        return 0;
    }
    e->vocab = llama_model_get_vocab(e->model);

    // Compile the model's Jinja template once. An empty override means "use
    // whatever the GGUF carries". Models without a template are not an error -
    // generation then falls back to the plain prompt below.
    try {
        e->chat_templates = common_chat_templates_init(e->model, "");
    } catch (const std::exception & ex) {
        LOGE("chat template compile failed: %s", ex.what());
        e->chat_templates.reset();
    }
    if (!e->chat_templates) {
        LOGE("no usable chat template; replies will use the plain fallback format");
    }

    llama_context_params cp = llama_context_default_params();
    cp.n_ctx           = static_cast<uint32_t>(e->n_ctx);
    cp.n_batch         = PREFILL_CHUNK;
    cp.n_ubatch        = PREFILL_CHUNK;
    cp.n_seq_max       = 1;
    cp.n_threads       = n_threads > 0 ? n_threads : -1;
    cp.n_threads_batch = n_threads > 0 ? n_threads : -1;
    cp.abort_callback  = abort_now;
    cp.abort_callback_data = e;

    e->ctx = llama_init_from_model(e->model, cp);
    if (!e->ctx) {
        LOGE("context init failed for n_ctx=%d", e->n_ctx);
        llama_model_free(e->model);
        delete e;
        return 0;
    }

    // A recurrent or hybrid cache keeps one state per sequence rather than a
    // per-position history, so it cannot be rewound to an earlier point; the
    // rollback path only works when llama.cpp was given state snapshots, which it
    // is not here. Reusing a prefix on such a model would resume from the state at
    // the end of the last turn and produce incoherent text, so those models always
    // re-prefill.
    const bool recurrent = llama_model_is_recurrent(e->model);
    const bool hybrid    = llama_model_is_hybrid(e->model);
    e->no_partial_rollback = (recurrent || hybrid) && llama_n_rs_seq(e->ctx) == 0;

    char arch[64] = {0};
    llama_model_meta_val_str(e->model, "general.architecture", arch, sizeof(arch) - 1);
    // "was_explicit" distinguishes a model that shipped its own template from one
    // that silently inherited llama.cpp's built-in ChatML default.
    const bool explicit_tmpl =
        e->chat_templates && common_chat_templates_was_explicit(e->chat_templates.get());
    LOGI("model loaded: arch=%s, n_ctx=%d (requested %d), recurrent=%d, hybrid=%d, "
         "template=%s, prefix reuse %s, temp=%.2f top_k=%d top_p=%.2f min_p=%.2f "
         "repeat=%.2f present=%.2f",
         arch[0] ? arch : "?", llama_n_ctx(e->ctx), e->n_ctx, recurrent, hybrid,
         explicit_tmpl ? "model" : "builtin chatml",
         e->no_partial_rollback ? "disabled" : "enabled",
         e->sampling.temp, e->sampling.top_k, e->sampling.top_p, e->sampling.min_p,
         e->sampling.penalty_repeat, e->sampling.penalty_present);
    return reinterpret_cast<jlong>(e);
}

JNIEXPORT void JNICALL
Java_dev_eternai_app_engine_LlamaNative_nativeFree(JNIEnv *, jclass, jlong handle) {
    auto * e = reinterpret_cast<Engine *>(handle);
    if (!e) return;
    std::lock_guard<std::mutex> lock(e->mu);
    e->chat_templates.reset();
    if (e->ctx)   llama_free(e->ctx);
    if (e->model) llama_model_free(e->model);
    llama_backend_free();
    delete e;
}

JNIEXPORT jstring JNICALL
Java_dev_eternai_app_engine_LlamaNative_nativeModelName(JNIEnv * env, jclass, jlong handle) {
    auto * e = reinterpret_cast<Engine *>(handle);
    if (!e) return env->NewStringUTF("");
    std::lock_guard<std::mutex> lock(e->mu);
    char buf[256] = {0};
    if (llama_model_meta_val_str(e->model, "general.name", buf, sizeof(buf) - 1) < 0) {
        return env->NewStringUTF("");
    }
    return env->NewStringUTF(buf);
}

JNIEXPORT void JNICALL
Java_dev_eternai_app_engine_LlamaNative_nativeAbort(JNIEnv *, jclass, jlong handle) {
    auto * e = reinterpret_cast<Engine *>(handle);
    if (e) e->abort_requested.store(true, std::memory_order_relaxed);
}

// Returns: 0 = stop, 1 = length, 2 = aborted, 3 = error.
JNIEXPORT jint JNICALL
Java_dev_eternai_app_engine_LlamaNative_nativeGenerate(
        JNIEnv * env, jclass, jlong handle, jobjectArray roles, jobjectArray contents,
        jobject sink) {
    auto * e = reinterpret_cast<Engine *>(handle);
    if (!e || !roles || !contents) return 3;

    const jsize n = env->GetArrayLength(roles);
    if (n != env->GetArrayLength(contents)) return 3;

    std::vector<std::string> role_str, content_str;
    role_str.reserve(n);
    content_str.reserve(n);
    for (jsize i = 0; i < n; i++) {
        auto r = (jstring) env->GetObjectArrayElement(roles, i);
        auto c = (jstring) env->GetObjectArrayElement(contents, i);
        const char * rs = env->GetStringUTFChars(r, nullptr);
        const char * cs = env->GetStringUTFChars(c, nullptr);
        role_str.emplace_back(rs);
        content_str.emplace_back(cs);
        env->ReleaseStringUTFChars(r, rs);
        env->ReleaseStringUTFChars(c, cs);
        env->DeleteLocalRef(r);
        env->DeleteLocalRef(c);
    }

    jclass sink_cls = env->GetObjectClass(sink);
    jmethodID sink_mid = env->GetMethodID(sink_cls, "onChunk", "(Ljava/lang/String;)V");
    if (!sink_mid) {
        LOGE("ChunkSink.onChunk missing");
        return 3;
    }

    std::lock_guard<std::mutex> lock(e->mu);
    e->abort_requested.store(false, std::memory_order_relaxed);

    std::vector<std::pair<std::string, std::string>> chat;
    chat.reserve(n);
    for (jsize i = 0; i < n; i++) chat.emplace_back(role_str[i], content_str[i]);

    common_chat_params chat_params;
    common_chat_prompt prompt;
    if (build_prompt(e, chat, &chat_params)) {
        prompt = std::move(chat_params.prompt);
    } else {
        prompt = fallback_prompt(chat);
    }
    std::vector<PartSpan> spans;
    llama_tokens tokens = tokenize_prompt(e->vocab, prompt, &spans);
    int32_t ntok = static_cast<int32_t>(tokens.size());
    LOGI("prompt rendered: %zu parts, %d tokens, thinking=%d",
         prompt.parts.size(), ntok, e->thinking ? 1 : 0);
    LOGI("prompt head: %s", preview_prompt(prompt, 400).c_str());
    LOGI("prompt tail: %s", tail_preview(prompt, 200).c_str());
    if (ntok <= 0) return 3;

    // Keep room for the reply; if the prompt alone would not fit, the oldest
    // tokens go. The shared Rust budget already trims to 8k, so this is a guard
    // against a template that expands more than estimated.
    const int32_t max_output_reserve =
        e->max_tokens < e->n_ctx - 256 ? e->max_tokens : 1;
    const int32_t room = max_output_reserve + 8;
    if (ntok > e->n_ctx - room && ntok > room) {
        trim_prompt(tokens, spans, ntok - (e->n_ctx - room));
        ntok = (int32_t) tokens.size();
    }
    if (ntok <= 0) return 3;
    const int32_t output_limit =
        std::min(e->max_tokens, std::max(1, e->n_ctx - ntok - 8));

    // Reuse the KV cache for the part of the conversation already prefilled. The
    // app resends the whole chat every turn, so the previous prompt is normally a
    // strict prefix of this one and only the new turn has to be decoded. Positions
    // continue correctly because llama_batch_get_one leaves pos null, which makes
    // llama.cpp derive it from the highest position still held by the sequence.
    //
    // When the history diverged - an edited message, or the trim above dropping
    // leading tokens - the common prefix is simply shorter, and an empty one
    // degrades to the old full re-prefill.
    auto * mem = llama_get_memory(e->ctx);
    const int32_t limit = std::min<int32_t>(static_cast<int32_t>(e->prefilled.size()), ntok);
    int32_t reuse = 0;
    if (!e->no_partial_rollback) {
        while (reuse < limit && e->prefilled[reuse] == tokens[reuse]) {
            reuse++;
        }
    }
    // The last prompt token has to be decoded for its logits to exist at all.
    if (reuse >= ntok) {
        reuse = ntok - 1;
    }
    // A false return means the cache could not be rolled back to that point, in
    // which case it still holds the whole previous prompt and continuing from
    // `reuse` would condition the model on text that is no longer in the prompt.
    // Fall back to a clean sequence and prefill everything.
    if (reuse > 0 && !llama_memory_seq_rm(mem, 0, reuse, -1)) {
        LOGI("cache cannot roll back to %d, re-prefilling %d tokens", reuse, ntok);
        reuse = 0;
        e->prefilled.clear();
    }
    if (reuse == 0) {
        llama_memory_seq_rm(mem, 0, -1, -1);
    } else {
        LOGI("reusing %d of %d prompt tokens", reuse, ntok);
    }

    // llama.cpp's own sampler chain, built from the preset values the app
    // published. common_sampler_init may adjust the parameters it is given, so it
    // receives a copy rather than the engine's stored configuration.
    common_params_sampling sparams = e->sampling;
    common_sampler * smpl = nullptr;
    try {
        smpl = common_sampler_init(e->model, sparams);
    } catch (const std::exception & ex) {
        LOGE("sampler init failed: %s", ex.what());
        return 3;
    }
    if (!smpl) {
        LOGE("sampler init returned null");
        return 3;
    }

    int result = 1;
    std::string pending;
    auto flush = [&]() {
        if (pending.empty()) return;
        jstring js = env->NewStringUTF(pending.c_str());
        env->CallVoidMethod(sink, sink_mid, js);
        env->DeleteLocalRef(js);
        pending.clear();
    };

    // Prefill in chunks so a long prompt does not exceed n_batch. A null logits
    // array means "only the last token is an output", which is what we want.
    int32_t decoded = reuse;
    bool failed = false;
    while (decoded < ntok) {
        const int32_t take = std::min<int32_t>(PREFILL_CHUNK, ntok - decoded);
        llama_batch batch = llama_batch_get_one(tokens.data() + decoded, take);
        if (llama_decode(e->ctx, batch) != 0) {
            // An abort surfaces as a failed decode; report it as one, not as an error.
            if (e->abort_requested.load(std::memory_order_relaxed)) {
                result = 2;
            }
            LOGE("prefill decode failed at %d", decoded);
            failed = true;
            break;
        }
        decoded += take;
        if (e->abort_requested.load(std::memory_order_relaxed)) {
            result = 2;
            failed = true;
            break;
        }
    }

    // Record only what is certainly in the cache. A failed or aborted prefill
    // leaves the tail in an unknown state, so the next turn starts over rather
    // than reusing a prefix that may not match its own tokens.
    if (failed) {
        e->prefilled.clear();
    } else {
        e->prefilled.assign(tokens.begin(), tokens.begin() + decoded);
    }

    if (!failed) {
        for (int32_t i = 0; i < output_limit; i++) {
            llama_token id = common_sampler_sample(smpl, e->ctx, -1);
            common_sampler_accept(smpl, id, /* is_generated */ true);

            // The first token is the one that reveals a bad prompt: a model fed the
            // wrong format usually answers with padding or a control token here.
            if (i == 0) {
                LOGI("first sampled token id=%d, ntok_prompt=%d", id, ntok);
            }

            if (llama_vocab_is_eog(e->vocab, id)) {
                result = 0;
                break;
            }

            char piece[256];
            int32_t len = llama_token_to_piece(e->vocab, id, piece, sizeof(piece), 0, true);
            if (len > 0) {
                pending.append(piece, static_cast<size_t>(len));
                // UTF-8 sequences can straddle a token boundary; only hand text
                // to Kotlin once it forms a complete sequence.
                const char * p = pending.data();
                size_t n_bytes = pending.size();
                size_t safe = 0;
                while (safe < n_bytes) {
                    unsigned char b = (unsigned char) p[safe];
                    size_t seqlen = b < 0x80 ? 1 : (b >> 5) == 0x6 ? 2
                                              : (b >> 4) == 0xE ? 3 : 4;
                    if (safe + seqlen > n_bytes) break;
                    safe += seqlen;
                }
                if (safe > 0) {
                    std::string head = pending.substr(0, safe);
                    pending.erase(0, safe);
                    jstring js = env->NewStringUTF(head.c_str());
                    env->CallVoidMethod(sink, sink_mid, js);
                    env->DeleteLocalRef(js);
                }
            }

            llama_batch one = llama_batch_get_one(&id, 1);
            if (llama_decode(e->ctx, one) != 0) {
                LOGE("decode failed at token %d", i);
                result = 3;
                break;
            }
            if (e->abort_requested.load(std::memory_order_relaxed)) {
                result = 2;
                break;
            }
            if (i == output_limit - 1) result = 1;
        }
    }

    flush();
    common_sampler_free(smpl);
    return result;
}

}  // extern "C"
