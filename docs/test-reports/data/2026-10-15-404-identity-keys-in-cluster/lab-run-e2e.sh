#!/usr/bin/env bash
# Repeats the lab run of Laysi/trustiot-v2#404 (docs/test-reports/2026-10-15-404-identity-keys-in-cluster.md).
# Run from the repository root on a workstation; LAB_SSH names the lab node's SSH host, which this
# file does not hold. The images are built on the build host and on the workstation as the report
# says, and imported into the node's containerd beforehand.
set -euo pipefail
: "${LAB_SSH:?set LAB_SSH to the SSH host of the lab node}"
: "${WORK:?set WORK to a private directory (mode 0700) for the kubeconfig and the results}"

# On the node, after taking the lab lock: the images and the namespace.
#   sudo k3s ctr -n k8s.io images import --platform linux/arm64 <postgres|migrate|identity|example-service>.tar
#   sudo k3s kubectl create namespace e2e-404
#   sudo k3s kubectl label namespace e2e-404 pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/enforce-version=latest

# The API server through an SSH forward that reconnects when the connection drops. set -m puts
# the loop and its ssh into a process group of their own, which the trap ends together; the
# kubeconfig stays in $WORK and is deleted with them.
umask 077
ssh "$LAB_SSH" 'sudo cat /etc/rancher/k3s/k3s.yaml' | sed 's#https://127.0.0.1:6443#https://127.0.0.1:16443#' > "$WORK/kubeconfig"
set -m
(while true; do ssh -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=4 -L 127.0.0.1:16443:127.0.0.1:6443 "$LAB_SSH"; sleep 2; done) &
forward=$!
set +m
trap 'kill -- "-$forward" 2>/dev/null || true; wait "$forward" 2>/dev/null || true; rm -f "$WORK/kubeconfig"' EXIT

for shape in saas onprem; do
  KUBECONFIG="$WORK/kubeconfig" \
  E2E_NAMESPACE=e2e-404 \
  E2E_SHAPE="$shape" \
  E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:404 \
  E2E_IDENTITY_IMAGE=localhost/trustiot-e2e/identity:404 \
  E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:404 \
  E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:404 \
  E2E_IMAGE_PULL_POLICY=Never \
  E2E_REPORT_DIR="$WORK/out" \
    bun test --timeout 3600000 ./deploy/chart/tests/e2e/identity-keys.e2e.ts
done
