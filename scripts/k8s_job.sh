#!/usr/bin/env bash
# Запускает одноразовый Job из образа сервиса рядом с ним (внутри кластера) и печатает его лог.
#   scripts/k8s_job.sh <namespace> <name-prefix> <image> <timeout-seconds> <args of /app/maskarad...>
# Пример: scripts/k8s_job.sh maskarad smoke cr.yandex/x/maskarad:tag 300 smoke https://maskarad.tech
# Код возврата: 0, если Job завершился успешно, иначе 1 (лог и describe печатаются в stderr).
set -euo pipefail
ns="${1:?namespace}"; prefix="${2:?name}"; image="${3:?image}"; timeout="${4:?timeout}"; shift 4
[ "$#" -gt 0 ] || { echo "usage: k8s_job.sh <ns> <name> <image> <timeout> <args...>" >&2; exit 2; }
name="$prefix-$(date +%s)"
args_json="$(printf '%s\n' "$@" | jq -R . | jq -cs .)"

kubectl -n "$ns" apply -f - >/dev/null <<MANIFEST
apiVersion: batch/v1
kind: Job
metadata:
  name: $name
  labels: { app.kubernetes.io/name: maskarad, app.kubernetes.io/component: $prefix }
spec:
  backoffLimit: 0
  activeDeadlineSeconds: $timeout
  ttlSecondsAfterFinished: 900
  template:
    metadata:
      labels: { app.kubernetes.io/name: maskarad-$prefix }
    spec:
      restartPolicy: Never
      securityContext: { runAsNonRoot: true, runAsUser: 65532, runAsGroup: 65532 }
      containers:
        - name: $prefix
          image: $image
          args: $args_json
          resources:
            requests: { cpu: "200m", memory: "64Mi" }
            limits: { cpu: "1", memory: "256Mi" }
MANIFEST

deadline=$(( $(date +%s) + timeout + 60 ))
status=""
while [ "$(date +%s)" -lt "$deadline" ]; do
  succeeded="$(kubectl -n "$ns" get job "$name" -o jsonpath='{.status.succeeded}' 2>/dev/null || true)"
  failed="$(kubectl -n "$ns" get job "$name" -o jsonpath='{.status.failed}' 2>/dev/null || true)"
  if [ "${succeeded:-0}" -ge 1 ] 2>/dev/null; then status=ok; break; fi
  if [ "${failed:-0}" -ge 1 ] 2>/dev/null; then status=failed; break; fi
  sleep 5
done

kubectl -n "$ns" logs "job/$name" --all-containers 2>/dev/null || true
if [ "$status" != ok ]; then
  echo "job $name: ${status:-timed out}" >&2
  kubectl -n "$ns" describe "job/$name" 2>/dev/null | tail -25 >&2 || true
  kubectl -n "$ns" delete job "$name" --ignore-not-found >/dev/null 2>&1 || true
  exit 1
fi
kubectl -n "$ns" delete job "$name" --ignore-not-found >/dev/null 2>&1 || true
