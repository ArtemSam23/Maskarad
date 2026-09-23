//! Load shedding: every API request is admitted into a lane; over the lane's
//! bound the answer is 429 with `Retry-After: 1` right away, not a place in a
//! queue inside the pod.
//!
//! | lane | requests | bound |
//! |---|---|---|
//! | short | `Content-Length` up to `server.blocking_threshold_bytes`, or none | `server.max_inflight`; while the CPU is congested, an adaptive limit that follows their latency ([`gradient`]) |
//! | large | larger bodies | `server.admission.large_limit` processed at once, the rest wait their turn up to half of `server.request_timeout_ms` |
//! | ceiling only | `/v1/chat/completions` | none of its own |
//!
//! `server.max_inflight` caps all lanes together, waiting large requests
//! included; with `server.admission.enabled: false` it is the only limit, as
//! it used to be.
//!
//! Only the short lane feeds the latency signal. A 16× text takes a second at
//! any load, and an LLM call takes whatever the upstream takes: sampled, either
//! would shrink the limit for short requests without any queue behind it.
//! Large texts wait instead of being shed: they are work for throughput, a
//! 429 costs each of them a second per retry, and their lane is there to keep
//! them from taking every CPU at once, which a queue does as well.
//!
//! A request without `Content-Length` is short. Behind ingress-nginx every
//! request has one (the body is buffered before proxying), and so do the
//! checker, k6 and `maskarad bench`. What remains is a chunked body from a
//! direct client, bounded by `server.max_body_bytes`: it holds one short slot
//! for at most a few seconds and is one latency sample among the hundreds a
//! loaded window has. Treating it as large instead would queue every such
//! small request behind the large texts.
//!
//! The hot path is a few relaxed atomics and no allocations: a CAS on the
//! lane and on the ceiling, a `fetch_add` on the in-flight gauge, one more
//! per sample. The limit is recomputed every `UPDATE_INTERVAL` by the short
//! request that completes first after the interval ends and wins a CAS on the
//! deadline, not by a background task: recomputing matters when the runtime
//! is overloaded, and a timer task would wait in the same run queue as the
//! requests it protects. It also keeps the controller a plain function of
//! samples and time, which the tests drive directly.
//!
//! The one background task, [`Admission::start_lag_probe`], measures how long
//! a woken task waits for the CPU and engages or releases the limit (see
//! [`gradient`] for when and why). Waiting in the run queue is the point of
//! that measurement, so it does not share the objection above.

mod gradient;

use crate::config::ServerSection;
use crate::error::ApiError;
use crate::logging::{logged, request_id, UNMATCHED};
use axum::body::{Body, HttpBody};
use axum::extract::{MatchedPath, Request, State};
use axum::http::header::{CONTENT_LENGTH, EXPECT};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use gradient::{Gradient, WindowStats, ENGAGE_PROBES, MIN_SAMPLES, UPDATE_INTERVAL};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, Weak};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, SemaphorePermit};

/// Middleware for routes whose latency is local work: short and large lanes.
pub async fn admit(State(admission): State<Arc<Admission>>, req: Request, next: Next) -> Response {
    let lane = admission.lane_for(&req);
    run(&admission, lane, req, next).await
}

/// Middleware for the LLM proxy: counted against `server.max_inflight`
/// only, never sampled.
pub async fn admit_unsampled(
    State(admission): State<Arc<Admission>>,
    req: Request,
    next: Next,
) -> Response {
    run(&admission, Lane::Ceiling, req, next).await
}

async fn run(admission: &Admission, lane: Lane, req: Request, next: Next) -> Response {
    let admitted = match lane {
        Lane::Large => admission.admit_large().await,
        Lane::Short | Lane::Ceiling => admission.admit(lane),
    };
    match admitted {
        Ok(mut slot) => {
            let resp = next.run(req).await;
            // A 4xx from the handler is answered without the work whose
            // latency the limit follows (bad JSON, a wrong key, an unknown
            // payload_id), and the fast ones would drag rtt_noload below what
            // a real request takes. STORE_BUSY, the one 429 a handler gives,
            // is sampled: it comes after a store timeout and a retry, and when
            // the CPU is short those are the slowest requests there are.
            let status = resp.status();
            if status.is_client_error() && status != StatusCode::TOO_MANY_REQUESTS {
                slot.sampled = false;
            }
            resp
        }
        Err(reason) => {
            metrics::counter!("maskarad_rejected_total", "reason" => reason.as_str()).increment(1);
            admission.reject(req, reason).await
        }
    }
}

pub struct Admission {
    /// `server.max_inflight`.
    ceiling: usize,
    total: AtomicUsize,
    /// `maskarad_inflight`, registered once: the macro looks the gauge up in
    /// the recorder's registry on every call.
    inflight: metrics::Gauge,
    max_body_bytes: usize,
    /// `None` with `server.admission.enabled: false`.
    adaptive: Option<Adaptive>,
}

struct Adaptive {
    short: AtomicUsize,
    /// The short lane's bound, `ceiling` while the gradient is disengaged.
    /// Stored under the `gradient` lock.
    limit: AtomicUsize,
    large: Semaphore,
    large_body_bytes: u64,
    large_wait: Duration,
    window: Window,
    epoch: Instant,
    next_update_ns: AtomicU64,
    /// Smoothed runtime lag from the probe, µs; `NO_PROBE` until it runs.
    lag_us: AtomicU64,
    gradient: Mutex<Gradient>,
}

/// The current window. `acc` is `count << COUNT_SHIFT | sum_us`: one
/// `fetch_add` per sample, and the updater swaps out a count and a sum that
/// belong together.
struct Window {
    acc: AtomicU64,
    shed: AtomicU64,
    lag_peak_us: AtomicU64,
}

/// 2^20 samples in a 100 ms window is ten million requests a second, and
/// 2^44 µs is 200 days: with every sample clamped to a minute, the sum of one
/// window's samples stays below that while fewer than 290 000 requests are
/// in flight.
const COUNT_SHIFT: u32 = 44;
const SUM_MASK: u64 = (1 << COUNT_SHIFT) - 1;
const MAX_SAMPLE_US: u64 = 60_000_000;

const NO_PROBE: u64 = u64::MAX;
const PROBE_PERIOD: Duration = Duration::from_millis(5);
/// Weight of the newest probe; about five probes, 25-50 ms, of memory.
const PROBE_ALPHA: f64 = 0.2;
/// How long the rest of a shed request's body may take to arrive; a client
/// slower than that loses the connection.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    Short,
    Large,
    Ceiling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    AdaptiveLimit,
    LargeLaneFull,
    InflightLimit,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Reason::AdaptiveLimit => "adaptive_limit",
            Reason::LargeLaneFull => "large_lane_full",
            Reason::InflightLimit => "inflight_limit",
        }
    }
}

impl Admission {
    pub fn new(server: &ServerSection) -> Self {
        let cfg = &server.admission;
        let adaptive = cfg.enabled.then(|| {
            let gradient = Gradient::new(
                cfg.min_limit,
                server.max_inflight,
                Duration::from_millis(cfg.target_ms),
            );
            // The runtime has as many workers: large texts may take every
            // core and no more, and short requests keep a fair share of each.
            let large_limit = cfg.large_limit.unwrap_or_else(|| {
                std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
            });
            Adaptive {
                short: AtomicUsize::new(0),
                limit: AtomicUsize::new(gradient.limit()),
                large: Semaphore::new(large_limit),
                large_body_bytes: server.blocking_threshold_bytes as u64,
                large_wait: Duration::from_millis(server.request_timeout_ms / 2),
                window: Window {
                    acc: AtomicU64::new(0),
                    shed: AtomicU64::new(0),
                    lag_peak_us: AtomicU64::new(0),
                },
                epoch: Instant::now(),
                next_update_ns: AtomicU64::new(duration_ns(UPDATE_INTERVAL)),
                lag_us: AtomicU64::new(NO_PROBE),
                gradient: Mutex::new(gradient),
            }
        });
        metrics::gauge!("maskarad_admission_limit").set(server.max_inflight as f64);
        Self {
            ceiling: server.max_inflight,
            total: AtomicUsize::new(0),
            inflight: metrics::gauge!("maskarad_inflight"),
            max_body_bytes: server.max_body_bytes,
            adaptive,
        }
    }

    /// Starts the runtime lag probe that engages the adaptive limit; without
    /// it the short lane is bounded by `server.max_inflight` only. Needs a
    /// running Tokio runtime; ends when the admission is dropped.
    pub fn start_lag_probe(self: &Arc<Self>) {
        if self.adaptive.is_some() {
            tokio::spawn(probe_lag(Arc::downgrade(self)));
        }
    }

    fn lane_for(&self, req: &Request) -> Lane {
        let Some(a) = &self.adaptive else {
            return Lane::Ceiling;
        };
        if content_length(req.headers()).is_some_and(|n| n > a.large_body_bytes) {
            Lane::Large
        } else {
            Lane::Short
        }
    }

    fn admit(&self, lane: Lane) -> Result<Slot<'_>, Reason> {
        if let (Lane::Short, Some(a)) = (lane, &self.adaptive) {
            let limit = a.limit.load(Relaxed);
            if try_increment(&a.short, limit).is_none() {
                a.window.shed.fetch_add(1, Relaxed);
                // Disengaged, the bound is the ceiling itself.
                return Err(if limit < self.ceiling {
                    Reason::AdaptiveLimit
                } else {
                    Reason::InflightLimit
                });
            }
        }
        if try_increment(&self.total, self.ceiling).is_none() {
            if lane == Lane::Short {
                self.release_short();
            }
            return Err(Reason::InflightLimit);
        }
        Ok(self.slot(lane))
    }

    /// Takes a place under the ceiling, then waits for the large lane. The
    /// slot exists before the wait, so a request dropped while waiting (a
    /// timeout, a client gone) gives its place back in `Drop`.
    async fn admit_large(&self) -> Result<Slot<'_>, Reason> {
        let a = self
            .adaptive
            .as_ref()
            .expect("the large lane exists only with admission enabled");
        if try_increment(&self.total, self.ceiling).is_none() {
            return Err(Reason::InflightLimit);
        }
        let mut slot = self.slot(Lane::Large);
        match tokio::time::timeout(a.large_wait, a.large.acquire()).await {
            Ok(Ok(permit)) => {
                slot.permit = Some(permit);
                Ok(slot)
            }
            Ok(Err(_)) | Err(_) => Err(Reason::LargeLaneFull),
        }
    }

    fn slot(&self, lane: Lane) -> Slot<'_> {
        self.inflight.increment(1.0);
        Slot {
            admission: self,
            lane,
            started: Instant::now(),
            sampled: lane == Lane::Short,
            permit: None,
        }
    }

    fn release_short(&self) {
        if let Some(a) = &self.adaptive {
            a.short.fetch_sub(1, Relaxed);
        }
    }

    /// A shed request never reaches a handler, so its request id and its only
    /// log line are made here.
    ///
    /// The body is read and dropped first. hyper keeps a connection only if
    /// the request was read to the end, and after the response it reads once
    /// more, what one read brings; for a longer body it closes the socket with
    /// the rest unread, and the peer gets a reset instead of the 429: `Broken
    /// pipe` for a 455 KB body in review, a failed upstream write (a 502) for
    /// ingress-nginx. Not read: a body over `server.max_body_bytes` (it would
    /// get a 413 anyway), one of unknown length, and one a client with
    /// `Expect: 100-continue` has not sent yet and will not.
    async fn reject(&self, req: Request, reason: Reason) -> Response {
        let (parts, body) = req.into_parts();
        let len = content_length(&parts.headers).unwrap_or(0);
        if len > 0 && len <= self.max_body_bytes as u64 && !expects_continue(&parts) {
            let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain(body)).await;
        }
        let e = ApiError::rate_limited();
        let rid = request_id();
        let route = parts
            .extensions
            .get::<MatchedPath>()
            .map_or(UNMATCHED, MatchedPath::as_str);
        tracing::warn!(
            request_id = %rid,
            route,
            code = e.code,
            status = e.status.as_u16(),
            reason = reason.as_str(),
            "request rejected"
        );
        logged(e.with_request_id(&rid))
    }
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

fn expects_continue(parts: &Parts) -> bool {
    parts
        .headers
        .get(EXPECT)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"100-continue"))
}

/// Reads `body` to the end without keeping it. Its length is the declared
/// `Content-Length`, which hyper enforces.
async fn drain(mut body: Body) {
    while let Some(Ok(_)) = std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)).await {}
}

/// How long a task that yields waits to run again, every `PROBE_PERIOD`:
/// the run queue ahead of it. Every woken task waits the same, an admitted
/// request after a store call as well as a connection with a new request to
/// admit or shed. Timer lateness would be the same wait plus up to 2 ms of
/// timer rounding, as much as the signal itself.
async fn probe_lag(admission: Weak<Admission>) {
    let mut ewma_us: Option<f64> = None;
    let mut recent = [0u64; ENGAGE_PROBES];
    for n in 0usize.. {
        tokio::time::sleep(PROBE_PERIOD).await;
        let yielded = Instant::now();
        tokio::task::yield_now().await;
        let lag_us = yielded.elapsed().as_secs_f64() * 1e6;
        let smoothed = ewma_us.map_or(lag_us, |e| e + PROBE_ALPHA * (lag_us - e));
        ewma_us = Some(smoothed);
        recent[n % ENGAGE_PROBES] = lag_us as u64;
        let sustained = recent.iter().copied().min().unwrap_or(0);
        let Some(admission) = admission.upgrade() else {
            return;
        };
        if let Some(a) = &admission.adaptive {
            a.observe_lag(smoothed as u64, sustained);
        }
    }
}

/// Increments `counter` unless it has reached `limit`; the previous value.
fn try_increment(counter: &AtomicUsize, limit: usize) -> Option<usize> {
    counter
        .fetch_update(Relaxed, Relaxed, |n| (n < limit).then_some(n + 1))
        .ok()
}

fn duration_ns(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

impl Adaptive {
    /// Never waits: the holder may be a thread descheduled mid-update (a CPU
    /// quota allows 100 ms of that), and both callers can skip a turn.
    fn try_gradient(&self) -> Option<MutexGuard<'_, Gradient>> {
        match self.gradient.try_lock() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// `lag_us` is the probe's EWMA, `sustained_us` the shortest of its last
    /// `ENGAGE_PROBES` waits.
    fn observe_lag(&self, lag_us: u64, sustained_us: u64) {
        self.lag_us.store(lag_us, Relaxed);
        self.window.lag_peak_us.fetch_max(lag_us, Relaxed);
        let Some(mut gradient) = self.try_gradient() else {
            return;
        };
        let now = Instant::now().saturating_duration_since(self.epoch);
        let in_lane = self.short.load(Relaxed);
        if let Some(limit) = gradient.observe_lag(lag_us, sustained_us, in_lane, now) {
            self.publish(limit);
        }
    }

    /// Called with the `gradient` lock held, so that the probe and the
    /// updater publish in the order they decided.
    fn publish(&self, limit: usize) {
        self.limit.store(limit, Relaxed);
        metrics::gauge!("maskarad_admission_limit").set(limit as f64);
    }

    fn record(&self, started: Instant) {
        let now = Instant::now();
        let rtt_us = u64::try_from(now.saturating_duration_since(started).as_micros())
            .unwrap_or(u64::MAX)
            .clamp(1, MAX_SAMPLE_US);
        self.window
            .acc
            .fetch_add((1 << COUNT_SHIFT) + rtt_us, Relaxed);
        let now_ns = duration_ns(now.saturating_duration_since(self.epoch));
        let due = self.next_update_ns.load(Relaxed);
        if now_ns >= due
            && self
                .next_update_ns
                .compare_exchange(due, now_ns + duration_ns(UPDATE_INTERVAL), Relaxed, Relaxed)
                .is_ok()
        {
            self.update(now_ns);
        }
    }

    /// Runs in the CAS winner only.
    fn update(&self, now_ns: u64) {
        let Some(mut gradient) = self.try_gradient() else {
            return;
        };
        if self.window.acc.load(Relaxed) >> COUNT_SHIFT < MIN_SAMPLES {
            return;
        }
        let acc = self.window.acc.swap(0, Relaxed);
        let lag_us = self.lag_us.load(Relaxed);
        // The next window starts from the lag of the moment rather than from
        // zero, so that a window the probe is late for is not clean.
        let lag_peak_us = match self.window.lag_peak_us.swap(lag_us, Relaxed) {
            NO_PROBE => 0,
            peak => peak,
        };
        let stats = WindowStats {
            count: acc >> COUNT_SHIFT,
            sum_us: acc & SUM_MASK,
            shed: self.window.shed.swap(0, Relaxed),
            in_lane: self.short.load(Relaxed),
            lag_peak_us,
        };
        let limit = gradient.update(stats, Duration::from_nanos(now_ns));
        self.publish(limit);
        if let Some(rtt) = gradient.rtt_short() {
            metrics::gauge!("maskarad_admission_rtt_seconds").set(rtt.as_secs_f64());
        }
        if let Some(rtt) = gradient.rtt_noload() {
            metrics::gauge!("maskarad_admission_rtt_noload_seconds").set(rtt.as_secs_f64());
        }
        if lag_us != NO_PROBE {
            metrics::gauge!("maskarad_admission_lag_seconds").set(lag_us as f64 / 1e6);
        }
    }
}

/// An admitted request, counted in `maskarad_inflight` and in its lane while
/// it lives. Everything happens in `Drop` because `TimeoutLayer` sits outside
/// this middleware: on timeout it drops the request future at
/// `next.run(req).await`, and code after that await never runs. Such a
/// request (and one the client abandoned) keeps `sampled`: it took as long as
/// it took.
struct Slot<'a> {
    admission: &'a Admission,
    lane: Lane,
    started: Instant,
    sampled: bool,
    /// The large lane's place, returned with the slot.
    permit: Option<SemaphorePermit<'a>>,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let admission = self.admission;
        admission.inflight.decrement(1.0);
        admission.total.fetch_sub(1, Relaxed);
        if self.lane == Lane::Short {
            admission.release_short();
            if let (true, Some(a)) = (self.sampled, &admission.adaptive) {
                a.record(self.started);
            }
        }
    }
}

#[cfg(test)]
impl Admission {
    pub fn in_flight(&self) -> usize {
        self.total.load(Relaxed)
    }

    pub fn limit(&self) -> usize {
        self.adaptive
            .as_ref()
            .map_or(self.ceiling, |a| a.limit.load(Relaxed))
    }

    fn window_count(&self) -> u64 {
        self.adaptive
            .as_ref()
            .map_or(0, |a| a.window.acc.load(Relaxed) >> COUNT_SHIFT)
    }

    fn large_free(&self) -> usize {
        self.adaptive
            .as_ref()
            .map_or(0, |a| a.large.available_permits())
    }

    fn lag_us(&self) -> u64 {
        self.adaptive
            .as_ref()
            .map_or(NO_PROBE, |a| a.lag_us.load(Relaxed))
    }

    /// As if the probe saw the CPU congested with the lane as it is now.
    fn congest(&self) {
        if let Some(a) = &self.adaptive {
            a.observe_lag(gradient::LAG_CONGESTED_US, gradient::LAG_CONGESTED_US);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::logging::{capture, RequestLogged};
    use axum::body::Bytes;
    use axum::routing::post;
    use axum::Router;
    use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
    use std::sync::atomic::AtomicBool;
    use tokio::sync::{oneshot, Notify};
    use tower::ServiceExt;
    use tower_http::timeout::TimeoutLayer;

    /// Tests that measure time take turns: a test spinning on another thread
    /// takes the CPU away from them, which the probe rightly reads as
    /// congestion.
    async fn timing() -> tokio::sync::MutexGuard<'static, ()> {
        static TIMING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        TIMING.lock().await
    }

    fn admission(server_yaml: &str) -> Arc<Admission> {
        let yaml = format!(
            "server:\n{server_yaml}\nsystems:\n  - id: a\n    auth: {{ anonymous: true }}\n"
        );
        let cfg = ServerConfig::from_yaml_str(&yaml).expect("test config");
        Arc::new(Admission::new(&cfg.server))
    }

    fn request() -> Request {
        Request::post("/process").body(Body::empty()).unwrap()
    }

    fn sized(bytes: usize) -> Request {
        Request::post("/process")
            .header(CONTENT_LENGTH, bytes)
            .body(Body::from(vec![b' '; bytes]))
            .unwrap()
    }

    /// `/process` whose handler signals when it is entered and then waits
    /// for a permit of `release`, one per request.
    fn holding_app(
        admission: Arc<Admission>,
        release: Arc<Semaphore>,
    ) -> (Router, oneshot::Receiver<()>) {
        let (entered_tx, entered_rx) = oneshot::channel::<()>();
        let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
        let app = Router::new()
            .route(
                "/process",
                post(move || {
                    let entered_tx = entered_tx.clone();
                    let release = release.clone();
                    async move {
                        if let Some(tx) = entered_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        release.acquire().await.unwrap().forget();
                        "done"
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(admission, admit));
        (app, entered_rx)
    }

    #[tokio::test]
    async fn a_request_over_the_limit_is_a_logged_429() {
        let (_guard, logs) = capture::start();
        let release = Arc::new(Semaphore::new(0));
        let (app, entered) = holding_app(
            admission("  max_inflight: 1\n  admission: { enabled: false }"),
            release.clone(),
        );

        let first = tokio::spawn(app.clone().oneshot(request()));
        entered
            .await
            .expect("the first request holds the only slot");

        let resp = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers()["Retry-After"], "1");
        assert!(resp.extensions().get::<RequestLogged>().is_some());
        let header_rid = resp.headers()["X-Request-Id"].to_str().unwrap().to_string();
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "RATE_LIMITED");
        assert_eq!(body["error"]["request_id"], header_rid.as_str());

        let rejected = logs.with_message("request rejected");
        assert_eq!(rejected.len(), 1, "one line per shed request");
        let line = &rejected[0];
        assert_eq!(line["level"], "WARN");
        assert_eq!(line["route"], "/process");
        assert_eq!(line["status"], 429);
        assert_eq!(line["code"], "RATE_LIMITED");
        assert_eq!(line["reason"], "inflight_limit");
        assert_eq!(line["request_id"], header_rid.as_str());

        release.add_permits(1);
        let resp = first.await.unwrap().unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        release.add_permits(1);
        let resp = app.oneshot(request()).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "the slot is returned after the first request"
        );
    }

    /// Disengaged, a full short lane is the ceiling: `inflight_limit`.
    /// Engaged, the same lane sheds with `adaptive_limit`.
    #[tokio::test]
    async fn a_full_short_lane_sheds_with_its_reason() {
        let (_guard, logs) = capture::start();
        let admission = admission("  max_inflight: 1\n  admission: { min_limit: 1 }");
        let release = Arc::new(Semaphore::new(0));
        let (app, entered) = holding_app(admission.clone(), release.clone());
        let first = tokio::spawn(app.clone().oneshot(request()));
        entered.await.unwrap();

        let resp = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        release.add_permits(1);
        assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);

        let admission = self::admission("  max_inflight: 8\n  admission: { min_limit: 1 }");
        let (app, entered) = holding_app(admission.clone(), release.clone());
        let first = tokio::spawn(app.clone().oneshot(request()));
        entered.await.unwrap();
        admission.congest();
        assert_eq!(admission.limit(), 2, "twice the one in flight");
        let second = tokio::spawn(app.clone().oneshot(request()));
        while admission.in_flight() < 2 {
            tokio::task::yield_now().await;
        }
        let resp = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(resp.headers()["Retry-After"], "1");

        let rejected = logs.with_message("request rejected");
        let reasons: Vec<_> = rejected.iter().map(|l| l["reason"].clone()).collect();
        assert_eq!(reasons, ["inflight_limit", "adaptive_limit"]);
        release.add_permits(2);
        assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
        assert_eq!(second.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    /// A large text holds its lane: the next one waits its turn instead of
    /// being shed, a short one gets in meanwhile, and neither large request
    /// is a latency sample.
    #[tokio::test]
    async fn large_bodies_wait_their_turn_and_are_not_sampled() {
        let admission = admission(
            "  max_inflight: 8\n  request_timeout_ms: 60000\n  blocking_threshold_bytes: 100\n  admission: { min_limit: 1, large_limit: 1 }",
        );
        let release = Arc::new(Semaphore::new(0));
        let (app, entered) = holding_app(admission.clone(), release.clone());

        let first = tokio::spawn(app.clone().oneshot(sized(101)));
        entered.await.unwrap();
        assert_eq!(admission.large_free(), 0);
        let second = tokio::spawn(app.clone().oneshot(sized(500)));
        while admission.in_flight() < 2 {
            tokio::task::yield_now().await;
        }

        release.add_permits(1);
        assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
        // The second one holds the lane now and waits in the handler.
        while admission.large_free() == 1 || admission.in_flight() != 1 {
            tokio::task::yield_now().await;
        }
        let short = tokio::spawn(app.clone().oneshot(sized(100)));
        while admission.in_flight() < 2 {
            tokio::task::yield_now().await;
        }
        release.add_permits(2);
        assert_eq!(second.await.unwrap().unwrap().status(), StatusCode::OK);
        assert_eq!(short.await.unwrap().unwrap().status(), StatusCode::OK);
        assert_eq!(admission.large_free(), 1);
        assert_eq!(admission.window_count(), 1, "only the short one");
        assert_eq!(admission.in_flight(), 0);
    }

    /// Half of `request_timeout_ms` is as long as a large request waits.
    #[tokio::test]
    async fn a_large_request_waits_at_most_half_the_timeout() {
        let _timing = timing().await;
        let (_guard, logs) = capture::start();
        let admission = admission(
            "  max_inflight: 8\n  request_timeout_ms: 200\n  blocking_threshold_bytes: 100\n  admission: { large_limit: 1 }",
        );
        let release = Arc::new(Semaphore::new(0));
        let (app, entered) = holding_app(admission.clone(), release.clone());
        let first = tokio::spawn(app.clone().oneshot(sized(101)));
        entered.await.unwrap();

        let started = Instant::now();
        let resp = app.clone().oneshot(sized(500)).await.unwrap();
        let waited = started.elapsed();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            waited >= Duration::from_millis(100) && waited < Duration::from_millis(190),
            "waited {waited:?}"
        );
        let rejected = logs.with_message("request rejected");
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0]["reason"], "large_lane_full");
        assert_eq!(admission.in_flight(), 1, "the shed one gave its place back");

        release.add_permits(1);
        assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn client_errors_are_not_sampled_but_store_busy_is() {
        let admission = admission("  max_inflight: 8");
        let app = Router::new()
            .route("/process", post(|| async { StatusCode::BAD_REQUEST }))
            .route("/v1/mask", post(|| async { StatusCode::TOO_MANY_REQUESTS }))
            .layer(axum::middleware::from_fn_with_state(
                admission.clone(),
                admit,
            ));
        let resp = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(admission.window_count(), 0);
        let req = Request::post("/v1/mask").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(admission.window_count(), 1);
        assert_eq!(admission.in_flight(), 0);
    }

    fn inflight_gauge(metrics: &PrometheusHandle) -> f64 {
        metrics
            .render()
            .lines()
            .find_map(|l| l.strip_prefix("maskarad_inflight "))
            .expect("maskarad_inflight is rendered")
            .parse()
            .expect("a number")
    }

    /// `TimeoutLayer` sits outside this middleware, as in `main`: on timeout
    /// it drops the request future while the handler is still running, or
    /// while a large request still waits for its lane.
    #[tokio::test]
    async fn a_timed_out_request_gives_back_its_slot() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let metrics = recorder.handle();
        let _recorder = metrics::set_default_local_recorder(&recorder);
        let admission = admission(
            "  max_inflight: 2\n  request_timeout_ms: 60000\n  blocking_threshold_bytes: 100\n  admission: { min_limit: 1, large_limit: 1 }",
        );
        let app = Router::new()
            .route("/process", post(std::future::pending::<&'static str>))
            .layer(axum::middleware::from_fn_with_state(
                admission.clone(),
                admit,
            ))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::SERVICE_UNAVAILABLE,
                Duration::from_millis(20),
            ));

        let resp = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(inflight_gauge(&metrics), 0.0);
        assert_eq!(admission.in_flight(), 0);
        assert_eq!(admission.window_count(), 1, "a timeout is a latency sample");

        let (first, second) = tokio::join!(app.clone().oneshot(sized(101)), async {
            tokio::task::yield_now().await;
            app.clone().oneshot(sized(101)).await
        });
        assert_eq!(first.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(second.unwrap().status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(inflight_gauge(&metrics), 0.0);
        assert_eq!(admission.in_flight(), 0);
        assert_eq!(admission.large_free(), 1);
    }

    fn percentile(sorted: &[Duration], p: f64) -> Duration {
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    }

    fn spin(d: Duration) {
        let started = Instant::now();
        while started.elapsed() < d {
            std::hint::spin_loop();
        }
    }

    /// `/process` behind the same layers as in `main`.
    fn loaded_app<H, T>(admission: Arc<Admission>, handler: H) -> Router
    where
        H: axum::handler::Handler<T, ()>,
        T: 'static,
    {
        Router::new()
            .route("/process", post(handler))
            .layer(axum::middleware::from_fn_with_state(admission, admit))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::SERVICE_UNAVAILABLE,
                Duration::from_secs(5),
            ))
            .layer(axum::middleware::from_fn(crate::logging::log_unlogged))
    }

    /// `clients` send one request after another for `total`; a shed one
    /// waits 5 ms (the real client waits a second, a few milliseconds keep
    /// the pressure on without spinning). Latencies of admitted requests
    /// sent after `warmup`, sorted, and the number shed.
    async fn drive(
        app: Router,
        clients: usize,
        warmup: Duration,
        total: Duration,
    ) -> (Vec<Duration>, usize) {
        let stop = Arc::new(AtomicBool::new(false));
        let measure_from = Instant::now() + warmup;
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..clients {
            let (app, stop) = (app.clone(), stop.clone());
            set.spawn(async move {
                let (mut admitted, mut shed) = (Vec::new(), 0usize);
                while !stop.load(Relaxed) {
                    let sent = Instant::now();
                    let resp = app.clone().oneshot(request()).await.unwrap();
                    match resp.status() {
                        StatusCode::OK if sent >= measure_from => admitted.push(sent.elapsed()),
                        StatusCode::OK => {}
                        StatusCode::TOO_MANY_REQUESTS => {
                            assert_eq!(resp.headers()["Retry-After"], "1");
                            assert!(resp.headers().contains_key("X-Request-Id"));
                            assert!(resp.extensions().get::<RequestLogged>().is_some());
                            shed += 1;
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                        other => panic!("unexpected {other}"),
                    }
                }
                (admitted, shed)
            });
        }
        tokio::time::sleep(total).await;
        stop.store(true, Relaxed);
        let (mut admitted, mut shed) = (Vec::new(), 0);
        while let Some(r) = set.join_next().await {
            let (a, s) = r.unwrap();
            admitted.extend(a);
            shed += s;
        }
        admitted.sort();
        (admitted, shed)
    }

    /// One core (a current-thread runtime), CPU-bound requests with an await
    /// in the middle, as `/process` has around the store. 100 clients keep
    /// sending; under the ceiling alone every request would wait behind ~100
    /// others twice, ~80 ms. The adaptive limit has to engage, shed the
    /// excess and keep admitted requests within a few targets.
    #[tokio::test]
    async fn cpu_overload_is_shed_and_admitted_latency_stays_bounded() {
        let _timing = timing().await;
        let admission = admission("  max_inflight: 2048\n  admission: { target_ms: 5 }");
        admission.start_lag_probe();
        let app = loaded_app(admission.clone(), || async {
            spin(Duration::from_micros(200));
            tokio::task::yield_now().await;
            spin(Duration::from_micros(200));
            "done"
        });

        let (admitted, shed) = drive(
            app,
            100,
            Duration::from_millis(2500),
            Duration::from_millis(3500),
        )
        .await;
        let (p50, p99) = (percentile(&admitted, 0.5), percentile(&admitted, 0.99));
        let limit = admission.limit();
        assert!(shed > 0, "overload is shed");
        assert!(admitted.len() > 300, "admitted {}", admitted.len());
        assert!(
            p99 < Duration::from_millis(25),
            "admitted p50 {p50:?}, p99 {p99:?}, limit {limit}"
        );
        assert!(limit < 64, "limit {limit}");
        assert_ne!(admission.lag_us(), NO_PROBE);
        assert_eq!(admission.in_flight(), 0);
    }

    /// Requests wait 50 ms for something else (a slow store) and the CPU is
    /// nearly idle: latency is ten times the target, but fewer requests would
    /// not wait less, so nothing is shed.
    #[tokio::test]
    async fn latency_without_cpu_congestion_is_not_shed() {
        let _timing = timing().await;
        let admission = admission("  max_inflight: 128\n  admission: { target_ms: 5 }");
        admission.start_lag_probe();
        // 40-60 ms, so that the clients drift apart instead of all waking in
        // the same millisecond: a hundred debug-build requests at once are
        // 5-10 ms of CPU, a real burst the probe rightly sees.
        let n = Arc::new(AtomicU64::new(0));
        let app = loaded_app(admission.clone(), move || {
            let n = n.fetch_add(1, Relaxed);
            async move {
                tokio::time::sleep(Duration::from_micros(40_000 + n * 7_919 % 20_000)).await;
                "done"
            }
        });

        let (admitted, shed) = drive(
            app,
            100,
            Duration::from_millis(500),
            Duration::from_millis(2000),
        )
        .await;
        let limit = admission.limit();
        assert_ne!(admission.lag_us(), NO_PROBE);
        assert!(limit >= 100, "limit {limit}");
        assert!(
            shed * 20 < admitted.len(),
            "shed {shed} of {}",
            admitted.len()
        );
    }

    /// The store stops answering for 1.5 s under a steady 500 requests/s:
    /// ~750 requests pile up in the lane while the CPU idles, then all of
    /// them finish in a burst of CPU work that congests the runtime. Nothing
    /// is shed; before the ceiling and the adaptive limit were separated, the
    /// lane was capped at 256 and shed the pile-up (135-145 pairs lost to a
    /// 3.5 s Valkey pause at 625 requests/s in review).
    #[tokio::test]
    async fn a_store_stall_at_low_load_is_not_shed() {
        let _timing = timing().await;
        let admission = admission("  max_inflight: 2048");
        admission.start_lag_probe();
        let stalled = Arc::new(AtomicBool::new(false));
        let resumed = Arc::new(Notify::new());
        let app = {
            let (stalled, resumed) = (stalled.clone(), resumed.clone());
            loaded_app(admission.clone(), move || {
                let (stalled, resumed) = (stalled.clone(), resumed.clone());
                async move {
                    spin(Duration::from_micros(100));
                    let wake = resumed.notified();
                    if stalled.load(Relaxed) {
                        wake.await;
                    }
                    spin(Duration::from_micros(100));
                    "done"
                }
            })
        };

        let mut sent = tokio::task::JoinSet::new();
        let mut tick = tokio::time::interval(Duration::from_millis(2));
        let started = Instant::now();
        let mut peak = 0;
        let mut was_engaged = false;
        while started.elapsed() < Duration::from_secs(4) {
            tick.tick().await;
            let at = started.elapsed();
            let stall = at >= Duration::from_millis(500) && at < Duration::from_millis(2000);
            if stalled.swap(stall, Relaxed) && !stall {
                resumed.notify_waiters();
            }
            peak = peak.max(admission.in_flight());
            was_engaged |= admission.limit() < 2048;
            let app = app.clone();
            sent.spawn(async move { app.oneshot(request()).await.unwrap().status() });
        }
        let mut statuses = std::collections::BTreeMap::new();
        while let Some(status) = sent.join_next().await {
            *statuses.entry(status.unwrap()).or_insert(0) += 1;
        }
        assert!(peak > 500, "the stall piled up only {peak}");
        assert_eq!(
            statuses.keys().collect::<Vec<_>>(),
            [&StatusCode::OK],
            "{statuses:?}, peak {peak}, engaged {was_engaged}"
        );
    }

    #[tokio::test]
    async fn lag_probe_sees_a_blocked_runtime_and_engages_until_it_clears() {
        let _timing = timing().await;
        let admission = admission("  max_inflight: 64\n  admission: { min_limit: 1 }");
        assert_eq!(admission.lag_us(), NO_PROBE);
        admission.start_lag_probe();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let idle = admission.lag_us();
        assert_ne!(idle, NO_PROBE);
        assert_eq!(admission.limit(), 64);
        // Four tasks that take turns on the CPU for 150 ms: every probe waits
        // behind them, ~2 ms.
        let hogs: Vec<_> = (0..4)
            .map(|_| {
                tokio::spawn(async {
                    let started = Instant::now();
                    while started.elapsed() < Duration::from_millis(150) {
                        spin(Duration::from_micros(500));
                        tokio::task::yield_now().await;
                    }
                })
            })
            .collect();
        for hog in hogs {
            hog.await.unwrap();
        }
        let blocked = admission.lag_us();
        assert!(
            blocked >= gradient::LAG_CONGESTED_US && blocked > idle,
            "idle {idle} µs, blocked {blocked} µs"
        );
        assert_eq!(admission.limit(), 1, "engaged at twice the empty lane");
        // A second of a free CPU; tests on other threads can take it away
        // for a moment and restart the count.
        let freed = Instant::now();
        while admission.limit() != 64 && freed.elapsed() < Duration::from_secs(10) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let released = freed.elapsed();
        assert_eq!(admission.limit(), 64, "not released in {released:?}");
        assert!(released >= Duration::from_millis(950), "{released:?}");
    }

    /// Sends a request the way ingress-nginx and Python's urllib do: the
    /// whole body first, then reads the response. Its status line, or the
    /// I/O error that ended it.
    async fn send_whole(addr: std::net::SocketAddr, bytes: usize) -> std::io::Result<String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut conn = tokio::io::BufStream::new(tokio::net::TcpStream::connect(addr).await?);
        let head = format!("POST /process HTTP/1.1\r\nHost: t\r\nContent-Length: {bytes}\r\n\r\n");
        conn.write_all(head.as_bytes()).await?;
        conn.write_all(&vec![b' '; bytes]).await?;
        conn.flush().await?;
        let mut status = String::new();
        conn.read_line(&mut status).await?;
        Ok(status.trim_end().to_string())
    }

    /// Over real TCP: a shed request's body is read before the 429, so the
    /// client gets the 429 rather than a reset connection. 16 MiB is more
    /// than loopback socket buffers hold, so an unread body leaves the client
    /// blocked in its write when the server closes. `oneshot` does not show
    /// any of this: there is no socket to close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shed_body_is_read_before_the_429() {
        const BIG: usize = 16 << 20;
        let serve = |yaml: &str| {
            let admission = admission(yaml);
            let release = Arc::new(Semaphore::new(0));
            let app = Router::new()
                .route(
                    "/process",
                    post({
                        let release = release.clone();
                        move |_body: Bytes| async move {
                            release.acquire().await.unwrap().forget();
                            "done"
                        }
                    }),
                )
                .layer(axum::middleware::from_fn_with_state(
                    admission.clone(),
                    admit,
                ));
            async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(async move { axum::serve(listener, app).await });
                (addr, admission, release)
            }
        };

        // The large lane, one text at a time: the second waits 100 ms and is shed.
        let (addr, admission, release) = serve(
            "  max_inflight: 8\n  max_body_bytes: 33554432\n  request_timeout_ms: 200\n  blocking_threshold_bytes: 1024\n  admission: { large_limit: 1 }",
        )
        .await;
        let first = tokio::spawn(send_whole(addr, 2048));
        while admission.large_free() == 1 {
            tokio::task::yield_now().await;
        }
        let status = send_whole(addr, BIG)
            .await
            .expect("a response, not a reset");
        assert_eq!(status, "HTTP/1.1 429 Too Many Requests");
        release.add_permits(1);
        assert_eq!(first.await.unwrap().unwrap(), "HTTP/1.1 200 OK");

        // The ceiling, with admission off: the same path.
        let (addr, admission, release) =
            serve("  max_inflight: 1\n  max_body_bytes: 33554432\n  admission: { enabled: false }")
                .await;
        let first = tokio::spawn(send_whole(addr, 10));
        while admission.in_flight() == 0 {
            tokio::task::yield_now().await;
        }
        let status = send_whole(addr, BIG)
            .await
            .expect("a response, not a reset");
        assert_eq!(status, "HTTP/1.1 429 Too Many Requests");
        release.add_permits(1);
        assert_eq!(first.await.unwrap().unwrap(), "HTTP/1.1 200 OK");
    }
}
