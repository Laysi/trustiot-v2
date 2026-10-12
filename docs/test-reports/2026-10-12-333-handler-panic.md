# outbox 消費端 handler panic 與 lag 指標（Laysi/trustiot-v2#333）

驗收 Laysi/trustiot-v2#333：handler panic 不再讓 `Consumer::run` 結束，lag 指標也不再停在舊值。問題來自 Laysi/trustiot-v2#271 獨立驗證的 F-5（[報告](2026-10-11-271-verify.md)），做法見 [`crates/outbox/README.md`](../../crates/outbox/README.md) 的「Handler panic」與「Lag 指標」兩節。

## 1. 環境

| 項目 | 內容 |
| --- | --- |
| 機器 A | 共用檢查佇列的遠端建置主機：x86_64 Linux（WSL2），Docker；`CI=1`，沒有容器執行環境時測試失敗、不略過 |
| 機器 B | 本機檢查佇列：Fedora，Linux 7.2.8-300.fc45.x86_64，x86_64，podman 6.1.3；`CI=1` |
| 工具 | rustc 1.99.0（`.prototools`、`rust-toolchain.toml`） |
| Postgres | `postgres:18.6-alpine@sha256:77f58511…`，每個測試一個用完即刪、以測試時產生的 CA 啟用 TLS 的容器；消費端以 service-kit 的 pool 連線（`verify-full`、session 檢查） |
| 受測 commit | `fix/333-consumer-panic`：實作 `8383e49`；最終檢查在合併 Laysi/trustiot-v2#271 分支（`2620d08`，含 main `d42a470`）之後的 `af69339`，其樹與合併 main `b4aae40` 後的 `4329e0c` 相同 |

## 2. 指令

從 repo 根目錄執行，一次佇列工作：

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
cargo test -p sql-check --locked
cargo run -p openapi --locked -- --check
for i in 1 2 3; do cargo test -p outbox --locked --no-fail-fast; done
```

機器 B 另外執行：

```
cargo test -p outbox --locked --test verify_271 --test handler_panic
```

## 3. 結果

### 3.1 新增與改動的測試

| 測試 | 內容 | 結果 |
| --- | --- | --- |
| `verify_271::f5_a_handler_panic_does_not_stop_the_consumer`（不再 `#[ignore]`） | handler 對第一個事件 panic 一次，以 `run` 執行 | `run` 沒有結束，事件處理 1 次，lag 回報 0；`stop` 後 `run` 回 `Ok`，`lag()` 為 `None` |
| `handler_panic::a_panic_is_retried_and_the_event_then_processed_once` | `handle`（寫入後）與 `begin` 各 panic 一次 | 第一次 `poll_once` 回 `Outcome::Retry`（`handler panicked`），位置不動、沒有寫入；之後兩個事件依序各處理一次，沒有死信 |
| `handler_panic::an_event_that_keeps_panicking_is_dead_lettered_and_the_rest_processed` | handler 對事件 1 每次都 panic | 前 2 次重試，第 3 次記進死信（`handler panicked repeatedly`），同一輪接著處理事件 2、3；handler 被呼叫正好 3 次 |
| `handler_panic::a_hook_that_ends_the_transaction_and_then_panics_is_dead_lettered` | `handle` 或 `begin` 自己 `COMMIT` 後 panic，`begin` 自己 `COMMIT` 後回錯誤 | 三種都直接記進死信（`handler ended the delivery's transaction`），不重試；`COMMIT` 前的寫入留著，下一個事件只處理一次 |
| `handler_panic::run_survives_panics_and_keeps_the_lag_current` | 一直 panic 的事件，以 `run` 執行 | 事件進死信，其他事件處理完，lag 回報 0，`run` 仍在執行；`stop` 後 `lag()` 為 `None` |
| `handler_panic::a_panic_mid_query_leaves_the_session_clean` | 每個 pool 只有 1 條連線；handler 寫入後送出 `pg_sleep(1)`，在等待回應時 panic | 事件 1 重試後在租戶 A 處理一次，全平台事件 2 在**同一個 backend**、沒有租戶下處理；panic 那次的寫入沒有留下 |
| `handler_panic::the_lag_rises_while_processing_is_stuck_and_stops_when_run_is_dropped` | handler 對事件 1 卡住（非同步等待） | lag 回報 2，age 持續上升；放行後回到 0；abort `run` 的 task 後 `lag()` 變成 `None` |
| `handler_panic::a_stuck_lag_measurement_is_not_reported` | 以 superuser 對產生端 `outbox_event` 取 `ACCESS EXCLUSIVE` 鎖，讓量測查詢卡住 | 超過 3 個 `lag_interval` 後 `lag()` 為 `None`（不回報舊值），`run` 仍在；解鎖後恢復回報 0 |
| `lag::tests`（單元測試） | 指標 callback 讀到的值 | 量測迴圈結束（drop guard）或量測過期後，gauge callback 不再回報該訂閱 |
| `verify_271::a_failed_delivery_leaves_nothing_and_the_event_is_processed_once`（未改） | panic 那一列 | 原本第一次嘗試是 task panic，現在是 `Outcome::Retry`；斷言（不留寫入、位置不動、之後只處理一次、同一 session 無殘留租戶）照舊通過 |

### 3.2 全部檢查（機器 A，2026-10-12 11:30–11:37）

在 `af69339` 的工作樹執行；之後合併 main（`b4aae40`，Laysi/trustiot-v2#344 的 squash）得到 `4329e0c`，合併前後的樹相同（`git diff --cached HEAD` 為空），所以結果適用於 `4329e0c`。

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通過，沒有警告 |
| `cargo test --workspace --locked --no-fail-fast` | 78 個測試執行檔，609 個通過，1 個失敗：`verify_271::a_database_restart_mid_run_loses_and_duplicates_nothing`（見 3.3） |
| `cargo test -p sql-check --locked` | 通過 |
| `cargo run -p openapi --locked -- --check` | `api/openapi.json is up to date` |
| `cargo test -p outbox --locked --no-fail-fast` 連跑 3 次 | `handler_panic` 3 次都是 7/7；`f5_` 3 次都通過；`verify_271` 每次只有 `a_database_restart…` 失敗（見 3.3）；第 2 次另有 `delivery::services_need_only_the_documented_grants` 的 Postgres 容器 90 秒內沒有啟動（見 3.3） |

機器 B（podman，`8383e49` 的樹，與 `4329e0c` 的 outbox 相同）：`handler_panic` 7/7，`verify_271` 13/13，`a_database_restart…` 也通過。

### 3.3 已知與本次無關的失敗

- `verify_271::a_database_restart_mid_run_loses_and_duplicates_nothing`（Laysi/trustiot-v2#271 的測試）在機器 A 固定失敗：`no container publishes 127.0.0.1:<port>->5432`。測試以發布的主機埠找容器、重啟後再以同一個埠找一次；Docker 重啟容器時會重新分配臨時的主機埠，podman 不會。在機器 B（podman）同一個測試通過（`verify_271` 13 個全過，`handler_panic` 7 個全過）。CI 的 runner 先找 podman，所以不受影響。建議另開 issue 修正測試，例如容器改為發布固定埠，或重啟後重新讀取埠並重建 pool。
- 機器 A 負載高時，偶爾有測試的 Postgres 容器 90 秒內沒有啟動（`support/postgres.rs` 的 `Postgres with TLS did not start in time`；2026-10-12 的三次批次中各出現一次，分別在 `notify`、`delivery` 的不同測試），發生在測試開始前的容器啟動，與本次改動無關。

## 4. 結論

| 驗收條件 | 結果 |
| --- | --- |
| Handler panic 後，消費端繼續處理後續事件；panic 的事件依規則重試或進死信（真 Postgres 測試） | 通過：3.1 的 `a_panic_is_retried…`、`an_event_that_keeps_panicking…`、`a_hook_that_ends_the_transaction…`、`run_survives_panics…`、`a_panic_mid_query…` |
| 消費端因任何原因停止時，lag 指標或 readiness 會反映出來，不會停在舊值（測試） | 通過：處理卡住時 lag 與 age 上升；`run` 停止（`stop`）、被 drop（task abort）、量測卡住時都不再回報舊值（`run_survives_panics…`、`the_lag_rises…`、`a_stuck_lag_measurement…`、`lag::tests`）。readiness 刻意不改，理由見 README |
| `f5_a_handler_panic_does_not_stop_the_consumer` 不再 ignore 並通過；README 已更新 | 通過：機器 A 三次、機器 B 一次都通過；README 新增「Handler panic」一節並更新失敗處理、Lag 指標、測試表 |

未涵蓋（待測）：handler 同步卡住 worker 執行緒時，lag 量測 task 是否能在其他 worker 上繼續執行，取決於 tokio 的排程（LIFO slot 的 task 不會被其他 worker 偷走），沒有寫成測試；文件只保證量測過期後不再回報（`a_stuck_lag_measurement…` 驗證了這條路徑）。
