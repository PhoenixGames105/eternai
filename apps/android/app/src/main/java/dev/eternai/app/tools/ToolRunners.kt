package dev.eternai.app.tools

import android.content.Context
import android.util.Log
import dev.eternai.app.EternApplication
import dev.eternai.core.EdgeTools
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import org.json.JSONArray
import org.json.JSONObject

/**
 * Runs the reduced edge tool set on the phone.
 *
 * Offline is precisely when the edge model is used, so the tools cannot be
 * proxied to the desktop: PLAN §6 keeps `web_search` off the edge profile for
 * exactly this reason. The runtime itself is `etern-core`'s, reached through
 * UniFFI, so the sandbox rules and the result shapes are the same code the
 * desktop runs and cannot drift from it.
 */
class EdgeToolRunner(context: Context) : ToolRunner {

    override val label: String = "edge"

    private val tools: EdgeTools? = runCatching {
        EdgeTools(context.filesDir.absolutePath)
    }.onFailure { e ->
        // A missing native library must not take the chat down with it.
        Log.e(TAG, "edge tool runtime unavailable", e)
    }.getOrNull()

    override suspend fun definitions(): JSONArray? = withContext(Dispatchers.IO) {
        runCatching { JSONArray(tools?.definitions() ?: "[]") }
            .onFailure { Log.w(TAG, "edge tool definitions unreadable: ${it.message}") }
            .getOrNull()
    }

    override suspend fun execute(calls: List<ToolCall>): List<ToolResult> =
        withContext(Dispatchers.IO) {
            val runtime = tools
                ?: return@withContext calls.map { unavailable(it) }
            val now = System.currentTimeMillis()
            calls.map { call ->
                // `execute` returns the gateway's result envelope rather than
                // raising, so a bad argument reaches the model as a retryable
                // error instead of an exception.
                val outcome = runCatching {
                    JSONObject(
                        runtime.execute(
                            name = call.name,
                            argumentsJson = call.arguments,
                            model = "edge",
                            turnId = turnId,
                            nowMs = now,
                        ),
                    )
                }
                val obj = outcome.getOrElse { e ->
                    Log.w(TAG, "edge tool ${call.name} crashed: ${e.message}")
                    JSONObject()
                }
                ToolResult(
                    id = call.id,
                    name = call.name,
                    ok = obj.optBoolean("ok", false),
                    retryable = obj.optBoolean("retryable", false),
                    payload = if (obj.has("content")) {
                        obj.get("content").toString()
                    } else {
                        obj.optJSONObject("error")?.toString()
                            ?: JSONObject().put("kind", "runtime").put("message", "tool runtime failed").toString()
                    },
                )
            }
        }

    /** Set per turn so `remember` lines carry the same provenance as the desktop's. */
    @Volatile
    override var turnId: String = "unknown"

    private fun unavailable(call: ToolCall) = ToolResult(
        id = call.id,
        name = call.name,
        ok = false,
        retryable = false,
        payload = JSONObject()
            .put("kind", if (call.name.isEmpty()) "unknowntool" else "runtime")
            .put("message", "edge tool runtime is not loaded")
            .toString(),
    )

    private companion object {
        const val TAG = "EternEdgeTools"
    }
}

/**
 * Runs tool calls on the desktop through the gateway.
 *
 * Used when the phone is online: the high-power model then has the full desktop
 * set available, and the files it touches are the desktop's.
 */
class GatewayToolRunner(
    private val app: dev.eternai.app.EternApplication,
) : ToolRunner {

    override val label: String = "gateway"

    @Volatile
    override var turnId: String = "unknown"

    override suspend fun definitions(): JSONArray? = withContext(Dispatchers.IO) {
        val device = app.prefs.device ?: return@withContext null
        runCatching { JSONArray(app.gateway.getTools(device)) }
            .onFailure { Log.w(TAG, "could not read tool definitions: ${it.message}") }
            .getOrNull()
    }

    override suspend fun execute(calls: List<ToolCall>): List<ToolResult> =
        withContext(Dispatchers.IO) {
            val device = app.prefs.device
                ?: return@withContext calls.map {
                    ToolResult(it.id, it.name, false, false, JSONObject().put("kind", "runtime").put("message", "not paired").toString())
                }
            val body = JSONObject()
                .put("model", app.probe.state.value.health?.model ?: "unknown")
                .put("turn_id", turnId)
                .put(
                    "calls",
                    JSONArray().apply {
                        calls.forEach { put(JSONObject().put("name", it.name).put("arguments", JSONObject(it.arguments)).put("id", it.id)) }
                    },
                )
            runCatching { app.gateway.executeToolCalls(device, body.toString()) }
                .map { parseToolResults(JSONObject(it), calls) }
                .getOrElse { e ->
                    Log.w(TAG, "gateway tool execution failed: ${e.message}")
                    calls.map {
                        ToolResult(
                            it.id,
                            it.name,
                            ok = false,
                            retryable = true,
                            payload = JSONObject().put("kind", "network").put("message", "gateway unreachable").toString(),
                        )
                    }
                }
        }

    private companion object {
        const val TAG = "EternGatewayTools"
    }
}
