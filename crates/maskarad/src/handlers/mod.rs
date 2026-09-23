pub mod admin;
pub mod docs;
pub mod health;
pub mod llm;
pub mod process;
pub mod ui;
pub mod v1;

use crate::error::ApiError;
use crate::state::Runtime;
use maskarad_core::{CompiledSystem, MaskResult};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;

/// A successfully handled request. `elapsed` (since the handler was
/// entered) is read once, right before the request's log line, and the same
/// value is recorded in `maskarad_request_duration_seconds`, so the log and
/// the metric agree.
pub struct Done<T> {
    pub body: T,
    pub system: String,
    pub direction: &'static str,
    pub elapsed: Duration,
}

/// Parses a JSON body with the unified error format on failure.
pub fn parse_json<T: DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    if body.is_empty() {
        return Err(ApiError::invalid("empty body, expected JSON"));
    }
    serde_json::from_slice(body).map_err(|e| ApiError::invalid(format!("invalid JSON body: {e}")))
}

/// Runs detection + masking, on the blocking pool for large texts so the
/// reactor never stalls on a 100k-token document.
pub async fn mask_text(
    rt: Arc<Runtime>,
    system: &CompiledSystem,
    text: String,
) -> Result<MaskResult, ApiError> {
    if text.len() <= rt.cfg.server.blocking_threshold_bytes {
        return Ok(rt.engine.mask(&text, system));
    }
    let system_id = system.id().to_string();
    tokio::task::spawn_blocking(move || {
        let system = rt
            .engine
            .system(&system_id)
            .expect("system exists in this runtime");
        rt.engine.mask(&text, system)
    })
    .await
    .map_err(masking_task_failed)
}

/// `JoinError`'s `Display` carries the panic message, and that can quote the
/// text being masked.
fn masking_task_failed(e: tokio::task::JoinError) -> ApiError {
    tracing::error!(
        panicked = e.is_panic(),
        cancelled = e.is_cancelled(),
        "masking task failed"
    );
    ApiError::internal()
}

pub fn count_types(result: &MaskResult) -> Vec<(maskarad_core::PiiType, usize)> {
    result.type_counts()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::capture;

    #[tokio::test]
    async fn a_failed_masking_task_is_logged_without_the_panic_message() {
        let err = tokio::task::spawn_blocking(|| {
            let text = String::from("Клиент Иванов Иван Иванович");
            text[..1].len()
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("'К'"), "{err}");

        let (_guard, logs) = capture::start();
        let e = masking_task_failed(err);
        assert_eq!(e.code, "INTERNAL");
        let lines = logs.with_message("masking task failed");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["panicked"], true);
        assert_eq!(lines[0]["cancelled"], false);
        let raw = logs.raw();
        assert!(
            !raw.contains('К') && !raw.contains("char boundary"),
            "{raw}"
        );
    }
}
