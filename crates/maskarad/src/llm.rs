//! Upstream LLM client (OpenAI-compatible) with a timeout and a simple
//! circuit breaker: after N consecutive failures the upstream is skipped for
//! a cool-down period and callers get a fast, clear error.

use crate::config::LlmSection;
use crate::error::ApiError;
use serde_json::Value;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct LlmClient {
    http: reqwest::Client,
    failures: AtomicU32,
    open_until_ms: AtomicU64,
    started: Instant,
}

impl LlmClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder().build().expect("reqwest client"),
            failures: AtomicU32::new(0),
            open_until_ms: AtomicU64::new(0),
            started: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    pub fn circuit_open(&self) -> bool {
        self.open_until_ms.load(Ordering::Relaxed) > self.now_ms()
    }

    fn record_failure(&self, cfg: &LlmSection) {
        let n = self.failures.fetch_add(1, Ordering::Relaxed) + 1;
        if n >= cfg.circuit_breaker_failures {
            self.open_until_ms.store(
                self.now_ms() + cfg.circuit_breaker_cooldown_ms,
                Ordering::Relaxed,
            );
            self.failures.store(0, Ordering::Relaxed);
            tracing::warn!(
                failures = n,
                cooldown_ms = cfg.circuit_breaker_cooldown_ms,
                "LLM circuit breaker opened"
            );
        }
    }

    pub async fn chat_completions(
        &self,
        cfg: &LlmSection,
        body: &Value,
    ) -> Result<Value, ApiError> {
        let Some(base) = cfg.base_url.as_deref().filter(|b| !b.is_empty()) else {
            return Err(ApiError::new(
                axum::http::StatusCode::NOT_IMPLEMENTED,
                "LLM_NOT_CONFIGURED",
                "llm.base_url is not configured",
            ));
        };
        if self.circuit_open() {
            metrics::counter!("maskarad_llm_requests_total", "result" => "circuit_open")
                .increment(1);
            return Err(ApiError::llm_unavailable(
                "circuit breaker open after repeated failures",
            ));
        }
        let url = format!("{}/chat/completions", base.trim_end_matches('/'));
        let mut req = self
            .http
            .post(&url)
            .timeout(Duration::from_millis(cfg.timeout_ms))
            .json(body);
        if let Some(key) = cfg.api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req.bearer_auth(key);
        }
        let started = Instant::now();
        let result = req.send().await;
        crate::metrics::observe_stage("llm", started.elapsed().as_secs_f64());
        match result {
            Err(e) if e.is_timeout() => {
                self.record_failure(cfg);
                metrics::counter!("maskarad_llm_requests_total", "result" => "timeout")
                    .increment(1);
                Err(ApiError::llm_timeout())
            }
            Err(e) => {
                self.record_failure(cfg);
                metrics::counter!("maskarad_llm_requests_total", "result" => "error").increment(1);
                Err(ApiError::llm_unavailable(&e.to_string()))
            }
            Ok(resp) => {
                let status = resp.status();
                if status.is_server_error() {
                    self.record_failure(cfg);
                    metrics::counter!("maskarad_llm_requests_total", "result" => "upstream_5xx")
                        .increment(1);
                    return Err(ApiError::llm_unavailable(&format!(
                        "upstream returned {status}"
                    )));
                }
                self.failures.store(0, Ordering::Relaxed);
                let value: Value = resp.json().await.map_err(|e| {
                    ApiError::llm_unavailable(&format!("invalid upstream response: {e}"))
                })?;
                if !status.is_success() {
                    metrics::counter!("maskarad_llm_requests_total", "result" => "upstream_4xx")
                        .increment(1);
                    let msg = value
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .unwrap_or("upstream error");
                    return Err(ApiError::new(
                        axum::http::StatusCode::BAD_GATEWAY,
                        "LLM_ERROR",
                        format!("upstream returned {status}: {msg}"),
                    ));
                }
                metrics::counter!("maskarad_llm_requests_total", "result" => "ok").increment(1);
                Ok(value)
            }
        }
    }
}

impl Default for LlmClient {
    fn default() -> Self {
        Self::new()
    }
}
