//! Prometheus metrics: latency, RPS, TPS, detected PII types, store and
//! upstream health. Labels never carry request content.

use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};

pub fn install() -> anyhow::Result<PrometheusHandle> {
    let buckets = [
        0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
    ];
    let handle = PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_seconds".into()), &buckets)?
        .install_recorder()?;
    metrics::describe_histogram!(
        "maskarad_request_duration_seconds",
        "End-to-end request latency by route, system and direction"
    );
    metrics::describe_histogram!(
        "maskarad_stage_duration_seconds",
        "Latency of processing stages (detect_mask, demask, store_get, store_put, llm)"
    );
    metrics::describe_counter!(
        "maskarad_requests_total",
        "Requests by route and status (rate() gives RPS)"
    );
    metrics::describe_counter!(
        "maskarad_tokens_total",
        "Approximate tokens processed by direction (rate() gives TPS)"
    );
    metrics::describe_counter!(
        "maskarad_pii_detected_total",
        "Detected entities by type and system"
    );
    metrics::describe_counter!("maskarad_rejected_total", "Rejected requests by reason");
    metrics::describe_counter!(
        "maskarad_store_ops_total",
        "Mapping store operations by op and result"
    );
    metrics::describe_counter!(
        "maskarad_store_retries_total",
        "Redis commands retried once after a timeout, by op"
    );
    metrics::describe_gauge!(
        "maskarad_store_degraded",
        "1 when the primary store is unavailable and memory fallback is active"
    );
    metrics::describe_gauge!(
        "maskarad_inflight",
        "Requests admitted and not yet answered, large ones waiting for their lane included"
    );
    metrics::describe_gauge!(
        "maskarad_admission_limit",
        "Bound of short requests in flight: the adaptive limit while the CPU is congested, server.max_inflight otherwise"
    );
    metrics::describe_gauge!(
        "maskarad_admission_rtt_seconds",
        "Smoothed latency of short requests the adaptive limit follows"
    );
    metrics::describe_gauge!(
        "maskarad_admission_lag_seconds",
        "How late a woken task gets the CPU (smoothed); the adaptive limit engages at 1 ms and is released after 1 s under 0.5 ms"
    );
    metrics::describe_gauge!(
        "maskarad_admission_rtt_noload_seconds",
        "Mean latency of short requests without a queue of their own: the minimum over clean windows (limit released, CPU not congested) of the last 30-60 s that had any; an overload keeps the value from before it"
    );
    metrics::describe_counter!(
        "maskarad_llm_requests_total",
        "Upstream LLM requests by result"
    );
    Ok(handle)
}

/// Cheap token estimate: alphanumeric runs and standalone punctuation marks.
pub fn count_tokens(text: &str) -> usize {
    let mut n = 0;
    let mut in_word = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if !in_word {
                n += 1;
                in_word = true;
            }
        } else {
            in_word = false;
            if !c.is_whitespace() {
                n += 1;
            }
        }
    }
    n
}

pub fn observe_request(
    route: &'static str,
    system: &str,
    direction: &'static str,
    status: u16,
    seconds: f64,
) {
    metrics::histogram!("maskarad_request_duration_seconds", "route" => route, "system" => system.to_string(), "direction" => direction)
        .record(seconds);
    metrics::counter!("maskarad_requests_total", "route" => route, "status" => status.to_string())
        .increment(1);
}

pub fn observe_stage(stage: &'static str, seconds: f64) {
    metrics::histogram!("maskarad_stage_duration_seconds", "stage" => stage).record(seconds);
}

pub fn observe_tokens(direction: &'static str, tokens: usize) {
    metrics::counter!("maskarad_tokens_total", "direction" => direction).increment(tokens as u64);
}

pub fn observe_pii(system: &str, ty: &str, count: usize) {
    metrics::counter!("maskarad_pii_detected_total", "type" => ty.to_string(), "system" => system.to_string()).increment(count as u64);
}

#[cfg(test)]
mod tests {
    use super::count_tokens;

    #[test]
    fn token_estimate() {
        assert_eq!(count_tokens("Клиент Иванов, паспорт 4509 123456."), 7);
    }
}
