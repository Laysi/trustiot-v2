# service-kit：`log_filter` 的 target 指令與經 `log` crate 橋接的事件（Laysi/trustiot-v2#298）

驗收 Laysi/trustiot-v2#298：`telemetry.log_filter` 的 `target=level` 指令對經 `log` crate 橋接的事件（例如 `reqwest`）有效，原生 tracing 事件的過濾與遮蔽不變。問題來自 Laysi/trustiot-v2#285（PR Laysi/trustiot-v2#297）的後續提議；說明見 [`crates/service-kit/README.md`](../../crates/service-kit/README.md) 的 `telemetry.log_filter` 與「日誌與 trace」。

## 1. 環境

| 項目 | 內容 |
| --- | --- |
| 機器 | 共用檢查佇列的遠端建置主機：x86_64 Linux（WSL2），Docker；`CI=1`，沒有容器執行環境時測試失敗、不略過 |
| 工具 | rustc 1.99.0（`.prototools`、`rust-toolchain.toml`） |
| 受測 commit | 分支 `fix/298-log-filter-bridge`，在合併 main `a7f332b` 之後（重定基底，無衝突）；檢查時的工作樹與提交的樹相同，只差本報告與 `data/` |
| 相依 | `tracing-log 0.2.0`（`log-tracer`、`std`）與 `log 0.4.34`，只加到 `service-kit` 的 `dev-dependencies`；正式程式碼的相依不變（`tracing-subscriber` 的 `tracing-log` feature 本來就開著） |

## 2. 做法與發現

issue 預期的缺陷是橋接事件以橋接器的 target `log` 比對。實測與讀 `tracing-log 0.2.0` 的 `dispatch_record` 的結論是：現有的 `LogFilter` 已經正確，不需要改正式程式碼。

- `LogTracer` 在送出橋接事件前，先以記錄的**真正 target** 與等級呼叫 `dispatch.enabled(..)`；per-layer filter 把這個判斷結果記在執行緒區域狀態，隨後的事件就依它放行或丟棄。所以 `LogFilter::enabled` 看到的是 `reqwest::connect` 之類的真 target，`Targets` 的指令照常比對。
- 橋接事件共用 callsite（metadata 的 target 都是 `log`），所以 `LogFilter::callsite_enabled` 對事件回 `Interest::sometimes()`，每個事件各自判斷（這在 Laysi/trustiot-v2#297 就是如此）。
- `LogLayer::on_event` 仍以 `log.target` 取代 `log` 作為日誌行的 `target`，遮蔽（`sdk_diagnostic_suppressed`、欄位遮蔽）與過濾判斷的是同一個 target。

因此這張 issue 的交付是：把這個行為釘成測試（這個行為來自 `tracing-log` 與 per-layer filter 的互動，升級這兩個 crate 後若改變，這些測試會先發現），並在 README 與 `LogFilter` 的註解寫明機制。

## 3. 指令

從 repo 根目錄，在檢查佇列執行（輸出：[`rcheck1.txt`](data/2026-10-12-298-log-filter-bridge/rcheck1.txt)、[`rcheck2.txt`](data/2026-10-12-298-log-filter-bridge/rcheck2.txt)；為公開 repo 已把主機名稱與路徑換成 `build-host`、`<workdir>`）：

```
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --locked -p service-kit --no-fail-fast
cargo clippy -p service-kit --no-default-features --all-targets --locked -- -D warnings
cargo test --locked -p service-kit --no-default-features
cargo test --locked -p repo-check
cargo deny --locked check
```

```
cargo test --locked -p service-kit --lib telemetry
cargo test --locked -p service-kit --test otlp
cargo test --locked -p service-kit --lib
```

## 4. 結果

### 4.1 新增與改動的測試

| 測試 | 內容 | 結果 |
| --- | --- | --- |
| `log_layer::tests::bridged_log_events_are_judged_by_their_original_target`（改寫） | 原本手寫 `log.target` 欄位，現在改用 `tracing-log` 的 `LogTracer` 送出真正的 `log` 記錄；`trace` 等級下 `opentelemetry-otlp` 的 DEBUG 被丟棄、WARN 保留，`reqwest::connect` 的 DEBUG 保留 | 通過 |
| `log_layer::tests::target_directives_suppress_bridged_log_events` | `debug,reqwest=warn`：`reqwest::connect` 的 DEBUG、INFO 被丟棄，`reqwest` 的 WARN 與 `hyper_util::client` 的 DEBUG 保留；原生 tracing 的 `reqwest::native` DEBUG 同樣被丟棄；日誌行的 `target` 是 `reqwest`、`hyper_util::client`，不是 `log` | 通過 |
| `log_layer::tests::target_directives_allow_bridged_log_events_above_the_default` | `warn,reqwest=debug,hyper=off`：`reqwest` 的 DEBUG 與 TRACE 中只有 DEBUG 保留；其他 target 只有 ERROR 保留；`hyper` 全部丟棄 | 通過 |
| `log_layer::tests::bridged_log_events_keep_redaction` | `debug,reqwest=warn`：被丟棄的橋接事件不留任何內容；同 target 的原生事件，敏感欄位（`password`）仍被遮蔽，輸出不含 canary | 通過 |
| `tests/otlp.rs::target_directives_apply_to_events_reqwest_sends_through_the_log_crate` | 以 OTLP 匯出失敗時 reqwest 真實送出的橋接事件（example-service 實際行程的 stdout）驗證：`debug` 基準有 reqwest 的 DEBUG；`debug,reqwest=warn` 沒有，且 `hyper_util` 仍在；`info,reqwest=debug` 有 reqwest 的 DEBUG，`hyper_util` 沒有；兩種設定的輸出都不含端點路徑的 canary | 通過 |

### 4.2 全部檢查

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通過，沒有警告 |
| `cargo test --locked -p service-kit --no-fail-fast` | 全部通過（含容器內 Postgres 的測試）：lib 89、`verify_config` 6、`verify_container` 1、`verify_job_lock` 4、`verify_leaks` 2、`verify_shutdown` 6，其餘測試檔與 doctest 通過；`otlp` 9 個通過（`rcheck2.txt`） |
| `cargo clippy -p service-kit --no-default-features --all-targets --locked -- -D warnings` | 通過 |
| `cargo test --locked -p service-kit --no-default-features` | 全部通過 |
| `cargo test --locked -p repo-check` | 通過（工作區相依只宣告一次） |
| `cargo deny --locked check` | advisories、bans、licenses、sources 皆 ok |

## 5. 結論

| 驗收條件 | 結果 |
| --- | --- |
| 對經 `log` crate 橋接的事件，`target=level` 指令有效（測試） | 通過：4.1 的單元測試以真正的 `LogTracer` 驗證抑制與放寬兩個方向，整合測試以 reqwest 的真實事件驗證 |
| 原生 tracing 事件的過濾行為與 #297 的遮蔽規則不變，既有日誌與遮蔽測試全部通過 | 通過：正式程式碼沒有改，`log_layer`、`redact`、`otlp` 既有測試全部通過；`bridged_log_events_keep_redaction` 另外確認被丟棄的事件不留內容 |
| README 的 `telemetry.log_filter` 說明一併確認 | 完成：設定表與「日誌與 trace」兩處都寫明對所有相依套件有效，橋接事件以原本的 target 比對 |
