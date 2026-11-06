# 助理：回答工具的開關改成預設打開（2026-11-06）

對應 issue：Laysi/trustiot-v2#1091（第 2 步）。`streams.answer_tool`（`TRUSTIOT__STREAMS__ANSWER_TOOL`）的預設從 `false` 改成 `true`；開關還在，設成 `false` 時新對話回到回答工具之前的提示詞（2026-11-03 版）與工具組。條件與理由見 [services/assistant](../../services/assistant/README.md) 「回答的串流」的開關。

## 環境

- 開發者工作站，分支 `feat/1091-answer-tool-default`，從 main `b3547833` 開出。
- 容器測試用 docker 啟動拋棄式 Postgres。
- 工作站上 `edge/agent` 因 `mode_t` 型別不同編不過，而 `assistant` 的 dev-dependency 經 `lab-dataset` 依賴它；為了編 `assistant` 的測試，驗證時暫時改了 `edge/agent/src/provision/install.rs` 一行，驗證後還原，不在 commit 裡。

## 指令

```
cargo fmt --all --check
cargo clippy -p assistant --all-targets -- -D warnings
cargo test -p assistant --no-fail-fast
cargo run -p openapi -- --check
cargo test -p repo-check
```

## 結果

- `cargo fmt --all --check`：通過。
- `cargo clippy -p assistant --all-targets -- -D warnings`：通過。`--workspace` 在工作站上因 `edge/agent` 的 `mode_t` 錯誤編不過，與這個變更無關。
- `cargo test -p assistant --no-fail-fast`：單元測試 195 項與其餘測試檔全部通過，只有兩項失敗，都是工作站已知的「連到關閉的埠會逾時」問題，在 main `b3547833` 上以相同方式失敗（同樣暫時修正 `edge/agent` 那一行）：
  - `tests/agent.rs` 的 `an_unreachable_gateway_is_unavailable`（`ModelTimeout` 而非 `ModelUnavailable`）。
  - `tests/dashboard_tool.rs` 的 `a_failed_or_missing_verdict_saves_nothing_and_the_draft_keeps_its_version`（`check_timeout` 而非 `check_unreachable`）。
- `cargo run -p openapi -- --check`：`api/openapi.json is up to date`，預設不進 API 規格，沒有重新產生。
- `cargo test -p repo-check`：通過。

新增與調整的測試：

- `src/config.rs`：預設是 `true`；沒有環境變數時載入的設定是 `true`；`TRUSTIOT__STREAMS__ANSWER_TOOL=false` 關掉它。
- `tests/answer_stream.rs` 的 `without_the_switch_a_new_conversation_has_the_answer_tool_and_the_current_prompt`（新增）：沒有環境變數啟動的服務，新對話存下 2026-11-05 版提示詞與 `answer` 工具，模型請求帶工具。
- 既有的關閉測試（`with_the_switch_off_a_new_conversation_answers_as_text_after_the_question`、`with_the_switch_off_a_new_conversation_has_no_answer_tool_and_the_previous_prompt`）驗 2026-11-03 版提示詞、沒有 `answer` 工具，改成明確設 `false`。
- `tests/answer_stream.rs`、`tests/regenerate.rs`、`tests/questions.rs` 的預設 `world()` 原本靠預設關，現在明確設 `false`，維持原本驗文字回答的用意。

## 結論

- 預設改成 `true`，開關與「已建立的對話不受開關影響」的行為不變：通過。
- 文件（服務 README 的開關段落與設定表、`docs/architecture/ai-assistant.md` 對話第 7 點）已更新：完成。
- 沒有動 `deploy/`；demo 的 values 保留明確的 `true`。
