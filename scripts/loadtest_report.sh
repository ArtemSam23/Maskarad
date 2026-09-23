#!/usr/bin/env bash
# Серверная картина нагрузочного прогона за окно [START, END] из Prometheus — markdown для GITHUB_STEP_SUMMARY.
#   scripts/loadtest_report.sh <start_unix> <end_unix> [label] [--stages "<STAGES>"] [--kv <file>]
# Источник (переменные окружения, нужен один из двух):
#   PROM_URL                    Prometheus HTTP API, например http://127.0.0.1:9090 (локальный стенд
#                               deploy/docker-compose.yml)
#   YC_PROMETHEUS_WORKSPACE_ID  воркспейс Managed Prometheus в Monium: URL строится из id,
#                               токен — yc iam create-token (нужен настроенный yc)
#   INGRESS_HOST                хост в метриках ingress-nginx, по умолчанию maskarad.tech
# --stages "3m:500,2m:750" — дополнительно таблица по ступеням, та же нарезка, что STAGES у k6.
# --kv <file>               — ключевые числа для вердикта, строки key=value (ingress_p99_200_s, ...).
# Ошибка любого запроса к Prometheus (нет ответа, не 200, status != success) — «(ошибка)» в ячейке,
# текст в stderr и код выхода 1 в конце: пустой отчёт легко принять за «всё по нулям».
set -euo pipefail

usage() {
  sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
}

STAGES=""; KV=""; positional=()
while [ $# -gt 0 ]; do
  case "$1" in
    --stages) STAGES="${2:?--stages needs a value}"; shift 2 ;;
    --kv) KV="${2:?--kv needs a file}"; shift 2 ;;
    -h|--help) usage ;;
    --*) echo "unknown option: $1" >&2; usage ;;
    *) positional+=("$1"); shift ;;
  esac
done
START="${positional[0]:-}"; END="${positional[1]:-}"; LABEL="${positional[2]:-loadtest}"
[[ "$START" =~ ^[0-9]+$ && "$END" =~ ^[0-9]+$ ]] || usage
[ "$END" -gt "$START" ] || { echo "end ($END) must be after start ($START)" >&2; exit 2; }
for tool in curl jq; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 2; }; done

INGRESS_HOST="${INGRESS_HOST:-maskarad.tech}"
CURL=(curl -sS -m 60)
AUTH=()
if [ -n "${PROM_URL:-}" ]; then
  API="${PROM_URL%/}/api/v1/query"
  # localhost не должен ходить через системный прокси
  case "$API" in http://127.0.0.1*|http://localhost*) CURL+=(--noproxy '*') ;; esac
elif [ -n "${YC_PROMETHEUS_WORKSPACE_ID:-}" ]; then
  API="https://monitoring.api.cloud.yandex.net/prometheus/workspaces/$YC_PROMETHEUS_WORKSPACE_ID/api/v1/query"
  # --format json явно: в CI профиль yc переключён на json, и голый `create-token` печатает объект.
  token=$(yc iam create-token --format json | jq -er .iam_token) || { echo "yc iam create-token failed: configure yc first" >&2; exit 1; }
  AUTH=(-H "Authorization: Bearer $token")
else
  echo "set PROM_URL (Prometheus HTTP API) or YC_PROMETHEUS_WORKSPACE_ID" >&2
  exit 2
fi

# Флаги между вызовами query: он работает в $(...), то есть в subshell, и переменные наружу не выйдут.
#   $TMP/failed — хотя бы один запрос не удался (итоговый exit 1);
#   $TMP/get    — сервер не принял POST, дальше ходим GET.
TMP=$(mktemp -d); trap 'rm -rf "$TMP"' EXIT

N=$(( END - START ))
[ "$N" -ge 30 ] || N=30   # короче интервала скрейпа rate() не считается
R="${N}s"
T="$END"

# query <promql> <time> → массив .data.result в stdout; при ошибке — текст в stderr, флаг и код 1.
# Вызывать как rows=$(query ...) || ...: exit внутри $(...) скрипт не завершил бы.
query() {
  local out rc http body
  local -a get; get=()
  [ ! -e "$TMP/get" ] || get=(--get)
  # ${AUTH[@]+...}: пустой массив под set -u в bash 3.2 (macOS) — «unbound variable»
  out=$("${CURL[@]}" ${AUTH[@]+"${AUTH[@]}"} ${get[@]+"${get[@]}"} -w '\n%{http_code}' "$API" \
        --data-urlencode "query=$1" --data-urlencode "time=$2") \
    || { rc=$?; echo "prometheus: request to $API failed (curl exit $rc)" >&2; touch "$TMP/failed"; return 1; }
  http="${out##*$'\n'}"; body="${out%$'\n'*}"
  if [ ! -e "$TMP/get" ] && { [ "$http" = 405 ] || { [[ "$http" =~ ^40[04]$ ]] && grep -qi 'method' <<<"$body"; }; }; then
    # Managed Prometheus по документации принимает и GET, и POST; если POST перестанут — переключаемся один раз.
    echo "prometheus: POST $API answered HTTP $http (${body:0:120}), switching to GET" >&2
    touch "$TMP/get"
    query "$1" "$2"
    return
  fi
  [ "$http" = "200" ] || { echo "prometheus: HTTP $http from $API: ${body:0:300}" >&2; touch "$TMP/failed"; return 1; }
  jq -ec 'select(.status == "success") | .data.result' <<<"$body" \
    || { echo "prometheus: unexpected answer: ${body:0:300}" >&2; touch "$TMP/failed"; return 1; }
}

# jq-функция форматирования: f(множитель; знаков после запятой), NaN и null → "-"
JQ_FMT='def f($mul; $d): if . == null or isnan then "-" else (. * $mul * pow(10; $d) | round / pow(10; $d) | tostring) end;'

# val <promql> [mul] [digits] → первое значение, "-" (нет данных) или "(ошибка)"
val() {
  local rows
  rows=$(query "$1" "$T") || { echo "(ошибка)"; return 0; }
  jq -r --argjson mul "${2:-1}" --argjson d "${3:-2}" "$JQ_FMT"' (.[0].value[1] // null | tonumber? // null) | f($mul; $d)' <<<"$rows"
}

# by_label <promql> <label> [mul] [digits] → строки "label<TAB>value", по алфавиту метки; при ошибке одна строка "(ошибка)"
by_label() {
  local rows
  rows=$(query "$1" "$T") || { printf '(ошибка)\t(ошибка)\n'; return 0; }
  jq -r --arg l "$2" --argjson mul "${3:-1}" --argjson d "${4:-2}" "$JQ_FMT"' sort_by(.metric[$l]) | .[] | "\(.metric[$l] // "(all)")\t\((.value[1] | tonumber? // null) | f($mul; $d))"' <<<"$rows"
}

# join_rows <строки label\tvalue> → "a 1, b 2", "—" или "(ошибка)"
join_rows() {
  local s
  s=$(awk -F'\t' '$1 == "(ошибка)" { printf "(ошибка)"; exit } $1 != "" { printf "%s%s %s", (n++ ? ", " : ""), $1, $2 }' <<<"$1")
  echo "${s:-—}"
}

# rows2 <строки label\tA> <строки label\tB> <префикс> <суффикс A> → строки таблицы "| префикс label | A суффикс | B |"
rows2() {
  { awk '{ print "A\t" $0 }' <<<"$1"; awk '{ print "B\t" $0 }' <<<"$2"; } | awk -F'\t' -v p="$3" -v sfx="$4" '
    $2 == "" { next }
    $1 == "A" { a[$2] = $3; if (!($2 in seen)) { seen[$2] = 1; order[n++] = $2 } }
    $1 == "B" { b[$2] = $3; if (!($2 in seen)) { seen[$2] = 1; order[n++] = $2 } }
    END {
      for (i = 0; i < n; i++) {
        k = order[i]; va = (k in a) ? a[k] : "-"; vb = (k in b) ? b[k] : "-"
        printf "| %s%s | %s%s | %s |\n", p, k, va, (va == "-" || va == "(ошибка)") ? "" : sfx, vb
      }
    }'
}

# ms <секунды с 4 знаками> → "12.3 мс"; "-" и "(ошибка)" проходят как есть
ms() { case "$1" in -|"(ошибка)") echo "$1" ;; *) awk -v s="$1" 'BEGIN { printf "%.1f мс", s * 1000 }' ;; esac; }
fmt_utc() { date -u -d "@$1" '+%H:%M:%S' 2>/dev/null || date -u -r "$1" '+%H:%M:%S'; }
kv() { [ -n "$KV" ] && echo "$1=$2" >> "$KV" || true; }

# Селекторы PromQL — в переменных: литеральные {…} во вложенных "$(… "$(by_label "…" …)" …)" bash 3.2
# (macOS) раскрывает как brace expansion. Ingress фильтруется по host: у агента для nginx нет
# target-метки namespace (метка бэкенда остаётся namespace, а не exported_namespace).
SEL_ING="{host=\"$INGRESS_HOST\"}"
SEL_ING_200="{host=\"$INGRESS_HOST\",status=\"200\"}"
SEL_PROC='{namespace="maskarad",route="/process"}'
SEL_SVC='{namespace="maskarad"}'
SEL_STORE_ERR='{namespace="maskarad",result="error"}'
SEL_POD='{namespace="maskarad",container="maskarad"}'
SEL_ING_POD='{namespace="ingress-nginx",container="controller"}'
SEL_ROOT='{id="/"}'   # корневой cgroup узла из cAdvisor: CPU и память узла целиком
SEL_UP='{job="maskarad"}'
sel_dir() { echo "{namespace=\"maskarad\",route=\"/process\",direction=\"$1\"}"; }

ing_q() { echo "histogram_quantile($1, sum(rate(nginx_ingress_controller_request_duration_seconds_bucket${SEL_ING}[$2])) by (le))"; }
ing200_q() { echo "histogram_quantile($1, sum(rate(nginx_ingress_controller_request_duration_seconds_bucket${SEL_ING_200}[$2])) by (le))"; }
svc_q() { echo "histogram_quantile($1, sum(rate(maskarad_request_duration_seconds_bucket$(sel_dir "$2")[$3])) by (le))"; }
nodes_cpu_q() { echo "sum(rate(container_cpu_usage_seconds_total${SEL_ROOT}[$1])) by (node) / on (node) sum(machine_cpu_cores) by (node)"; }
nodes_mem_q() { echo "max(max_over_time(container_memory_working_set_bytes${SEL_ROOT}[$1])) by (node)"; }
# Реплики — по числу целей скрейпа сервиса (kube-state-metrics нет): subquery с шагом в интервал скрейпа.
replicas_q() { echo "${1}_over_time(count(up${SEL_UP} == 1)[$2:30s])"; }

MB=0.00000095367431640625

[ -z "$KV" ] || : > "$KV"

# Проверка связи до первой таблицы: понятная ошибка вместо пустого отчёта.
probe=$(query 'vector(1)' "$T") || { echo "prometheus: probe query failed ($API)" >&2; exit 1; }
[ "$(jq -r 'length' <<<"$probe")" = "1" ] || { echo "prometheus: probe query returned no data ($API)" >&2; exit 1; }

echo "## Сервер: $LABEL, окно $(fmt_utc "$START")–$(fmt_utc "$END") UTC ($N с)"
echo

# --- ingress ---
echo "### Ingress (host $INGRESS_HOST)"
echo
echo "| Статус | Запросов | Запросов/с |"
echo "|---|---:|---:|"
if rows=$(query "sum(increase(nginx_ingress_controller_requests${SEL_ING}[$R])) by (status)" "$T"); then
  jq -r --argjson n "$N" --arg host "$INGRESS_HOST" "$JQ_FMT"'
    if length == 0 then "| (нет данных за окно: метка host=\"\($host)\" и job ingress-nginx у агента) | | |" else
    [.[] | {s: (.metric.status // "?"), v: ((.value[1] | tonumber? // 0))}] | sort_by(.s) |
    (.[] | select(.v >= 0.5) | "| \(.s) | \(.v | f(1; 0)) | \(.v / $n | f(1; 1)) |"),
    "| всего | \(map(.v) | add // 0 | f(1; 0)) | \((map(.v) | add // 0) / $n | f(1; 1)) |" end' <<<"$rows"
  total=$(jq -r --argjson n "$N" '[.[] | .value[1] | tonumber? // 0] | add // 0 | . / $n' <<<"$rows")
  non2xx=$(jq -r '[.[] | select((.metric.status // "") | startswith("2") | not) | .value[1] | tonumber? // 0] | add // 0 | round' <<<"$rows")
else
  echo "| (ошибка) | | |"; total="(ошибка)"; non2xx="(ошибка)"
fi
kv ingress_rps "$total"
kv ingress_non2xx "$non2xx"
echo

# --- latency ---
echo "### Latency"
echo
echo "| Точка | p50 | p95 | p99 | p99.9 |"
echo "|---|---:|---:|---:|---:|"
ip50=$(val "$(ing_q 0.5 "$R")" 1 4); ip95=$(val "$(ing_q 0.95 "$R")" 1 4)
ip99=$(val "$(ing_q 0.99 "$R")" 1 4); ip999=$(val "$(ing_q 0.999 "$R")" 1 4)
echo "| ingress, все статусы (nginx) | $(ms "$ip50") | $(ms "$ip95") | $(ms "$ip99") | $(ms "$ip999") |"
kv ingress_p50_s "$ip50"; kv ingress_p95_s "$ip95"; kv ingress_p99_s "$ip99"
# Гейт вердикта — по status="200": 429 отвечаются мгновенно и тянут общий p99 вниз, а таймауты
# upstream (504) — вверх; общий p99 выше — справочный.
i200p50=$(val "$(ing200_q 0.5 "$R")" 1 4); i200p95=$(val "$(ing200_q 0.95 "$R")" 1 4)
i200p99=$(val "$(ing200_q 0.99 "$R")" 1 4); i200p999=$(val "$(ing200_q 0.999 "$R")" 1 4)
echo "| ingress, status=200 (гейт вердикта) | $(ms "$i200p50") | $(ms "$i200p95") | $(ms "$i200p99") | $(ms "$i200p999") |"
kv ingress_p99_200_s "$i200p99"
for dir in mask demask; do
  p50=$(val "$(svc_q 0.5 "$dir" "$R")" 1 4); p95=$(val "$(svc_q 0.95 "$dir" "$R")" 1 4)
  p99=$(val "$(svc_q 0.99 "$dir" "$R")" 1 4); p999=$(val "$(svc_q 0.999 "$dir" "$R")" 1 4)
  echo "| сервис /process $dir | $(ms "$p50") | $(ms "$p95") | $(ms "$p99") | $(ms "$p999") |"
  kv "service_${dir}_p99_s" "$p99"
done
echo

# --- service ---
echo "### Сервис"
echo
echo "| Показатель | Значение |"
echo "|---|---|"
echo "| /process по статусу | $(join_rows "$(by_label "sum(increase(maskarad_requests_total${SEL_PROC}[$R])) by (status) > 0.5" status 1 0)") |"
echo "| max inflight по подам | $(join_rows "$(by_label "max(max_over_time(maskarad_inflight${SEL_SVC}[$R])) by (pod)" pod 1 0)") |"
echo "| отклонено по лимиту (reason) | $(join_rows "$(by_label "sum(increase(maskarad_rejected_total${SEL_SVC}[$R])) by (reason)" reason 1 0)") |"
if store_rows=$(query "sum(increase(maskarad_store_ops_total${SEL_SVC}[$R])) by (op, result)" "$T"); then
  echo "| store ops (op/result) | $(jq -r "$JQ_FMT"' [.[] | "\(.metric.op)/\(.metric.result) \((.value[1] | tonumber? // 0) | f(1; 0))"] | join(", ") | if . == "" then "—" else . end' <<<"$store_rows") |"
  store_err=$(jq -r '[.[] | select(.metric.result == "error") | .value[1] | tonumber? // 0] | add // 0 | round' <<<"$store_rows")
else
  echo "| store ops (op/result) | (ошибка) |"; store_err="(ошибка)"
fi
echo "| **store errors** | $store_err |"
kv store_errors "$store_err"
echo "| store degraded (max) | $(val "max(max_over_time(maskarad_store_degraded${SEL_SVC}[$R]))" 1 0) |"
rmin=$(val "$(replicas_q min "$R")" 1 0); rmax=$(val "$(replicas_q max "$R")" 1 0)
echo "| реплик (целей up) min / max | $rmin / $rmax |"
kv replicas_min "$rmin"; kv replicas_max "$rmax"
echo

# --- resources (cAdvisor через prometheus-agent) ---
echo "### Ресурсы"
echo
echo "| Под / узел | CPU (среднее) | Память max, МБ |"
echo "|---|---:|---:|"
rows2 "$(by_label "sum(rate(container_cpu_usage_seconds_total${SEL_POD}[$R])) by (pod)" pod 1 2)" \
      "$(by_label "max(max_over_time(container_memory_working_set_bytes${SEL_POD}[$R])) by (pod)" pod "$MB" 0)" "" " ядер"
rows2 "$(by_label "sum(rate(container_cpu_usage_seconds_total${SEL_ING_POD}[$R])) by (pod)" pod 1 2)" \
      "$(by_label "max(max_over_time(container_memory_working_set_bytes${SEL_ING_POD}[$R])) by (pod)" pod "$MB" 0)" "" " ядер"
rows2 "$(by_label "$(nodes_cpu_q "$R")" node 100 0)" "$(by_label "$(nodes_mem_q "$R")" node "$MB" 0)" "узел " " %"
echo

# --- stages ---
if [ -n "$STAGES" ]; then
  echo "### По ступеням"
  echo
  echo "| Старт (UTC) | Цель, пар/с | ingress rps | ingress p99 (200) | сервис mask p99 | mask p99.9 | CPU узлов | Реплик | store errors | Статусы ingress |"
  echo "|---|---:|---:|---:|---:|---:|---|---|---|---|"
  t="$START"
  for st in ${STAGES//,/ }; do
    d="${st%%:*}"; tgt="${st##*:}"
    case "$d" in
      *h) secs=$(( ${d%h} * 3600 )) ;;
      *m) secs=$(( ${d%m} * 60 )) ;;
      *s) secs=${d%s} ;;
      *) secs=$d ;;
    esac
    [[ "$secs" =~ ^[0-9]+$ ]] || { echo "bad stage duration: $d" >&2; exit 2; }
    e=$(( t + secs )); SR="${secs}s"; [ "$secs" -ge 30 ] || SR="30s"
    T="$e"
    rps=$(val "sum(rate(nginx_ingress_controller_requests${SEL_ING}[$SR]))" 1 0)
    ip99=$(val "$(ing200_q 0.99 "$SR")" 1 4)
    sp99=$(val "$(svc_q 0.99 mask "$SR")" 1 4)
    sp999=$(val "$(svc_q 0.999 mask "$SR")" 1 4)
    cpu=$(by_label "$(nodes_cpu_q "$SR")" node 100 0 | awk -F'\t' '{ printf "%s%s", (NR > 1 ? "/" : ""), $2 } END { if (NR) printf " %%" }')
    rep="$(val "$(replicas_q min "$SR")" 1 0)–$(val "$(replicas_q max "$SR")" 1 0)"
    serr=$(join_rows "$(by_label "sum(increase(maskarad_store_ops_total${SEL_STORE_ERR}[$SR])) by (op)" op 1 0)")
    ist=$(join_rows "$(by_label "sum(increase(nginx_ingress_controller_requests${SEL_ING}[$SR])) by (status) > 0.5" status 1 0)")
    echo "| $(fmt_utc "$t") | $tgt | $rps | $(ms "$ip99") | $(ms "$sp99") | $(ms "$sp999") | ${cpu:--} | $rep | $serr | $ist |"
    t="$e"
  done
  echo
fi

if [ -e "$TMP/failed" ]; then
  echo "loadtest_report: some Prometheus queries failed (see «(ошибка)» cells and stderr above)" >&2
  exit 1
fi
