//! End-to-end check of a running deployment by the `/process` contract:
//! readiness, masking of the contract example, idempotent retry, demasking,
//! error handling and metrics. Meant to run next to the service (a Kubernetes
//! Job) so the check does not depend on the network path from CI runners.

use std::time::{Duration, Instant};

const ORIGINAL: &str = "Клиент Иванов Иван Иванович, паспорт 4509 123456";
const EXPECTED: &str = "Клиент И. И. И., паспорт 45** ****56";

pub struct SmokeArgs {
    pub url: String,
    pub insecure: bool,
    pub timeout_secs: u64,
}

async fn send(
    what: &str,
    build: impl Fn() -> reqwest::RequestBuilder,
) -> anyhow::Result<reqwest::Response> {
    let mut last = None;
    for attempt in 1..=3 {
        match build().send().await {
            Ok(resp) => return Ok(resp),
            Err(err) => {
                eprintln!("{what}: attempt {attempt} failed: {err}");
                last = Some(err);
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
    anyhow::bail!("{what}: unreachable after 3 attempts: {}", last.unwrap())
}

async fn process(
    client: &reqwest::Client,
    url: &str,
    payload: &str,
    id: &str,
) -> anyhow::Result<String> {
    let body = serde_json::json!({ "payload": payload, "payload_id": id });
    let resp = send("POST /process", || client.post(url).json(&body)).await?;
    let status = resp.status();
    let text = resp.text().await?;
    anyhow::ensure!(
        status.is_success(),
        "POST /process returned HTTP {status}: {text}"
    );
    let value: serde_json::Value = serde_json::from_str(&text)?;
    value["result"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("POST /process: no `result` in {text}"))
}

pub async fn run(args: SmokeArgs) -> anyhow::Result<()> {
    let base = args.url.trim_end_matches('/').to_string();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(args.timeout_secs))
        .danger_accept_invalid_certs(args.insecure)
        .build()?;
    let started = Instant::now();

    let ready = send("GET /readyz", || client.get(format!("{base}/readyz"))).await?;
    let ready_status = ready.status();
    let ready_body = ready.text().await?;
    anyhow::ensure!(
        ready_status.is_success(),
        "readyz returned HTTP {ready_status}: {ready_body}"
    );
    let ready_json: serde_json::Value = serde_json::from_str(&ready_body)?;
    match ready_json["status"].as_str() {
        Some("ok") => {}
        Some("degraded") => eprintln!("warning: service is degraded: {ready_body}"),
        _ => anyhow::bail!("readyz: unexpected body {ready_body}"),
    }

    let process_url = format!("{base}/process");
    let id = format!(
        "smoke-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        rand::random::<u32>()
    );
    let masked = process(&client, &process_url, ORIGINAL, &id).await?;
    anyhow::ensure!(
        masked == EXPECTED,
        "mask: got {masked:?}, expected {EXPECTED:?}"
    );
    let again = process(&client, &process_url, ORIGINAL, &id).await?;
    anyhow::ensure!(again == EXPECTED, "mask retry is not idempotent: {again:?}");
    let restored = process(&client, &process_url, &masked, &id).await?;
    anyhow::ensure!(restored == ORIGINAL, "demask: got {restored:?}");

    let invalid = send("POST /process (invalid JSON)", || {
        client
            .post(&process_url)
            .header("Content-Type", "application/json")
            .body("{\"payload\":")
    })
    .await?;
    anyhow::ensure!(
        invalid.status().as_u16() == 400,
        "invalid JSON returned HTTP {}, expected 400",
        invalid.status()
    );

    let unauthenticated = send("POST /v1/mask (no key)", || {
        client
            .post(format!("{base}/v1/mask"))
            .json(&serde_json::json!({ "text": "x" }))
    })
    .await?;
    anyhow::ensure!(
        unauthenticated.status().as_u16() == 401,
        "unauthenticated /v1/mask returned HTTP {}, expected 401",
        unauthenticated.status()
    );

    let metrics = send("GET /metrics", || client.get(format!("{base}/metrics")))
        .await?
        .text()
        .await?;
    anyhow::ensure!(
        metrics
            .lines()
            .any(|l| l.starts_with("maskarad_requests_total")),
        "metrics: maskarad_requests_total missing"
    );

    println!("SMOKE OK: {base} ({} ms)", started.elapsed().as_millis());
    Ok(())
}
