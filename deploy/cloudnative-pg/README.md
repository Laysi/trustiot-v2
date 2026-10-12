# CloudNativePG operator（平台元件）

chart 的叢集內 Postgres（`postgres.mode: bundled`，見 [chart 的「叢集內的 Postgres」](../chart/README.md#叢集內的-postgres)）由 CloudNativePG 管理（清單 M8）。operator 與它的 CRD 是整個叢集共用的平台元件，和 [Istio](../istio/README.md) 一樣不放進 `deploy/chart`，每個叢集在裝 chart 之前先依本頁裝一次。chart 只產生 namespace 內的 ImageCatalog、Cluster、Database 與 NetworkPolicy。

## 為什麼不放進 chart

決策 5 的 2026-10-12 補充（Laysi/trustiot-v2#58）。issue 原本寫「chart 內安裝 operator」，實作時改成獨立的平台元件，理由：

- operator 會建立 CRD、mutating 與 validating webhook、ClusterRole 與 ClusterRoleBinding，這些都是叢集層級、一個叢集只能有一份。放進 chart 後，同一個叢集的第二個 release（另一個 namespace 的測試或 staging、e2e）會互相搶同一批物件，`helm install` 直接失敗。
- Helm 不升級 `crds/` 目錄裡的 CRD；把 CRD 當一般 template 管理時，`helm uninstall` 會刪掉 CRD，而刪掉 CRD 會連帶刪掉所有 namespace 的 Cluster，資料庫跟著沒了。
- 地端客戶可能已經有自己的 CloudNativePG（例如 OpenShift 由 OperatorHub 安裝），chart 再裝一份會衝突。
- operator 的升級節奏與產品無關（CloudNativePG 約每 2 到 3 個月一個 minor），升級時會滾動更新所有 Postgres pod，應該是平台維運的獨立動作。

誰來裝：每個叢集由維運在裝 chart 之前安裝一次。SaaS 主機在 `deploy/host/README.md` 的安裝步驟裡、Istio 之後；SaaS 的 Flux（[deploy/flux](../flux/README.md) 的「建立叢集」）部署 chart 時以 operator 已經在跑為前提，和 Istio 一樣不由 Flux 安裝；地端由客戶或我們的安裝人員依本頁安裝。改由 Flux 管理平台元件時，operator 要排在 chart 的 HelmRelease 之前（`dependsOn`）。

## 版本

- CloudNativePG 1.30.1，官方 release 的單一 manifest `cnpg-1.30.1.yaml`：`https://github.com/cloudnative-pg/cloudnative-pg/releases/download/v1.30.1/cnpg-1.30.1.yaml`，SHA-256 `37237f145d8138256ea25ae830f87759255665ff08f8d552fdd8224a5ec032fb`。nightly 以同一組版本與雜湊安裝（`.github/workflows/nightly.yml` 的 `CNPG_VERSION`、`CNPG_MANIFEST_SHA256`，`bun test scripts/nightly` 檢查兩邊相同）；升級時一起改。
- operator 映像是 manifest 裡的 `ghcr.io/cloudnative-pg/cloudnative-pg:1.30.1`（multi-arch，含 arm64）。manifest 以 tag 指定映像；manifest 本身以 SHA-256 固定，映像要以 digest 固定時，在 SaaS 的 Flux 設定以 Kustomize 的 `images` 改寫。
- chart 用到 CloudNativePG 1.30 的欄位（ImageCatalog、Database 的 `extensions`、`localeProvider: builtin`），只在 1.30.1 驗證過。客戶已有的 operator 版本較舊時，先升級。

## 安裝

在可以連到叢集的機器上，kubectl 用 repo 鎖定的版本（`proto install`）：

```sh
curl -fsSLo cnpg-1.30.1.yaml https://github.com/cloudnative-pg/cloudnative-pg/releases/download/v1.30.1/cnpg-1.30.1.yaml
echo "37237f145d8138256ea25ae830f87759255665ff08f8d552fdd8224a5ec032fb  cnpg-1.30.1.yaml" | sha256sum -c -
# Server-side：CRD 太大，放不進 client-side apply 的 annotation
kubectl apply --server-side -f cnpg-1.30.1.yaml
# operator 的 pod 符合 restricted Pod Security Standard
kubectl label namespace cnpg-system pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/warn=restricted pod-security.kubernetes.io/audit=restricted
kubectl wait --for=condition=Established crd/clusters.postgresql.cnpg.io crd/imagecatalogs.postgresql.cnpg.io crd/databases.postgresql.cnpg.io --timeout=120s
kubectl -n cnpg-system rollout status deployment/cnpg-controller-manager --timeout=180s
```

operator 啟動後才接受 Cluster：它的 webhook 憑證在啟動時產生，`rollout status` 完成前裝 chart 會被 webhook 拒絕。

manifest 加進叢集的東西（2026-10-12 在實驗機逐一確認，見測試報告）：

| 範圍 | 物件 |
| --- | --- |
| 叢集層級 | Namespace `cnpg-system`；11 個 CRD（`*.postgresql.cnpg.io`：backups、clusterimagecatalogs、clusters、databaseroles、databases、failoverquorums、imagecatalogs、poolers、publications、scheduledbackups、subscriptions）；7 個 ClusterRole（`cnpg-manager` 與 database、publication、subscription 的 editor／viewer）；ClusterRoleBinding `cnpg-manager-rolebinding`；MutatingWebhookConfiguration `cnpg-mutating-webhook-configuration`；ValidatingWebhookConfiguration `cnpg-validating-webhook-configuration` |
| `cnpg-system` | ServiceAccount `cnpg-manager`、Deployment `cnpg-controller-manager`（一個副本，requests 100m／100Mi、limits 100m／200Mi）、Service `cnpg-webhook-service`、ConfigMap `cnpg-default-monitoring`；operator 啟動後自己建立 webhook 用的 CA 與憑證 Secret |

權限：預設的安裝管理所有 namespace 的 Cluster，`cnpg-manager` 這個 ClusterRole 可以讀寫叢集內所有的 Secret、ConfigMap、Service、PVC 與 Pod，也能 exec 進 pod、建立 Role 與 RoleBinding。它是平台元件，權限等同叢集管理者，和 Istio 一樣只由平台維運安裝與升級。

## 升級與移除

- 升級：換 manifest 的版本與雜湊，同樣以 `kubectl apply --server-side` 套用。operator 換版後會更新每個 Postgres pod 的 instance manager，單一 instance 的 Cluster 會短暫中斷，排在維護時段。先讀該版的 release notes，確認 chart 用到的欄位仍然支援。
- 移除：先刪掉所有 namespace 的 Cluster（`kubectl get clusters -A`），確定資料不要了再刪 operator 與 CRD。直接 `kubectl delete -f cnpg-1.30.1.yaml` 會刪掉 CRD，所有 Cluster 與它們的 PVC 跟著被刪。

## 服務的資料庫角色

chart 只建立資料庫、owner 與 AGE extension。每個服務自己的登入角色（決策 4、29）與 migration Job 由 Laysi/trustiot-v2#60 加入，做法建議（2026-10-12 在實驗機以 e2e 驗證過前兩點，見測試報告）：

- 以 CloudNativePG 1.30 的 DatabaseRole 物件宣告每個服務的角色（`login: true`，不給 SUPERUSER、CREATEROLE、BYPASSRLS），密碼放在 `kubernetes.io/basic-auth` 類型的 Secret（`username`、`password`），由 `passwordSecret` 指向它。operator 以 superuser 建立角色，之後改密碼只要改 Secret。
- 服務以 `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` 拿到自己的帳密（身分服務與 `example-service` 已經用這兩個名稱），只給該服務，與 `services.<name>.credentials` 同一套規則，並把這兩個名稱保留給 chart。
- migration Job 用 owner `trustiot_owner`（Secret `<cluster>-app`），先套用 `crates/service-kit/sql/database.sql`（收回 PUBLIC 的 TEMPORARY），再建各服務的 schema 與權限。Job 的 pod 要加進 `<release>-postgres` NetworkPolicy 的來源。
- 也可以不用密碼：DatabaseRole 的 `clientCertificate` 讓 operator 簽發用戶端憑證，搭配 service-kit 的用戶端憑證支援與一條 `hostssl ... cert` 的 pg_hba 規則。需要時在 Laysi/trustiot-v2#60 決定。
- AGE 的權限：extension 由 operator 以 superuser 建立，owner 沒有 `ag_catalog` 的 USAGE、`ag_catalog.ag_graph` 的 INSERT 與 `ag_catalog.ag_label` 的 SELECT（2026-10-12 在實驗機查得三項都是 false），所以 owner 與服務角色都不能自己 `create_graph`。誰建立圖、要由 superuser 給哪些權限，在 Laysi/trustiot-v2#60 與 Laysi/trustiot-v2#176（每租戶的 AGE 角色）決定。
- AGE 的時機：`helm install --wait` 只等 Cluster 健康，Database 的 `age` extension 再晚約 20 秒才建立（實驗機），用到 AGE 的 migration 要先等它出現（`SELECT 1 FROM pg_extension WHERE extname = 'age'`）。
