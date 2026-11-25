package dev.eternai.app

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.lifecycle.viewmodel.compose.viewModel
import dev.eternai.app.ui.ChatScreen
import dev.eternai.app.ui.EternAITheme
import dev.eternai.app.viewmodel.ChatViewModel

class MainActivity : ComponentActivity() {

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Services start from EternApplication (cold start while paired) and from
        // PairingClient (fresh pair); starting them here too would probe forever
        // on an unpaired install.
        setContent {
            EternAITheme {
                val vm: ChatViewModel = viewModel(key = "chat")
                ChatScreen(vm)
            }
        }
    }
}
