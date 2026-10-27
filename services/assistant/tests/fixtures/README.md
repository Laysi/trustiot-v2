# 測試樣本

替身 AI 閘道（`tests/support/mod.rs`）回放的 Anthropic Messages 回應。它們與 crates/ai-clients 的樣本一樣取自供應商的官方文件，不是對真實 API 錄下的回應：測試不呼叫供應商，也沒有 API key。以真實 API 驗證快取與 thinking 的行為在 Laysi/trustiot-v2#554。

| 檔案 | 來源 | 方式 | 用途 |
| --- | --- | --- | --- |
| `step_tool_use.json` | crates/ai-clients 的 `anthropic/message_thinking_tool_use.json`（[thinking](https://platform.claude.com/docs/en/build-with-claude/thinking)，「Progress updates between tool calls」） | 組合：區塊的形狀照原樣，工具換成測試工具 `count_readings`；`__STEP__` 由替身換成步驟編號，讓每一步的 ID 與 signature 不同 | 呼叫工具的一步：thinking（含 signature）與 `tool_use` |
| `answer_end_turn.json` | crates/ai-clients 的 `anthropic/message_cache_read.json`（[prompt-caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)） | 組合：前面加一個 thinking 區塊 | 最後的回答，有快取讀取 |
| `refusal.json` | crates/ai-clients 的 `anthropic/message_refusal.json`（[refusals-and-fallback](https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback)） | 組合：`model` 換成主 agent 的模型 | 沒有內容的拒答 |
