variable "cloud_id" {
  description = "Идентификатор облака Яндекс Облака"
  type        = string
}

variable "folder_id" {
  description = "Идентификатор каталога, в котором создаются ресурсы"
  type        = string
}

variable "zone" {
  description = "Зона доступности для кластера, узлов и Redis"
  type        = string
  default     = "ru-central1-a"
}

variable "domain" {
  description = "Публичный домен сервиса (зона DNS создаётся в Cloud DNS)"
  type        = string
  default     = "maskarad.tech"
}

variable "name" {
  description = "Префикс имён ресурсов"
  type        = string
  default     = "maskarad"
}

variable "subnet_cidr" {
  description = "Подсеть мастера и Managed Valkey"
  type        = string
  default     = "10.10.0.0/24"
}

variable "nodes_subnet_cidr" {
  description = "Подсеть рабочих узлов"
  type        = string
  default     = "10.10.1.0/24"
}

variable "cluster_ipv4_range" {
  description = "Диапазон адресов подов"
  type        = string
  default     = "10.96.0.0/16"
}

variable "service_ipv4_range" {
  description = "Диапазон адресов сервисов"
  type        = string
  default     = "10.112.0.0/16"
}

variable "node_cores" {
  type    = number
  default = 2
}

variable "node_memory_gb" {
  type    = number
  default = 4
}

variable "node_disk_gb" {
  description = "Загрузочный диск узла, ГиБ (network-ssd). 32 — чтобы nodes_max узлов, один узел на пересоздание (deploy_policy.max_expansion = 1) умещались в квоту compute.ssdDisks.size; арифметика и текущие значения — infra/README.md, раздел «Квоты»"
  type        = number
  default     = 32
}

variable "nodes_min" {
  description = "Минимум узлов в группе. 4 — по итогам нагрузочного прогона 2026-09-23: одна реплика сервиса (request 750m) на узел 2 vCPU рядом с ingress-nginx; при двух узлах обе реплики оказывались на одном"
  type        = number
  default     = 4
}

variable "nodes_max" {
  description = "Максимум узлов. 8 = maxReplicas HPA в overlay production: один под на узел (topologySpreadConstraints DoNotSchedule), реплики 5–8 появляются только когда автоскейлер добавит узлы под Pending-поды. Выше квот по умолчанию: нужны compute.ssdDisks.size ≥ 9 × node_disk_gb ГиБ и vpc.externalAddresses.count ≥ 12 (у каждого узла публичный адрес) — infra/README.md, «Квоты»; без них группа не растёт и поды остаются в Pending"
  type        = number
  default     = 8
}

# Окна обслуживания задаются в UTC. Раньше стояло «в любое время»: автообновление мастера,
# пересоздание узлов и рестарт Valkey могли прийтись на нагрузочный прогон или рабочий день.
# Мастер и узлы — в разные дни: узлы обновляются после мастера, и оба события не попадают в один час.
variable "master_maintenance_window" {
  description = "Окно автообновления мастера Managed Kubernetes (UTC): день недели строчными буквами, начало HH:MM, длительность"
  type = object({
    day        = string
    start_time = string
    duration   = string
  })
  default = { day = "saturday", start_time = "02:00", duration = "3h" }
}

variable "nodes_maintenance_window" {
  description = "Окно автообновления и автовосстановления группы узлов (UTC); узлы пересоздаются по одному (deploy_policy)"
  type = object({
    day        = string
    start_time = string
    duration   = string
  })
  default = { day = "sunday", start_time = "01:00", duration = "3h" }
}

variable "redis_maintenance_window" {
  description = "Окно обслуживания Managed Service for Valkey: день недели (MON…SUN) и час начала в UTC (1–24); хост один, на время обслуживания хранилище недоступно"
  type = object({
    day  = string
    hour = number
  })
  default = { day = "SAT", hour = 1 }
}

variable "redis_preset" {
  description = "Класс хоста Managed Service for Valkey. hm3-c2-m8 — минимальный класс с гарантией vCPU 100% (2 vCPU, 8 ГБ); burstable-классы b1/b2/b3 создавать больше нельзя."
  type        = string
  default     = "hm3-c2-m8"

  validation {
    condition     = !can(regex("^b[123]", var.redis_preset))
    error_message = "Burstable-классы (b1/b2/b3) недоступны для новых кластеров — укажите класс с гарантией vCPU 100%, например hm3-c2-m8."
  }
}

variable "redis_version" {
  description = "Версия Managed Service for Valkey (config.version): 7.2-valkey, 8.0-valkey, 8.1-valkey или 9.0-valkey"
  type        = string
  default     = "8.1-valkey"

  validation {
    condition     = can(regex("^[0-9]+\\.[0-9]+-valkey$", var.redis_version))
    error_message = "Managed Service for Valkey принимает только версии с суффиксом -valkey, например 8.1-valkey."
  }
}

variable "redis_disk_gb" {
  description = "Диск хоста Valkey, ГБ (не меньше удвоенной памяти класса)"
  type        = number
  default     = 16
}

variable "log_retention" {
  description = "Срок хранения записей в лог-группе Cloud Logging, где остались только события мастера Kubernetes (логи подов — в Monium). 72h — три дня: хватает разобрать инцидент после выходных; максимум сервиса — 31 день"
  type        = string
  default     = "72h"

  validation {
    condition     = can(regex("^([0-9]+[hms])+$", var.log_retention))
    error_message = "Cloud Logging принимает срок хранения только в часах, минутах и секундах, например 72h или 36h30m."
  }
}
