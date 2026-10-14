#!/usr/bin/env bash
# Runs deploy/chart/tests/e2e/clickhouse-admin.e2e.ts against the lab's k3s through the SSH forward.
E=${E2E_WORK:?set E2E_WORK to a private directory holding kc/kubeconfig (namespace e2e-403 only); output goes to $E2E_WORK/out}
cd "${REPO_ROOT:?set REPO_ROOT to the root of a checkout of the branch}" || exit 2
export PATH="$HOME/.proto/shims:$HOME/.proto/bin:$PATH"
mkdir -p "$E/out"
date -u +%FT%TZ > "$E/out/start.txt"
git rev-parse HEAD > "$E/out/head.txt"
KUBECONFIG=$E/kc/kubeconfig \
E2E_NAMESPACE=e2e-403 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:403 \
E2E_IMAGE_PULL_POLICY=Never E2E_REPORT_DIR=$E/out \
bun test --timeout 900000 --reporter=junit --reporter-outfile=$E/out/clickhouse-admin-e2e.junit.xml ./deploy/chart/tests/e2e/clickhouse-admin.e2e.ts > "$E/out/test.log" 2>&1
rc=$?
echo "rc=$rc" >> "$E/out/test.log"
date -u +%FT%TZ > "$E/out/end.txt"
exit $rc
