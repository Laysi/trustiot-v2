# ai-clients

AI 供應商的原生協定 client（決策 15）：Anthropic Messages 與 OpenAI Decisions 的請求與回應型別、SSE 串流解析、依計費分類的用量、錯誤分類與每個端點的並發上限（Laysi/trustiot-v2#138）。設計在 [docs/architecture/ai-assistant.md](../../docs/architecture/ai-assistant.md) 的「AI 閘道」與「分流」。

| 誰 | 用什麼 |
| --- | --- |
| AI 閘道（Laysi/trustiot-v2#139） | `AnthropicClient`、`DecisionsClient` 送出請求；`MessageStream` 逐一轉送事件（`StreamItem::raw` 是收到的原樣事件）；`Error::class()` 決定換備援模型或重試；`Usage::attempts()` 與 `DecisionUsage::tokens()` 交給用量帳本（Laysi/trustiot-v2#140） |
| 組請求、讀回應的服務（主 agent、worker、分流） | `MessagesRequest`、`InputMessage::from_response`（助理回合原樣加回歷史）、`Message::outcome()`；`DecisionRequest`、`DecisionResponse` |

不包含：備援與模型角色（Laysi/trustiot-v2#139）、Vertex AI（Laysi/trustiot-v2#440）、地端的 OpenAI 相容 client。

## 規則

- **不依賴 `crates/contract`**（決策 28）：`tests/no_contract.rs` 以 `cargo tree -p ai-clients`（含 dev 與 build 依賴）檢查，並確認同一個檢查在 `outbox` 上找得到 contract。
- **TLS 由呼叫端決定**：`new` 收一個 `reqwest::ClientBuilder`，TLS 後端與信任錨由 AI 閘道設定；這個 crate 只加上連線逾時、TCP keepalive 與「不跟隨重新導向」（`x-api-key` 不在 reqwest 跨來源導向時移除的標頭之列，跟隨導向會把 key 送到回應指定的地方）。不要在 builder 上設逾時，逾時由 `Timeouts` 管。這個 crate 不開 reqwest 的任何 TLS feature，所以在 AI 閘道開啟一個 TLS feature（例如 `rustls`）之前，`https://` 連線一律失敗（fail closed）；AI 閘道絕不可傳入設了 `danger_accept_invalid_certs` 或 `danger_accept_invalid_hostnames` 的 builder。
- **端點網址**：只收 `https://`；`http://` 只限 loopback（測試與同機的 proxy）。不可帶帳密、query 或 fragment。`http://` 端點不走任何 proxy（`no_proxy()`）：reqwest 預設套用環境變數 `HTTP_PROXY`/`ALL_PROXY`，而且不排除 loopback，會把請求連同 API key 以明文送給 proxy。`https://` 端點保留 builder 的 proxy 設定（部署在 egress proxy 後面時需要；CONNECT 讓 TLS 端到端，proxy 看不到內容）。
- **API key**：`ApiKey` 的 `Debug` 不顯示內容，drop 時以 `zeroize` 清除，標頭標成 sensitive；client 的 `Debug` 只顯示端點名稱。`zeroize` 只清 `ApiKey` 自己存的那一份：每個請求建出的 `HeaderValue` 與 HTTP 層（hyper）寫出時的緩衝區也有副本，釋放時不會被清除。
- **大小與時間上限**：`EndpointConfig::max_response_bytes`（預設 32 MiB）限制非串流回應本文，也限制整條串流收到的位元組（事件、`ping` 與註解都算，所以 `MessageAccumulator` 累積的內容有上限）；錯誤本文最多讀 64 KiB；`max_event_bytes`（預設 4 MiB）限制單一 SSE 事件，真實事件只有數 KiB，最大的是整個放在 `content_block_start` 的伺服器工具結果。串流另有總時間上限 `Timeouts::stream_total`（預設 60 分鐘，從送出請求起算；以每秒 50 token 產生 128k 輸出 token 約 43 分鐘），不讓一條不斷送 `ping` 或小 delta 的串流永遠占住並發名額。超過見下方「錯誤情境」。直接用 `MessageAccumulator`（不經 `MessageStream`）的人自己限制餵進去的量。
- **錯誤訊息不引用供應商的文字**：錯誤字串會進 log，所以不放模型輸出或租戶資料。delta 型別不符只寫 delta 與區塊的種類；JSON 解析錯誤只寫類別（語法、截斷、內容不符）、行與欄，以及缺少的欄位名（來自我們的型別）；Decisions 答案不符只引用請求裡的題目名稱與等級標籤。`ApiError` 的 `Display` 把供應商的 `type` 與 `message` 各截到 512 個字元；完整的 `message` 只留在結構化欄位 `ApiErrorBody::message`（HTTP 錯誤本文最多 64 KiB，串流的 `error` 事件受 `max_event_bytes` 限制）。
- **原樣送回**：內容區塊以 JSON 物件解析，已知型別（text、image、tool_use、tool_result、thinking、redacted_thinking）有型別欄位，其他欄位保留在 `extra`；未知型別（`fallback`、`server_tool_use`、`connector_text` 等）整個保留成 `ContentBlock::Other`。所以 thinking 的 `signature`、工具呼叫的 `input` 與每個未知欄位，經解析再序列化後與收到的完全相同（JSON 值相同；物件鍵的順序不保留，API 不依賴順序）。串流以 `MessageAccumulator` 組出與非串流相同的 `Message`。
- **對話中的 system 訊息**：`InputMessage::system_text` 產生 `role: "system"` 的訊息；只能放服務產生的文字，不放使用者輸入、工具結果或文件內容（ai-assistant.md 「對話歷史只往後加」）。

## 錯誤情境

`Error::class()` 回傳 `ErrorClass`，AI 閘道依它處理；`RateLimited`、`Overloaded`、`Timeout` 是換下一個備援模型的條件（ai-assistant.md 「AI 閘道」第 2 點）。拒答與 `max_tokens` 沒有文字不是錯誤，是 `Message::outcome()` 的結果（照樣計費）。測試名稱前的檔名在 `tests/`，樣本來源見 [tests/fixtures/README.md](tests/fixtures/README.md)。

| 情境 | 怎麼判斷 | 結果 | 測試 |
| --- | --- | --- | --- |
| 429 速率限制 | HTTP 429 或 `rate_limit_error` | `RateLimited { retry_after }`，帶 `retry-after`（或 `retry-after-ms`） | `anthropic_http.rs` `recorded_error_responses_are_classified`、`anthropic_fixtures.rs` `error_responses_classify_by_status_and_type` |
| 429 月消費上限 | 同上，但沒有 `retry-after`；重試不會好 | `RateLimited { retry_after: None }` | 同上 |
| 529 過載 | HTTP 529 或 `overloaded_error` | `Overloaded` | 同上 |
| 串流中途過載 | HTTP 200 後的 `error` 事件（`overloaded_error`） | `Error::StreamError`，`Overloaded`，串流結束；已收到的部分留在 `MessageStream::message()` | `anthropic_fixtures.rs` `an_overloaded_error_event_mid_stream_ends_the_stream`、`anthropic_http.rs` `an_error_event_mid_stream_is_classified_with_the_request_id` |
| 供應商逾時 | HTTP 504 或 `timeout_error` | `Timeout(Provider)` | `recorded_error_responses_are_classified` |
| 連線逾時 | TCP 連線在 `Timeouts::connect` 內沒有建立 | `Timeout(Connect)` | `anthropic_http.rs` `an_unanswered_connection_attempt_is_a_connect_timeout` |
| 首 token 逾時 | 串流請求從送出起，在 `Timeouts::first_token` 內沒有收到回應標頭或第一個內容區塊事件（`ping` 與 `message_start` 不算）；供應商端排隊也表現在這裡 | `Timeout(FirstToken)` | `anthropic_http.rs` `no_response_headers_in_time_are_first_token_and_response_timeouts`、`headers_without_model_output_are_a_first_token_timeout`、`anthropic_fixtures.rs` `no_output_before_the_first_token_limit_is_a_first_token_timeout` |
| 串流中途 idle 逾時 | 兩段資料之間（`ping` 也算資料）超過 `Timeouts::stream_idle` | `Timeout(StreamIdle)` | `anthropic_http.rs` `a_stream_that_goes_silent_is_a_stream_idle_timeout`、`anthropic_fixtures.rs` `silence_after_output_started_is_a_stream_idle_timeout` |
| 串流總時間逾時 | 從送出請求起，`Timeouts::stream_total` 內沒有收到 `message_stop`（資料一直在來也一樣） | `Timeout(StreamTotal)`，串流結束、並發名額空出；已收到的部分留在 `MessageStream::message()` | 2026-10-22 審查修正，依專案的驗證分級沒有另加測試 |
| 串流總大小超過上限 | 整條串流收到的位元組超過 `max_response_bytes` | `Error::Protocol`（`Other`），串流結束、並發名額空出 | 同上 |
| HTTP 200 但回應不可用 | 非串流回應的本文超過 `max_response_bytes`、不是合法的訊息，或 Decisions 的答案對不上題目 | `Error::InvalidResponse { reason, usage, request_id }`（`Other`）。供應商已計費，`usage` 是讀得到的用量：`ReportedUsage::Attempts`（同 `Usage::attempts()`，Anthropic 從 `usage` 物件單獨解析，Decisions 取已解析的 `usage`），讀不到時是 `ReportedUsage::Unknown`，帳本要估算並標記，絕不記 0。`Error::reported_usage()` 取得它 | `anthropic_http.rs` `responses_that_break_the_protocol_are_refused`、`decisions.rs` `the_client_refuses_answers_that_do_not_fit_and_invalid_requests` |
| 非串流回應逾時 | 標頭與本文沒有在 `Timeouts::response` 內收完 | `Timeout(Response)` | `no_response_headers_in_time_are_first_token_and_response_timeouts` |
| 拒答 | `stop_reason: refusal`（非串流，或串流的 `message_delta`；串流的拒答可能在部分輸出之後、甚至工具呼叫區塊還沒結束時到達） | `Outcome::Refusal { category }`，`category` 取自 `stop_details`。串流組出的訊息捨棄拒答前的部分輸出（`content` 清空，與非串流的拒答相同）；已轉送的原樣事件仍含部分輸出，顯示端收到拒答時要丟掉 | `anthropic_fixtures.rs` `outcomes_of_recorded_responses`、`streamed_refusal_and_thinking_only_responses_have_the_same_outcomes`、`a_refusal_while_a_tool_call_is_open_ends_the_message_without_content` |
| `max_tokens` 且沒有文字 | `stop_reason: max_tokens`，沒有非空白的 text 區塊（thinking 用光上限） | `Outcome::MaxTokensWithoutText`；有文字時是 `Completed`。預熱快取（`max_tokens: 0`）的回應也是這個結果，送出的人自己知道 | 同上 |
| 超過並發上限 | 端點的請求數已滿，`queue_wait` 內沒有空出 | `Error::ConcurrencyLimited`，請求沒有送出 | `anthropic_http.rs` `requests_over_the_concurrency_limit_are_not_sent`、`a_queued_request_waits_for_a_free_slot_up_to_the_queue_wait`、`decisions.rs` `decisions_over_the_concurrency_limit_are_not_sent` |
| 取消串流 | `MessageStream::cancel()` 或 `cancellation_token()` 從別的 task 取消 | `Error::Cancelled`，不再讀取。`cancel()` 立即釋放回應（連線關閉）並空出並發名額；token 取消時，擁有串流的 task 要再 poll（正在 `next()` 裡等待的會立即醒來）或 drop 串流才會釋放，在那之前連線與名額仍被占住 | `anthropic_fixtures.rs` `after_cancel_nothing_more_is_read`、`a_token_cancels_a_stream_waiting_in_another_task`、`anthropic_http.rs` `requests_over_the_concurrency_limit_are_not_sent`、`dropping_a_stream_closes_the_connection_and_frees_the_slot` |
| 其他 | 其他 4xx 與 5xx（400、401、500 等）、重新導向（3xx，不跟隨）、連線失敗、不符協定的串流（JSON 錯誤、事件順序錯、內容區塊還沒結束就收到 `message_delta` 或 `message_stop`、`message_stop` 前就結束、串流請求收到非 `text/event-stream`） | `Other`（`Error::Api`、`Transport`、`Protocol`、`Sse`） | `recorded_error_responses_are_classified`、`redirects_are_not_followed`、`responses_that_break_the_protocol_are_refused`、`a_stream_cut_before_message_stop_is_a_protocol_error`、`content_blocks_must_be_stopped_before_the_message_ends`、`decisions.rs` `malformed_answers_are_errors` |

OpenAI（Decisions）依 HTTP 狀態分類：429（包含 `insufficient_quota`）是 `RateLimited`，503 是 `Overloaded`，504 是 `Timeout(Provider)`，其他是 `Other`（`decisions.rs` `openai_errors_are_classified`）。

`retry_after` 是供應商標頭的原樣值，沒有上限；AI 閘道把它當成暫停某個模型的時間之前，要先夾到自己的上限內。

除了 `Error::InvalidResponse`，錯誤不帶用量：逾時、連線中斷（含 HTTP 200 之後讀本文時）與串流錯誤時，供應商可能已經計費，帳本依 ai-assistant.md 「AI 閘道」第 3 點估算並標記；串流已收到的用量在 `MessageStream::message()`。

## 用量

`TokenUsage` 是帳本記錄的分類：一般輸入、快取寫入 5 分鐘、快取寫入 1 小時、快取讀取、輸出（含 thinking）。

- **Anthropic**：`input_tokens` 只是最後一個快取斷點之後的輸入；`cache_creation_input_tokens` 依 `cache_creation` 分成 5 分鐘與 1 小時。供應商只給總數、沒有分開時，沒分到的部分算 5 分鐘（預設的存活時間），並設 `cache_write_ttl_assumed`，帳本應標記這筆。有 `usage.iterations`（伺服器端備援、壓縮）時，頂層只描述最後回答的那次嘗試，計費依 `Usage::attempts()`：每次嘗試以自己的模型計。串流的 `message_delta` 用量是累計值，後到的欄位取代先前的值。
- **Decisions**：只收輸入 token，單一費率，沒有快取或輸出費用，所以 `input = input_tokens`；`cached_tokens` 與 `cache_write_tokens` 保留供觀察。
- **沒回報的數字**：Anthropic 的 `input_tokens` 與 `output_tokens` 是必有欄位；回應（或 `usage.iterations` 的某次嘗試）缺了它（例如 proxy 拿掉了 `usage`）時，`TokenUsage` 該欄是 0，並設 `input_missing` 或 `output_missing`（`is_incomplete()`），帳本要估算並標記，絕不記 0。快取的兩個數字在 API 裡可以是 null，沒有就是 0。Decisions 的 `usage` 沒有 `input_tokens` 時整個回應解析失敗；沒有 `usage` 時 `DecisionResponse::usage` 是 `None`。HTTP 200 但回應不可用時，用量在 `Error::InvalidResponse` 的 `usage`（見「錯誤情境」）。

測試：`anthropic_fixtures.rs` `usage_categories_parse_from_recorded_responses`、`a_server_side_fallback_bills_every_attempt_at_its_own_model`、`cache_writes_without_a_lifetime_split_count_as_5_minute_and_are_flagged`、`stream_usage_is_cumulative_and_ends_as_the_non_streaming_response`；`decisions.rs` `decisions_usage_bills_every_input_token_at_the_input_rate`。

## OpenAI Decisions

`POST /v1/decisions`（公開 beta，2026-10-21 只有 `gpt-6-luna`）。請求的 `input` 是字串或訊息（`input_text` 與 `input_image`），題目 1 到 400 題、名稱唯一；這個 crate 只用 predicate 與 score 兩種題型。圖片只送 base64 data URL（`InputPart::image_base64`，PNG、JPEG、GIF、WebP，最多 128 張）：API 也收公開網址，但我們的圖片不公開。`DecisionsClient::decide` 送出前檢查請求（`DecisionRequest::validate`），收到後檢查每個答案對得上題目（數量、順序、名稱、題型或拒答、機率與信心在 0 到 1、score 的分布剛好涵蓋題目的每個等級、機率加總為 1、`score` 等於依機率加權的等級索引平均；後兩項容許 0.02 的誤差，`score` 的誤差再乘上最高等級的索引），不符是 `Error::Protocol`。不串流。

## 測試

`cargo test -p ai-clients`。HTTP 層的測試在 loopback 起一個腳本化的 HTTP/1.1 伺服器（`tests/support/mod.rs`），不連外；連線逾時的測試以一個 accept 佇列塞滿的 listener 讓連線得不到回應。時間上限只用在測試自己設定的逾時（數百毫秒，對方永不回應），等待事件以 `support::WAIT`（30 秒）為上限。
