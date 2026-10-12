# outbox：失敗路徑明確回滾、`begin` 的交易檢查（2026-10-12）

對應 issue：Laysi/trustiot-v2#350

## 環境

- 建置主機（check queue）：x86_64 Linux（WSL2），20 核心，Docker；每個測試起一個用完即刪的 `postgres:18.6-alpine`（TLS，`verify-full`）容器。指令經 check queue 執行，環境變數 `CI=1`（沒有容器執行環境時測試失敗而不是略過）。
- Rust 依 `rust-toolchain.toml`；sqlx 0.9.0、tokio 1.53.2（`Cargo.lock`）。
- 分支 `fix/350-outbox-explicit-rollback`，基於 main `a7f332b`（含 Laysi/trustiot-v2#333 與 Laysi/trustiot-v2#271）。「舊程式」指 `a7f332b` 的 `crates/outbox/src/consumer.rs`，搭配本分支的新測試。

## 指令

```sh
# 舊程式 + 新測試（預期失敗，證明測試抓得到問題）；在 check queue 的副本裡換回舊檔再跑兩次
cp old-consumer.rs crates/outbox/src/consumer.rs   # 內容為 git show a7f332b:crates/outbox/src/consumer.rs
cargo test --locked -p outbox --test failure_paths

# 新程式
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
for i in 1 2 3; do cargo test --locked -p outbox --test failure_paths; done
cargo test --locked -p outbox --no-fail-fast
cargo test --locked -p sql-check
cargo test --locked -p repo-check
cargo run --locked -p openapi -- --check
```

## 結果

原始輸出（已去掉 `Compiling` 行與建置主機的路徑）：[old-vs-new.log](data/2026-10-12-350-explicit-rollback/old-vs-new.log)、[sql-check-rerun.log](data/2026-10-12-350-explicit-rollback/sql-check-rerun.log)。

| 執行 | `the_probe_sees_…` | `a_failed_delivery_is_rolled_back_…` | `a_begin_hook_that_breaks_…` |
| --- | --- | --- | --- |
| 舊程式，第 1 次 | ok | FAILED：`in_begin false, Permanent`，探針看到失敗投遞的 backend 還持有 2 個鎖（`[2]`，交易的 virtualxid 與訂閱的 advisory lock） | FAILED：`duplicate true, Commit`，`begin` 自己 `COMMIT` 的重複投遞被算成 `duplicates: 1`，沒有記進死信 |
| 舊程式，第 2 次 | ok | FAILED（同上，`[2]`） | FAILED（同上） |
| 新程式，第 1–3 次 | ok | ok | ok |

- 新程式：`cargo test -p outbox` 除了下面一項全部通過（lib 10、crash 2、delivery 13、failure_paths 3、handler_panic 7、metrics 1、notify 2、producer 3、tenancy 5、verify_271 12/13）。
- `verify_271::a_database_restart_mid_run_loses_and_duplicates_nothing` 在 Docker 建置主機失敗（測試在容器重啟後找不到新的 host port），與本變更無關，由 Laysi/trustiot-v2#348（PR Laysi/trustiot-v2#352）修正；本分支合併 main 後重跑的結果見 PR。
- 第一次批次中 `sql-check` 的 `the_workspace_has_no_string_built_sql` 失敗：新測試檔以 `set_config` 改租戶（模擬寫錯的 handler）。在 `tools/sql-check` 的 `EXCEPTIONS` 為該檔只免除 `SessionSetting`（與 `verify_271.rs`、`tenant_db.rs` 相同做法）後，`sql-check` 26 個測試、`repo-check` 全部通過，fmt 與 clippy 無警告，`api/openapi.json` 沒有變動。

### 測試怎麼做到與時序無關

`a_failed_delivery_is_rolled_back_before_the_consumer_goes_on` 讓 handler 在 `begin` 或 `handle` 失敗前，把自己的 backend PID 交給探針。消費端失敗後下一次從 pool 取連線（記死信的交易，或重試後下一輪的第一個查詢）時，pool 的 `before_acquire` 掛勾從另一個執行緒、另一個 runtime、另一條連線查 `pg_locks`，看那個 backend 是否還持有 `virtualxid`（交易還開著）或 `advisory` 鎖。查詢期間掛勾刻意卡住測試的單執行緒 runtime，sqlx 在 drop 之後負責送出 `ROLLBACK` 的歸還 task 不可能執行；pool 關掉 `test_before_acquire`，所以從交出閒置連線到掛勾之間也沒有任何會讓出執行權的 I/O。因此：

- 只 drop 交易的舊程式，探針每次都會看到交易與鎖還在（上表兩次都是 `[2]`）。
- 明確 `rollback().await` 的新程式，伺服器回覆 `ROLLBACK` 時交易已結束、鎖已釋放，探針每次都看到 `[0]`。
- `the_probe_sees_an_open_transaction_and_its_advisory_lock` 是探針本身的對照：開著、拿了 advisory lock 的交易看得到 2 個鎖，回滾後是 0。

同一個測試另外檢查四種情況（`begin`／`handle` × 永久／可重試）的 handler 呼叫次數剛好是預期（永久失敗 1 次，重試 2 次）、沒有任何一輪回報 `Busy`，以及死信與寫入的結果。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 所有失敗路徑都在記死信之前明確回滾；有測試證明 handler 不會因為 lock 競爭被多呼叫一次（真 Postgres） | 通過。`Consumer::deliver` 只在 commit 與 `roll_back`（`rollback().await`）兩處結束投遞交易；`failure_paths` 在真 Postgres 上證明下一筆交易開始前鎖已釋放、呼叫次數剛好，舊程式兩次都失敗、新程式連續三次通過 |
| 重複投遞的路徑會檢查 `begin` 的交易，或文件寫明不需要的理由；有測試 | 通過。改為 `begin` 回 `Ok` 後立即檢查（在取得訂閱的鎖之前），重複投遞與第一次投遞都涵蓋；`a_begin_hook_that_breaks_the_transaction_is_caught_on_every_path` 驗證 `COMMIT` 與改租戶兩種破壞都進死信、`handle` 不在被破壞的交易執行；舊程式失敗、新程式通過 |
| `cargo test -p outbox` 通過，README 已更新 | 通過（除已知、由 Laysi/trustiot-v2#348 處理的 Docker 重啟測試）；`crates/outbox/README.md` 的失敗處理表、交易邊界表、測試表與新增的「明確回滾」一節已更新 |
