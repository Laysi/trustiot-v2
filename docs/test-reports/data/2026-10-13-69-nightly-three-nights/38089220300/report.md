## nightly smoke test: passed

- started 2026-10-10T21:51:47.571Z, finished 2026-10-10T21:58:11.735Z
- cluster: k3d with `rancher/k3s:v1.36.5-k3s1@sha256:057cfd5a9d51b89f1f0ee139c28d5d5fdd5aa7ddeac2742a0ba90c6dbc0329bb`

### values-onprem.yaml: passed (1m33s)

| check | result | duration | detail |
| --- | --- | --- | --- |
| namespace with the restricted Pod Security level | ok | 0s | enforce, warn and audit are restricted |
| dummy Secrets for the references the chart makes | ok | 0s | 2 created: trustiot-clickhouse-admin, trustiot-ingress-tls |
| helm install --wait | ok | 18s | release trustiot deployed, 18 objects |
| every workload of the release is ready | ok | 0s | Deployment/trustiot-smoke-echo, StatefulSet/trustiot-clickhouse |
| every Gateway is programmed | ok | 0s | trustiot-ingress |
| no pod is broken, not ready or crash-looping | ok | 0s | 4 pods running or completed |
| Services that expose a health endpoint have ready endpoints | ok | 0s | trustiot-smoke-echo:http/healthz, trustiot-smoke-echo:http/readyz, trustiot-clickhouse:http/ping |
| helm uninstall leaves no object of the release | ok | 1s | all objects removed |
| ingress e2e test (deploy/chart/tests/e2e) | ok | 1m03s | 61 passed, 0 failed |
| delete the namespace | ok | 11s | deleted |

### values-saas.yaml: passed (3m41s)

| check | result | duration | detail |
| --- | --- | --- | --- |
| namespace with the restricted Pod Security level | ok | 0s | enforce, warn and audit are restricted |
| dummy Secrets for the references the chart makes | ok | 0s | 2 created: trustiot-sentry, trustiot-clickhouse-admin |
| helm install --wait | ok | 10s | release trustiot deployed, 18 objects |
| every workload of the release is ready | ok | 0s | Deployment/trustiot-smoke-echo, StatefulSet/trustiot-clickhouse |
| every Gateway is programmed | ok | 0s | trustiot-ingress |
| no pod is broken, not ready or crash-looping | ok | 0s | 4 pods running or completed |
| Services that expose a health endpoint have ready endpoints | ok | 0s | trustiot-smoke-echo:http/healthz, trustiot-smoke-echo:http/readyz, trustiot-clickhouse:http/ping |
| helm uninstall leaves no object of the release | ok | 1s | all objects removed |
| ingress e2e test (deploy/chart/tests/e2e) | ok | 3m19s | 61 passed, 0 failed |
| delete the namespace | ok | 11s | deleted |
