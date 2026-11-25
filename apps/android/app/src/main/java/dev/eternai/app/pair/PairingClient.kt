package dev.eternai.app.pair

import android.util.Log
import dev.eternai.app.EternApplication
import dev.eternai.app.net.GatewayException
import dev.eternai.core.EternException
import dev.eternai.core.PairTarget
import dev.eternai.core.parsePairedDevice
import dev.eternai.core.parseQrPayload
import dev.eternai.core.pairRequestJson

/**
 * Turns a scanned QR into a stored device credential.
 *
 * The order matters: health first, so a mistyped address or a replaced cert fails with a
 * clear message instead of a hung pairing request; then the redeem, which is the only
 * unauthenticated call the gateway accepts. The pin from the QR is stored *before* the
 * device token, because a client without the pin cannot be built and a token without a
 * pin would be useless.
 */
class PairingClient(private val app: EternApplication) {

    private val prefs = app.prefs
    private val gateway = app.gateway

    sealed class Result {
        /** QR parsed OK; the UI shows [target] and asks the owner to confirm. */
        data class Ready(val target: PairTarget) : Result()

        /** Paired; [name] is what the desktop now shows in its device list. */
        data class Success(val name: String, val host: String) : Result()

        data class Failure(val message: String) : Result()
    }

    /** Parses QR text. Never throws — a wrong QR is an ordinary UI state. */
    fun parse(qrText: String): Result =
        try {
            Result.Ready(parseQrPayload(qrText))
        } catch (e: EternException) {
            Result.Failure("not an eternAI pairing code: ${e.message}")
        }

    /** Redeems a scanned code. Blocking; call on Dispatchers.IO. */
    fun redeem(target: PairTarget, deviceName: String): Result {
        // 1. Prove the address is the right gateway before burning the single-use code.
        try {
            gateway.health(target.host, target.port, target.certPin)
        } catch (e: Exception) {
            return Result.Failure("cannot reach ${target.host}:${target.port}: ${e.message}")
        }

        // 2. Build the body in Rust so the field names can't drift from the gateway's.
        val body =
            try {
                pairRequestJson(target.code, deviceName).json
            } catch (e: EternException) {
                return Result.Failure(e.message ?: "invalid device name")
            }

        val response =
            try {
                gateway.pair(target, body)
            } catch (e: GatewayException) {
                return Result.Failure(pairFailed(e))
            }

        val identity =
            try {
                parsePairedDevice(response)
            } catch (e: EternException) {
                return Result.Failure("gateway returned an unreadable pairing response")
            }

        // 3. Persist. `paired` needs host + pin + credential, so write all three together.
        prefs.certPin = target.certPin
        prefs.host = target.host
        prefs.port = target.port
        prefs.device = identity
        prefs.lastSeenWindowSeq = 0L
        gateway.invalidate()

        Log.i(TAG, "paired as ${identity.deviceName} (${identity.deviceId})")
        app.startServices()
        return Result.Success(identity.deviceName, target.host)
    }

    private fun pairFailed(e: GatewayException): String =
        when (e.status) {
            403 -> "pairing code expired or already used — generate a new one on the desktop"
            400 -> "the desktop rejected the pairing request"
            0 -> e.message
            else -> "gateway returned HTTP ${e.status}"
        }

    private companion object {
        const val TAG = "EternPair"
    }
}
