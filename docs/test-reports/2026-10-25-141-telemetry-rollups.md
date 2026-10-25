# 1 分鐘與 1 小時彙總表

對應 issue：Laysi/trustiot-v2#141（1 分鐘與 1 小時彙總表、彙總的保存期限、租戶讀取路徑、I1 重寫後的對帳、既有資料的 backfill）。測試日期 2026-10-25（Asia/Taipei）。本報告是審查修正後重跑的結果：backfill 改由 migration Job 在對齊 TTL 之後執行、只補彙總缺的格、只比對原始資料一定還在的收到日，backfill view 平常不給任何帳號看，寫入串流的語句帶 `stream_settings`，測試只用靜態 SQL。原始輸出在 [data/2026-10-25-141-telemetry-rollups/](data/2026-10-25-141-telemetry-rollups/)。

## 環境

- **受測程式碼**：分支 `feat/telemetry-rollups`（由 `origin/main` 的 `dab8822` 開出），程式與測試的 commit `6521348`；本報告的 commit 只加文件與測試輸出。
- **執行的地方**：開發工作站，容器由測試的 podman 或 docker 偵測啟動，一次只跑一個起容器的測試程式；沒有設 `CI`、沒有設 `TEST_CLICKHOUSE_URL`。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2、cargo-deny 0.20.2、gitleaks 8.30.1。
- **測試用的伺服器**：拋棄式容器 `docker.io/clickhouse/clickhouse-server:26.8@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c`（與 chart、本地開發環境相同的 digest），回報版本 26.8.22.13。`tools/migrate` 的測試另外起 `crates/service-kit` 共用的 TLS Postgres。

## 指令

在 repo 根目錄：

```sh
# 彙總值與原始資料、backfill 的界限、串流的 query ID（--nocapture 印出每一步的證據）
cargo test -p ingest --test rollups -- --nocapture
cargo test -p lab-dataset --test rollups -- --nocapture
TRUSTIOT_LAB_DATASET_INDEX=<合成 fixture 索引的複本> cargo test -p lab-dataset --test rollups -- --nocapture

# 租戶隔離（含兩個彙總 view、兩張彙總表與兩個 backfill view）
cargo test -p ingest --test clickhouse -- --nocapture

# migration Job：套用 0010、彙總的保存期限、backfill、租戶角色的稽核
cargo test -p migrate --test clickhouse -- --nocapture
cargo test -p migrate --test clickhouse_backfill -- --nocapture
cargo test -p migrate --test clickhouse_telemetry_query -- --nocapture

# 受影響的 crate 全部測試（一次一個起容器的程式）
cargo test -p ingest --no-fail-fast
cargo test -p lab-dataset --no-fail-fast
cargo test -p migrate --no-fail-fast
cargo test -p telemetry-query --no-fail-fast

# repo 規則
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p sql-check
cargo test -p repo-check
cargo deny --locked check
cargo run -p openapi -- --check
bun test scripts/ci/changes.test.ts
bun test deploy/chart
```

另外以 `clickhouse-client --multiquery` 把 `services/ingest/migrations/clickhouse/` 的全部檔案依序串起來套用到同一個映像的拋棄式容器（`deploy/chart/tests/e2e/otel.e2e.ts` 的做法），結束碼 0：`trustiot` 資料庫有兩張彙總表、兩個 materialized view、兩個 backfill view 與三個租戶 view，兩個 backfill view 的 `migration_account` policy 的 `apply_to_list` 都是 `[]`（不點名任何帳號）。`0010` 本身不再寫入任何列。

## 結果

### 彙總值與原始資料（`services/ingest/tests/rollups.rs`）

每一步之後，租戶經 `tenant_telemetry_1m`、`tenant_telemetry_1h` 讀到的每一格（筆數、數值筆數、和、最小、最大、第一筆與最後一筆）都和測試以 Rust 從原始列（同一個租戶經 `tenant_telemetry` 讀）算出的值相同（[ingest-rollups.txt](data/2026-10-25-141-telemetry-rollups/ingest-rollups.txt)）：

| 步驟 | 結果 |
| --- | --- |
| 套用 `0010` 之前寫入的批次，以 Job 的 backfill 語句（`BACKFILL_1M`、`BACKFILL_1H`）補 | policy 點名管理帳號之前，管理帳號從 backfill view 讀到 0 列；之後 7 個收到日要補，寫入 84 個 1 分鐘列、40 個 1 小時列，84 與 36 格一致；再補一次寫入 0 與 0 列 |
| 套用之後再寫一批 | 100 與 37 格一致 |
| 界限：原始 TTL 縮短到 10 天並強制 merge，12 天前的原始列全部過期、10 天前的只過期一部分（當天第一毫秒收到的列過期、最後一毫秒的留下） | 重新套用 `0010`、以 10 天或 400 天再 backfill 都寫入 0 列；以 10 天檢查沒有任何差異、修正（`REPAIR_1M`、`REPAIR_1H`）寫入 0 列；兩天的格都不變。同樣的檢查改傳 400 天，會把這兩天列成負的差（-8、-4），這就是 view 要有界限的原因 |
| 界限以內刪掉一批（沒有對帳） | backfill 不動它的格，修正把它們拿掉 |
| 界限以內彙總遺失的一天 | 檢查列出 `aggregated = 0` 的格，backfill 補回 |
| 同一個區塊重送 | 原始表與彙總都只算一次（20 與 8 格一致） |
| I1：前一次嘗試只寫進一部分，沒有完成紀錄 | 彙總與原始資料同樣算進這些列（24 與 8 格一致） |
| 同步刪除後以 `deduplicate_insert = 'disable'` 重寫整批 | 對帳之前 14 格不一致；1 分鐘的對帳寫入 10 列後中斷，重跑時 1 分鐘寫 0 列、1 小時寫 4 列；32 與 12 格一致；再對帳一次寫 0 與 0 列；同租戶另一個串流的格不變 |
| 去重的風險（E7a）：同步刪除後以同樣的 `insert_deduplication_token`、同樣的內容、同樣的內容加 `insert_deduplicate = 0` 重寫 | 三種都回 OK，12 列中寫入 0 列 |
| 同一批改用 `deduplicate_insert = 'disable'` 重寫並對帳 | 12 與 7 格一致 |
| 原始列寫入、`telemetry_1h_mv` 卸下（模擬兩者之間當機） | 對帳補回 1 小時表，16 與 8 格一致 |
| 串流的 query ID：同一個串流還有一句在伺服器上執行 | 同一個 query ID、不帶 `replace_running_query` 的語句被拒（`QUERY_WITH_SAME_ID_IS_ALREADY_RUNNING`）；帶 `stream_settings` 的對帳取消那一句（`QUERY_WAS_CANCELLED`）並照常執行，12 與 4 格一致 |
| 累計型資料點 | 25 與 8 格一致，電能見下表 |
| 保存期限：收到日在 380 到 410 天前的 31 天，強制 merge | 1 分鐘表依 chrono 算出的「13 個月加一天」保留 16 天、刪掉 15 天，沒有提早刪的；1 小時表 31 天全部保留 |

測試裡每一句寫入串流的語句都帶 `stream_settings`；ingest 與租戶帳號建立後，測試確認 `CREATE USER ... DEFAULT ROLE` 授與了角色（`system.role_grants`）。

電能（區間差，參數 `wraps_at` 0 表示不溢位）：

| 情況 | 讀值 | 逐筆相鄰差 | 1 分鐘 view | 1 小時 view |
| --- | --- | --- | --- | --- |
| bucket 內先到峰值再歸零 | 5、7、0、6 | 8 | 8 | 8 |
| 第一筆是 0 的歸零 | 0、5、0、3 | 8 | 8 | 8 |
| bucket 內溢位（W = 100） | 90、95、3、8 | 18 | 18 | 18 |
| 兩個 bucket 之間歸零 | 10、12、4、9 | 11 | 11 | 11 |
| 平穩遞增，跨兩個小時 | 1、2、3、5、6 | 5 | 5 | 5 |
| 超出條件：歸零後超過歸零前的峰值 | 5、7、0、9 | 11 | 11 | 13（近似，預期） |

### 資料集（`tools/lab-dataset/tests/rollups.rs`）

以 Laysi/trustiot-v2#679 的載入方式載入 `lab_dataset::index_for_tests()` 的資料集，比對資料集本身的讀值；預期值全部從資料集與它的索引算（[lab-dataset-rollups.txt](data/2026-10-25-141-telemetry-rollups/lab-dataset-rollups.txt)）：

- 預設的合成 fixture `datasets/lab/synthetic`（316 筆、16 批，其中 1 批沒有完成紀錄，含補送與歸零、32 位元計數溢位）：1 分鐘 view 316 格、1 小時 view 28 格，全部與資料集算出的值一致，合計 316 筆，沒有完成紀錄的那一批與補送的讀值都算進去。
- 電能：要算的資料點是索引裡 `kind = "cumulative"` 的每一個（溢位值取它的 `wraps_at`）。兩個 view 的答案等於以同一個公式從資料集算出的值；三個資料點的每個 bucket 都符合精確條件，所以也等於逐筆相鄰差的和：`meter-1/energy_kwh`（48 筆，含歸零）164.046（只差在浮點數的最後一位）、`meter-2/pulses`（47 筆，`wraps_at` 4294967296，含溢位）3099、`press-1/cycle_count`（42 筆）172。
- 以 `stream_settings` 對 16 批各跑一次對帳，兩句都寫入 0 列：只寫一次的批次不需要修正。
- 設 `TRUSTIOT_LAB_DATASET_INDEX` 指向 fixture 索引的複本（Laysi/trustiot-v2#681 換成 lab 錄製資料時的用法），輸出標明資料集來自這個變數，結果相同（[lab-dataset-rollups-index-variable.txt](data/2026-10-25-141-telemetry-rollups/lab-dataset-rollups-index-variable.txt)）。

### 租戶隔離（`services/ingest/tests/clickhouse.rs`）

[ingest-clickhouse.txt](data/2026-10-25-141-telemetry-rollups/ingest-clickhouse.txt)：

- 內省：24 個指名表的 table function、`EXPLAIN indexes`／`ESTIMATE`、讀取統計、`hasColumnInTable()`、`system.tables`／`system.columns` 的統計，對每張表（兩張彙總表在內）、兩個 materialized view、兩個 backfill view 與三個租戶 view，以租戶 B 的 ID 與不存在的 ID 各跑一次，兩次答案相同；對表、materialized view 與 backfill view 一律被拒。
- 兩個彙總 view 各跑 29 項攻擊（A、B 的讀值在同一個設備、資料點與分鐘；直接查、對 B 過濾、`OR 1 = 1`、`GROUP BY tenant_id`、WHERE 裡的 `throwIf()` 碰 B 的格或 B 的最後一筆、`PREWHERE`、JOIN、子查詢、UNION、`FINAL`、`merge()`、`view()`、改 `SQL_tenant_id`、`additional_table_filters`、寫入、改 view 的 DEFINER 等），A 只讀到自己的 3 筆，回應裡沒有 B 的資料；其中讀取類的 20 項改對底表再跑，全部 `ACCESS_DENIED`。
- 直接被授與 `SELECT` 的帳號、管理帳號讀兩張彙總表、兩個 materialized view 與兩個 backfill view 都是 0 列（backfill view 的 `migration_account` policy 不點名任何帳號）；ingest 帳號讀 backfill view 被拒；`merge('trustiot', '.*')` 被拒（`CANNOT_CONVERT_TYPE`）。
- 存取物件：4 個角色、29 條 row policy、1 個 settings profile、1 個 quota、2 個不能登入的 DEFINER 帳號，全部在 `local_directory`；租戶角色只有三個租戶 view 的 `SELECT`；彙總表是 `AggregatingMergeTree`，TTL 13 與 48 個月。

### migration Job（`tools/migrate/tests/clickhouse.rs`、`clickhouse_backfill.rs`、`clickhouse_telemetry_query.rs`）

[migrate-clickhouse.txt](data/2026-10-25-141-telemetry-rollups/migrate-clickhouse.txt)、[migrate-clickhouse-backfill.txt](data/2026-10-25-141-telemetry-rollups/migrate-clickhouse-backfill.txt)、[migrate-clickhouse-telemetry-query.txt](data/2026-10-25-141-telemetry-rollups/migrate-clickhouse-telemetry-query.txt)：

- 全新的 ClickHouse 套用到 `0010`；建表時的 13 與 48 個月就是平台值，第一次執行不改彙總表；同一次執行在對齊 TTL 之後 backfill（原始 180 天：補 2026-05-01 到 2026-10-25），第二次執行不再補。
- 設成原始 400 天、1 分鐘 14 個月、1 小時 60 個月：依序改 1 小時、1 分鐘、完成表、原始資料；改回平台值時順序相反；從伺服器讀回的 TTL 等於設定。
- 1 分鐘 12 個月、1 小時短於 1 分鐘、121 個月、原始 400 天配 13 個月（最短 393 天）都在碰任何資料庫之前被拒，訊息寫出 key 與理由。
- 手動改成 `bucket + toIntervalMonth(13)` 的 TTL 讓 Job 停止並寫出要改回的形式，改回後 Job 接著對齊。
- 租戶角色少了 `tenant_telemetry_1h` 的 `SELECT`、多了 `telemetry_1m` 的 `SELECT`、多了 `tenant_telemetry_1m` 的 `GRANT OPTION`，Job 都讓服務帳號不能登入並停止，修好後恢復；租戶帳號讀彙總表被拒。
- backfill（`clickhouse_backfill.rs`，在套用 `0010` 之前已有原始列、原始資料保存 401 天的 ClickHouse 上）：

| 步驟 | 結果 |
| --- | --- |
| 第一次執行，1 分鐘彙總 14 個月 | 報告裡 1 分鐘表 TTL 從 13 改成 14 個月的那一行在 backfill 那一行之前；backfill 從 2025-09-22（`today − 398`）補到今天；13 個月又一天前（2025-09-24）收到的格在強制 merge 後仍在；界限之前（2025-09-21，`today − 399`）的原始列不補；記錄 `backfill/telemetry_rollups`；兩個 policy 回到不點名任何帳號 |
| 再執行一次 | 沒有任何變更，每一天的格不變 |
| 原始資料改成 200 天、強制 merge、刪掉昨天的格 | 截止日 2026-04-08 的原始列只剩最後一毫秒收到的 4 筆，彙總仍是 8 筆 |
| policy 不點名任何帳號時 | 管理帳號對昨天的檢查讀到 0 列；點名自己之後讀到 2 列 |
| `trustiot_migrate.applied` 遺失（TRUNCATE），以第二個管理帳號執行 | 重新套用全部檔案，backfill 從 2026-04-11（`today − 197`）補到今天，補回昨天的格；2026-04-11 以前各天的列（原始與彙總共 5 列，含只過期一部分的 2026-04-08）完全不變；policy 回到不點名任何帳號 |

### 其他檢查

| 指令 | 結果 |
| --- | --- |
| `cargo test -p ingest`、`-p lab-dataset`、`-p telemetry-query` | 全過 |
| `cargo test -p migrate --no-fail-fast` | 除 `tests/mqtt_broker.rs` 的 `gateways_need_a_current_certificate_of_our_ca` 外全過；這一項在 `main`（`dab8822`）的樹上同樣失敗（broker 重新載入信任檔時讀不到 CRL 後停止），與本變更無關 |
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p sql-check`、`cargo test -p repo-check` | 通過 |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都 ok |
| `cargo run -p openapi -- --check` | `api/openapi.json` 與程式一致 |
| `bun test scripts/ci/changes.test.ts` | 282 項通過 |
| `bun test deploy/chart` | 655 項通過 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 以 Laysi/trustiot-v2#679 的合成 fixture 驗證彙總值與原始資料計算的結果一致 | 通過：316 與 28 格一致，電能三個資料點一致；測試從索引讀資料點，換成 lab 資料集（Laysi/trustiot-v2#681）只要設 `TRUSTIOT_LAB_DATASET_INDEX` |
| 彙總表的 TTL 等於 docs/ 訂的期限（測試讀取 TTL） | 通過：從伺服器讀回 13 與 48 個月，設定改變後讀回設定值；保存期限依收到日按月生效；backfill 在對齊 TTL 之後執行，補進的舊格依部署的設定保存 |
| 租戶 A 讀不到 B 的彙總資料 | 通過：兩個彙總 view 各 29 項攻擊與內省探測，底表、materialized view 與 backfill view 對租戶一律被拒；backfill view 平常連管理帳號也讀不到 |
| 模擬 I1 的批次重寫與離線補送的亂序資料，彙總值仍與原始資料一致 | 通過：部分寫入後重寫並對帳、對帳中斷後重跑、原始與彙總之間當機、補送的舊讀值、前一句仍在執行時的對帳，每一步都一致 |
| 既有資料的 backfill 不改動原始列已過期的收到日 | 通過：重新套用、`applied` 遺失後再補、以實際保存天數檢查與修正，都不動界限以前（含只過期一部分的那天）的格 |

沒有實測的項目（依驗證分級延後）：彙總表與原始資料的列數與大小比例、每次 INSERT 多寫兩個 part 的寫入延遲、負載下對帳的延遲、大表上 backfill 每一天要多久、views 在大區塊 INSERT 時的記憶體用量（審查時在 1.5 GB 的容器以 100 萬列一個區塊寫入出現 `MEMORY_LIMIT_EXCEEDED`）。
