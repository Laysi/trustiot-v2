# lab 資料集工具第二階段：人工異常片段與錄製工具

對應 issue：Laysi/trustiot-v2#679（第二階段：人工異常片段的插入 API、錄製工具、存放位置與存取權限）。測試日期 2026-10-25（Asia/Taipei）。原始輸出在 [data/2026-10-25-679-anomalies-recorder/](data/2026-10-25-679-anomalies-recorder/)。第一階段的證據見 [2026-10-25-679-dataset-loader.md](2026-10-25-679-dataset-loader.md)。

## 環境

- **受測程式碼**：分支 `feat/lab-dataset-tools`，功能 commit `3635c7f`（人工異常片段）與 `ec81331`（錄製工具、存放位置與存取權限），由 `origin/main` 的 `fddd1f1` 開出；本報告的 commit 只加文件與測試輸出。
- **執行的地方**：開發工作站，容器由測試的 podman 或 docker 偵測啟動；沒有設 `CI`、沒有設 `TEST_CLICKHOUSE_URL`。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2、cargo-deny 0.20.2、gitleaks 8.30.1。
- **測試用的伺服器**：`clickhouse_load.rs` 與 `record.rs` 以 `services/ingest/tests/common/mod.rs` 的 `start_throwaway` 起拋棄式容器 `docker.io/clickhouse/clickhouse-server:26.8@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c`，套用 migration Job 的 `PRODUCT` 全部 ClickHouse migration。
- **假的 RS485 轉換器**：`crates/testkit` 的 `FakeModbus`（`Transport::RtuOverTcp`，loopback），時鐘是 `testkit::clock::manual()`（暫停的 tokio 時鐘，測試推進才會動）。

## 指令

在 repo 根目錄：

```sh
# 人工異常片段（--nocapture 印出每個資料點在時間窗內的讀值數）
cargo test -p lab-dataset --test anomalies -- --nocapture

# 錄製工具對假 RTU-over-TCP 端點錄製，再載入拋棄式 ClickHouse
cargo test -p lab-dataset --test record -- --nocapture

# 載入 ClickHouse（租戶 C 帶人工異常片段）
cargo test -p lab-dataset --test clickhouse_load -- --nocapture

# 單元測試（異常片段的效果與規則、錄製設定的檢查、指令列）
cargo test -p lab-dataset --lib

# 整個 workspace 與 repo 規則
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
cargo test -p repo-check
cargo deny --locked check
bun test scripts/ci/changes.test.ts
```

## 結果

### 人工異常片段

`tests/support/mod.rs` 定義四個片段，`anomalies.rs` 與 `clickhouse_load.rs` 共用：

| 代號 | 種類 | 設備與資料點 | 時間窗（資料集時間） | 預期觸發 |
| --- | --- | --- | --- | --- |
| `power-spike` | `spike`，`value = 480.5` | `meter-1`：`power_kw` | 2026-01-05T22:20:00.000Z–22:30:00.999Z | `power-high` 觸發 |
| `pulse-stop` | `outage` | `meter-2`：`pulses`、`power_kw` | 2026-01-05T23:50:00.000Z–2026-01-06T00:00:00.999Z | `machine-stopped` 觸發 |
| `meter-stop` | `outage` | `meter-1`：`energy_kwh`、`power_kw` | 2026-01-06T00:50:00.000Z–01:00:00.999Z | `machine-stopped` 觸發 |
| `press-changeover` | `changeover`，`state = "changeover"` | `press-1`：`running`、`mode`、`cycle_count` | 2026-01-06T01:30:00.000Z–01:40:00.999Z | `changeover-detected` 觸發、`machine-stopped` 不觸發 |

`tests/anomalies.rs`（[anomalies.txt](data/2026-10-25-679-anomalies-recorder/anomalies.txt)，4 項全過）：

| 測試 | 檢查的事 |
| --- | --- |
| `inserted_anomalies_follow_their_parameters` | 以 `Dataset::with_anomalies` 插入後，逐筆比對：只有讀值時間落在參數給的時間窗內的讀值照種類改變（7 個資料點共 24 筆，見輸出），窗外的 gauge 與狀態值不變；筆數、`ts`、`received_at`、批次都不變；累計值（含 32 位元溢位的 `pulses`）在窗內停住、之後每一步的增量與原資料相同、從不倒退；`expect` 標籤原樣保留 |
| `anomalies_in_the_index_are_applied` | 把同樣四個片段寫進複製的 fixture 索引 `[[anomalies]]`：`Dataset::open` 得到與插入 API 相同的讀值；`lab-dataset check` 回報 4 個片段；時間窗內沒有讀值的片段（`press-1` 斷線那段）被拒絕 |
| `the_readme_example_is_a_valid_index_table` | `datasets/lab/README.md` 的 `[[anomalies]]` 範例能被開啟 |
| `bad_insertions_are_refused` | 累計值的時間窗內有歸零、代號與已插入的片段重複、沒有 `expect`，都被拒絕。其他規則（時間窗重疊、觸發名稱重複、種類與資料點型別的每一種組合、參數缺漏或多餘、`int` 資料點的非整數突波值）由 `src/anomaly.rs` 的單元測試逐條檢查 |

載入 ClickHouse（`tests/clickhouse_load.rs` 的租戶 C，[clickhouse-load.txt](data/2026-10-25-679-anomalies-recorder/clickhouse-load.txt)）：帶這四個片段載入，`Loaded.anomalies` 的時間窗加上同一個位移（2026-10-24T22:20:00.000Z–22:30:00.999Z）；ClickHouse 裡 `meter-1/power_kw` 讀成 480.5 的列正好是位移後時間窗內的 3 筆，筆數仍是 316，`expect` 是 `power-high` 觸發。

### 錄製工具

`tests/record.rs`（[record.txt](data/2026-10-25-679-anomalies-recorder/record.txt)）：一個 `FakeModbus`（RTU over TCP）後面兩台設備，`meter-1` 站號 1（每 1 秒：`u32` 累計值從 4294967000 起跨過 32 位元溢位、`f32` 功率）、`press-1` 站號 2（每 2 秒：`u16` 計數器跨過 65536、線圈、2 個暫存器的文字），`batch_s = 4`、`segment_bytes = 1024`，手動時鐘推進 10 秒。

| 項目 | 結果 |
| --- | --- |
| 連線 | 兩台設備共用 1 條連線，請求只用讀取的功能碼（1–4），站號 1 與 2 都有被問到 |
| `Dataset::open` | 接受錄出的資料集（錄製工具結束時也自己檢查一次） |
| 索引內容 | 只有轉換器代號 `rs485-a` 與站號；沒有 `127.0.0.1`、`host`、`port` |
| 值與時間 | 40 筆，每一筆的值與讀值時間都等於假端點在那一秒提供的值（含溢位後的值） |
| 批次 | 3 批，`live` 序號 1–3 連續；`received_at` 分別是 08:00:04、08:00:08、08:00:10（關批時間），每筆 `ts <= received_at`；0 筆被略過 |
| 分段檔 | 3 個，在批次之間切開，每個都比 1024 bytes 大（單一批次比上限大時自成一個分段） |
| 重複錄製 | 同一個輸出目錄再錄一次被拒絕（`exists`） |
| 載入 ClickHouse | 拋棄式 ClickHouse 裡 40 筆，5 個資料點的筆數都等於索引（11、11、6、6、6），3 筆完成紀錄 |

### 其他檢查

| 指令 | 結果 |
| --- | --- |
| `cargo test -p lab-dataset --lib` | 37 通過（[unit.txt](data/2026-10-25-679-anomalies-recorder/unit.txt)） |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test --workspace --no-fail-fast` | 1596 通過、7 失敗。失敗的都在本分支沒有改動的 crate（程式與 `origin/main` 相同）：`identity` 的 `codes_are_used_once_and_die_after_five_attempts`、`telemetry-query` 的 `each_tenant_account_sees_its_own_readings_only`（都是 HTTP 401）、`certs` 的 `mosquitto_serves_the_broker_certificate_and_refuses_it_from_a_client`，單獨重跑都通過；`ai-clients` 的 connect timeout、`service-kit` 的兩個 PID 1 容器測試、`migrate` 的 `gateways_need_a_current_certificate_of_our_ca` 與第一階段相同，是這台工作站的平台與容器環境造成。`lab-dataset` 的 5 個測試目標全過。以 CI 的結果為準 |
| `cargo test -p repo-check` | 通過 |
| `cargo deny --locked check` | `advisories ok, bans ok, licenses ok, sources ok`（新增的相依只有 workspace 內的 `agent`、`protocol`、`testkit` 與 tokio 的 `signal` feature） |
| `bun test scripts/ci/changes.test.ts` | 282 pass |

## 結論

第二階段涵蓋的驗收條件：

- 人工異常片段的插入 API 有測試：**通過**。時間窗跟參數走、帶預期觸發標籤、測試程式與索引 `[[anomalies]]` 兩種來源一致、跟資料一起位移（上表）。
- 錄製工具對假 RTU-over-TCP 端點錄出的資料符合索引格式，而且能由載入工具寫進 ClickHouse（測試）：**通過**（上表）。
- `datasets/lab/README.md` 寫明存放位置與存取權限：**規則已寫**（2026-10-25 的決定：S3 相容、依版本的鍵、公開讀取、上傳只給維護者、資料不能有客戶資料與位址）。bucket 本身在 Laysi/trustiot-v2#133 選定，在那之前不上傳任何 lab 資料集。

待測：

- 真的設備與轉換器上的錄製：Laysi/trustiot-v2#680。
- 長時間錄製與大量批次的載入（`TOO_MANY_PARTS` 的實際界線）：本階段只記錄限制，沒有量測。
