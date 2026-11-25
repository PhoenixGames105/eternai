package dev.eternai.app.net

import dev.eternai.app.data.Prefs
import dev.eternai.core.DeviceIdentity
import java.util.concurrent.TimeUnit
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import javax.net.ssl.SSLContext
import javax.net.ssl.X509TrustManager

/** A gateway call failed in a way the caller should surface or retry. */
class GatewayException(
    val status: Int = 0,
    override val message: String,
    cause: Throwable? = null,
) : RuntimeException(message, cause)

/**
 * HTTP client for the desktop gateway.
 *
 * Every request is HTTPS with the pinned cert, so a single client instance per
 * pin is enough. Rebuilding it on re-pair is intentional: the pin is baked into
 * the trust manager, not chosen per call, which removes the "forgot to pin"
 * failure mode entirely.
 */
class GatewayClient(private val prefs: Prefs) {

    @Volatile
    private var cached: Pair<String, OkHttpClient>? = null

    private val json = "application/json; charset=utf-8".toMediaType()

    private fun client(): Pair<String, OkHttpClient> {
        val pin = prefs.certPin?.takeIf { it.isNotBlank() }
            ?: throw GatewayException(message = "not paired: no certificate pin")
        cached?.let { if (it.first == pin) return it }

        val trust = PinnedTrustManager(pin)
        val sslContext = SSLContext.getInstance("TLS").apply {
            init(null, arrayOf<X509TrustManager>(trust), null)
        }
        val built = OkHttpClient.Builder()
            .sslSocketFactory(sslContext.socketFactory, trust)
            .hostnameVerifier(PinningHostnameVerifier)
            .connectTimeout(4, TimeUnit.SECONDS)
            .readTimeout(180, TimeUnit.SECONDS)
            .writeTimeout(30, TimeUnit.SECONDS)
            // A dead gateway should fail fast; the probe drives failover.
            .retryOnConnectionFailure(false)
            .build()
        cached = pin to built
        return cached!!
    }

    private fun baseUrl(): String {
        val host = prefs.host?.takeIf { it.isNotBlank() }
            ?: throw GatewayException(message = "not paired: no gateway host")
        return "https://$host:${prefs.port}"
    }

    /** Drops the cached client so a new pin takes effect immediately. */
    fun invalidate() {
        cached = null
    }

    /**
     * `GET /health`. Pairing uses this before any credential exists, hence the
     * explicit target instead of prefs.
     */
    fun health(host: String, port: Long, pin: String): String {
        val trust = PinnedTrustManager(pin)
        val sslContext = SSLContext.getInstance("TLS").apply {
            init(null, arrayOf<X509TrustManager>(trust), null)
        }
        val bootstrap = OkHttpClient.Builder()
            .sslSocketFactory(sslContext.socketFactory, trust)
            .hostnameVerifier(PinningHostnameVerifier)
            .connectTimeout(3, TimeUnit.SECONDS)
            .readTimeout(5, TimeUnit.SECONDS)
            .retryOnConnectionFailure(false)
            .build()
        return execute(bootstrap, Request.Builder().url("https://$host:$port/health").build())
    }

    /** Liveness check for the probe: cheap, and must not need a device token. */
    fun probe(): String {
        val (pin, c) = client()
        val body = execute(c, Request.Builder().url("${baseUrl()}/health").build())
        // A cert swap mid-session must not keep working on a stale pin.
        if (prefs.certPin != pin) invalidate()
        return body
    }

    /** Unauthenticated: the pair code is the credential. */
    fun pair(target: dev.eternai.core.PairTarget, bodyJson: String): String {
        val trust = PinnedTrustManager(target.certPin)
        val sslContext = SSLContext.getInstance("TLS").apply {
            init(null, arrayOf<X509TrustManager>(trust), null)
        }
        val bootstrap = OkHttpClient.Builder()
            .sslSocketFactory(sslContext.socketFactory, trust)
            .hostnameVerifier(PinningHostnameVerifier)
            .connectTimeout(5, TimeUnit.SECONDS)
            .readTimeout(15, TimeUnit.SECONDS)
            .retryOnConnectionFailure(false)
            .build()
        return execute(
            bootstrap,
            Request.Builder()
                .url("https://${target.host}:${target.port}/etern/pair")
                .post(bodyJson.toRequestBody(json))
                .build(),
        )
    }

    fun get(pathWithQuery: String, device: DeviceIdentity): String {
        val (_, c) = client()
        return execute(
            c,
            Request.Builder()
                .url(baseUrl() + pathWithQuery)
                .header("authorization", "Bearer ${device.token}")
                .build(),
        )
    }

    fun post(path: String, device: DeviceIdentity, bodyJson: String): String {
        val (_, c) = client()
        return execute(
            c,
            Request.Builder()
                .url(baseUrl() + path)
                .header("authorization", "Bearer ${device.token}")
                .post(bodyJson.toRequestBody(json))
                .build(),
        )
    }

    /**
     * The tool definitions the desktop advertises for proxy execution.
     *
     * These are the *desktop* set because an online phone is talking to the
     * high-power model, and the calls it makes are executed by the desktop's
     * runtime against the desktop jails. A phone that is offline never asks:
     * it builds the reduced edge set locally.
     */
    fun getTools(device: DeviceIdentity): String = get("/etern/tools", device)

    /**
     * Runs tool calls on the desktop's runtime.
     *
     * Per-call failures come back as HTTP 200 with `{"ok":false,...}` entries so
     * the harness can feed the error to the model for its single retry; only a
     * malformed request raises [GatewayException].
     */
    fun executeToolCalls(device: DeviceIdentity, bodyJson: String): String =
        post("/etern/tools/execute", device, bodyJson)

    /**
     * Streaming chat completion. Returns the raw SSE body as a cold flow of
     * `data:` payloads; the caller assembles deltas.
     */
    suspend fun chatStream(
        device: DeviceIdentity,
        bodyJson: String,
        onDelta: (String) -> Unit,
    ): Boolean {
        val (_, c) = client()
        val request = Request.Builder()
            .url(baseUrl() + "/v1/chat/completions")
            .header("authorization", "Bearer ${device.token}")
            .header("accept", "text/event-stream")
            .post(bodyJson.toRequestBody(json))
            .build()

        c.newCall(request).execute().use { resp ->
            if (!resp.isSuccessful) {
                throw GatewayException(resp.code, "chat failed: HTTP ${resp.code}")
            }
            val source = resp.body?.source()
                ?: throw GatewayException(message = "chat returned no body")
            var done = false
            while (!done) {
                val line = source.readUtf8Line() ?: break
                if (!line.startsWith("data:")) continue
                val payload = line.substring(5).trim()
                if (payload.isEmpty() || payload == "[DONE]") {
                    done = payload == "[DONE]"
                    continue
                }
                onDelta(payload)
            }
            return true
        }
    }

    private fun execute(client: OkHttpClient, request: Request): String {
        try {
            client.newCall(request).execute().use { resp ->
                val body = resp.body?.string().orEmpty()
                if (!resp.isSuccessful) {
                    throw GatewayException(resp.code, "HTTP ${resp.code}: ${body.take(300)}")
                }
                return body
            }
        } catch (e: GatewayException) {
            throw e
        } catch (e: java.io.IOException) {
            throw GatewayException(message = "network: ${e.message}", cause = e)
        } catch (e: javax.net.ssl.SSLException) {
            throw GatewayException(message = "tls: ${e.message}", cause = e)
        }
    }
}
