//! OpenAI-compatible proxy: `messages[].content` is masked with shared
//! placeholder identities, the request goes upstream, `choices[]` come back
//! demasked. A consumer only changes its `base_url`.

use super::{parse_json, Done};
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
use maskarad_core::{MaskEntry, MaskState, PiiType};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Instant;
use utoipa::ToSchema;

/// Сообщение диалога в формате OpenAI Chat Completions. `content` — строка или
/// массив частей `{"type":"text","text":"..."}` (мультимодальный формат); маскируются
/// только текстовые части, остальные поля сообщения проксируются как есть.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ChatMessage {
    pub role: String,
    pub content: Value,
}

/// Запрос в формате OpenAI Chat Completions. Это документная схема того, что видит и
/// трогает Maskarad (`messages`, `maskarad_system`); остальные поля стандарта
/// (`temperature`, `max_tokens`, `tools`, …) принимаются и проксируются в `llm.base_url`
/// без изменений — реальный обработчик разбирает тело как свободный JSON, а не строго
/// по этой схеме.
#[derive(Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "model": "gpt-4o-mini",
    "messages": [
        {"role": "user", "content": "Иванов Иван Иванович, паспорт 4509 123456"}
    ]
}))]
pub struct ChatCompletionsRequest {
    pub model: Option<String>,
    pub messages: Vec<ChatMessage>,
    /// Только в демо-режиме (`ui.allow_system_override: true`): имя системы вместо
    /// `X-API-Key`. Поле вырезается перед отправкой апстриму.
    #[serde(default)]
    pub maskarad_system: Option<String>,
}

/// Маскирует `messages[].content`, вызывает `llm.base_url` (формат OpenAI Chat
/// Completions) и демаскирует `choices[]` в ответе, если системе разрешено
/// демаскирование. Потребитель меняет только `base_url` в своём OpenAI-клиенте — набор
/// полей запроса и ответа не меняется, кроме добавленного поля `maskarad` в ответе.
/// Плейсхолдеры общие на весь диалог: модель, увидевшая `<FIO_1>` в одном сообщении,
/// использует тот же плейсхолдер в остальных.
#[utoipa::path(
    post,
    path = "/v1/chat/completions",
    tag = "llm",
    request_body = ChatCompletionsRequest,
    responses(
        (status = 200,
            description = "Ответ апстрима (формат OpenAI Chat Completions) с демаскированным `choices[].message.content` и добавленным полем `maskarad`",
            body = Value,
            headers(("X-Request-Id" = String, description = "Идентификатор запроса")),
            example = json!({
                "id": "chatcmpl-abc123",
                "object": "chat.completion",
                "model": "gpt-4o-mini",
                "choices": [
                    {"index": 0, "message": {"role": "assistant", "content": "Здравствуйте, Иванов Иван Иванович! Паспорт 4509 123456 принят в обработку."}, "finish_reason": "stop"}
                ],
                "usage": {"prompt_tokens": 42, "completion_tokens": 18, "total_tokens": 60},
                "maskarad": {"payload_id": "b6f1c2d3e4a57f08", "entities": 2, "demasked": true}
            })
        ),
        (status = 400, description = "Невалидный JSON или нет массива `messages` (`INVALID_REQUEST`)", body = ErrorResponse),
        (status = 401, description = "Ключ не передан или не распознан (`UNAUTHORIZED`)", body = ErrorResponse),
        (status = 403, description = "Система выключена (`SYSTEM_DISABLED`) или в демо-режиме указано неизвестное имя системы (`UNKNOWN_SYSTEM`, 404)", body = ErrorResponse),
        (status = 413, description = "Тело запроса больше `server.max_body_bytes` — стандартный ответ axum"),
        (status = 429, description = "`RATE_LIMITED` — превышен `server.max_inflight`", body = ErrorResponse,
            headers(("Retry-After" = String, description = "Через сколько секунд повторить (`1`)"))),
        (status = 501, description = "`llm.base_url` не настроен (`LLM_NOT_CONFIGURED`)", body = ErrorResponse),
        (status = 502, description = "Апстрим недоступен, вернул ошибку, или сработал circuit breaker после серии отказов (`LLM_UNAVAILABLE` / `LLM_ERROR`)", body = ErrorResponse),
        (status = 503, description = "Обработка не уложилась в `server.request_timeout_ms` — ответ tower-http с пустым телом, не в едином формате ошибок"),
        (status = 504, description = "Апстрим не ответил за `llm.timeout_ms` (`LLM_TIMEOUT`)", body = ErrorResponse),
    ),
    security(
        ("ApiKeyHeader" = []),
        ("BearerAuth" = []),
    )
)]
pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let rid = request_id();
    match handle(&state, &headers, &body, &rid, started).await {
        Ok(done) => {
            observe_request(
                "/v1/chat/completions",
                &done.system,
                done.direction,
                200,
                done.elapsed.as_secs_f64(),
            );
            let mut r = (StatusCode::OK, Json(done.body)).into_response();
            r.headers_mut()
                .insert("X-Request-Id", rid.parse().expect("hex"));
            logged(r)
        }
        Err(e) => {
            let elapsed = started.elapsed();
            observe_request(
                "/v1/chat/completions",
                "-",
                "error",
                e.status.as_u16(),
                elapsed.as_secs_f64(),
            );
            tracing::warn!(request_id = %rid, route = "/v1/chat/completions", code = e.code, status = e.status.as_u16(), duration_ms = duration_ms(elapsed), "proxy request rejected");
            logged(e.with_request_id(&rid))
        }
    }
}

fn mask_content(
    content: &mut Value,
    rt: &crate::state::Runtime,
    system: &maskarad_core::CompiledSystem,
    st: &mut MaskState,
    entries: &mut Vec<MaskEntry>,
    counts: &mut Vec<(PiiType, usize)>,
    tokens: &mut usize,
) {
    match content {
        Value::String(s) => {
            *tokens += count_tokens(s);
            let r = rt.engine.mask_with_state(s, system, st);
            for (ty, n) in r.type_counts() {
                match counts.iter_mut().find(|(t, _)| *t == ty) {
                    Some((_, c)) => *c += n,
                    None => counts.push((ty, n)),
                }
            }
            entries.extend(r.entries);
            *s = r.masked;
        }
        Value::Array(parts) => {
            for part in parts {
                if let Some(text) = part.get_mut("text") {
                    mask_content(text, rt, system, st, entries, counts, tokens);
                }
            }
        }
        _ => {}
    }
}

async fn handle(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
    rid: &str,
    started: Instant,
) -> Result<Done<Value>, ApiError> {
    let rt = state.runtime();
    let mut req: Value = parse_json(body)?;
    let override_system: Option<String> = req
        .get("maskarad_system")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let system = resolve_system(
        &rt.engine,
        headers,
        AuthOptions {
            allow_anonymous: false,
            override_system: override_system.as_deref(),
            allow_override: rt.cfg.ui.allow_system_override,
        },
    )?;
    let system_id = system.id().to_string();
    let Some(messages) = req.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return Err(ApiError::invalid("`messages` array is required"));
    };
    let mut st = MaskState::default();
    let mut entries = Vec::new();
    let mut counts = Vec::new();
    let mut tokens = 0usize;
    let t = Instant::now();
    for msg in messages.iter_mut() {
        if let Some(content) = msg.get_mut("content") {
            mask_content(
                content,
                &rt,
                system,
                &mut st,
                &mut entries,
                &mut counts,
                &mut tokens,
            );
        }
    }
    observe_stage("detect_mask", t.elapsed().as_secs_f64());
    observe_tokens("mask", tokens);
    for (ty, n) in &counts {
        observe_pii(&system_id, ty.as_str(), *n);
    }
    if let Some(obj) = req.as_object_mut() {
        obj.remove("maskarad_system");
        // Streaming is answered as a whole response in this version.
        obj.insert("stream".into(), Value::Bool(false));
    }
    let payload_id = request_id();
    let mapping = Mapping {
        system: system_id.clone(),
        original_hash: String::new(),
        masked_hash: String::new(),
        entries: entries.clone(),
        created_at: now_secs(),
    };
    if let Err(e) = state
        .store
        .put(&mapping_key(&system_id, &payload_id), &mapping)
        .await
    {
        tracing::warn!(request_id = rid, error = %e, "mapping not stored; demasking continues in-request");
    }
    let mut answer = state.llm.chat_completions(&rt.cfg.llm, &req).await?;
    let mut demask_tokens = 0usize;
    if system.def.demask {
        let t = Instant::now();
        if let Some(choices) = answer.get_mut("choices").and_then(|c| c.as_array_mut()) {
            for choice in choices {
                if let Some(Value::String(content)) =
                    choice.get_mut("message").and_then(|m| m.get_mut("content"))
                {
                    demask_tokens += count_tokens(content);
                    *content = rt.engine.demask(content, &entries, false);
                }
            }
        }
        observe_stage("demask", t.elapsed().as_secs_f64());
        observe_tokens("demask", demask_tokens);
    }
    if let Some(obj) = answer.as_object_mut() {
        obj.insert("maskarad".into(), serde_json::json!({ "payload_id": payload_id, "entities": entries.len(), "demasked": system.def.demask }));
    }
    let elapsed = started.elapsed();
    tracing::info!(request_id = rid, route = "/v1/chat/completions", system = %system_id, payload_id_hash = &sha256_hex(&payload_id)[..12], tokens_in = tokens, tokens_out = demask_tokens, entities = entries.len(), types = %types_summary(&counts), demasked = system.def.demask, status = 200u16, duration_ms = duration_ms(elapsed), "proxied");
    Ok(Done {
        body: answer,
        system: system_id,
        direction: "proxy",
        elapsed,
    })
}
