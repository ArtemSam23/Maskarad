# prometheus-agent: метрики в Managed Prometheus (Monium)

Prometheus в режиме agent (только скрейп и remote write, без локального хранилища и запросов) читает
`/metrics` подов maskarad в namespace `maskarad`, порт 10254 контроллера ingress-nginx и
`/metrics/cadvisor` kubelet'ов (CPU и память подов и узлов), а серии отправляет в воркспейс Managed
Prometheus в Monium. Дашборд и алерты живут там; в кластере ничего не хранится, при рестарте
агент теряет только неотправленную очередь (WAL на emptyDir). Приложение при этом ничего о Яндексе не
знает: обычный `/metrics`.

Воркспейс создаётся вручную в UI Monium (Поставка и хранение → Prometheus → «Создать воркспейс»;
ни Terraform, ни API для этого нет), его идентификатор кладётся в GitHub variable
`YC_PROMETHEUS_WORKSPACE_ID`. В `prometheus.yml` адрес remote write содержит плейсхолдер
`${YC_PROMETHEUS_WORKSPACE_ID}`, его подставляет workflow infra при применении. Список переменных
у envsubst обязателен: без него он затрёт любой другой `$` в конфиге.

```sh
kubectl create namespace observability --dry-run=client -o yaml | kubectl apply -f -
kubectl -n observability create secret generic monitoring-api-key \
  --from-literal=api-key="$(terraform -chdir=infra/terraform output -raw monitoring_api_key)" \
  --dry-run=client -o yaml | kubectl apply -f -
kubectl kustomize deploy/k8s/addons/monitoring-agent \
  | envsubst '$YC_PROMETHEUS_WORKSPACE_ID' | kubectl apply -f -
```

Секрет `monitoring-api-key` — API-ключ сервисного аккаунта `<name>-observability` (роль
`monitoring.editor`, scope ключа `yc.monitoring.manage`), Terraform output `monitoring_api_key`.
Scope на живом воркспейсе не проверен: если remote write отвечает 401 или 403, первым делом
проверить его — задать в `infra/terraform/observability.tf` другой явный список scopes (меняется
на месте); просто убрать атрибут нельзя, в state останется старое значение. ConfigMap с
`prometheus.yml` собирает `configMapGenerator` с хеш-суффиксом в имени: правка конфига меняет имя
ConfigMap, и Deployment перекатывается сам (Prometheus конфиг на лету не перечитывает).

Локальная проверка без кластера (плейсхолдер подставляется sed'ом; promtool проверяет, что файл
`bearer_token_file` job'а cadvisor существует, поэтому по пути токена сервисного аккаунта монтируется
пустой файл):

```sh
kubectl kustomize deploy/k8s/addons/monitoring-agent | sed 's/${YC_PROMETHEUS_WORKSPACE_ID}/test/' \
  | kubectl apply --dry-run=client -f -
dir=$(mktemp -d)
sed 's/${YC_PROMETHEUS_WORKSPACE_ID}/test/' deploy/k8s/addons/monitoring-agent/prometheus.yml > "$dir/prometheus.yml"
: > "$dir/token"
docker run --rm --entrypoint promtool -v "$dir:/cfg:ro" \
  -v "$dir/token:/var/run/secrets/kubernetes.io/serviceaccount/token:ro" \
  cr.yandex/mirror/prom/prometheus:v2.46.0 check config /cfg/prometheus.yml
```

## Бюджет серий

Запись через Prometheus Remote API бесплатна до 50 млн значений в месяц на платёжный аккаунт,
дальше 2,3058 ₽ за 1 млн значений (SKU «Monium. Writing metric values via Prometheus Remote API»
в billing API, сентябрь 2026). 0,32 ₽ из правил тарификации Monium — цена записи пользовательских
метрик через Monium API, к remote write она не относится. Штатно (4 пода, 4 узла) агент пишет
≈ 70,9 млн значений в месяц, на пике HPA (8 подов) ≈ 117,5 млн; расчёт и рычаги ниже. При
`scrape_interval: 30s` одна серия — это 2 × 60 × 24 × 30 = 86 400 значений в месяц, то есть
бесплатно живут ≈ 578 постоянных серий.
Считать надо штатное состояние: серии, которые появляются на два часа нагрузочного теста
(поды от HPA, лишние статусы), стоят 240 значений каждая — доли процента месяца.

Под maskarad сейчас: 182 строки `/metrics`, из них 28 HELP/TYPE, 154 сэмпла — 60 бакетов
`maskarad_request_duration_seconds` (4 набора route/system/direction × 15), 60 бакетов
`maskarad_stage_duration_seconds` (4 стадии × 15), 16 `_sum`/`_count`, 14 счётчиков и gauge и
4 gauge адаптивного допуска (`maskarad_admission_*`). Под нагрузкой с новыми маршрутами,
статусами, вторым system и причинами отказа наборов станет больше: около 114 сэмплов на под без
stage-бакетов.

Что выброшено и почему:

- `maskarad_stage_duration_seconds_bucket` — 60 серий на под ради p95 по стадиям. `_sum`/`_count`
  остаются: `rate(_sum)/rate(_count)` по стадии показывает, где уходит время, а p95 по стадиям
  смотрится через `kubectl port-forward` на `/metrics` или локальным `maskarad bench`.
  Панель «Стадии обработки, среднее» на дашборде считает именно `rate(_sum)/rate(_count)`.
- Из ingress-nginx только `nginx_ingress_controller_requests`,
  `nginx_ingress_controller_request_duration_seconds_{bucket,sum,count}` и
  `nginx_ingress_controller_nginx_process_connections{,_total}`. Без фильтра контроллер отдаёт
  семь гистограмм по тем же меткам host/status/method/path плюс go_* и process_* — оценочно
  больше тысячи серий на узел. `response_duration_seconds` (время ответа upstream) не берём:
  дублирует `maskarad_request_duration_seconds`, а стоит столько же серий, сколько
  `request_duration_seconds`. Хосты в метках ограничены хостами Ingress-ресурсов, сканеры с
  чужим `Host` серий не добавляют.
- `scrape_*` (4 служебных серии на цель) выброшены через `write_relabel_configs`, `up` остаётся.
- Из cAdvisor (`/metrics/cadvisor` kubelet, ~3 000 строк на узел) только
  `container_cpu_usage_seconds_total` и `container_memory_working_set_bytes` для контейнеров в
  `maskarad` и `ingress-nginx` (уровень пода и pause с `container=""` выброшены), те же две серии для
  корневого cgroup `id="/"` (узел целиком) и `machine_cpu_cores`. Это замена node-exporter и
  kube-state-metrics для серверного отчёта нагрузочного прогона: CPU узла —
  `rate(container_cpu_usage_seconds_total{id="/"}) / machine_cpu_cores`.
- Fluent Bit (`/api/v2/metrics/prometheus`, порт 2020): только счётчики выхода `proc_records`,
  `dropped_records`, `errors`, `retries_failed` — 4 серии и `up` на узел. 4xx и partialSuccess
  Monium агент не повторяет, и без этих счётчиков потеря строк лога ничем не видна.

Оценка (серий → значений в месяц):

- под maskarad: ~95 сейчас (154 − 60 + `up`), ~114 под нагрузкой; плюс 2 из cAdvisor;
- узел, то есть под ingress-nginx: requests ~6 (один хост × статусы × методы) + duration
  6 × 14 = 84 + connections 6 + `up` ≈ 97; плюс 2 из cAdvisor на контроллер и 4 на узел
  (корневой cgroup 2, `machine_cpu_cores`, `up` цели cadvisor) и 5 от Fluent Bit;
- штатно (HPA min 4 пода, `nodes_min` 4): 4 × 97 + 4 × 108 = 820 серий → 70,9 млн; на пике HPA
  (8 подов, 4 узла): 8 × 116 + 4 × 108 = 1 360 → 117,5 млн. Узел loadgen на время прогона добавляет
  4 серии cAdvisor — на часы, не на месяц.

Итог: штатно примерно на 40 % выше free tier, переплата ≈ 20,9 млн × 2,3058 ₽ ≈ 48 ₽ в месяц;
если 8 подов HPA держатся весь месяц, ≈ 67,5 млн × 2,3058 ₽ ≈ 156 ₽.

Самый сильный рычаг — `scrape_interval: 60s`: значений вдвое меньше, штатно ≈ 35,4 млн, то есть
в пределах free tier (экономия все ≈ 48 ₽ в месяц), на пике HPA ≈ 58,8 млн и ≈ 20 ₽ вместо
≈ 156 ₽. Цена — точность: в окно `rate()` дашборда (2m) попадают две точки вместо четырёх, а
серверный отчёт `scripts/loadtest_report.sh` считает окна ступеней от 30 с, и ступени короче двух
минут могут остаться без данных. Пока не сделано: выигрыш — десятки рублей в месяц. Остальные
рычаги по убыванию эффекта: выбросить `request_duration_seconds` ingress (−84 на узел, латентность
останется только по метрике приложения), прореживание бакетов обеих гистограмм по метке `le` в
`metric_relabel_configs` (точность p95/p99).
