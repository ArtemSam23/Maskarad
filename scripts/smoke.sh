#!/usr/bin/env bash
# Проверка живого окружения по контракту /process: readyz, маскирование по примеру
# из контракта, демаскирование, обработка невалидного запроса.
#   scripts/smoke.sh https://maskarad.tech
set -euo pipefail
URL="${1:?usage: smoke.sh <base-url>}"
URL="${URL%/}"
# SMOKE_CURL_OPTS="-k" — для окружения, где сертификат ещё не выпущен.
curl() { command curl ${SMOKE_CURL_OPTS:-} "$@"; }
fail() { echo "SMOKE FAIL: $*" >&2; exit 1; }

ready="$(curl -fsS -m 15 "$URL/readyz")" || fail "readyz unreachable"
echo "$ready" | jq -e '.status == "ok" or .status == "degraded"' >/dev/null || fail "readyz: $ready"
echo "$ready" | jq -e '.status == "ok"' >/dev/null || echo "WARN: service is degraded: $ready" >&2

id="smoke-$(date +%s)-$RANDOM"
orig='Клиент Иванов Иван Иванович, паспорт 4509 123456'
expected='Клиент И. И. И., паспорт 45** ****56'
post() { curl -fsS -m 15 -X POST "$URL/process" -H 'Content-Type: application/json' -d "$(jq -cn --arg p "$1" --arg id "$2" '{payload: $p, payload_id: $id}')"; }

masked="$(post "$orig" "$id" | jq -r .result)"
[ "$masked" = "$expected" ] || fail "mask: got '$masked', expected '$expected'"
again="$(post "$orig" "$id" | jq -r .result)"
[ "$again" = "$expected" ] || fail "mask retry is not idempotent: '$again'"
restored="$(post "$masked" "$id" | jq -r .result)"
[ "$restored" = "$orig" ] || fail "demask: got '$restored'"

code="$(curl -s -o /dev/null -m 15 -w '%{http_code}' -X POST "$URL/process" -H 'Content-Type: application/json' -d '{"payload":')"
[ "$code" = "400" ] || fail "invalid JSON returned HTTP $code, expected 400"
code="$(curl -s -o /dev/null -m 15 -w '%{http_code}' -X POST "$URL/v1/mask" -H 'Content-Type: application/json' -d '{"text":"x"}')"
[ "$code" = "401" ] || fail "unauthenticated /v1/mask returned HTTP $code, expected 401"
# Тело /metrics (~30 КБ) читаем целиком: в конвейере `curl | grep -q` grep выходит на первом
# совпадении, curl получает EPIPE на остатке ответа и завершается кодом 23, а pipefail
# превращает это в ложный «metrics missing».
metrics="$(curl -fsS -m 15 "$URL/metrics")" || fail "metrics unreachable"
grep -q '^maskarad_requests_total' <<<"$metrics" || fail "metrics missing"
echo "SMOKE OK: $URL"
