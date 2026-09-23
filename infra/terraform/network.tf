resource "yandex_vpc_network" "main" {
  name = var.name
}

# Без NAT-шлюза и маршрута 0.0.0.0/0: в подсети с таким маршрутом ответный трафик мастера
# (публичный API) и узлов за сетевым балансировщиком уходит через шлюз, и снаружи они не отвечают.
# Узлы получают публичные адреса (nat = true в группе узлов), входящий трафик на них
# ограничен группой безопасности.

# Подсеть мастера и Managed Valkey.
resource "yandex_vpc_subnet" "main" {
  name           = "${var.name}-${var.zone}"
  zone           = var.zone
  network_id     = yandex_vpc_network.main.id
  v4_cidr_blocks = [var.subnet_cidr]
}

# Подсеть рабочих узлов.
resource "yandex_vpc_subnet" "nodes" {
  name           = "${var.name}-nodes-${var.zone}"
  zone           = var.zone
  network_id     = yandex_vpc_network.main.id
  v4_cidr_blocks = [var.nodes_subnet_cidr]
}

# Статический публичный адрес для сетевого балансировщика ingress-nginx:
# DNS-записи указывают на него, а не на адрес, который выдал бы балансировщик сам.
#
# Адрес из ru-central1-d, хотя узлы в ru-central1-a. Прежний 93.77.189.227
# (ru-central1-a) и пробный 46.21.245.29 (ru-central1-a) теряли входящие SYN
# до узлов: из России 0/10, с раннеров GitHub через раз, при HEALTHY targets.
# Балансировщик на эфемерном адресе 84.201.147.113 (облако выдало его в
# ru-central1-d) на тех же узлах отвечал 10/10. Зона это или диапазон адресов —
# не установлено. Адрес зарезервирован вручную и импортирован, чтобы не
# зависеть от того, что выдаст облако при создании нового.
import {
  to = yandex_vpc_address.ingress_lb
  id = "fl876i7hdljolko0hi2j"
}

resource "yandex_vpc_address" "ingress_lb" {
  name = "${var.name}-ingress"

  external_ipv4_address {
    zone_id = "ru-central1-d"
  }
}

# Прежний адрес 93.77.189.227 (maskarad-ingress-old) выводится из-под Terraform
# без удаления: пока addons не переключили Service, он занят балансировщиком.
# После переключения освобождается вручную: yc vpc address delete maskarad-ingress-old.
removed {
  from = yandex_vpc_address.ingress

  lifecycle {
    destroy = false
  }
}

# Группа безопасности мастера и узлов — по рекомендациям документации Managed Kubernetes.
resource "yandex_vpc_security_group" "k8s" {
  name        = "${var.name}-k8s"
  description = "Kubernetes control plane and worker nodes"
  network_id  = yandex_vpc_network.main.id

  ingress {
    description       = "Health checks of the network load balancer"
    protocol          = "TCP"
    predefined_target = "loadbalancer_healthchecks"
    from_port         = 0
    to_port           = 65535
  }

  ingress {
    description       = "Node-to-node and control plane traffic"
    protocol          = "ANY"
    predefined_target = "self_security_group"
    from_port         = 0
    to_port           = 65535
  }

  ingress {
    description    = "Pod and service networks"
    protocol       = "ANY"
    v4_cidr_blocks = [var.cluster_ipv4_range, var.service_ipv4_range]
    from_port      = 0
    to_port        = 65535
  }

  ingress {
    description    = "ICMP from private networks"
    protocol       = "ICMP"
    v4_cidr_blocks = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]
  }

  ingress {
    description    = "NodePorts behind the network load balancer"
    protocol       = "TCP"
    v4_cidr_blocks = ["0.0.0.0/0"]
    from_port      = 30000
    to_port        = 32767
  }

  ingress {
    description    = "Kubernetes API"
    protocol       = "TCP"
    v4_cidr_blocks = ["0.0.0.0/0"]
    port           = 443
  }

  ingress {
    description    = "Kubernetes API"
    protocol       = "TCP"
    v4_cidr_blocks = ["0.0.0.0/0"]
    port           = 6443
  }

  egress {
    description    = "All outbound traffic"
    protocol       = "ANY"
    v4_cidr_blocks = ["0.0.0.0/0"]
    from_port      = 0
    to_port        = 65535
  }
}

resource "yandex_vpc_security_group" "redis" {
  name        = "${var.name}-redis"
  description = "Managed Redis: reachable only from the Kubernetes nodes"
  network_id  = yandex_vpc_network.main.id

  ingress {
    description       = "Redis from Kubernetes nodes"
    protocol          = "TCP"
    security_group_id = yandex_vpc_security_group.k8s.id
    port              = 6379
  }

  egress {
    description    = "All outbound traffic"
    protocol       = "ANY"
    v4_cidr_blocks = ["0.0.0.0/0"]
    from_port      = 0
    to_port        = 65535
  }
}
