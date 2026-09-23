#!/usr/bin/env bash
# Импорт Grafana-дашборда в Monium: серверная конвертация
# DashboardConversionService.ConvertFromGrafana → DashboardService.Create или Update (gRPC).
#   scripts/monium_dashboard.sh <grafana.json>
# Окружение: YC_FOLDER_ID, YC_PROMETHEUS_WORKSPACE_ID; токен — YC_IAM_TOKEN или `yc iam create-token`.
# Нужны jq и curl. grpcurl берётся из GRPCURL или PATH, иначе скачивается в $RUNNER_TEMP (CI) или ./.cache.
# Имя дашборда — uid из Grafana JSON (или DASHBOARD_NAME); по нему ищется существующий, поэтому
# повторный запуск обновляет дашборд, а не создаёт копию.
# Запросы остаются PromQL (KEEP_PROMQL) и читают воркспейс. Существование воркспейса сервер не
# проверяет: опечатка в id даёт пустой дашборд без ошибки.
# ConvertFromGrafana не описан в документации (есть в cloudapi и в gRPC reflection сервера);
# REST этих методов отвечает 404, поэтому grpcurl. Протосы не нужны — сервер отдаёт reflection.
# DRY_RUN=1 — только конвертация и поиск существующего дашборда, без записи.
set -euo pipefail

src=${1:?usage: monium_dashboard.sh <grafana.json>}
[ -f "$src" ] || { echo "$src: no such file" >&2; exit 2; }
folder=${YC_FOLDER_ID:?YC_FOLDER_ID is not set}
workspace=${YC_PROMETHEUS_WORKSPACE_ID:?YC_PROMETHEUS_WORKSPACE_ID is not set}
name=${DASHBOARD_NAME:-$(jq -r '.uid // empty' "$src")}
[ -n "$name" ] || { echo "$src: no uid, set DASHBOARD_NAME" >&2; exit 2; }

# grpcurl: релиз fullstorydev/grpcurl с проверкой sha256 (из grpcurl_1.9.3_checksums.txt).
grpcurl=${GRPCURL:-}
if [ -z "$grpcurl" ]; then
  if command -v grpcurl >/dev/null; then grpcurl=grpcurl; else
    cache=${RUNNER_TEMP:-.cache}; mkdir -p "$cache"; grpcurl="$cache/grpcurl"
    if [ ! -x "$grpcurl" ]; then
      case "$(uname -s)/$(uname -m)" in
        Linux/x86_64)  asset=linux_x86_64; sha=a926b62a85787ccf73ef8736b3ae554f1242e39d92bb8767a79d6dd23b11d1d5 ;;
        Linux/aarch64) asset=linux_arm64;  sha=b20a00c1cb82ab81ec32696766d4076e99b4cb5ca0823a71767ba64dbea0f263 ;;
        Darwin/arm64)  asset=osx_arm64;    sha=d8391485e99a728a3a4e82af3fd621f9fdea0c417a74e5122803ad20b207b623 ;;
        Darwin/x86_64) asset=osx_x86_64;   sha=246a6669e58c282dcaf0e9dcb06dd1c8681833d59df24eb83d3123ec64c2d2e5 ;;
        *) echo "no grpcurl release for $(uname -s)/$(uname -m)" >&2; exit 2 ;;
      esac
      tgz=$(mktemp); trap 'rm -f "$tgz"' EXIT
      curl -sSfL -o "$tgz" "https://github.com/fullstorydev/grpcurl/releases/download/v1.9.3/grpcurl_1.9.3_${asset}.tar.gz"
      actual=$( (sha256sum "$tgz" 2>/dev/null || shasum -a 256 "$tgz") | cut -d' ' -f1)
      [ "$actual" = "$sha" ] || { echo "grpcurl archive checksum mismatch: $actual" >&2; exit 1; }
      tar -xzf "$tgz" -C "$cache" grpcurl; rm -f "$tgz"
    fi
  fi
fi

# Токен уходит в заголовок через -expand-headers, а не в argv grpcurl (argv виден в ps).
# --format json явно: в CI профиль yc переключён на json, и голый `create-token` печатает объект.
MONIUM_TOKEN=${YC_IAM_TOKEN:-$(yc iam create-token --format json | jq -er .iam_token)}  # отдельно от export: иначе set -e не видит сбой yc
export MONIUM_TOKEN
[ -n "${GITHUB_ACTIONS:-}" ] && echo "::add-mask::$MONIUM_TOKEN"
call() {  # <Service/Method>, JSON-запрос на stdin
  # shellcheck disable=SC2016  # ${MONIUM_TOKEN} раскрывает grpcurl (-expand-headers), а не shell
  "$grpcurl" -expand-headers -rpc-header 'Authorization: Bearer ${MONIUM_TOKEN}' \
    -d @ monitoring.api.cloud.yandex.net:443 "yandex.cloud.monitoring.v3.$1"
}

converted=$(jq -n --arg p "folder__$folder" --rawfile g "$src" --arg ws "$workspace" \
  '{project_id: $p, grafana_json: $g,
    options: {query_translation: "QUERY_TRANSLATION_MODE_KEEP_PROMQL", prometheus_workspace_id: $ws}}' \
  | call DashboardConversionService/ConvertFromGrafana)

# SEVERITY_ERROR — панель выброшена, дашборд неполный.
jq -r '.diagnostics[]? | "\(.severity) \(.widget_title // ""): \(.message)"' <<<"$converted" >&2
if jq -e 'any(.diagnostics[]?; .severity == "SEVERITY_ERROR")' <<<"$converted" >/dev/null; then
  echo "conversion dropped panels, see diagnostics above" >&2; exit 1
fi

body=$(jq --arg n "$name" '.dashboard
  | {name: $n, title, description, labels, widgets, parametrization, timeline, links, preset_items}
  | with_entries(select(.value != null))' <<<"$converted")

found=$(jq -n --arg f "$folder" --arg n "$name" '{folder_id: $f, selectors: "name = \"\($n)\""}' \
  | call DashboardService/List | jq -c --arg n "$name" '[.dashboards[]? | select(.name == $n)][0] // empty')
if [ -n "$found" ]; then
  # etag из List: если дашборд поменяли между List и Update, сервер вернёт ошибку в операции.
  req=$(jq --argjson d "$found" '. + {dashboard_id: $d.id, etag: $d.etag}' <<<"$body"); method=Update
else
  req=$(jq --arg f "$folder" '. + {folder_id: $f}' <<<"$body"); method=Create
fi

if [ -n "${DRY_RUN:-}" ]; then
  jq -c --arg m "$method" '{method: $m, dashboard_id, etag, name, widgets: ([.widgets[] | .. | objects | select(has("multi_source_chart"))] | length)}' <<<"$req"
  exit 0
fi
# Create/Update синхронные, но ошибка (конфликт etag, занятое имя) приходит внутри Operation
# при нулевом коде выхода grpcurl.
op=$(call "DashboardService/$method" <<<"$req")
jq -e '.done == true and .error == null' <<<"$op" >/dev/null \
  || { echo "$method failed: $(jq -c '{done, error}' <<<"$op")" >&2; exit 1; }
echo "dashboard '$name' $method: $(jq -c '.response | {id, etag}' <<<"$op")"
