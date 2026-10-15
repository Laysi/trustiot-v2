# 服務角色不能使用 `postgres` 與 `template1`（2026-10-15）

對應 issue：Laysi/trustiot-v2#400。原始資料在 [data/2026-10-15-400-system-databases/](data/2026-10-15-400-system-databases/)。CloudNativePG 自己對這兩個資料庫的連線、選用資料庫權限而不用 pg_hba 的理由，寫在 [deploy/cloudnative-pg/README.md 的「其他資料庫的權限」](../../deploy/cloudnative-pg/README.md#其他資料庫的權限)；已存在的 Cluster 與 external Postgres 的步驟在 [docs/runbooks/migrations.md](../runbooks/migrations.md#postgres-與-template1收回-public-的-connect-與-temporary)。

## 1. 環境

- **受測程式碼**：分支 `fix/400-system-database-privileges`，從 main `495a54b` 開出。lab 的三次執行都在 commit `bfb0156`；之後只加了本報告與 `data/` 目錄。
- **建置主機**（x86_64 Linux）：經檢查佇列，`CI=1`，所以需要容器的測試沒有容器時會失敗而不是略過。容器測試的 Postgres 是 `postgres:18.6-alpine`（以 digest 鎖定），連線一律 `verify-full`。Rust 1.99.0、bun 1.4.2、helm 與 kubeconform 皆由 `.prototools` 鎖定。
- **lab 的 Mac mini M1**：Fedora Linux Asahi Remix 44（aarch64），k3s v1.36.5+k3s1，單一節點。測試在 namespace `e2e-400`（Pod Security `restricted`），測試的身分是只在這個 namespace 有權限的 ServiceAccount。
- **lab 的叢集層級元件**：使用者在 2026-10-15 03:49–03:50 UTC 安裝，不是測試裝的，測完保留：

  | 元件 | 版本 | manifest 的 SHA-256 |
  | --- | --- | --- |
  | CloudNativePG | 1.30.1 | `37237f145d8138256ea25ae830f87759255665ff08f8d552fdd8224a5ec032fb` |
  | Barman Cloud plugin | v0.15.1 | `8f377ff4a7d0d3f74fc64b0f7ac4f449fc38ca9e83a40107cca2f9315a9bd920` |
  | cert-manager（plugin 需要） | v1.21.2 | `e03b668ec8675214af6b0a671699d088f2601fa3878e0dbe1b41d3feafd1879f` |

  安裝新增 66 個叢集層級物件，沒有移除任何物件。
- **映像**：在建置主機用獨立的 podman store 建 arm64 映像（`deploy/images/postgres` 的 Containerfile；`migrate` 與 `example-service` 以 `cargo zigbuild` 交叉編譯，用 `deploy/images/rust-binary.Containerfile` 包成映像），存成 tar 後匯入 lab 的 k3s containerd，名稱是 `localhost/trustiot-e2e/{postgres,migrate,example-service}:400`。建置時的樹與 `bfb0156` 在 `tools/migrate/src`、`crates`、`services`、`deploy/images` 與 `Cargo.lock` 上相同。測完已從節點移除。
- **物件儲存**：同一個 namespace 裡的 RustFS（`docker.io/rustfs/rustfs:1.0.0-glibc@sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`），選它的理由在 `deploy/chart/tests/e2e/README.md`。存取金鑰是測試行程產生的隨機值，沒有出現在輸出或報告。

## 2. 指令

建置主機，在 repo 根目錄，經檢查佇列：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo test --locked -p migrate --test system_databases
cargo test --locked --workspace
cargo test --locked -p service-kit --no-default-features
bun install --cwd scripts --frozen-lockfile
bun test scripts
bun run --cwd scripts typecheck
bun test deploy/chart
bun scripts/ci/check-secret-handling.ts
```

lab（kubeconfig 指向 lab 的叢集、namespace `e2e-400`，持有 lab 的鎖）：

```sh
E2E_NAMESPACE=e2e-400 \
E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:400 \
E2E_REPORT_DIR=<dir> \
bun test --timeout 2400000 ./deploy/chart/tests/e2e/postgres-replication.e2e.ts

# E2E_SHAPE 各跑一次：onprem 與 saas
E2E_NAMESPACE=e2e-400 E2E_SHAPE=<onprem|saas> \
E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:400 \
E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:400 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:400 \
E2E_IMAGE_PULL_POLICY=Never E2E_REPORT_DIR=<dir> \
bun test --timeout 900000 ./deploy/chart/tests/e2e/postgres.e2e.ts
```

## 3. 結果

### PostgreSQL 18 初始化後的預設（容器測試的第一階段）

`tools/migrate/tests/system_databases.rs` 先在沒動過的 Postgres 18 上讓 migration Job 建立服務角色，確認問題存在：

| 項目 | `postgres` | `template1` |
| --- | --- | --- |
| initdb 後的 `datacl` | NULL（PUBLIC 有 CONNECT 與 TEMPORARY） | `{=c/postgres,postgres=CTc/postgres}`（PUBLIC 只有 CONNECT） |
| 服務角色能連線 | 能 | 能 |
| 服務角色建立暫存表 | 能 | 不能（42501） |
| 服務角色在 `public` 建立物件 | 不能（42501，PostgreSQL 15 起） | 不能（42501） |

所以 `template1` 寫不進物件，但服務角色能連上；在 `template1` 留著一個連線就會讓 `CREATE DATABASE` 失敗（「source database "template1" is being accessed by other users」）。Job 這時對兩個資料庫各留一筆警告，不停止。

### 建置主機的檢查

[workspace-checks.txt](data/2026-10-15-400-system-databases/workspace-checks.txt) 是輸出（主機路徑已遮蔽）。

| 檢查 | 結果 |
| --- | --- |
| `system_databases.rs` 的 `service_roles_cannot_use_the_other_databases`：套用 `deploy/chart/files/postgres/system-databases.sql`（逐句、跑兩次確認冪等）之後，ACL 是 `postgres|{postgres=CTc/postgres,streaming_replica=c/postgres,cnpg_metrics_exporter=c/postgres}` 與 `template1|{postgres=CTc/postgres}`；預先建立的兩個 operator 角色形狀和 CloudNativePG 建的一樣；從 `template1` 建立的新資料庫仍讓服務角色連線；再跑 Job 時沒有警告、沒有變更；每個登入角色與 owner 對兩個資料庫都沒有 CONNECT 與 TEMPORARY，連線被拒（42501），自己的資料庫照常 | 通過 |
| `the_split_matches_the_chart_tests_rule`：Rust 測試與 chart 測試用同一個規則切 SQL 檔 | 通過 |
| `bun test deploy/chart`：兩種型態渲染出的 `postInitSQL` 等於 SQL 檔的語句，沒有 `postInitTemplateSQL`，每句以 `;` 結尾，`GRANT` 只有那一句；initdb 的三個 SQL 清單都沒有 `$$` 或 `$(` | 通過 |
| fmt、兩種 clippy、`cargo test --workspace`、不含 `contract` 的 service-kit 測試、`bun test scripts`、typecheck、`check-secret-handling` | 通過 |

### lab：複寫、switchover、備份與還原（`postgres-replication.e2e.ts`）

[postgres-replication-e2e.json](data/2026-10-15-400-system-databases/postgres-replication-e2e.json) 是完整紀錄，[lab-runs.txt](data/2026-10-15-400-system-databases/lab-runs.txt) 是測試輸出：6 個測試通過，0 個失敗，164 秒。

測試以地端型態渲染 chart 的 ImageCatalog、Cluster 與 NetworkPolicy，改過自己那份 Cluster 再套用：`instances: 2`、Barman Cloud plugin 當 WAL archiver、較小的資源與 1Gi 磁碟區（報告的 `clusterChanges`）。bootstrap、`postInitSQL`、pg_hba、image catalog 與不開 superuser 都是 chart 的。

| 步驟 | 結果 |
| --- | --- |
| 新 bootstrap 的 Cluster | ACL 同容器測試；superuser 與 `pg_` 角色以外，只有 `streaming_replica` 與 `cnpg_metrics_exporter` 有 `postgres` 的 CONNECT，`trustiot_owner` 與以 owner 建立的 `e2e_app` 兩個資料庫都沒有 CONNECT 與 TEMPORARY。operator 接手預先建立的角色：`streaming_replica login=t replication=t`、`cnpg_metrics_exporter login=t pg_monitor=t` |
| 複寫 | primary 的 `pg_stat_replication` 有 `streaming_replica trustiot-postgres-2 streaming`，replica 的 WAL receiver 是 `streaming`，資料一致 |
| switchover（`trustiot-postgres-1` → `trustiot-postgres-2`） | 27 秒完成；前 primary 以 replica 身分串流。它的日誌：`Starting up pg_rewind`、`pg_rewind: connected to server`、`servers diverged at WAL location 0/50000A0 on timeline 1`、`no rewind required`。`pg_rewind` 以 `streaming_replica` 連新 primary 的 `postgres` 成功 |
| 備份 | `ContinuousArchiving` 是 `True`（`Continuous archiving is working`）；`method: plugin` 的 Backup `completed`，9 秒 |
| 還原 | `trustiot-postgres-restored` 經 plugin 從這份備份與之後的 WAL bootstrap，55 秒；資料五列都在（含備份之後寫入的一列）；ACL 與角色權限和原 Cluster 相同；metrics `cnpg_collector_up` 1、`cnpg_collector_last_collection_error` 0。檢查完即刪除 |
| instance manager | 兩個 instance 最後都 Ready；metrics `cnpg_collector_up` 1、`cnpg_collector_last_collection_error` 0，收集的資料庫是 `postgres`、`template1`、`trustiot`；日誌沒有 `permission denied for database`、`CONNECT privilege` 或 `permission denied to create temporary`。前 primary 的 postgres 容器重啟一次（switchover 的正常行為） |

### lab：chart 部署的 bundled Postgres 與 migration Job（`postgres.e2e.ts`）

[postgres-e2e-onprem.json](data/2026-10-15-400-system-databases/postgres-e2e-onprem.json)、[postgres-e2e-saas.json](data/2026-10-15-400-system-databases/postgres-e2e-saas.json)：兩種型態各 13 個測試通過，0 個失敗。新增的測試「only the superuser and CloudNativePG's own roles may use the postgres and template1 databases」：

- ACL 同上；Job 建立的 6 個角色（`identity_*`、`assets_*`）、`trustiot_owner` 與 operator 的兩個角色，共 9 個角色、18 列權限，只有 `cnpg_metrics_exporter postgres` 與 `streaming_replica postgres` 有 CONNECT，沒有任何 TEMPORARY；
- `identity_app` 用 Job 產生的密碼（經 stdin 傳入）登入：`postgres` 與 `template1` 都是 `User does not have CONNECT privilege.`，`trustiot` 照常；
- migration Job 的日誌沒有「may connect to it」的警告。

### lab 上發現並修正的問題

1. **`$$` 被 Kubernetes 改掉**：第一次在 lab bootstrap 時 initdb Job 失敗，`syntax error at or near "$"`，收到的語句是 `DO $` 開頭。CloudNativePG 把 `postInitSQL` 當成 initdb Job 的容器參數（`--post-init-sql`），Kubernetes 會把容器參數裡的 `$$` 換成 `$`、展開 `$(NAME)`。容器測試直接執行 SQL，抓不到這件事。修正：DO 區塊改用 `$do$`，chart 測試拒絕 initdb 任何 SQL 清單出現 `$$` 或 `$(`，SQL 檔與 template 的註解寫明原因（commit `2e76008`）。沒有這個修正，每個新建的 bundled Cluster 都會 bootstrap 失敗，nightly 的 smoke 也會。
2. **plugin 的 sidecar 拉不到**：Cluster 的 `imagePullPolicy` 也套用在 CloudNativePG 加進 instance 的容器，`Never` 讓 plugin 的 sidecar 起不來。複寫測試改成固定 `IfNotPresent`（commit `fc36fdc`）。
3. **新 replica 連不到 primary**：複製新 replica 的 join Job 的 pod 沒有 `cnpg.io/podRole=instance`，被測試自己加的 NetworkPolicy 擋住。測試的 NetworkPolicy 改成放行這個 Cluster 的每個 pod；metrics 改在 pod 內讀，因為 chart 的 NetworkPolicy 不放行 9187（commit `bfb0156`）。chart 本身維持一個 instance，不受影響；`instances` 大於 1 時 chart 的 NetworkPolicy 也要放行 instance 之間與 join Job，留給備份與高可用的工作（Laysi/trustiot-v2#134）。

## 4. 結論

| 驗收條件 | 結果 |
| --- | --- |
| bundled 新建的 Cluster 上，服務角色對 `postgres` 與 `template1` 沒有 CONNECT 或 TEMPORARY，並有自動化測試證明 | 通過：chart 測試（渲染出的語句等於 SQL 檔）、容器測試（套用後的權限與拒絕）、lab 的 `postgres.e2e.ts`（兩種型態，chart 部署、Job 建立角色之後） |
| CloudNativePG 的複寫、備份與還原與 instance manager 不受影響：在 lab 實測 | 通過：`postgres-replication.e2e.ts` 在 lab 的兩個 instance 上驗證複寫、switchover 與 `pg_rewind`、WAL 封存、plugin 備份、plugin 還原與 metrics |
| runbook 有已存在 Cluster 的補做步驟，以及 external Postgres 的收回步驟 | 通過：`docs/runbooks/migrations.md` 的新節，用同一個 SQL 檔；runbook 的步驟本身沒有在 lab 已存在的 Cluster 上跑過，同樣的 SQL 由容器測試對已有服務角色的資料庫套用兩次驗證 |
| 移除 `tools/migrate/README.md` 已知限制中的這一條，或改寫成剩下的部分 | 通過：改寫成剩下的部分：其他資料庫的權限不歸 Job 管，Job 只警告；本 issue 之前 bootstrap 的 Cluster 與 external Postgres 要照 runbook 收回一次 |
