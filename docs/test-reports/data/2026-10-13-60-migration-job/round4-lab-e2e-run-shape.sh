#!/usr/bin/env bash
# usage: run-shape.sh <saas|onprem>
shape=$1
E=<scratch>
cd <worktree> || exit 2
export PATH="$HOME/.proto/shims:$HOME/.proto/bin:$PATH"
date -u +%FT%TZ > $E/start-$shape.txt
KUBECONFIG=$E/kc/kubeconfig \
E2E_NAMESPACE=e2e-60 E2E_SHAPE=$shape \
E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:60 \
E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:60 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:60 \
E2E_IMAGE_PULL_POLICY=Never E2E_REPORT_DIR=$E/out \
bun test --timeout 900000 --reporter=junit --reporter-outfile=$E/out/postgres-e2e-$shape.junit.xml ./deploy/chart/tests/e2e/postgres.e2e.ts > $E/out/test-$shape.log 2>&1
rc=$?
echo "rc=$rc" >> $E/out/test-$shape.log
date -u +%FT%TZ > $E/end-$shape.txt
exit $rc
