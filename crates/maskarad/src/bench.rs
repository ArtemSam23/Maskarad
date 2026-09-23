//! Load generator reproducing the checker protocol: pairs of `/process`
//! requests (mask, then demask with the same `payload_id`), up to three
//! attempts per request, `Retry-After` honoured on 429, demasked text
//! compared with the original. Open-loop constant arrival rate.

use crate::golden;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Clone)]
pub struct BenchArgs {
    pub url: String,
    pub rps: u64,
    pub duration: u64,
    pub dataset: PathBuf,
    pub max_inflight: usize,
    pub timeout_secs: u64,
    pub export_corpus: Option<PathBuf>,
    /// Generate the corpus in memory (records per category) instead of reading `dataset`.
    pub generate: Option<usize>,
    pub insecure: bool,
}

#[derive(Default)]
struct Counters {
    mask_ok: AtomicU64,
    demask_ok: AtomicU64,
    mask_failed: AtomicU64,
    demask_failed: AtomicU64,
    rate_limited: AtomicU64,
    retries: AtomicU64,
    timeouts: AtomicU64,
    mismatches: AtomicU64,
    shed: AtomicU64,
}

#[derive(Default)]
struct Latencies {
    mask: Vec<f64>,
    demask: Vec<f64>,
}

#[derive(Serialize)]
struct Corpus<'a> {
    text: &'a str,
}

enum Outcome {
    Ok(String),
    Failed,
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    payload: &str,
    id: &str,
    counters: &Counters,
    lat: &mut Vec<f64>,
) -> Outcome {
    let body = serde_json::json!({ "payload": payload, "payload_id": id });
    for attempt in 0..3 {
        if attempt > 0 {
            counters.retries.fetch_add(1, Ordering::Relaxed);
        }
        let started = Instant::now();
        match client.post(url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if status == 429 {
                    counters.rate_limited.fetch_add(1, Ordering::Relaxed);
                    let wait = resp
                        .headers()
                        .get("Retry-After")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(1);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }
                let text = resp.text().await.unwrap_or_default();
                lat.push(started.elapsed().as_secs_f64() * 1000.0);
                if status == 200 {
                    let result = serde_json::from_str::<serde_json::Value>(&text)
                        .ok()
                        .and_then(|v| {
                            v.get("result")
                                .and_then(|r| r.as_str())
                                .map(|s| s.to_string())
                        });
                    return match result {
                        Some(r) => Outcome::Ok(r),
                        None => Outcome::Failed,
                    };
                }
                return Outcome::Failed;
            }
            Err(e) => {
                if e.is_timeout() {
                    counters.timeouts.fetch_add(1, Ordering::Relaxed);
                }
                lat.push(started.elapsed().as_secs_f64() * 1000.0);
            }
        }
    }
    Outcome::Failed
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn summary(name: &str, values: &mut [f64]) -> String {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    format!(
        "| {name} | {} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} |",
        values.len(),
        percentile(values, 0.5),
        percentile(values, 0.95),
        percentile(values, 0.99),
        percentile(values, 0.999),
        values.last().copied().unwrap_or(0.0)
    )
}

pub async fn run(args: BenchArgs) -> anyhow::Result<()> {
    let records = match args.generate {
        Some(per_category) => {
            let dict = maskarad_core::Dictionaries::builtin();
            let mut records = golden::Generator::new(42, &dict).generate(per_category);
            records.extend(golden::curated(&dict));
            records
        }
        None => golden::load(&args.dataset)?,
    };
    anyhow::ensure!(!records.is_empty(), "dataset is empty");
    let texts: Arc<Vec<String>> = Arc::new(records.iter().map(|r| r.text.clone()).collect());
    if let Some(path) = &args.export_corpus {
        let corpus: Vec<Corpus<'_>> = texts.iter().map(|t| Corpus { text: t }).collect();
        std::fs::write(path, serde_json::to_string(&corpus)?)?;
        println!(
            "corpus with {} texts written to {}",
            corpus.len(),
            path.display()
        );
    }
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(args.max_inflight.max(64))
        .timeout(Duration::from_secs(args.timeout_secs))
        .tcp_nodelay(true)
        .danger_accept_invalid_certs(args.insecure)
        .build()?;
    let url = format!("{}/process", args.url.trim_end_matches('/'));
    let counters = Arc::new(Counters::default());
    let latencies = Arc::new(Mutex::new(Latencies::default()));
    let permits = Arc::new(Semaphore::new(args.max_inflight));
    let mut tasks = JoinSet::new();

    println!(
        "bench: {} pairs/s for {} s against {} ({} texts, max in flight {})",
        args.rps,
        args.duration,
        url,
        texts.len(),
        args.max_inflight
    );
    let started = Instant::now();
    let deadline = started + Duration::from_secs(args.duration);
    let mut interval = tokio::time::interval(Duration::from_secs_f64(1.0 / args.rps as f64));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    let mut n: u64 = 0;
    let run_id: u64 = rand::random();
    while Instant::now() < deadline {
        interval.tick().await;
        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
            counters.shed.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let (client, url, texts, counters, latencies) = (
            client.clone(),
            url.clone(),
            Arc::clone(&texts),
            Arc::clone(&counters),
            Arc::clone(&latencies),
        );
        let idx = (n as usize) % texts.len();
        let id = format!("bench-{run_id:x}-{n}");
        n += 1;
        tasks.spawn(async move {
            let _permit = permit;
            let text = &texts[idx];
            let mut lat_mask = Vec::with_capacity(1);
            let mut lat_demask = Vec::with_capacity(1);
            let masked = match post(&client, &url, text, &id, &counters, &mut lat_mask).await {
                Outcome::Ok(m) => {
                    counters.mask_ok.fetch_add(1, Ordering::Relaxed);
                    Some(m)
                }
                Outcome::Failed => {
                    counters.mask_failed.fetch_add(1, Ordering::Relaxed);
                    None
                }
            };
            if let Some(masked) = masked {
                match post(&client, &url, &masked, &id, &counters, &mut lat_demask).await {
                    Outcome::Ok(restored) => {
                        counters.demask_ok.fetch_add(1, Ordering::Relaxed);
                        if restored != *text {
                            counters.mismatches.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    Outcome::Failed => {
                        counters.demask_failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            let mut l = latencies.lock().unwrap();
            l.mask.extend(lat_mask);
            l.demask.extend(lat_demask);
        });
        // Reap finished tasks so the set does not grow without bound.
        while tasks.try_join_next().is_some() {}
    }
    let submit_elapsed = started.elapsed();
    let drain = tokio::time::timeout(Duration::from_secs(args.timeout_secs + 5), async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    if drain.is_err() {
        println!("warning: some requests did not finish within the drain timeout");
    }
    let elapsed = started.elapsed();
    let c = &counters;
    let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let requests =
        get(&c.mask_ok) + get(&c.mask_failed) + get(&c.demask_ok) + get(&c.demask_failed);
    let mut l = latencies.lock().unwrap();
    println!();
    println!("| Метрика | Значение |\n|---|---:|");
    println!(
        "| Целевая интенсивность | {} пар/с = {} запросов/с |",
        args.rps,
        args.rps * 2
    );
    println!("| Пар отправлено | {n} |");
    println!(
        "| Запросов выполнено | {requests} ({:.0} запросов/с за {:.1} с) |",
        requests as f64 / submit_elapsed.as_secs_f64(),
        elapsed.as_secs_f64()
    );
    println!(
        "| Маскирование 200 / ошибок | {} / {} |",
        get(&c.mask_ok),
        get(&c.mask_failed)
    );
    println!(
        "| Демаскирование 200 / ошибок | {} / {} |",
        get(&c.demask_ok),
        get(&c.demask_failed)
    );
    println!("| Демаскирование ≠ оригинал | {} |", get(&c.mismatches));
    println!("| Ответов 429 | {} |", get(&c.rate_limited));
    println!("| Повторных попыток | {} |", get(&c.retries));
    println!(
        "| Таймаутов ({} с) | {} |",
        args.timeout_secs,
        get(&c.timeouts)
    );
    println!("| Не отправлено (лимит генератора) | {} |", get(&c.shed));
    println!();
    println!(
        "| Latency, мс | n | p50 | p95 | p99 | p99.9 | max |\n|---|---:|---:|---:|---:|---:|---:|"
    );
    println!("{}", summary("маскирование", &mut l.mask));
    println!("{}", summary("демаскирование", &mut l.demask));
    Ok(())
}
