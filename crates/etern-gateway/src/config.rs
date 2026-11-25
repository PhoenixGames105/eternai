//! Gateway configuration.
//!
//! Everything is overridable by `--key=value` arguments and then by environment
//! variables, so the same binary works as a dev proxy and as the packaged
//! desktop companion's embedded server.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

/// Defaults chosen to coexist with the existing llama.cpp server, which already
/// owns `:8080` on this host.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8090";
pub const DEFAULT_ENGINE: &str = "http://127.0.0.1:8080";
/// A model swap on this host takes ~20 minutes (`engine_ready_s: 1230`), so a
/// vanished port is treated as "loading" for at least this long.
pub const DEFAULT_LOAD_GRACE_SECS: u64 = 1_500;
pub const DEFAULT_PROPS_TTL_SECS: u64 = 30;
/// Canonical SQLite store and the journal projection both live next to the
/// binary's working directory unless told otherwise. They are the companion's
/// only durable state; the `.txt` journal is a rebuildable projection of the db.
pub const DEFAULT_STORE: &str = "etern.db";
pub const DEFAULT_JOURNAL_DIR: &str = "Journal";
/// Root of the file-tool sandbox. The desktop jail exposes `Journal/`,
/// `memory/` and `inbox/` beneath it; defaulting to the working directory keeps
/// `Journal/` aligned with [`DEFAULT_JOURNAL_DIR`].
pub const DEFAULT_TOOL_ROOT: &str = ".";
/// TLS port the LAN listener uses when only a port or address is supplied.
pub const DEFAULT_TLS_PORT: u16 = 8443;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayConfig {
    /// Where the gateway listens. LAN-reachable by default because the phone is
    /// a first-class client; access control is the bearer token.
    pub listen: SocketAddr,
    /// Base URL of the llama.cpp-compatible server currently occupying the port.
    pub engine_base: String,
    /// Bearer token required on inference endpoints. `None` disables auth,
    /// which is only sensible on a trusted loopback listener.
    pub token: Option<String>,
    /// How long a vanished-but-recently-ready engine is still called `loading`.
    pub load_grace: Duration,
    /// How long `/props` is cached before a refresh.
    pub props_ttl: Duration,
    /// Interval between engine health probes.
    pub probe_interval: Duration,
    /// Path to the canonical SQLite store (the source of truth for turns and
    /// journal bookkeeping).
    pub store_path: PathBuf,
    /// Directory the daily/sync `.txt` journal files are written to. Deleting
    /// it loses nothing — it is rebuilt from the store.
    pub journal_dir: PathBuf,
    /// Root of the file-tool sandbox (`read_file`/`write_file`/`list_dir`).
    pub tool_root: PathBuf,
    /// Base URL of the self-hosted SearXNG instance backing `web_search`.
    /// `None` (the default) makes every `web_search` return an explicit
    /// `no_network` result instead of a guess.
    pub searxng_url: Option<String>,
    /// Second listener serving the same API over TLS with the self-signed
    /// gateway cert. `None` (the default) disables TLS entirely; the plain
    /// listener keeps working for loopback dev either way.
    pub tls_listen: Option<SocketAddr>,
    /// Directory holding `cert.pem`/`key.pem`. `None` derives
    /// `<store_dir>/tls/` from [`GatewayConfig::store_path`].
    pub tls_dir: Option<PathBuf>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            listen: DEFAULT_LISTEN.parse().expect("static"),
            engine_base: DEFAULT_ENGINE.to_string(),
            token: None,
            load_grace: Duration::from_secs(DEFAULT_LOAD_GRACE_SECS),
            props_ttl: Duration::from_secs(DEFAULT_PROPS_TTL_SECS),
            probe_interval: Duration::from_secs(5),
            store_path: PathBuf::from(DEFAULT_STORE),
            journal_dir: PathBuf::from(DEFAULT_JOURNAL_DIR),
            tool_root: PathBuf::from(DEFAULT_TOOL_ROOT),
            searxng_url: None,
            tls_listen: None,
            tls_dir: None,
        }
    }
}

impl GatewayConfig {
    /// Layered resolution: defaults, then `--key=value`, then environment.
    pub fn build(args: impl IntoIterator<Item = String>, env: impl Fn(&str) -> Option<String>) -> Self {
        let mut cfg = Self::default();
        for arg in args {
            cfg.apply_arg(&arg);
        }
        if let Some(v) = env("ETERN_LISTEN") {
            cfg.set_listen(&v);
        }
        if let Some(v) = env("ETERN_ENGINE") {
            cfg.set_engine(&v);
        }
        if let Some(v) = env("ETERN_TOKEN") {
            cfg.token = Some(v);
        }
        if let Some(v) = env("ETERN_STORE") {
            cfg.store_path = PathBuf::from(v);
        }
        if let Some(v) = env("ETERN_JOURNAL_DIR") {
            cfg.journal_dir = PathBuf::from(v);
        }
        if let Some(v) = env("ETERN_TOOL_ROOT") {
            cfg.tool_root = PathBuf::from(v);
        }
        if let Some(v) = env("ETERN_SEARXNG") {
            cfg.searxng_url = Some(v.trim_end_matches('/').to_string());
        }
        if let Some(v) = env("ETERN_TLS_LISTEN") {
            cfg.set_tls_listen(&v);
        }
        if let Some(v) = env("ETERN_TLS_DIR") {
            cfg.tls_dir = Some(PathBuf::from(v));
        }
        cfg
    }

    fn apply_arg(&mut self, arg: &str) {
        let Some((key, value)) = arg.split_once('=') else {
            return;
        };
        match key.trim_start_matches('-') {
            "listen" => self.set_listen(value),
            "engine" => self.set_engine(value),
            "token" => self.token = Some(value.to_string()),
            "load-grace-secs" => {
                if let Ok(secs) = value.parse() {
                    self.load_grace = Duration::from_secs(secs);
                }
            }
            "props-ttl-secs" => {
                if let Ok(secs) = value.parse() {
                    self.props_ttl = Duration::from_secs(secs);
                }
            }
            "probe-interval-secs" => {
                if let Ok(secs) = value.parse::<u64>() {
                    self.probe_interval = Duration::from_secs(secs.max(1));
                }
            }
            "store" => self.store_path = PathBuf::from(value),
            "journal-dir" => self.journal_dir = PathBuf::from(value),
            "tool-root" => self.tool_root = PathBuf::from(value),
            "searxng" => self.searxng_url = Some(value.trim_end_matches('/').to_string()),
            "tls-listen" => self.set_tls_listen(value),
            "tls-dir" => self.tls_dir = Some(PathBuf::from(value)),
            _ => {}
        }
    }

    /// A bare port (`8090`) or bare address (`lan`) is accepted and expanded.
    fn set_listen(&mut self, value: &str) {
        if let Ok(port) = value.parse::<u16>() {
            self.listen = SocketAddr::from(([0, 0, 0, 0], port));
            return;
        }
        if let Ok(ip) = value.parse::<IpAddr>() {
            self.listen = SocketAddr::new(ip, self.listen.port());
            return;
        }
        if let Ok(addr) = value.parse::<SocketAddr>() {
            self.listen = addr;
        }
    }

    fn set_engine(&mut self, value: &str) {
        self.engine_base = value.trim_end_matches('/').to_string();
    }

    /// TLS listener accepts the same shapes as [`Self::set_listen`]; a bare
    /// port or address expands to the PLAN's `:8443`.
    fn set_tls_listen(&mut self, value: &str) {
        if let Ok(port) = value.parse::<u16>() {
            self.tls_listen = Some(SocketAddr::from(([0, 0, 0, 0], port)));
            return;
        }
        if let Ok(ip) = value.parse::<IpAddr>() {
            self.tls_listen = Some(SocketAddr::new(ip, DEFAULT_TLS_PORT));
            return;
        }
        if let Ok(addr) = value.parse::<SocketAddr>() {
            self.tls_listen = Some(addr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn defaults_match_the_live_host() {
        let cfg = GatewayConfig::build([], no_env);
        assert_eq!(cfg.listen.port(), 8090);
        assert_eq!(cfg.engine_base, "http://127.0.0.1:8080");
        assert_eq!(cfg.token, None);
        assert!(cfg.load_grace.as_secs() > 1_230, "must outlast a model swap");
    }

    #[test]
    fn args_override_defaults() {
        let cfg = GatewayConfig::build(
            [
                "--listen=127.0.0.1:9000".to_string(),
                "--engine=http://127.0.0.1:8081/".to_string(),
                "--token=s3cret".to_string(),
            ],
            no_env,
        );
        assert_eq!(cfg.listen.to_string(), "127.0.0.1:9000");
        assert_eq!(cfg.engine_base, "http://127.0.0.1:8081");
        assert_eq!(cfg.token.as_deref(), Some("s3cret"));
    }

    #[test]
    fn env_wins_over_args() {
        let cfg = GatewayConfig::build(["--token=from-arg".to_string()], |k| {
            (k == "ETERN_TOKEN").then(|| "from-env".to_string())
        });
        assert_eq!(cfg.token.as_deref(), Some("from-env"));
    }

    #[test]
    fn bare_port_and_bad_values_are_tolerated() {
        let cfg = GatewayConfig::build(
            ["--listen=9999".to_string(), "--listen=not-an-address".to_string()],
            no_env,
        );
        assert_eq!(cfg.listen.port(), 9999, "the bad value must not clobber the good one");
    }

    #[test]
    fn store_and_journal_paths_are_configurable() {
        let cfg = GatewayConfig::build(
            ["--store=/var/db/etern.db".to_string(), "--journal-dir=/data/Journal".to_string()],
            no_env,
        );
        assert_eq!(cfg.store_path.to_string_lossy(), "/var/db/etern.db");
        assert_eq!(cfg.journal_dir.to_string_lossy(), "/data/Journal");
    }

    #[test]
    fn store_and_journal_env_vars_win() {
        let cfg = GatewayConfig::build(["--store=from-arg".to_string()], |k| match k {
            "ETERN_STORE" => Some("from-env.db".to_string()),
            "ETERN_JOURNAL_DIR" => Some("EnvJournal".to_string()),
            _ => None,
        });
        assert_eq!(cfg.store_path.to_string_lossy(), "from-env.db");
        assert_eq!(cfg.journal_dir.to_string_lossy(), "EnvJournal");
    }

    #[test]
    fn tool_root_defaults_and_searxng_off_by_default() {
        let cfg = GatewayConfig::build([], no_env);
        assert_eq!(cfg.tool_root.to_string_lossy(), ".");
        assert_eq!(cfg.searxng_url, None);
    }

    #[test]
    fn tool_root_and_searxng_are_configurable() {
        let cfg = GatewayConfig::build(
            ["--tool-root=/data/tools".to_string(), "--searxng=http://127.0.0.1:8888/".to_string()],
            no_env,
        );
        assert_eq!(cfg.tool_root.to_string_lossy(), "/data/tools");
        assert_eq!(cfg.searxng_url.as_deref(), Some("http://127.0.0.1:8888"));
    }

    #[test]
    fn tool_root_and_searxng_env_vars_win() {
        let cfg = GatewayConfig::build(["--searxng=from-arg".to_string()], |k| match k {
            "ETERN_TOOL_ROOT" => Some("EnvTools".to_string()),
            "ETERN_SEARXNG" => Some("http://env:8888".to_string()),
            _ => None,
        });
        assert_eq!(cfg.tool_root.to_string_lossy(), "EnvTools");
        assert_eq!(cfg.searxng_url.as_deref(), Some("http://env:8888"));
    }

    #[test]
    fn tls_is_off_by_default() {
        let cfg = GatewayConfig::build([], no_env);
        assert_eq!(cfg.tls_listen, None);
        assert_eq!(cfg.tls_dir, None);
    }

    #[test]
    fn tls_listen_accepts_bare_port_and_full_addr() {
        let cfg = GatewayConfig::build(["--tls-listen=8443".to_string()], no_env);
        assert_eq!(cfg.tls_listen.map(|a| a.to_string()).as_deref(), Some("0.0.0.0:8443"));
        let cfg = GatewayConfig::build(["--tls-listen=127.0.0.1:9443".to_string()], no_env);
        assert_eq!(cfg.tls_listen.map(|a| a.to_string()).as_deref(), Some("127.0.0.1:9443"));
        let cfg = GatewayConfig::build(["--tls-listen=junk".to_string()], no_env);
        assert_eq!(cfg.tls_listen, None, "junk must not enable TLS");
    }

    #[test]
    fn tls_dir_and_listen_env_vars_win() {
        let cfg = GatewayConfig::build(["--tls-listen=1111".to_string()], |k| match k {
            "ETERN_TLS_LISTEN" => Some("2222".to_string()),
            "ETERN_TLS_DIR" => Some("EnvTls".to_string()),
            _ => None,
        });
        assert_eq!(cfg.tls_listen.map(|a| a.port()), Some(2222));
        assert_eq!(cfg.tls_dir.map(|p| p.to_string_lossy().into_owned()).as_deref(), Some("EnvTls"));
    }
}
