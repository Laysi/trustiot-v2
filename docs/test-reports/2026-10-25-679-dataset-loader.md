# lab 資料集工具第一階段：索引格式、合成 fixture 與載入工具

對應 issue：Laysi/trustiot-v2#679（第一階段：索引格式與版本化規則、合成 fixture、載入工具）。測試日期 2026-10-25（Asia/Taipei）。原始輸出在 [data/2026-10-25-679-dataset-loader/](data/2026-10-25-679-dataset-loader/)。

## 環境

- **受測程式碼**：分支 `feat/lab-dataset-tools`，功能 commit `d155382`（由 `origin/main` 的 `fddd1f1` 開出）；本報告的 commit 只加文件與測試輸出。
- **執行的地方**：開發工作站，容器由測試的 podman 或 docker 偵測啟動；沒有設 `CI`、沒有設 `TEST_CLICKHOUSE_URL`。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2、cargo-deny 0.20.2、gitleaks 8.30.1。
- **測試用的伺服器**：`tools/lab-dataset/tests/clickhouse_load.rs` 以 `services/ingest/tests/common/mod.rs` 的 `start_throwaway` 起拋棄式容器 `docker.io/clickhouse/clickhouse-server:26.8@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c`（與 chart、本地開發環境相同的 digest），套用 migration Job 的 `PRODUCT` 全部 ClickHouse migration（`0001`–`0009`）。

## 指令

在 repo 根目錄：

```sh
# 合成 fixture 的檢查與摘要
cargo run -p lab-dataset -- check datasets/lab/synthetic/index.toml

# 單元測試、fixture 內容、載入 ClickHouse（--nocapture 印出逐點筆數）
cargo test -p lab-dataset -- --nocapture

# 整個 workspace 與 repo 規則
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
cargo deny --locked check
bun test scripts/ci/changes.test.ts
bun test deploy/chart
```

## 結果

### 合成 fixture

`check` 的摘要（[check.json](data/2026-10-25-679-dataset-loader/check.json)）：2 個分段、316 筆，2026-01-05T22:00:00.120Z 到 2026-01-06T01:55:02.500Z。

`tests/synthetic_fixture.rs` 從分段檔的內容檢查（[fixture-tests.txt](data/2026-10-25-679-dataset-loader/fixture-tests.txt)，8 項全過）：

| 狀況 | 測試 | 在內容裡找到的 |
| --- | --- | --- |
| 累計電表歸零 | `a_cumulative_meter_is_reset_to_zero` | `meter-1/energy_kwh` 在 23:30:00.120 由 15293.813 降為 0，且是唯一一次下降 |
| 計數器溢位 | `a_counter_overflows_at_its_register_size` | `meter-2/pulses` 由 4294967245 繞回 18（`wraps_at` 4294967296，實際增量 69） |
| 資料缺漏 | `readings_are_missing` | `meter-2` 兩個資料點各缺 1 次（間隔 2 倍輪詢），`press-1` 三個資料點各缺 6 次（間隔 7 倍） |
| 亂序與補送 | `backfilled_readings_arrive_after_newer_ones` | 63 筆讀值晚於同一資料點更新的讀值到達，全在 `backlog` 串流，7 個資料點都有；延遲都長於任何即時資料的延遲，最長超過 50 分鐘 |
| 同一台轉換器下多台設備 | `two_devices_share_a_converter` | `rs485-a` 下 `meter-1`（站號 1）、`meter-2`（站號 2），兩台都有讀值；`rs485-b` 的 `press-1` 也是站號 1 |
| 未完成的批次 | `one_batch_is_uncommitted` | `live` 第 13 批有 21 筆、沒有完成紀錄 |
| 索引與檔案一致 | `the_index_matches_its_files` | 316 筆，四種值型別都有 |
| 竄改被拒 | `a_changed_copy_is_refused` | 改一個數字（SHA-256 不符）、目錄名稱不符、symlink 指到目錄外，都被拒絕 |

### 載入 ClickHouse

`tests/clickhouse_load.rs`（[clickhouse-load.txt](data/2026-10-25-679-dataset-loader/clickhouse-load.txt)）以 library 載入租戶 A（位移取整天，資料集最後的時間落在現在之前一天內），以 `lab-dataset load` 指令載入租戶 B（指定確切位移與 `press-1` 的設備 ID），以測試用的讀取帳號（row policy `USING 1`）讀回比對：

| 項目 | 租戶 A | 租戶 B |
| --- | --- | --- |
| ClickHouse 的總筆數 / 索引 | 316 / 316 | 316 / 316 |
| 每個資料點的筆數 | 7 個資料點都與索引相同（48、48、47、47、42、42、42） | 同左 |
| 每一列（串流與設備 ID、批次序號、位移後的 `ts` 與 `received_at`、值） | 316 列全部相同 | 316 列全部相同 |
| INSERT | 16 批，每批 1 個 part，依抵達順序（`_part` 的 block 編號） | 同左 |
| 完成紀錄 | 15 筆（`row_count`、`committed_at` = 位移後的 `received_at`、閘道器都正確），未完成的 1 批沒有 | 同左 |

兩個租戶的設備與串流 ID 不重疊，載入 B 之後 A 的內容不變（再比對一次）。

驗證測試確實會失敗（暫時改程式後還原，未提交）：讓載入工具每批都寫完成紀錄，測試在筆數摘要失敗；把 `received_at` 換成 `ts`，測試在逐列比對失敗。

### 其他檢查

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test --workspace --no-fail-fast` | 1579 通過、5 失敗。失敗的都在本分支沒有改動的 crate（程式與相依和 `origin/main` 相同），逐一重跑：`ai-clients` 的 `an_unanswered_connection_attempt_is_a_connect_timeout`（依賴作業系統的 accept queue 行為）、`service-kit` 的兩個 PID 1 容器測試（容器的 8080 埠沒有對應到主機）、`migrate` 的 `gateways_need_a_current_certificate_of_our_ca`（reload 後的連線）重跑時同樣失敗，是這台工作站的平台與容器環境造成；`identity` 的 `there_is_no_password_login_sign_up_or_fake_login`（連線被重設）重跑通過。`lab-dataset`、`ingest`、`telemetry-query`、`repo-check`、`sql-check` 全過。以 CI 的結果為準 |
| `cargo deny --locked check` | `advisories ok, bans ok, licenses ok, sources ok`（重複版本的警告是既有的；本分支沒有新增第三方 crate） |
| `bun test scripts/ci/changes.test.ts` | 282 pass |
| `bun test deploy/chart`（讀 `services/ingest/tests/common/mod.rs` 的映像 digest） | 655 pass |

## 結論

第一階段涵蓋的驗收條件：

- `datasets/lab/README.md` 寫明索引格式、版本化規則、存放位置與存取權限，以及下游測試怎麼載入資料集：**部分通過**。索引格式、版本化規則、下游測試的載入方式已寫；存放位置與存取權限待第二階段決定（README 已寫明 `location` 的 S3 相容 URI 形式與金鑰的處理原則）。
- 載入工具把合成 fixture 寫進本地 ClickHouse 的窄表，筆數和索引一致（測試）：**通過**，以套用產品 schema 的拋棄式 ClickHouse 驗證（上表）。載入 `bun scripts/dev.ts up` 起的開發環境 ClickHouse 待第二階段：開發環境的 migration Job 目前不建 ClickHouse schema。
- 合成 fixture 含累計電表歸零或溢位、資料缺漏、亂序與補送，以及同一台轉換器下至少兩台設備（測試檢查 fixture 的內容）：**通過**（兩者都有：歸零與溢位）。

第二階段的驗收條件（人工異常片段的插入 API、錄製工具）：**待測**，不在本階段範圍。
