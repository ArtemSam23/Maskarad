//! Operator endpoints: configuration reload.

use crate::auth::require_admin;
use crate::reload;
use crate::state::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;

pub async fn reload_config(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let rt = state.runtime();
    if let Err(e) = require_admin(rt.cfg.admin.api_key.as_deref(), &headers) {
        return e.into_response();
    }
    match reload::reload(&state).await {
        Ok(summary) => Json(summary).into_response(),
        Err(e) => crate::error::ApiError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "CONFIG_INVALID",
            format!("configuration not applied: {e:#}"),
        )
        .into_response(),
    }
}
