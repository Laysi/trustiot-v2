#!/usr/bin/env bash
# A fresh install with no Secret made beforehand: the chart's defaults (generated admin password),
# ClickHouse only, as in clickhouse-admin.e2e.ts. Prints names and states only, never a value.
# Exits non-zero when the install, any check or the cleanup fails. (The status checks were added
# after the 2026-10-15 run, whose output, lab-fresh-install.txt, shows every check succeeding.)
set -uo pipefail
E=${E2E_WORK:?set E2E_WORK to a private directory holding kc/kubeconfig (namespace e2e-403 only); output goes to $E2E_WORK/out}
export KUBECONFIG=$E/kc/kubeconfig
K=$(proto bin kubectl --reporter text)
H=$(proto bin helm --reporter text)
cd "${REPO_ROOT:?set REPO_ROOT to the root of a checkout of the branch}/deploy/chart" || exit 2
echo "# $(date -u +%FT%TZ) secrets before the install:"
$K -n e2e-403 get secret -o name
$H install trustiot . -n e2e-403 -f values-onprem.yaml -f tests/values/onprem-instance.yaml -f tests/e2e/values-otel-e2e.yaml \
  --set observability.collector.enabled=false --set services.assets.enabled=false \
  --set-string migrate.image.repository=localhost/trustiot-e2e/migrate --set-string migrate.image.tag=403 \
  --set-string migrate.image.digest= --set-string migrate.image.pullPolicy=Never \
  --wait --timeout 10m > /dev/null
rc=$?
echo "# $(date -u +%FT%TZ) helm install --wait exit $rc"
$K -n e2e-403 get pod trustiot-clickhouse-0 -o jsonpath='{range .status.initContainerStatuses[*]}init {.name}: exit {.state.terminated.exitCode} {.state.terminated.reason}{"\n"}{end}{range .status.conditions[?(@.type=="Ready")]}Ready={.status}{"\n"}{end}nodeSelector={.spec.nodeSelector}{"\n"}' || rc=1
$K -n e2e-403 get secret trustiot-clickhouse-admin-password -o go-template='keys:{{range $k, $v := .data}} {{$k}} (length {{len $v}} base64){{end}}{{"\n"}}generation={{index .metadata.annotations "trustiot.io/password-generation"}} bound={{eq (index .metadata.annotations "trustiot.io/password-secret-uid") .metadata.uid}}{{"\n"}}' || rc=1
echo "# init container log:"
$K -n e2e-403 logs trustiot-clickhouse-0 -c admin-password || rc=1
echo "# ClickHouse accepts the password from the file the init container wrote:"
$K -n e2e-403 exec trustiot-clickhouse-0 -c clickhouse -- bash -c 'exec clickhouse-client --user "$CLICKHOUSE_USER" --password "$(cat "$CLICKHOUSE_PASSWORD_FILE")" --query "SELECT currentUser()"' || rc=1
echo "# file mode in the server container:"
$K -n e2e-403 exec trustiot-clickhouse-0 -c clickhouse -- stat -c '%a %u:%g %n' /etc/trustiot/clickhouse-admin/password || rc=1
echo "# cleanup"
$H uninstall trustiot -n e2e-403 --wait --timeout 3m || rc=1
$K -n e2e-403 delete pvc data-trustiot-clickhouse-0 --wait=true --timeout=180s || rc=1
$K -n e2e-403 delete secret trustiot-clickhouse-admin-password || rc=1
$K -n e2e-403 get all,secret,pvc
# Nothing of the release may be left (the namespace's own default objects are not listed by name).
left=$($K -n e2e-403 get statefulset,pod,pvc,secret -l app.kubernetes.io/instance=trustiot -o name) || rc=1
for name in pvc/data-trustiot-clickhouse-0 secret/trustiot-clickhouse-admin-password; do
  if $K -n e2e-403 get "$name" -o name > /dev/null 2>&1; then left="$left $name"; fi
done
if [ -n "$left" ]; then echo "# left behind:$left"; rc=1; fi
exit $rc
