resource "yandex_kubernetes_cluster" "main" {
  name        = var.name
  description = "Maskarad PII masking proxy"
  network_id  = yandex_vpc_network.main.id

  cluster_ipv4_range = var.cluster_ipv4_range
  service_ipv4_range = var.service_ipv4_range

  master {
    zonal {
      zone      = var.zone
      subnet_id = yandex_vpc_subnet.main.id
    }

    public_ip          = true
    security_group_ids = [yandex_vpc_security_group.k8s.id]

    maintenance_policy {
      auto_upgrade = true

      maintenance_window {
        day        = var.master_maintenance_window.day
        start_time = var.master_maintenance_window.start_time
        duration   = var.master_maintenance_window.duration
      }
    }

    # События кластера (OOM, вытеснения, HPA) и логи автоскейлера — в лог-группу Cloud Logging:
    # без Loki в кластере иначе не разобрать, почему пересоздались поды. В Monium master_logging
    # писать не умеет, но группы Cloud Logging Monium читает: записи видны как
    # {cluster="default", service="maskarad"} (service — имя лог-группы).
    # kube-apiserver и аудит выключены: самые объёмные потоки, для разбора
    # инцидентов приложения не нужны. Меняется на живом кластере без пересоздания.
    master_logging {
      enabled                    = true
      log_group_id               = yandex_logging_group.main.id
      kube_apiserver_enabled     = false
      cluster_autoscaler_enabled = true
      events_enabled             = true
      audit_enabled              = false
    }
  }

  service_account_id      = yandex_iam_service_account.k8s.id
  node_service_account_id = yandex_iam_service_account.k8s.id
  release_channel         = "STABLE"

  kms_provider {
    key_id = yandex_kms_symmetric_key.k8s.id
  }

  depends_on = [yandex_resourcemanager_folder_iam_member.k8s]
}

resource "yandex_kubernetes_node_group" "main" {
  cluster_id  = yandex_kubernetes_cluster.main.id
  name        = "${var.name}-workers"
  description = "Autoscaled worker nodes"

  instance_template {
    platform_id = "standard-v3"

    resources {
      cores         = var.node_cores
      memory        = var.node_memory_gb
      core_fraction = 100
    }

    boot_disk {
      type = "network-ssd"
      size = var.node_disk_gb
    }

    network_interface {
      subnet_ids         = [yandex_vpc_subnet.nodes.id]
      security_group_ids = [yandex_vpc_security_group.k8s.id]
      nat                = true
    }

    container_runtime {
      type = "containerd"
    }

    scheduling_policy {
      preemptible = false
    }
  }

  scale_policy {
    auto_scale {
      min     = var.nodes_min
      max     = var.nodes_max
      initial = var.nodes_min
    }
  }

  allocation_policy {
    location {
      zone = var.zone
    }
  }

  # Пересоздание узлов по одному: запас квоты на диски нужен только на один узел.
  deploy_policy {
    max_expansion   = 1
    max_unavailable = 0
  }

  maintenance_policy {
    auto_upgrade = true
    auto_repair  = true

    maintenance_window {
      day        = var.nodes_maintenance_window.day
      start_time = var.nodes_maintenance_window.start_time
      duration   = var.nodes_maintenance_window.duration
    }
  }
}
