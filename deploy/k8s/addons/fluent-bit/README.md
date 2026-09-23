# fluent-bit: логи подов в Monium Logs

DaemonSet Fluent Bit 4.2.8 в namespace `observability` читает с узла файлы логов контейнеров
maskarad и контроллера ingress-nginx и отправляет записи в Monium Logs по OTLP/HTTP со сжатием
zstd (`ingest.monium.yandex.cloud`). Ставит его workflow infra, шаг «Fluent Bit → Monium»; вручную
то же самое:

```sh
kubectl -n observability create secret generic monium-api-key \
  --from-file=api-key=<(terraform -chdir=infra/terraform output -raw monium_logs_api_key) \
  --dry-run=client -o yaml | kubectl apply -f -
kubectl kustomize deploy/k8s/addons/fluent-bit | envsubst '$YC_FOLDER_ID' | kubectl apply -f -
kubectl -n observability rollout restart daemonset/fluent-bit   # только если менялся Secret
```

Namespace создаёт monitoring-agent. envsubst подставляет только `YC_FOLDER_ID` (проект Monium
`folder__<folder_id>`), а `${MONIUM_API_KEY}`, `${MONIUM_PROJECT}` и `${NODE_NAME}` в
`fluent-bit.yaml` Fluent Bit раскрывает сам из env пода. ConfigMap собирает `configMapGenerator`
с хеш-суффиксом: правка `fluent-bit.yaml` перекатывает DaemonSet без ручного рестарта. Ключ из
Secret Fluent Bit читает только при старте, поэтому после смены ключа нужен `rollout restart`
(workflow делает его сам, когда `kubectl apply` Secret'а ответил `configured`).

## Что собирается

Два входа `tail`, отбор по имени файла `/var/log/containers/<pod>_<namespace>_<container>-<id>.log`:
`*_maskarad_maskarad-*.log` (контейнер maskarad в namespace maskarad) и
`*_ingress-nginx_controller-*.log`. Остальные поды (kube-system, smoke, loadtest, cert-manager)
не отправляются. Access-лог ingress-nginx выключен (`disable-access-log`), от контроллера приходит
только stderr nginx и самого контроллера.

Отбор по пути, а не фильтром по namespace, по двум причинам. Атрибуты ресурса (`service.name`,
`cluster`, `host`) в 4.2 задаёт только `content_modifier` с постоянным значением, из записи их не
взять, поэтому на каждый сервис нужен свой вход. И имя файла уже несёт pod, namespace и контейнер:
процессор `kubernetes` с `use_tag_for_meta` берёт их оттуда, не ходит в API, и агенту не нужны
ни токен сервисного аккаунта, ни RBAC. Новый сервис — новый вход `tail` по образцу существующих.

## Monium

Запросы в `ingest.monium.yandex.cloud/otlp/v1/logs` несут два заголовка:
`Authorization: Api-Key <ключ>` и `x-monium-project: folder__<folder_id>`. Схема именно `Api-Key`
через дефис: `ApiKey`, как написано в тексте документации Monium, даёт 401. Ключ — Terraform output
`monium_logs_api_key` (сервисный аккаунт `maskarad-observability`, роль `monium.logs.writer`,
scope ключа `yc.monium.logs.write`), в кластере — Secret `observability/monium-api-key`, ключ `api-key`.

В Monium записи лежат в проекте `folder__<folder_id>`: `cluster=maskarad`, `service=maskarad` или
`service=ingress-nginx`, `host` — имя узла. Метки (индексируются, по ним быстрый фильтр):
`route`, `direction`, `system` — у них конечное множество значений. Всё остальное уходит в meta,
в том числе `request_id`: атрибут без префикса Monium тоже сделал бы меткой, а уникальное значение
на каждый запрос раздувает индекс. Уровень берётся из поля `level` (`severity_text`). Тело записи —
`message` у JSON-строк сервиса и вся строка у текстовых (stderr nginx, паника Rust). События
мастера Kubernetes сюда не относятся: они идут через лог-группу Cloud Logging и видны как
`{cluster="default", service="maskarad"}`.

## Буфер и потери

Чанки и позиции `tail` лежат на узле в hostPath `/var/lib/fluent-bit` и переживают рестарт и
обновление пода. Пока Monium недоступен, очередь растёт на диске: повтор без ограничения числа
попыток, пауза между ними до 60 с, потолок очереди выхода — 1 ГБ на узел, при переполнении
выбрасываются самые старые чанки. Повторяются только 429, 502, 503, 504 и сетевые ошибки. Остальные
4xx (401 и 403 при неверном или отозванном ключе, 400, 413) и 500 выход не повторяет: чанк
выбрасывается, в логе агента `[error] ... HTTP status=4xx`, растёт
`fluentbit_output_dropped_records_total` — его снимает prometheus-agent (job `fluent-bit`), так что
потеря видна в воркспейсе. Буфер от неверного ключа не спасает. `kubectl logs` видит только текущий
файл лога контейнера: kubelet ротирует его по размеру, под нагрузкой это минуты истории, полная
история — в Monium.

## Стоимость

Запись в Monium Logs стоит 4,40 ₽ за ГБ (минимальная единица — 1 МБ); чтение, хранение (TTL
31 день) и трафик не тарифицируются. Запись сервиса в OTLP без сжатия — около 400 Б, одна строка
на запрос: 2000 rps ≈ 2,9 ГБ/ч ≈ 12,7 ₽/ч, десятиминутный нагрузочный прогон ≈ 2 ₽, без трафика —
около нуля. Считает ли Monium сжатый или распакованный объём, документация не говорит, так что это
оценка сверху. Квота Monium — 150 МиБ/с на проект, наш пик около 1 МБ/с; как предохранитель можно
снизить квоту Logs Write в UI Monium (Настройки → Квоты).

## Локальная проверка

После правки `fluent-bit.yaml` или версии образа конфиг проверяется настоящим запуском: в 4.2.8
`--dry-run` пропускает опечатки в параметрах. Образ из `daemonset.yaml` с конфигом из репозитория
пишет в локальный OpenTelemetry Collector, отличается только адрес выхода. Из корня репозитория:

```sh
work=$(mktemp -d); mkdir -p "$work/conf" "$work/containers" "$work/state"
# root в контейнере без capabilities пишет только в свой или открытый каталог (на Linux иначе
# «storage creation failed»).
chmod 0777 "$work/state"
sed -e 's/^\( *host:\).*/\1 otelcol/' -e 's/^\( *port:\).*/\1 4318/' \
  -e 's/^\( *tls\(\.verify\)\{0,1\}:\).*/\1 off/' \
  deploy/k8s/addons/fluent-bit/fluent-bit.yaml > "$work/conf/fluent-bit.yaml"
cat > "$work/otelcol.yaml" <<'EOF'
receivers:
  otlp:
    protocols:
      http: { endpoint: 0.0.0.0:4318, logs_url_path: /otlp/v1/logs }
exporters:
  debug: { verbosity: detailed }
service:
  pipelines:
    logs: { receivers: [otlp], exporters: [debug] }
EOF
# id контейнера — ровно 64 символа: иначе процессор kubernetes не разберёт имя файла.
t=2026-09-23T10:00:00.000000000Z id=$(printf '%064d' 0)
cat > "$work/containers/maskarad-6d8f7c9b4-x2k9q_maskarad_maskarad-$id.log" <<EOF
$t stdout F {"level":"WARN","message":"store write failed","request_id":"r1","route":"/process","direction":"mask","system":"crm"}
$t stderr F thread 'tokio-runtime-worker' panicked at src/x.rs:1:1:
$t stderr F explicit panic
EOF
echo "$t stdout F must not be collected" > "$work/containers/coredns-1_kube-system_coredns-$id.log"
docker network create fb-local
docker run -d --name fb-otelcol --network fb-local --network-alias otelcol \
  -v "$work/otelcol.yaml:/etc/otelcol.yaml:ro" otel/opentelemetry-collector-contrib:latest --config /etc/otelcol.yaml
sleep 3
docker run -d --name fb-local --network fb-local --platform linux/amd64 --read-only --user 0 --cap-drop ALL \
  -e HTTP_PROXY= -e HTTPS_PROXY= -e MONIUM_API_KEY=local -e MONIUM_PROJECT=folder__local -e NODE_NAME=local \
  -v "$work/containers:/var/log/containers:ro" -v "$work/conf:/fluent-bit/etc/conf:ro" \
  -v "$work/state:/var/lib/fluent-bit" \
  "$(awk '/image:/{print $2; exit}' deploy/k8s/addons/fluent-bit/daemonset.yaml)" \
  /fluent-bit/bin/fluent-bit -c /fluent-bit/etc/conf/fluent-bit.yaml
sleep 10
docker logs fb-local 2>&1 | grep -E '\[ *(error|warn)\]|HTTP status='
docker logs fb-otelcol 2>&1 | grep -E '^(SeverityText|Body)|-> (service\.name|labels\.|meta\.)'
docker rm -f fb-local fb-otelcol; docker network rm fb-local; rm -rf "$work"
```

Ожидается `HTTP status=200` без `[error]`, в коллекторе три записи с `service.name: maskarad`:
WARN с `labels.route` и `meta.request_id` и две строки паники со stderr с уровнем ERROR (паника Rust —
отдельные строки, склеиваются только частичные строки CRI с флагом P); строки coredns нет.
Пустые `HTTP_PROXY`/`HTTPS_PROXY` нужны, если Docker прокидывает в контейнеры прокси хоста.

Манифесты:

```sh
kubectl kustomize deploy/k8s/addons/fluent-bit | YC_FOLDER_ID=test envsubst '$YC_FOLDER_ID' \
  | docker run --rm -i ghcr.io/yannh/kubeconform:latest -strict -summary -kubernetes-version 1.35.0 -
```

## Откат

```sh
kubectl kustomize deploy/k8s/addons/fluent-bit | kubectl delete -f -
```

Удаляет DaemonSet, ConfigMap и ServiceAccount; Secret `monium-api-key` и каталог
`/var/lib/fluent-bit` на узлах остаются. Сервис это не затрагивает. Следующий запуск infra → addons
поставит агент снова.
