//! The `/process` contract: one endpoint, direction decided by `payload_id`.
//!
//! | state for payload_id | payload           | action                          |
//! |----------------------|-------------------|---------------------------------|
//! | none                 | any               | mask, remember                  |
//! | known                | == original       | retried mask: same mask again   |
//! | known                | == our mask       | demask by position              |
//! | known                | anything else     | demask by substitution (LLM)    |
//!
//! When the store cannot tell which state the payload_id is in, or cannot
//! record a new mapping, the request is answered with `429 STORE_BUSY` and
//! `Retry-After: 1` instead of being guessed as masking: the checker retries,
//! and no mask is ever returned where the original was expected.

use super::{count_types, mask_text, parse_json, Done};
use crate::auth::{resolve_system, AuthOptions};
use crate::error::{ApiError, ErrorResponse};
use crate::logging::{duration_ms, logged, request_id, types_summary};
use crate::metrics::{count_tokens, observe_pii, observe_request, observe_stage, observe_tokens};
use crate::state::AppState;
use crate::store::{mapping_key, now_secs, sha256_hex, Mapping, MappingStore};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;
use utoipa::ToSchema;

#[derive(Deserialize, ToSchema)]
#[schema(example = json!({
    "payload": "Клиент Иванов Иван Иванович, паспорт 4509 123456",
    "payload_id": "demo-1"
}))]
pub struct ProcessRequest {
    /// Текст, который нужно замаскировать, либо ранее выданная маска / изменённый
    /// моделью текст — для обратного восстановления.
    pub payload: String,
    /// Идентификатор пары маскирование/демаскирование. Один и тот же `payload_id`
    /// связывает оба направления одного диалога.
    pub payload_id: String,
}

#[derive(Serialize, ToSchema)]
pub struct ProcessResponse {
    /// Замаскированный или восстановленный текст — направление см. в заголовке
    /// `X-Maskarad-Direction`.
    pub result: String,
}

/// Контракт проверяющей системы: маскирование и демаскирование одним эндпоинтом.
///
/// Направление определяется состоянием `payload_id` в хранилище соответствий, а не
/// отдельным полем запроса: первый запрос с новым `payload_id` маскирует, повтор с тем
/// же текстом идемпотентно возвращает ту же маску, запрос с ранее выданной маской
/// демаскирует по позициям, изменённый моделью текст демаскируется подстановкой масок.
/// Доступен без ключа анонимной системе `alfasonar` (`auth.anonymous: true` в
/// конфигурации); можно передать `X-API-Key`/`Authorization: Bearer`, чтобы работать от
/// имени другой системы.
#[utoipa::path(
    post,
    path = "/process",
    tag = "contract",
    request_body(
        content = ProcessRequest,
        description = "Текст и идентификатор пары маскирование/демаскирование",
        content_type = "application/json"
    ),
    responses(
        (status = 200, description = "Результат: замаскированный или восстановленный текст",
            body = ProcessResponse,
            headers(
                ("X-Request-Id" = String, description = "Идентификатор запроса — тот же, что в логах сервиса"),
                ("X-Maskarad-Direction" = String, description = "`mask` или `demask` — какое направление применено"),
                ("X-Maskarad-Degraded" = String, description = "Присутствует со значением `store`, если хранилище соответствий деградировало (Redis недоступен) и ответ мог обслуживаться из L1-кэша реплики"),
            ),
            examples(
                ("Маскирование" = (summary = "Первый запрос с новым payload_id", value = json!({"result": "Клиент И. И. И., паспорт 45** ****56"}))),
                ("Демаскирование" = (summary = "Запрос с ранее выданной маской", value = json!({"result": "Клиент Иванов Иван Иванович, паспорт 4509 123456"}))),
            )
        ),
        (status = 400, description = "Невалидный JSON или пустой `payload_id` (код `INVALID_REQUEST`)", body = ErrorResponse),
        (status = 401, description = "Передан `X-API-Key`/`Bearer`, но ключ не распознан (код `UNAUTHORIZED`)", body = ErrorResponse),
        (status = 403, description = "Система выключена (`SYSTEM_DISABLED`) или ей запрещено демаскирование (`DEMASK_FORBIDDEN`)", body = ErrorResponse),
        (status = 413, description = "Тело запроса больше `server.max_body_bytes` — стандартный ответ axum (текст, не единый JSON-формат ошибок)"),
        (status = 429, description = "`RATE_LIMITED` — перегрузка: нет места в полосе допуска (адаптивный лимит коротких запросов, полоса больших текстов или `server.max_inflight`), либо `STORE_BUSY` — хранилище соответствий не ответило и состояние `payload_id` неизвестно (маска не выдаётся вместо оригинала); в обоих случаях запрос стоит повторить",
            body = ErrorResponse,
            headers(
                ("Retry-After" = String, description = "Через сколько секунд повторить запрос (всегда `1`)"),
            )
        ),
        (status = 503, description = "Обработка не уложилась в `server.request_timeout_ms` — ответ tower-http с пустым телом, не в едином формате ошибок"),
    ),
    security(
        (),
        ("ApiKeyHeader" = []),
        ("BearerAuth" = []),
    )
)]
pub async fn process(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    let rid = request_id();
    match handle(&state, &headers, &body, &rid, started).await {
        Ok(done) => {
            observe_request(
                "/process",
                &done.system,
                done.direction,
                200,
                done.elapsed.as_secs_f64(),
            );
            let mut r = (StatusCode::OK, Json(done.body)).into_response();
            let h = r.headers_mut();
            h.insert("X-Request-Id", rid.parse().expect("hex"));
            h.insert(
                "X-Maskarad-Direction",
                done.direction.parse().expect("ascii"),
            );
            if state.store.is_degraded() {
                h.insert("X-Maskarad-Degraded", "store".parse().expect("ascii"));
            }
            logged(r)
        }
        Err(e) => {
            let elapsed = started.elapsed();
            observe_request(
                "/process",
                "-",
                "error",
                e.status.as_u16(),
                elapsed.as_secs_f64(),
            );
            tracing::warn!(request_id = %rid, route = "/process", code = e.code, status = e.status.as_u16(), duration_ms = duration_ms(elapsed), "request rejected");
            logged(e.with_request_id(&rid))
        }
    }
}

async fn handle(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
    rid: &str,
    started: Instant,
) -> Result<Done<ProcessResponse>, ApiError> {
    let rt = state.runtime();
    let req: ProcessRequest = parse_json(body)?;
    if req.payload_id.trim().is_empty() {
        return Err(ApiError::invalid("payload_id must not be empty"));
    }
    let system = resolve_system(
        &rt.engine,
        headers,
        AuthOptions {
            allow_anonymous: true,
            override_system: None,
            allow_override: false,
        },
    )?;
    let system_id = system.id().to_string();
    let key = mapping_key(&system_id, &req.payload_id);
    let pid_hash = &sha256_hex(&req.payload_id)[..12];

    let t = Instant::now();
    let existing = match state.store.get(&key).await {
        Ok(m) => m,
        Err(e) => {
            // Unknown state: neither masking nor demasking is safe to guess.
            tracing::warn!(request_id = rid, error = %e, "store read failed, asking the caller to retry");
            return Err(ApiError::store_busy());
        }
    };
    observe_stage("store_get", t.elapsed().as_secs_f64());

    let tokens = count_tokens(&req.payload);
    match existing {
        None => {
            let t = Instant::now();
            let result = mask_text(Arc::clone(&rt), system, req.payload.clone()).await?;
            observe_stage("detect_mask", t.elapsed().as_secs_f64());
            let mapping = Mapping {
                system: system_id.clone(),
                original_hash: sha256_hex(&req.payload),
                masked_hash: sha256_hex(&result.masked),
                entries: result.entries.clone(),
                created_at: now_secs(),
            };
            let t = Instant::now();
            if let Err(e) = state.store.put(&key, &mapping).await {
                // Not recorded anywhere: a retry masks again to the same result.
                tracing::warn!(request_id = rid, error = %e, "store write failed, asking the caller to retry");
                return Err(ApiError::store_busy());
            }
            observe_stage("store_put", t.elapsed().as_secs_f64());
            let counts = count_types(&result);
            for (ty, n) in &counts {
                observe_pii(&system_id, ty.as_str(), *n);
            }
            observe_tokens("mask", tokens);
            let elapsed = started.elapsed();
            tracing::info!(
                request_id = rid,
                route = "/process",
                direction = "mask",
                system = %system_id,
                payload_id_hash = pid_hash,
                text_bytes = req.payload.len(),
                tokens,
                entities = result.entries.len(),
                types = %types_summary(&counts),
                store = state.store.backend(),
                degraded = state.store.is_degraded(),
                status = 200u16,
                duration_ms = duration_ms(elapsed),
                "masked"
            );
            Ok(Done {
                body: ProcessResponse {
                    result: result.masked,
                },
                system: system_id,
                direction: "mask",
                elapsed,
            })
        }
        Some(mapping) => {
            let hash = sha256_hex(&req.payload);
            if hash == mapping.original_hash {
                // Retry of the masking step: answer exactly as before.
                let masked = maskarad_core::Engine::rebuild_masked(&req.payload, &mapping.entries)
                    .ok_or_else(ApiError::internal)?;
                let elapsed = started.elapsed();
                tracing::info!(request_id = rid, route = "/process", direction = "mask_retry", system = %system_id, payload_id_hash = pid_hash, status = 200u16, duration_ms = duration_ms(elapsed), "masked (retry)");
                return Ok(Done {
                    body: ProcessResponse { result: masked },
                    system: system_id,
                    direction: "mask",
                    elapsed,
                });
            }
            if !system.def.demask {
                return Err(ApiError::demask_forbidden(&system_id));
            }
            let t = Instant::now();
            let exact = hash == mapping.masked_hash;
            let restored = rt.engine.demask(&req.payload, &mapping.entries, exact);
            observe_stage("demask", t.elapsed().as_secs_f64());
            observe_tokens("demask", tokens);
            let elapsed = started.elapsed();
            tracing::info!(
                request_id = rid,
                route = "/process",
                direction = if exact { "demask_exact" } else { "demask_substitute" },
                system = %system_id,
                payload_id_hash = pid_hash,
                text_bytes = req.payload.len(),
                tokens,
                entries = mapping.entries.len(),
                status = 200u16,
                duration_ms = duration_ms(elapsed),
                "demasked"
            );
            Ok(Done {
                body: ProcessResponse { result: restored },
                system: system_id,
                direction: "demask",
                elapsed,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::capture;
    use crate::state::AppState;
    use crate::store::testing::{Failure, FlakyStore};

    const CONFIG: &str = "systems:\n  - id: alfasonar\n    auth: { anonymous: true }\n    pii_types: all\n    demask: true\n    profile: reference\n";
    const TEXT: &str = "Клиент Иванов Иван Иванович, паспорт 4509 123456";
    const MASKED: &str = "Клиент И. И. И., паспорт 45** ****56";

    /// The service state over a tiered store whose primary is `primary`.
    fn app_state(primary: Arc<FlakyStore>) -> AppState {
        AppState::for_tests(CONFIG, primary)
    }

    async fn call(
        state: &AppState,
        payload: &str,
        payload_id: &str,
    ) -> (StatusCode, HeaderMap, serde_json::Value) {
        let body = serde_json::json!({ "payload": payload, "payload_id": payload_id }).to_string();
        let resp = process(State(state.clone()), HeaderMap::new(), Bytes::from(body)).await;
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        (
            status,
            headers,
            serde_json::from_slice(&bytes).expect("json"),
        )
    }

    fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn assert_store_busy(status: StatusCode, headers: &HeaderMap, body: &serde_json::Value) {
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(header(headers, "Retry-After"), Some("1"));
        assert_eq!(body["error"]["code"], "STORE_BUSY");
        assert!(
            body.get("result").is_none(),
            "no mask must be returned: {body}"
        );
        assert!(body["error"]["request_id"].is_string());
        assert_eq!(header(headers, "X-Maskarad-Direction"), None);
    }

    #[tokio::test]
    async fn mask_retry_and_demask_through_a_healthy_store() {
        let primary = Arc::new(FlakyStore::new());
        let state = app_state(primary.clone());

        let (status, headers, body) = call(&state, TEXT, "p1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"], MASKED);
        assert_eq!(header(&headers, "X-Maskarad-Direction"), Some("mask"));
        assert_eq!(header(&headers, "X-Maskarad-Degraded"), None);
        assert_eq!(primary.puts(), 1);

        let (status, _, body) = call(&state, TEXT, "p1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"], MASKED, "retried mask answers the same");
        assert_eq!(primary.puts(), 1, "a retried mask is not written again");

        let (status, headers, body) = call(&state, MASKED, "p1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"], TEXT);
        assert_eq!(header(&headers, "X-Maskarad-Direction"), Some("demask"));
    }

    /// One line per request with its status and duration; the text, the
    /// answer and the raw payload_id stay out of the log.
    #[tokio::test]
    async fn request_lines_carry_status_and_duration_but_no_text() {
        const PAYLOAD_ID: &str = "pid-log-check";
        let (_guard, logs) = capture::start();
        let state = app_state(Arc::new(FlakyStore::new()));

        call(&state, TEXT, PAYLOAD_ID).await;
        call(&state, TEXT, PAYLOAD_ID).await;
        call(&state, MASKED, PAYLOAD_ID).await;
        let (status, headers, body) = call(&state, TEXT, " ").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        for (message, status) in [
            ("masked", 200),
            ("masked (retry)", 200),
            ("demasked", 200),
            ("request rejected", 400),
        ] {
            let lines = logs.with_message(message);
            assert_eq!(lines.len(), 1, "one `{message}` line");
            let line = &lines[0];
            assert_eq!(line["route"], "/process", "{line}");
            assert_eq!(line["status"], status, "{line}");
            assert!(
                line["duration_ms"].as_f64().is_some_and(|ms| ms >= 0.0),
                "{line}"
            );
        }
        let rejected = &logs.with_message("request rejected")[0];
        assert_eq!(rejected["code"], "INVALID_REQUEST");
        assert_eq!(rejected["request_id"], body["error"]["request_id"]);
        assert_eq!(
            header(&headers, "X-Request-Id"),
            rejected["request_id"].as_str()
        );

        let raw = logs.raw();
        for secret in [
            "Иванов",
            "Клиент",
            "паспорт",
            "4509 123456",
            "И. И. И.",
            "45** ****56",
            PAYLOAD_ID,
        ] {
            assert!(!raw.contains(secret), "`{secret}` in the log:\n{raw}");
        }
    }

    #[tokio::test]
    async fn store_read_error_is_store_busy_not_a_mask() {
        let primary = Arc::new(FlakyStore::new());
        let state = app_state(primary.clone());
        primary.fail_get(true);

        let (status, headers, body) = call(&state, TEXT, "p1").await;
        assert_store_busy(status, &headers, &body);
        assert_eq!(
            primary.puts(),
            0,
            "nothing is written while the state is unknown"
        );
    }

    /// The production failure: the mapping was written by another replica,
    /// this replica's cache is empty and its Redis read times out. The demask
    /// request must not be answered with a mask.
    #[tokio::test]
    async fn store_read_error_on_a_demask_request_is_store_busy() {
        let primary = Arc::new(FlakyStore::new());
        let other_replica = app_state(primary.clone());
        let (status, _, _) = call(&other_replica, TEXT, "p1").await;
        assert_eq!(status, StatusCode::OK);

        let this_replica = app_state(primary.clone());
        primary.fail_get(true);
        let (status, headers, body) = call(&this_replica, MASKED, "p1").await;
        assert_store_busy(status, &headers, &body);
        assert_eq!(primary.puts(), 1, "the existing mapping is not overwritten");

        primary.fail_get(false);
        let (status, headers, body) = call(&this_replica, MASKED, "p1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"], TEXT);
        assert_eq!(header(&headers, "X-Maskarad-Direction"), Some("demask"));
    }

    /// A concurrent retry wrote the mapping between our read and our write:
    /// the mask is a pure function of the text, so the answer is still ours.
    #[tokio::test]
    async fn a_mapping_written_meanwhile_is_not_an_error() {
        let primary = Arc::new(FlakyStore::new());
        let state = app_state(primary.clone());
        primary.fail_put(Failure::Exists);

        let (status, headers, body) = call(&state, TEXT, "p1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"], MASKED);
        assert_eq!(header(&headers, "X-Maskarad-Direction"), Some("mask"));
    }

    #[tokio::test]
    async fn store_write_error_is_store_busy_and_the_retry_succeeds() {
        let primary = Arc::new(FlakyStore::new());
        let state = app_state(primary.clone());
        primary.fail_put(Failure::Unavailable);

        let (status, headers, body) = call(&state, TEXT, "p1").await;
        assert_store_busy(status, &headers, &body);

        primary.fail_put(Failure::None);
        let (status, headers, body) = call(&state, TEXT, "p1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"], MASKED);
        assert_eq!(header(&headers, "X-Maskarad-Direction"), Some("mask"));

        let (status, _, body) = call(&state, MASKED, "p1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["result"], TEXT);
    }
}
