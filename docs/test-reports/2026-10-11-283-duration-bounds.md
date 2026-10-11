# service-kit 時間設定上限（Laysi/trustiot-v2#283）

來源：Laysi/trustiot-v2#51 獨立驗證（PR Laysi/trustiot-v2#280）的 F-1：`shutdown.drain_timeout` 設成 `500000000000y` 時，啟動驗證通過，SIGTERM 後 `Instant + Duration` 溢位 panic（exit 101）；`shutdown.drain_delay` 設成同樣的值則永遠不退出。

## 1. 環境

- 機器：build host `ruisi-wsl`（Ubuntu on WSL2、Docker、`CI=1`），經 `rcheck.sh 283` 執行；git 與 commit 在本機。
- 分支 `fix/283-shutdown-duration-bounds`，基底 origin/main e8f180b；工具版本依 `.prototools`。
- 變更：`crates/service-kit/src/shutdown.rs`（`MAX_DRAIN`、`ShutdownConfig::validate`）、`crates/service-kit/src/http.rs`（`MAX_REQUEST_TIMEOUT`、`HttpConfig::validate`）、`crates/service-kit/README.md`、測試。

## 2. 指令

```
bash rcheck.sh 283 'cargo fmt --all --check && cargo clippy --locked -p service-kit --all-targets -- -D warnings'
bash rcheck.sh 283 'cargo test --locked -p service-kit'
bash rcheck.sh 283 'cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings; cargo test --locked -p service-kit --no-default-features --lib --test lifecycle'
bash rcheck.sh 283 'cargo clippy --locked --workspace --all-targets -- -D warnings'
bash rcheck.sh 283 'cargo test --locked -p service-kit --lib -- capped limit_not_the_value positive'
bash rcheck.sh 283 'cargo test --locked -p service-kit --test lifecycle -- durations'
```

## 3. 結果

上限（含剛好等於上限）：

| 設定 | 上限 | 超過時 |
| --- | --- | --- |
| `shutdown.drain_delay` | 1 小時 | 啟動被拒，key `shutdown.drain_delay` |
| `shutdown.drain_timeout` | 1 小時 | 啟動被拒，key `shutdown.drain_timeout` |
| `drain_delay + drain_timeout` | 1 小時 | 啟動被拒，key `shutdown.drain_timeout`，訊息同時點名兩個設定 |
| `http.request_timeout` | 1 小時 | 啟動被拒，key `http.request_timeout` |

錯誤訊息只寫設定名稱（載入時另帶環境變數名）與上限，不寫設定值；單元測試以 `Duration::MAX` 驗證檢查不做會溢位的運算（每個值先比上限，再相加）。

新測試：

- `shutdown::tests::each_drain_time_is_capped_at_one_hour`、`the_sum_of_both_is_capped_at_one_hour`、`messages_name_the_limit_not_the_value`
- `http::tests::request_timeout_is_capped_at_one_hour`
- `tests/lifecycle.rs::durations_beyond_the_caps_stop_startup`：真實的 example-service 行程，`500000000000y` 與 `3601s` 對三個設定、以及 2400s + 2400s 的合計，皆 exit 1，輸出含設定名稱與環境變數名、不含設定值、沒有 `listening`
- `tests/lifecycle.rs::durations_at_the_caps_still_start_and_drain_on_sigterm`：`drain_timeout` 與 `request_timeout` 剛好 1 小時可以啟動，SIGTERM 後 exit 0

完整輸出：[tests.txt](data/2026-10-11-283-duration-bounds/tests.txt)。service-kit 全部測試（含容器測試）、`--no-default-features`、workspace clippy 皆通過。既有的關閉測試（`drain_timeout_bounds_shutdown`、`second_signal_exits_without_draining` 等）與 chart 契約測試不變且通過。

### 時間類設定盤點

| 項目 | 種類 | 溢位風險 | 處理 |
| --- | --- | --- | --- |
| `shutdown.drain_delay` | 設定（humantime） | 永遠不退出（F-1） | 上限 1 小時 |
| `shutdown.drain_timeout` | 設定（humantime） | `Instant + Duration` 溢位 panic（F-1） | 上限 1 小時 |
| `http.request_timeout` | 設定（humantime） | 不 panic（`tokio::time::timeout` 用 `checked_add`，溢位時等同沒有逾時），但極端值使逾時形同關閉（F-1 探測：exit 0） | 上限 1 小時 |
| `telemetry.*` | 設定 | 沒有時間欄位 | 不需要 |
| `identity` 的 token 壽命與 leeway | 常數 `MAX_LIFETIME_SECS`、`LEEWAY_SECS`；token 內的整數用 `checked_sub` 與上限檢查 | 無 | 不需要 |
| `TELEMETRY_FLUSH`、`HALT_GRACE`、`CHECK_TIMEOUT`、`RESULT_TTL` | 程式內常數 | 無（固定值） | 不需要 |
| `JobLock::with_heartbeat`、`run_periodically(period)` | 程式參數（不是設定檔或環境變數），在 `job_lock.rs` | `tokio::time::interval` 用 `checked_add`，不 panic；`period` 為零時 `interval` 會 panic | 不在本 issue 範圍（job lock 由 Laysi/trustiot-v2#282、#284 處理）；沒有改動 |
| `config::duration`（給服務自己的欄位用） | 共用 deserializer | 不設上限 | 刻意不設（上限因欄位而異）；README 要求服務在 `Validate` 自設上限 |
| `outbox-example` 的 `process_after`、`hold`、`poll_interval`、`lag_interval` | outbox crate 示範程式的設定，用 `config::duration` | 未盤點行為 | 不在 service-kit，沒有改動；列為後續建議 |

## 4. 結論

- 超過上限的 `drain_delay`、`drain_timeout` 在啟動時被拒，錯誤訊息不含設定值：通過（單元測試與行程層級測試）。
- 上限以內的值與預設值行為不變，既有的關閉測試全部通過：通過。
- 時間類設定的盤點與處理寫在測試報告：通過（上表）。
- README 寫明各上限：通過（`crates/service-kit/README.md` 的設定表與說明段）。
