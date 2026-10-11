# service-kit：OTLP 端點路徑不進日誌（Laysi/trustiot-v2#285）

對應 issue：Laysi/trustiot-v2#285（來源：Laysi/trustiot-v2#51 獨立驗證的 F-4，`2026-10-11-51-verify.md`）。

## 環境

- 建置主機 `ruisi-wsl`（Ubuntu on WSL2，Docker，`CI=1`），經 `rcheck.sh 285 '<指令>'` 執行。
- Rust 1.99.0（`.prototools` 鎖定），依賴版本以 `Cargo.lock` 為準：`opentelemetry`、`opentelemetry_sdk`、`opentelemetry-otlp`、`opentelemetry-http` 0.33.1，`reqwest` 0.13.5，`hyper` 1.12.0，`hyper-util` 0.1.21。
- 分支 `fix/285-otlp-endpoint-redaction`，合併 `origin/main`（cc1bef2，含 Laysi/trustiot-v2#280 的 `verify_leaks.rs`）之後。

## 原因與做法

OpenTelemetry SDK 的診斷日誌以 `tracing` 事件輸出，target 是各 crate 的名稱。審查鎖定版本的原始碼，可能印出端點 URL 的位置：

| 位置 | 等級 | 內容 |
| --- | --- | --- |
| `opentelemetry-otlp` `HttpClient.NetworkError` | DEBUG | `url` 欄位與 reqwest 錯誤（`error sending request for url (...)`） |
| `opentelemetry-otlp` `HttpClient.StatusError` | DEBUG | `url` 欄位與 collector 回應的 body |
| `opentelemetry-otlp` `HttpClient.ResponseBodyTooLarge` | DEBUG | `url` 欄位 |
| `opentelemetry-otlp` `LogExporterBuilt` | DEBUG | exporter 的 `Debug` 結果 |

`opentelemetry`、`opentelemetry_sdk`、`opentelemetry-http` 的 INFO、WARN、ERROR 診斷，以及 `opentelemetry-otlp` 的 INFO 以上，欄位只有操作名稱、次數、延遲與固定訊息（`HTTP export failed: network error`），沒有 URL。傳輸層（`reqwest`、`hyper_util`）在 DEBUG 與 TRACE 只記主機與連接埠（例如 `connecting to 127.0.0.1:39540`），不含路徑。

做法：target 以 `opentelemetry` 開頭的事件，DEBUG 與 TRACE 一律丟棄，不受 `log_filter` 影響（`LogFilter::enabled`，以及在 `on_event` 以實際 target 再判斷一次，涵蓋經 `log` crate 橋接、真正 target 在 `log.target` 欄位的事件）。INFO 以上照常輸出。

## 指令

```sh
# 修正前（暫時讓 sdk_diagnostic_suppressed 恆為 false）
rcheck.sh 285 'cargo test --locked -p service-kit --test otlp 2>&1 | cut -c1-600 | tail -80'
# 修正後
rcheck.sh 285 'cargo fmt --all --check && cargo clippy --locked --workspace --all-targets -- -D warnings && cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings && cargo test --locked --workspace && cargo test --locked -p service-kit --no-default-features'
rcheck.sh 285 'cargo test --locked -p service-kit --test otlp --test logging --test verify_leaks 2>&1 | grep -E "^test |^test result|Running"'
```

## 結果

測試設計（`crates/service-kit/tests/otlp.rs`）：範例服務跑成真的行程，OTLP 端點是 `http://127.0.0.1:<port>/endpoint-path-canary-7d41`，指向一個每次匯出都失敗的假 collector，另外在 SDK 讀取的環境變數（`OTEL_EXPORTER_OTLP_HEADERS`、`OTEL_EXPORTER_OTLP_ENDPOINT`、`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`）放 canary。測試先確認 collector 真的收到 `/endpoint-path-canary-7d41/v1/logs` 與 `/v1/traces` 的請求（匯出確實發生並失敗），再檢查 stdout 與 stderr 都沒有任何 canary。

| 情境 | 修正前 | 修正後 |
| --- | --- | --- |
| `debug`，collector 重設連線 | 失敗：DEBUG `HttpClient.NetworkError` 含完整 URL | 通過 |
| `trace`，collector 重設連線 | 失敗 | 通過 |
| `debug`，collector 回 400 與含 canary 的 body | 失敗 | 通過（URL 與回應 body 都不出現） |
| `trace`，collector 回 400 | 失敗 | 通過 |
| `info,opentelemetry=trace,opentelemetry_sdk=trace,opentelemetry-otlp=trace,opentelemetry-http=trace,reqwest=trace,hyper_util=trace` | 失敗 | 通過（針對 SDK 的指令不能把等級調高） |
| 端點帶 query（`?token=<canary>`），`debug` 與 `trace` | 通過（設定驗證本來就拒絕） | 通過：退出碼 1，錯誤訊息點出 `telemetry.otlp_endpoint`，沒有 canary |
| 既有測試（`logging`、`verify_leaks`、`otlp` 其餘） | 通過 | 通過 |

單元測試（`telemetry::log_layer`）：四個 SDK target（`opentelemetry`、`opentelemetry_sdk`、`opentelemetry-otlp`、`opentelemetry-http`）在濾器為 `trace` 且各 target 單獨設 `trace` 時，ERROR、WARN、INFO 保留、DEBUG、TRACE 丟棄；其他 target 的 DEBUG、TRACE 不受影響；經 `log` 橋接、`log.target` 為 SDK target 的 DEBUG 丟棄、WARN 保留。

原始資料：[修正前](data/2026-10-11-285-otlp-endpoint-redaction/pre-fix-failure-excerpt.txt)、[修正後](data/2026-10-11-285-otlp-endpoint-redaction/post-fix-tests.txt)。

## 結論

- `debug`、`trace` 等級下匯出失敗時，stdout 與 stderr 都沒有端點路徑或查詢字串（canary 測試）：**通過**。
- 預設等級下的既有遮蔽測試全部通過（`logging`、`verify_leaks` 含 Laysi/trustiot-v2#280 新增的 `an_unreachable_collector_url_with_a_secret_path_does_not_reach_stdout`、單元測試）：**通過**。
- README 的遮蔽規則已更新（`crates/service-kit/README.md` 的設定表、OTLP 匯出一節、測試表），並同步 `docs/architecture/observability.md`：**通過**。

沒有涵蓋：沒有真實 collector。SDK 升級後 INFO 以上的診斷可能新增含 URL 的欄位，由 `tests/otlp.rs` 的 canary 測試在 CI 攔截（它只涵蓋失敗匯出的兩種情境，重設與 400）。
