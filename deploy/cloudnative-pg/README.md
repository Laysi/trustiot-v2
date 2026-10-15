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

chart 只透過 operator 建立資料庫、owner 與 AGE extension。每個服務自己的角色（決策 4、29）由 migration Job（[tools/migrate](../../tools/migrate/README.md)，Laysi/trustiot-v2#60）建立，做法與理由：

- **不用 DatabaseRole 物件**：2026-10-12 的建議是以 CloudNativePG 的 DatabaseRole 宣告角色，但決策 29 的角色不只一個登入角色：每個服務還有擁有 schema 與表的 `<服務>_owner`、需要時有 `<服務>_platform`、之後還有每租戶的角色，它們的屬性、成員關係與權限要和 migration 一起冪等地建立與檢查，而且外部 Postgres（`postgres.mode: external`）沒有 operator。所以角色統一由 Job 以 SQL 建立，bundled 與 external 走同一條路。
- **migration 角色是 owner**：Job 以 `trustiot_owner`（Secret `<cluster>-app`）登入。chart 在 Cluster 的 `bootstrap.initdb.postInitApplicationSQL` 給它 CREATEROLE（operator 在 bootstrap 時以 superuser 執行一次）。PostgreSQL 16 起 CREATEROLE 不能建立 superuser、BYPASSRLS 或 REPLICATION 角色，也只能管理自己建立（有 ADMIN）的角色，所以 owner 碰不到 `postgres` 與 operator 的角色。2026-10-13 之前 bootstrap 的 Cluster 要補一次，步驟在 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md)。
- **服務的帳密**：服務以 `TRUSTIOT__DATABASE__USERNAME`（`<服務>_app`）、`TRUSTIOT__DATABASE__PASSWORD`（Secret `<release>-<服務>-password` 的同名 key）登入，只給該服務，這兩個名稱保留給 chart。密碼由 migration Job 產生並寫進這個 Secret，沒有人經手（docs/architecture/secrets.md 的「migration Job 產生的密碼」）；Job 只送 SCRAM-SHA-256 的 verifier，明文密碼不進 SQL 與 log。
- **Job 的網路**：Job 的 pod 在 `<release>-postgres` NetworkPolicy 的來源裡，與在 `mesh.calls` 列出 `postgres` 的服務相同。
- **用戶端憑證**：DatabaseRole 的 `clientCertificate` 不用；改用憑證時，要在 Job 與 service-kit 兩邊一起做，另開 issue。
- **AGE 的權限**：extension 由 operator 以 superuser 建立，owner 沒有 `ag_catalog` 的 USAGE、`ag_catalog.ag_graph` 的 INSERT 與 `ag_catalog.ag_label` 的 SELECT（2026-10-12 在實驗機查得三項都是 false），所以 owner、Job 與服務角色都不能自己 `create_graph`。目前沒有服務用 AGE；誰建立圖、要由 superuser 給哪些權限，在 Laysi/trustiot-v2#176（每租戶的 AGE 角色）決定，它會接在 tools/migrate 的每租戶角色 hook 上。
- **AGE 的時機**：`helm install --wait` 只等 Cluster 健康，Database 的 `age` extension 再晚約 20 秒才建立（實驗機），用到 AGE 的 migration 要先等它出現（`SELECT 1 FROM pg_extension WHERE extname = 'age'`）。

## 其他資料庫的權限

chart 的 Cluster 在 bootstrap 時收回 `PUBLIC` 在 `postgres` 與 `template1` 的 `CONNECT` 與 `TEMPORARY`（`bootstrap.initdb.postInitSQL`，Laysi/trustiot-v2#400，做法見 [chart 的「叢集內的 Postgres」](../chart/README.md#叢集內的-postgres)）。superuser 不受資料庫權限限制，其他角色要連這兩個資料庫就得依名稱拿到 `CONNECT`。2026-10-15 依 CloudNativePG 1.30.1 與 Barman Cloud plugin v0.15.1 的原始碼逐一查過 operator 自己的連線（路徑相對於各自 repo 的根目錄；lab 的實測見測試報告）：

| 連線 | 角色與方式 | 資料庫 | 需要 | 來源 |
| --- | --- | --- | --- | --- |
| instance manager：狀態與探針、bootstrap 的 `postInitSQL`／`postInitTemplateSQL`／`postInitApplicationSQL`、`streaming_replica` 與 `cnpg_metrics_exporter` 的建立、extension 的調和 | `postgres`（superuser），pod 內 local socket 的 peer 驗證 | `postgres`、`template1`（只有 `postInitTemplateSQL`）、各資料庫 | 不需要（superuser） | `pkg/management/postgres/instance.go` 的 `ConnectionPool`、`GetSuperUserDB`、`GetTemplateDB`；`pkg/management/postgres/initdb.go` 的 `ConfigureNewInstance`；`internal/cmd/manager/instance/run/lifecycle/run.go` 的 `configureInstancePermissions` |
| Database、DatabaseRole 與 Cluster 的 `managed.roles` 的調和 | `postgres`（superuser），local socket | `postgres`、目標資料庫 | 不需要 | `internal/management/controller/database_controller.go`、`databaserole_controller.go`、`roles/runnable.go` |
| 實體複寫：replica 的 walreceiver、加入時的 `pg_basebackup` | `streaming_replica`，TLS 用戶端憑證（pg_hba `hostssl replication streaming_replica all cert`） | 不連資料庫（`replication`） | 不需要：實體複寫連線不檢查資料庫權限 | `pkg/management/postgres/conninfo.go`、`pkg/postgres/configuration.go` |
| replica 對 primary 的一般連線：`pg_rewind --source-server`（前 primary 重新加入）、等待新 primary 可連線、replica 檢查 WAL 歸檔、replica 的 replication slot 同步 | `streaming_replica`，TLS 用戶端憑證（pg_hba `hostssl postgres streaming_replica all cert`），`dbname=postgres` | `postgres` | `CONNECT`：依名稱給（`pg_rewind` 要的函式權限由 operator 自己給） | `pkg/management/postgres/instance.go` 的 `GetPrimaryConnInfo`、`Rewind`、`WaitForPrimaryAvailable`；`pkg/management/postgres/wal.go`；`internal/management/controller/slots/runner/runner.go` |
| 監控查詢（`cnpg-default-monitoring` 與自訂查詢） | `cnpg_metrics_exporter`（pg_monitor 的成員，不是 superuser），local socket 的 peer 驗證 | 預設 `postgres`；`target_databases: '*'` 展開成 `datallowconn AND NOT datistemplate` 的資料庫，不含 `template1` | `postgres` 的 `CONNECT`：依名稱給；`trustiot` 維持 `PUBLIC` 的 `CONNECT` | `pkg/management/postgres/instance.go` 的 `metricsConnectionPool`、`pkg/management/postgres/metrics/collector.go`；文件 `docs/src/monitoring.md` 的「Every database in `target_databases` must allow `cnpg_metrics_exporter` to `CONNECT`」 |
| 備份（`Backup` 的 `method: plugin`） | plugin 的 sidecar 執行 `barman-cloud-backup --user postgres`，經 `PGHOST` 的 local socket，superuser | `postgres` | 不需要 | barman-cloud v0.6.0 的 `pkg/backup/backup.go`；plugin 的 `internal/cnpgi/instance/backup.go` |
| WAL 歸檔與還原（`restore_command`、從備份 bootstrap） | plugin 的 sidecar 或還原 Job，只讀寫物件儲存與 WAL 檔 | 不連資料庫 | 不需要 | plugin 的 `internal/cnpgi/instance/`、`internal/cmd/restore/` |
| Pooler（chart 沒有用） | `cnpg_pooler_pgbouncer`，TLS 用戶端憑證；`auth_query` 在 `postgres` 執行 | `postgres` | `CONNECT`：operator 第一次為 Pooler 建立這個角色時自己 `GRANT CONNECT ON DATABASE postgres` 給它，依名稱，所以收回 `PUBLIC` 不影響；用戶端經 Pooler 連的是 `trustiot` | `internal/management/controller/instance_controller.go` 的 `reconcilePgbouncerAuthUser`、`api/v1/pooler_types.go` 的 `PoolerAuthDBName` |

operator 在 instance 第一次啟動時才建立 `streaming_replica` 與 `cnpg_metrics_exporter`，比 `postInitSQL` 晚。所以 chart 的檔案先以 operator 的方式建立它們（`CREATE ROLE streaming_replica LOGIN REPLICATION` 與 operator 的註解、`CREATE ROLE cnpg_metrics_exporter LOGIN PASSWORD NULL`），再給 `CONNECT`；operator 遇到已存在的角色只補屬性與權限（`pg_monitor` 的成員關係、`pg_rewind` 的函式權限）。升級 operator 時，release notes 若提到新的、會連 `postgres` 的角色，要把它加進 `deploy/chart/files/postgres/system-databases.sql`，並在 lab 重跑 `deploy/chart/tests/e2e/postgres-replication.e2e.ts`。

為什麼收回資料庫權限，而不是加一條拒絕這兩個資料庫的 pg_hba 規則：

- 權限記在 catalog 裡，隨資料走：replica、從備份還原的 Cluster 都帶著它。pg_hba 是 Cluster spec 的設定，還原或搬到沒有這條規則的 Cluster 就沒了。
- pg_hba 只決定能不能連線，表達不了 `TEMPORARY`。權限則是 `has_database_privilege` 查得到的狀態，Job 的警告、chart 的測試與 lab 的測試都以它檢查。
- 外部 Postgres 沒有 chart 管的 pg_hba，只能收回權限；bundled 與 external 用同一種做法，runbook 的步驟也一樣。
- pg_hba 的規則依資料庫與角色名稱比對，要排在 CloudNativePG 的固定規則之後才不會擋到 `streaming_replica` 與 Pooler；擋住 TCP 的一般角色也擋到之後任何依名稱需要 `postgres` 的角色，而權限可以依名稱給。

代價是 `postInitSQL` 只在 bootstrap 執行一次，之前建立的 Cluster 要照 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md) 補一次；operator 的兩個角色也要先建好，才能依名稱給權限（上一段）。
