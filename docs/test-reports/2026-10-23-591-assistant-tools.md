# assistant 的第一批工具：以 run 委派代表使用者呼叫

對應 issue：Laysi/trustiot-v2#591。

## 環境

- 建置主機（x86_64 Linux，Rust 1.99.0，見 `rust-toolchain.toml`），經共用的檢查佇列執行，`CI=1`。
- 分支 `feat/591-assistant-tools`，基於 `7371f15b`，之後合併 main 的 `994ffae7`（Laysi/trustiot-v2#588 的對話儲存）；兩個樹都測過，見「結果」。`tests/history_store.rs` 與 `tests/cross_tenant.rs`（來自 main）以 podman 起拋棄式 TLS Postgres。
- 不需要容器、叢集或 API key：身分服務、telemetry-query、assets 與 AI 閘道都是測試在 127.0.0.1 上起的替身。身分服務替身以 `testkit::TestIssuer` 簽真的內部身分 token（測試金鑰），服務替身以 `IdentityVerifier` 依自己的受眾驗證，與真的服務相同。

## 指令

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p assistant
cargo test --locked -p repo-check
cargo deny --locked check
```

`cargo test -p assistant` 連跑 5 次，確認平行執行時穩定（見「結果」的日誌擷取問題）。

## 結果

輸出摘要在 [data/2026-10-23-591-assistant-tools/check-output.txt](data/2026-10-23-591-assistant-tools/check-output.txt)。

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p assistant` | 單元測試 25 個、`tests/agent.rs` 12 個、`tests/delegated_tools.rs` 9 個、`tests/tools.rs` 7 個全部通過；連跑 5 次都通過 |
| `cargo test -p repo-check` | 通過 |
| `cargo deny --locked check` | 通過 |
| 合併 main 後：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test -p repo-check`、`cargo run -p openapi -- --check` | 都通過 |
| 合併 main 後：`cargo test -p assistant`，連跑 3 次 | 每次都全部通過：單元測試 29 個、`tests/agent.rs` 10 個、`tests/agent_logs.rs` 2 個、`tests/cross_tenant.rs` 1 個、`tests/delegated_tools.rs` 9 個、`tests/history_store.rs` 5 個、`tests/tools.rs` 7 個 |

`tests/delegated_tools.rs` 的替身以虛擬時鐘模擬時間：每次模型呼叫算過了 61 秒（一個「分鐘」），服務替身只接受在當下這一分鐘換來的 token，所以重用前一步的 token 會像過期的 token 一樣被拒，測試不需要真的等 60 秒。

開發中遇到兩個測試本身的問題，已修正：

1. 同一秒內為同一受眾換的 token 位元組相同（內部身分 token 沒有 nonce），替身原本以 token 當索引記錄換發的分鐘，後一次覆蓋前一次。改成依序記下每次換發，並另外記錄每個呼叫進來的分鐘。
2. 平行執行時日誌擷取收不到事件（`tests/agent.rs` 偶發、`tests/delegated_tools.rs` 每次）：tracing-core 在程序裡只有一個 dispatcher 時，以第一個碰到 callsite 的執行緒的預設 subscriber 決定該 callsite 要不要記，其他測試執行緒（沒有 subscriber）先碰到就快取成「不記」。測試的 `Capture::install` 在整個測試程式期間多留一個 dispatcher，讓 tracing-core 改問所有存活的 dispatcher。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 每個工具請求都帶受眾正確、代表使用者的 token，沒有租戶參數；403、404 變成 ToolError，模型與日誌看不到回應本文 | 通過：`each_tool_call_runs_as_the_user_with_a_fresh_token_for_its_service_and_no_tenant`（三個工具各自的受眾、`sub` 是使用者、`tenant` 是使用者的租戶、`act` 是 assistant；路徑與查詢參數沒有租戶或使用者）；`a_service_refusal_reaches_neither_the_model_nor_the_logs`（403、404、422、503 與非 JSON 的回應，本文裡的標記字串不出現在模型的請求與任何日誌事件；日誌只有狀態碼類別）；`the_tools_take_no_tenant_or_user_and_refuse_extra_input` |
| 超過 60 秒後的工具呼叫仍以新的 token 成功（不真的等 60 秒） | 通過：同一個測試的第二、三次呼叫在第 2、3 分鐘，各用當分鐘換的 token；第一次呼叫的 token 在第 4 分鐘重送時被拒（401） |
| 身分服務拒絕換 token（撤銷）時工具失敗、問題結束、不重試 | 通過：`a_revoked_delegation_ends_the_question_without_a_retry`（`unauthenticated` 與 `permission_denied` 兩種；被拒的那一步有兩個呼叫，那一步與它的結果不留在歷史、不再呼叫模型、`AgentError::AccessRevoked`；每個呼叫最多換一次 token，被拒後這次提問不再向身分服務要 token）；對照組 `an_unavailable_identity_service_fails_only_the_call` |
| 問題在委派到期前結束 | 通過：`the_question_ends_before_its_delegation_expires`（`AgentError::RunExpired`，在委派到期之前）、`a_question_whose_delegation_has_no_time_left_does_not_start`；截止時間的計算有單元測試（`delegation.rs`） |
| `cargo test -p assistant` 與 `cargo clippy --workspace --all-targets -- -D warnings` 通過 | 通過：見「結果」 |

## 安全審查後的修正（2026-10-23）

安全審查的四個 Low 項目修正後重跑（建置主機，`CI=1`）：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p assistant   # 連跑 3 次
cargo test --locked -p repo-check
```

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p assistant`，連跑 3 次 | 每次都全部通過：單元測試 30 個、`tests/agent.rs` 10 個、`tests/agent_logs.rs` 2 個、`tests/cross_tenant.rs` 1 個、`tests/delegated_tools.rs` 12 個、`tests/history_store.rs` 5 個、`tests/tools.rs` 7 個 |
| `cargo test -p repo-check` | 通過 |

沒有改 API 型別，所以沒有重跑 `cargo run -p openapi -- --check`。

| 修正 | 測試 |
| --- | --- |
| `QuestionRun` 記住建立時的對話租戶與使用者；`Agent::ask` 收到別人對話的 run 時，在呼叫模型或工具之前以 `AgentError::NotTheConversationsRun` 拒絕 | `a_run_is_refused_for_another_conversations_principal`：同租戶的另一個使用者與另一個租戶的同一使用者都被拒，沒有模型呼叫、沒有換 token、沒有服務收到呼叫，歷史不變 |
| 截止時間的測試不再依賴約 2 秒內完成一個工具步驟：模型不回應，只以順序與測試自己設定的值斷言（截止時間不晚於委派效期減去餘裕、結束時已過截止時間、委派尚未到期） | `the_question_ends_before_its_delegation_expires` |
| README 與程式註解改正：`mesh.calls` 少一筆造成的 `permission_denied`，service-kit 記的是 INFO，不是 ERROR，對提問來說和撤銷一樣；另加一行 WARN：一次提問在換到任何 token 之前就被拒絕時，只記 `run_id`、`audience`、`code` | `a_refused_first_exchange_is_logged_as_a_warning`：同一步兩個呼叫都被拒，只有一行 WARN，沒有錯誤本文 |
| 工具結果裡每個字串最多 256 個字元，超過的以 `…[cut]` 結尾；工具說明寫明上限與這些文字是資料 | 單元測試 `long_text_is_cut_with_a_visible_marker_and_short_text_is_kept`；`device_text_reaches_the_model_cut_to_the_cap`：資料點 key、文字讀數與型號名稱都截斷，後段文字沒有進到模型的任何請求 |
