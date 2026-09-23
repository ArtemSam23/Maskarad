//! Unified error responses: `{"error": {"code", "message", "request_id"}}`.
//! Messages never include request content.

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retry_after: Option<u64>,
}

#[derive(Serialize)]
struct Body<'a> {
    error: BodyInner<'a>,
}

#[derive(Serialize)]
struct BodyInner<'a> {
    code: &'a str,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<&'a str>,
}

/// Тело ошибки для спеки OpenAPI. Повторяет форму `Body`/`BodyInner` выше как
/// owned-тип: `ToSchema` не выводит схему по заимствованным `Body<'a>`, а
/// сериализация на рантайме по-прежнему идёт через `Body` без лишних копий.
#[derive(Serialize, ToSchema)]
#[schema(example = json!({
    "error": {
        "code": "STORE_BUSY",
        "message": "mapping store is busy, retry later",
        "request_id": "4f1c2a9b7d3e8f60"
    }
}))]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Serialize, ToSchema)]
pub struct ErrorBody {
    /// Машиночитаемый код: INVALID_REQUEST, UNAUTHORIZED, SYSTEM_DISABLED,
    /// DEMASK_FORBIDDEN, MAPPING_NOT_FOUND, UNKNOWN_SYSTEM, RATE_LIMITED,
    /// STORE_BUSY, STORE_UNAVAILABLE, LLM_NOT_CONFIGURED, LLM_UNAVAILABLE,
    /// LLM_ERROR, LLM_TIMEOUT, INTERNAL — см. описание по кодам ответов ниже.
    pub code: String,
    /// Человекочитаемое описание; никогда не содержит текста запроса.
    pub message: String,
    /// 16 hex-символов; тот же id, что в заголовке `X-Request-Id` и в строке
    /// лога этого запроса.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "4f1c2a9b7d3e8f60")]
    pub request_id: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "INVALID_REQUEST", message)
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "missing or unknown API key (X-API-Key)",
        )
    }

    pub fn system_disabled(id: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "SYSTEM_DISABLED",
            format!("system `{id}` is disabled"),
        )
    }

    pub fn demask_forbidden(id: &str) -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "DEMASK_FORBIDDEN",
            format!("system `{id}` is not allowed to demask"),
        )
    }

    pub fn mapping_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "MAPPING_NOT_FOUND",
            "no mapping for this payload_id (expired or never masked)",
        )
    }

    pub fn rate_limited() -> Self {
        let mut e = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "RATE_LIMITED",
            "too many requests in flight, retry later",
        );
        e.retry_after = Some(1);
        e
    }

    /// The mapping store did not answer in time on `/process`. The state of
    /// the payload_id is unknown, so neither direction is taken: the caller
    /// retries (the contract allows three attempts) instead of receiving a
    /// mask where it may have expected the original.
    pub fn store_busy() -> Self {
        let mut e = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "STORE_BUSY",
            "mapping store is busy, retry later",
        );
        e.retry_after = Some(1);
        e
    }

    pub fn store_unavailable(detail: &str) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "STORE_UNAVAILABLE",
            format!("mapping store unavailable: {detail}"),
        )
    }

    pub fn llm_unavailable(detail: &str) -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "LLM_UNAVAILABLE",
            format!("upstream LLM unavailable: {detail}"),
        )
    }

    pub fn llm_timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "LLM_TIMEOUT",
            "upstream LLM did not answer in time",
        )
    }

    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            "internal error",
        )
    }

    /// The id goes to the body and to `X-Request-Id`, the header successful
    /// responses carry, so a caller finds the log line either way.
    pub fn with_request_id(self, request_id: &str) -> Response {
        let body = Body {
            error: BodyInner {
                code: self.code,
                message: &self.message,
                request_id: Some(request_id),
            },
        };
        let mut resp = (self.status, Json(body)).into_response();
        if let Ok(v) = HeaderValue::from_str(request_id) {
            resp.headers_mut().insert("X-Request-Id", v);
        }
        if let Some(secs) = self.retry_after {
            resp.headers_mut()
                .insert("Retry-After", secs.to_string().parse().expect("ascii"));
        }
        resp
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Body {
            error: BodyInner {
                code: self.code,
                message: &self.message,
                request_id: None,
            },
        };
        let mut resp = (self.status, Json(body)).into_response();
        if let Some(secs) = self.retry_after {
            resp.headers_mut()
                .insert("Retry-After", secs.to_string().parse().expect("ascii"));
        }
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn parts(resp: Response) -> (StatusCode, Option<String>, serde_json::Value) {
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get("Retry-After")
            .map(|v| v.to_str().unwrap().to_string());
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, retry_after, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn store_busy_is_a_retryable_429() {
        let (status, retry_after, body) = parts(ApiError::store_busy().into_response()).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry_after.as_deref(), Some("1"));
        assert_eq!(body["error"]["code"], "STORE_BUSY");
        assert!(body["error"].get("request_id").is_none());

        let resp = ApiError::store_busy().with_request_id("abc123");
        assert_eq!(resp.headers()["X-Request-Id"], "abc123");
        let (status, retry_after, body) = parts(resp).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry_after.as_deref(), Some("1"));
        assert_eq!(body["error"]["code"], "STORE_BUSY");
        assert_eq!(body["error"]["request_id"], "abc123");
    }

    #[tokio::test]
    async fn store_unavailable_stays_a_503_without_retry_after() {
        let (status, retry_after, body) =
            parts(ApiError::store_unavailable("redis timeout").into_response()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(retry_after, None);
        assert_eq!(body["error"]["code"], "STORE_UNAVAILABLE");
    }
}
