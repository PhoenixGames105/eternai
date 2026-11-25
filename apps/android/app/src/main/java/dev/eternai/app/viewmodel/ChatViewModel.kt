package dev.eternai.app.viewmodel

import android.app.Application
import android.net.Uri
import android.provider.OpenableColumns
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import dev.eternai.app.EternApplication
import dev.eternai.app.engine.Engine
import dev.eternai.app.engine.Finish
import dev.eternai.app.engine.GatewayEngine
import dev.eternai.app.engine.ImportResult
import dev.eternai.app.engine.LlamaEdgeEngine
import dev.eternai.app.engine.PersonalityPrompt
import dev.eternai.app.engine.Reply
import dev.eternai.app.engine.ServerEdgeEngine
import dev.eternai.app.engine.ToolLoop
import dev.eternai.app.net.HealthProbe
import dev.eternai.app.pair.PairingClient
import dev.eternai.app.sync.OutboxSync
import dev.eternai.app.sync.ConversationTurn
import dev.eternai.app.tools.EdgeToolRunner
import dev.eternai.app.tools.GatewayToolRunner
import dev.eternai.app.tools.ToolRunner
import dev.eternai.app.voice.VoiceInput
import dev.eternai.app.voice.VoiceOutput
import dev.eternai.core.EternException
import dev.eternai.core.MessageDto
import dev.eternai.core.PairTarget
import dev.eternai.core.SyncTurnDto
import dev.eternai.core.disposeWindow
import dev.eternai.core.mintTurnId
import dev.eternai.core.newChatTurn
import dev.eternai.core.route
import dev.eternai.core.syncTurnFromTurn
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import java.io.File

/**
 * Drives one conversation.
 *
 * The routing decision is taken per prompt from the shared Rust matrix rather than from a
 * cached "am I online" boolean, because the answer can change between typing and sending.
 * A turn is written to the durable outbox as soon as it exists, before the reply has even
 * finished streaming, so a process death mid-answer still leaves the question on disk.
 *
 * It must extend `AndroidViewModel`, not plain `ViewModel`: the default factory only hands
 * an `Application` to `AndroidViewModel` subclasses, and otherwise looks for a no-argument
 * constructor and dies when it finds none.
 */
class ChatViewModel(app: Application) : AndroidViewModel(app) {

    private val eternApp = app as EternApplication
    private val prefs = eternApp.prefs
    private val localContext = eternApp.context

    private val gatewayEngine: Engine = GatewayEngine(eternApp)
    private val llamaEngine: LlamaEdgeEngine = eternApp.legacyEdgeEngine
    private val serverEngine: ServerEdgeEngine = eternApp.serverEngine
    private val gatewayRunner: ToolRunner = GatewayToolRunner(eternApp)
    private val edgeRunner: ToolRunner = EdgeToolRunner(eternApp)

    /**
     * Voice I/O lives here rather than in the activity so a rotation does not
     * cut off a reply mid-sentence. The recogniser is main-thread only, hence
     * its own main dispatcher scope instead of `viewModelScope`.
     */
    private val voiceInput = VoiceInput(eternApp)
    private val voiceOutput = VoiceOutput(eternApp)
    private val voiceScope = MainScope()

    private val _ui = MutableStateFlow(
        UiState(
            paired = prefs.paired,
            speakReplies = prefs.speakReplies,
            personality = prefs.personality,
            personalitySyncStatus = null,
            edgeMaxTokens = prefs.edgeMaxTokens,
            edgeThinking = prefs.edgeThinking,
            engineIsServer = prefs.edgeUseServer,
            voiceAvailable = voiceInput.available,
        ),
    )
    val ui: StateFlow<UiState> = _ui.asStateFlow()

    val pairing = PairingClient(eternApp)

    init {
        // Mirror the probe into the UI so the model chip updates without polling.
        viewModelScope.launch {
            eternApp.probe.state.collect { state ->
                val paired = prefs.paired
                _ui.update {
                    it.copy(
                        paired = paired,
                        target = state.decision.target,
                        reason = state.decision.reason,
                        engineState = state.health?.engineState ?: "offline",
                        model = state.health?.model.orEmpty(),
                        failures = state.consecutiveFailures,
                    )
                }
            }
        }
        viewModelScope.launch {
            localContext.turns.collect { turns ->
                _ui.update { state ->
                    if (state.busy) state else state.copy(bubbles = contextBubbles(turns))
                }
            }
        }
    }

    data class Bubble(
        val id: String,
        val role: String,
        val text: String,
        val streaming: Boolean = false,
        val failed: Boolean = false,
    )

    /** One GGUF the user has copied into the app's model directory. */
    data class ModelOption(val path: String, val name: String, val sizeMb: Long)

    /** Progress of an in-flight import; null while nothing is copying. */
    data class ImportProgress(val name: String, val copiedMb: Long, val totalMb: Long)

    data class UiState(
        val paired: Boolean = false,
        val target: String = "edge",
        val reason: String = "not paired",
        val engineState: String = "offline",
        val model: String = "",
        val failures: Long = 0,
        val bubbles: List<Bubble> = emptyList(),
        val busy: Boolean = false,
        val notice: String? = null,
        val pairTarget: PairTarget? = null,
        val showModels: Boolean = false,
        val listening: Boolean = false,
        val speakReplies: Boolean = false,
        val personality: String = "",
        val personalitySyncStatus: String? = null,
        /** Requested edge reply cap; the active context window may lower it. */
        val edgeMaxTokens: Int = 1024,
        /** False when the device ships no recogniser; a dead mic button is worse than none. */
        val voiceAvailable: Boolean = true,
        val importing: ImportProgress? = null,
        /** Bumped whenever the model directory changes so the sheet re-reads it. */
        val modelsVersion: Int = 0,
        /** Whether the edge model may write inside its reasoning block. */
        val edgeThinking: Boolean = false,
        /** True when edge prompts go to the embedded llama.cpp server. */
        val engineIsServer: Boolean = true,
        /** Opens the embedded server's log viewer. */
        val showServerLog: Boolean = false,
    ) {
        val online: Boolean get() = target == "high_power"
    }

    fun consumeNotice() = _ui.update { it.copy(notice = null) }

    // ------------------------------------------------------------------- voice

    /**
     * Starts or stops dictation. The transcript lands in the caller's prompt
     * field through [onTranscript] rather than being sent directly, so a mishearing
     * can be corrected before it costs a generation.
     */
    fun toggleMic(onTranscript: (String) -> Unit) {
        if (_ui.value.listening) {
            voiceInput.stop()
            return
        }
        if (_ui.value.busy || !_ui.value.voiceAvailable) return
        voiceOutput.stop()
        _ui.update { it.copy(listening = true, notice = null) }
        voiceScope.launch {
            voiceInput.start(
                onPartial = { },
                onResult = { text ->
                    _ui.update { it.copy(listening = false) }
                    onTranscript(text)
                },
                onError = { message ->
                    _ui.update { it.copy(listening = false, notice = message) }
                },
            )
        }
    }

    /** Cancels dictation without using a partial transcript (sheet dismissed, screen left). */
    fun cancelListening() {
        voiceInput.cancel()
        _ui.update { it.copy(listening = false) }
    }

    /** Turns spoken replies on or off, stopping any reply already in progress. */
    fun toggleSpeakReplies() {
        val enabled = !_ui.value.speakReplies
        prefs.speakReplies = enabled
        _ui.update { it.copy(speakReplies = enabled) }
        if (!enabled) voiceOutput.stop()
    }

    fun showModelPicker() = _ui.update { it.copy(showModels = true, notice = null) }

    fun hideModelPicker() = _ui.update { it.copy(showModels = false) }

    fun refreshPersonality() {
        _ui.update { it.copy(personality = prefs.personality) }
        if (!prefs.paired) return
        viewModelScope.launch(Dispatchers.IO) {
            val report = eternApp.sync.syncPersonality()
            _ui.update {
                it.copy(
                    personality = prefs.personality,
                    personalitySyncStatus = report.error?.let { error ->
                        "Saved locally; sync failed: $error"
                    } ?: "Personality synced.",
                )
            }
        }
    }

    fun savePersonality(rawValue: String) {
        val value = rawValue.trim()
        if (value.codePointCount(0, value.length) > PersonalityPrompt.MAX_CHARS) {
            _ui.update {
                it.copy(personalitySyncStatus = "Personality must be ${PersonalityPrompt.MAX_CHARS} characters or fewer.")
            }
            return
        }
        val updatedAt = maxOf(System.currentTimeMillis(), prefs.personalityUpdatedAt + 1)
        try {
            prefs.savePersonality(value, updatedAt)
        } catch (e: IllegalArgumentException) {
            _ui.update { it.copy(personalitySyncStatus = e.message ?: "personality could not be saved") }
            return
        }
        _ui.update {
            it.copy(
                personality = value,
                personalitySyncStatus = if (prefs.paired) "Saved locally; syncing…" else "Saved locally; sync when paired.",
            )
        }
        if (!prefs.paired) return
        viewModelScope.launch(Dispatchers.IO) {
            val report = eternApp.sync.syncPersonality()
            _ui.update {
                it.copy(
                    personality = prefs.personality,
                    personalitySyncStatus = report.error?.let { error ->
                        "Saved locally; sync failed: $error"
                    } ?: "Personality saved and synced.",
                )
            }
        }
    }

    fun beginPairing() = _ui.update { it.copy(pairTarget = null, notice = null) }

    fun cancelPairing() = _ui.update { it.copy(pairTarget = null) }

    fun onQrScanned(text: String) {
        when (val result = pairing.parse(text)) {
            is PairingClient.Result.Ready -> _ui.update { it.copy(pairTarget = result.target, notice = null) }
            is PairingClient.Result.Failure -> _ui.update { it.copy(notice = result.message) }
            is PairingClient.Result.Success -> Unit
        }
    }

    fun confirmPairing(deviceName: String) {
        val target = _ui.value.pairTarget ?: return
        viewModelScope.launch(Dispatchers.IO) {
            _ui.update { it.copy(busy = true, notice = null) }
            when (val result = pairing.redeem(target, deviceName)) {
                is PairingClient.Result.Success -> _ui.update {
                    it.copy(
                        busy = false,
                        paired = true,
                        pairTarget = null,
                        notice = "paired as " + result.name + " with " + result.host,
                    )
                }

                is PairingClient.Result.Failure -> _ui.update {
                    it.copy(busy = false, pairTarget = null, notice = result.message)
                }

                is PairingClient.Result.Ready -> Unit
            }
        }
    }

    fun send(prompt: String) {
        val text = prompt.trim()
        if (text.isEmpty() || _ui.value.busy) return

        val turnId = mintTurnId()
        _ui.update {
            it.copy(
                busy = true,
                bubbles = it.bubbles +
                    Bubble(turnId, "user", text) +
                    Bubble(turnId, "assistant", "", streaming = true),
            )
        }

        viewModelScope.launch(Dispatchers.IO) {
            eternApp.beginTurn()
            try {
                val probe = eternApp.probe.state.value
                val decision = route(
                    probe.consecutiveFailures < HealthProbe.FAILURES_BEFORE_OFFLINE,
                    probe.health?.engineServing ?: false,
                )
                // The embedded llama.cpp server is the default edge engine; the legacy
                // JNI engine stays reachable from the model sheet for A/B comparison.
                val edge = if (prefs.edgeUseServer) serverEngine else llamaEngine
                val engine = if (decision.target == "high_power") gatewayEngine else edge
                // Tools run wherever the model is: an online phone asks the desktop's
                // runtime, an offline one uses its own jail.
                val runner = if (engine === gatewayEngine) gatewayRunner else edgeRunner
                runner.turnId = turnId

                if (engine === gatewayEngine) {
                    val profile = eternApp.sync.syncPersonality()
                    _ui.update {
                        it.copy(
                            personality = prefs.personality,
                            personalitySyncStatus = profile.error?.let { error ->
                                "Saved locally; sync failed: $error"
                            } ?: "Personality synced.",
                        )
                    }
                }

                if (engine !== gatewayEngine) {
                    val personalityOverhead = PersonalityPrompt.estimatedTokens(prefs.personality)
                    val edgeWindow = disposeWindow(
                        localContext.turnsForEdgeWindow(),
                        OutboxSync.DEFAULT_EDGE_CTX,
                        OutboxSync.DEFAULT_RESPONSE_RESERVE,
                        OutboxSync.DEFAULT_EDGE_OVERHEAD + personalityOverhead,
                        null,
                    )
                    localContext.setWindow(edgeWindow.turns)
                }

                val messages = buildList {
                    PersonalityPrompt.message(prefs.personality)?.let(::add)
                    addAll(localContext.messages())
                    add(MessageDto("user", text, null, null, null))
                }
                // StateFlow.update is atomic, so the stream callback can stay on the IO thread.
                val outcome = ToolLoop(engine, runner, turnId).run(
                    messages,
                    onTool = { result -> appendToolStep(turnId, result) },
                    onChunk = { delta -> appendDelta(turnId, delta) },
                )

                finalize(turnId, text, outcome)
            } catch (e: Exception) {
                _ui.update { it.copy(notice = describeError("chat failed", e)) }
            } finally {
                eternApp.endTurn()
                val turns = localContext.turns.value
                val bubbles = contextBubbles(turns)
                val visible = if (turns.any { it.turnId == turnId }) {
                    bubbles
                } else {
                    val pending = _ui.value.bubbles
                        .filter { it.id == turnId || it.id.startsWith("$turnId:tool:") }
                        .map { if (it.role == "assistant") it.copy(streaming = false) else it }
                    bubbles + pending
                }
                _ui.update {
                    it.copy(busy = false, bubbles = visible)
                }
            }
        }
    }

    /**
     * Wipes the conversation: the phone's context, its outbox, and (when the
     * desktop is reachable) this device's turns in the desktop store.
     *
     * Order matters. Anything still unsynced is pushed first so genuine history
     * reaches the journal before it is deleted, then the desktop copy goes, then
     * the local state. Clearing the phone alone would be undone by the next
     * window pull, which is built from the desktop's database.
     *
     * The embedded server is restarted afterwards: it holds its own conversation
     * state only through the messages we send, but the KV cache is rebuilt per
     * request, so a drop is enough to keep stale tokens out of the next prompt.
     */
    fun clearContext() {
        if (_ui.value.busy) return
        viewModelScope.launch(Dispatchers.IO) {
            _ui.update { it.copy(notice = "clearing context...") }
            val paired = prefs.paired
            val remote = eternApp.clearConversation()
            // The window watermark has to go with the window, or the next pull
            // would look "already up to date" and reinstall the turns we just
            // deleted.
            prefs.lastSeenWindowSeq = 0
            _ui.update {
                it.copy(
                    bubbles = emptyList(),
                    notice = when {
                        remote.isSuccess -> "context cleared (" + remote.getOrDefault(0) + " desktop turn(s) removed)"
                        paired -> "context cleared locally; desktop still holds its copy (" +
                            (remote.exceptionOrNull()?.message ?: "unreachable") + ")"
                        else -> "context cleared locally; not paired, so nothing was removed from the desktop"
                    },
                )
            }
        }
    }

    /** Manual "sync now" for the settings sheet. */
    fun syncNow() {
        viewModelScope.launch(Dispatchers.IO) {
            _ui.update { it.copy(notice = "syncing...") }
            val outcome = eternApp.syncNow()
            _ui.update {
                it.copy(
                    notice = describe(outcome),
                    personality = prefs.personality,
                    personalitySyncStatus = outcome.personality?.error?.let { error ->
                        "Saved locally; sync failed: $error"
                    } ?: if (outcome.personality != null) "Personality synced." else it.personalitySyncStatus,
                )
            }
        }
    }

    /** Where the user is expected to place GGUFs; shown in the picker. */
    val modelsDirectory: String get() = llamaEngine.modelsDirectory

    /**
     * GGUFs sitting in the app's model directory.
     *
     * Files are copied in over adb or a file manager rather than downloaded: a
     * multi-gigabyte model over a phone browser has no resume, and the desktop
     * already holds the file.
     */
    fun availableModels(): List<ModelOption> =
        llamaEngine.installedModels().map {
            ModelOption(it.absolutePath, it.name, it.length() / (1024 * 1024))
        }

    val selectedModelPath: String? get() = prefs.edgeModelPath

    /** Switches the edge model, dropping the resident one so the new file loads. */
    fun selectModel(path: String) {
        prefs.edgeModelPath = path
        viewModelScope.launch(Dispatchers.IO) {
            llamaEngine.unload()
            serverEngine.unload()
            _ui.update { it.copy(notice = "edge model set to " + File(path).name) }
        }
    }

    /**
     * Allows or suppresses the edge model's chain-of-thought.
     *
     * The flag is handed to the native engine at load time, so changing it drops
     * the resident model; the next prompt pays one reload to honour it.
     */
    fun toggleEdgeThinking() {
        val enabled = !_ui.value.edgeThinking
        prefs.edgeThinking = enabled
        _ui.update { it.copy(edgeThinking = enabled) }
        viewModelScope.launch(Dispatchers.IO) {
            llamaEngine.unload()
            serverEngine.unload()
        }
    }

    /** Saves the edge reply cap; the server applies it per request. */
    fun setEdgeMaxTokens(tokens: Int) {
        val value = tokens.coerceIn(
            dev.eternai.app.data.MIN_EDGE_MAX_TOKENS,
            dev.eternai.app.data.MAX_EDGE_MAX_TOKENS,
        )
        prefs.edgeMaxTokens = value
        _ui.update { it.copy(edgeMaxTokens = value) }
        if (!prefs.edgeUseServer) {
            viewModelScope.launch(Dispatchers.IO) { llamaEngine.unload() }
        }
    }

    /**
     * Flips between the embedded llama.cpp server and the legacy JNI engine.
     *
     * Diagnostic switch: if one engine produces nonsense and the other does not,
     * the fault lies in the engine rather than in the model.
     */
    fun toggleEdgeEngine() {
        val useServer = !prefs.edgeUseServer
        prefs.edgeUseServer = useServer
        _ui.update { it.copy(engineIsServer = useServer) }
        viewModelScope.launch(Dispatchers.IO) {
            llamaEngine.unload()
            serverEngine.unload()
            val name = if (useServer) "llama-server (embedded)" else "legacy JNI engine"
            _ui.update { it.copy(notice = "edge engine set to $name") }
        }
    }

    fun showServerLog() = _ui.update { it.copy(showServerLog = true, notice = null) }

    fun hideServerLog() = _ui.update { it.copy(showServerLog = false) }

    /** Path of the embedded server's log file, for the debug viewer. */
    val serverLogPath: String get() = serverEngine.logFile.absolutePath

    /** Tail of the server log for the debug viewer; empty if nothing was written yet. */
    fun readServerLog(): String = serverEngine.readLogTail()

    /**
     * Copies the whole embedded-server log to a user-picked document.
     *
     * The file lives in app-private storage, so without this the only way to see
     * a long log is `adb run-as`.
     */
    fun exportLog(uri: Uri) {
        viewModelScope.launch(Dispatchers.IO) {
            _ui.update { it.copy(notice = "exporting log...") }
            val result = serverEngine.exportLog(uri)
            _ui.update {
                it.copy(
                    notice = result.fold(
                        { bytes -> "log exported (" + bytes / 1024 + " KB)" },
                        { t -> "log export failed: " + (t.message ?: t.javaClass.simpleName) },
                    ),
                )
            }
        }
    }

    /**
     * Copies a user-picked GGUF into the app's private model directory and selects it.
     *
     * A multi-gigabyte copy takes long enough that the user needs to see it moving, and
     * the persistable URI permission is released afterwards so the provider slot does not
     * leak.
     */
    fun importModel(uri: Uri) {
        if (_ui.value.importing != null) return
        val app = getApplication<Application>()
        viewModelScope.launch(Dispatchers.IO) {
            val name = displayName(uri) ?: "model.gguf"
            val size = statSize(uri)
            try {
                val stream = app.contentResolver.openInputStream(uri)
                if (stream == null) {
                    _ui.update { it.copy(notice = "$name could not be opened") }
                    return@launch
                }
                when (val result = llamaEngine.importModel(stream, name, size) { copied ->
                    _ui.update {
                        it.copy(
                            importing = ImportProgress(
                                name = name,
                                copiedMb = copied / BYTES_PER_MB,
                                totalMb = size / BYTES_PER_MB,
                            ),
                        )
                    }
                }) {
                    is ImportResult.Installed -> {
                        _ui.update {
                            it.copy(
                                importing = null,
                                modelsVersion = it.modelsVersion + 1,
                                showModels = true,
                            )
                        }
                        selectModel(result.file.absolutePath)
                    }

                    is ImportResult.Failed -> _ui.update {
                        it.copy(importing = null, notice = result.reason)
                    }
                }
            } catch (e: Throwable) {
                _ui.update { it.copy(importing = null, notice = "import failed: ${e.message}") }
            }
        }
    }

    /** The name the user sees in their file manager, not the content URI's last segment. */
    private fun displayName(uri: Uri): String? {
        val app = getApplication<Application>()
        app.contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst() && !c.isNull(0)) return c.getString(0)
        }
        return uri.lastPathSegment?.substringAfterLast('/')
    }

    private fun statSize(uri: Uri): Long {
        val app = getApplication<Application>()
        app.contentResolver.query(uri, arrayOf(OpenableColumns.SIZE), null, null, null)?.use { c ->
            if (c.moveToFirst() && !c.isNull(0)) return c.getLong(0)
        }
        return 0L
    }

    private fun appendDelta(turnId: String, delta: String) {
        if (delta.isEmpty()) return
        _ui.update { state ->
            val bubbles = state.bubbles.toMutableList()
            val index = bubbles.indexOfLast { it.id == turnId && it.role == "assistant" }
            if (index >= 0) {
                val assistant = bubbles.removeAt(index)
                bubbles += assistant.copy(text = assistant.text + delta)
            }
            state.copy(bubbles = bubbles)
        }
    }

    private fun appendToolStep(turnId: String, result: dev.eternai.app.tools.ToolResult) {
        val label = toolLabel(result.name, result.ok)

        _ui.update { state ->
            val sequence = state.bubbles.count { it.id.startsWith("$turnId:tool:") }
            state.copy(
                bubbles = state.bubbles + Bubble(
                    id = "$turnId:tool:$sequence",
                    role = "tool",
                    text = label,
                    failed = !result.ok,
                ),
            )
        }
    }

    private fun contextBubbles(turns: List<ConversationTurn>): List<Bubble> = buildList {
        turns.forEach { turn ->
            turn.messages.forEachIndexed { index, message ->
                when (message.role) {
                    "user" -> add(Bubble("${turn.turnId}:$index", "user", message.content))
                    "assistant" -> if (message.content.isNotBlank()) {
                        add(Bubble("${turn.turnId}:$index", "assistant", message.content))
                    }
                    "tool" -> {
                        val failed = Regex("\"ok\"\\s*:\\s*false").containsMatchIn(message.content)
                        add(
                            Bubble(
                                "${turn.turnId}:$index",
                                "tool",
                                toolLabel(message.name ?: "tool", !failed),
                                failed = failed,
                            ),
                        )
                    }
                }
            }
        }
    }

    private fun toolLabel(name: String, ok: Boolean): String {
        val action = when (name) {
            "read_file" -> "read a file"
            "write_file" -> "wrote a file"
            "list_dir" -> "listed a folder"
            "web_search" -> "searched the web"
            "remember" -> "saved a memory"
            "recall" -> "searched memories"
            "journal_read" -> "read the journal"
            else -> "used $name"
        }
        return if (ok) action else "$action failed"
    }

    /**
     * Records the exchange in the live context and the durable outbox.
     *
     * Even a failed reply is recorded: the question is real history, and the desktop journal
     * should show that the phone was offline rather than silently skipping the exchange.
     * Tool steps are recorded too, so a turn that happened offline arrives on the desktop
     * with the actions the edge model took still attached.
     */
    private suspend fun finalize(turnId: String, prompt: String, outcome: ToolLoop.ToolOutcome) {
        val reply = outcome.reply
        val seq = runCatching { eternApp.outbox.nextSeq() }.getOrDefault(0L)
        val ts = System.currentTimeMillis()
        val body = outcome.text.ifEmpty { reply?.error ?: "(no reply)" }

        val turn = runCatching {
            newChatTurn(
                turnId = turnId,
                userText = prompt,
                assistantText = body,
                engine = reply?.engine ?: "edge",
                model = reply?.model.orEmpty(),
                deviceId = prefs.device?.deviceId.orEmpty(),
                seq = seq,
                ts = ts,
                toolMessages = outcome.steps,
            )
        }.getOrElse { e ->
            _ui.update { it.copy(notice = describeError("could not record turn", e)) }
            return
        }

        val sync: SyncTurnDto = runCatching { syncTurnFromTurn(turn) }
            .getOrElse { e ->
                _ui.update { it.copy(notice = describeError("could not serialise turn", e)) }
                return
            }

        try {
            eternApp.appendTurn(sync)
        } catch (e: Exception) {
            _ui.update { it.copy(notice = describeError("outbox write failed", e)) }
        }

        _ui.update { state ->
            state.copy(
                notice = when {
                    outcome.aborted -> "tool rounds exceeded the limit"
                    reply?.finish == Finish.Error -> reply.error
                    else -> state.notice
                },
                bubbles = state.bubbles.map {
                    if (it.id == turnId && it.role == "assistant") {
                        it.copy(text = it.text.ifEmpty { body }, streaming = false)
                    } else {
                        it
                    }
                },
            )
        }

        // Delivered online: the desktop already has this turn in its store, so hand over
        // anything else still queued behind it.
        if (reply?.engine == "high_power" && reply.finish == Finish.Stop) {
            viewModelScope.launch(Dispatchers.IO) { eternApp.syncNow() }
        }

        if (prefs.speakReplies && reply?.finish == Finish.Stop) voiceOutput.speak(body)
    }

    private companion object {
        const val BYTES_PER_MB = 1024L * 1024L
    }

    override fun onCleared() {
        cancelListening()
        voiceInput.destroy()
        voiceOutput.shutdown()
        voiceScope.cancel()
        super.onCleared()
    }

    private fun describe(outcome: OutboxSync.SyncOutcome): String {
        val flush = outcome.flush
        val base = when {
            flush.error == "not paired" -> "not paired"
            flush.delivered > 0 -> "sent " + flush.delivered + " turn(s)"
            flush.error != null -> "sync failed: " + flush.error
            else -> "nothing to send"
        }
        val withMemory = when (val memory = outcome.memory) {
            null -> base
            else -> base + (memory.error?.let { ", memory sync failed: $it" } ?: ", memory synced")
        }
        val withPersonality = when (val personality = outcome.personality) {
            null -> withMemory
            else -> withMemory + (personality.error?.let { ", personality sync failed: $it" } ?: ", personality synced")
        }
        val withTranscript = when (val transcript = outcome.transcript) {
            is OutboxSync.TranscriptPull.Installed -> withPersonality + ", history " + transcript.turns + " turn(s)"
            is OutboxSync.TranscriptPull.Error -> withPersonality + ", history failed: " + transcript.message
            null -> withPersonality
        }
        return when (val w = outcome.window) {
            is OutboxSync.WindowPull.Installed -> withTranscript + ", window " + w.installed + "/" + w.proposed + " turn(s)"
            is OutboxSync.WindowPull.Error -> withTranscript + ", window failed: " + w.message
            null -> withTranscript
        }
    }

    private fun describeError(what: String, e: Throwable): String =
        what + ": " + (if (e is EternException) e.message else e.message ?: e.toString())
}
