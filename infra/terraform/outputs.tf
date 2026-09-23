output "cluster_id" {
  value = yandex_kubernetes_cluster.main.id
}

output "cluster_name" {
  value = yandex_kubernetes_cluster.main.name
}

output "registry_id" {
  value = yandex_container_registry.main.id
}

output "image_repository" {
  description = "Полное имя репозитория образов сервиса"
  value       = "cr.yandex/${yandex_container_registry.main.id}/${var.name}"
}

output "ingress_ip" {
  value = local.ingress_ip
}

output "redis_host" {
  value = yandex_mdb_redis_cluster.main.host[0].fqdn
}

output "redis_password" {
  value     = random_password.redis.result
  sensitive = true
}

output "redis_url" {
  value     = "redis://:${random_password.redis.result}@${yandex_mdb_redis_cluster.main.host[0].fqdn}:6379/0"
  sensitive = true
}

output "domain" {
  value = var.domain
}

output "nameservers" {
  description = "NS-серверы, которые нужно указать у регистратора домена"
  value       = ["ns1.yandexcloud.net", "ns2.yandexcloud.net"]
}

output "log_group_id" {
  description = "Лог-группа Cloud Logging: события мастера Kubernetes (master_logging)"
  value       = yandex_logging_group.main.id
}

output "monitoring_api_key" {
  description = "API-ключ для remote write Prometheus в Managed Prometheus (Secret monitoring-api-key)"
  value       = yandex_iam_service_account_api_key.monitoring.secret_key
  sensitive   = true
}

output "monium_logs_api_key" {
  description = "API-ключ Fluent Bit для записи логов в Monium (Secret monium-api-key)"
  value       = yandex_iam_service_account_api_key.logs.secret_key
  sensitive   = true
}
