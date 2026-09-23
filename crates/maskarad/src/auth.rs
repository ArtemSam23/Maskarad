//! Consumer system resolution: API key (`X-API-Key` or `Authorization:
//! Bearer`), the anonymous system for the `/process` contract, or — in demo
//! mode — an explicit system name.

use crate::error::ApiError;
use axum::http::HeaderMap;
use maskarad_core::{CompiledSystem, Engine};

pub fn api_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub struct AuthOptions<'a> {
    pub allow_anonymous: bool,
    pub override_system: Option<&'a str>,
    pub allow_override: bool,
}

pub fn resolve_system<'e>(
    engine: &'e Engine,
    headers: &HeaderMap,
    opts: AuthOptions<'_>,
) -> Result<&'e CompiledSystem, ApiError> {
    if let Some(key) = api_key(headers) {
        return match engine.system_by_key(&key) {
            Some(s) if s.def.enabled => Ok(s),
            Some(s) => Err(ApiError::system_disabled(s.id())),
            None => Err(ApiError::unauthorized()),
        };
    }
    if let (true, Some(id)) = (opts.allow_override, opts.override_system) {
        return match engine.system(id) {
            Some(s) if s.def.enabled => Ok(s),
            Some(s) => Err(ApiError::system_disabled(s.id())),
            None => Err(ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "UNKNOWN_SYSTEM",
                format!("unknown system `{id}`"),
            )),
        };
    }
    if opts.allow_anonymous {
        if let Some(s) = engine.anonymous_system() {
            return Ok(s);
        }
    }
    Err(ApiError::unauthorized())
}

pub fn require_admin(admin_key: Option<&str>, headers: &HeaderMap) -> Result<(), ApiError> {
    match (admin_key, api_key(headers)) {
        (Some(expected), Some(given)) if !expected.is_empty() && expected == given => Ok(()),
        (Some(expected), _) if !expected.is_empty() => Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "ADMIN_KEY_REQUIRED",
            "admin API key required",
        )),
        _ => Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "ADMIN_DISABLED",
            "admin endpoints are disabled (set admin.api_key)",
        )),
    }
}
