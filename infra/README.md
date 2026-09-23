# Инфраструктура и доставка

Всё живёт в Яндекс Облаке и создаётся кодом: `infra/terraform` (Terraform), `deploy/k8s` (kustomize и Helm-аддоны), `.github/workflows` (GitHub Actions). Ручных шагов после первичной настройки нет.

```
GitHub ──ci.yml──► тесты, качество, smoke, аудит ──► (main) cd.yml ──► образ → [одобрение] → production (rollout с откатом при провале, smoke) → контрольный прогон
       └─infra.yml► terraform plan (PR) / apply (main, одобрение) → аддоны: ingress-nginx, cert-manager, prometheus-agent → Managed Prometheus, Fluent Bit (DaemonSet) → Monium Logs
```

| Что | Где |
|---|---|
| Прод | https://maskarad.tech (`POST /process` — для прогона организаторов) |
| Метрики | Monium, проект `folder__<folder_id>`: воркспейс Prometheus и дашборд `maskarad` (доступ в каталог — по запросу) |
| Логи | Monium Logs, проект `folder__<folder_id>`: cluster `maskarad`, service `maskarad` и `ingress-nginx`, хранение 31 день |
| Образы | `cr.yandex/<registry>/maskarad:sha-<commit>` |
| Состояние Terraform | бакет `maskarad-tfstate-<folder_id>` в Object Storage |

## Ресурсы (Terraform)

VPC с двумя подсетями (мастер и Valkey отдельно от узлов; узлы с публичными адресами, потому что маршрут 0.0.0.0/0 на NAT-шлюз ломает ответный трафик мастера и сетевого балансировщика), группы безопасности по рекомендациям Managed Kubernetes, статический адрес балансировщика, Managed Kubernetes (зональный мастер, автообновление, шифрование секретов через KMS) с группой узлов 4–8 × (2 vCPU, 4 ГБ, диск 32 ГБ) — по одной реплике сервиса на узел рядом с ingress-nginx (`nodes_min = 4`, `nodes_max = 8` = maxReplicas HPA; узлы 5–8 автоскейлер добавляет под Pending-поды и убирает через 10 минут простоя, для них нужны квоты выше значений по умолчанию — раздел «Квоты»), Managed Service for Valkey — Redis-совместимое хранилище (1 хост `hm3-c2-m8`, минимальный класс с гарантией vCPU; `maxmemory_policy = NOEVICTION`, чтобы соответствия не вытеснялись до истечения TTL; пароль генерируется и хранится только в state и в Secret кластера), Container Registry (образ сервиса и копии образов ingress-nginx), публичная зона Cloud DNS с A-записью `@` на адрес балансировщика, сервисный аккаунт `maskarad-observability` (`monium.logs.writer`, `monitoring.editor`) с двумя API-ключами, от имени которого агенты в кластере пишут логи в Monium и метрики в воркспейс Prometheus, и лог-группа Cloud Logging `maskarad` (хранение 72 ч) только для событий мастера Kubernetes: `master_logging` другого назначения не принимает. Проект Monium, его квоты и воркспейс Prometheus в Terraform не описаны: ресурсов для них у провайдера нет.

Окна обслуживания (UTC, `variables.tf`): мастер — суббота 02:00–05:00, узлы — воскресенье 01:00–04:00 (по одному узлу), Valkey — суббота 01:00; вне окон облако ничего не пересоздаёт.

Ориентировочная стоимость постоянной части — 18–24 тыс. ₽/мес: Valkey (заметная доля, burstable-классы для него больше недоступны) и четыре узла standard-v3 2 vCPU 100 % / 4 ГБ с диском 32 ГБ и публичным адресом — порядка 2.5–3 тыс. ₽/мес каждый (четыре вместо двух добавили 5–6 тыс.). Узлы 5–8 тарифицируются, только пока существуют: автоскейлер создаёт их под Pending-поды и удаляет через 10 минут без нагрузки (HPA держит реплики ещё 30 минут после пика, `stabilizationWindowSeconds`), это ~4 ₽/ч за узел — час прогона на восьми узлах стоит около 30 ₽ сверх постоянной части и на месячную оценку не влияет. После хакатона `nodes_min = 1`, `nodes_max = 2` в `variables.tf` ужимают счёт, а Valkey можно заменить на Redis внутри кластера (`store.backend: redis` с адресом in-cluster сервиса, как в overlay `local`).

## Квоты

Квоты считаются на облако, не на каталог, и Terraform ими не управляет. Значения по умолчанию рассчитаны на 4 узла; на `nodes_max = 8` группа не вырастет: облако не создаст узел сверх квоты, HPA доведёт реплики до 8, а поды 5–8 останутся в Pending. Диски — в ГиБ, как задаёт Terraform.

| Квота | По умолчанию | Нужно | Из чего |
|---|---|---|---|
| `compute.ssdDisks.size` — Compute Cloud, общий объём SSD-дисков | 200 | 320 (минимум 288) | 8 узлов × 32 + 32 на пересоздание (`deploy_policy.max_expansion = 1`). Узел генератора `loadgen` берёт network-hdd и сюда не входит, диск Valkey тоже — у Managed Service for Valkey свои квоты |
| `vpc.externalAddresses.count` — VPC, публичные IP-адреса | 8 | 16 (минимум 12) | мастер + 8 узлов (`nat = true`) + 1 на пересоздание + статический адрес балансировщика + узел генератора `loadgen` на время замера |

`compute.instances.count` (12), `compute.instanceCores.count` (32) и `compute.instanceMemory.size` (128 ГиБ) на 9 узлов и узел генератора (8 vCPU, 16 ГБ) хватает; `vpc.externalStaticAddresses.count` (2) занят одним адресом балансировщика.

Повышение — ручной шаг, нужна роль `quota-manager.requestOperator` или `editor`/`admin` на облако. В консоли: облако → «Квоты» → Compute Cloud → «Общий объём SSD-дисков» → «Изменить» → новое значение → отправить запрос; то же для Virtual Private Cloud → «Публичные IP-адреса». Или одним запросом из CLI (`value` — в тех же единицах, что показывает `quota-limit get`: для дисков это байты, 320 ГиБ = 343597383680):

```bash
yc quota-manager quota-request create \
  --resource-type resource-manager.cloud --resource-id b1gt5hdhuc4hovu7kij9 \
  --desired-limit quota-id=compute.ssdDisks.size,value=343597383680 \
  --desired-limit quota-id=vpc.externalAddresses.count,value=16
```

Статус запроса: `yc quota-manager quota-request list --resource-type resource-manager.cloud --resource-id b1gt5hdhuc4hovu7kij9`. Текущий предел и потребление: `yc quota-manager quota-limit get --service compute --resource-type resource-manager.cloud --resource-id b1gt5hdhuc4hovu7kij9 --quota-id compute.ssdDisks.size` (для адресов — `--service vpc`, `--quota-id vpc.externalAddresses.count`). До одобрения `nodes_max = 8` безвреден: группа растёт, пока хватает квоты, дальше поды ждут — как при `nodes_max = 4`.

## Однократная настройка

1. **Яндекс Облако.** Сервисный аккаунт для CI (в проекте он называется `github-ci-cd`) с ролью **`admin`** на каталог: `editor` не может назначать роли (Terraform выдаёт их кластерному сервисному аккаунту) и не даёт полного доступа к Kubernetes API для установки аддонов. Если `admin` нежелателен — `editor` + `resource-manager.admin` + `k8s.cluster-api.cluster-admin`. Для него — **авторизованный ключ** (JSON) и **статический ключ доступа** (для Object Storage).
2. **GitHub → Settings → Secrets and variables → Actions.**

   Variables: `YC_PROMETHEUS_WORKSPACE_ID` — id воркспейса Prometheus в Monium (обязательна: в воркспейс пишет агент метрик и из него читает дашборд, см. п. 5). Необязательные: `YC_CLOUD_ID`, `YC_FOLDER_ID` — идентификаторы облака и каталога уже зашиты в workflow по умолчанию, переменные нужны только чтобы их переопределить; `LETSENCRYPT_EMAIL` (почта для уведомлений Let's Encrypt) — как variable или как secret.

   Secrets: `YC_SA_KEY_JSON` (содержимое авторизованного ключа), `YC_S3_ACCESS_KEY`, `YC_S3_SECRET_KEY`, `MASKARAD_STORE_KEY` (ключ шифрования хранилища), `MASKARAD_SECRET` (секрет токенизации), `MASKARAD_ADMIN_KEY`, `SUPPORT_BOT_KEY`, `ANALYTICS_KEY`; опционально `LLM_BASE_URL`, `LLM_API_KEY` для живого демо прокси. Случайные значения: `openssl rand -base64 32`.
3. **GitHub → Settings → Environments.** Только `infrastructure` и `production`, оба с Required reviewers (вы).
4. **Домен.** У регистратора выставить NS `ns1.yandexcloud.net`, `ns2.yandexcloud.net`. Записи создаёт Terraform.
5. **Воркспейс Prometheus.** В UI Monium: Поставка и хранение → Prometheus → «Создать воркспейс» (ни Terraform, ни API для этого нет); id воркспейса — в variable `YC_PROMETHEUS_WORKSPACE_ID`. Кластер для этого не нужен, воркспейс можно создать заранее.
6. **Квоты облака.** Поднять `compute.ssdDisks.size` и `vpc.externalAddresses.count` по разделу «Квоты» — без этого автоскейлер не создаст узлы 5–8.

## Первый запуск

1. Слить PR в `main` — `infra.yml` запросит одобрение в environment `infrastructure` и создаст ресурсы (10–15 минут: кластер и Redis поднимаются долго), затем поставит аддоны (агенту метрик нужна переменная `YC_PROMETHEUS_WORKSPACE_ID`, без неё job аддонов завершается ошибкой с подсказкой).
2. `cd.yml` запускается после зелёного `ci` на `main`: собирает образ, ждёт одобрения на production, выкатывает (если rollout не прошёл, Deployment откатывается на прежний образ) и прогоняет smoke-тест контракта внутри кластера, затем контрольный нагрузочный прогон на production. Провал контрольного прогона роняет job, но выкат не откатывает — он уже прошёл smoke, а причина может быть не в новой версии; команда отката печатается в лог. Если workflow стартовал раньше, чем появился реестр, — перезапустить (`Actions → cd → Re-run`).
3. Сертификаты появятся, как только делегирование домена доедет до Яндекса (`dig NS maskarad.tech`); до этого HTTPS отвечает самоподписанным сертификатом ingress-nginx. Владение доменом подтверждается через HTTP-01 (валидаторы Let's Encrypt ходят на балансировщик); DNS-01 не используется.

## Логи и диагностика

Наблюдаемость вынесена из кластера: kube-prometheus-stack, Loki и Promtail отбирали у сервиса ~1.5 vCPU и 2 ГБ на узлах 2 vCPU и под нагрузкой пропадали первыми. В кластере остались два агента, оба в namespace `observability`. `prometheus-agent` (`deploy/k8s/addons/monitoring-agent`) раз в 30 с снимает `/metrics` и отправляет серии remote write в воркспейс Managed Prometheus в Monium. Fluent Bit 4.2.8 — свой DaemonSet (`deploy/k8s/addons/fluent-bit`) — читает файлы логов контейнеров на узле и отправляет строки по OTLP/HTTP в Monium Logs. Файлы он отбирает по пути, а не по namespace: только контейнер `maskarad` из namespace `maskarad` и контроллер ingress-nginx; kube-system, smoke, loadtest и остальные поды в Monium не попадают. Access-лог у ingress-nginx выключен, от контроллера приходит только stderr nginx и самого контроллера. Позиции чтения и очередь неотправленных строк Fluent Bit держит на диске узла: после рестарта агент дочитывает файлы с сохранённой позиции, а пока Monium недоступен, очередь растёт до 1 ГБ на узел, дальше выбрасываются самые старые чанки. Сервис о Яндексе не знает: JSON-логи в stdout и `/metrics` в формате Prometheus, а ключи агентов приходят из Terraform (сервисный аккаунт `maskarad-observability`) в Secret'ы кластера.

Логи: Monium Logs, проект `folder__<folder_id>`, cluster `maskarad`, service `maskarad` (сервис) или `ingress-nginx` (контроллер). Метки, по которым фильтр работает быстро: `host` (имя узла) и поля строки сервиса `route`, `direction`, `system`. Остальные поля JSON-строки лежат в meta: `request_id` (по нему ищут конкретный запрос), `payload_id_hash`, `types`, `entities`, `text_bytes`, `tokens`, `store`, `degraded`, а также `k8s.pod.name` и `k8s.container.name`. Уровень записи берётся из поля `level`, текст — из `message`; строка не в JSON (паника, stderr nginx) приходит целиком. Значений ПДн в логах сервиса нет: только идентификаторы, хэши, типы ПДн, количества и размеры.

События мастера Kubernetes (`master_logging`) по-прежнему пишутся в лог-группу Cloud Logging `maskarad`: другого назначения у `master_logging` нет. В Monium они видны как cluster `default`, service `maskarad`. Cloud Logging закрывается во II квартале 2027 года, к этому сроку группу и `master_logging` придётся пересмотреть.

Метрики: дашборд `maskarad` в Monium, произвольные PromQL-запросы — к воркспейсу Prometheus того же проекта.

Стоимость метрик: запись через Prometheus Remote API бесплатна до 50 млн значений в месяц на платёжный аккаунт, дальше 2,3058 ₽ за 1 млн. При интервале 30 с 50 млн — это около 580 постоянных серий, поэтому в конфиге агента оставлены только нужные серии, а не всё, что есть в кластере. Штатно (4 пода, 4 узла) агент пишет ≈ 70,9 млн значений в месяц, переплата ≈ 48 ₽; на пике HPA (8 подов весь месяц) ≈ 117,5 млн и ≈ 156 ₽. Расчёт по сериям — в `deploy/k8s/addons/monitoring-agent/README.md`.

Стоимость логов: квота записи Monium Logs (Logs Write) — 150 МиБ/с на проект, считается в байтах, а не в записях; логи сервиса при 2000 запросов/с — около 1 МБ/с. Поэтому прореживания нет: строка на каждый запрос (требование ТЗ) доезжает до Monium. Запись стоит 4,40 ₽ за ГБ, чтение не тарифицируется. Оценка по несжатому OTLP, около 400 Б на запись: 2000 запросов/с — это ≈ 2,9 ГБ/ч, то есть ≈ 13 ₽ за час нагрузки, без трафика — почти ноль. Какой объём тарифицирует Monium, сжатый или несжатый, документация не говорит. Ограничить счёт сверху можно, снизив квоту Logs Write в UI Monium (Настройки → Квоты): сверх квоты Monium отбрасывает часть входящих строк.

Сервис пишет в stdout, а агент читает файлы логов узла; у агента запрос 200m CPU и лимит 1 ядро: когда узел упирается в CPU, процессор делится по запросам, и с прежними 50m агент отставал от ротации логов и терял строки (стресс-прогон 2026-09-23, выше 5 тыс. запросов/с). `kubectl logs` видит только текущий файл лога контейнера (kubelet ротирует его по размеру, под нагрузкой это минуты), полная история — в Monium. Потерю строк при доставке показывает счётчик `fluentbit_output_dropped_records_total` в воркспейсе.

Workflow `diagnostics` (Actions → diagnostics → Run workflow, параметр — namespace) выводит в лог узлы, поды, события, логи сервиса, Ingress, сертификаты cert-manager и релизы Helm — этого достаточно, чтобы разобрать любой инцидент, не подключаясь к кластеру вручную.

Workflow `ops` (Actions → ops → Run workflow) выполняет операции, которых нет в Terraform:

- `retry-tls` пересоздаёт Certificate `maskarad-tls`, если он не в состоянии Ready, и cert-manager сразу начинает выпуск заново, не дожидаясь часовой паузы после неудачи.
- `recreate-ingress-lb` удаляет Service ingress-nginx (облако удаляет сетевой балансировщик) и создаёт его заново через Helm с тем же статическим адресом, затем ждёт HEALTHY у всех targets — так чинится балансировщик, который отвечает через раз при здоровых узлах. Внешний трафик недоступен одну-две минуты.
- `recreate-node` с именем узла выводит узел из работы (drain), удаляет его ВМ из группы узлов и дожидается замены.
- `loadtest` — нагрузочный прогон k6 из облака: поднимает временную группу узлов `loadgen` (не в Terraform, живёт только на время прогона), запускает Job с `load/k6-process.js` через публичный адрес сервиса, кладёт в summary клиентский отчёт k6 и серверный по метрикам из воркспейса Prometheus (`YC_PROMETHEUS_WORKSPACE_ID`) с вердиктом PASS, FAIL или НЕПОЛНЫЙ (гейты — в `load/README.md`), после чего удаляет группу узлов (`keep_loadgen` оставляет её для серии прогонов). Профиль задаётся `rps` и `duration` или ступенями `stages`, дополнительно `keepalive` и `big_per_10s`; подробности — `load/README.md`, результаты замеров — `docs/performance.md`.

## Повседневная работа

- Изменение кода → PR → CI → merge → одобрение → production. Откат: `Actions → cd → Run workflow` с `ref` предыдущего коммита или `kubectl -n maskarad rollout undo deployment/maskarad`.
- Изменение конфигурации сервиса (системы-потребители, типы ПДн) — `deploy/k8s/base/configmap.yaml`, применяется той же доставкой; сервис перечитывает файл на лету. Секции `server`, `store` и `logging` читаются только при старте — после их изменения `kubectl -n maskarad rollout restart deployment/maskarad`.
- Изменение инфраструктуры — правка `infra/terraform`, план виден в PR, apply после merge с одобрением. Удаление: локально `terraform destroy` с теми же ключами (намеренно не автоматизировано).
- ingress-nginx ставится `scripts/ingress_nginx_helm.sh` с версией чарта из `INGRESS_NGINX_CHART_VERSION` (`infra.yml` и `ops.yml`, менять вместе): образы чарта копируются с раннера из registry.k8s.io в Container Registry каталога, узлы тянут их оттуда. Обновление версии — поменять значение в обоих workflow, план/apply infra подтянет образы новой версии.
- Изменение дашборда — `deploy/grafana/dashboards/maskarad.json` (Grafana JSON, один и тот же для локального стенда и облака). Импорт в Monium — Actions → infra → Run workflow → `addons`: `scripts/monium_dashboard.sh` отдаёт JSON серверному конвертеру Monium (`ConvertFromGrafana`), запросы остаются PromQL и читают воркспейс `YC_PROMETHEUS_WORKSPACE_ID`. Правки дашборда сами по себе workflow не запускают. Проверить конвертацию, ничего не записывая: `DRY_RUN=1 YC_FOLDER_ID=<folder_id> YC_PROMETHEUS_WORKSPACE_ID=<workspace_id> scripts/monium_dashboard.sh deploy/grafana/dashboards/maskarad.json`.
- Локальный кластер: `kubectl apply -k deploy/k8s/overlays/local` (Redis в кластере, секреты из примера).
