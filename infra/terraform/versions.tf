terraform {
  required_version = ">= 1.6"

  required_providers {
    # Lock-файла в репозитории нет, поэтому версия закреплена до патча: "~> 0.229" пустил бы любую 0.x.
    # На 0.229.0 конфигурация проверена validate.
    yandex = {
      source  = "yandex-cloud/yandex"
      version = "~> 0.229.0"
    }
    random = {
      source  = "hashicorp/random"
      version = "~> 3.6"
    }
  }

  # Состояние хранится в Object Storage Яндекс Облака (S3-совместимый API).
  # Имя бакета передаётся при init: -backend-config="bucket=…"; ключи доступа —
  # через AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY (статический ключ сервисного аккаунта).
  backend "s3" {
    endpoints = {
      s3 = "https://storage.yandexcloud.net"
    }
    region                      = "ru-central1"
    key                         = "maskarad/terraform.tfstate"
    skip_region_validation      = true
    skip_credentials_validation = true
    skip_requesting_account_id  = true
    skip_s3_checksum            = true
  }
}

# Аутентификация провайдера — через переменную окружения YC_SERVICE_ACCOUNT_KEY_FILE
# (авторизованный ключ сервисного аккаунта), её выставляет composite action .github/actions/yc.
provider "yandex" {
  cloud_id  = var.cloud_id
  folder_id = var.folder_id
  zone      = var.zone
}
