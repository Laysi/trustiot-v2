# 語意層工具：最新值、統計與用量

對應 issue：Laysi/trustiot-v2#142（語意查詢 API `POST /api/telemetry/v1/statistics`、`POST /api/telemetry/v1/usage` 與 AI 助理的 `telemetry_statistics`、`telemetry_usage` 工具；`telemetry_latest` 不變）。測試日期 2026-10-26（Asia/Taipei）。原始輸出在 [data/2026-10-26-142-semantic-tools/](data/2026-10-26-142-semantic-tools/)。

## 環境

- **受測程式碼**：分支 `feat/semantic-tools`（由 `origin/main` 的 `8c74384` 開出）。服務在 commit `5aa96ae`，工具與文件在 `4461bef`；本報告的 commit 另外在 `tests/semantic.rs` 加上最小與最大的時間、平均最高與最低的區間、最新值的原始資料期間三項比對，下面的輸出是加上之後跑的。
- **執行的地方**：開發工作站，容器由測試偵測 docker 啟動，一次只跑一個起容器的測試程式；沒有設 `CI`、沒有設 `TEST_CLICKHOUSE_URL`。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2、cargo-deny 0.20.2、gitleaks 8.30.1。
- **測試用的伺服器**：拋棄式容器 `docker.io/clickhouse/clickhouse-server:26.8@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c`（與 chart 相同的 digest）與 `docker.io/library/postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873`（TLS，產品 schema 由 migration Job 套用）。
- **資料**：`datasets/lab/synthetic`（Laysi/trustiot-v2#679 的合成 fixture，version 1），以 `lab-dataset` 照 ingest 的寫法載入；另以 `TRUSTIOT_LAB_DATASET_INDEX` 指向同一份 fixture 的複本跑一次，確認換成 lab 資料集（Laysi/trustiot-v2#681）時測試不必改。

## 指令

在 repo 根目錄：

```sh
# 語意查詢的驗收（--nocapture 印出每個比對）
cargo test -p telemetry-query --test semantic -- --nocapture
TRUSTIOT_LAB_DATASET_INDEX=<synthetic 目錄複本裡的 index.toml> cargo test -p telemetry-query --test semantic -- --nocapture

# 受影響的 crate 全部測試（一次一個起容器的程式）
cargo test -p telemetry-query
cargo test -p assistant
cargo test -p openapi
cargo test -p repo-check
cargo test -p sql-check

# 規格、相依、repo 規則
cargo run -p openapi -- --check
cargo deny --locked check
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# 前端（新的 message key 與 API client）
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test

# CI 的變更判斷
bun install --cwd scripts --frozen-lockfile
bun test scripts/ci/changes.test.ts
```

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo test -p telemetry-query` | 單元 37、`chart_connections` 1、`cross_tenant` 1、`history` 1、`semantic` 1、`tenant_accounts` 1，全部通過（[telemetry-query-all.txt](data/2026-10-26-142-semantic-tools/telemetry-query-all.txt)） |
| `cargo test -p telemetry-query --test semantic -- --nocapture` | 通過，每個比對一行（[telemetry-query-semantic.txt](data/2026-10-26-142-semantic-tools/telemetry-query-semantic.txt)） |
| 同上，設 `TRUSTIOT_LAB_DATASET_INDEX` | 通過；fixture 才有的檢查略過，其餘相同（[telemetry-query-semantic-index-variable.txt](data/2026-10-26-142-semantic-tools/telemetry-query-semantic-index-variable.txt)） |
| `cargo test -p assistant` | 單元 36、`agent` 10、`agent_logs` 2、`chart_connections` 1、`cross_tenant` 1、`delegated_tools` 13、`history_store` 5、`startup` 1、`tools` 7，全部通過（[assistant.txt](data/2026-10-26-142-semantic-tools/assistant.txt)） |
| `cargo test -p openapi`、`-p repo-check`、`-p sql-check` | 全部通過 |
| `cargo run -p openapi -- --check` | `api/openapi.json is up to date` |
| `cargo deny --locked check` | `advisories ok, bans ok, licenses ok, sources ok`；重複版本的警告與 main 相同（13 項），新加的 jiff 0.2.35、jiff-tzdb 0.1.9 是 Unlicense OR MIT |
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | 沒有輸出、沒有警告 |
| 前端 `api:check`、`typecheck`、`lint`、`test` | 218 個測試通過，其餘沒有錯誤 |
| `bun test scripts/ci/changes.test.ts` | 282 個測試通過；`datasets/` 與 `tools/` 本來就觸發 rust job，不必加規則 |

`tests/semantic.rs` 的比對（預期值都由測試從資料集的讀數逐格算出，不用服務的 SQL）：

| 項目 | 內容 | 結果 |
| --- | --- | --- |
| 最新值 | 三個設備每個資料點的最後讀數（含沒有完成紀錄的最後一批） | 相同 |
| 統計 | 7 個資料點；UTC、Asia/Taipei（1 小時表）與 Asia/Kolkata（1 分鐘表）；`15m`、`1h`、`1d`、`1w`、`1mo`、`total`；每個區間的筆數、最小、最大、平均、最後讀數，整段的筆數、平均、最小與最大（含時間）、第一與最後讀數、平均最高與最低的區間 | 全部相同 |
| 用量 | 同樣三個時區，`1h`、`1d`、`1w`、`1mo`、`total`、`15m`，另有從第二個小時開始、有 `baseline` 的範圍；每個區間的用量、讀數、下降、斷訊、可疑跳增，整段的總量、累計檢查、最長斷訊、最大的可疑跳增 | 全部相同 |
| fixture 的性質（1 分鐘表） | `energy_kwh` 164.046（1 次重置，檢查 1 下降、46 上升、47 比較）；`pulses` 3048（1 次 32 位元回捲，考慮回捲是 3099，差不到兩個讀數間隔）；`cycle_count` 172（漏讀的 35 分鐘是斷訊）；兩個 `power_kw` 是 `not_cumulative`（26/21/47、26/20/46），`running`、`mode` 是 `not_numeric`；沒有可疑跳增 | 符合 |
| fixture 的性質（1 小時表） | 同樣的總量；只有 7 次比較，三個計數器是 `too_few_readings`，`power_kw` 仍是 `not_cumulative`（6/1/7、5/2/7） | 符合 |
| 為規則寫的序列（每 5 分鐘一筆、15 小時） | 讀到 0 後回來：1 次可疑（1182），總量 1536；讀到 70000：1 分鐘表 2 次（68822 與回落後的 1182）、1 小時表 1 次，總量 70358；換成從 5000 開始的錶：1 次（3822），總量 4178；6 小時斷訊後以平常速率回來：0 次可疑、1 次斷訊；閒置在 0、跳三次 50：`not_cumulative`（3 下降、3 上升） | 符合，總量等於逐筆計算 |
| 可查期間 | 租戶 C 的資料集跨過可查期間開始：UTC 截在 1 小時表的開始、Asia/Kolkata 截在 1 分鐘表的開始（都在測試前後兩次讀時鐘算出的區間內），`truncated`、沒有 `baseline`、檢查視窗從期間開始；整段在之前時兩個 API 都回 403 與 `details.earliest`；只在 8 天前有讀數的資料點不在最新值裡，統計仍讀得到 | 符合 |
| 租戶 | 租戶 A 問租戶 B 的設備：兩個 API 每個資料點都是 `no_data`；租戶 B 讀得到自己的；本文帶 `tenant_id` 回 400；沒有 token 回 401 | 符合 |
| 注入 | 7 種惡意本文（資料點、設備 ID、時區、期間、區間、時間裡的 SQL 片段）在兩個 API 都回 400 或 422；以租戶 A 的帳號直接執行服務的語句：惡意的資料點比對不到任何列，惡意的時區是錯誤，在語句裡把租戶參數換成 B 也讀不到 B 的列 | 符合 |

AI 助理（`tests/delegated_tools.rs` 與單元測試）：兩個工具以使用者的 token POST，本文就是檢查過的輸入（替身以服務的 `SeriesQuery` 解析、拒絕未知欄位），請求裡沒有租戶或使用者；輸入帶 `tenant_id`（頂層或 `series` 裡）時不呼叫；工具的輸入 schema 與 `telemetry_query::openapi()` 的 `SeriesQuery` 的欄位、必填、上限、pattern 與 enum 相同；路徑與上限與服務的常數相同；列數超過服務上限時不呼叫；`not_cumulative` 的說明請模型改用 `telemetry_statistics`、`no_data` 說資料不足、`too_few_readings` 說未確認、可疑跳增要說明可能含誤讀；沒有任何資料點算得出用量時是錯誤結果；超過 500 列時只留各資料點的數字。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 以 Laysi/trustiot-v2#679 的合成 fixture 與標準答案測每個工具 | 通過：最新值、統計、用量都與從讀數算出的答案相同（上表）。用 lab 資料集再測一次依 issue 延後到 Laysi/trustiot-v2#681；設 `TRUSTIOT_LAB_DATASET_INDEX` 的一次確認只需換索引 |
| 用量拒絕不像累計值的資料點並請 AI 改用統計、讀數太少標示未確認、異常跳動有標示、歸零與溢位依「下降即從 0 重算」 | 通過：fixture 的 `power_kw` 與閒置的序列是 `not_cumulative`，工具說明請模型改用統計；1 小時表上 fixture 的計數器是 `too_few_readings`，工具說明數字未確認；三種誤讀被標成可疑跳增、總量照算；fixture 的重置與回捲各算一次下降、總量等於逐筆計算。判定規則的邊界在單元測試 |
| 注入測試：參數帶 SQL 片段時被當成值處理 | 通過（上表「注入」；工具的輸入檢查在 assistant 單元測試） |
| 原始資料與彙總資料超出各自可查期間的查詢被拒或截斷 | 通過：彙總資料截斷與 403（上表「可查期間」）；原始資料的最新值不列 8 天前的資料點；歷史 API 原有的測試不變 |
| 工具的租戶只取自內部身分，agent 參數帶別的租戶時讀不到 | 通過：API 拒絕租戶欄位、別的租戶的設備是 `no_data`、語句層也讀不到；工具不收租戶欄位、請求裡沒有租戶 |

待測（進階驗證，延後）：jiff 與 ClickHouse 的時區資料庫對表選擇的對照、1 分鐘表用量的記憶體量測（1,000,000 格上限）、lab 資料集（Laysi/trustiot-v2#681）。
