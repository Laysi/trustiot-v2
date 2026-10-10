# ClickHouse 窄表、批次完成表與 row policy 租戶隔離（2026-10-10）

對應 issue：Laysi/trustiot-v2#59。

## 環境

- 本機工作站：Fedora（Linux 7.2，x86_64），rootless podman 6.1.3；Rust 1.99.0（`rust-toolchain.toml`）、Bun 1.4.2、Helm 4.3.0、kubeconform 0.8.0（`.prototools`）。
- ClickHouse：`docker.io/clickhouse/clickhouse-server@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c`（26.8 LTS 的 multi-arch index，伺服器回報 26.8.22.13），與 `scripts/lib/stack.ts`、`deploy/chart/values.yaml` 同一個 digest。容器記憶體上限 1500 MB，測完即移除。
- 分支 `feat/59-clickhouse-schema`。
- 實驗機（Mac mini M1）的鎖在 2026-10-10 13:38Z 起由 Laysi/trustiot-v2#62 的驗證持有，這次沒有在 k3s 上部署；以 Job 套用 DDL 的驗證在 Laysi/trustiot-v2#60。

## 指令

```sh
# migration 檔的靜態檢查，以及對真的 ClickHouse 套用 DDL、跑租戶隔離測試（以 podman 起容器）
CARGO_BUILD_JOBS=2 cargo test -p ingest -- --nocapture

# 執行期程式碼不改 schema（services/*/src 的靜態檢查）
CARGO_BUILD_JOBS=2 cargo test -p repo-check

# chart：helm lint、兩份 values 的 kubeconform（Kubernetes 1.30.0 與 1.36.0）、chart 規則
bun test deploy/chart
```

另外以 chart 產生的 `config.d`（`helm template` 輸出的 ConfigMap 內容）在 podman 以與 pod 相同的限制啟動 ClickHouse：`--read-only`、`--user 101:101`、`--cap-drop ALL`、`--security-opt no-new-privileges`，`users.d` 與 `/tmp` 為 tmpfs，admin 密碼以檔案掛入（`CLICKHOUSE_PASSWORD_FILE`）。

## 結果

### 表設定（從伺服器讀回）

| 表 | engine_full（節錄） |
| --- | --- |
| `trustiot.telemetry` | `MergeTree PARTITION BY toYYYYMM(received_at) ORDER BY (tenant_id, device_id, point, ts) TTL toDateTime(received_at) + toIntervalDay(400) SETTINGS fsync_after_insert = 1, fsync_part_directory = 1, non_replicated_deduplication_window = 1000` |
| `trustiot.batch_commits` | `ReplacingMergeTree PARTITION BY toYYYYMM(committed_at) ORDER BY (stream_id, seq) TTL toDateTime(committed_at) + toIntervalDay(400) SETTINGS fsync_after_insert = 1, fsync_part_directory = 1` |

伺服器層級的 `fsync_after_insert` 與 `fsync_part_directory` 預設都是 0，兩張表都在表設定開啟。migration 連續套用兩次都成功。

### access entities 的儲存位置

2 個角色、5 條 row policy、1 個 settings profile、1 個 quota，`storage` 全部是 `local_directory`（以 SQL 建立，不在設定檔）。

### 租戶 A 讀租戶 B（49 項攻擊）

以租戶 A 的帳號執行，每一項都得到預期的答案或被拒絕；所有回應（含錯誤訊息）都不含 B 的租戶 ID 與 B 的資料。A 有 3 列、B 有 4 列，兩個租戶共用同一個設備與資料點，所以 JOIN 在 policy 失效時一定會碰到 B。

| 類型 | 項目 | 結果 |
| --- | --- | --- |
| 直接查 | 全表筆數、讀全部值、`WHERE tenant_id = B`、`B OR 1 = 1`、`PREWHERE`、`GROUP BY tenant_id`、虛擬欄位、throwIf 錯誤 oracle | 只看到 A 的 3 列；B 條件得到 0；throwIf 從未觸發 |
| JOIN | 以共用設備自我 JOIN 到 B、CROSS JOIN、LEFT JOIN 到 B | 0、9（3×3）、0 |
| 子查詢 | IN、純量子查詢、FROM 子查詢、CTE | 0 或 3 |
| UNION | UNION ALL 加 B 條件、UNION DISTINCT 列出租戶 | 3；只有 A |
| FINAL | `FROM telemetry FINAL` | 拒絕（`ILLEGAL_FINAL`，MergeTree 沒有 FINAL） |
| 表函式 | `merge()`（單表與整個資料庫）、`view()`（含 B 條件） | 3、3、3、0 |
| 表函式 | `mergeTreeIndex()`、`remote()`、`cluster()`、`loop()`、`url()`、`file()` | 全部拒絕（`ACCESS_DENIED`／`DATABASE_ACCESS_DENIED`；`mergeTreeIndex` 明確說明有 row policy 不能讀索引） |
| 系統表 | `query_log`、`processes`、`parts`、`part_log`、`text_log`、`users`、`row_policies`、`settings_profile_elements`、`mutations` | 全部拒絕（先 `SYSTEM FLUSH LOGS` 讓 log 表存在，拒絕原因是權限而不是表不存在） |
| 改設定 | 查詢的 `SETTINGS SQL_tenant_id = B`、URL 參數 `SQL_tenant_id=B`、`additional_table_filters`、`max_threads = 64` | 全部拒絕（`READONLY`） |
| 冒用與寫入 | `EXECUTE AS` ingest 帳號、讀 `batch_commits`、寫入 B 的列、`DELETE FROM`、`ALTER TABLE ... DELETE`、`CREATE VIEW`、`CREATE ROW POLICY`、`GRANT` | 全部拒絕（`ACCESS_DENIED`） |

完整清單與每項輸出：[cargo-test-ingest.log](data/2026-10-10-59-clickhouse-schema/cargo-test-ingest.log)。

其他：

- 在 session 裡 `SET ROLE NONE` 之後讀表被拒（`ACCESS_DENIED`），`SET max_threads` 仍被拒（`READONLY`）。
- 租戶 B 讀到自己的 4 列；ingest 帳號（`USING 1` policy）讀到 A、B 共 7 列；只有 `SELECT` 權限、沒有 policy 點名的角色讀到 0 列（`deny_by_default`，伺服器預設 `users_without_row_policies_can_read_rows = true`）。
- 沒有 `SQL_tenant_id` 的租戶帳號：直接查、`SELECT *`、`merge()` 三種查詢都回 `UNKNOWN_SETTING`。
- ingest 帳號：寫入、`DELETE FROM`（`lightweight_deletes_sync = 2`）刪掉一批、寫完成紀錄並讀回 `max(seq) = 7`；建表、加欄位、改 TTL、改表設定、刪表、清表、建帳號、改 row policy 一律 `ACCESS_DENIED`；全零租戶與放錯欄位的值被 constraint 擋下。
- quota：第一個租戶帳號在第 121 個查詢被擋下（`QUOTA_EXCEEDED`，每分鐘 120），另一個租戶帳號同時照常查詢。
- 測試會失敗的證明（只在本機改檔驗證，未提交）：把 `tenant_isolation` 改成 `... OR value_string LIKE 'B-SECRET%'` 時，租戶 A 讀到 7 列，測試失敗；把 `deny_by_default` 改成只給租戶角色時，沒有 policy 點名的角色讀到 7 列，測試失敗。

### 執行期不改 schema

`cargo test -p repo-check` 的 `runtime_code_never_changes_the_schema` 掃描 `services/*/src`，目前沒有任何建表、改表、刪表的語句；單元測試證明單行與跨行的 `CREATE TABLE`、`ALTER TABLE`、`DROP DATABASE`、`CREATE MATERIALIZED VIEW` 等會被找到，`INSERT`、`DELETE FROM`、`CREATE USER`、`GRANT`、註解與 `create_table` 這類識別字不會。

### chart

`bun test deploy/chart`：106 項全數通過（[bun-test-chart.log](data/2026-10-10-59-clickhouse-schema/bun-test-chart.log)），包含兩份 values 在 Kubernetes 1.30.0 與 1.36.0 的 kubeconform（含 StatefulSet）。新增的規則：ClickHouse 映像與 dev 環境、schema 測試同一個 digest；單副本、PVC 保留、只有 ClusterIP 8123／9000；restricted Pod Security；admin 密碼只從掛載的 Secret 讀、`users.d` 在記憶體、輪替密碼時改 `adminSecretRevision` 會重啟 ClickHouse；伺服器設定沒有帳號、角色、profile、quota、row policy 或密碼；`external` 模式不產生 ClickHouse；values 驗證擋下缺 admin Secret、沒有設定磁碟區大小、沒有 digest、admin 叫 `default`、允許 root、在 values 放帳號設定；服務的自訂 `env` 讀不到 admin 密碼。

以 chart 的 `config.d` 在 podman 以 pod 的限制啟動（[hardened-container.log](data/2026-10-10-59-clickhouse-schema/hardened-container.log)）：entrypoint 以 `trustiot_admin` 取代 `default`，只監聽 HTTP 8123 與 native 9000（MySQL、PostgreSQL 相容埠與 9009 已關），唯讀根目錄下正常啟動。資料目錄擁有者是 root 時（k3s local-path 的情況）只有一行警告，仍正常啟動。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| DDL 以 SQL 檔放在 services/ingest，執行期程式碼沒有建表或改表的路徑（靜態檢查；由 Job 套用在 Laysi/trustiot-v2#60 驗證） | 通過：`services/ingest/migrations/clickhouse/*.sql`；`tools/repo-check` 靜態檢查 `services/*/src`；資料庫端 ingest 帳號沒有改 schema 的權限。Job 套用待 Laysi/trustiot-v2#60 |
| 以 2026-10-09 PoC 的攻擊類型測租戶 A 帳號讀 B：全部 0 筆（自動測試） | 通過：49 項攻擊，B 的資料 0 筆 |
| 沒有租戶設定的帳號查詢時報錯（測試） | 通過：`UNKNOWN_SETTING` |
| fsync_after_insert 已開啟（測試讀取表設定） | 通過：兩張表 `fsync_after_insert = 1`、`fsync_part_directory = 1` |

已知限制（不違反驗收條件，記錄供後續參考）：有 `SELECT` 權限的租戶帳號從 `system.tables.total_rows` 與 `EXPLAIN ESTIMATE` 看得到整張表（所有租戶）的總筆數，看不到任何一列；ClickHouse 26.8 的 quota 不計算沒有讀表的常數查詢（例如 `SELECT 1`）。

待測：在 k3s（實驗機）上以 chart 部署 ClickHouse 並以 Job 套用 DDL，併入 Laysi/trustiot-v2#60。
