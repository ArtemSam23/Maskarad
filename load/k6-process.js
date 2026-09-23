// Нагрузка по протоколу проверяющей системы (ТЗ, прил. B): пары mask → demask с одним payload_id,
// до 3 попыток на запрос, при 429 ожидание Retry-After, таймаут 10 с, восстановленный текст
// сравнивается с исходным. Открытый цикл (arrival rate): интенсивность не зависит от latency сервера,
// поэтому очередь копится в dropped_iterations, а не прячется в замедлении генератора.
//
// Переменные окружения:
//   URL           адрес сервиса, по умолчанию https://maskarad.tech
//   STAGES        ступени "длительность:пар/с[,...]", по умолчанию "5m:500" (= 1000 запросов/с).
//                 Каждая ступень линейно ведёт интенсивность от предыдущей цели к своей; первая
//                 начинается с START_RATE (по умолчанию равен её цели, то есть полка).
//                 Полка после роста — повтор цели: "2m:750,3m:750".
//   KEEPALIVE     1 (по умолчанию) — соединения переиспользуются; 0 — новое TLS-соединение на запрос
//   BIG_PER_10S   больших пар за 10 с параллельно основному потоку; 0 (по умолчанию) — поток выключен.
//                 Поток информационный: его счётчики и latency идут отдельно и в гейт вердикта не входят.
//   TAG           метка прогона: часть payload_id и User-Agent; по умолчанию k6-<время старта>.
//                 Должна быть уникальной между прогонами: payload_id с прошлого прогона ещё живут в
//                 хранилище (TTL 3600 с), и сервис уйдёт в ветку demask, не маскируя.
//   LOG_MISMATCH  1 — писать в лог первые несовпадения (оригинал, маска, ответ)
//   PREVUS/MAXVUS VU: по умолчанию max(цель)×1 и max(цель)×4, не больше 6000
//   CORPUS_PATH   ./corpus.json — массив [{text}] (jq -c -n '[inputs | {text: .text}]' tests/golden/*.jsonl)
//   BIGTEXT_PATH  ./bigtext.txt — базовый большой текст; если файла нет, большой поток выключен
//
// Итоги: текстовый отчёт в stdout и JSON-сводка между ===K6_SUMMARY_BEGIN=== / ===K6_SUMMARY_END===
// (workflow ops → loadtest вырезает её из лога Job'а). В сводке run_start_ms/run_end_ms — окно самого
// прогона без init VU: по нему workflow строит серверный отчёт.
import http from "k6/http";
import { sleep } from "k6";
import { SharedArray } from "k6/data";
import { Counter, Trend, Rate } from "k6/metrics";

const URL = __ENV.URL || "https://maskarad.tech";
const KEEPALIVE = (__ENV.KEEPALIVE || "1") !== "0";
const BIG_PER_10S = Number(__ENV.BIG_PER_10S || 0);
const CORPUS_PATH = __ENV.CORPUS_PATH || "./corpus.json";
const BIGTEXT_PATH = __ENV.BIGTEXT_PATH || "./bigtext.txt";
const STAGES_RAW = __ENV.STAGES || "5m:500";
const VU_CEILING = 6000;
const TIMEOUT_MS = 10000;      // таймаут проверяющей
const BIG_TIMEOUT_MS = 40000;  // большие тексты: 16× корпуса маскируется дольше 10 с, это не ошибка

function seconds(d) {
  const m = /^(\d+(?:\.\d+)?)(ms|s|m|h)?$/.exec(String(d).trim());
  if (!m) throw new Error(`STAGES: bad duration "${d}" (expected 30s, 5m, 1h)`);
  return Number(m[1]) * { ms: 0.001, s: 1, m: 60, h: 3600 }[m[2] || "s"];
}

const STAGES = STAGES_RAW.split(",").map((s) => {
  const [d, t] = s.split(":");
  const target = Number(t);
  if (!d || !t || Number.isNaN(target) || target < 0) throw new Error(`STAGES: bad stage "${s}" (expected 5m:500)`);
  return { duration: d.trim(), target };
});
const START_RATE = Number(__ENV.START_RATE || STAGES[0].target);
const totalSeconds = STAGES.reduce((a, s) => a + seconds(s.duration), 0);
// Ожидаемое число пар при линейных ступенях — для вердикта «фактическая интенсивность ≥ 95 % цели».
const expectedPairs = STAGES.reduce((acc, s) => {
  const secs = seconds(s.duration);
  acc.pairs += ((acc.prev + s.target) / 2) * secs;
  acc.prev = s.target;
  return acc;
}, { pairs: 0, prev: START_RATE }).pairs;
const peakRate = Math.max(START_RATE, ...STAGES.map((s) => s.target));
const PREVUS = Number(__ENV.PREVUS || Math.min(VU_CEILING, Math.max(10, peakRate)));
const MAXVUS = Number(__ENV.MAXVUS || Math.min(VU_CEILING, Math.max(PREVUS, peakRate * 4)));

// SharedArray: один экземпляр на процесс, иначе каждый VU держит свою копию корпуса.
// Через него же раздаём метку прогона, чтобы Date.now() не различался между VU.
const TAG = new SharedArray("run", () => [__ENV.TAG || `k6-${Date.now().toString(36)}`])[0];
const corpus = new SharedArray("corpus", () => {
  const items = JSON.parse(open(CORPUS_PATH));
  if (!Array.isArray(items) || items.length === 0 || typeof items[0].text !== "string") {
    throw new Error(`${CORPUS_PATH}: expected a non-empty array of {text}`);
  }
  return items;
});
// Большие тексты: 1× ≈ 14k токенов, 4× ≈ 55k, 8× ≈ 118k, 16× ≈ 236k (оценка сервиса ~0.2 токена/символ).
const big = new SharedArray("big", () => {
  if (BIG_PER_10S <= 0) return [];
  // open здесь — k6 init API (строка), а не DOM window.open.
  /** @type {string} */
  let base;
  try {
    base = open(BIGTEXT_PATH);
  } catch (e) {
    console.warn(`big stream disabled: cannot read ${BIGTEXT_PATH} (${e})`);
    return [];
  }
  return [
    { size: "1x", text: base },
    { size: "4x", text: (base + " ").repeat(4) },
    { size: "8x", text: (base + " ").repeat(8) },
    { size: "16x", text: (base + " ").repeat(16) },
  ];
});
const bigEnabled = big.length > 0;
const BIG_WEIGHTS = [0.4, 0.3, 0.2, 0.1];

// Основной поток — то, по чему выносится вердикт.
const maskLatency = new Trend("mask_latency", true);      // первая попытка, мс
const demaskLatency = new Trend("demask_latency", true);
const pairLatency = new Trend("pair_latency", true);      // вся пара с ретраями, мс
const pairFailed = new Counter("pair_failed");            // пара не завершена за 3 попытки
const mismatches = new Counter("demask_mismatch");
const maskFirstTry = new Rate("mask_first_try_ok");
const demaskFirstTry = new Rate("demask_first_try_ok");
const protoHttp1 = new Rate("proto_http1");               // доля ответов по HTTP/1.1 (как у проверяющей)
// Счётчики ответов у каждого потока свои: пара 16× корпуса, упавшая по таймауту, не должна портить
// invalid_responses основного потока — большой поток информационный, не гейт.
const MAIN = {
  rateLimited: new Counter("rate_limited"),               // ответов 429
  retries: new Counter("retries"),                        // повторных попыток
  invalid: new Counter("invalid_responses"),              // не 200 и не 429: 5xx, таймаут, обрыв
  timeouts: new Counter("timeouts"),
};

const bigMask = new Trend("big_mask_latency", true);
const bigDemask = new Trend("big_demask_latency", true);
const bigPairFailed = new Counter("big_pair_failed");
const bigMismatch = new Counter("big_mismatch");
const bigOk = new Rate("big_pair_ok");
const BIG = {
  rateLimited: new Counter("big_rate_limited"),
  retries: new Counter("big_retries"),
  invalid: new Counter("big_invalid"),
  timeouts: new Counter("big_timeouts"),
  over10s: new Counter("big_over_10s"),                   // первых попыток дольше таймаута проверяющей (10 с)
};

export const options = {
  noConnectionReuse: !KEEPALIVE,
  userAgent: `maskarad-loadtest/${TAG}`,
  summaryTrendStats: ["avg", "min", "med", "p(90)", "p(95)", "p(99)", "p(99.9)", "max"],
  scenarios: {
    pairs: {
      executor: "ramping-arrival-rate",
      exec: "pair",
      startRate: START_RATE,
      timeUnit: "1s",
      stages: STAGES,
      preAllocatedVUs: PREVUS,
      maxVUs: MAXVUS,
    },
  },
  thresholds: {
    invalid_responses: ["count<1"],
    demask_mismatch: ["count<1"],
  },
};

if (bigEnabled) {
  options.scenarios.big = {
    executor: "constant-arrival-rate",
    exec: "bigPair",
    rate: BIG_PER_10S,
    timeUnit: "10s",
    duration: `${totalSeconds}s`,
    preAllocatedVUs: 10,
    maxVUs: 60,
    gracefulStop: "40s",
  };
  // Порог на каждый размер нужен, чтобы k6 положил sub-метрику {size:…} в сводку: без threshold
  // тегированные срезы в data.metrics не попадают. В вердикт эти пороги не входят (см. thresholds_failed).
  for (const item of big) {
    options.thresholds[`big_mask_latency{size:${item.size}}`] = ["p(95)<10000"];
    options.thresholds[`big_demask_latency{size:${item.size}}`] = ["p(95)<10000"];
  }
}

// post — до 3 попыток запроса; latency и firstTry пополняются по первой попытке, счётчики c — по всем.
// Возвращает ответ 200 или null, если за 3 попытки его не было.
// У таймаута status 0 и duration без смысла: в тренд идёт сам таймаут, иначе самые медленные
// запросы выпадали бы из выборки и тянули p99 вниз. Обрыв соединения (status 0, другой
// error_code) в latency не попадает — это не время сервера.
function recordFirstAttempt(res, timedOut, latency, firstTry, tags, timeoutMs, c) {
  if (res.status > 0) latency.add(res.timings.duration, tags);
  else if (timedOut) latency.add(timeoutMs, tags);
  if (firstTry) firstTry.add(res.status === 200, tags);
  if (c.over10s && (timedOut || (res.status > 0 && res.timings.duration > TIMEOUT_MS))) c.over10s.add(1, tags);
}

function post(payload, id, latency, firstTry, tags, timeoutMs, c) {
  const body = JSON.stringify({ payload, payload_id: id });
  for (let attempt = 0; attempt < 3; attempt++) {
    if (attempt > 0) c.retries.add(1, tags);
    const res = http.post(`${URL}/process`, body, {
      headers: { "Content-Type": "application/json" },
      timeout: `${timeoutMs}ms`,
      tags: { name: "process", ...tags },
    });
    const timedOut = res.status === 0 && res.error_code === 1050;
    if (attempt === 0) recordFirstAttempt(res, timedOut, latency, firstTry, tags, timeoutMs, c);
    if (res.status > 0) protoHttp1.add(res.proto === "HTTP/1.1", tags);
    if (res.status === 200) return res;
    if (res.status === 429) {
      c.rateLimited.add(1, tags);
      sleep(Number(res.headers["Retry-After"] || 1));
      continue;
    }
    c.invalid.add(1, { status: String(res.status), ...tags });
    if (timedOut) c.timeouts.add(1, tags);
    sleep(0.2);
  }
  return null;
}

export function pair() {
  const item = corpus[Math.floor(Math.random() * corpus.length)];
  const id = `${TAG}-${__VU}-${__ITER}`;
  const tags = { kind: "small" };
  const started = Date.now();
  const masked = post(item.text, id, maskLatency, maskFirstTry, tags, TIMEOUT_MS, MAIN);
  if (!masked) { pairFailed.add(1, tags); return; }
  const maskedText = masked.json("result");
  if (typeof maskedText !== "string") { pairFailed.add(1, tags); MAIN.invalid.add(1, tags); return; }
  const restored = post(maskedText, id, demaskLatency, demaskFirstTry, tags, TIMEOUT_MS, MAIN);
  if (!restored) { pairFailed.add(1, tags); return; }
  pairLatency.add(Date.now() - started, tags);
  const back = restored.json("result");
  if (back !== item.text) {
    mismatches.add(1, tags);
    if (__ENV.LOG_MISMATCH && __VU <= 60 && __ITER < 300) {
      const kind = back === maskedText ? "UNCHANGED(=masked)" : "OTHER";
      console.warn(`MISMATCH id=${id} kind=${kind} dir=${restored.headers["X-Maskarad-Direction"]} degraded=${restored.headers["X-Maskarad-Degraded"]} rid=${restored.headers["X-Request-Id"]} | orig=${item.text} | masked=${maskedText} | back=${back}`);
    }
  }
}

function pickBig() {
  let r = Math.random();
  for (let i = 0; i < big.length; i++) { r -= BIG_WEIGHTS[i]; if (r <= 0) return big[i]; }
  return big[big.length - 1];
}

export function bigPair() {
  const item = pickBig();
  const id = `${TAG}-big-${__VU}-${__ITER}`;
  const tags = { kind: "big", size: item.size };
  const masked = post(item.text, id, bigMask, null, tags, BIG_TIMEOUT_MS, BIG);
  if (!masked) { bigPairFailed.add(1, tags); bigOk.add(false, tags); return; }
  const maskedText = masked.json("result");
  if (typeof maskedText !== "string") { bigPairFailed.add(1, tags); BIG.invalid.add(1, tags); bigOk.add(false, tags); return; }
  const restored = post(maskedText, id, bigDemask, null, tags, BIG_TIMEOUT_MS, BIG);
  if (!restored) { bigPairFailed.add(1, tags); bigOk.add(false, tags); return; }
  const ok = restored.json("result") === item.text;
  if (!ok) bigMismatch.add(1, tags);
  bigOk.add(ok, tags);
}

// --- итоги -------------------------------------------------------------------------------------
// Свой текстовый отчёт вместо textSummary из jslib.k6.io: удалённый import с узла кластера ненадёжен.

const TREND_KEYS = ["avg", "min", "med", "p(90)", "p(95)", "p(99)", "p(99.9)", "max"];

function fmt(n, digits) {
  if (n === null || n === undefined || Number.isNaN(n)) return "-";
  return Number(n).toFixed(digits === undefined ? 2 : digits);
}

function metricValues(m, name) {
  return m[name]?.values || {};
}

function trendSummary(m, name) {
  const v = metricValues(m, name);
  if (v.med === undefined && v["p(99)"] === undefined) return null;
  return {
    avg: v.avg, min: v.min, p50: v.med, p90: v["p(90)"], p95: v["p(95)"], p99: v["p(99)"],
    p999: v["p(99.9)"], max: v.max,
  };
}

function count(m, name) {
  const v = metricValues(m, name);
  return v.count === undefined ? 0 : v.count;
}

function metricBody(metric) {
  const v = metric.values || {};
  switch (metric.type) {
    case "trend":
      return TREND_KEYS.filter((k) => v[k] !== undefined).map((k) => `${k}=${fmt(v[k])}`).join(" ") + (metric.contains === "time" ? " ms" : "");
    case "counter":
      return `count=${fmt(v.count, 0)} rate=${fmt(v.rate)}/s`;
    case "rate":
      return `rate=${fmt(v.rate * 100)}% passes=${fmt(v.passes, 0)} fails=${fmt(v.fails, 0)}`;
    default:
      return `value=${fmt(v.value)} min=${fmt(v.min)} max=${fmt(v.max)}`;
  }
}

function thresholdMarks(metric) {
  let marks = "";
  for (const [expr, t] of Object.entries(metric.thresholds || {})) marks += ` ${t.ok ? "✓" : "✗"} ${expr}`;
  return marks;
}

function textReport(data) {
  const m = data.metrics || {};
  const durationMs = data.state ? data.state.testRunDurationMs : null;
  const bigLabel = bigEnabled ? `${BIG_PER_10S}/10s` : "off";
  const lines = [
    "",
    `maskarad loadtest ${TAG}: ${URL}, stages ${STAGES_RAW} (start ${START_RATE} pairs/s), keepalive ${KEEPALIVE ? "on" : "off"}, big ${bigLabel}, VUs ${PREVUS}..${MAXVUS}, ran ${fmt(durationMs / 1000, 1)}s of ${totalSeconds}s planned`,
    "",
  ];
  for (const name of Object.keys(m).sort((a, b) => a.localeCompare(b))) {
    lines.push(`${name.padEnd(36)} ${metricBody(m[name])}${thresholdMarks(m[name])}`);
  }
  const checks = data.root_group?.checks || [];
  for (const c of checks) lines.push(`check ${c.name}: passes=${c.passes} fails=${c.fails}`);
  lines.push("");
  return lines.join("\n");
}

// Пороги большого потока — отдельно: он информационный, и его провал не должен читаться как провал прогона.
function splitFailedThresholds(m) {
  const main = [];
  const bigOnly = [];
  for (const [name, metric] of Object.entries(m)) {
    for (const [expr, t] of Object.entries(metric.thresholds || {})) {
      if (!t.ok) (name.startsWith("big_") ? bigOnly : main).push(`${name}: ${expr}`);
    }
  }
  return { main, bigOnly };
}

function bigStreamSummary(m, thresholdsFailed) {
  const sizes = {};
  for (const item of big) {
    sizes[item.size] = {
      mask_ms: trendSummary(m, `big_mask_latency{size:${item.size}}`),
      demask_ms: trendSummary(m, `big_demask_latency{size:${item.size}}`),
    };
  }
  const ok = metricValues(m, "big_pair_ok");
  return {
    pairs: Math.round((ok.passes || 0) + (ok.fails || 0)),
    pair_failed: count(m, "big_pair_failed"),
    invalid: count(m, "big_invalid"),
    timeouts: count(m, "big_timeouts"),
    rate_limited: count(m, "big_rate_limited"),
    retries: count(m, "big_retries"),
    mismatch: count(m, "big_mismatch"),
    over_10s: count(m, "big_over_10s"),
    sizes,
    thresholds_failed: thresholdsFailed,
  };
}

export function handleSummary(data) {
  const m = data.metrics || {};
  const failed = splitFailedThresholds(m);
  const vusMax = metricValues(m, "vus_max");
  const durationMs = data.state ? data.state.testRunDurationMs : null;
  // testRunDurationMs считается от старта прогона (init VU уже позади) до его конца; handleSummary
  // вызывается сразу после, поэтому Date.now() — конец окна с точностью до долей секунды.
  const runEndMs = Date.now();
  const summary = {
    tag: TAG,
    url: URL,
    stages: STAGES_RAW,
    start_rate: START_RATE,
    keepalive: KEEPALIVE,
    big_per_10s: bigEnabled ? BIG_PER_10S : 0,
    planned_seconds: totalSeconds,
    // Фактическая длительность включает gracefulStop (k6 дожидается начатых итераций, до 30 с),
    // поэтому actual_rps считается по плановой: хвост без новых итераций занижал бы интенсивность.
    duration_seconds: durationMs === null ? null : durationMs / 1000,
    run_start_ms: durationMs === null ? null : runEndMs - durationMs,
    run_end_ms: runEndMs,
    expected_pairs: Math.round(expectedPairs),
    target_rps_avg: totalSeconds > 0 ? (2 * expectedPairs) / totalSeconds : null,
    iterations: count(m, "iterations"),
    http_reqs: count(m, "http_reqs"),
    actual_rps: totalSeconds > 0 ? count(m, "http_reqs") / totalSeconds : null,
    http_req_failed_rate: metricValues(m, "http_req_failed").rate,
    invalid_responses: count(m, "invalid_responses"),
    timeouts: count(m, "timeouts"),
    rate_limited: count(m, "rate_limited"),
    retries: count(m, "retries"),
    pair_failed: count(m, "pair_failed"),
    demask_mismatch: count(m, "demask_mismatch"),
    dropped_iterations: count(m, "dropped_iterations"),
    mask_first_try_ok: metricValues(m, "mask_first_try_ok").rate,
    demask_first_try_ok: metricValues(m, "demask_first_try_ok").rate,
    proto_http1: metricValues(m, "proto_http1").rate,
    vus_max: vusMax.max === undefined ? vusMax.value : vusMax.max,
    mask_latency_ms: trendSummary(m, "mask_latency"),
    demask_latency_ms: trendSummary(m, "demask_latency"),
    pair_latency_ms: trendSummary(m, "pair_latency"),
    tls_handshake_ms: trendSummary(m, "http_req_tls_handshaking"),
    // Большой поток: информационно, в гейт вердикта не входит.
    big: bigEnabled ? bigStreamSummary(m, failed.bigOnly) : null,
    thresholds_failed: failed.main,
  };
  return {
    stdout: `${textReport(data)}\n===K6_SUMMARY_BEGIN===\n${JSON.stringify(summary)}\n===K6_SUMMARY_END===\n`,
  };
}
