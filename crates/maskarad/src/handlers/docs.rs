//! Публичная документация API: спека OpenAPI собирается из кода (`#[derive(ToSchema)]`
//! на структурах запросов/ответов, `#[utoipa::path]` на обработчиках) и раздаётся через
//! ReDoc на `/docs`, а сама спека — на `/docs/openapi.json` и `/docs/openapi.yaml`.
//!
//! ReDoc UI грузит `redoc.standalone.js` с `cdn.redoc.ly`, а не встраивает бандл
//! (~1 МБ) в бинарник или в архив с исходниками: архив для проверки организаторов
//! должен оставаться лёгким, а `utoipa-redoc` по умолчанию как раз ссылается на CDN.

use crate::error::{ErrorBody, ErrorResponse};
use crate::handlers::{health, llm, process, v1};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use utoipa::openapi::header::Header;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};
use utoipa::openapi::RefOr;
use utoipa::{Modify, OpenApi};

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Maskarad API",
        description = "Прокси между системой-потребителем и LLM: находит персональные данные \
            в запросе, маскирует их до отправки в модель и восстанавливает в ответе. \
            `POST /process` — контракт автопроверки; `/v1/*` — маскирование, демаскирование, \
            детекция и OpenAI-совместимый прокси для интеграций.",
        license(name = "MIT", url = "https://github.com/ArtemSam23/Maskarad/blob/main/LICENSE")
    ),
    servers(
        (url = "https://maskarad.tech", description = "Живой стенд"),
        (url = "http://localhost:8080", description = "Локальный запуск"),
    ),
    tags(
        (name = "contract", description = "Контракт проверяющей системы"),
        (name = "v1", description = "Маскирование, демаскирование и детекция ПДн"),
        (name = "llm", description = "OpenAI-совместимый прокси с маскированием"),
        (name = "service", description = "Проверки живости, готовности и метрики"),
    ),
    paths(
        process::process,
        v1::mask,
        v1::demask,
        v1::detect,
        llm::chat_completions,
        health::healthz,
        health::readyz,
        health::metrics,
    ),
    components(schemas(
        process::ProcessRequest,
        process::ProcessResponse,
        v1::MaskRequest,
        v1::MaskResponse,
        v1::EntityView,
        v1::DemaskRequest,
        v1::DemaskResponse,
        v1::DetectRequest,
        v1::DetectResponse,
        llm::ChatMessage,
        llm::ChatCompletionsRequest,
        ErrorResponse,
        ErrorBody,
    )),
    modifiers(&SecurityAddon, &RequestIdHeader)
)]
pub struct ApiDoc;

/// Tag of the service routes: their responses carry no `X-Request-Id`.
const SERVICE_TAG: &str = "service";

/// Documents `X-Request-Id` on every response of the API routes, errors
/// included: handlers set it, and `logging::log_unlogged` sets it on the rest
/// (413, 503 on timeout). Done here once rather than per status in every
/// `#[utoipa::path]`, so a new status cannot miss it. A response that already
/// documents the header (200) keeps its own description.
struct RequestIdHeader;

impl Modify for RequestIdHeader {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        for item in openapi.paths.paths.values_mut() {
            let operations = [
                item.get.as_mut(),
                item.post.as_mut(),
                item.put.as_mut(),
                item.patch.as_mut(),
                item.delete.as_mut(),
            ];
            for op in operations.into_iter().flatten() {
                let service = op
                    .tags
                    .as_ref()
                    .is_some_and(|tags| tags.iter().any(|t| t == SERVICE_TAG));
                if service {
                    continue;
                }
                for response in op.responses.responses.values_mut() {
                    if let RefOr::T(response) = response {
                        response
                            .headers
                            .entry("X-Request-Id".to_string())
                            .or_insert_with(request_id_header);
                    }
                }
            }
        }
    }
}

fn request_id_header() -> RefOr<Header> {
    let mut header = Header::default();
    header.description = Some(
        "Идентификатор запроса, 16 hex-символов: по нему находится строка этого запроса \
         в логе сервиса; у ошибок в едином формате совпадает с `error.request_id`"
            .to_string(),
    );
    RefOr::T(header)
}

/// Регистрирует security schemes отдельно от `components(schemas(...))`, чтобы не
/// перезаписать уже собранные схемы: `Modify::modify` выполняется после того, как
/// derive-макрос заполнил `openapi.components` целиком.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "ApiKeyHeader",
                SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                    "X-API-Key",
                    "Ключ системы-потребителя из `systems[].auth.api_keys` (можно хранить в конфиге как sha256:<хэш>)",
                ))),
            );
            components.add_security_scheme(
                "BearerAuth",
                SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
            );
        }
    }
}

/// GET /docs/openapi.json — спека в JSON, собранная на запрос из того же `ApiDoc`,
/// что рендерит ReDoc на `/docs`.
pub async fn openapi_json() -> Response {
    Json(ApiDoc::openapi()).into_response()
}

/// GET /docs/openapi.yaml — тот же документ в YAML через уже используемый в проекте
/// `serde_yaml` (без отдельного шага генерации при сборке и без новых зависимостей:
/// `utoipa::openapi::OpenApi` — обычная `Serialize`-структура).
pub async fn openapi_yaml() -> Response {
    match serde_yaml::to_string(&ApiDoc::openapi()) {
        Ok(yaml) => (
            [(header::CONTENT_TYPE, "application/yaml; charset=utf-8")],
            yaml,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "failed to render openapi.yaml");
            crate::error::ApiError::internal().into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::StatusCode;

    /// Собирает тот же `Router` (без состояния — маршрут `/docs` его не требует),
    /// каким main.rs раздаёт документацию, и проверяет реальный HTTP-ответ.
    #[tokio::test]
    async fn docs_route_serves_redoc_html() {
        use tower::ServiceExt;
        use utoipa_redoc::{Redoc, Servable};

        let app: axum::Router<()> =
            axum::Router::new().merge(Redoc::with_url("/docs", ApiDoc::openapi()));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/docs")
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.contains("text/html"),
            "content-type was {content_type}"
        );
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let html = String::from_utf8(bytes.to_vec()).expect("utf8 html");
        assert!(
            html.contains("redoc.standalone.js"),
            "ReDoc must load from CDN"
        );
    }

    #[test]
    fn spec_contains_all_five_documented_paths() {
        let spec = ApiDoc::openapi();
        let paths = &spec.paths.paths;
        for path in [
            "/process",
            "/v1/mask",
            "/v1/demask",
            "/v1/detect",
            "/v1/chat/completions",
        ] {
            assert!(
                paths.contains_key(path),
                "missing path {path} in generated spec"
            );
        }
    }

    /// As at runtime: every API route response has `X-Request-Id`, service
    /// route responses do not.
    #[test]
    fn every_api_response_documents_x_request_id() {
        let spec = ApiDoc::openapi();
        let mut api_responses = 0;
        for (path, item) in &spec.paths.paths {
            for op in [&item.get, &item.post].into_iter().flatten() {
                let service = op
                    .tags
                    .as_ref()
                    .is_some_and(|tags| tags.iter().any(|t| t == SERVICE_TAG));
                for (status, response) in &op.responses.responses {
                    let RefOr::T(response) = response else {
                        panic!("{path} {status}: a reference, expected an inline response");
                    };
                    assert_eq!(
                        response.headers.contains_key("X-Request-Id"),
                        !service,
                        "{path} {status}"
                    );
                    if !service {
                        api_responses += 1;
                    }
                }
            }
        }
        assert!(api_responses >= 30, "{api_responses} API responses");
        let process = spec.paths.paths["/process"].post.as_ref().unwrap();
        let RefOr::T(rate_limited) = &process.responses.responses["429"] else {
            panic!("inline 429");
        };
        assert!(rate_limited.headers.contains_key("X-Request-Id"));
        assert!(rate_limited.headers.contains_key("Retry-After"));
    }

    #[tokio::test]
    async fn openapi_json_is_valid_json_with_paths() {
        let resp = openapi_json().await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
        assert!(value["paths"]["/process"].is_object());
    }

    #[tokio::test]
    async fn openapi_yaml_parses_back_to_the_same_paths() {
        let resp = openapi_yaml().await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.contains("yaml"),
            "content-type was {content_type}"
        );
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        let value: serde_yaml::Value = serde_yaml::from_slice(&bytes).expect("valid yaml");
        assert!(value.get("paths").and_then(|p| p.get("/v1/mask")).is_some());
    }
}
