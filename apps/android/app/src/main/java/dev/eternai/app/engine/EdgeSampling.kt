package dev.eternai.app.engine

/**
 * Sampling settings for the resident edge model.
 *
 * Qwen publishes sampling parameters as complete sets per mode, and the sets only
 * work as written: an earlier build took the general figures but kept a repeat
 * penalty on and dynamic temperature enabled, which is how the edge model ended up
 * emitting nonsense. So the values are fixed here against the published presets
 * and the only choice the app offers is the mode, which is the same switch that
 * decides whether the model writes inside its reasoning block.
 */
data class SamplingPreset(
    val temperature: Float,
    val topP: Float,
    val topK: Int,
    val minP: Float,
    /** 1.0 means no repetition penalty. */
    val repetitionPenalty: Float,
    /** 0 means no presence penalty. */
    val presencePenalty: Float,
)

object EdgeSampling {

    /** Reasoning off: the preset for general tasks, the mode the phone runs in. */
    val instruct = SamplingPreset(
        temperature = 0.7f,
        topP = 0.8f,
        topK = 20,
        minP = 0.0f,
        repetitionPenalty = 1.0f,
        presencePenalty = 1.5f,
    )

    /**
     * Reasoning on: the general-task thinking preset. The coding preset (a cooler
     * temperature and no presence penalty) is deliberately not offered - a
     * companion chat is not a code-completion session.
     */
    val thinking = SamplingPreset(
        temperature = 1.0f,
        topP = 0.95f,
        topK = 20,
        minP = 0.0f,
        repetitionPenalty = 1.0f,
        presencePenalty = 1.5f,
    )

    fun forThinking(thinking: Boolean): SamplingPreset = if (thinking) this.thinking else instruct
}
