//! Shared application state: the hot-swappable runtime (config + engine),
//! the mapping store, the LLM client and admission control.

use crate::config::ServerConfig;
use crate::limits::Admission;
use crate::llm::LlmClient;
use crate::store::TieredStore;
use arc_swap::ArcSwap;
use maskarad_core::Engine;
use metrics_exporter_prometheus::PrometheusHandle;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub struct Runtime {
    pub cfg: ServerConfig,
    pub engine: Engine,
    pub loaded_at: Instant,
}

impl Runtime {
    pub fn build(cfg: ServerConfig) -> anyhow::Result<Self> {
        let engine = Engine::new(&cfg.core)?;
        Ok(Self {
            cfg,
            engine,
            loaded_at: Instant::now(),
        })
    }
}

#[derive(Clone)]
pub struct AppState {
    pub runtime: Arc<ArcSwap<Runtime>>,
    pub store: Arc<TieredStore>,
    pub llm: Arc<LlmClient>,
    pub admission: Arc<Admission>,
    pub metrics: PrometheusHandle,
    pub config_path: Option<PathBuf>,
    pub started: Instant,
}

impl AppState {
    pub fn runtime(&self) -> Arc<Runtime> {
        self.runtime.load_full()
    }
}

#[cfg(test)]
impl AppState {
    /// `config` (YAML) over a tiered store whose primary is `primary`.
    pub fn for_tests(config: &str, primary: Arc<crate::store::testing::FlakyStore>) -> Self {
        let cfg = ServerConfig::from_yaml_str(config).expect("test config");
        let admission = Arc::new(Admission::new(&cfg.server));
        let runtime = Runtime::build(cfg).expect("engine");
        Self {
            runtime: Arc::new(ArcSwap::from_pointee(runtime)),
            store: Arc::new(TieredStore::with_primary(
                primary,
                std::time::Duration::from_secs(60),
                1 << 20,
            )),
            llm: Arc::new(LlmClient::new()),
            admission,
            metrics: metrics_exporter_prometheus::PrometheusBuilder::new()
                .build_recorder()
                .handle(),
            config_path: None,
            started: Instant::now(),
        }
    }
}
