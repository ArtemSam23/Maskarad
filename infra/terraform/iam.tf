# Один сервисный аккаунт для control plane и узлов: создаёт балансировщики,
# публичные адреса, тянет образы из Container Registry, шифрует секреты через KMS,
# пишет события мастера в Cloud Logging (master_logging в kubernetes.tf).
resource "yandex_iam_service_account" "k8s" {
  name        = "${var.name}-k8s"
  description = "Managed Kubernetes cluster and node service account"
}

resource "yandex_resourcemanager_folder_iam_member" "k8s" {
  for_each = toset([
    "k8s.clusters.agent",
    "vpc.publicAdmin",
    "load-balancer.admin",
    "container-registry.images.puller",
    "kms.keys.encrypterDecrypter",
    "logging.writer",
  ])

  folder_id = var.folder_id
  role      = each.value
  member    = "serviceAccount:${yandex_iam_service_account.k8s.id}"
}

# Ключ для шифрования секретов Kubernetes на диске (etcd).
resource "yandex_kms_symmetric_key" "k8s" {
  name              = "${var.name}-k8s-secrets"
  default_algorithm = "AES_128"
  rotation_period   = "8760h"
}
