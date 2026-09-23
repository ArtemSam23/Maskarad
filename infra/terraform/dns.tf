# Публичная зона в Cloud DNS. У регистратора домен делегируется на
# ns1.yandexcloud.net и ns2.yandexcloud.net.
resource "yandex_dns_zone" "main" {
  name        = replace(var.domain, ".", "-")
  description = "Maskarad public zone"
  zone        = "${var.domain}."
  public      = true
}

locals {
  ingress_ip = yandex_vpc_address.ingress_lb.external_ipv4_address[0].address
  # Только apex: отдельных хостов у сервиса нет.
  hosts = {
    apex = "${var.domain}."
  }
}

resource "yandex_dns_recordset" "a" {
  for_each = local.hosts

  zone_id = yandex_dns_zone.main.id
  name    = each.value
  type    = "A"
  ttl     = 300
  data    = [local.ingress_ip]
}
