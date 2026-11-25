package dev.eternai.app.ui

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import dev.eternai.app.viewmodel.ChatViewModel

/**
 * Pairing flow: scan (or paste) the QR payload from the desktop, then redeem the code.
 *
 * The scan uses zxing's ScanContract, which runs in its own activity, so it works from a
 * plain ComponentActivity host without a fragment.
 */
@Composable
fun PairingSheet(vm: ChatViewModel, deviceName: String, onDeviceNameChange: (String) -> Unit) {
    val ui by vm.ui.collectAsStateWithLifecycle()
    var manual by remember { mutableStateOf("") }

    val scanner = rememberLauncherForActivityResult(ScanContract()) { result ->
        // ScanContract decodes the activity result into a ScanIntentResult, so the
        // raw Intent is already gone by the time this callback runs.
        val text = result?.contents
        if (text.isNullOrBlank()) {
            vm.consumeNotice()
        } else {
            vm.onQrScanned(text)
        }
    }

    val options = remember {
        ScanOptions().apply {
            setDesiredBarcodeFormats(ScanOptions.QR_CODE)
            setPrompt("Point at the pairing QR code in the eternAI desktop app")
            setBeepEnabled(false)
            setOrientationLocked(true)
        }
    }

    Card(Modifier.fillMaxWidth()) {
        Column(
            Modifier
                .fillMaxWidth()
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Text("Pair with your desktop", style = MaterialTheme.typography.titleMedium)
            Text(
                "Open eternAI on your PC, go to Settings, and press \"Generate pair code\". The " +
                    "code is single-use and expires in 10 minutes, so scan it right away. The PC " +
                    "and this phone must be on the same network.",
                style = MaterialTheme.typography.bodySmall,
            )

            Button(
                onClick = { scanner.launch(options) },
                modifier = Modifier.fillMaxWidth(),
            ) { Text("Scan QR code") }

            HorizontalDivider()

            OutlinedTextField(
                value = manual,
                onValueChange = { manual = it },
                label = { Text("...or paste the pairing payload") },
                minLines = 2,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedButton(
                onClick = { vm.onQrScanned(manual) },
                enabled = manual.isNotBlank(),
                modifier = Modifier.fillMaxWidth(),
            ) { Text("Read payload") }

            ui.pairTarget?.let { target ->
                HorizontalDivider()
                Text(
                    "Connect to " + target.host + ":" + target.port + " ?",
                    style = MaterialTheme.typography.bodyMedium,
                )
                Text(
                    "The certificate pin " + target.certPin.take(12) + "... is checked on every " +
                        "connection after this.",
                    style = MaterialTheme.typography.bodySmall,
                )
                OutlinedTextField(
                    value = deviceName,
                    onValueChange = onDeviceNameChange,
                    label = { Text("Name this device") },
                    singleLine = true,
                    modifier = Modifier.fillMaxWidth(),
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    Button(
                        onClick = { vm.confirmPairing(deviceName) },
                        enabled = !ui.busy,
                    ) { Text(if (ui.busy) "Pairing..." else "Pair") }
                    TextButton(onClick = { vm.cancelPairing() }, enabled = !ui.busy) {
                        Text("Cancel")
                    }
                }
            }

            ui.notice?.let {
                Spacer(Modifier.height(4.dp))
                Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.error)
            }
        }
    }
}
