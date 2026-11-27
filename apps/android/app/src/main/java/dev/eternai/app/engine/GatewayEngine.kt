package dev.eternai.app.engine

import android.util.Log
import dev.eternai.app.EternApplication
import dev.eternai.app.net.GatewayException
import dev.eternai.app.tools.ToolCallAccumulator
import dev.eternai.app.util.optStringOrNull
import dev.eternai.core.MessageDto
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject
import kotlin.coroutines.coroutineContext

/**
 * Streams from the desktop's high-power model through the gateway.
 *
 * The request body is assembled here rather than in Rust: it is a plain OpenAI-shaped
 * object, and the gateway already owns the context budget for the 200k model — the phone
 * only sends the messages it was given.
 */
class GatewayEngine(private val app: EternApplication) : Engine {

    override val name: String = "high_power"
    override val model: String
        get() = app.probe.state.value.health?.model.orEmpty()

    override suspend fun generate(
        messages: List<MessageDto>,
        tools: JSONArray?,
        onChunk: (Chunk) -> Unit,
    ): Reply = withContext(Dispatchers.IO) {
        val device = app.prefs.device
            ?: return@withContext Reply("", Finish.Error, name, "", "not paired")

        val body = JSONObject()
            .put("model", model.ifEmpty { "etern" })
            .put("stream", true)
            .put("messages", toWire(messages))
        tools?.let { body.put("tools", it) }

        val text = StringBuilder()
        val calls = ToolCallAccumulator()
        try {
            app.gateway.chatStream(device, body.toString()) { payload ->
                val choice = JSONObject(payload).optJSONArray("choices")?.optJSONObject(0)
                val delta = choice?.optJSONObject("delta") ?: return@chatStream
                delta.optStringOrNull("content")?.takeIf { it.isNotEmpty() }?.let {
                    text.append(it)
                    onChunk(Chunk(it))
                }
                calls.absorb(delta)
            }
            coroutineContext.ensureActive()
            Reply(text.toString(), Finish.Stop, name, model, toolCalls = calls.result())
        } catch (e: GatewayException) {
            Log.w(TAG, "gateway chat failed: ${e.message}")
            Reply(text.toString(), Finish.Error, name, model, "gateway ${e.status}: ${e.message}")
        } catch (e: java.io.IOException) {
            // Hanging the call up in cancel() surfaces here too, worded
            // differently per HTTP protocol version, so trust the latch.
            if (app.gateway.cancelledByUser) {
                Reply(text.toString(), Finish.Cancelled, name, model)
            } else {
                Log.w(TAG, "gateway stream broke: ${e.message}")
                Reply(text.toString(), Finish.Error, name, model, "network: ${e.message}")
            }
        } catch (e: org.json.JSONException) {
            Log.w(TAG, "gateway sent a non-JSON chunk: ${e.message}")
            Reply(text.toString(), Finish.Error, name, model, "malformed stream: ${e.message}")
        }
    }

    /**
     * Drops the gateway stream. The gateway's own decode continues to
     * completion, but nothing more reaches the phone and the turn ends here.
     */
    override fun cancel() {
        app.gateway.cancelActive()
    }

    private fun toWire(messages: List<MessageDto>): JSONArray {
        val arr = JSONArray()
        for (m in messages) {
            val o = JSONObject().put("role", m.role).put("content", m.content)
            m.name?.let { o.put("name", it) }
            // Tool fields stay raw JSON strings so they replay byte-for-byte.
            m.toolCalls?.let { o.put("tool_calls", JSONArray(it)) }
            m.toolCallId?.let { o.put("tool_call_id", it) }
            arr.put(o)
        }
        return arr
    }

    private companion object {
        const val TAG = "EternGatewayEngine"
    }
}

