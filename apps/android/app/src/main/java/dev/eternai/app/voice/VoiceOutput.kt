package dev.eternai.app.voice

import android.content.Context
import android.speech.tts.TextToSpeech
import android.util.Log

/**
 * Spoken replies (PLAN §9, M7c).
 *
 * The system engine is used rather than a second on-device model: the phone
 * already ships one, it needs no RAM budget from the edge model, and it works
 * while the GGUF is loading. The engine binds asynchronously, so a reply
 * produced before `onInit` is kept and spoken as soon as it is ready — losing
 * the first answer of a session to a race would be a poor way to greet a user.
 */
class VoiceOutput(context: Context) : TextToSpeech.OnInitListener {

    private val appContext = context.applicationContext
    private var tts: TextToSpeech? = null
    private var ready = false
    private var pending: String? = null

    /** Speaks [text], replacing whatever is being spoken. */
    fun speak(text: String) {
        val body = text.trim().take(MAX_SPEAK_CHARS)
        if (body.isEmpty()) return
        val engine = tts ?: TextToSpeech(appContext, this).also { tts = it }
        if (!ready) {
            pending = body
            return
        }
        engine.speak(body, TextToSpeech.QUEUE_FLUSH, null, "etern-reply")
    }

    fun stop() {
        pending = null
        tts?.stop()
    }

    fun shutdown() {
        pending = null
        ready = false
        tts?.shutdown()
        tts = null
    }

    override fun onInit(status: Int) {
        if (status != TextToSpeech.SUCCESS) {
            Log.w(TAG, "no text-to-speech engine available (status $status)")
            pending = null
            return
        }
        ready = true
        pending?.let { text ->
            pending = null
            tts?.speak(text, TextToSpeech.QUEUE_FLUSH, null, "etern-reply")
        }
    }

    private companion object {
        const val TAG = "EternVoiceOutput"

        /** Engines cut off well before this; a long answer is better truncated than lost. */
        const val MAX_SPEAK_CHARS = 4000
    }
}
