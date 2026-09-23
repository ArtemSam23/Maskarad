resource "random_password" "redis" {
  length  = 24
  special = false
}

# Общее хранилище соответствий для всех реплик сервиса (Managed Service for Valkey,
# протокол Redis). Значения в нём зашифрованы самим сервисом (AES-256-GCM),
# хранилище видит только шифртекст. Диск — не меньше 2×RAM класса хоста.
resource "yandex_mdb_redis_cluster" "main" {
  name               = var.name
  environment        = "PRODUCTION"
  network_id         = yandex_vpc_network.main.id
  security_group_ids = [yandex_vpc_security_group.redis.id]
  tls_enabled        = false

  config {
    password = random_password.redis.result
    version  = var.redis_version
    # NOEVICTION: соответствие маскирования, вытесненное по LRU до истечения TTL, — это сломанное
    # демаскирование у другой реплики. Лучше явный отказ записи (/process отвечает 429 STORE_BUSY,
    # проверяющая система повторяет запрос, метрика store_ops result=error), чем тихая потеря.
    # Объём ограничен TTL (store.ttl_seconds = 3600) при 8 ГБ хоста.
    maxmemory_policy = "NOEVICTION"
  }

  resources {
    resource_preset_id = var.redis_preset
    disk_size          = var.redis_disk_gb
    disk_type_id       = "network-ssd"
  }

  host {
    zone      = var.zone
    subnet_id = yandex_vpc_subnet.main.id
  }

  maintenance_window {
    type = "WEEKLY"
    day  = var.redis_maintenance_window.day
    hour = var.redis_maintenance_window.hour
  }
}
