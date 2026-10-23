# AI 助理的主 agent 迴圈

對應 issue：Laysi/trustiot-v2#145。

## 環境

- 開發者工作站：x86_64 Linux。
- 工具版本：
  - Rust 1.99.0（見 `rust-toolchain.toml`）
  - Bun 1.4.2
  - cargo-deny 0.20.2（見 `.prototools`）
- 執行方式：一次只跑一個 cargo 指令，`CARGO_BUILD_JOBS=2`。
- 分支 `feat/145-assistant-agent-loop`，commit `c80fef73`，基於 `c4d045a2`。測試時的工作樹就是這個 commit，沒有未提交的修改。
- 不需要容器或叢集：測試在本機 loopback 起替身 AI 閘道，回放 `services/assistant/tests/fixtures/` 的錄製回應，並使用假工具；不呼叫供應商，也沒有 API key。`CI=1`，所以需要容器的測試會失敗，而不是略過。本服務沒有這類測試。

## 指令

從 repository 根目錄執行：

```sh
cargo clippy -p assistant -p openapi --all-targets -- -D warnings
CI=1 cargo test -p assistant
cargo run -p openapi -- --check
cargo test -p repo-check
cargo deny --locked check
bun install --cwd scripts --frozen-lockfile
bun test scripts
cargo fmt --all --check
```

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo clippy -p assistant -p openapi --all-targets -- -D warnings` | 通過，沒有警告 |
| `CI=1 cargo test -p assistant` | 33 個通過、0 失敗、0 略過：單元測試 14 個、`tests/agent.rs` 12 個、`tests/tools.rs` 7 個；doc test 0 個 |
| `cargo run -p openapi -- --check` | 通過：`api/openapi.json is up to date`（assistant 列在沒有對外 API 的服務） |
| `cargo test -p repo-check` | 26 個通過、0 失敗、0 略過：單元測試 17 個、`tests/repository.rs` 9 個 |
| `cargo deny --locked check` | `advisories ok, bans ok, licenses ok, sources ok` |
| `bun test scripts` | 1590 個通過、0 失敗、9 個略過。略過的是需要另外開啟的測試：podman 的開發環境 e2e（`TRUSTIOT_DEV_E2E`）與測試信件的容器測試，與本變更無關 |
| `cargo fmt --all --check` | 通過 |

第一次執行就全部通過，沒有修正。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 長分析不受步數限制而完成；接近預算時 agent 收尾 | 通過，見下方說明 |
| 連續的請求不改動先前的訊息（含 thinking）、system prompt 與工具定義 | 通過：`later_requests_resend_the_history_byte_for_byte`。兩個問題共 6 個請求，每個請求都以前一個請求的訊息逐位元組開頭；`tools`、`system`、`thinking`、`output_config`、`cache_control`、`max_tokens` 都不變；模型的 thinking 與 signature 原樣送回 |
| 兩個使用者同時發問互不阻塞 | 通過：`two_users_asking_at_once_do_not_wait_for_each_other`。一位使用者的工具還在等待時，另一位使用者照樣得到回答 |
| 模型逾時給固定錯誤，不含底層文字 | 通過：`a_model_call_that_times_out_ends_with_a_generic_error`、`gateway_errors_reach_the_user_without_their_text`、`an_unreachable_gateway_is_unavailable`。使用者只看到固定訊息；閘道的錯誤文字不進回答，也不進日誌 |
| 每一步的時間記在日誌 | 通過，見下方說明 |

第一條的對應測試：
- `a_long_analysis_runs_without_a_step_limit`：60 步之後回答。
- `the_agent_winds_down_near_the_budget_and_answers`：用到 80% 時第 9 個請求帶收尾提示，模型隨即回答。
- `a_call_that_would_cross_the_budget_is_not_sent`、`a_question_whose_first_call_does_not_fit_is_not_sent`：放不進剩餘預算的呼叫不送出，`max_tokens` 降到剩下的空間。
- `tool_calls_past_the_budget_are_not_run`：預算用完後，工具不執行。

最後一條的對應測試是 `every_step_is_logged_with_its_duration`：
- 每次模型呼叫記錄 `step`、`duration_ms`、`llm_call_id`、`usage.cache_read`。
- 每次工具呼叫記錄 `duration_ms`。
- 記錄的欄位名稱都不會被日誌管線遮蔽。

工具介面由 `tests/tools.rs` 涵蓋，全部通過：
- 最多 10 個工具；超過每步呼叫數上限的呼叫不執行。
- 結果有大小與圖片上限，超過時回覆縮小範圍的提示。
- 錯誤會說明如何修正。
- 圖片照 Anthropic 格式送給模型。

待測：經真實 AI 閘道與 Anthropic 的多輪對話與 prompt caching 命中，由 Laysi/trustiot-v2#554 在部署了 AI 閘道與 assistant 的環境實測；使用者從聊天 API 起的端到端路徑要等 Laysi/trustiot-v2#146。
