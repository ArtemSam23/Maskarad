//! Liveness, readiness and metrics.

use crate::state::AppState;
use crate::store::MappingStore;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Проверка живости процесса — всегда `200 ok`, без обращения к хранилищу или движку.
#[utoipa::path(
    get,
    path = "/healthz",
    tag = "service",
    responses((status = 200, description = "Процесс жив", body = String, content_type = "text/plain")),
    security()
)]
pub async fn healthz() -> &'static str {
    "ok"
}

/// Готовность к трафику: состояние хранилища соответствий, движка и LLM-клиента.
/// `status: "degraded"`, если хранилище соответствий деградировало (см. «Надёжность и
/// деградация» в README) — используется балансировщиком/Kubernetes для readiness probe.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "service",
    responses((status = 200, description = "`{status, store, engine, llm, uptime_seconds, version}` — форма свободная, см. пример", body = serde_json::Value)),
    security()
)]
pub async fn readyz(State(state): State<AppState>) -> Response {
    let rt = state.runtime();
    let store_ok = state.store.healthy().await;
    let degraded = state.store.is_degraded() || !store_ok;
    let body = json!({
        "status": if degraded { "degraded" } else { "ok" },
        "store": { "backend": state.store.backend(), "healthy": store_ok, "degraded": state.store.is_degraded() },
        "engine": { "systems": rt.engine.systems().count(), "types": rt.engine.registry().defs().len(), "loaded_seconds_ago": rt.loaded_at.elapsed().as_secs() },
        "llm": { "configured": rt.cfg.llm.base_url.as_deref().map(|b| !b.is_empty()).unwrap_or(false), "circuit_open": state.llm.circuit_open() },
        "uptime_seconds": state.started.elapsed().as_secs(),
        "version": env!("CARGO_PKG_VERSION"),
    });
    (StatusCode::OK, Json(body)).into_response()
}

/// Метрики Prometheus: latency, RPS, TPS, найденные типы ПДн, состояние хранилища и LLM
/// — полный список в README, раздел «Безопасность, логи, метрики».
#[utoipa::path(
    get,
    path = "/metrics",
    tag = "service",
    responses((status = 200, description = "Текст в формате экспозиции Prometheus", body = String, content_type = "text/plain; version=0.0.4; charset=utf-8")),
    security()
)]
pub async fn metrics(State(state): State<AppState>) -> Response {
    let body = state.metrics.render();
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response()
}
