# Helm chart 骨架：兩份 values 的檢查與安裝（2026-10-10）

對應 issue：Laysi/trustiot-v2#66。原始資料在 [data/2026-10-10-66-helm-chart-install/](data/2026-10-10-66-helm-chart-install/)。

## 環境

- **commit**：`5ceaa26`（分支 `feat/66-helm-chart-skeleton`，base 為 `integration/ms0` 的 `175aac2`），chart `trustiot` 0.1.0。lab 安裝是在 rebase 前的 `010cfad` 上跑的；rebase 只換了 base，本 issue 改動的檔案內容完全相同（`git diff 010cfad 5ceaa26` 在這些路徑上沒有差異）。本機檢查在 `5ceaa26` 上重跑過。
- **本機**：x86_64 的 Fedora。helm 4.3.0、kubeconform 0.8.0、bun 1.4.2，都由 `.prototools` 鎖定版本、以 proto 安裝。kubeconform 用 Kubernetes 1.30.0（`Chart.yaml` 的 `kubeVersion` 下限）與 1.36.0 的 schema。
- **lab 的 Mac mini M1**（`trustiot-mac-m1`）：Fedora Linux Asahi Remix 44、aarch64、k3s v1.36.5+k3s1（containerd 2.3.4）、Istio 1.30.5 ambient（本次沒有改動）。詳見 [environment-lab.txt](data/2026-10-10-66-helm-chart-install/environment-lab.txt)。
- **命名空間**：`e2e-66`，Pod Security Admission 設為 `restricted`（enforce、warn、audit 都是 restricted，enforce-version 為 latest）。沒有加入 ambient mesh，入口與 mesh 設定屬於 Laysi/trustiot-v2#62。命名空間裡的 `istio-ca-root-cert` 與 `istio-ca-crl` 兩個 ConfigMap 是 istiod 放的，不是 chart 產生的。
- **替身服務**：TrustIoT 的服務映像還沒有發布，所以 12 個服務維持預設關閉。通用服務樣板以 `tests/values/smoke.yaml` 的 `smoke-echo` 部署，映像是 `docker.io/traefik/whoami:v1.12.0`，以多架構 index digest `sha256:c4717a8d…` 固定，含 arm64。
- **存取方式**：helm 在本機執行，經 SSH tunnel 連到 lab 的 API server。身分是 `e2e-66` 裡的 ServiceAccount `e2e-66-helm`，以 RoleBinding 綁 ClusterRole `admin`，權限只在這個命名空間內；token 有效 2 小時，隨命名空間一起刪除。kubeconfig 與 token 只放在本機的暫存目錄，測完已刪除。唯讀的檢查在 lab 主機上以 `sudo k3s kubectl` 執行。
- **測試用的 Secret**：`trustiot-sentry`、`trustiot-object-storage`、`trustiot-smtp`、`trustiot-pg-ca` 只放假值，例如 `e2e-dummy`、網域為 `sentry.invalid` 的 DSN，以及當場產生、私鑰直接丟棄的自簽憑證。刪除命名空間時一起刪除。

## 指令

以下都在 repo 根目錄執行，`kubectl` 指 lab 主機上的 `sudo k3s kubectl`。

本機檢查（[local-lint-kubeconform.txt](data/2026-10-10-66-helm-chart-install/local-lint-kubeconform.txt)、[local-bun-test.txt](data/2026-10-10-66-helm-chart-install/local-bun-test.txt)）：

```sh
proto install
bun test deploy/chart
helm lint deploy/chart --strict -f deploy/chart/values-saas.yaml -f deploy/chart/tests/values/saas-instance.yaml
helm template trustiot deploy/chart -n trustiot -f deploy/chart/values-saas.yaml -f deploy/chart/tests/values/saas-instance.yaml \
  | kubeconform -strict -summary -kubernetes-version 1.36.0 -
# 1.30.0 再跑一次；再加上 -f deploy/chart/tests/values/smoke.yaml 與 12 個 --set services.<name>.enabled=true 各跑一次；
# 地端把 saas 換成 onprem
```

lab 的準備（取得鎖之後，在 lab 主機上）：

```sh
mkdir ~/e2e.lock && echo "Laysi/trustiot-v2#66 $(date -u +%FT%TZ)" > ~/e2e.lock/owner
mkdir -p ~/e2e-66
sudo k3s kubectl create namespace e2e-66
sudo k3s kubectl label namespace e2e-66 pod-security.kubernetes.io/enforce=restricted \
  pod-security.kubernetes.io/enforce-version=latest pod-security.kubernetes.io/warn=restricted \
  pod-security.kubernetes.io/audit=restricted
sudo k3s kubectl -n e2e-66 create serviceaccount e2e-66-helm
sudo k3s kubectl -n e2e-66 create rolebinding e2e-66-helm --clusterrole=admin --serviceaccount=e2e-66:e2e-66-helm
sudo k3s kubectl -n e2e-66 create secret generic trustiot-sentry --from-literal=dsn=https://0000000000000000@sentry.invalid/0
sudo k3s kubectl -n e2e-66 create secret generic trustiot-object-storage \
  --from-literal=access-key-id=e2e-dummy --from-literal=secret-access-key=e2e-dummy
sudo k3s kubectl -n e2e-66 create secret generic trustiot-smtp --from-literal=username=e2e-dummy --from-literal=password=e2e-dummy
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout /dev/null \
  -subj "/CN=e2e-66 dummy CA" -days 1 -out ~/e2e-66/dummy-ca.crt
sudo k3s kubectl -n e2e-66 create secret generic trustiot-pg-ca --from-file=ca.crt=$HOME/e2e-66/dummy-ca.crt
```

本機連線：

```sh
# token 直接寫進本機暫存檔，不輸出到終端
ssh trustiot-mac-m1 'sudo k3s kubectl -n e2e-66 create token e2e-66-helm --duration=2h' > <暫存目錄>/token
ssh trustiot-mac-m1 'sudo cat /var/lib/rancher/k3s/server/tls/server-ca.crt' > <暫存目錄>/server-ca.crt
ssh -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=20 -L 127.0.0.1:16443:127.0.0.1:6443 trustiot-mac-m1 &
# KUBECONFIG 指向暫存目錄裡的 kubeconfig：server https://127.0.0.1:16443、上面的 CA 與 token、namespace e2e-66
```

每種部署型態各跑一次。以下是 SaaS；地端改用 `values-onprem.yaml` 與 `tests/values/onprem-instance.yaml`，不加 `saas-e2e-extra.yaml`，OTA 主機改為 `ota2.trustiot.factory.test`：

```sh
helm install trustiot deploy/chart -n e2e-66 -f deploy/chart/values-saas.yaml \
  -f deploy/chart/tests/values/saas-instance.yaml -f deploy/chart/tests/values/smoke.yaml \
  -f docs/test-reports/data/2026-10-10-66-helm-chart-install/saas-e2e-extra.yaml --wait --timeout 5m
kubectl -n e2e-66 get deploy,rs,pods,svc,endpointslices,cm,sa -o wide
kubectl -n e2e-66 get deploy trustiot-smoke-echo -o json | jq '{strategy: .spec.strategy, template: .spec.template}'
kubectl -n e2e-66 get configmap trustiot-platform -o json | jq .data
kubectl get --raw /api/v1/namespaces/e2e-66/services/trustiot-smoke-echo:http/proxy/healthz   # /readyz 同樣
# 在 lab 主機背景記錄 Service 的 ready endpoint，同時改一個平台設定
for i in $(seq 1 400); do [ -e ~/e2e-66/stop ] && break
  echo "$(date +%s.%N) $(sudo k3s kubectl -n e2e-66 get endpointslices -l kubernetes.io/service-name=trustiot-smoke-echo \
    -o jsonpath='{range .items[*].endpoints[*]}{.targetRef.name}={.conditions.ready} {end}')"; sleep 0.2
done > ~/e2e-66/poll.log &
helm upgrade trustiot deploy/chart -n e2e-66 <同樣的 -f> --set hosts.ota=ota2.saas.trustiot.test --wait --timeout 5m
touch ~/e2e-66/stop
kubectl -n e2e-66 get rs -o wide && helm history trustiot -n e2e-66
# 在 lab 主機上計時刪除 pod
s=$(date +%s.%N); sudo k3s kubectl -n e2e-66 delete pod <pod> --wait=true; e=$(date +%s.%N)
helm uninstall trustiot -n e2e-66 --wait --timeout 3m
```

反向對照（[negative-pod-security.txt](data/2026-10-10-66-helm-chart-install/negative-pod-security.txt)）：

```sh
helm install trustiot-neg deploy/chart -n e2e-66 -f deploy/chart/values-onprem.yaml \
  -f deploy/chart/tests/values/onprem-instance.yaml -f deploy/chart/tests/values/smoke.yaml \
  --set services.smoke-echo.securityContext.allowPrivilegeEscalation=true
# 同一行再加 --skip-schema-validation
kubectl -n e2e-66 get events --field-selector reason=FailedCreate
helm uninstall trustiot-neg -n e2e-66 --wait
```

清理：

```sh
sudo k3s kubectl delete namespace e2e-66 --wait=true   # 一併刪除 ServiceAccount、RoleBinding 與測試用 Secret
rm -rf ~/e2e-66 ~/e2e.lock
```

## 結果

### 本機

| 檢查 | SaaS | 地端 |
| --- | --- | --- |
| `helm lint --strict` | 通過，只有 `icon is recommended` 的 INFO | 通過，同左 |
| `helm template` + `kubeconform -strict`，1.30.0 與 1.36.0，服務維持預設關閉 | 1 個資源（平台設定 ConfigMap），全部有效 | 同左 |
| 同上，12 個服務全開，再加 `smoke-echo` | 40 個資源，全部有效 | 40 個資源，全部有效 |

`bun test deploy/chart`：91 項通過，0 項失敗。除了上面的 lint 與 kubeconform，也涵蓋兩種型態的平台設定、Secret 只給需要的服務、ClusterIP 與 restricted 安全設定、應該被拒絕的 values（49 種由 schema 或樣板擋下，另 7 種在跳過 schema 驗證時仍由樣板擋下），以及 README 的欄位檢查。

### lab 安裝

| 項目 | SaaS | 地端 |
| --- | --- | --- |
| `helm install --wait` | exit 0，7.9 秒 | exit 0，3.4 秒 |
| 建立的物件 | 平台設定 ConfigMap（23 個 key），以及 `smoke-echo` 的 ServiceAccount、ClusterIP Service、Deployment | 同左，ConfigMap 有 16 個 key |
| pod | Running、Ready，0 次重啟；restricted 沒有任何警告 | 同左 |
| 平台設定 | `TRUSTIOT_CLIENT_IP_SOURCE=cf-connecting-ip`，有 Qdrant URL，`SENTRY_ENVIRONMENT=test`，外部 Postgres 的主機與 `TRUSTIOT_POSTGRES_CA_FILE` | `x-forwarded-for-rightmost`，沒有 Qdrant URL，沒有 Sentry，SMTP 為客戶的 relay（port 25） |
| 密鑰 | `SENTRY_DSN`、S3 兩把與 SMTP 兩把以 `secretKeyRef` 注入；Postgres CA 唯讀掛在 `/etc/trustiot/postgres-ca`；pod 正常啟動，表示 Secret 與 key 都解析得到 | 沒有任何 Secret 引用，只有 `OTEL_SERVICE_NAME` 與替身自己的 `WHOAMI_PORT_NUMBER` |
| `/healthz`、`/readyz`（經 API server 的 service proxy） | 都回應 | 都回應 |
| `helm upgrade` 改 `hosts.ota` | revision 2；平台設定的 checksum 改變，由新的 ReplicaSet 接手，ConfigMap 的 `TRUSTIOT_OTA_HOST` 已更新 | 同左 |
| 更新期間 Service 的 ready endpoint（平均每 0.29 秒取樣一次） | 42 次取樣，每次至少 1 個，沒有任何一次是 0 | 43 次取樣，每次至少 1 個，沒有任何一次是 0 |
| 刪除 pod 到 pod 消失（在 lab 主機上計時） | 5.6 秒；`terminationGracePeriodSeconds` 為 35 | 5.7 秒；35 |
| `helm uninstall --wait` | chart 的物件都移除，pod 在指令結束後幾秒內終止 | 同左 |

逐步的輸出在 `saas-01` 到 `saas-11`、`onprem-01` 到 `onprem-11` 這兩組檔案（例如 [saas-03-deployment.json](data/2026-10-10-66-helm-chart-install/saas-03-deployment.json) 是 API server 儲存的 Deployment，[saas-08-endpoint-poll.txt](data/2026-10-10-66-helm-chart-install/saas-08-endpoint-poll.txt) 是更新期間的取樣）。

更新的過程兩種型態相同：新 pod 先出現、約 2 秒後 Ready，同一次取樣裡舊 pod 轉為不 Ready；舊 pod 的 endpoint 以 terminating 狀態再留約 5 秒，與 preStop 的 5 秒等待一致，之後才移除。刪除 pod 花 5.6–5.7 秒，同樣是 preStop 的 5 秒加上 whoami 收到 SIGTERM 後立即結束；寬限期 35 秒是 `preStopSleepSeconds` 5 加上 `drainSeconds` 30。

### 反向對照

| 情況 | 結果 |
| --- | --- |
| values 設 `allowPrivilegeEscalation=true` | `helm install` 直接失敗：`/services/smoke-echo/securityContext/allowPrivilegeEscalation: value must be false`，叢集上沒有建立任何東西；之後同名的 release 可以照常安裝 |
| 同上，加 `--skip-schema-validation` | API server 回警告 `would violate PodSecurity "restricted:latest"`；ReplicaSet 無法建立 pod（`FailedCreate`：`violates PodSecurity "restricted:latest": allowPrivilegeEscalation != false`），Deployment 維持 0/1 |

命名空間的 restricted 確實在執行，所以上面兩次安裝的 pod 能建立，就代表它們符合 restricted。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| `helm lint` 通過；兩份 values 的 `helm template` 都通過 kubeconform | 通過。服務關閉時與 12 個服務全開時，在 Kubernetes 1.30.0 與 1.36.0 都有效 |
| 以兩份 values 各安裝一次 | 通過。issue 寫的是 k3d，本次依協調者指示改在 lab 的 k3s（v1.36.5，與 SaaS 主機相同）安裝；兩種型態都 `helm install --wait` 成功，pod Ready、探針回應、升級不中斷、刪除時先等 preStop |
| `deploy/chart/README.md` 列出所有 values 欄位 | 通過。`bun test` 檢查 `values.yaml` 的每個欄位、每個服務，以及 schema 裡服務與埠的每個選填欄位都寫在 README |

待測：

- TrustIoT 自己的服務：映像尚未發布，本次以 whoami 替身驗證通用樣板，12 個服務只做 render 與 kubeconform。各服務出映像後，由 nightly（Laysi/trustiot-v2#69）在叢集上安裝。
- 叢集內的 Postgres、ClickHouse、Qdrant、MQTT broker 與入口 gateway：尚未加入 chart（Laysi/trustiot-v2#58、Laysi/trustiot-v2#59、Laysi/trustiot-v2#194、Laysi/trustiot-v2#84、Laysi/trustiot-v2#62）。地端入口在客戶網路開 443、SaaS 經 Cloudflare Tunnel 轉入，都要等入口 gateway 加入後才能測；本次只驗證平台設定裡的可信來源 IP 值。
- OpenShift 的 restricted-v2 SCC：目前沒有 OpenShift 環境。
- 服務實際讀到的環境變數與 CA 檔案內容：whoami 映像沒有 shell，無法 exec 進去查看。證據是 API server 儲存的 pod spec，以及 pod 能正常啟動：Secret 或 key 不存在時，容器會卡在 `CreateContainerConfigError`，volume 掛載失敗時 pod 會停在 `ContainerCreating`。
