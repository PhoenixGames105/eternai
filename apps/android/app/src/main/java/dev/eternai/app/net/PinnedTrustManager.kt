package dev.eternai.app.net

import java.math.BigInteger
import java.security.MessageDigest
import java.security.cert.X509Certificate
import javax.net.ssl.HostnameVerifier
import javax.net.ssl.X509TrustManager

/**
 * Trusts exactly one certificate: the gateway's self-signed cert, identified by
 * the SHA-256 of its DER encoding, base64-encoded — the same value the gateway
 * publishes as `cert_pin` on `/health` and hands out in the pairing QR.
 *
 * A self-signed cert can't chain to anything, so the pin *is* the trust anchor.
 * Hostname checking is therefore deliberately not used: the pin is strictly
 * stronger than a name match (a wrong-name cert from the right key still pins,
 * a right-name cert from an attacker's key does not).
 */
class PinnedTrustManager(private val pinBase64: String) : X509TrustManager {

    private val expected: ByteArray = run {
        val cleaned = pinBase64.trim().removePrefix("sha256/").replace(" ", "")
        java.util.Base64.getDecoder().decode(cleaned)
    }

    override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) {
        throw CertificatePinException("eternAI does not accept client certificates")
    }

    override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {
        val leaf = chain.firstOrNull()
            ?: throw CertificatePinException("empty certificate chain")
        val actual = MessageDigest.getInstance("SHA-256").digest(leaf.encoded)
        if (!actual.contentEquals(expected)) {
            throw CertificatePinException(
                "certificate pin mismatch — the gateway's cert changed, re-pair. " +
                    "expected=${encode(expected)} actual=${encode(actual)}"
            )
        }
    }

    override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()

    private fun encode(bytes: ByteArray): String =
        java.util.Base64.getEncoder().encodeToString(bytes)
}

class CertificatePinException(message: String) : RuntimeException(message)

/**
 * The gateway's cert carries LAN-address SANs that go stale when the router
 * reassigns a lease. Pinning the key makes a name mismatch harmless, and
 * refusing every name would break pairing whenever the IP moves.
 */
val PinningHostnameVerifier = HostnameVerifier { _, _ -> true }

/** Formats a fingerprint the way `openssl x509 -fingerprint` does, for logs. */
fun formatFingerprint(cert: X509Certificate): String =
    MessageDigest.getInstance("SHA-1")
        .digest(cert.encoded)
        .joinToString(":") { "%02X".format(it) }

/** Hex serial, handy when comparing two certs that both fail the pin. */
fun serialHex(cert: X509Certificate): String =
    String.format("%016X", cert.serialNumber ?: BigInteger.ZERO)
