# 閘道器 agent 的取樣排程與自我恢復（2026-10-22）

Laysi/trustiot-v2#89：`edge/agent` 的每台設備由一個輪詢迴圈負責（`polling::run`，rebuild checklist F4）：固定節拍、每次等待都有上限、重連退避只在讀到值後歸零、每條連線開 TCP keepalive，以及每台設備的 watchdog。規則見 `edge/agent/README.md`「排程與自我恢復」。

## 環境

- **程式碼**：分支 `feat/89-sampling-watchdog`，基於 main `6a0938e`，commit `b6157e7`（功能）與 `7012870`（clippy 修正）。下列結果是在建置主機上、`7012870` 的內容（`b6157e7` 加上同一處修正，尚未 commit 時）執行的。
- **工具版本**：依 `rust-toolchain.toml`（Rust 1.99.0）與 `Cargo.lock`（tokio 1.53.2、socket2 0.6.5，`all` feature 提供 keepalive 的間隔與次數）。
- **機器**：建置主機（x86_64），經 check queue 執行，`CI=1`。aarch64 只做交叉建置，沒有在閘道器上執行。
- 不需要容器、叢集或 lab 的硬體：testkit 的假 Modbus 端點與輪詢迴圈都在測試行程內，只用 127.0.0.1。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked -p agent --all-targets -- -D warnings
cargo test --locked -p agent
cargo build --locked --target aarch64-unknown-linux-gnu -p agent
```

## 結果

四個指令都以 exit 0 結束；clippy 沒有警告。第一次執行（同一天稍早）clippy 以 `unnecessary_lazy_evaluations` 擋下 `polling.rs` 的一處 `unwrap_or_else`，`7012870` 改成 `unwrap_or` 後重跑，即本節的結果。

### `cargo test -p agent`：96 通過、0 失敗

單元測試 62 個、`tests/modbus_fake_endpoint.rs` 9 個、`tests/modbus_rtu_converter.rs` 8 個、`tests/modbus_rtu_framing.rs` 6 個、`tests/modbus_tcp_framing.rs` 8 個、`tests/polling_recovery.rs` 3 個（新）。整合測試都用 `#[tokio::test(start_paused = true)]` 加 `testkit::clock::manual()`：時鐘只在測試推進時前進，逾時與 watchdog 依序觸發，不因機器慢而觸發；等待都經過 `testkit::clock::bounded` 或假端點的 `wait_for_*`，不依賴固定的短時間。

| 項目 | 測試 | 驗證的內容 |
| --- | --- | --- |
| 驗收條件 1：一直連著卻不回應 | `polling_recovery::the_watchdog_recovers_a_device_that_stays_connected_but_never_answers` | 假端點注入 `Fault::no_response()`（接受連線、收下請求、永不回答），watchdog 設 30 秒、請求逾時 5 秒、間隔 1 秒：前 5 次輪詢各以 `timeout` 結束，每次在前一次逾時時開始；第 6 次在第一個沒有回答的請求之後剛好 30 秒觸發 watchdog（`silent_since` 等於開始時間），以 `connect` 結束，說明為「no answer for 30000 ms: the watchdog closed the connection」；退避 1 秒後的第一次輪詢在新連線（`ConnectionId(1)`）上讀到值 2301；第一條連線收到 6 個請求後被關閉，只送功能碼 3 |
| 驗收條件 2：半開連線 | `polling_recovery::a_half_open_connection_is_detected_and_replaced` | 假端點注入 `Fault::half_open()`（設備忘了連線，不送 FIN 或 RST，寫入照樣成功）：與上一列相同的時序，watchdog 在 30 秒時關掉連線，新連線讀到值；第一條連線的狀態是 `Forgotten` |
| 驗收條件 2：keepalive | `modbus::tcp::tests::connections_use_tcp_keepalive` | 每條連線都開 `SO_KEEPALIVE`，閒置 30 秒、間隔 10 秒、3 次（讀回 socket 選項），並開 `TCP_NODELAY` |
| 驗收條件 3：一小時的節拍漂移 | `polling::tests::the_schedule_does_not_drift_over_an_hour` | 暫停的 tokio 時鐘上模擬一小時、間隔 1 秒，輪詢時間輪流為 130、470、999、1000、1500、20、2600、0、640 ms（有的超過間隔）：每次輪詢的預定時間都是開始時間加整數個間隔；實際開始比預定晚不超過 1 ms（設定的上限，tokio 計時器的解析度，不隨輪詢次數增加）；輪詢不重疊；每個節拍不是執行就是記為跳過（`missed`），涵蓋至少 3600 個節拍；超過間隔的輪詢確實跳過了節拍 |
| 驗收條件 4：只有讀到值才歸零退避 | `polling_recovery::only_a_poll_that_read_values_resets_the_backoff` | 假端點每次接受連線後 reset（連線成功仍算失敗）：六次輪詢的（開始秒數、連續失敗次數、之後的退避秒數）依序為 (0, 1, 1)、(1, 2, 2)、(3, 2, 0)、(4, 3, 4)、(8, 0, 0)、(9, 1, 1)。第 3 次是例外回應（設備回答了、沒有值）：不歸零也不加長；第 5 次讀到值：歸零；之後再失敗從 1 秒重新開始。假端點接受了全部 6 條連線 |
| 退避的上限 | `polling::tests::the_backoff_doubles_up_to_its_maximum` | 退避 1 秒到 60 秒時依序為 1、2、4、8、16、32、60、60 秒；歸零後回到 1 秒；連續失敗次數極大時不溢位 |
| 節拍的計算 | `polling::tests::due_times_are_whole_periods_from_the_start` | 間隔 1.5 秒：同一時間再問得到下一個節拍、不重複；4.2 秒結束的輪詢之後是 4.5 秒，跳過 3 秒的節拍；剛好在節拍上（6 秒）結束就立刻執行 |
| 設定 | `polling::tests::settings_have_defaults_and_ranges` | 預設值（請求 5 秒、連線 10 秒、watchdog 60 秒、退避 1 秒到 60 秒）與每個設定的範圍；超出範圍的錯誤寫出設定名稱、值與範圍；watchdog 的下限是 `connect_timeout + 2 × request_timeout` |

### aarch64 交叉建置

`cargo build --locked --target aarch64-unknown-linux-gnu -p agent` 成功（socket2 的 keepalive 選項在 Linux aarch64 上可用）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 設備一直連著卻不回應時，watchdog 在設定時間內恢復輪詢（故障注入測試） | 通過：watchdog 在第一個沒有回答的請求後剛好 30 秒（設定值）關掉連線，退避後的第一次輪詢在新連線上讀到值 |
| 半開連線被偵測並重連（測試） | 通過：半開連線由 watchdog 偵測並換新連線；每條連線開 TCP keepalive（設定值以測試讀回）。kernel 在真正的閘道器上送出探測並關掉閒置的死連線，屬於跨環境的重複驗證，2026-10-22 延後到 Laysi/trustiot-v2#512 |
| 模擬一小時內節拍漂移小於設定值（測試） | 通過：一小時內每次輪詢比預定時間晚不超過 1 ms，不累積 |
| 只有成功讀到資料才重置退避（測試） | 通過：連線成功、例外回應都不歸零退避，只有讀到值的輪詢歸零 |
