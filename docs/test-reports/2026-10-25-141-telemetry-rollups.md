# 1 分鐘與 1 小時彙總表

對應 issue：Laysi/trustiot-v2#141（1 分鐘與 1 小時彙總表、彙總的保存期限、租戶讀取路徑、I1 重寫後的對帳、既有資料的 backfill）。測試日期 2026-10-25（Asia/Taipei）。原始輸出在 [data/2026-10-25-141-telemetry-rollups/](data/2026-10-25-141-telemetry-rollups/)。

## 環境

- **受測程式碼**：分支 `feat/telemetry-rollups`（由 `origin/main` 的 `dab8822` 開出），程式與測試的 commit `95d0c83`；本報告的 commit 只加文件與測試輸出。
- **執行的地方**：開發工作站，容器由測試的 podman 或 docker 偵測啟動，一次只跑一個起容器的測試程式；沒有設 `CI`、沒有設 `TEST_CLICKHOUSE_URL`。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2、cargo-deny 0.20.2、gitleaks 8.30.1。
- **測試用的伺服器**：拋棄式容器 `docker.io/clickhouse/clickhouse-server:26.8@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c`（與 chart、本地開發環境相同的 digest），回報版本 26.8.22.13。`tools/migrate` 的測試另外起 `crates/service-kit` 共用的 TLS Postgres。

## 指令

在 repo 根目錄：

```sh
# 彙總值與原始資料（--nocapture 印出每一步的證據）
cargo test -p ingest --test rollups -- --nocapture
cargo test -p lab-dataset --test rollups -- --nocapture

# 租戶隔離（含兩個彙總 view 與兩張彙總表）
cargo test -p ingest --test clickhouse -- --nocapture

# migration Job：套用 0010、彙總的保存期限設定、租戶角色的稽核
cargo test -p migrate --test clickhouse -- --nocapture
cargo test -p migrate --test clickhouse_telemetry_query -- --nocapture

# 受影響的 crate 全部測試（一次一個程式）
cargo test -p ingest -- --test-threads=1
cargo test -p lab-dataset -- --test-threads=1
cargo test -p migrate --no-fail-fast -- --test-threads=1
cargo test -p telemetry-query -- --test-threads=1

# repo 規則
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p repo-check
cargo deny --locked check
bun test scripts/ci/changes.test.ts
bun test deploy/chart
```

另外以 `clickhouse-client --multiquery` 把 `services/ingest/migrations/clickhouse/` 的全部檔案依序串起來套用到同一個映像的拋棄式容器（`deploy/chart/tests/e2e/otel.e2e.ts` 的做法；`0010` 是第一個帶查詢參數 `{name:Type}` 的檔案），結束碼 0，`trustiot` 資料庫有兩張彙總表、兩個 materialized view、兩個 backfill view 與三個租戶 view。

## 結果

### 彙總值與原始資料（`services/ingest/tests/rollups.rs`）

每一步之後，租戶經 `tenant_telemetry_1m`、`tenant_telemetry_1h` 讀到的每一格（筆數、數值筆數、和、最小、最大、第一筆與最後一筆）都和測試以 Rust 從原始列（同一個租戶經 `tenant_telemetry` 讀）算出的值相同（[ingest-rollups.txt](data/2026-10-25-141-telemetry-rollups/ingest-rollups.txt)）：

| 步驟 | 結果 |
| --- | --- |
| 套用 `0010` 之前寫入的三批（三天前收到、一小時前收到，以及剛收到的三天前讀值） | backfill 寫入 80 列，60 個 1 分鐘格、16 個 1 小時格一致；再套用一次寫入 0 列 |
| 套用之後再寫一批 | 76 與 17 格一致 |
| 同一個區塊重送 | 原始表與彙總都只算一次（20 與 8 格一致） |
| I1：前一次嘗試只寫進 32 列中的 10 列（含三天前的補送讀值），沒有完成紀錄 | 彙總與原始資料同樣算進這 10 列 |
| 同步刪除後以 `deduplicate_insert = 'disable'` 重寫整批 | 對帳之前 14 格不一致；1 分鐘的對帳寫入 10 列後中斷，重跑時 1 分鐘寫 0 列、1 小時寫 4 列；32 與 12 格一致；再對帳一次寫 0 與 0 列；同租戶另一個串流的格不變 |
| 去重的風險（E7a）：同步刪除後以同樣的 `insert_deduplication_token`、同樣的內容、同樣的內容加 `insert_deduplicate = 0` 重寫 | 三種都回 OK，12 列中寫入 0 列 |
| 同一批改用 `deduplicate_insert = 'disable'` 重寫並對帳 | 12 與 7 格一致 |
| 原始列寫入、`telemetry_1h_mv` 卸下（模擬兩者之間當機） | 對帳補回 1 小時表，16 與 8 格一致 |
| 累計型資料點 | 25 與 8 格一致，電能見下表 |
| 保存期限：收到日在 380 到 410 天前的 31 天，強制 merge | 1 分鐘表依 chrono 算出的「13 個月加一天」保留 16 天、刪掉 15 天，沒有提早刪的；1 小時表 31 天全部保留 |

電能（區間差，參數 `wraps_at` 0 表示不溢位）：

| 情況 | 讀值 | 逐筆相鄰差 | 1 分鐘 view | 1 小時 view |
| --- | --- | --- | --- | --- |
| bucket 內先到峰值再歸零 | 5、7、0、6 | 8 | 8 | 8 |
| 第一筆是 0 的歸零 | 0、5、0、3 | 8 | 8 | 8 |
| bucket 內溢位（W = 100） | 90、95、3、8 | 18 | 18 | 18 |
| 兩個 bucket 之間歸零 | 10、12、4、9 | 11 | 11 | 11 |
| 平穩遞增，跨兩個小時 | 1、2、3、5、6 | 5 | 5 | 5 |
| 超出條件：歸零後超過歸零前的峰值 | 5、7、0、9 | 11 | 11 | 13（近似，預期） |

### 合成 fixture（`tools/lab-dataset/tests/rollups.rs`）

以 Laysi/trustiot-v2#679 的載入方式載入 `datasets/lab/synthetic`（316 筆、16 批，其中 1 批沒有完成紀錄，含補送與歸零、32 位元計數溢位），比對資料集本身的讀值（[lab-dataset-rollups.txt](data/2026-10-25-141-telemetry-rollups/lab-dataset-rollups.txt)）：

- 1 分鐘 view 316 格、1 小時 view 28 格，全部與資料集算出的值一致，合計 316 筆，沒有完成紀錄的那一批與補送的讀值都算進去。
- 電能：`meter-1/energy_kwh`（48 筆，含歸零）逐筆相鄰差 164.046，兩個 view 都是 164.046（只差在浮點數的最後一位：小數第 15 位）；`meter-2/pulses`（47 筆，`wraps_at` 4294967296，含溢位）3099；`press-1/cycle_count`（42 筆）172，兩個 view 都相同。
- 對 16 批各跑一次對帳，兩句都寫入 0 列：只寫一次的批次不需要修正。

### 租戶隔離（`services/ingest/tests/clickhouse.rs`）

[ingest-clickhouse.txt](data/2026-10-25-141-telemetry-rollups/ingest-clickhouse.txt)：

- 內省：24 個指名表的 table function、`EXPLAIN indexes`／`ESTIMATE`、讀取統計、`hasColumnInTable()`、`system.tables`／`system.columns` 的統計，對每張表（兩張彙總表在內）、兩個 materialized view、兩個 backfill view 與三個租戶 view，以租戶 B 的 ID 與不存在的 ID 各跑一次，兩次答案相同；對表、materialized view 與 backfill view 一律被拒。
- 兩個彙總 view 各跑 29 項攻擊（A、B 的讀值在同一個設備、資料點與分鐘；直接查、對 B 過濾、`OR 1 = 1`、`GROUP BY tenant_id`、WHERE 裡的 `throwIf()` 碰 B 的格或 B 的最後一筆、`PREWHERE`、JOIN、子查詢、UNION、`FINAL`、`merge()`、`view()`、改 `SQL_tenant_id`、`additional_table_filters`、寫入、改 view 的 DEFINER 等），A 只讀到自己的 3 筆，回應裡沒有 B 的資料；其中讀取類的 20 項改對底表再跑，全部 `ACCESS_DENIED`。
- 直接被授與 `SELECT` 的帳號、管理帳號讀兩張彙總表與兩個 materialized view 都是 0 列；backfill view 只有套用檔案的管理帳號讀得到；`merge('trustiot', '.*')` 被拒（`CANNOT_CONVERT_TYPE`）。
- 存取物件：4 個角色、29 條 row policy、1 個 settings profile、1 個 quota、2 個不能登入的 DEFINER 帳號，全部在 `local_directory`；租戶角色只有三個租戶 view 的 `SELECT`；彙總表是 `AggregatingMergeTree`，TTL 13 與 48 個月。

### migration Job（`tools/migrate/tests/clickhouse.rs`、`clickhouse_telemetry_query.rs`）

[migrate-clickhouse.txt](data/2026-10-25-141-telemetry-rollups/migrate-clickhouse.txt)、[migrate-clickhouse-telemetry-query.txt](data/2026-10-25-141-telemetry-rollups/migrate-clickhouse-telemetry-query.txt)：

- 全新的 ClickHouse 套用到 `0010`；建表時的 13 與 48 個月就是平台值，第一次執行不改彙總表。
- 設成原始 400 天、1 分鐘 14 個月、1 小時 60 個月：依序改 1 小時、1 分鐘、完成表、原始資料；改回平台值時順序相反；從伺服器讀回的 TTL 等於設定。
- 1 分鐘 12 個月、1 小時短於 1 分鐘、121 個月、原始 400 天配 13 個月（最短 393 天）都在碰任何資料庫之前被拒，訊息寫出 key 與理由。
- 手動改成 `bucket + toIntervalMonth(13)` 的 TTL 讓 Job 停止並寫出要改回的形式，改回後 Job 接著對齊。
- 租戶角色少了 `tenant_telemetry_1h` 的 `SELECT`、多了 `telemetry_1m` 的 `SELECT`、多了 `tenant_telemetry_1m` 的 `GRANT OPTION`，Job 都讓服務帳號不能登入並停止，修好後恢復；租戶帳號讀彙總表被拒。

### 其他檢查

| 指令 | 結果 |
| --- | --- |
| `cargo test -p ingest`、`-p lab-dataset`、`-p telemetry-query` | 全過 |
| `cargo test -p migrate --no-fail-fast` | 除 `tests/mqtt_broker.rs` 的 `gateways_need_a_current_certificate_of_our_ca` 外全過；這一項在 `main`（`dab8822`）的樹上同樣失敗（broker 重新載入信任檔時讀不到 CRL 後停止），與本變更無關 |
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p repo-check` | 通過 |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都 ok |
| `bun test scripts/ci/changes.test.ts` | 282 項通過 |
| `bun test deploy/chart` | 655 項通過 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 以 Laysi/trustiot-v2#679 的合成 fixture 驗證彙總值與原始資料計算的結果一致 | 通過：316 與 28 格一致，電能三個資料點一致。用 lab 資料集再驗一次延後到 Laysi/trustiot-v2#681 |
| 彙總表的 TTL 等於 docs/ 訂的期限（測試讀取 TTL） | 通過：從伺服器讀回 13 與 48 個月，設定改變後讀回設定值；保存期限依收到日按月生效 |
| 租戶 A 讀不到 B 的彙總資料 | 通過：兩個彙總 view 各 29 項攻擊與內省探測，底表、materialized view 與 backfill view 對租戶一律被拒 |
| 模擬 I1 的批次重寫與離線補送的亂序資料，彙總值仍與原始資料一致 | 通過：部分寫入後重寫並對帳、對帳中斷後重跑、原始與彙總之間當機、補送的舊讀值，每一步都一致 |

沒有實測的項目（依驗證分級延後）：彙總表與原始資料的列數與大小比例、每次 INSERT 多寫兩個 part 的寫入延遲、負載下對帳的延遲、大表上 backfill 的時間。
