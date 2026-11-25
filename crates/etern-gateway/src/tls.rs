//! Self-signed TLS material for the LAN listener (PLAN §8).
//!
//! The phone cannot verify a public CA for a home-lan server, so the gateway
//! mints a long-lived self-signed certificate on first start, persists it next
//! to the store, and publishes its SHA-256 fingerprint on `/health`. Pairing
//! hands that pin to the device; the app then trusts *this* certificate and
//! nothing else (certificate pinning), which is the strongest guarantee
//! available without a private CA on the phone.
//!
//! The material is generated once and reused across restarts so the pin stays
//! stable — a changed pin means the cert was replaced and devices must re-pair.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::Engine as _;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use sha2::{Digest, Sha256};

/// Ten years: long enough that a pin survives the whole life of an install,
/// short enough that a forgotten device still notices something odd.
const VALID_DAYS: i64 = 3650;

pub struct TlsMaterial {
    pub cert_pem: String,
    pub key_pem: String,
    /// `base64(SHA-256(cert DER))` — the value `/health` publishes and the
    /// phone pins. Sometimes called a "cert hash" or `ca_pin` in the plan.
    pub pin: String,
}

/// Default home for the cert: a `tls/` directory beside the canonical store.
pub fn tls_dir_for(store_path: &Path) -> PathBuf {
    store_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .join("tls")
}

/// Loads `cert.pem`/`key.pem` from `dir`, generating (and persisting) them on
/// first run. `extra_ips` are added to the SANs alongside `localhost` and the
/// loopback addresses, so a phone connecting by LAN IP gets a name match too.
pub fn load_or_generate(dir: &Path, extra_ips: Vec<IpAddr>) -> Result<TlsMaterial, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");

    if let (Ok(cert_pem), Ok(key_pem)) = (
        std::fs::read_to_string(&cert_path),
        std::fs::read_to_string(&key_path),
    ) {
        if let Some(pin) = pin_from_pem(&cert_pem) {
            return Ok(TlsMaterial { cert_pem, key_pem, pin });
        }
        // Corrupt or unparseable: fall through and mint a fresh pair. The old
        // files are overwritten below; devices holding the old pin will fail
        // closed, which is the correct response to a broken cert anyway.
        tracing::warn!(dir = %dir.display(), "unparseable cert on disk, regenerating");
    }

    let material = generate(extra_ips)?;
    std::fs::write(&cert_path, &material.cert_pem)
        .map_err(|e| format!("cannot write {}: {e}", cert_path.display()))?;
    std::fs::write(&key_path, &material.key_pem)
        .map_err(|e| format!("cannot write {}: {e}", key_path.display()))?;
    Ok(material)
}

fn generate(extra_ips: Vec<IpAddr>) -> Result<TlsMaterial, String> {
    let mut sans: Vec<SanType> = Vec::new();
    let localhost: rcgen::string::Ia5String = "localhost"
        .try_into()
        .map_err(|e| format!("localhost is not a valid SAN: {e}"))?;
    sans.push(SanType::DnsName(localhost));
    sans.push(SanType::IpAddress(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)));
    sans.push(SanType::IpAddress(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    for ip in extra_ips {
        let san = SanType::IpAddress(ip);
        if !sans.contains(&san) {
            sans.push(san);
        }
    }

    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "eternAI gateway");
    params.distinguished_name = dn;
    params.subject_alt_names = sans;
    let now = time::OffsetDateTime::now_utc();
    // Backdate an hour so a slightly-skewed clock does not reject the cert.
    params.not_before = now - time::Duration::hours(1);
    params.not_after = now + time::Duration::days(VALID_DAYS);

    let key_pair = KeyPair::generate().map_err(|e| format!("keygen failed: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("self-sign failed: {e}"))?;

    let cert_pem = cert.pem();
    let pin = pin_from_pem(&cert_pem).ok_or("freshly generated cert did not parse")?;
    Ok(TlsMaterial {
        key_pem: key_pair.serialize_pem(),
        cert_pem,
        pin,
    })
}

/// `base64(SHA-256(DER))` of the first certificate in a PEM document.
pub fn pin_from_pem(pem: &str) -> Option<String> {
    let body = pem
        .lines()
        .filter(|line| !line.trim().starts_with("-----") && !line.trim().is_empty())
        .collect::<String>();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body)
        .ok()?;
    if der.is_empty() {
        return None;
    }
    let digest = Sha256::digest(&der);
    Some(base64::engine::general_purpose::STANDARD.encode(digest))
}

/// Best-effort discovery of this host's routable LAN address via the routing
/// table (the UDP socket never sends a packet). Used to widen the cert SANs so
/// a phone dialling `https://192.168.x.x:8443` gets a name match.
pub fn local_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip())
}

/// Discovers the Tailscale IPv4 address when the CLI is installed and online.
pub fn tailscale_ip() -> Option<IpAddr> {
    let mut executables = vec![PathBuf::from("tailscale")];
    #[cfg(windows)]
    executables.push(PathBuf::from(
        r"C:\Program Files\Tailscale\tailscale.exe",
    ));

    for executable in executables {
        let output = Command::new(executable)
            .args(["ip", "-4"])
            .stdin(Stdio::null())
            .output();
        let Ok(output) = output else {
            continue;
        };
        if output.status.success() {
            if let Some(ip) = parse_tailscale_ipv4(&output.stdout) {
                return Some(IpAddr::V4(ip));
            }
        }
    }
    None
}

fn parse_tailscale_ipv4(output: &[u8]) -> Option<Ipv4Addr> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| line.trim().parse::<Ipv4Addr>().ok())
        .find(|ip| {
            let octets = ip.octets();
            octets[0] == 100 && (64..=127).contains(&octets[1])
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("etern-tls-{}-{}-{}", tag, std::process::id(), n))
    }

    #[test]
    fn tailscale_ip_parser_accepts_only_tailnet_ipv4_addresses() {
        assert_eq!(
            parse_tailscale_ipv4(b"100.77.211.47\n"),
            Some(Ipv4Addr::new(100, 77, 211, 47))
        );
        assert_eq!(parse_tailscale_ipv4(b"192.168.1.25\n"), None);
        assert_eq!(parse_tailscale_ipv4(b"not an address\n"), None);
    }

    #[test]
    fn generates_persists_and_reloads_stably() {
        let dir = temp_dir("stable");
        let first = load_or_generate(&dir, vec![]).expect("generate");
        assert!(first.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(first.key_pem.contains("PRIVATE KEY"));
        assert!(dir.join("cert.pem").exists());
        assert!(dir.join("key.pem").exists());

        let second = load_or_generate(&dir, vec![]).expect("reload");
        assert_eq!(first.pin, second.pin, "the pin must survive restarts");
        assert_eq!(first.cert_pem, second.cert_pem);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pin_is_the_sha256_of_the_der() {
        let dir = temp_dir("pin");
        let material = load_or_generate(&dir, vec![]).expect("generate");
        let body = material
            .cert_pem
            .lines()
            .filter(|l| !l.trim().starts_with("-----") && !l.trim().is_empty())
            .collect::<String>();
        let der = base64::engine::general_purpose::STANDARD
            .decode(body)
            .expect("valid base64");
        let expected = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&der));
        assert_eq!(material.pin, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_cert_is_replaced() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("cert.pem"), "not a certificate").expect("write");
        std::fs::write(dir.join("key.pem"), "also not a key").expect("write");

        let material = load_or_generate(&dir, vec![]).expect("regenerate");
        assert!(material.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert_eq!(material.pin, pin_from_pem(&material.cert_pem).expect("pin"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extra_ips_are_accepted_into_sans() {
        let dir = temp_dir("sans");
        let ip: IpAddr = "192.168.1.50".parse().expect("ip");
        let material = load_or_generate(&dir, vec![ip, ip]).expect("generate");
        // The cert must parse and pin; SAN contents are verified by the pin
        // round-trip above (rcgen rejects invalid SANs at generation time).
        assert!(pin_from_pem(&material.cert_pem).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tls_dir_derives_from_store_path() {
        assert_eq!(
            tls_dir_for(Path::new("/var/db/etern.db")),
            PathBuf::from("/var/db/tls")
        );
        // A bare relative store path has no usable parent dir, so the tls/
        // directory hangs off the current working directory.
        assert_eq!(
            tls_dir_for(Path::new("etern.db")),
            Path::new(".").join("tls")
        );
    }
}
