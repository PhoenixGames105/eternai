package dev.eternai.app.voice

import android.content.Context
import android.content.Intent
import android.os.Bundle
import android.speech.RecognitionListener
import android.speech.RecognizerIntent
import android.speech.SpeechRecognizer
import android.util.Log
import java.util.Locale

/**
 * On-device speech recognition (PLAN §9, M7c).
 *
 * Uses the platform recogniser with `EXTRA_PREFER_OFFLINE`, which selects the
 * manufacturer's local model (Samsung ships one on the Fold). That keeps the
 * transcript off the network and, more importantly for a companion that already
 * runs a model on the same phone, costs no memory or CPU that the edge engine
 * needs.
 *
 * [SpeechRecognizer] is single-use per session and must be driven from the main
 * thread, so every entry point here is main-thread only; the view model hops
 * over with `Dispatchers.Main`.
 */
class VoiceInput(context: Context) {

    private val appContext = context.applicationContext
    private var recognizer: SpeechRecognizer? = null
    private var listening = false

    /** False when the device has no recogniser at all; the mic is then hidden. */
    val available: Boolean
        get() = SpeechRecognizer.isRecognitionAvailable(appContext)

    val isListening: Boolean get() = listening

    /**
     * Starts a session. [onPartial] may fire several times with the interim
     * transcript; [onResult] fires once with the final one. Every error path
     * ends the session, so the caller only has to re-enable its mic button.
     */
    fun start(onPartial: (String) -> Unit, onResult: (String) -> Unit, onError: (String) -> Unit) {
        cancel()
        val engine = recognizer
            ?: SpeechRecognizer.createSpeechRecognizer(appContext).also { recognizer = it }
        engine.setRecognitionListener(
            object : RecognitionListener {
                override fun onReadyForSpeech(params: Bundle?) = Unit
                override fun onBeginningOfSpeech() = Unit
                override fun onRmsChanged(rmsdB: Float) = Unit
                override fun onBufferReceived(buffer: ByteArray?) = Unit
                override fun onEndOfSpeech() = Unit
                override fun onEvent(eventType: Int, params: Bundle?) = Unit

                override fun onPartialResults(partialResults: Bundle?) {
                    partialResults.transcript()?.let(onPartial)
                }

                override fun onResults(results: Bundle?) {
                    finish()
                    val text = results.transcript()
                    if (text.isNullOrBlank()) {
                        onError("I did not catch that.")
                    } else {
                        onResult(text)
                    }
                }

                override fun onError(error: Int) {
                    val message = describe(error)
                    finish()
                    Log.i(TAG, "voice input ended: $message")
                    onError(message)
                }
            },
        )

        listening = true
        engine.startListening(sessionIntent())
    }

    /** Stops picking up audio but keeps the session, so the final result still arrives. */
    fun stop() {
        if (listening) recognizer?.stopListening()
    }

    /** Abandons the session; nothing is delivered to the callbacks. */
    fun cancel() {
        finish()
        recognizer?.cancel()
    }

    fun destroy() {
        finish()
        recognizer?.cancel()
        recognizer?.destroy()
        recognizer = null
    }

    private fun finish() {
        listening = false
    }

    private fun sessionIntent(): Intent =
        Intent(RecognizerIntent.ACTION_RECOGNIZE_SPEECH).apply {
            putExtra(RecognizerIntent.EXTRA_LANGUAGE_MODEL, RecognizerIntent.LANGUAGE_MODEL_FREE_FORM)
            putExtra(RecognizerIntent.EXTRA_LANGUAGE, Locale.getDefault().toLanguageTag())
            putExtra(RecognizerIntent.EXTRA_PARTIAL_RESULTS, true)
            putExtra(RecognizerIntent.EXTRA_MAX_RESULTS, 1)
            putExtra(RecognizerIntent.EXTRA_PREFER_OFFLINE, true)
        }

    private fun Bundle?.transcript(): String? =
        this?.getStringArrayList(SpeechRecognizer.RESULTS_RECOGNITION)?.firstOrNull()?.trim()

    private fun describe(code: Int): String = when (code) {
        SpeechRecognizer.ERROR_AUDIO -> "the microphone had a problem"
        SpeechRecognizer.ERROR_CLIENT -> "recognition was cancelled"
        SpeechRecognizer.ERROR_INSUFFICIENT_PERMISSIONS -> "microphone permission is required"
        SpeechRecognizer.ERROR_NETWORK -> "the recogniser wanted the network"
        SpeechRecognizer.ERROR_NETWORK_TIMEOUT -> "the recogniser timed out"
        SpeechRecognizer.ERROR_NO_MATCH -> "I did not catch that"
        SpeechRecognizer.ERROR_RECOGNIZER_BUSY -> "another recogniser is running"
        SpeechRecognizer.ERROR_SERVER -> "the device recogniser failed"
        SpeechRecognizer.ERROR_SPEECH_TIMEOUT -> "I did not hear anything"
        else -> "voice input failed (code $code)"
    }

    private companion object {
        const val TAG = "EternVoiceInput"
    }
}
