#!/usr/bin/env bash
# Устанавливает или обновляет ingress-nginx закреплённой версии чарта с образами из Container Registry
# каталога. Образы чарта (контроллер и kube-webhook-certgen) копируются с раннера из registry.k8s.io
# в cr.yandex/<реестр> как есть — digest сохраняется, так что pin чарта по digest остаётся в силе;
# узлы тянут registry.k8s.io через раз (контроллер на новом узле застревал в ImagePullBackOff),
# а из cr.yandex — всегда. Реестр — тот же, что у образа сервиса, узлы уже имеют роль puller.
# Использование: scripts/ingress_nginx_helm.sh <адрес балансировщика>
# Нужны helm, yc (с настроенным профилем), docker с buildx, jq; версия чарта — INGRESS_NGINX_CHART_VERSION
# (задаётся в .github/workflows/infra.yml и ops.yml).
set -euo pipefail

ip="${1:?load balancer address}"
version="${INGRESS_NGINX_CHART_VERSION:?set INGRESS_NGINX_CHART_VERSION (see .github/workflows/infra.yml)}"
values=deploy/k8s/addons/ingress-nginx.values.yaml
source_registry=registry.k8s.io

registry="cr.yandex/$(yc container registry get --name maskarad --format json | jq -r .id)"
echo "ingress-nginx chart $version, images from $registry"

helm repo add ingress-nginx https://kubernetes.github.io/ingress-nginx >/dev/null 2>&1 || true
helm repo update ingress-nginx >/dev/null
chart_dir=$(mktemp -d)
helm pull ingress-nginx/ingress-nginx --version "$version" --destination "$chart_dir" >/dev/null
chart="$chart_dir/ingress-nginx-$version.tgz"
field() { helm show values "$chart" --jsonpath "{$1}"; }

yc container registry configure-docker >/dev/null

# mirror <путь image-блока в values чарта>: копирует образ, печатает digest копии.
mirror() {
  local image tag digest src dst copied
  image=$(field "$1.image")
  tag=$(field "$1.tag")
  digest=$(field "$1.digest")
  [ -n "$image" ] && [ -n "$tag" ] && [ -n "$digest" ] || { echo "cannot read $1 from chart values" >&2; exit 1; }
  src="$source_registry/$image:$tag@$digest"
  dst="$registry/$image:$tag"
  docker buildx imagetools create --tag "$dst" "$src" >/dev/null
  copied=$(docker buildx imagetools inspect "$dst" --format '{{.Manifest.Digest}}')
  echo "mirrored $src -> $dst ($copied)" >&2
  [ "$copied" = "$digest" ] || echo "::warning::digest changed on copy for $dst: chart $digest, registry $copied" >&2
  echo "$copied"
}
controller_digest=$(mirror .controller.image)
certgen_digest=$(mirror .controller.admissionWebhooks.patch.image)

helm upgrade --install ingress-nginx ingress-nginx/ingress-nginx --version "$version" \
  -n ingress-nginx --create-namespace -f "$values" \
  --set controller.service.loadBalancerIP="$ip" \
  --set global.image.registry="$registry" \
  --set controller.image.registry="$registry" \
  --set controller.image.digest="$controller_digest" \
  --set controller.admissionWebhooks.patch.image.registry="$registry" \
  --set controller.admissionWebhooks.patch.image.digest="$certgen_digest" \
  --wait --timeout 10m
rm -rf "$chart_dir"
