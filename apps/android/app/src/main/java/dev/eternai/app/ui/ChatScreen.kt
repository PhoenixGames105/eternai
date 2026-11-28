package dev.eternai.app.ui

import android.Manifest
import android.content.pm.PackageManager
import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.filled.MoreVert
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.ime
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.gestures.scrollBy
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListState
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
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
import androidx.compose.runtime.snapshotFlow
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalContext
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.Canvas
import androidx.compose.material3.IconButtonDefaults
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.material3.LocalContentColor
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
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

    // The Fold's cover display is ~400dp wide while the inner one is ~690dp, so
    // the bar and the input row have to fall back to a stacked layout below the
    // point where a single row stops fitting.
    val wide = LocalConfiguration.current.screenWidthDp >= 520

    Scaffold(
        topBar = {
            // The cover display is ~400dp wide: the status has to ellipsize and the
            // secondary actions collapse into an overflow menu, otherwise the title
            // and chips squeeze each other into multiple lines.
            TopAppBar(
                title = {
                    // The title slot is not a RowScope, so it cannot be weighted;
                    // ellipsizing alone keeps a long label from pushing the actions
                    // off the end of a cover-display bar.
                    Text(
                        "eternAI",
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                    )
                },
                actions = {
                    StatusChip(ui, wide)
                    if (wide) {
                        TextButton(onClick = { vm.showModelPicker() }, enabled = !ui.busy) {
                            Text("Model")
                        }
                        TextButton(onClick = { vm.syncNow() }, enabled = ui.paired && !ui.busy) {
                            Text("Sync")
                        }
                    } else {
                        OverflowMenu(
                            modelEnabled = !ui.busy,
                            syncEnabled = ui.paired && !ui.busy,
                            onModel = { vm.showModelPicker() },
                            onSync = { vm.syncNow() },
                        )
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
            // The turn's tool group is seeded before the first token so the list never
            // grows mid-turn, which means the bubble that actually grows is the reply
            // itself — that is what auto-scroll has to follow, not the last row.
            val streaming = bubbles.lastOrNull { it.streaming }
            StickToBottom(
                listState = listState,
                bubbleCount = bubbles.size,
                lastTextLength = streaming?.text?.length ?: 0,
                busy = ui.busy,
                keyboardHeight = keyboardHeight,
            )

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
                        // While a turn streams, its user and assistant bubbles both
                        // carry the bare turn id, so the role is part of the key.
                        items(bubbles, key = { it.id + it.role }) { bubble ->
                            BubbleRow(bubble, onDelete = { vm.deleteBubble(bubble.id) })
                        }
                        // The turn's tool group is seeded before the first token so the
                        // list does not grow mid-turn; only a group with steps, or a
                        // reply that has not started, counts as the tail of the turn.
                        val tail = bubbles.lastOrNull { it.role != "tools" || it.steps.isNotEmpty() }
                        if (ui.busy && tail?.role != "assistant") {
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

            // On the cover display a single row leaves the text field so narrow
            // that even its label wraps to three lines, so the voice toggles move
            // to a second row and the field keeps the full width.
            if (wide) {
                Row(
                    Modifier
                        .fillMaxWidth()
                        .padding(top = 8.dp, bottom = 12.dp),
                    verticalAlignment = Alignment.Bottom,
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    VoiceControls(
                        ui,
                        listening = ui.listening,
                        onMic = { onMic() },
                        onToggleSpeak = { vm.toggleSpeakReplies() },
                    )
                    OutlinedTextField(
                        value = prompt,
                        onValueChange = { prompt = it },
                        label = { PromptLabel(ui) },
                        maxLines = 5,
                        modifier = Modifier
                            .weight(1f)
                            .heightIn(max = 160.dp),
                    )
                    SendButton(
                        busy = ui.busy,
                        enabled = prompt.isNotBlank(),
                        onSend = {
                            vm.send(prompt)
                            prompt = ""
                        },
                        onStop = { vm.stop() },
                    )
                }
            } else {
                Column(
                    Modifier
                        .fillMaxWidth()
                        .padding(top = 8.dp, bottom = 12.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) {
                    Row(
                        Modifier.fillMaxWidth(),
                        verticalAlignment = Alignment.Bottom,
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                    ) {
                        OutlinedTextField(
                            value = prompt,
                            onValueChange = { prompt = it },
                            label = { PromptLabel(ui) },
                            maxLines = 5,
                            modifier = Modifier
                                .weight(1f)
                                .heightIn(max = 160.dp),
                        )
                        SendButton(
                            busy = ui.busy,
                            enabled = prompt.isNotBlank(),
                            onSend = {
                                vm.send(prompt)
                                prompt = ""
                            },
                            onStop = { vm.stop() },
                        )
                    }
                    Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                        VoiceControls(
                            ui,
                            listening = ui.listening,
                            onMic = { onMic() },
                            onToggleSpeak = { vm.toggleSpeakReplies() },
                        )
                    }
                }
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

/**
 * Overflow entry point for the secondary actions on a narrow screen.
 *
 * "Model" and "Sync" are used rarely enough that a menu is cheaper than the
 * ~160dp two text buttons would cost on the cover display.
 */
@Composable
private fun OverflowMenu(
    modelEnabled: Boolean,
    syncEnabled: Boolean,
    onModel: () -> Unit,
    onSync: () -> Unit,
) {
    var open by remember { mutableStateOf(false) }
    Box {
        IconButton(onClick = { open = true }) {
            Icon(Icons.Filled.MoreVert, contentDescription = "More actions")
        }
        DropdownMenu(expanded = open, onDismissRequest = { open = false }) {
            DropdownMenuItem(
                text = { Text("Model") },
                enabled = modelEnabled,
                onClick = {
                    open = false
                    onModel()
                },
            )
            DropdownMenuItem(
                text = { Text("Sync now") },
                enabled = syncEnabled,
                onClick = {
                    open = false
                    onSync()
                },
            )
        }
    }
}

/**
 * Placeholder for the prompt field, kept to a single line so a narrow screen
 * cannot stack it into three lines and shrink the field itself.
 */
@Composable
private fun PromptLabel(ui: ChatViewModel.UiState) {
    Text(
        when {
            ui.listening -> "Listening..."
            ui.online -> "Ask the desktop"
            else -> "Ask the edge model"
        },
        maxLines = 1,
        overflow = TextOverflow.Ellipsis,
    )
}

/** Dictation and read-aloud toggles; grouped so they can share a row. */
@Composable
private fun VoiceControls(
    ui: ChatViewModel.UiState,
    listening: Boolean,
    onMic: () -> Unit,
    onToggleSpeak: () -> Unit,
) {
    val compact = PaddingValues(horizontal = 12.dp, vertical = 6.dp)
    if (ui.voiceAvailable) {
        OutlinedButton(
            onClick = onMic,
            enabled = !ui.busy || listening,
            contentPadding = compact,
        ) {
            Text(if (listening) "Listening" else "Mic", maxLines = 1)
        }
    }
    OutlinedButton(
        onClick = onToggleSpeak,
        contentPadding = compact,
    ) {
        Text(if (ui.speakReplies) "Speak on" else "Speak off", maxLines = 1)
    }
}

/**
 * Icon-only send control, which becomes the stop control while a turn runs.
 *
 * A text button costs ~90dp of the ~400dp cover display, which is what left the
 * prompt field unusable, so the stop affordance reuses the same footprint. The
 * glyph is drawn rather than taken from `material-icons` because the stop icon
 * lives in the extended artifact this build does not carry.
 */
@Composable
private fun SendButton(busy: Boolean, enabled: Boolean, onSend: () -> Unit, onStop: () -> Unit) {
    if (busy) {
        FilledIconButton(
            onClick = onStop,
            colors = IconButtonDefaults.filledIconButtonColors(
                containerColor = MaterialTheme.colorScheme.errorContainer,
                contentColor = MaterialTheme.colorScheme.onErrorContainer,
            ),
        ) {
            val tint = LocalContentColor.current
            Canvas(
                Modifier
                    .size(18.dp)
                    .semantics { contentDescription = "Stop generating" },
            ) {
                val side = size.minDimension * 0.62f
                drawRoundRect(
                    color = tint,
                    topLeft = Offset((size.width - side) / 2f, (size.height - side) / 2f),
                    size = Size(side, side),
                    cornerRadius = CornerRadius(side * 0.18f),
                )
            }
        }
        return
    }
    FilledIconButton(onClick = onSend, enabled = enabled) {
        Icon(Icons.AutoMirrored.Filled.Send, contentDescription = "Send")
    }
}

/**
 * Follows the conversation only while the user is already at the bottom.
 *
 * The previous behaviour re-scrolled on every streamed token, which fought any
 * attempt to read a long reply while it was still being written. Scrolling up
 * (or dragging up) detaches, and returning to the bottom re-attaches.
 */
@Composable
private fun StickToBottom(
    listState: LazyListState,
    bubbleCount: Int,
    lastTextLength: Int,
    busy: Boolean,
    keyboardHeight: Dp,
) {
    var following by remember { mutableStateOf(true) }
    var lastCount by remember { mutableStateOf(bubbleCount) }

    // A bubble taller than the viewport can never report itself as fully
    // visible, so "at bottom" means the last item is the last one laid out and
    // nothing further can be scrolled to.
    fun atBottom(): Boolean {
        val info = listState.layoutInfo
        val last = info.visibleItemsInfo.lastOrNull() ?: return true
        return last.index >= info.totalItemsCount - 1 && !listState.canScrollForward
    }

    // Sending a message always re-attaches, even if the user had scrolled up to
    // re-read something older.
    LaunchedEffect(bubbleCount) {
        if (bubbleCount > lastCount) following = true
        lastCount = bubbleCount
    }

    // Direction is sampled from the scroll position itself rather than from the
    // settled drag state: waiting for the gesture to end would let a token that
    // arrives in between yank the view back down over the user's finger.
    LaunchedEffect(listState) {
        var previous: Pair<Int, Int>? = null
        snapshotFlow {
            listState.firstVisibleItemIndex to listState.firstVisibleItemScrollOffset
        }.collect { current ->
            val before = previous
            previous = current
            val scrolledUp = before != null &&
                (current.first < before.first ||
                    (current.first == before.first && current.second < before.second))
            following = if (scrolledUp) false else atBottom() || following
        }
    }

    LaunchedEffect(bubbleCount, lastTextLength, busy, keyboardHeight) {
        if (bubbleCount == 0 || !following) return@LaunchedEffect
        if (listState.isScrollInProgress) return@LaunchedEffect
        // A plain scrollBy (not animateScrollToItem) keeps up with tokens that
        // arrive faster than an animation frame and never jumps over a bubble.
        listState.scrollBy(100_000f)
    }
}

@Composable
private fun StatusChip(ui: ChatViewModel.UiState, wide: Boolean) {
    val (label, tint) = when {
        !ui.paired -> "not paired" to MaterialTheme.colorScheme.outline
        ui.online -> "desktop" to MaterialTheme.colorScheme.primary
        else -> "edge" to MaterialTheme.colorScheme.tertiary
    }
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .clip(CircleShape)
            .background(MaterialTheme.colorScheme.surfaceVariant)
            .padding(horizontal = 10.dp, vertical = 6.dp),
    ) {
        Box(
            Modifier
                .size(8.dp)
                .clip(CircleShape)
                .background(tint),
        )
        Spacer(Modifier.width(6.dp))
        Text(
            label,
            style = MaterialTheme.typography.labelSmall,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
        // The engine/model name is only useful once it is actually known, and
        // only affordable on a screen wide enough to show it.
        if (wide && ui.online && ui.model.isNotBlank()) {
            Text(
                " · " + ui.model,
                style = MaterialTheme.typography.labelSmall,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.widthIn(max = 220.dp),
            )
        }
    }
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

/** Everything in a tool group, as the clipboard sees it. */
private fun toolGroupText(steps: List<ChatViewModel.ToolStep>): String =
    steps.joinToString("\n") { step ->
        if (step.detail.isEmpty()) step.label else step.label + ": " + step.detail
    }

/**
 * A run of tool calls folded into one bubble.
 *
 * Ten chips down the side of the cover display used to push the reply itself off
 * screen, so the group stays a single line until it is tapped open. The header
 * keeps the count, any failures, and the most recent step, which is all the
 * feedback a streaming turn needs.
 */
@Composable
private fun ToolGroupCard(bubble: ChatViewModel.Bubble, onLongPress: () -> Unit) {
    var expanded by rememberSaveable(bubble.id) { mutableStateOf(false) }
    val count = bubble.steps.size
    val failed = bubble.steps.count { it.failed }
    val header = buildString {
        append(if (count == 1) "1 tool call" else "$count tool calls")
        append(
            when (failed) {
                0 -> ""
                1 -> " · 1 failed"
                else -> " · $failed failed"
            },
        )
        if (!expanded) bubble.steps.lastOrNull()?.let { append(" · " + it.label) }
    }

    Card(
        shape = RoundedCornerShape(14.dp),
        colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.surfaceVariant),
        border = BorderStroke(
            1.dp,
            if (failed > 0) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.outlineVariant,
        ),
        modifier = Modifier
            .widthIn(max = 520.dp)
            .pointerInput(bubble.id) {
                // One detector owns both gestures; a `clickable` child would fight
                // the long-press that opens the copy/delete menu.
                detectTapGestures(
                    onTap = { expanded = !expanded },
                    onLongPress = { onLongPress() },
                )
            },
    ) {
        Column(Modifier.padding(horizontal = 12.dp, vertical = 8.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    header,
                    style = MaterialTheme.typography.labelLarge,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    color = if (failed > 0) MaterialTheme.colorScheme.error
                    else MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.weight(1f),
                )
                Icon(
                    if (expanded) Icons.Filled.KeyboardArrowUp else Icons.Filled.KeyboardArrowDown,
                    contentDescription = if (expanded) "Collapse tool calls" else "Expand tool calls",
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            if (expanded) {
                bubble.steps.forEachIndexed { index, step ->
                    Column(Modifier.padding(top = if (index == 0) 10.dp else 8.dp)) {
                        Text(
                            step.label,
                            style = MaterialTheme.typography.labelMedium,
                            color = if (step.failed) MaterialTheme.colorScheme.error
                            else MaterialTheme.colorScheme.onSurface,
                        )
                        if (step.detail.isNotEmpty()) {
                            Text(
                                step.detail,
                                style = MaterialTheme.typography.bodySmall
                                    .copy(fontFamily = FontFamily.Monospace),
                                maxLines = 6,
                                overflow = TextOverflow.Ellipsis,
                                color = MaterialTheme.colorScheme.onSurfaceVariant,
                            )
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun BubbleRow(bubble: ChatViewModel.Bubble, onDelete: () -> Unit) {
    // Long-press opens a small menu. Copy alone used to be the whole gesture, but
    // deleting a message the model keeps re-reading needs an escape hatch that is
    // not buried in Settings.
    val clipboard = LocalClipboardManager.current
    val context = LocalContext.current
    var menuOpen by remember(bubble.id) { mutableStateOf(false) }
    var confirmDelete by remember(bubble.id) { mutableStateOf(false) }
    val deletable = !bubble.streaming
    val copyText = if (bubble.role == "tools") toolGroupText(bubble.steps) else bubble.text
    val copyable = copyText.isNotBlank() && deletable
    val menuModifier = if (deletable) {
        Modifier.pointerInput(bubble.id) {
            detectTapGestures(onLongPress = { menuOpen = true })
        }
    } else {
        Modifier
    }

    val fromUser = bubble.role == "user"
    Row(
        Modifier.fillMaxWidth(),
        horizontalArrangement = if (fromUser) Arrangement.End else Arrangement.Start,
    ) {
        // The menu is anchored to the bubble itself so it drops where the finger
        // was, rather than at the far edge of the row.
        Box {
            if (bubble.role == "tools") {
                ToolGroupCard(bubble, onLongPress = { if (deletable) menuOpen = true })
            } else {
                Card(
                    shape = RoundedCornerShape(14.dp),
                    colors = CardDefaults.cardColors(
                        containerColor = if (fromUser) {
                            MaterialTheme.colorScheme.primary
                        } else {
                            MaterialTheme.colorScheme.surfaceVariant
                        },
                    ),
                    modifier = Modifier.widthIn(max = 520.dp).then(menuModifier),
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
            DropdownMenu(expanded = menuOpen, onDismissRequest = { menuOpen = false }) {
                DropdownMenuItem(
                    text = { Text("Copy") },
                    enabled = copyable,
                    onClick = {
                        menuOpen = false
                        clipboard.setText(AnnotatedString(copyText))
                        Toast.makeText(context, "copied to clipboard", Toast.LENGTH_SHORT).show()
                    },
                )
                DropdownMenuItem(
                    text = { Text("Delete") },
                    onClick = {
                        menuOpen = false
                        confirmDelete = true
                    },
                )
            }
        }
    }

    if (confirmDelete) {
        // Deleting takes the whole exchange, not just this bubble, because the
        // store and the sync protocol both key on turns — say so before it goes.
        AlertDialog(
            onDismissRequest = { confirmDelete = false },
            title = { Text("Delete this exchange?") },
            text = {
                Text(
                    "Removes this message together with its reply and any tool steps, " +
                        "here and on the desktop. The desktop is told immediately, or " +
                        "as soon as you reconnect if it is offline."
                )
            },
            confirmButton = {
                Button(
                    onClick = {
                        confirmDelete = false
                        onDelete()
                    },
                ) { Text("Delete") }
            },
            dismissButton = {
                TextButton(onClick = { confirmDelete = false }) { Text("Cancel") }
            },
        )
    }
}
