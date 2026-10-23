# ai-gateway（AI 閘道）

保管 AI 供應商的 API key，依模型角色照各供應商的原生協定轉送請求與串流，不轉換格式（決策 15、28；Laysi/trustiot-v2#139）。閘道填入角色設定的模型與憑證，每次呼叫都有時間上限，遇到 429、過載或逾時改用角色的下一個模型；回應與串流照供應商送來的樣子交回。設計在 [docs/architecture/ai-assistant.md](../../docs/architecture/ai-assistant.md) 的「AI 閘道」。

閘道與產品無關（決策 28）：租戶只是不透明的 ID，設定用自己的前綴 `AI_GATEWAY`，也不依賴 `crates/contract`（`tests/no_contract.rs` 以 `cargo tree -p ai-gateway` 檢查，service-kit 以 `default-features = false` 引入）。

目前支援兩種協定：Anthropic Messages（含串流）與 OpenAI Decisions。模型角色的模型清單可以在執行中更換：更換存在 Postgres，每個轉送請求都確認設定有沒有變（見「資料庫」與「多副本」，Laysi/trustiot-v2#605）。不在這裡：更換用的管理 API（Laysi/trustiot-v2#606、Laysi/trustiot-v2#607）、用量帳本與租戶額度（Laysi/trustiot-v2#140）、Vertex AI（Laysi/trustiot-v2#440）。部署（映像 `deploy/images/ai-gateway`、chart 從 values 的 `ai` 產生的設定、金鑰的 Secret、只允許三個呼叫端的 AuthorizationPolicy）見 [chart README](../../deploy/chart/README.md)「AI 閘道」（Laysi/trustiot-v2#536）。

| 檔案 | 內容 |
| --- | --- |
| `src/config.rs` | 設定與啟動時的檢查，以及設定與存下的模型清單共用的模型規則（`RoleRules`） |
| `src/gateway.rs` | 每個角色的模型與供應商 client、讀 key、時間預算；設定的快照與它的重載 |
| `src/store.rs` | Postgres 裡的模型清單、設定世代與變更紀錄：讀取、更換、重設 |
| `src/schema.rs`、`migrations/postgres/` | schema `ai_gateway` 的 migration（由 `tools/migrate` 執行） |
| `src/relay.rs` | 內部 API：讀請求、依序嘗試模型、交回回應或轉送串流、日誌 |
| `src/body.rs` | 請求本文的檢查與換 `model` |
| `src/reject.rs` | 錯誤回應（各協定自己的錯誤格式） |

供應商 client 是 [crates/ai-clients](../../crates/ai-clients/README.md)：HTTP/1.1 與 TLS（rustls、ring）、逾時、串流解析、錯誤分類、並發上限都在那裡。

## 內部 API

呼叫端（assistant、knowledge、alerts）在路徑裡指定模型角色，本文用該角色供應商的原生格式：

| 方法與路徑 | 協定 |
| --- | --- |
| `POST /internal/roles/{role}/v1/messages` | Anthropic Messages；本文 `"stream": true` 時以 SSE 串流回應 |
| `POST /internal/roles/{role}/v1/decisions` | OpenAI Decisions，不串流 |

請求：

- 標頭 `ai-gateway-tenant`（必填，剛好一個）：呼叫是為哪個租戶，1 到 128 個英數字或 `.`、`_`、`:`、`-`。閘道只記在日誌裡（帳本與額度在 Laysi/trustiot-v2#140）。
- 標頭 `anthropic-beta`（選填，只用於 Messages）：以逗號分隔的 beta 名稱，每個 1 到 128 個英數字或 `.`、`_`、`-`，最多 32 個，原樣轉給 Anthropic。
- 其他標頭都不轉送：`x-api-key`、`authorization` 由閘道用自己保管的 key 填入，`anthropic-version` 固定是 `2023-06-01`。
- 本文：UTF-8 的 JSON 物件，頂層欄位不可重複；Messages 的 `stream` 若有，要是 `true` 或 `false`。上限 `max_request_bytes`（預設 32 MiB，超過回 413）。閘道只把頂層 `model` 的值換成這次嘗試的模型（沒有 `model` 就加在最前面），其餘位元組原樣送出：thinking 區塊與它的 `signature`、快取斷點、對話中的 system 訊息、鍵的順序、空白與數字寫法都不變。Decisions 的本文送出前由 ai-clients 依 `DecisionRequest` 檢查（不符回 400，不送出）。

成功的回應：

- 非串流：HTTP 200，本文是供應商回應的原樣位元組，`content-type: application/json`。標頭 `ai-gateway-model` 是回答的模型（`<provider>/<model>`），`ai-gateway-outcome` 是 `completed`、`refusal`（Anthropic 的 `stop_reason: refusal`；Decisions 有任何一題拒答）或 `max_tokens_without_text`（thinking 用光 `max_tokens`），供應商的請求 ID 照各自的標頭名稱交回（Anthropic `request-id`、OpenAI `x-request-id`）。
- 串流：HTTP 200，`content-type: text/event-stream; charset=utf-8`，帶 `ai-gateway-model` 與 `request-id`。事件照收到的內容轉送（SSE 註解與 `id` 欄位不轉送，見 ai-clients 的「原樣轉送」）。串流在第一個內容區塊事件（`content_block_start` 或 `content_block_delta`）之後才回應標頭，之前收到的事件（`message_start`、`ping`）先暫存；所以第一個輸出之前的失敗還能改用下一個模型，呼叫端不會看到兩個模型的事件。串流的結果（拒答等）在串流自己的 `message_delta` 裡，沒有 `ai-gateway-outcome`。
- 拒答是模型的回答，不是錯誤：照常回 200，不換模型（ai-assistant.md 「AI 閘道」第 2 點）。

### 範例：一次最小的呼叫

確認一把供應商金鑰能用（例如輪替 Anthropic 金鑰後，[展示環境 runbook](../../docs/runbooks/demo-environment.md)「輪替 Resend 與 Anthropic 金鑰」）就送一次最小的 Messages 呼叫。`<gateway>` 是閘道的位址與連接埠，`main` 換成設定裡用那個供應商的角色；內部 API 只開給 mesh 授權的呼叫端，所以在叢集裡以 `kubectl port-forward` 連進閘道的 pod 送出，`<gateway>` 是轉出來的本機埠（chart README「AI 閘道」）。

```sh
curl -sS -D - -X POST "http://<gateway>/internal/roles/main/v1/messages" \
  -H 'content-type: application/json' -H 'ai-gateway-tenant: key-check' \
  -d '{"max_tokens":16,"messages":[{"role":"user","content":"Reply with OK."}]}'
```

HTTP 200 而且 `ai-gateway-model` 是那個供應商的模型，表示金鑰可用（角色有備援時，看這個標頭才知道是哪個模型回答的）；502 `api_error` 加上 `the provider refused the gateway's credentials` 表示供應商拒絕了金鑰（401 或 403）。

## 錯誤

錯誤用呼叫端協定自己的錯誤格式，供應商的 SDK 讀得懂：Anthropic `{"type":"error","error":{"type":…,"message":…}}`，OpenAI `{"error":{"message":…,"type":…,"param":null,"code":…}}`。錯誤回應帶 `cache-control: no-store`，訊息不引用請求的內容。

| 情況 | Anthropic | OpenAI |
| --- | --- | --- |
| 沒有這個角色 | 404 `not_found_error` | 404 `invalid_request_error`，`code: role_not_found` |
| 角色是另一種協定 | 400 `invalid_request_error` | 400 `invalid_request_error` |
| 缺少或不合格的 `ai-gateway-tenant`、`anthropic-beta`，本文不是合格的 JSON 物件，Decisions 請求不合格 | 400 `invalid_request_error` | 400 `invalid_request_error` |
| 本文超過 `max_request_bytes` | 413 `request_too_large` | 413 `invalid_request_error` |
| 最後一個模型的供應商錯誤（401、403 以外的 4xx、5xx） | 供應商的狀態碼、`type` 與訊息（裡面的 key 換成 `[REDACTED]`；訊息最多 2048 字，`type` 最多 256 字），`retry-after` 夾到 1 小時內再交回 | 同左，加 `code`（同樣遮蔽，最多 256 字） |
| 供應商拒絕閘道的 key（401、403），或回 4xx、5xx 以外的狀態 | 502 `api_error`，不帶供應商的訊息 | 502 `server_error` |
| 最後一個模型逾時、時間預算用完，或請求本文沒在時間預算內送完 | 504 `timeout_error` | 504 `server_error` |
| 每個模型都到了並發上限 | 529 `overloaded_error` | 503 `server_error` |
| 讀不到模型角色的設定（Postgres 連不上、查詢失敗，或超過 `database.timeout`）；不呼叫供應商 | 503 `api_error` | 503 `server_error` |
| 連不上供應商、回應不合協定 | 502 `api_error` | 502 `server_error` |

串流的錯誤：第一個輸出之前失敗、而且沒有下一個模型時，如果失敗是供應商的 `error` 事件，暫存的事件與那個 `error` 事件照原樣以 200 串流交回；其他失敗照上表回 JSON。第一個輸出之後的失敗不換模型：供應商的 `error` 事件照原樣轉送後結束；逾時（`stream_idle`、`stream_total`）或連線中斷時，閘道補一個 `event: error`（`timeout_error` 或 `api_error`）後結束。供應商的 `error` 事件轉送前一樣遮蔽 key（見「金鑰與 TLS」）。

## 備援

角色的 `models` 依序嘗試。換下一個模型的條件（ai-clients 的 `ErrorClass`）：

- 429（`RateLimited`，含 Anthropic 的月消費上限、OpenAI 的 `insufficient_quota`）
- 過載（`Overloaded`：Anthropic 529 或 `overloaded_error` 事件，OpenAI 503）
- 逾時（`Timeout`：供應商的 504 或 `timeout_error`，以及角色的 `connect`、`first_token`、`response` 等上限）
- 模型的並發上限已滿（`ConcurrencyLimited`，請求沒有送出）

其他錯誤（400、401、500 等）、拒答與串流第一個輸出之後的失敗都不換模型，直接交回。每個模型的請求都是同一份本文，只有 `model` 不同。快取依模型分開，換模型後從零開始。

## 時間上限

- 每次嘗試受角色的 `timeouts` 限制（ai-clients 的 `Timeouts`）：`connect` 連線與 TLS 交握；`response` 非串流的回應標頭與本文；`first_token` 串流從送出到第一個內容區塊事件；`stream_idle` 串流兩段資料之間；`stream_total` 整條串流。另有 `queue_wait`：等並發名額的時間，等不到算過載。
- 整個呼叫（讀請求本文加上所有嘗試，到非串流的回應或串流的第一個輸出為止）不超過時間預算：service-kit 的 `http.request_timeout` 減去它的十分之一、最多 2 秒。預算用完回 504（呼叫端協定的格式），不再嘗試下一個模型；這比 service-kit 自己沒有本文的 504 早到。
- 啟動時檢查每個角色：`queue_wait` 加上 `response` 與 `first_token` 較長者，要在時間預算內，否則不啟動（錯誤訊息寫出角色的設定名稱與 `AI_GATEWAY__HTTP__REQUEST_TIMEOUT`）。service-kit 的預設 `http.request_timeout` 是 30 秒，角色預設的 `response` 是 300 秒，所以部署一定要調高 `AI_GATEWAY__HTTP__REQUEST_TIMEOUT`，例如 `10m`（上限 1 小時）。
- 串流的第一個輸出之後不受時間預算限制，只受 `stream_idle` 與 `stream_total`。

## 設定

設定來自 `AI_GATEWAY__` 開頭的環境變數（`__` 分隔層級，名稱轉小寫；清單以逗號分隔；時間長度寫成 `30s`、`10m`），或 `AI_GATEWAY_CONFIG_FILE` 指的 TOML 檔，與 service-kit 的其他設定（`http`、`shutdown`、`telemetry`，見 [crates/service-kit/README.md](../../crates/service-kit/README.md)）相同的規則。設定只在啟動時讀，改設定要重新啟動；唯一的例外是模型角色的模型清單，它可以在執行中以存進資料庫的清單取代（見「資料庫」），其他欄位（角色有哪些、供應商、協定、上限與時間）仍只來自設定。錯誤訊息只寫設定名稱，不寫值。

供應商（`providers.<name>`，名稱是 1 到 32 個小寫英文字母、數字或 `_`，字母開頭）：

| 鍵 | 環境變數（名稱 `anthropic` 為例） | 預設 | 說明 |
| --- | --- | --- | --- |
| `protocol` | `AI_GATEWAY__PROVIDERS__ANTHROPIC__PROTOCOL` | 必填 | `anthropic-messages` 或 `openai-decisions` |
| `base_url` | `AI_GATEWAY__PROVIDERS__ANTHROPIC__BASE_URL` | 供應商自己的 API（`https://api.anthropic.com`、`https://api.openai.com`） | API 的 origin（可帶路徑前綴），例如 Cloudflare AI Gateway 的 `https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/anthropic`。只收 `https://`；`http://` 只限 loopback。不可帶帳密、query 或 fragment。需要額外驗證標頭的 gateway 目前不支援 |
| `key_env` | `AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV` | 必填 | 放 API key 的環境變數名稱，例如 `ANTHROPIC_API_KEY`、`OPENAI_API_KEY`：大寫英文字母、數字或 `_`，不可以 `AI_GATEWAY_` 開頭（那是設定的範圍）。key 本身不放在設定裡 |

模型角色（`roles.<name>`，名稱規則同上）：

| 鍵 | 環境變數（名稱 `main` 為例） | 預設 | 說明 |
| --- | --- | --- | --- |
| `models` | `AI_GATEWAY__ROLES__MAIN__MODELS` | 必填 | `<provider>/<model>`，第一個是首選，其後是備援，1 到 8 個、不可重複；provider 要在 `providers` 裡，而且同一個角色的模型要是同一種協定。模型 ID 是 1 到 256 個可列印 ASCII、不含空白，例如 `anthropic/claude-opus-5-5,anthropic/claude-sonnet-5-5` |
| `max_concurrent` | `AI_GATEWAY__ROLES__MAIN__MAX_CONCURRENT` | `32` | 這個角色的每個模型同時進行的呼叫數（每個角色、每個模型各自計算），1 到 10000 |
| `queue_wait` | `AI_GATEWAY__ROLES__MAIN__QUEUE_WAIT` | `0s` | 名額滿時等多久，等不到算過載、換下一個模型；上限 1 小時 |
| `timeouts.connect` | `AI_GATEWAY__ROLES__MAIN__TIMEOUTS__CONNECT` | `10s` | 見「時間上限」；每項都要大於 0、不超過 1 小時 |
| `timeouts.first_token` | `AI_GATEWAY__ROLES__MAIN__TIMEOUTS__FIRST_TOKEN` | `60s` | |
| `timeouts.stream_idle` | `AI_GATEWAY__ROLES__MAIN__TIMEOUTS__STREAM_IDLE` | `60s` | |
| `timeouts.stream_total` | `AI_GATEWAY__ROLES__MAIN__TIMEOUTS__STREAM_TOTAL` | `60m` | 上限 24 小時 |
| `timeouts.response` | `AI_GATEWAY__ROLES__MAIN__TIMEOUTS__RESPONSE` | `300s` | |
| `max_response_bytes` | `AI_GATEWAY__ROLES__MAIN__MAX_RESPONSE_BYTES` | 32 MiB | 非串流回應本文、整條串流的上限；1 KiB 到 256 MiB |
| `max_event_bytes` | `AI_GATEWAY__ROLES__MAIN__MAX_EVENT_BYTES` | 4 MiB | 單一 SSE 事件的上限；1 KiB 到 `max_response_bytes` |

其他：

| 鍵 | 環境變數 | 預設 | 說明 |
| --- | --- | --- | --- |
| `max_request_bytes` | `AI_GATEWAY__MAX_REQUEST_BYTES` | 32 MiB | 請求本文上限（Anthropic Messages 的上限）；1 KiB 到 256 MiB |
| `http.request_timeout` | `AI_GATEWAY__HTTP__REQUEST_TIMEOUT` | `30s`（service-kit） | 見「時間上限」；部署要調高 |

資料庫（見「資料庫」；chart 從平台設定與 migration Job 產生的密碼填入這些變數，見 chart README「AI 閘道」）：

| 鍵 | 環境變數 | 預設 | 說明 |
| --- | --- | --- | --- |
| 平台設定 | `AI_GATEWAY_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 必填 | Postgres 的位置與驗證伺服器憑證的 CA，與其他服務的 `TRUSTIOT_POSTGRES_*` 相同，只是用閘道自己的前綴；`_SSLMODE` 只收 `verify-full` |
| `database.username` | `AI_GATEWAY__DATABASE__USERNAME` | 必填 | 服務角色 `ai_gateway_app` |
| `database.password` | `AI_GATEWAY__DATABASE__PASSWORD` | 必填 | 由 Secret 提供；不進日誌與 `Debug` |
| `database.max_connections` | `AI_GATEWAY__DATABASE__MAX_CONNECTIONS` | `4` | 每個副本的連線池大小，1 到 32 |
| `database.timeout` | `AI_GATEWAY__DATABASE__TIMEOUT` | `5s` | 每個轉送請求等設定確認（取得連線、讀世代、必要時重載）的上限，100 ms 到 60 s，而且不超過這次呼叫剩下的時間預算；超過回 503 |

至少要有一個角色。一個最小的設定：

```sh
AI_GATEWAY__PROVIDERS__ANTHROPIC__PROTOCOL=anthropic-messages
AI_GATEWAY__PROVIDERS__ANTHROPIC__KEY_ENV=ANTHROPIC_API_KEY
AI_GATEWAY__PROVIDERS__OPENAI__PROTOCOL=openai-decisions
AI_GATEWAY__PROVIDERS__OPENAI__KEY_ENV=OPENAI_API_KEY
AI_GATEWAY__ROLES__MAIN__MODELS=anthropic/claude-opus-5-5,anthropic/claude-sonnet-5-5
AI_GATEWAY__ROLES__ROUTER__MODELS=openai/gpt-6-luna
AI_GATEWAY__HTTP__REQUEST_TIMEOUT=10m
# 加上 ANTHROPIC_API_KEY、OPENAI_API_KEY（由 Secret 提供），
# AI_GATEWAY_POSTGRES_*、AI_GATEWAY__DATABASE__USERNAME 與 AI_GATEWAY__DATABASE__PASSWORD
```

本機開發環境（`bun scripts/dev.ts`）的 `env` 印出 `TRUSTIOT_POSTGRES_*` 與 `TRUSTIOT_DEV_AI_GATEWAY_DATABASE_{USERNAME,PASSWORD}`，改名成上面的變數就能在主機上以 `cargo run -p ai-gateway` 啟動（[scripts/README.md](../../scripts/README.md)）。

## 資料庫

chart 的 `ai.roles` 決定有哪些角色、各自的協定、上限、時間與預設的模型；Postgres 的 schema `ai_gateway`（決策 29：owner `ai_gateway_owner`、服務角色 `ai_gateway_app`，沒有平台角色、沒有 outbox，都不是租戶資料）只存模型清單的覆寫，部署不會丟掉管理員的更換（Laysi/trustiot-v2#537 的 2026-10-23 決定）。

| 表 | 內容 | 服務角色的權限 |
| --- | --- | --- |
| `role_models` | 每個更換過的角色一列：`models`（1 到 8 個 `<provider>/<model>`，重設之後是 NULL）、`version`、`updated_at`、`updated_by` | SELECT、INSERT、UPDATE（不能 DELETE、TRUNCATE） |
| `settings_generation` | 只有一列的 `generation`，每次更換或重設加一 | SELECT、UPDATE（只有 `generation` 欄） |
| `role_model_changes` | append-only 的變更紀錄：`version`、`role`、`action`（`set` 或 `reset`）、`models`、`changed_at`、`changed_by` | SELECT、INSERT（UPDATE、DELETE、TRUNCATE 被資料庫拒絕） |

更換與重設（`src/store.rs` 的 `Store::set_models`、`Store::reset`；管理 API 在 Laysi/trustiot-v2#606）：

- 先檢查，被拒絕的更換什麼都不寫：模型清單的規則與設定的 `models` 相同（1 到 8 個不重複的 `<provider>/<model>`、供應商有設定、模型 ID 是不含空白與逗號的可列印 ASCII），而且每個模型的供應商協定要等於角色的協定；角色要是設定裡有的。換協定是 chart 加上呼叫端程式的變更，不能靠覆寫做到。重設接受任何合格的角色名稱，所以 chart 移除的角色留下的清單也能清掉。
- 再在一個交易裡：世代加一（它的列鎖讓寫入者排隊）、鎖住角色的列並比對呼叫端讀到的版本（角色的列的 `version`，沒有列的角色是 0；不符回衝突，什麼都不寫）、寫入覆寫或把它清成 NULL、寫一筆變更紀錄、commit。
- 版本就是這次更換之後的世代，所以重設之後再更換，新版本仍比重設前大。重設保留角色的列與重設的版本，不刪除：帶著重設前任何版本（包括從沒更換過時讀到的 0）的更換都是衝突，不會在別人的更換與重設之後悄悄蓋掉。重設沒有覆寫的角色（從沒更換過或已經重設）什麼都不寫。

存下的清單對目前的設定不再成立時（供應商被移除、chart 改了角色的協定、角色被移除），該角色忽略它、用設定的模型，記 ERROR `ignored the stored models of a model role; it uses its configured models`（`role`、`version`、`reason`），快照的 `ignored` 也標記它，給之後的管理 API 顯示。使用存下的清單時記 INFO `model role uses stored models`。

讀不到設定時不放行（與 Laysi/trustiot-v2#140 帳本的規則一致）：Postgres 連不上、查詢失敗或超過 `database.timeout`，轉送請求以呼叫端協定的格式回 503，不呼叫供應商，記 WARN `refused a model call: the model role settings could not be checked`。readiness 是 migration 檢查（`migrations`，schema 的 migration 都已套用）加上讀一次世代（`postgres`）。

## 多副本

更換的呼叫回 200 之後才開始的請求，在任何副本都用新的模型。

做法：每個轉送請求在讀完本文之後讀一次 `SELECT generation`（小的連線池，算在這次呼叫的時間預算內）。與記憶體裡快照的世代相同就用快照；資料庫的世代較新時，在 `REPEATABLE READ READ ONLY` 的交易裡一次讀入所有覆寫與世代，建新的快照，只在它比目前的新時換上（同一個副本同時只有一個重載）。重載以（角色、`<provider>/<model>`）為鍵重用既有的 client，所以沒變的模型的並發計數不會歸零。進行中的請求、串流與同一個請求內的備援，沿用它開始時拿到的快照；串流結束前它的 client 不會被釋放。

資料庫的世代比副本記憶體裡的舊（例如從備份還原）時不會重載：還原之後要重新啟動閘道。

## 金鑰與 TLS

- 每個供應商的 key 在啟動時從 `key_env` 指的環境變數讀一次；沒設、空的或含空白與控制字元時不啟動，錯誤訊息只寫變數名稱。key 只存在 ai-clients 的 `ApiKey`（`Debug` 不顯示、drop 時清除、標頭標成 sensitive）與閘道用來遮蔽錯誤訊息的副本（同樣在 drop 時清除）；環境變數本身仍留在程序環境裡，所以 chart 只讓這個 pod 拿得到那個 Secret（`services.<name>.credentials` 的 `ai` 只能給 `ai-gateway`，變數是 `TRUSTIOT_AI_ANTHROPIC_API_KEY`、`TRUSTIOT_AI_OPENAI_API_KEY`）。
- key 不進日誌也不進回應：供應商錯誤訊息裡的 key 換成 `[REDACTED]`，401 與 403 的訊息整個不交回。串流的 `error` 事件先解析成 JSON，在解碼後的每個字串與欄位名稱、以及原始文字裡找 key，所以用 JSON 跳脫寫成的 key 也會被換掉；找到時事件資料改寫成遮蔽後的 JSON（欄位順序可能改變），沒找到時照原樣轉送，不是 JSON 的資料換成閘道自己的 `api_error` 事件。供應商的錯誤 `type`、`code` 與 request ID 也遮蔽後才交回或寫進日誌。成功的回應與串流照原樣轉送，不檢查。`tests/process.rs` 以 trace 等級執行真正的程式檢查。
- 連供應商的 TLS 信任系統的根憑證（`SSL_CERT_FILE` 或 `SSL_CERT_DIR` 有設時用它們），一張都載不到就不啟動，所以映像要有 CA bundle。不走 proxy、不跟隨重新導向（ai-clients 的「規則」）。
- 閘道本身不驗證呼叫端：內部 API 只靠 mesh 授權，只有 assistant、knowledge、alerts 能連（決策 28；chart 在 `mesh.calls` 把其他服務列為呼叫端時拒絕渲染，見 chart README「服務間授權（mesh）」）。

## 日誌

每次呼叫記一行，欄位有 `role`、`protocol`、`tenant`、`model`、`attempts`、`provider_request_id`、`duration_ms`：成功是 INFO `model call completed`（加 `streaming`、`outcome`），換模型是 WARN `falling back to the next model`（加 `next_model`、`class`、`status`、`error_type`），失敗是 WARN `model call failed`（串流途中是 `model stream failed`，加 `class`、`status`、`error_type`，連線與協定錯誤加 `detail`），呼叫端在串流結束前離開是 WARN `the caller left before the stream ended`。啟動時每個角色記一行 INFO `model role ready`（`role`、`protocol`、`models`，啟動時資料庫還沒讀，所以是設定的模型）；讀不到設定、忽略或使用存下的模型清單的日誌見「資料庫」。不記請求或回應的本文、供應商的錯誤訊息與 key。

## 關機

service-kit 對長連線的建議是在 `Shutdown::stopping` 時結束 SSE，讓用戶端改連新的 pod；閘道刻意不這樣做：模型的回答不能在另一個 pod 接續，提早結束只會讓呼叫端整段重來並重複計費。進行中的串流在 drain 期間照常轉送到結束，`shutdown.drain_timeout`（預設 20 秒）到了才隨程序結束被切斷。chart 給閘道的 `drainSeconds` 預設是 310 秒，並把 `AI_GATEWAY__SHUTDOWN__DRAIN_TIMEOUT` 設成它減 10 秒（300 秒），涵蓋一般的回答長度（service-kit 的「收尾」一節說明兩者的關係；chart README「AI 閘道」）。

## 測試

`cargo test -p ai-gateway`。`tests/process.rs` 與 `tests/roles_store.rs` 以 podman 或 docker 啟動 service-kit 的測試用 TLS Postgres（以路徑引入 `crates/service-kit/tests/support/postgres.rs`），沒有容器執行環境時跳過（設了 `CI` 時失敗）。不連真正的供應商、不用真的 key：供應商是 loopback 上的腳本化 HTTP/1.1 伺服器（以路徑引入 `crates/ai-clients/tests/support/mod.rs`），回應用 ai-clients 的錄製樣本（`crates/ai-clients/tests/fixtures/`），key 在執行時產生。

| 檔案 | 內容 |
| --- | --- |
| `tests/messages.rs` | 本文除了 `model` 逐位元組相同（thinking 與 signature、快取斷點、對話中的 system 訊息）、缺 `model` 時補上、串流原樣交回、第一個輸出之後的錯誤原樣轉送且不換模型、拒答的串流、各種不能轉送的請求在送出前拒絕、本文沒在時間預算內送完時回 504 |
| `tests/fallback.rs` | 429、529、供應商 504、閘道自己的逾時（`TIMEOUT_UNDER_TEST`，15 秒）與並發上限都換下一個模型；串流第一個輸出之前的 `error` 事件與 429 也換；全部失敗時交回最後一個錯誤；400、401、500 與拒答不換；供應商訊息裡的 key 被遮蔽 |
| `tests/decisions.rs` | 錄製的 Decisions 請求逐位元組轉送、回應原樣交回、拒答、429 與 503 換模型、不合格的請求與未知角色 |
| `tests/process.rs` | 真正的程式：以測試 CA 與 `SSL_CERT_FILE` 走 TLS、key 從 `key_env` 讀取並送到供應商、trace 等級的日誌與回應裡都沒有 key 與資料庫密碼；沒有 key、沒有 Postgres 的平台設定或角色的時間上限超過 `http.request_timeout` 時不啟動；兩個程序共用一個資料庫時，一邊的更換在另一邊的下一個請求生效；重新啟動後用最後一次更換；不再成立的覆寫被忽略並記 ERROR |
| `tests/roles_store.rs` | 更換與重設：被拒絕的更換不寫任何東西、每次更換寫一筆紀錄、重設之後版本仍上升與衝突、服務角色不能改寫或刪除變更紀錄；下一個請求用新的模型、只在世代改變時重載；資料庫連不上（從啟動起、或中途斷線）時回 503 而不呼叫供應商；進行中的串流保留它的模型；重載不讓沒變的模型的並發計數歸零 |
| `tests/no_contract.rs` | `cargo tree -p ai-gateway` 沒有 `contract` |

`src/` 裡的單元測試涵蓋設定檢查、本文的換 `model`、錯誤格式與備援條件，以及 key 的遮蔽：互相重疊的 key、錯誤的 `type` 與 `code`、串流 `error` 事件裡用 JSON 跳脫寫成的 key。
