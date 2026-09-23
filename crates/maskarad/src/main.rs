//! Maskarad service and CLI.

mod auth;
mod bench;
mod config;
mod error;
mod eval;
mod golden;
mod handlers;
mod limits;
mod llm;
mod logging;
mod metrics;
mod reload;
mod smoke;
mod state;
mod store;

use anyhow::Context;
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use clap::{Parser, Subcommand};
use config::{ServerConfig, StoreBackend};
use handlers::docs::ApiDoc;
use state::{AppState, Runtime};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use store::{MappingStore, TieredStore};
use tower_http::timeout::TimeoutLayer;
use utoipa::OpenApi;
use utoipa_redoc::{Redoc, Servable};

#[derive(Parser)]
#[command(
    name = "maskarad",
    version,
    about = "PII masking proxy for LLM traffic"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP service.
    Serve {
        #[arg(long, default_value = "config/maskarad.yaml")]
        config: PathBuf,
    },
    /// Mask a text from the command line and print entities.
    Mask {
        #[arg(long, default_value = "config/maskarad.yaml")]
        config: PathBuf,
        /// Consumer system id (default: the anonymous system).
        #[arg(long)]
        system: Option<String>,
        /// Read the text from a file instead of the argument.
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(default_value = "")]
        text: String,
    },
    /// Load test against a running service, reproducing the checker protocol.
    Bench {
        #[arg(long, default_value = "http://127.0.0.1:8080")]
        url: String,
        /// Pairs (mask + demask) per second.
        #[arg(long, default_value_t = 500)]
        rps: u64,
        #[arg(long, default_value_t = 30)]
        duration: u64,
        #[arg(long, default_value = "tests/golden")]
        dataset: PathBuf,
        #[arg(long, default_value_t = 4096)]
        max_inflight: usize,
        #[arg(long, default_value_t = 10)]
        timeout_secs: u64,
        /// Also write the corpus for the k6 scenario.
        #[arg(long)]
        export_corpus: Option<PathBuf>,
        /// Generate the corpus in memory (records per category) instead of reading --dataset.
        #[arg(long)]
        generate: Option<usize>,
        /// Do not verify the TLS certificate (before Let's Encrypt has issued it).
        #[arg(long)]
        insecure: bool,
    },
    /// Check a running deployment end to end by the /process contract.
    Smoke {
        url: String,
        /// Do not verify the TLS certificate (before Let's Encrypt has issued it).
        #[arg(long)]
        insecure: bool,
        #[arg(long, default_value_t = 15)]
        timeout_secs: u64,
    },
    /// Validate a configuration file and build the engine.
    CheckConfig {
        #[arg(long, default_value = "config/maskarad.yaml")]
        config: PathBuf,
    },
    /// Generate a synthetic golden dataset (JSONL).
    Gen {
        #[arg(long, default_value = "tests/golden/generated.jsonl")]
        out: PathBuf,
        #[arg(long, default_value_t = 42)]
        seed: u64,
        #[arg(long, default_value_t = 60)]
        per_category: usize,
    },
    /// Evaluate detection/masking/demasking quality on a golden dataset.
    Eval {
        #[arg(long, default_value = "config/maskarad.yaml")]
        config: PathBuf,
        /// A .jsonl file or a directory of them.
        #[arg(long, default_value = "tests/golden")]
        dataset: PathBuf,
        #[arg(long)]
        system: Option<String>,
        /// Write a Markdown report here.
        #[arg(long)]
        report: Option<PathBuf>,
        /// Fail (exit 1) when micro F1 is below this value.
        #[arg(long, default_value_t = 0.95)]
        min_f1: f64,
        #[arg(long, default_value_t = 40)]
        max_failures: usize,
        /// Print the text of every record with a discrepancy (for debugging).
        #[arg(long)]
        verbose: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve { config } => {
            let cfg = ServerConfig::load(&config)?;
            logging::init(&cfg.logging);
            logging::install_panic_hook();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(serve(cfg, config))
        }
        Command::Mask {
            config,
            system,
            file,
            text,
        } => {
            let cfg = ServerConfig::load(&config)?;
            let runtime = Runtime::build(cfg)?;
            let text = match file {
                Some(path) => std::fs::read_to_string(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?,
                None => text,
            };
            let sys = match system {
                Some(id) => runtime
                    .engine
                    .system(&id)
                    .with_context(|| format!("unknown system `{id}`"))?,
                None => runtime
                    .engine
                    .anonymous_system()
                    .context("no anonymous system configured; pass --system")?,
            };
            if std::env::var("MASKARAD_PROFILE").is_ok() {
                for (name, dur, n) in runtime.engine.detector_timings(&text) {
                    eprintln!("  detector {name:<12} {dur:?} ({n} candidates)");
                }
            }
            let started = Instant::now();
            let result = runtime.engine.mask(&text, sys);
            eprintln!(
                "mask: {:?} for {} bytes, {} entities",
                started.elapsed(),
                text.len(),
                result.entries.len()
            );
            println!("{}", result.masked);
            for (e, m) in result.entities.iter().zip(result.entries.iter()) {
                eprintln!(
                    "{:<22} {:.2} {:<28} {}..{} -> {}",
                    e.ty.to_string(),
                    e.confidence,
                    e.evidence,
                    e.start,
                    e.end,
                    m.mask
                );
            }
            Ok(())
        }
        Command::Gen {
            out,
            seed,
            per_category,
        } => {
            let dict = maskarad_core::Dictionaries::builtin();
            let records = golden::Generator::new(seed, &dict).generate(per_category);
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            golden::write(&out, &records)?;
            println!("wrote {} records to {}", records.len(), out.display());
            let curated_path = out.with_file_name("curated.jsonl");
            let curated = golden::curated(&dict);
            golden::write(&curated_path, &curated)?;
            println!(
                "wrote {} curated records to {}",
                curated.len(),
                curated_path.display()
            );
            Ok(())
        }
        Command::Eval {
            config,
            dataset,
            system,
            report,
            min_f1,
            max_failures,
            verbose,
        } => {
            let cfg = ServerConfig::load(&config)?;
            let runtime = Runtime::build(cfg)?;
            let sys = match &system {
                Some(id) => runtime
                    .engine
                    .system(id)
                    .with_context(|| format!("unknown system `{id}`"))?,
                None => runtime
                    .engine
                    .anonymous_system()
                    .context("no anonymous system configured; pass --system")?,
            };
            let records = golden::load(&dataset)?;
            let started = Instant::now();
            let rep = eval::evaluate(&runtime.engine, sys, &records, max_failures);
            let elapsed = started.elapsed();
            let md = eval::render_markdown(
                &rep,
                &format!("Качество на golden-датасете (система `{}`)", sys.id()),
            );
            println!("{md}");
            if verbose {
                for f in &rep.failures {
                    if let Some(rec) = records.iter().find(|r| r.id == f.id) {
                        let result = runtime.engine.mask(&rec.text, sys);
                        println!(
                            "--- {} [{}] {}\n    text: {}\n    exp : {}\n    got : {}",
                            f.id, f.kind, f.detail, rec.text, rec.masked, result.masked
                        );
                    }
                }
            }
            println!(
                "({} records in {:.2}s, {:.0} µs/record)",
                records.len(),
                elapsed.as_secs_f64(),
                elapsed.as_micros() as f64 / records.len().max(1) as f64
            );
            if let Some(path) = report {
                std::fs::write(&path, &md)?;
                println!("report written to {}", path.display());
            }
            if rep.micro.f1() < min_f1 {
                anyhow::bail!(
                    "micro F1 {:.3} is below the threshold {min_f1}",
                    rep.micro.f1()
                );
            }
            Ok(())
        }
        Command::Bench {
            url,
            rps,
            duration,
            dataset,
            max_inflight,
            timeout_secs,
            export_corpus,
            generate,
            insecure,
        } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(bench::run(bench::BenchArgs {
                url,
                rps,
                duration,
                dataset,
                max_inflight,
                timeout_secs,
                export_corpus,
                generate,
                insecure,
            }))
        }
        Command::Smoke {
            url,
            insecure,
            timeout_secs,
        } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(smoke::run(smoke::SmokeArgs {
                url,
                insecure,
                timeout_secs,
            }))
        }
        Command::CheckConfig { config } => {
            let cfg = ServerConfig::load(&config)?;
            let started = Instant::now();
            let runtime = Runtime::build(cfg)?;
            println!(
                "ok: {} systems, {} types, engine built in {} ms",
                runtime.engine.systems().count(),
                runtime.engine.registry().defs().len(),
                started.elapsed().as_millis()
            );
            Ok(())
        }
    }
}

async fn build_store(cfg: &ServerConfig) -> anyhow::Result<TieredStore> {
    let ttl = Duration::from_secs(cfg.store.ttl_seconds);
    match cfg.store.backend {
        StoreBackend::Memory => Ok(TieredStore::memory_only(
            ttl,
            cfg.store.memory_capacity_bytes,
        )),
        StoreBackend::Redis => {
            let url = cfg.store.redis_url.clone().context("store.redis_url")?;
            let cipher = store::crypto::Cipher::from_secret(
                cfg.store.encryption_key.as_deref().unwrap_or_default(),
            );
            let timeout = Duration::from_millis(cfg.store.timeout_ms);
            match store::redis::RedisStore::connect(&url, cipher, ttl, timeout).await {
                Ok(redis) => {
                    tracing::info!("connected to redis");
                    Ok(TieredStore::with_primary(
                        Arc::new(redis),
                        ttl,
                        cfg.store.memory_capacity_bytes,
                    ))
                }
                Err(e) if cfg.store.memory_fallback => {
                    tracing::error!(error = %e, "redis unavailable at startup; starting degraded with memory store");
                    Ok(TieredStore::startup_fallback(
                        ttl,
                        cfg.store.memory_capacity_bytes,
                    ))
                }
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
    }
}

async fn serve(cfg: ServerConfig, config_path: PathBuf) -> anyhow::Result<()> {
    let started = Instant::now();
    let metrics_handle = metrics::install()?;
    let store = build_store(&cfg).await?;
    let listen = cfg.server.listen.clone();
    let admission = Arc::new(limits::Admission::new(&cfg.server));
    admission.start_lag_probe();
    let runtime = Runtime::build(cfg).context("building engine")?;
    tracing::info!(
        systems = runtime.engine.systems().count(),
        types = runtime.engine.registry().defs().len(),
        store = store.backend(),
        build_ms = started.elapsed().as_millis(),
        "engine ready"
    );
    let state = AppState {
        runtime: Arc::new(arc_swap::ArcSwap::from_pointee(runtime)),
        store: Arc::new(store),
        llm: Arc::new(llm::LlmClient::new()),
        admission,
        metrics: metrics_handle,
        config_path: Some(config_path.clone()),
        started,
    };
    if let Err(e) = reload::watch(state.clone(), &config_path) {
        tracing::warn!(error = %e, "configuration file watching disabled");
    }

    let app = app(state);

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    tracing::info!(listen = %listen, "maskarad listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("shutdown complete");
    Ok(())
}

/// API routes get a log line per request and the request limits; service
/// routes (health, metrics, settings, docs, UI) get neither.
fn app(state: AppState) -> Router {
    let rt = state.runtime();
    let local = Router::new()
        .route("/process", post(handlers::process::process))
        .route("/v1/mask", post(handlers::v1::mask))
        .route("/v1/demask", post(handlers::v1::demask))
        .route("/v1/detect", post(handlers::v1::detect))
        .layer(axum::middleware::from_fn_with_state(
            state.admission.clone(),
            limits::admit,
        ));
    // The upstream's latency is not ours to react to: this route is counted
    // against the ceiling only, see `limits`.
    let upstream = Router::new()
        .route(
            "/v1/chat/completions",
            post(handlers::llm::chat_completions),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.admission.clone(),
            limits::admit_unsampled,
        ));
    let api = local
        .merge(upstream)
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_millis(rt.cfg.server.request_timeout_ms),
        ))
        .layer(DefaultBodyLimit::max(rt.cfg.server.max_body_bytes))
        // A caller with a wrong URL still finds its request id in the log.
        // Set after the limits, so unknown paths skip them.
        .fallback(|| async { axum::http::StatusCode::NOT_FOUND })
        // Outermost: it logs what the body limit, the timeout and the router
        // answer on their own.
        .layer(axum::middleware::from_fn(logging::log_unlogged));
    // Документация — публичная: вне auth (как /healthz) и вне допуска (роутер `ops`,
    // а не `api`, к маршрутам которого применены `limits::admit*`).
    let mut ops = Router::new()
        .route("/healthz", get(handlers::health::healthz))
        .route("/readyz", get(handlers::health::readyz))
        .route("/metrics", get(handlers::health::metrics))
        .route("/v1/systems", get(handlers::v1::systems))
        .route("/v1/types", get(handlers::v1::types))
        .route("/v1/profiles", get(handlers::v1::profiles))
        .route("/admin/reload", post(handlers::admin::reload_config))
        .merge(Redoc::with_url("/docs", ApiDoc::openapi()))
        .route("/docs/openapi.json", get(handlers::docs::openapi_json))
        .route("/docs/openapi.yaml", get(handlers::docs::openapi_yaml));
    if rt.cfg.ui.enabled {
        ops = ops.route("/", get(handlers::ui::index));
    }
    api.merge(ops).with_state(state)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received, finishing in-flight requests");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::capture;
    use crate::store::testing::FlakyStore;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::response::Response;
    use tower::ServiceExt;

    const CONFIG: &str = "server:\n  max_body_bytes: 1024\n  request_timeout_ms: 50\nsystems:\n  - id: alfasonar\n    auth: { anonymous: true }\n    pii_types: all\n    demask: true\n    profile: reference\n";
    const TEXT: &str = "Клиент Иванов Иван Иванович, паспорт 4509 123456";

    fn process_body(payload: &str) -> Body {
        Body::from(serde_json::json!({ "payload": payload, "payload_id": "p1" }).to_string())
    }

    async fn send(app: &Router, method: &str, uri: &str, body: Body) -> Response {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(body)
            .unwrap();
        app.clone().oneshot(req).await.unwrap()
    }

    fn request_id(resp: &Response) -> &str {
        resp.headers()["X-Request-Id"].to_str().unwrap()
    }

    /// The only line written is `message` with these route and status, and
    /// its request id is the response's `X-Request-Id`.
    fn assert_one_line(
        logs: &capture::Logs,
        resp: &Response,
        message: &str,
        route: &str,
    ) -> serde_json::Value {
        let lines = logs.lines();
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = lines.into_iter().next().unwrap();
        assert_eq!(line["message"], message, "{line}");
        assert_eq!(line["route"], route, "{line}");
        assert_eq!(line["status"], resp.status().as_u16(), "{line}");
        assert_eq!(line["request_id"], request_id(resp), "{line}");
        assert!(line["duration_ms"].as_f64().is_some(), "{line}");
        line
    }

    #[tokio::test]
    async fn a_handled_request_has_exactly_one_line() {
        let (_guard, logs) = capture::start();
        let app = app(AppState::for_tests(CONFIG, Arc::new(FlakyStore::new())));

        let resp = send(&app, "POST", "/process", process_body(TEXT)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_one_line(&logs, &resp, "masked", "/process");
    }

    #[tokio::test]
    async fn an_oversized_body_is_a_logged_413() {
        let (_guard, logs) = capture::start();
        let app = app(AppState::for_tests(CONFIG, Arc::new(FlakyStore::new())));

        let resp = send(&app, "POST", "/process", process_body(&TEXT.repeat(20))).await;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let line = assert_one_line(&logs, &resp, "request rejected", "/process");
        assert_eq!(line["level"], "WARN");
        assert!(!logs.raw().contains("Иванов"), "{}", logs.raw());
    }

    #[tokio::test]
    async fn a_timed_out_request_is_a_logged_503() {
        let (_guard, logs) = capture::start();
        let store = Arc::new(FlakyStore::new());
        store.hang_get(true);
        let state = AppState::for_tests(CONFIG, store);
        let timeout_ms = state.runtime().cfg.server.request_timeout_ms;
        let app = app(state);

        let resp = send(&app, "POST", "/process", process_body(TEXT)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let line = assert_one_line(&logs, &resp, "request rejected", "/process");
        assert!(line["duration_ms"].as_f64().unwrap() >= timeout_ms as f64);
    }

    #[tokio::test]
    async fn wrong_method_and_unknown_path_are_logged() {
        let (_guard, logs) = capture::start();
        let app = app(AppState::for_tests(CONFIG, Arc::new(FlakyStore::new())));

        let resp = send(&app, "GET", "/process", Body::empty()).await;
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_one_line(&logs, &resp, "request rejected", "/process");

        let (_guard, logs) = capture::start();
        let resp = send(&app, "POST", "/v1/unknown?pid=4509123456", Body::empty()).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_one_line(&logs, &resp, "request rejected", "unmatched");
        let raw = logs.raw();
        assert!(
            !raw.contains("unknown") && !raw.contains("4509123456"),
            "{raw}"
        );
    }

    /// The client gives up before the response: hyper drops the request
    /// future, as `timeout` does here.
    #[tokio::test]
    async fn a_request_the_client_aborted_is_logged() {
        let (_guard, logs) = capture::start();
        let store = Arc::new(FlakyStore::new());
        store.hang_get(true);
        let config = CONFIG.replace("request_timeout_ms: 50", "request_timeout_ms: 60000");
        let app = app(AppState::for_tests(&config, store));

        let sent = send(&app, "POST", "/process", process_body(TEXT));
        let aborted = tokio::time::timeout(Duration::from_millis(20), sent).await;
        assert!(aborted.is_err(), "the request is still in flight");

        let lines = logs.lines();
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert_eq!(line["message"], "request aborted");
        assert_eq!(line["level"], "WARN");
        assert_eq!(line["route"], "/process");
        assert_eq!(line["status"], 499);
        assert!(line["request_id"].as_str().is_some_and(|id| id.len() == 16));
        assert!(line["duration_ms"].as_f64().is_some());
    }

    #[tokio::test]
    async fn a_request_whose_handler_panicked_is_logged() {
        let (_guard, logs) = capture::start();
        let app = Router::new()
            .route(
                "/process",
                post(|| async {
                    let text = "Клиент Иванов";
                    &text[..1]
                }),
            )
            .layer(axum::middleware::from_fn(logging::log_unlogged));

        let req = Request::post("/process").body(Body::empty()).unwrap();
        let joined = tokio::spawn(app.oneshot(req)).await;
        assert!(joined.unwrap_err().is_panic());

        let lines = logs.lines();
        assert_eq!(lines.len(), 1, "{lines:#?}");
        let line = &lines[0];
        assert_eq!(line["message"], "request panicked");
        assert_eq!(line["level"], "ERROR");
        assert_eq!(line["route"], "/process");
        assert!(line.get("status").is_none(), "{line}");
        assert!(!logs.raw().contains("Иванов"));
    }

    /// A large text waiting on the store holds the only large place: the
    /// next large one waits its turn and after half the request timeout is
    /// shed with its reason; a short request is not affected.
    #[tokio::test]
    async fn a_large_text_waits_for_its_own_lane() {
        let (_guard, logs) = capture::start();
        let store = Arc::new(FlakyStore::new());
        store.hang_get(true);
        let config = CONFIG
            .replace("max_body_bytes: 1024", "max_body_bytes: 8192\n  blocking_threshold_bytes: 200\n  admission: { large_limit: 1 }")
            .replace("request_timeout_ms: 50", "request_timeout_ms: 400");
        let state = AppState::for_tests(&config, store);
        let admission = state.admission.clone();
        let app = app(state);
        let large = || {
            let body =
                serde_json::json!({ "payload": TEXT.repeat(10), "payload_id": "p1" }).to_string();
            Request::post("/process")
                .header("content-length", body.len())
                .body(Body::from(body))
                .unwrap()
        };

        let first = tokio::spawn(app.clone().oneshot(large()));
        while admission.in_flight() == 0 {
            tokio::task::yield_now().await;
        }
        let second = tokio::spawn(app.clone().oneshot(large()));
        while admission.in_flight() < 2 {
            tokio::task::yield_now().await;
        }
        // No key: 401 from the handler, i.e. past admission.
        let body = serde_json::json!({ "text": TEXT }).to_string();
        let resp = send(&app, "POST", "/v1/detect", Body::from(body)).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "the short lane is free"
        );

        let resp = second.await.unwrap().unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers()["Retry-After"], "1");
        let shed: Vec<_> = logs
            .lines()
            .into_iter()
            .filter(|l| l["reason"] == "large_lane_full")
            .collect();
        assert_eq!(shed.len(), 1, "{:#?}", logs.lines());
        let line = &shed[0];
        assert_eq!(line["message"], "request rejected");
        assert_eq!(line["code"], "RATE_LIMITED");
        assert_eq!(line["request_id"], request_id(&resp));
        assert_eq!(
            first.await.unwrap().unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn service_routes_are_not_logged() {
        let (_guard, logs) = capture::start();
        let app = app(AppState::for_tests(CONFIG, Arc::new(FlakyStore::new())));

        for (method, uri, status) in [
            ("GET", "/healthz", StatusCode::OK),
            ("GET", "/readyz", StatusCode::OK),
            ("GET", "/metrics", StatusCode::OK),
            ("GET", "/", StatusCode::OK),
            ("GET", "/v1/types", StatusCode::OK),
            ("GET", "/docs/openapi.json", StatusCode::OK),
            ("POST", "/healthz", StatusCode::METHOD_NOT_ALLOWED),
        ] {
            let resp = send(&app, method, uri, Body::empty()).await;
            assert_eq!(resp.status(), status, "{method} {uri}");
            assert!(resp.headers().get("X-Request-Id").is_none(), "{uri}");
        }
        assert_eq!(logs.raw(), "");
    }
}
