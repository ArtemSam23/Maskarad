//! Configuration hot reload: on file change or `POST /admin/reload` the
//! configuration is parsed and a new engine is built; the old runtime stays
//! active until the new one is ready, and an invalid file changes nothing.

use crate::config::ServerConfig;
use crate::state::{AppState, Runtime};
use notify::{EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Serialize)]
pub struct ReloadSummary {
    pub status: &'static str,
    pub systems: usize,
    pub types: usize,
    pub build_ms: u128,
}

pub async fn reload(state: &AppState) -> anyhow::Result<ReloadSummary> {
    let path = state
        .config_path
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no configuration file to reload"))?;
    let started = Instant::now();
    let runtime = tokio::task::spawn_blocking(move || -> anyhow::Result<Runtime> {
        let cfg = ServerConfig::load(&path)?;
        Runtime::build(cfg)
    })
    .await??;
    let summary = ReloadSummary {
        status: "reloaded",
        systems: runtime.engine.systems().count(),
        types: runtime.engine.registry().defs().len(),
        build_ms: started.elapsed().as_millis(),
    };
    state.runtime.store(Arc::new(runtime));
    tracing::info!(
        systems = summary.systems,
        types = summary.types,
        build_ms = summary.build_ms,
        "configuration reloaded"
    );
    Ok(summary)
}

/// Watches the configuration file and reloads on change (debounced).
pub fn watch(state: AppState, path: &Path) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| ".".into());
    let file_name = path.file_name().map(|n| n.to_os_string());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let relevant = matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
                && event.paths.iter().any(|p| {
                    p.file_name()
                        .map(|n| Some(n.to_os_string()) == file_name)
                        .unwrap_or(false)
                });
            if relevant {
                let _ = tx.send(());
            }
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    tokio::spawn(async move {
        let _watcher = watcher;
        while rx.recv().await.is_some() {
            // Debounce bursts of write events from editors.
            tokio::time::sleep(Duration::from_millis(500)).await;
            while rx.try_recv().is_ok() {}
            match reload(&state).await {
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "configuration file changed but not applied; previous configuration stays active")
                }
            }
        }
    });
    Ok(())
}
