//! Structured logging. Log records carry identifiers, counts, types and
//! timings — never request text or entity values.

use crate::config::{LogFormat, LoggingSection};
use axum::extract::{MatchedPath, Request};
use axum::http::{HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use std::time::{Duration, Instant};
use tracing_subscriber::EnvFilter;

const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

pub fn init(section: &LoggingSection) {
    let filter = EnvFilter::try_new(&section.level).unwrap_or_else(|_| EnvFilter::new("info"));
    match section.format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_current_span(false)
                .with_env_filter(filter)
                .init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }
}

/// Replaces the default panic hook, which prints the panic message to
/// stderr, and stderr goes to the log storage like stdout. A message can quote
/// the request text: slicing a `&str` off a char boundary panics with part of
/// the string (up to 256 bytes on Rust 1.94). Only the location is logged.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| match info.location() {
        Some(location) => tracing::error!(panic_location = %location, "panic"),
        None => tracing::error!("panic"),
    }));
}

/// Short random request id: 16 lowercase hex characters.
///
/// One allocation: it is also taken for every request shed by the
/// concurrency limit, i.e. exactly when the service is overloaded.
pub fn request_id() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes: [u8; 8] = rand::random();
    let mut id = String::with_capacity(16);
    for b in bytes {
        id.push(char::from(HEX[usize::from(b >> 4)]));
        id.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    id
}

/// `duration_ms` log field: milliseconds rounded to 0.01 ms. A small
/// `/process` request takes tenths of a millisecond, so whole milliseconds
/// would log 0; a 10 µs step keeps two significant digits there.
pub fn duration_ms(elapsed: Duration) -> f64 {
    (elapsed.as_secs_f64() * 100_000.0).round() / 100.0
}

/// Response extension: the request's log line is already written.
#[derive(Clone, Copy, Debug)]
pub struct RequestLogged;

/// Marks a response whose request has its log line, so that
/// [`log_unlogged`] lets it through.
pub fn logged(mut resp: Response) -> Response {
    resp.extensions_mut().insert(RequestLogged);
    resp
}

/// Writes the log line of an API request answered without a handler writing
/// one: 413 from the body limit (the body extractor rejects before the
/// handler runs), 503 from `TimeoutLayer` (the handler future is dropped),
/// 405 and 404 from the router. So it has to sit outside those layers.
/// Responses of handlers and of the in-flight limit carry [`RequestLogged`]
/// and pass through without allocations. A request that never gets a
/// response is logged by `Pending`.
///
/// The request id is the response's `X-Request-Id` if it has one, otherwise
/// a new one that is also set as `X-Request-Id`: every API response points at
/// its log line.
pub async fn log_unlogged(req: Request, next: Next) -> Response {
    let mut pending = Pending {
        started: Instant::now(),
        route: req.extensions().get::<MatchedPath>().cloned(),
        armed: true,
    };
    let mut resp = next.run(req).await;
    pending.armed = false;
    if resp.extensions().get::<RequestLogged>().is_some() {
        return resp;
    }
    let elapsed = pending.started.elapsed();
    let rid = match resp.headers().get(X_REQUEST_ID) {
        Some(v) => v.clone(),
        None => {
            let v = HeaderValue::try_from(request_id()).expect("hex is a valid header value");
            resp.headers_mut().insert(X_REQUEST_ID, v.clone());
            v
        }
    };
    tracing::warn!(
        request_id = rid.to_str().unwrap_or("-"),
        route = pending.route(),
        status = resp.status().as_u16(),
        duration_ms = duration_ms(elapsed),
        "request rejected"
    );
    resp
}

/// Armed until the request has a response. Its future is dropped unfinished
/// when the client disconnects (hyper drops it) or a handler panics (tokio
/// drops it while unwinding); code after `next.run(req).await` never runs
/// then, `Drop` does. A handler writes its line in the same poll that returns
/// its response, so an aborted request never has a line already.
struct Pending {
    started: Instant,
    route: Option<MatchedPath>,
    armed: bool,
}

impl Pending {
    fn route(&self) -> &str {
        self.route.as_ref().map_or(UNMATCHED, MatchedPath::as_str)
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let rid = request_id();
        let duration_ms = duration_ms(self.started.elapsed());
        if std::thread::panicking() {
            // No status: the client gets no response, the connection closes.
            tracing::error!(
                request_id = %rid,
                route = self.route(),
                duration_ms,
                "request panicked"
            );
        } else {
            // 499 is how nginx logs a request the client closed.
            tracing::warn!(
                request_id = %rid,
                route = self.route(),
                status = 499u16,
                duration_ms,
                "request aborted"
            );
        }
    }
}

/// `route` of a request that matched no route.
pub const UNMATCHED: &str = "unmatched";

/// `TYPE:count,TYPE:count` for log lines.
pub fn types_summary(counts: &[(maskarad_core::PiiType, usize)]) -> String {
    counts
        .iter()
        .map(|(t, n)| format!("{t}:{n}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Captures the JSON log lines written on the current thread, formatted as
/// `init` formats them in production.
#[cfg(test)]
pub mod capture {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing::subscriber::DefaultGuard;

    #[derive(Clone, Default)]
    pub struct Logs(Arc<Mutex<Vec<u8>>>);

    impl Logs {
        pub fn raw(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).expect("utf-8 log output")
        }

        pub fn lines(&self) -> Vec<serde_json::Value> {
            self.raw()
                .lines()
                .map(|l| serde_json::from_str(l).expect("a JSON log line"))
                .collect()
        }

        /// Lines with this `message`.
        pub fn with_message(&self, message: &str) -> Vec<serde_json::Value> {
            self.lines()
                .into_iter()
                .filter(|l| l["message"] == message)
                .collect()
        }
    }

    impl Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Lines are captured while the guard lives. The default subscriber is
    /// per thread, so this works with `#[tokio::test]` (current-thread runtime).
    pub fn start() -> (DefaultGuard, Logs) {
        // tracing caches a callsite's interest globally. A test on another thread
        // that hits the same `warn!` with no subscriber caches it as "never", and
        // this capture then silently loses the event. A global subscriber that is
        // interested in everything (and writes nowhere) keeps every callsite live;
        // the thread-local capture below still takes precedence.
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let sink = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(std::io::sink)
                .finish();
            let _ = tracing::subscriber::set_global_default(sink);
        });
        let logs = Logs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .flatten_event(true)
            .with_current_span(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        (tracing::subscriber::set_default(subscriber), logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_id_is_16_lowercase_hex() {
        let id = request_id();
        assert_eq!(id.len(), 16);
        assert!(id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_ne!(id, request_id());
    }

    /// The hook is process-wide, so it is swapped in only around one
    /// `catch_unwind`.
    #[test]
    fn a_panic_is_logged_without_its_message() {
        let (_guard, logs) = capture::start();
        let text = String::from("Клиент Иванов Иван Иванович, паспорт 4509 123456");
        let default_hook = std::panic::take_hook();
        install_panic_hook();
        let payload = std::panic::catch_unwind(|| text[..1].len()).unwrap_err();
        std::panic::set_hook(default_hook);

        // Rust 1.94 quotes up to 256 bytes of the string here, 1.98 only the
        // char at the boundary.
        let message = payload
            .downcast_ref::<String>()
            .expect("a formatted message");
        assert!(
            message.contains("'К'"),
            "the message quotes the text: {message}"
        );
        let lines = logs.lines();
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert_eq!(lines[0]["message"], "panic");
        assert_eq!(lines[0]["level"], "ERROR");
        assert!(
            lines[0]["panic_location"]
                .as_str()
                .is_some_and(|l| l.contains("logging.rs")),
            "{}",
            lines[0]
        );
        let raw = logs.raw();
        for fragment in ["К", "Иванов", "4509", "char boundary"] {
            assert!(!raw.contains(fragment), "`{fragment}` in the log:\n{raw}");
        }
    }

    #[test]
    fn duration_is_rounded_to_hundredths_of_a_millisecond() {
        assert_eq!(duration_ms(Duration::ZERO), 0.0);
        assert_eq!(duration_ms(Duration::from_micros(1_234)), 1.23);
        assert_eq!(duration_ms(Duration::from_micros(1_236)), 1.24);
        assert_eq!(duration_ms(Duration::from_micros(3)), 0.0);
        assert_eq!(duration_ms(Duration::from_secs(2)), 2000.0);
    }
}
