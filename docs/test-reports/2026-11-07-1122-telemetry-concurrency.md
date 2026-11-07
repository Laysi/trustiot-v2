# telemetry 工具同時的查詢數

Laysi/trustiot-v2#1122：一步超過 4 個 telemetry 工具呼叫時，超過租戶 ClickHouse 帳號同時查詢數的呼叫不再記成 `failed`；同時查詢數滿了不再叫模型這次提問不要再查。設計與理由在 [docs/architecture/ai-assistant.md](../architecture/ai-assistant.md) 工具的設計規則 7、[services/telemetry-query](../../services/telemetry-query/README.md)「同時的查詢」與 [services/assistant](../../services/assistant/README.md)「工具」。

## 環境

- 建置主機（arm64 Linux）；Rust 與工具版本依 `.prototools` 與 `rust-toolchain.toml`。
- 分支 `fix/1122-telemetry-concurrency`，從 main `5381d35a` 開出。
- ClickHouse 實測：`docker.io/clickhouse/clickhouse-server:26.8@sha256:110d3d1f8e1ed7e451a6b5d85a16761e948ad43bc2d9ddf10b155964a8b2b40c`（`SELECT version()` 是 26.8.22.13），用完即刪的容器；帳號與密碼只在容器內，測完連同容器刪除。

## 指令

1. 同時查詢數的拒絕怎麼算 quota（[measure.sh](data/2026-11-07-1122-telemetry-concurrency/measure.sh)）：建立一個 `readonly = 1 CONST`、`max_concurrent_queries_for_user = 1 CONST` 的帳號與每小時 1000 個查詢、1000 個錯誤的 quota，以 HTTP 送一個成功的 `SELECT 1`，再以 `SELECT sleep(3)` 佔住唯一的名額時送第二個 `SELECT 1`，讀 `system.quotas_usage` 與 `system.query_log`：

   ```sh
   bash docs/test-reports/data/2026-11-07-1122-telemetry-concurrency/measure.sh
   ```

2. 驗收條件 1、2 的自動測試（不需要容器）：

   ```sh
   cargo test --locked -p assistant --test telemetry_concurrency -- --nocapture
   cargo test --locked -p assistant --test telemetry_sql
   cargo test --locked -p telemetry-query --lib
   ```

3. 專案規定的檢查：`cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo test --locked --workspace --exclude agent`、`cargo run -p openapi -- --check`、`bun run --cwd frontend api:check`、`bun run --cwd frontend typecheck`、`bun test scripts/ci/changes.test.ts`。

## 結果

### 同時查詢數的拒絕與 quota

原始輸出：[measure.out](data/2026-11-07-1122-telemetry-concurrency/measure.out)。

| 步驟 | HTTP | `quotas_usage` 的 queries | errors |
| --- | --- | --- | --- |
| 開始 | | 0 | 0 |
| 一個成功的 `SELECT 1` | 200 | 0 | 0 |
| 名額被 `sleep(3)` 佔住時的 `SELECT 1` | 500（代碼 202） | 0 | 1 |

`query_log` 記被拒的查詢為 `ExceptionBeforeStart`、`exception_code` 202。不讀表的查詢不算 quota 的查詢（`queries` 一直是 0），但被同時查詢數拒絕時算一個錯誤。所以 telemetry-query 對 202 的重試只做 2 次（一個語句最多花 3 個錯誤，帳號每分鐘 60 個），主要靠每個租戶的空位不讓查詢超過帳號的上限；請求在查詢中被取消時空位延後 35 秒才放回（`slots::LINGER`），因為 ClickHouse 可能還在跑那個查詢，下一個請求會被 202 拒絕、花掉錯誤額度（`telemetry-query` 的單元測試 `a_slot_whose_work_was_cancelled_stays_taken_for_the_linger` 以暫停的時鐘確認）。

### 一步 8 個 telemetry 呼叫（驗收條件 1）

`tests/telemetry_concurrency.rs` 的替身 telemetry-query 像修改前的服務一樣，同時已有 4 個請求時以 429（沒說上限）拒絕下一個，並把接受的請求留到同時有 3 個或整步的呼叫都到了才回答：

| 測試 | 呼叫 | 同時最多 | 被拒 | 每個呼叫的 `outcome` |
| --- | --- | --- | --- | --- |
| `a_step_of_more_telemetry_calls_than_the_account_runs_at_once_has_no_failed_call`（租戶的空位） | 8（`telemetry_latest` 與 `telemetry_sql` 交錯） | 3 | 0 | 8 個 `ok` |
| `without_slots_the_same_step_has_calls_refused_as_before`（對照組：沒有空位的 client） | 8 | 8 | 4 | 4 個 `ok`，`telemetry_latest` 被拒的是 `failed`，`telemetry_sql` 被拒的是 `input_error` 與「Do not call telemetry_sql again in this question」 |

對照組證明替身會像以前一樣拒絕，所以第一個測試的 0 個被拒來自空位。

### 429 的上限給模型的文字（驗收條件 2）

`a_concurrency_refusal_asks_to_call_again_and_only_the_quota_says_stop`，兩個工具（GET 的 `telemetry_latest`、POST 的 `telemetry_sql`）都測：

| telemetry-query 的 429 | `outcome` | 模型拿到的文字 |
| --- | --- | --- |
| `details.limit` `concurrency` | `busy` | The service is busy with other queries of the user's organisation, so this call did not run; nothing is wrong with the input. Call the tool again with the same input in the next step, or get the data in fewer calls. |
| `details.limit` `quota` | `limit_used_up` | The organisation's query limit at this service is used up for now. Do not call this tool, or other tools that read the same data, again in this question; answer with what you have and say which data could not be read. |
| 不認得的上限 | 修改前的行為：`telemetry_latest` 是 `failed`，`telemetry_sql` 是 `input_error`（它原本對 429 的說明） | 固定訊息，不含本文 |

`concurrency` 的文字沒有「Do not call」。本文裡的標記沒有出現在模型的文字或日誌。儀表板的資料集執行對 `concurrency` 回 429 `dashboard.runs_limited`（`Retry-After: 5`），`quota` 與沒說上限的仍是 `dashboard.query_quota_exhausted`（`dashboards::runner` 的單元測試）。

### 專案規定的檢查

在建置主機跑，全部通過；數字見 PR 說明。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 一步超過 4 個 telemetry 工具呼叫時，沒有呼叫因同時查詢數被記成 `failed`（自動測試） | 通過：上表 8 個呼叫都是 `ok`，對照組證明替身會拒絕 |
| 碰到同時查詢數（不是額度）時，不叫模型這次提問不要再查（自動測試） | 通過：`concurrency` 是 `busy`、要模型下一步再呼叫，只有 `quota` 叫模型停止 |
| 做法與理由寫進 `docs/architecture/ai-assistant.md` 或對應的 README | 通過：工具的設計規則 7、兩個服務的 README |
| 專案規定的測試與 CI 通過 | 本機的檢查通過；CI 待 PR 跑完 |

待測（延後的進階驗證）：對真的 ClickHouse 同時跑多個問題的壓力測試與重試率量測、在 demo 上以真的模型重跑（Laysi/trustiot-v2#459 的預演）。
