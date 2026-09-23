//! Public API for integrations: mask, demask, detect, plus read-only views
//! of the configuration for operators and the demo UI.

use super::{count_types, mask_text, parse_json, Done};
use crate::auth::{require_admin, resolve_system, AuthOptions};
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
    "text": "Иванов Иван Иванович, паспорт 4509 123456",
    "payload_id": "demo-2"
}))]
pub struct MaskRequest {
    pub text: String,
    /// Не задан — сервис сгенерирует случайный. Тот же `payload_id` нужен потом для `/v1/demask`.
    #[serde(default)]
    pub payload_id: Option<String>,
    /// Только в демо-режиме (`ui.allow_system_override: true`): имя системы вместо `X-API-Key`.
    #[serde(default)]
    pub system: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct EntityView {
    /// Идентификатор типа ПДн (`FIO`, `PASSPORT`, `CARD_NUMBER`, …) — справочник
    /// расширяем через `pii_types` в `config/maskarad.yaml` без правки кода.
    #[serde(rename = "type")]
    pub ty: String,
    /// Байтовое смещение начала сущности в исходном тексте (UTF-8, не символьный индекс).
    pub start: usize,
    /// Байтовое смещение конца сущности (не включая).
    pub end: usize,
    /// Уверенность детектора, 0..=1.
    pub confidence: f32,
    /// Короткое PII-free описание основания решения, например `surname+first+patronymic`.
    pub evidence: String,
    /// Чем сущность заменена в `masked`. Отсутствует в ответе `/v1/detect` — там текст не меняется.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mask: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[schema(example = json!({
    "payload_id": "demo-2",
    "system": "support-chatbot",
    "masked": "<FIO_1>, паспорт <PASSPORT_1>",
    "entities": [
        {"type": "FIO", "start": 0, "end": 38, "confidence": 0.97, "evidence": "surname+first+patronymic", "mask": "<FIO_1>"},
        {"type": "PASSPORT", "start": 55, "end": 66, "confidence": 0.9, "evidence": "pattern+context", "mask": "<PASSPORT_1>"}
    ],
    "degraded": false
}))]
pub struct MaskResponse {
    pub payload_id: String,
    pub system: String,
    pub masked: String,
    pub entities: Vec<EntityView>,
    /// `true`, если хранилище соответствий деградировало (см. «Надёжность и деградация» в README).
    pub degraded: bool,
}

#[derive(Deserialize, ToSchema)]
#[schema(example = json!({
    "text": "<FIO_1>, паспорт <PASSPORT_1>",
    "payload_id": "demo-2"
}))]
pub struct DemaskRequest {
    /// Текст с плейсхолдерами/маской — как правило, ответ модели, прошедший через `/v1/mask`.
    pub text: String,
    pub payload_id: String,
    #[serde(default)]
    pub system: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[schema(example = json!({
    "text": "Иванов Иван Иванович, паспорт 4509 123456",
    "entries": 2
}))]
pub struct DemaskResponse {
    pub text: String,
    /// Число сущностей, использованных при восстановлении (не сам список).
    pub entries: usize,
}

#[derive(Deserialize, ToSchema)]
#[schema(example = json!({"text": "Иванов Иван Иванович, паспорт 4509 123456"}))]
pub struct DetectRequest {
    pub text: String,
    /// Только в демо-режиме (`ui.allow_system_override: true`): имя системы вместо `X-API-Key`.
    #[serde(default)]
    pub system: Option<String>,
}

#[derive(Serialize, ToSchema)]
#[schema(example = json!({
    "system": "analytics",
    "entities": [
        {"type": "FIO", "start": 0, "end": 38, "confidence": 0.97, "evidence": "surname+first+patronymic"},
        {"type": "PASSPORT", "start": 55, "end": 66, "confidence": 0.9, "evidence": "pattern+context"}
    ]
}))]
pub struct DetectResponse {
    pub system: String,
    pub entities: Vec<EntityView>,
}

fn finish<T: Serialize>(
    route: &'static str,
    started: Instant,
    rid: &str,
    result: Result<Done<T>, ApiError>,
) -> Response {
    match result {
        Ok(done) => {
            observe_request(
                route,
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
                route,
                "-",
                "error",
                e.status.as_u16(),
                elapsed.as_secs_f64(),
            );
            tracing::warn!(request_id = %rid, route, code = e.code, status = e.status.as_u16(), duration_ms = duration_ms(elapsed), "request rejected");
            logged(e.with_request_id(rid))
        }
    }
}

fn auth_opts<'a>(state: &AppState, system: Option<&'a str>) -> AuthOptions<'a> {
    AuthOptions {
        allow_anonymous: false,
        override_system: system,
        allow_override: state.runtime().cfg.ui.allow_system_override,
    }
}

/// Маскирует текст и запоминает соответствие для последующего `/v1/demask`.
///
/// В отличие от `/process`, направление тут не угадывается: `/v1/mask` всегда
/// маскирует. Каждой сущности присвоен плейсхолдер вида `<TYPE_N>` при профиле
/// `placeholder` (типовой выбор для текста, уходящего в LLM) — модель копирует
/// плейсхолдер в ответ, а `/v1/demask` подставляет исходное значение обратно.
#[utoipa::path(
    post,
    path = "/v1/mask",
    tag = "v1",
    request_body = MaskRequest,
    responses(
        (status = 200, description = "Замаскированный текст и список найденных сущностей", body = MaskResponse,
            headers(("X-Request-Id" = String, description = "Идентификатор запроса"))),
        (status = 400, description = "Невалидный JSON (`INVALID_REQUEST`)", body = ErrorResponse),
        (status = 401, description = "Ключ не передан или не распознан (`UNAUTHORIZED`)", body = ErrorResponse),
        (status = 403, description = "Система выключена (`SYSTEM_DISABLED`) или в демо-режиме указано неизвестное имя системы (`UNKNOWN_SYSTEM`, 404)", body = ErrorResponse),
        (status = 413, description = "Тело запроса больше `server.max_body_bytes` — стандартный ответ axum"),
        (status = 429, description = "`RATE_LIMITED` — перегрузка: нет места в полосе допуска (адаптивный лимит коротких запросов, полоса больших текстов или `server.max_inflight`)", body = ErrorResponse,
            headers(("Retry-After" = String, description = "Через сколько секунд повторить (`1`)"))),
        (status = 503, description = "`STORE_UNAVAILABLE` — хранилище соответствий не приняло запись, либо обработка не уложилась в `server.request_timeout_ms` (в этом случае — ответ tower-http с пустым телом, не JSON)", body = ErrorResponse),
    ),
    security(
        ("ApiKeyHeader" = []),
        ("BearerAuth" = []),
    )
)]
pub async fn mask(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    let rid = request_id();
    let result = async {
        let rt = state.runtime();
        let req: MaskRequest = parse_json(&body)?;
        let system = resolve_system(&rt.engine, &headers, auth_opts(&state, req.system.as_deref()))?;
        let system_id = system.id().to_string();
        let payload_id = req.payload_id.filter(|p| !p.trim().is_empty()).unwrap_or_else(request_id);
        let t = Instant::now();
        let result = mask_text(Arc::clone(&rt), system, req.text.clone()).await?;
        observe_stage("detect_mask", t.elapsed().as_secs_f64());
        let mapping = Mapping {
            system: system_id.clone(),
            original_hash: sha256_hex(&req.text),
            masked_hash: sha256_hex(&result.masked),
            entries: result.entries.clone(),
            created_at: now_secs(),
        };
        let t = Instant::now();
        state.store.put(&mapping_key(&system_id, &payload_id), &mapping).await.map_err(|e| ApiError::store_unavailable(&e.to_string()))?;
        observe_stage("store_put", t.elapsed().as_secs_f64());
        let counts = count_types(&result);
        for (ty, n) in &counts {
            observe_pii(&system_id, ty.as_str(), *n);
        }
        let tokens = count_tokens(&req.text);
        observe_tokens("mask", tokens);
        let entities = result
            .entities
            .iter()
            .zip(result.entries.iter())
            .map(|(e, m)| EntityView { ty: e.ty.to_string(), start: e.start, end: e.end, confidence: e.confidence, evidence: e.evidence.clone(), mask: Some(m.mask.clone()) })
            .collect();
        let elapsed = started.elapsed();
        tracing::info!(request_id = %rid, route = "/v1/mask", system = %system_id, payload_id_hash = &sha256_hex(&payload_id)[..12], text_bytes = req.text.len(), tokens, entities = result.entries.len(), types = %types_summary(&counts), status = 200u16, duration_ms = duration_ms(elapsed), "masked");
        let body = MaskResponse { payload_id, system: system_id.clone(), masked: result.masked, entities, degraded: state.store.is_degraded() };
        Ok(Done { body, system: system_id, direction: "mask", elapsed })
    }
    .await;
    finish("/v1/mask", started, &rid, result)
}

/// Восстанавливает исходный текст по маске, выданной ранее `/v1/mask`.
///
/// Точное совпадение с замаскированным текстом восстанавливается по сохранённым
/// позициям; изменённый моделью текст (переставлен порядок, другой регистр) —
/// подстановкой масок, от самых длинных плейсхолдеров к коротким. Доступно только
/// системам с `demask: true` в конфигурации.
#[utoipa::path(
    post,
    path = "/v1/demask",
    tag = "v1",
    request_body = DemaskRequest,
    responses(
        (status = 200, description = "Восстановленный текст", body = DemaskResponse,
            headers(("X-Request-Id" = String, description = "Идентификатор запроса"))),
        (status = 400, description = "Невалидный JSON (`INVALID_REQUEST`)", body = ErrorResponse),
        (status = 401, description = "Ключ не передан или не распознан (`UNAUTHORIZED`)", body = ErrorResponse),
        (status = 403, description = "Система выключена (`SYSTEM_DISABLED`), ей запрещено демаскирование (`DEMASK_FORBIDDEN`), либо в демо-режиме указано неизвестное имя системы (`UNKNOWN_SYSTEM`, 404)", body = ErrorResponse),
        (status = 404, description = "Нет записи для этого `payload_id` — истекла по TTL или никогда не маскировалась (`MAPPING_NOT_FOUND`)", body = ErrorResponse),
        (status = 413, description = "Тело запроса больше `server.max_body_bytes` — стандартный ответ axum"),
        (status = 429, description = "`RATE_LIMITED` — перегрузка: нет места в полосе допуска (адаптивный лимит коротких запросов, полоса больших текстов или `server.max_inflight`)", body = ErrorResponse,
            headers(("Retry-After" = String, description = "Через сколько секунд повторить (`1`)"))),
        (status = 503, description = "`STORE_UNAVAILABLE` — хранилище соответствий не ответило, либо обработка не уложилась в `server.request_timeout_ms` (ответ tower-http с пустым телом, не JSON)", body = ErrorResponse),
    ),
    security(
        ("ApiKeyHeader" = []),
        ("BearerAuth" = []),
    )
)]
pub async fn demask(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    let rid = request_id();
    let result = async {
        let rt = state.runtime();
        let req: DemaskRequest = parse_json(&body)?;
        let system = resolve_system(&rt.engine, &headers, auth_opts(&state, req.system.as_deref()))?;
        let system_id = system.id().to_string();
        if !system.def.demask {
            return Err(ApiError::demask_forbidden(&system_id));
        }
        let mapping = state
            .store
            .get(&mapping_key(&system_id, &req.payload_id))
            .await
            .map_err(|e| ApiError::store_unavailable(&e.to_string()))?
            .ok_or_else(ApiError::mapping_not_found)?;
        let t = Instant::now();
        let exact = sha256_hex(&req.text) == mapping.masked_hash;
        let text = rt.engine.demask(&req.text, &mapping.entries, exact);
        observe_stage("demask", t.elapsed().as_secs_f64());
        observe_tokens("demask", count_tokens(&req.text));
        let elapsed = started.elapsed();
        tracing::info!(request_id = %rid, route = "/v1/demask", system = %system_id, payload_id_hash = &sha256_hex(&req.payload_id)[..12], exact, entries = mapping.entries.len(), status = 200u16, duration_ms = duration_ms(elapsed), "demasked");
        let body = DemaskResponse { text, entries: mapping.entries.len() };
        Ok(Done { body, system: system_id, direction: "demask", elapsed })
    }
    .await;
    finish("/v1/demask", started, &rid, result)
}

/// Находит ПДн в тексте без маскирования — для предпросмотра или аналитики.
#[utoipa::path(
    post,
    path = "/v1/detect",
    tag = "v1",
    request_body = DetectRequest,
    responses(
        (status = 200, description = "Найденные сущности (без поля `mask` — текст не изменяется)", body = DetectResponse,
            headers(("X-Request-Id" = String, description = "Идентификатор запроса"))),
        (status = 400, description = "Невалидный JSON (`INVALID_REQUEST`)", body = ErrorResponse),
        (status = 401, description = "Ключ не передан или не распознан (`UNAUTHORIZED`)", body = ErrorResponse),
        (status = 403, description = "Система выключена (`SYSTEM_DISABLED`) или в демо-режиме указано неизвестное имя системы (`UNKNOWN_SYSTEM`, 404)", body = ErrorResponse),
        (status = 413, description = "Тело запроса больше `server.max_body_bytes` — стандартный ответ axum"),
        (status = 429, description = "`RATE_LIMITED` — перегрузка: нет места в полосе допуска (адаптивный лимит коротких запросов, полоса больших текстов или `server.max_inflight`)", body = ErrorResponse,
            headers(("Retry-After" = String, description = "Через сколько секунд повторить (`1`)"))),
        (status = 503, description = "Обработка не уложилась в `server.request_timeout_ms` — ответ tower-http с пустым телом, не в едином формате ошибок"),
    ),
    security(
        ("ApiKeyHeader" = []),
        ("BearerAuth" = []),
    )
)]
pub async fn detect(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let started = Instant::now();
    let rid = request_id();
    let result = async {
        let rt = state.runtime();
        let req: DetectRequest = parse_json(&body)?;
        let system = resolve_system(&rt.engine, &headers, auth_opts(&state, req.system.as_deref()))?;
        let system_id = system.id().to_string();
        let t = Instant::now();
        let entities = rt.engine.detect(&req.text, system);
        observe_stage("detect", t.elapsed().as_secs_f64());
        let entities: Vec<EntityView> = entities
            .into_iter()
            .map(|e| EntityView { ty: e.ty.to_string(), start: e.start, end: e.end, confidence: e.confidence, evidence: e.evidence, mask: None })
            .collect();
        let elapsed = started.elapsed();
        tracing::info!(request_id = %rid, route = "/v1/detect", system = %system_id, text_bytes = req.text.len(), entities = entities.len(), status = 200u16, duration_ms = duration_ms(elapsed), "detected");
        let body = DetectResponse { system: system_id.clone(), entities };
        Ok(Done { body, system: system_id, direction: "detect", elapsed })
    }
    .await;
    finish("/v1/detect", started, &rid, result)
}

#[derive(Serialize)]
pub struct SystemView {
    pub id: String,
    pub description: String,
    pub enabled: bool,
    pub anonymous: bool,
    pub profile: String,
    pub demask: bool,
    pub pii_types: serde_json::Value,
    pub exclude_types: Vec<String>,
    pub weak_entities: String,
    pub address_mode: String,
    pub combination_rules: usize,
}

/// Systems are listed for operators (admin key) and, in demo mode, for the UI.
pub async fn systems(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let rt = state.runtime();
    if !rt.cfg.ui.allow_system_override {
        if let Err(e) = require_admin(rt.cfg.admin.api_key.as_deref(), &headers) {
            return e.into_response();
        }
    }
    let mut list: Vec<SystemView> = rt
        .engine
        .systems()
        .map(|s| SystemView {
            id: s.def.id.clone(),
            description: s.def.description.clone(),
            enabled: s.def.enabled,
            anonymous: s.def.auth.anonymous,
            profile: s.def.profile.clone(),
            demask: s.def.demask,
            pii_types: serde_json::to_value(&s.def.pii_types).unwrap_or_default(),
            exclude_types: s.def.exclude_types.clone(),
            weak_entities: format!("{:?}", s.def.weak_entities).to_lowercase(),
            address_mode: format!("{:?}", s.def.address_mode).to_lowercase(),
            combination_rules: s.def.combination_rules.len(),
        })
        .collect();
    list.sort_by(|a, b| a.id.cmp(&b.id));
    Json(list).into_response()
}

#[derive(Serialize)]
pub struct TypeView {
    pub id: String,
    pub description: String,
    pub kind: String,
    pub enabled: bool,
    pub priority: i32,
    pub weak: bool,
    pub group: Option<String>,
}

pub async fn types(State(state): State<AppState>) -> Response {
    let rt = state.runtime();
    let list: Vec<TypeView> = rt
        .engine
        .registry()
        .defs()
        .iter()
        .map(|d| TypeView {
            id: d.id.clone(),
            description: d.description.clone(),
            kind: format!("{:?}", d.kind).to_lowercase(),
            enabled: d.enabled,
            priority: d.priority,
            weak: d.weak,
            group: d.group.clone(),
        })
        .collect();
    Json(list).into_response()
}

#[derive(Serialize)]
pub struct ProfileView {
    pub name: String,
    pub description: String,
    pub default: serde_json::Value,
    pub types: serde_json::Value,
}

pub async fn profiles(State(state): State<AppState>) -> Response {
    let rt = state.runtime();
    let mut list: Vec<ProfileView> = rt
        .engine
        .profiles()
        .iter()
        .map(|(name, p)| ProfileView {
            name: name.clone(),
            description: p.description.clone(),
            default: serde_json::to_value(&p.default).unwrap_or_default(),
            types: serde_json::to_value(&p.types).unwrap_or_default(),
        })
        .collect();
    list.sort_by(|a, b| a.name.cmp(&b.name));
    Json(list).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::capture;
    use crate::store::testing::FlakyStore;
    use serde_json::json;

    const CONFIG: &str = "systems:\n  - id: crm\n    auth: { api_keys: [\"test-key\"] }\n    pii_types: all\n    demask: true\n    profile: reference\n";
    const TEXT: &str = "Клиент Иванов Иван Иванович, паспорт 4509 123456";
    const MASKED: &str = "Клиент И. И. И., паспорт 45** ****56";
    const PAYLOAD_ID: &str = "pid-v1-log-check";

    fn headers() -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-api-key", "test-key".parse().unwrap());
        h
    }

    fn body(value: serde_json::Value) -> Bytes {
        Bytes::from(value.to_string())
    }

    /// Status, `X-Request-Id` and the JSON body.
    async fn parts(resp: Response) -> (StatusCode, String, serde_json::Value) {
        let status = resp.status();
        let rid = resp.headers()["X-Request-Id"].to_str().unwrap().to_string();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, rid, serde_json::from_slice(&bytes).unwrap())
    }

    /// One line per request with its route, status and duration; the text,
    /// the masks and the raw payload_id stay out of the log.
    #[tokio::test]
    async fn request_lines_carry_route_status_and_duration_but_no_text() {
        let (_guard, logs) = capture::start();
        let state = AppState::for_tests(CONFIG, Arc::new(FlakyStore::new()));

        let masked = mask(
            State(state.clone()),
            headers(),
            body(json!({ "text": TEXT, "payload_id": PAYLOAD_ID })),
        )
        .await;
        let (status, mask_rid, answer) = parts(masked).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer["masked"], MASKED);

        let demasked = demask(
            State(state.clone()),
            headers(),
            body(json!({ "text": MASKED, "payload_id": PAYLOAD_ID })),
        )
        .await;
        let (status, demask_rid, answer) = parts(demasked).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer["text"], TEXT);

        let detected = detect(
            State(state.clone()),
            headers(),
            body(json!({ "text": TEXT })),
        )
        .await;
        let (status, detect_rid, _) = parts(detected).await;
        assert_eq!(status, StatusCode::OK);

        let missing = demask(
            State(state.clone()),
            headers(),
            body(json!({ "text": MASKED, "payload_id": "pid-never-masked" })),
        )
        .await;
        let (status, missing_rid, answer) = parts(missing).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(answer["error"]["code"], "MAPPING_NOT_FOUND");

        assert_eq!(logs.lines().len(), 4, "one line per request");
        for (message, route, status, rid) in [
            ("masked", "/v1/mask", 200, &mask_rid),
            ("demasked", "/v1/demask", 200, &demask_rid),
            ("detected", "/v1/detect", 200, &detect_rid),
            ("request rejected", "/v1/demask", 404, &missing_rid),
        ] {
            let lines = logs.with_message(message);
            assert_eq!(lines.len(), 1, "one `{message}` line");
            let line = &lines[0];
            assert_eq!(line["route"], route, "{line}");
            assert_eq!(line["status"], status, "{line}");
            assert_eq!(line["request_id"], rid.as_str(), "{line}");
            assert!(
                line["duration_ms"].as_f64().is_some_and(|ms| ms >= 0.0),
                "{line}"
            );
        }
        assert_eq!(
            logs.with_message("request rejected")[0]["code"],
            "MAPPING_NOT_FOUND"
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
            "pid-never-masked",
            "test-key",
        ] {
            assert!(!raw.contains(secret), "`{secret}` in the log:\n{raw}");
        }
    }
}
