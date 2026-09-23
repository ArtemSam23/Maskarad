//! Service configuration: the engine configuration plus server, store, LLM,
//! admin, logging and UI sections. `${VAR}` and `${VAR:-default}` in the file
//! are replaced from the environment before parsing.

use anyhow::{Context, Result};
use maskarad_core::CoreConfig;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::path::Path;

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}
fn default_body() -> usize {
    4 * 1024 * 1024
}
fn default_inflight() -> usize {
    1024
}
fn default_timeout() -> u64 {
    8000
}
fn default_blocking_threshold() -> usize {
    64 * 1024
}
fn default_ttl() -> u64 {
    3600
}
fn default_store_timeout() -> u64 {
    1000
}
fn default_true() -> bool {
    true
}
fn default_llm_timeout() -> u64 {
    60_000
}
fn default_cb_failures() -> u32 {
    5
}
fn default_cb_cooldown() -> u64 {
    10_000
}
fn default_level() -> String {
    "info".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub store: StoreSection,
    #[serde(default)]
    pub llm: LlmSection,
    #[serde(default)]
    pub admin: AdminSection,
    #[serde(default)]
    pub logging: LoggingSection,
    #[serde(default)]
    pub ui: UiSection,
    #[serde(flatten)]
    pub core: CoreConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    #[serde(default = "default_listen")]
    pub listen: String,
    /// Maximum request body; texts of 100 000 tokens fit comfortably.
    #[serde(default = "default_body")]
    pub max_body_bytes: usize,
    /// Hard ceiling on requests in flight, all admission lanes together,
    /// large requests waiting their turn included; above it — 429. The short
    /// lane's only bound while the adaptive limit is disengaged.
    #[serde(default = "default_inflight")]
    pub max_inflight: usize,
    #[serde(default = "default_timeout")]
    pub request_timeout_ms: u64,
    /// Texts larger than this are processed on the blocking thread pool so
    /// they never stall the network reactor. Requests with a larger
    /// `Content-Length` go to the large admission lane.
    #[serde(default = "default_blocking_threshold")]
    pub blocking_threshold_bytes: usize,
    #[serde(default)]
    pub admission: AdmissionSection,
}

fn default_min_limit() -> usize {
    8
}
fn default_target_ms() -> u64 {
    5
}

/// Admission lanes and the adaptive limit of short requests, see `limits`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionSection {
    /// `false`: a single static limit of `max_inflight`, no lanes.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// The adaptive limit never goes below this.
    #[serde(default = "default_min_limit")]
    pub min_limit: usize,
    /// Requests with a body over `blocking_threshold_bytes` processed at
    /// once; the rest wait their turn. Default: the number of CPUs available
    /// to the process, as many as the runtime has workers.
    #[serde(default)]
    pub large_limit: Option<usize>,
    /// Mean latency of short requests the limit never tries to go below:
    /// it shrinks only above max(target_ms, min(2 × latency without load,
    /// 10 × target_ms)).
    #[serde(default = "default_target_ms")]
    pub target_ms: u64,
}

impl Default for AdmissionSection {
    fn default() -> Self {
        Self {
            enabled: true,
            min_limit: default_min_limit(),
            large_limit: None,
            target_ms: default_target_ms(),
        }
    }
}

impl Default for ServerSection {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            max_body_bytes: default_body(),
            max_inflight: default_inflight(),
            request_timeout_ms: default_timeout(),
            blocking_threshold_bytes: default_blocking_threshold(),
            admission: AdmissionSection::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreBackend {
    #[default]
    Memory,
    Redis,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreSection {
    #[serde(default)]
    pub backend: StoreBackend,
    #[serde(default)]
    pub redis_url: Option<String>,
    #[serde(default = "default_ttl")]
    pub ttl_seconds: u64,
    /// Base64 32-byte key (or any passphrase, hashed) for AES-256-GCM
    /// encryption of stored mappings. Required for the Redis backend.
    #[serde(default)]
    pub encryption_key: Option<String>,
    /// Start with a memory-only store when Redis is unreachable at startup;
    /// such a replica works but reports itself as degraded, because its
    /// mappings are invisible to the other replicas. Redis errors at runtime
    /// are never hidden behind memory: they are returned to the caller.
    #[serde(default = "default_true")]
    pub memory_fallback: bool,
    /// Per-command timeout for Redis; a command is retried once after a
    /// timeout, so a store operation takes at most twice this long.
    #[serde(default = "default_store_timeout")]
    pub timeout_ms: u64,
    /// Bytes of mappings kept in process memory (L1 cache / memory backend),
    /// estimated from keys, entries and originals. Default 256 MiB.
    #[serde(default = "default_capacity_bytes")]
    pub memory_capacity_bytes: u64,
}

fn default_capacity_bytes() -> u64 {
    256 * 1024 * 1024
}

impl Default for StoreSection {
    fn default() -> Self {
        Self {
            backend: StoreBackend::Memory,
            redis_url: None,
            ttl_seconds: default_ttl(),
            encryption_key: None,
            memory_fallback: true,
            timeout_ms: default_store_timeout(),
            memory_capacity_bytes: default_capacity_bytes(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSection {
    /// OpenAI-compatible base URL, e.g. `https://api.openai.com/v1`.
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_llm_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_cb_failures")]
    pub circuit_breaker_failures: u32,
    #[serde(default = "default_cb_cooldown")]
    pub circuit_breaker_cooldown_ms: u64,
}

impl Default for LlmSection {
    fn default() -> Self {
        Self {
            base_url: None,
            api_key: None,
            timeout_ms: default_llm_timeout(),
            circuit_breaker_failures: default_cb_failures(),
            circuit_breaker_cooldown_ms: default_cb_cooldown(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminSection {
    /// Key for `/admin/*` and `/v1/systems`; unset disables them.
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingSection {
    /// `json` (default) or `pretty`.
    #[serde(default)]
    pub format: LogFormat,
    #[serde(default = "default_level")]
    pub level: String,
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            format: LogFormat::Json,
            level: default_level(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    #[default]
    Json,
    Pretty,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Demo mode: `/v1/*` requests may name a `system` without its key.
    #[serde(default)]
    pub allow_system_override: bool,
}

impl Default for UiSection {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_system_override: false,
        }
    }
}

/// Replaces `${VAR}` and `${VAR:-default}` with environment values.
pub fn interpolate_env(text: &str) -> String {
    let re = Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(?::-([^}]*))?\}").expect("static regex");
    re.replace_all(text, |caps: &regex::Captures<'_>| {
        let name = &caps[1];
        match std::env::var(name) {
            Ok(v) => v,
            Err(_) => caps
                .get(2)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default(),
        }
    })
    .into_owned()
}

impl ServerConfig {
    pub fn from_yaml_str(text: &str) -> Result<Self> {
        let cfg: ServerConfig =
            serde_yaml::from_str(&interpolate_env(text)).context("invalid configuration")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::from_yaml_str(&text)
    }

    fn validate(&self) -> Result<()> {
        self.server
            .listen
            .parse::<std::net::SocketAddr>()
            .with_context(|| format!("invalid server.listen `{}`", self.server.listen))?;
        let admission = &self.server.admission;
        anyhow::ensure!(
            self.server.max_inflight >= 1,
            "server.max_inflight must be at least 1"
        );
        anyhow::ensure!(
            !admission.enabled || (1..=self.server.max_inflight).contains(&admission.min_limit),
            "server.admission.min_limit must be between 1 and server.max_inflight"
        );
        anyhow::ensure!(
            admission.large_limit != Some(0),
            "server.admission.large_limit must be at least 1"
        );
        anyhow::ensure!(
            admission.target_ms >= 1,
            "server.admission.target_ms must be at least 1"
        );
        if self.store.backend == StoreBackend::Redis {
            anyhow::ensure!(
                self.store
                    .redis_url
                    .as_deref()
                    .map(|u| !u.is_empty())
                    .unwrap_or(false),
                "store.redis_url is required for the redis backend"
            );
            anyhow::ensure!(
                self.store.encryption_key.as_deref().map(|k| !k.is_empty()).unwrap_or(false),
                "store.encryption_key is required for the redis backend (mappings are encrypted at rest)"
            );
        }
        anyhow::ensure!(
            !self.core.systems.is_empty(),
            "at least one consumer system must be configured"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_interpolation_with_defaults() {
        std::env::set_var("MASKARAD_TEST_VAR", "value");
        assert_eq!(
            interpolate_env(
                "a=${MASKARAD_TEST_VAR} b=${MASKARAD_MISSING:-dflt} c=${MASKARAD_MISSING}"
            ),
            "a=value b=dflt c="
        );
    }

    #[test]
    fn minimal_config_parses() {
        let cfg =
            ServerConfig::from_yaml_str("systems:\n  - id: a\n    auth: { anonymous: true }\n")
                .unwrap();
        assert_eq!(cfg.server.listen, "0.0.0.0:8080");
        assert_eq!(cfg.core.systems.len(), 1);
    }

    #[test]
    fn store_defaults() {
        let cfg =
            ServerConfig::from_yaml_str("systems:\n  - id: a\n    auth: { anonymous: true }\n")
                .unwrap();
        assert_eq!(cfg.store.timeout_ms, 1000);
        assert!(cfg.store.memory_fallback);
        assert_eq!(cfg.store.ttl_seconds, 3600);
        assert_eq!(cfg.store.memory_capacity_bytes, 256 * 1024 * 1024);
    }

    #[test]
    fn admission_defaults_and_validation() {
        let base = "systems:\n  - id: a\n    auth: { anonymous: true }\n";
        let cfg = ServerConfig::from_yaml_str(base).unwrap();
        assert_eq!(cfg.server.max_inflight, 1024);
        let a = &cfg.server.admission;
        assert!(a.enabled);
        assert_eq!((a.min_limit, a.large_limit, a.target_ms), (8, None, 5));

        let with =
            |server: &str| ServerConfig::from_yaml_str(&format!("server:\n{server}\n{base}"));
        assert!(
            with("  max_inflight: 4").is_err(),
            "min_limit 8 above the ceiling"
        );
        assert!(with("  max_inflight: 4\n  admission: { min_limit: 4 }").is_ok());
        assert!(with("  admission: { large_limit: 0 }").is_err());
        let large = with("  admission: { large_limit: 3 }").unwrap();
        assert_eq!(large.server.admission.large_limit, Some(3));
        assert!(with("  admission: { target_ms: 0 }").is_err());
        assert!(with("  admission: { unknown: 1 }").is_err());
        let off = with("  max_inflight: 1\n  admission: { enabled: false }").unwrap();
        assert!(
            !off.server.admission.enabled,
            "min_limit is not checked when off"
        );
    }
}
