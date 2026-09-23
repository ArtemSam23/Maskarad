# Наблюдаемость вынесена из кластера в Monium: логи — Monium Logs (OTLP), метрики — воркспейс
# Managed Prometheus. На узлах 2 vCPU / 4 ГБ Prometheus, Loki и Grafana занимали ~1.5 vCPU и 2 ГБ.
# В кластере остаются только агенты (Fluent Bit и Prometheus в режиме agent),
# приложение по-прежнему пишет JSON-логи в stdout и отдаёт /metrics.

# Отдельный сервисный аккаунт, а не кластерный: агентам нужна только запись
# логов и метрик, а у кластерного SA — балансировщики, адреса, KMS и реестр.
# Ключи этого SA лежат в Secret'ах кластера.
resource "yandex_iam_service_account" "observability" {
  name        = "${var.name}-observability"
  description = "Fluent Bit and Prometheus agent: write logs to Monium and metrics to Managed Prometheus"
}

resource "yandex_resourcemanager_folder_iam_member" "observability" {
  for_each = toset([
    # Fluent Bit → ingest.monium.yandex.cloud (OTLP). Проект Monium каталога — folder__<folder_id>.
    "monium.logs.writer",
    # Remote write в воркспейс Prometheus: документация называет только эту роль (403 без неё).
    # Роль шире записи: чтение метрик каталога и правка дашбордов Monitoring.
    "monitoring.editor",
  ])

  folder_id = var.folder_id
  role      = each.value
  member    = "serviceAccount:${yandex_iam_service_account.observability.id}"
}

# API-ключ для remote write Prometheus (bearer_token_file). Без scopes ключ получает набор по
# умолчанию, в том числе yc.ai.* и serverless.*; из него к Monitoring относится только
# yc.monitoring.manage, её и оставляем. На живом воркспейсе scope не проверен: если remote write
# ответит 401/403, задать другой явный список (например, добавить yc.monium.metrics.write) —
# он меняется на месте, секрет прежний. Просто убрать scopes нельзя: атрибут Optional+Computed,
# в state останется старое значение.
resource "yandex_iam_service_account_api_key" "monitoring" {
  service_account_id = yandex_iam_service_account.observability.id
  description        = "Prometheus remote write to Managed Prometheus"
  scopes             = ["yc.monitoring.manage"]

  # API отдаёт и устаревший атрибут scope (первая область из scopes), провайдер пишет его в state,
  # и каждый plan хотел «обнулить» scope на живом ключе. Источник правды — scopes.
  lifecycle {
    ignore_changes = [scope]
  }
}

# API-ключ Fluent Bit: заголовок `Authorization: Api-Key <ключ>` (через дефис; `ApiKey` даёт 401).
resource "yandex_iam_service_account_api_key" "logs" {
  service_account_id = yandex_iam_service_account.observability.id
  description        = "Fluent Bit OTLP logs to Monium"
  scopes             = ["yc.monium.logs.write"]

  # API отдаёт и устаревший атрибут scope (первая область из scopes), провайдер пишет его в state,
  # и каждый plan хотел «обнулить» scope на живом ключе. Источник правды — scopes.
  lifecycle {
    ignore_changes = [scope]
  }
}

# Лог-группа остаётся только для событий мастера: master_logging в Managed Kubernetes пишет
# лишь в Cloud Logging. В Monium эти записи видны как cluster=default, service=<имя группы>.
# Cloud Logging закрывается во II кв. 2027 — тогда группу и master_logging придётся пересмотреть.
resource "yandex_logging_group" "main" {
  name             = var.name
  description      = "Kubernetes master events (master_logging); pod logs go to Monium"
  retention_period = var.log_retention
}
