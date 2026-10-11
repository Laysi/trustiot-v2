# Laysi/trustiot-v2#260 ClickHouse 內省管道：盤點、修正與回歸測試

對應 issue：`Laysi/trustiot-v2#260`（來源：`docs/test-reports/2026-10-11-59-verify.md` 的發現 1、6、7）。

缺口：ClickHouse 26.8 有兩個分析 MergeTree 索引的 table function，只要表的 `SELECT` 權限就能呼叫，卻不套用 row policy，回應會隨其他租戶的鍵值是否存在而不同。修正：租戶帳號不再拿任何表的權限，只能讀 DEFINER view `trustiot.tenant_telemetry`，租戶過濾寫成 view 的 DEFINER 帳號在表上的 row policy（`services/ingest/migrations/clickhouse/0005_tenant_view.sql`；設計比較見 `services/ingest/README.md`「租戶讀取路徑」與決策 14 的 2026-10-11 補充）。本報告記錄盤點、設計實測與回歸測試；每個函式只記一個查詢的結果，不記錄逐步推算的方法。

## 環境

| 項目 | 內容 |
| --- | --- |
| 主機 | 本機（Fedora 45，x86_64，podman rootless），不使用 lab 與 Kubernetes；測試期間主機 load average 約 23–28（8 核，其他 agent 同時在跑） |
| ClickHouse | `docker.io/clickhouse/clickhouse-server@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c`（測試與 chart 相同的 digest），版本 26.8.22.13 |
| 容器 | 記憶體上限 1500 MB，`CLICKHOUSE_USER=trustiot_admin`、`CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1`；盤點與設計實測用一個手動啟動的容器，`cargo test` 每次起自己的容器並在結束時移除 |
| 分支 | `fix/260-clickhouse-introspection`，從 `origin/main`（`404817d`）建立 |
| 建置 | `CARGO_BUILD_JOBS=2`，target 在 worktree 內 |
| 盤點資料 | 3 個租戶（A、B、C）各 30 萬列：30 個設備 × 10 個資料點 × 1,000 筆，`OPTIMIZE FINAL` 後 1 個 part、110 個 granule；租戶帳號依 README 的兩句範本建立 |

## 指令

```sh
# 回歸測試（修正後）：靜態檢查加真的 ClickHouse
CI=1 CARGO_BUILD_JOBS=2 cargo test -p ingest -- --nocapture

# 修正前：同一份測試，配 main（404817d）的 migration
git show origin/main:services/ingest/migrations/clickhouse/0004_access.sql > services/ingest/migrations/clickhouse/0004_access.sql
rm services/ingest/migrations/clickhouse/0005_tenant_view.sql
CI=1 CARGO_BUILD_JOBS=2 cargo test -p ingest --test clickhouse -- --nocapture
# 之後還原兩個檔案（以 sha256 確認與分支內容相同）

# 其餘
cargo fmt --all --check
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test -p repo-check
```

盤點、設計比較與效能比較以手動容器的 HTTP 介面逐條執行（每個查詢記錄 HTTP 狀態、回應與 `X-ClickHouse-Summary`），輸出在 `data/2026-10-11-260-introspection/`。

## 結果

### 1. table function 盤點（ClickHouse 26.8.22.13，共 94 個）

`system.table_functions` 的 94 個函式分三類，分類寫在 `tests/clickhouse.rs` 的 `TABLE_FUNCTIONS`；伺服器多出沒分類的函式時測試失敗。

- **不指名表（15 個）**：`SQLStandardValues`、`format`、`fuzzJSON`、`fuzzQuery`（只回傳改寫過的查詢文字，不執行）、`generateRandom`、`generateSeries`、`generate_series`、`input`（只能用在 INSERT）、`null`、`numbers`、`numbers_mt`、`primes`、`values`、`zeros`、`zeros_mt`。
- **外部來源（55 個）**：物件儲存、檔案、URL、其他資料庫與程式，例如 `s3*`、`azureBlobStorage*`、`gcs`、`hdfs*`、`iceberg*`（10 個）、`deltaLake*`（7 個）、`paimon*`（9 個）、`hudi*`、`file*`、`filesystem`、`url*`、`mysql`、`postgresql`、`mongodb`、`redis`、`sqlite`、`odbc`、`jdbc`、`hive`、`bigquery`、`arrowFlight`、`ytsaurus`、`executable`、`cosn`、`oss`。全部 `allow_readonly = 0`，租戶帳號也沒有 `CREATE TEMPORARY TABLE` 與任何來源權限；測試檢查這個旗標沒有改變。
- **指名表（24 個）**：下表。「修正前」是租戶角色有 `telemetry` 的 `SELECT` 加 row policy（main 的設計），「修正後」是本分支。

| 函式 | 唯讀帳號可用 | 修正前，對 `telemetry` | 修正後，對兩張表 | 修正後，對 view |
| --- | --- | --- | --- | --- |
| `mergeTreeAnalyzeIndexes` | 是 | **不套用 row policy**：以其他租戶的鍵值為條件時回應與不存在的鍵值不同（發現 1） | `ACCESS_DENIED` | 無法執行（不是 MergeTree 表） |
| `mergeTreeAnalyzeIndexesUUID` | 是 | **不套用 row policy**：同一個缺口的第二個入口，以表的 UUID 指定表；驗證報告當時以錯誤的參數型別呼叫，沒有觸發 | `ACCESS_DENIED` | 無法執行 |
| `mergeTreeCodecBlockCounts` | 是 | 有 row policy 時拒絕讀計數欄，但仍列出 part 名稱、欄位與 substream（整張表的中繼資訊，與被拒的 `system.parts` 同類） | `ACCESS_DENIED` | 無法執行 |
| `mergeTreeIndex` | 是 | 有 row policy 時拒絕 | `ACCESS_DENIED` | 無法執行 |
| `mergeTreeTextIndex` | 是 | 有 row policy 時拒絕（以帶 text index 的暫存表驗證）；`telemetry` 沒有 text index | `ACCESS_DENIED` | 找不到索引而報錯 |
| `mergeTreeProjection` | 是 | 套用 row policy（以帶 projection 的暫存表驗證）；`telemetry` 沒有 projection | 報 projection 不存在：名稱檢查在權限檢查之前，只透露 schema；有 projection 時沒有權限就被拒（暫存表驗證） | 同左 |
| `merge` | 是 | 套用 row policy | 沒有可讀的表而報錯 | 套用（只有自己的列） |
| `view` | 是 | 套用 | `ACCESS_DENIED` | 套用 |
| `viewIfPermitted` | 是 | 套用 | 走 `ELSE`，0 列 | 套用 |
| `cluster`、`clusterAllReplicas` | 是 | 被拒（`READ ON REMOTE`） | 被拒 | 被拒 |
| `eval` | 是 | 被拒（實驗功能，唯讀帳號不能開啟） | 被拒 | 被拒 |
| `remote`、`remoteSecure` | 否 | 被拒（`READ ON REMOTE`） | 被拒 | 被拒 |
| `loop`、`viewExplain`（`SELECT * FROM (EXPLAIN …)`）、`dictionary` | 否 | 被拒（需要 `CREATE TEMPORARY TABLE`） | 被拒 | 被拒 |
| `timeSeriesData`、`timeSeriesMetrics`、`timeSeriesSamples`、`timeSeriesSelector`、`timeSeriesTags`、`prometheusQuery`、`prometheusQueryRange` | 否 | 不是 TimeSeries 表而報錯 | 同左 | 同左 |

原始輸出：`inventory-before.txt`（修正前）、`inventory-after-tables.txt`、`inventory-after-view.txt`、`projection-text-index.txt`。

### 2. 其他會描述表資料的語句與系統表

| 項目 | 修正前 | 修正後 |
| --- | --- | --- |
| `system.tables` 的 `total_rows`、`total_bytes`、`parts`、`total_marks` | 整張表（所有租戶）的筆數與大小（驗證報告發現 2） | 兩張表不出現在 `system.tables`；view 的這些欄位是 `NULL` |
| `system.columns` 的各欄壓縮大小 | 整張表的大小 | 兩張表不出現；view 是 0 |
| `EXPLAIN ESTIMATE` | 以其他租戶為條件時 0 | 表：被拒。view：以其他租戶為條件時 0；不加條件時只算到自己的 granule（90 萬列中 303,104 列） |
| `EXPLAIN indexes` | 主鍵段落含 row policy 條件；Min-Max 與 Partition 段落顯示整張表的 part 與 granule 總數 | 表：被拒。view：同修正前，仍看得到整張表的 part 與 granule 總數（已知限制） |
| 讀取統計（`X-ClickHouse-Summary`） | 以其他租戶為條件時 0 | 同左；以 granule 為單位，邊界 granule 會含其他租戶的列數（已知限制，對其他租戶的條件不影響數字） |
| `hasColumnInTable()` | 回 1（schema） | 表：被拒（`SHOW COLUMNS`）；view：回 1 |

系統表：伺服器有 141 張 system 表與 40 張 information_schema 表（`SYSTEM FLUSH LOGS` 之後 system 表會多出 log 表）。修正前後，租戶帳號都只讀得到 24 張：`aggregate_function_combinators`、`collations`、`columns`、`completions`、`contributors`、`current_roles`、`data_type_families`、`database_engines`、`databases`、`dictionary_layouts`、`disk_types`、`enabled_roles`、`formats`、`functions`、`licenses`、`one`、`privileges`、`quota_usage`、`settings`、`table_engines`、`table_functions`、`tables`、`time_zones`、`user_query_log`；information_schema 一張都讀不到。這 24 張都不套用 row policy，但內容只有伺服器內建的參考資料，或帳號自己的設定、角色、quota、查詢與看得到的物件。不套用 row policy 又描述表資料的只有 `tables` 與 `columns` 的統計欄：修正後兩張表不再出現在其中，`completions` 也只列出 view。

### 3. 設計實測（`view-properties.txt`、`rejected-view-policy-only.txt`、`definer-limits-quota.txt`、`view-settings.txt`）

| 檢查 | 採用的設計（過濾在 DEFINER 帳號的表 policy） | 只靠 view 上的 policy（DEFINER 讀全部列） |
| --- | --- | --- |
| 以租戶 A 讀 view | 只有 A 的 30 萬列 | 同左 |
| 內省函式（表與 view） | 全部被拒或無法執行 | 同左 |
| WHERE 裡 `throwIf()` 判斷其他租戶的值 | 不觸發，回 A 的筆數 | **觸發**：view 的條件與使用者的 WHERE 合併在同一個 PREWHERE，碰到邊界 granule 裡其他租戶的列 |
| 轉型錯誤訊息引用的值 | 只出現 A 的值 | 只出現 A 的值 |
| `EXPLAIN ESTIMATE`（不加條件） | 只算到自己的 granule | 同左 |
| 執行計畫 | 表的 row policy 是 row-level filter，先於 PREWHERE；WHERE 自動轉 PREWHERE | 只有 PREWHERE |

把租戶條件寫在 view 的查詢裡的做法，同樣觸發上面的 `throwIf()`；而且 bootstrap 帳號建立 view 時沒有 `SQL_tenant_id`，必須改用 `getSettingOrDefault`。兩者都不採用。

DEFINER 帳號、查詢上限與 quota：

- DEFINER 帳號 `IDENTIFIED WITH no_password HOST NONE`：以空密碼或任何密碼登入都是 `AUTHENTICATION_FAILED`（HTTP 與容器內的 native 連線都試過）。
- 改用另一個 DEFINER（`ALTER TABLE … MODIFY SQL SECURITY DEFINER DEFINER = …`）可行；新帳號沒有自己的 row policy 時，view 讀到 0 列。view 還在用的 DEFINER 不能刪除（`HAVE_DEPENDENT_OBJECTS`）。
- view 內的查詢沿用呼叫者的設定：`readonly = 1`、`max_threads = 2`、`max_execution_time = 30`、`SQL_tenant_id` 都是呼叫帳號的值。view 內 `getSetting('max_rows_to_read')` 等「結果與讀取上限」顯示 0，但外層查詢的上限仍涵蓋 view 內讀的列：`max_rows_to_read = 100000` 的帳號讀 30 萬列得到 `TOO_MANY_ROWS`；`max_memory_usage = 50000000` 的帳號配置大陣列得到 `MEMORY_LIMIT_EXCEEDED`。這與 `SQL SECURITY INVOKER` 的 view 相同，不是 DEFINER 特有。
- quota：經 view 讀 303,104 列後，呼叫帳號的 `system.quota_usage` 從 0 個查詢、0 列變成 1 個查詢、303,104 列。

### 4. 效能（`performance.txt`）

90 萬列、同樣 6 種查詢，舊設計（租戶直接查表加 row policy）與新設計（經 view）比較，各跑 7 次與 15 次取伺服器端中位數：

| 查詢 | 讀取列數（兩者相同） | 主鍵 granule（兩者相同） | 第 2 輪：表／view（毫秒） |
| --- | --- | --- | --- |
| 全部筆數 | 8,193 | 37/110 | 7.6／12.6 |
| 全部列加總 | 303,104 | 37/110 | 58.2／83.6 |
| 單一設備、資料點與時間範圍 | 8,192 | 1/110 | 16.4／29.8 |
| 單一設備依資料點分組 | 16,384 | 2/110 | 21.2／28.9 |
| 每個設備與資料點的最新值 | 303,104 | 37/110 | 154.0／173.5 |
| 非主鍵欄過濾 | 303,104 | 37/110 | 73.8／82.9 |

主鍵分析與自動 PREWHERE 經 view 照常生效，讀取量完全相同；在高負載的主機上 view 多 5–25 毫秒。view 不接受明寫的 `PREWHERE`，也沒有 `_part` 等虛擬欄位。

### 5. 回歸測試

修正前（main 的 migration，`cargo-test-before-fix.txt`）：測試失敗，第一個失敗的檢查是 `introspection_tells_nothing_of_other_tenants`，列出：

- `mergeTreeAnalyzeIndexes`、`mergeTreeAnalyzeIndexesUUID` 對 `telemetry`：以租戶 B 的 ID 與不存在的 ID 執行，答案不同（depends on tenant B's rows），而且租戶帳號得到回應；
- `EXPLAIN indexes`、`EXPLAIN ESTIMATE`、`hasColumnInTable()`、`system.tables` 的統計對 `telemetry` 有回應（`total_rows` 為整張表的 7 列）。

修正後（`cargo-test-after-fix.txt`）：`clickhouse_schema_and_tenant_isolation` 通過（33.74 秒），`migrations` 6 項通過。內容：

- 24 個指名表的函式與 6 個其他探測，對 `telemetry`、`batch_commits`、view 各以 B 的 ID 與不存在的 ID 執行，共 180 個查詢：兩次答案與讀取列數全部相同，對表一律被拒或沒有任何列；94 個函式的分類與伺服器一致。
- 系統表：193 張中租戶讀得到的正好是上面 24 張，內容不含 B 的 ID 或資料，`system.tables` 只列出 view。
- 攻擊：Laysi/trustiot-v2#59 原本的攻擊加本 issue 新增的 7 項，共 56 項經 view 執行；驗證 PR 的 37 項加本 issue 新增的 3 項，共 40 項；其中查 view 的 42 項改查 `telemetry` 再跑一次，全部被拒。全部沒有出現 B 的 ID 或資料。
- 服務帳號：寫入、`DELETE FROM` 刪除一批（`ALTER DELETE` 權限）、寫完成紀錄與讀回高水位都正常；租戶經 view 也看不到刪掉的列。
- 上限與 quota：`max_rows_to_read = 1` 經 view 得到 `TOO_MANY_ROWS`，`max_memory_usage = 50000000` 得到 `MEMORY_LIMIT_EXCEEDED`，quota 計入 view 讀的列，第 121 個查詢被 quota 擋下。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 回歸測試加在 `services/ingest/tests/clickhouse.rs`：租戶帳號對表呼叫該函式與盤點出的同類函式一律被拒；對 view 也一律被拒；不再有命中訊號 | 通過。`introspection_tells_nothing_of_other_tenants` 涵蓋全部 24 個指名表的函式與 6 個其他探測；對表一律被拒或沒有任何列；對 view，內省函式全部被拒或無法執行，其餘（`merge`、`view` 等一般讀取）只回自己的列，而且以 B 的 ID 與不存在的 ID 執行的答案相同 |
| 修正前的 main 失敗、修正後通過 | 通過（第 5 節，`cargo-test-before-fix.txt`、`cargo-test-after-fix.txt`） |
| Laysi/trustiot-v2#59 的攻擊（原本 49 項與驗證 PR 新增的項目）全部仍讀到 0 筆；服務帳號的寫入與 `ALTER DELETE` 路徑仍可用 | 通過（第 5 節） |
| 做法寫進決策 14 的補充與 `services/ingest/README.md`；README 補上 `ALTER DELETE` 的實際範圍與 repo-check 的定位 | 通過：`docs/architecture/decisions.md` 決策 14、`services/ingest/README.md`「租戶讀取路徑」「租戶隔離」與 schema 一節，`0004_access.sql` 的註解 |
| 測試證據存到 `docs/test-reports/` | 本報告與 `data/2026-10-11-260-introspection/` |

已知限制與未涵蓋：

- 整張表的 part 數與 granule 總數仍可經 `EXPLAIN indexes` 看到；讀取統計以 granule 為單位（第 2 節）。驗證報告的發現 2（總筆數揭露）因此只部分消失：`system.tables` 與 `EXPLAIN ESTIMATE` 不再透露整張表的筆數。
- 只測了單節點、本機容器；ClickHouse Cloud 上的 `no_password HOST NONE` 帳號與 DEFINER view 待搬遷時驗證（README 已註明）。
- 效能數字在高負載的主機上量得，只用來比較兩種設計，不代表正式環境的延遲。
