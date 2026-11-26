package dev.eternai.app.ui

import android.Manifest
import android.content.pm.PackageManager
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
import androidx.compose.material3.FilterChip
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.ui.input.pointer.pointerInput
import android.widget.Toast
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import dev.eternai.app.data.MAX_EDGE_MAX_TOKENS
import dev.eternai.app.data.MIN_EDGE_MAX_TOKENS
import dev.eternai.app.viewmodel.ChatViewModel

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen(vm: ChatViewModel) {
    val ui by vm.ui.collectAsStateWithLifecycle()
    var prompt by rememberSaveable { mutableStateOf("") }
    var deviceName by rememberSaveable { mutableStateOf("Galaxy Fold") }

    // The recogniser needs RECORD_AUDIO at runtime; the mic is only offered once
    // it has been granted, so a tap never silently does nothing.
    val micPermission = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (!granted) vm.cancelListening()
    }
    LaunchedEffect(ui.listening) {
        // A reply that starts while dictation is open would be read over the
        // user's own voice; drop the session instead.
        if (ui.listening && ui.busy) vm.cancelListening()
    }
    DisposableEffect(Unit) {
        onDispose { vm.cancelListening() }
    }

    val micContext = LocalContext.current
    fun onMic() {
        if (ui.listening) {
            // The transcript from a stopped session arrives through the callback
            // registered when dictation started.
            vm.toggleMic { }
            return
        }
        val granted = ContextCompat.checkSelfPermission(micContext, Manifest.permission.RECORD_AUDIO) ==
            PackageManager.PERMISSION_GRANTED
        if (granted) {
            vm.toggleMic { transcript ->
                prompt = if (prompt.isBlank()) transcript else "$prompt $transcript"
            }
        } else {
            micPermission.launch(Manifest.permission.RECORD_AUDIO)
        }
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("eternAI") },
                actions = {
                    StatusChip(ui)
                    TextButton(onClick = { vm.showModelPicker() }, enabled = !ui.busy) {
                        Text("Model")
                    }
                    TextButton(onClick = { vm.syncNow() }, enabled = ui.paired && !ui.busy) {
                        Text("Sync")
                    }
                },
            )
        },
    ) { padding ->
        // Android 15 draws edge-to-edge and stops resizing the window for the
        // keyboard, so adjustResize no longer lifts anything: the input row has to
        // move by the IME inset itself. That inset is measured from the bottom of
        // the screen and therefore includes the navigation bar the scaffold has
        // already padded, so only the part above it is added here.
        val density = LocalDensity.current
        val imeBottom = WindowInsets.ime.getBottom(density)
        val barBottom = WindowInsets.navigationBars.getBottom(density)
        val keyboardHeight = with(density) { (imeBottom - barBottom).coerceAtLeast(0).toDp() }

        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(bottom = keyboardHeight)
                .padding(horizontal = 12.dp),
        ) {
            if (!ui.paired) {
                PairingSheet(vm, deviceName) { deviceName = it }
                return@Column
            }

            ui.notice?.let { notice ->
                Card(
                    Modifier
                        .fillMaxWidth()
                        .padding(bottom = 8.dp),
                    colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
                ) {
                    Row(
                        Modifier.fillMaxWidth(),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Text(
                            notice,
                            style = MaterialTheme.typography.bodySmall,
                            modifier = Modifier
                                .weight(1f)
                                .padding(12.dp),
                        )
                        TextButton(onClick = { vm.consumeNotice() }) { Text("OK") }
                    }
                }
            }

            val listState = rememberLazyListState()
            val bubbles = ui.bubbles
            // Re-anchor when the keyboard opens too: the viewport shrinks, which can
            // otherwise leave the newest bubble hidden behind the input row.
            LaunchedEffect(bubbles.size, bubbles.lastOrNull()?.text?.length, keyboardHeight) {
                if (bubbles.isNotEmpty()) listState.animateScrollToItem(bubbles.lastIndex)
            }

            Box(Modifier.weight(1f)) {
                if (bubbles.isEmpty()) {
                    Text(
                        "Offline prompts are answered by the edge model and queued for the " +
                            "desktop; online prompts go straight to the high power model.",
                        style = MaterialTheme.typography.bodyMedium,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        modifier = Modifier.padding(16.dp),
                    )
                } else {
                    LazyColumn(
                        state = listState,
                        verticalArrangement = Arrangement.spacedBy(8.dp),
                        modifier = Modifier.fillMaxSize(),
                    ) {
                        items(bubbles, key = { it.id + it.role }) { bubble ->
                            BubbleRow(bubble)
                        }
                        if (ui.busy && bubbles.lastOrNull()?.role != "assistant") {
                            item {
                                Row(verticalAlignment = Alignment.CenterVertically) {
                                    CircularProgressIndicator(Modifier.height(16.dp).widthIn(max = 16.dp))
                                    Spacer(Modifier.height(8.dp))
                                }
                            }
                        }
                    }
                }
            }

            Row(
                Modifier
                    .fillMaxWidth()
                    .padding(top = 8.dp, bottom = 12.dp),
                verticalAlignment = Alignment.Bottom,
                horizontalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (ui.voiceAvailable) {
                    OutlinedButton(
                        onClick = { onMic() },
                        enabled = !ui.busy || ui.listening,
                    ) { Text(if (ui.listening) "Listening" else "Mic") }
                }
                OutlinedButton(onClick = { vm.toggleSpeakReplies() }) {
                    Text(if (ui.speakReplies) "Speak: on" else "Speak: off")
                }
                OutlinedTextField(
                    value = prompt,
                    onValueChange = { prompt = it },
                    label = {
                        Text(
                            when {
                                ui.listening -> "Listening..."
                                ui.online -> "Ask the high power model"
                                else -> "Ask the edge model"
                            },
                        )
                    },
                    maxLines = 5,
                    modifier = Modifier
                        .weight(1f)
                        .heightIn(max = 160.dp),
                )
                Button(
                    onClick = {
                        vm.send(prompt)
                        prompt = ""
                    },
                    enabled = prompt.isNotBlank() && !ui.busy,
                ) { Text("Send") }
            }
        }

        if (ui.showModels) {
            ModelSheet(vm)
        }

        if (ui.showServerLog) {
            ServerLogSheet(vm)
        }
    }
}

/**
 * Tails the embedded llama.cpp server's log file.
 *
 * The server writes straight to disk, so the viewer just re-reads the tail on
 * a timer rather than plumbing a second in-process stream.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ServerLogSheet(vm: ChatViewModel) {
    var lines by remember { mutableStateOf("reading log...") }
    LaunchedEffect(Unit) {
        while (true) {
            lines = vm.readServerLog()
            kotlinx.coroutines.delay(1500)
        }
    }

    // The log lives in app-private storage, which no file manager can reach on
    // Android 11+, so the system's create-document flow is the only way out.
    val exporter = rememberLauncherForActivityResult(ActivityResultContracts.CreateDocument("text/plain")) { uri: Uri? ->
        if (uri != null) vm.exportLog(uri)
    }

    ModalBottomSheet(onDismissRequest = { vm.hideServerLog() }) {
        Column(Modifier.padding(horizontal = 20.dp, vertical = 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text("Server log", style = MaterialTheme.typography.titleMedium, modifier = Modifier.weight(1f))
            }
            Text(
                vm.serverLogPath,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(8.dp))
            Box(
                Modifier
                    .fillMaxWidth()
                    .heightIn(max = 420.dp)
                    .verticalScroll(rememberScrollState()),
            ) {
                Text(
                    lines,
                    style = MaterialTheme.typography.bodySmall.copy(fontFamily = FontFamily.Monospace),
                )
            }
            Spacer(Modifier.height(16.dp))
            OutlinedButton(
                onClick = { exporter.launch("llama-server-" + System.currentTimeMillis() + ".txt") },
                modifier = Modifier.fillMaxWidth(),
            ) { Text("Export full log") }
            Spacer(Modifier.height(16.dp))
        }
    }
}

@Composable
private fun StatusChip(ui: ChatViewModel.UiState) {
    val label = when {
        !ui.paired -> "not paired"
        ui.online -> "desktop: " + (ui.model.ifEmpty { ui.engineState })
        else -> "edge (" + ui.engineState + ")"
    }
    AssistChip(onClick = { }, label = { Text(label, style = MaterialTheme.typography.labelSmall) })
}

/**
 * Lets the user choose which GGUF the edge engine loads.
 *
 * Models are not downloaded here: they are copied into the app's files
 * directory (the path is shown), which keeps a multi-gigabyte transfer out of
 * the app and lets the same file be reused after a reinstall.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ModelSheet(vm: ChatViewModel) {
    val ui by vm.ui.collectAsStateWithLifecycle()
    val models = remember(ui.showModels, ui.modelsVersion) { vm.availableModels() }
    var confirmClear by remember { mutableStateOf(false) }
    var personalityDraft by remember(ui.personality) { mutableStateOf(ui.personality) }
    LaunchedEffect(ui.showModels) {
        if (ui.showModels) vm.refreshPersonality()
    }

    // The model directory is private storage, unreachable for a file manager on
    // Android 11+, so importing through the system picker is the only way in.
    // "*/*" because most managers report a GGUF as application/octet-stream at best.
    val picker = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        if (uri != null) vm.importModel(uri)
    }

    ModalBottomSheet(onDismissRequest = { vm.hideModelPicker() }) {
        Column(
            Modifier
                .verticalScroll(rememberScrollState())
                .padding(horizontal = 20.dp, vertical = 8.dp),
        ) {
            Text("Edge model", style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.height(4.dp))
            Text(
                "Models live in " + vm.modelsDirectory + ", which only this app can read.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(12.dp))

            val importing = ui.importing
            if (importing != null) {
                val fraction = if (importing.totalMb > 0) {
                    (importing.copiedMb.toFloat() / importing.totalMb.toFloat()).coerceIn(0f, 1f)
                } else {
                    0f
                }
                LinearProgressIndicator(
                    progress = { fraction },
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.height(6.dp))
                Text(
                    "Copying " + importing.name + ": " + importing.copiedMb.toString() + " MB" +
                        if (importing.totalMb > 0) " of " + importing.totalMb.toString() + " MB" else "",
                    style = MaterialTheme.typography.bodySmall,
                )
                Spacer(Modifier.height(12.dp))
            } else {
                OutlinedButton(
                    onClick = { picker.launch(arrayOf("*/*")) },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("Import GGUF") }
                Spacer(Modifier.height(12.dp))
            }

            if (models.isEmpty()) {
                Text(
                    "No models installed. Offline prompts will fail until one is added.",
                    style = MaterialTheme.typography.bodyMedium,
                )
            } else {
                models.forEach { option ->
                    val selected = option.path == vm.selectedModelPath
                    Row(
                        Modifier
                            .fillMaxWidth()
                            .padding(vertical = 4.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(Modifier.weight(1f)) {
                            Text(option.name, style = MaterialTheme.typography.bodyMedium)
                            Text(
                                option.sizeMb.toString() + " MB",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                        if (selected) {
                            TextButton(onClick = { vm.hideModelPicker() }) { Text("in use") }
                        } else {
                            Button(
                                onClick = {
                                    vm.selectModel(option.path)
                                    vm.hideModelPicker()
                                },
                                enabled = !ui.busy,
                            ) { Text("Use") }
                        }
                    }
                }
            }
            Spacer(Modifier.height(16.dp))

            var maxTokens by remember(ui.edgeMaxTokens) {
                mutableStateOf(ui.edgeMaxTokens.toFloat())
            }
            Text(
                "Maximum reply length: " + maxTokens.toInt() + " tokens",
                style = MaterialTheme.typography.bodyMedium,
            )
            Slider(
                value = maxTokens,
                onValueChange = { maxTokens = it },
                onValueChangeFinished = { vm.setEdgeMaxTokens(maxTokens.toInt()) },
                valueRange = MIN_EDGE_MAX_TOKENS.toFloat()..MAX_EDGE_MAX_TOKENS.toFloat(),
                steps = 199,
                modifier = Modifier.fillMaxWidth(),
            )
            Text(
                "The context window limits the actual reply length. Older turns are trimmed first; " +
                    "if needed, the reply cap is reduced to fit.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(16.dp))

            Row(
                Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(Modifier.weight(1f)) {
                    Text("Think before answering", style = MaterialTheme.typography.bodyMedium)
                    Text(
                        "Reasoning models spend minutes writing out their chain of thought " +
                            "before any answer appears. Turning this off makes them reply " +
                            "directly. Changing it reloads the model.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Spacer(Modifier.width(12.dp))
                Switch(
                    checked = ui.edgeThinking,
                    onCheckedChange = { vm.toggleEdgeThinking() },
                )
            }
            Spacer(Modifier.height(16.dp))

            Row(
                Modifier.fillMaxWidth(),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Column(Modifier.weight(1f)) {
                    Text("Embedded llama.cpp server", style = MaterialTheme.typography.bodyMedium)
                    Text(
                        "Edge prompts are served by the official llama.cpp server running " +
                            "inside the app. Turning this off falls back to the legacy " +
                            "built-in engine, which is useful for comparing output quality.",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                }
                Spacer(Modifier.width(12.dp))
                Switch(
                    checked = ui.engineIsServer,
                    onCheckedChange = { vm.toggleEdgeEngine() },
                )
            }
            if (ui.engineIsServer) {
                Spacer(Modifier.height(8.dp))
                Text("Compute backend", style = MaterialTheme.typography.bodyMedium)
                Text(
                    "CPU is the fastest option measured on this device. The GPU " +
                        "backends are experimental: on the Fold 5 neither Vulkan nor " +
                        "OpenCL beat the CPU, and a GPU start that fails falls back " +
                        "to CPU automatically.",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                Spacer(Modifier.height(8.dp))
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    vm.availableBackends.forEach { backend ->
                        FilterChip(
                            selected = ui.edgeBackend == backend,
                            onClick = { vm.setEdgeBackend(backend) },
                            label = {
                                Text(if (backend.isGpu) "${backend.label} (exp)" else backend.label)
                            },
                        )
                    }
                }
                Spacer(Modifier.height(8.dp))
                OutlinedButton(
                    onClick = { vm.showServerLog() },
                    modifier = Modifier.fillMaxWidth(),
                ) { Text("View server log") }
            }
            Spacer(Modifier.height(16.dp))
            Text("Shared personality", style = MaterialTheme.typography.titleMedium)
            Text(
                "A style preference used by both models. It affects tone only and does not " +
                    "replace system instructions, change tools, or alter memory rules. " +
                    "Changes sync when connected; the newest edit is kept.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.height(8.dp))
            OutlinedTextField(
                value = personalityDraft,
                onValueChange = { value ->
                    if (value.codePointCount(0, value.length) <= 2000) personalityDraft = value
                },
                label = { Text("Personality / style") },
                placeholder = { Text("For example: warm, curious, and concise") },
                minLines = 3,
                maxLines = 5,
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.height(8.dp))
            Button(
                onClick = { vm.savePersonality(personalityDraft) },
                modifier = Modifier.fillMaxWidth(),
                enabled = !ui.busy,
            ) { Text("Save personality") }
            ui.personalitySyncStatus?.let { status ->
                Spacer(Modifier.height(4.dp))
                Text(status, style = MaterialTheme.typography.bodySmall)
            }
            Spacer(Modifier.height(8.dp))
            OutlinedButton(
                onClick = { confirmClear = true },
                modifier = Modifier.fillMaxWidth(),
            ) { Text("Clear conversation context") }
            Spacer(Modifier.height(16.dp))
        }
    }

    if (confirmClear) {
        AlertDialog(
            onDismissRequest = { confirmClear = false },
            title = { Text("Clear the conversation?") },
            text = {
                Text(
                    "Deletes this phone's conversation and asks the desktop to delete the " +
                        "turns it stored for this device. Anything still waiting to sync is " +
                        "sent first, so it still reaches the journal. Journal entries are kept.\n\n" +
                        "Use this when earlier bad replies keep showing up in the model's output."
                )
            },
            confirmButton = {
                TextButton(
                    onClick = {
                        confirmClear = false
                        vm.clearContext()
                        vm.hideModelPicker()
                    },
                ) { Text("Clear") }
            },
            dismissButton = {
                TextButton(onClick = { confirmClear = false }) { Text("Cancel") }
            },
        )
    }
}

@Composable
private fun BubbleRow(bubble: ChatViewModel.Bubble) {
    // Long-press copies the message; the toast confirms it because the
    // clipboard write itself is silent.
    val clipboard = LocalClipboardManager.current
    val context = LocalContext.current
    val copyModifier = if (bubble.role != "tool" && bubble.text.isNotBlank() && !bubble.streaming) {
        Modifier.pointerInput(bubble.id) {
            detectTapGestures(onLongPress = {
                clipboard.setText(AnnotatedString(bubble.text))
                Toast.makeText(context, "copied to clipboard", Toast.LENGTH_SHORT).show()
            })
        }
    } else {
        Modifier
    }

    if (bubble.role == "tool") {
        val color = if (bubble.failed) {
            MaterialTheme.colorScheme.error
        } else {
            MaterialTheme.colorScheme.outlineVariant
        }
        Row(
            Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.Start,
        ) {
            Card(
                shape = RoundedCornerShape(50),
                colors = CardDefaults.cardColors(
                    containerColor = MaterialTheme.colorScheme.surfaceVariant,
                ),
                border = BorderStroke(1.dp, color),
            ) {
                Text(
                    bubble.text,
                    modifier = Modifier.padding(horizontal = 12.dp, vertical = 6.dp),
                    style = MaterialTheme.typography.labelMedium,
                    color = if (bubble.failed) MaterialTheme.colorScheme.error
                    else MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        }
        return
    }

    val fromUser = bubble.role == "user"
    Row(
        Modifier.fillMaxWidth(),
        horizontalArrangement = if (fromUser) Arrangement.End else Arrangement.Start,
    ) {
        Card(
            shape = RoundedCornerShape(14.dp),
            colors = CardDefaults.cardColors(
                containerColor = if (fromUser) {
                    MaterialTheme.colorScheme.primary
                } else {
                    MaterialTheme.colorScheme.surfaceVariant
                },
            ),
            modifier = Modifier.widthIn(max = 520.dp).then(copyModifier),
        ) {
            Column(Modifier.padding(12.dp)) {
                if (!fromUser) {
                    Text(
                        bubble.text.ifEmpty { if (bubble.streaming) "..." else "" },
                        style = MaterialTheme.typography.bodyMedium,
                    )
                } else {
                    Text(bubble.text, style = MaterialTheme.typography.bodyMedium)
                }
            }
        }
    }
}
