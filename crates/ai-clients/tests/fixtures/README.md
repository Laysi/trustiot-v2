# 測試樣本

這裡的回應與串流取自供應商的官方文件（2026-10-21 讀取），不是對真實 API 錄下的回應：沒有供應商的 API key，也不在測試中呼叫供應商。「原樣」表示文件中的範例逐字照抄（JSON 排版可能不同）；「組合」表示由文件中的片段與說明組成，欄位名稱、型別與事件順序都依文件，ID、signature、內容文字等值是範例或自填。SSE 檔的開頭以 `:` 註解行（SSE 的註解，解析時略過）寫出來源。以真實 API 錄下的回應再驗證一次，排在後續的 issue。

## Anthropic Messages（`anthropic/`）

| 檔案 | 來源 | 方式 | 測什麼 |
| --- | --- | --- | --- |
| `message_end_turn.json` | [handling-stop-reasons](https://platform.claude.com/docs/en/build-with-claude/handling-stop-reasons)，`end_turn` 範例 | 原樣，補上 `model` | 一般輸入與輸出 |
| `message_max_tokens_with_text.json` | 同上，`max_tokens` 範例（`max_tokens: 10`） | 組合 | 有文字的 `max_tokens` 仍是正常結果 |
| `message_cache_write_5m.json` | [prompt-caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)，預熱快取（`max_tokens: 0`）的回應 | 原樣 | 5 分鐘快取寫入、`iterations` |
| `message_cache_write_1h.json` | 同上，「1-hour cache duration」的 `usage` | 組合（`usage` 原樣，外層補成完整訊息） | 5 分鐘與 1 小時快取寫入、快取讀取、輸出 |
| `message_cache_read.json` | 同上，快取讀取 100,000、新輸入 50 的計費說明 | 組合 | 快取讀取 |
| `message_thinking_tool_use.json` | [thinking](https://platform.claude.com/docs/en/build-with-claude/thinking)，「Progress updates between tool calls」的回應 | 組合（`content` 原樣，外層欄位與 `usage` 自填） | thinking（含 signature）與工具呼叫原樣送回 |
| `message_max_tokens_thinking_only.json` | 同上，「Progress updates between tool calls」：以 `max_tokens` 停下時最後一個區塊是文字為「This part of the response was interrupted before it finished.」的 thinking | 組合 | `max_tokens` 且沒有文字 |
| `message_refusal.json` | [refusals-and-fallback](https://platform.claude.com/docs/en/build-with-claude/refusals-and-fallback)，「What a refusal looks like」 | 原樣 | 拒答與 `stop_details` |
| `message_mixed_blocks.json` | 同上（`fallback` 區塊、`iterations` 的兩次嘗試、`connector_text`）、[streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)（GCD 的 thinking 與 signature、web search 的 `server_tool_use` 與 `web_search_tool_result`）、thinking（`redacted_thinking` 的形狀，`data` 自填）、[Messages API 參考](https://platform.claude.com/docs/en/api/messages)（`tool_use` 的 `caller`） | 組合 | 每種區塊原樣送回，未知區塊與欄位保留；每次嘗試依自己的模型計費 |
| `errors.json` | [errors](https://platform.claude.com/docs/en/api/errors)（錯誤型別、HTTP 狀態、`request_id`、`retry-after`、月消費上限的 429 沒有 `retry-after`） | 組合 | 429、529、504 與其他錯誤的分類 |
| `stream_text.sse` | streaming，「Basic streaming request」 | 原樣 | 文字串流、ping |
| `stream_tool_use.sse` | streaming，「Streaming request with tool use」 | 原樣 | `input_json_delta` 組成工具輸入 |
| `stream_thinking.sse` | streaming，「Streaming request with thinking」 | 原樣 | `thinking_delta` 與 `signature_delta` |
| `stream_thinking_tool_use.sse` | thinking（「Streaming thinking」的 `omitted` 與 `updates` 事件順序）與 streaming（工具呼叫） | 組合，串流出 `message_thinking_tool_use.json` | 串流組出的訊息與非串流完全相同 |
| `stream_max_tokens_thinking_only.sse` | thinking | 組合，串流出 `message_max_tokens_thinking_only.json` | 串流的 `max_tokens` 且沒有文字 |
| `stream_cache_usage.sse` | streaming（`message_start` 與 `message_delta` 的快取欄位，累計值）與 prompt-caching | 組合，串流出 `message_cache_write_1h.json` | 串流的用量是累計值 |
| `stream_refusal.sse` | refusals-and-fallback（串流中途拒答）與 handling-stop-reasons（`stop_reason` 在 `message_delta`） | 組合 | 串流中途拒答 |
| `stream_overloaded.sse` | streaming，「Error events」 | 組合（error 事件原樣，前面接 basic 範例的事件） | 串流中途的 `overloaded_error` |

## OpenAI Decisions（`decisions/`）

| 檔案 | 來源 | 方式 |
| --- | --- | --- |
| `predicate.request.json`、`predicate.response.json` | [Create decision 參考](https://developers.openai.com/api/reference/resources/decisions/methods/create)的範例 | 原樣 |
| `score.request.json`、`score.response.json` | [Decisions 指南](https://developers.openai.com/api/docs/guides/decisions)的 score 範例 | 組合：題目與答案原樣，補上 `model` 與參考格式的 `usage` |
| `router.request.json`、`router.response.json`、`router_refusal.response.json` | 依參考的請求與回應格式組成，題目是 AI 分流的兩題（docs/architecture/ai-assistant.md 「分流」），圖片是 1×1 PNG 的 base64 data URL | 組合 |
