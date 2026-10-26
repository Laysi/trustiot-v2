# assistant

AI 助理（決策 31）的主 agent 迴圈：使用者對自己租戶的資料發問，主 agent 透過工具讀資料、回答。對話歷史只往後加、依 prompt caching 的順序排列，每個問題以 token 預算收斂，不設步數上限。只經由 AI 閘道（[services/ai-gateway](../ai-gateway/README.md)）呼叫 Claude，模型藏在一個介面後面（決策 15）。設計在 [docs/architecture/ai-assistant.md](../../docs/architecture/ai-assistant.md)；本服務由 Laysi/trustiot-v2#145 建立。

公開 API（`/api/assistant/v1`）目前只有儀表板（Laysi/trustiot-v2#150，見「儀表板」）；聊天 API 與串流是 Laysi/trustiot-v2#146。

## 檔案

| 路徑 | 內容 |
| --- | --- |
| `src/agent.rs` | 主 agent 迴圈：呼叫模型、執行工具、收尾、錯誤與日誌 |
| `src/model.rs` | 模型介面 `ModelClient` 與中性型別（`ModelRequest`、`ModelTurn`、`TokenCount`、`ModelError`）；原生格式的訊息是不透明的 `NativeMessage` |
| `src/model/anthropic.rs` | 經 AI 閘道的 Anthropic Messages：請求的快取排列、回應解析、錯誤對應 |
| `src/history.rs` | 對話：固定的前段（`Preamble`）與只往後加的歷史 |
| `src/tools.rs` | `Tool` trait、`ToolRegistry`（上限、並行、逾時、結果大小）、`ToolContext` 與 `Delegation` |
| `src/tools/telemetry.rs` | 工具 `telemetry_latest`（telemetry-query 的歷史 API） |
| `src/tools/semantic.rs` | 工具 `telemetry_statistics`、`telemetry_usage`（telemetry-query 的語意查詢 API，Laysi/trustiot-v2#142） |
| `src/tools/device_models.rs` | 工具 `device_models`（assets 的設備型號列表） |
| `src/tools/telemetry_sql.rs` | 工具 `telemetry_sql`（telemetry-query 的內部 SQL 入口），拒絕類別對應的固定說明 |
| `src/callee.rs` | 工具與儀表板的資料集執行以使用者身分呼叫其他服務的 HTTP client（GET 與 JSON 本文的 POST）：每次呼叫換 token、錯誤只看狀態碼（`telemetry_sql` 的 422 例外，嚴格解析成封閉的拒絕型別）；`Caller` 是呼叫代表誰、日誌記成哪個工具或資料集執行 |
| `src/delegation.rs` | 一次提問的 run 委派：`QuestionRuns::start`（聊天 API 的入口）、`QuestionRun`（截止時間、撤銷）；`OnBehalfOf` 以公開 API 請求的身分換 token（資料集執行） |
| `src/api.rs` | 公開 API（`/api/assistant/v1`）的共用部分：每個請求驗證內部身分、`cache-control: no-store`、M1 錯誤、嚴格的請求本文；`openapi()` 的文件 |
| `src/api/dashboards.rs` | 儀表板的路由：列表、開啟、版本、發布、還原、封存、執行資料集 |
| `src/dashboards/store.rs` | 儀表板與版本的 Postgres 儲存：一個函式一個完整操作、鎖、誰看得到與誰能改（草稿的可見性只由 `conversation_access` 決定） |
| `src/dashboards/runner.rs` | 資料集的執行：以呼叫者的身分呼叫 telemetry-query，回答與錯誤對應成封閉的結果 |
| `src/dashboards/caps.rs` | 資料集執行的上限：每個租戶、每個副本每分鐘 60 次、同時 2 個 |
| `src/dashboards/flatten.rs` | 語意資料集（統計、用量）的回答攤平成表格 |
| `src/budget.rs` | 每個問題的 token 預算 |
| `src/prompt.rs` | 平台 system prompt（有版本）與對話中的固定指示 |
| `src/config.rs` | 設定與啟動檢查 |
| `src/store.rs` | 對話與歷史的 Postgres 儲存：只往後加、原樣保存、依租戶隔離 |
| `src/schema.rs` | 本版本預期的 migration 檔（`migrations/postgres/`：`0001_assistant.sql` 對話、`0002_dashboards.sql` 儀表板），`tools/migrate` 也編進它 |

## 一次問答怎麼跑

1. 問題以使用者訊息加到歷史最後。
2. 呼叫模型。請求本文依序是：`tools`（依名稱排序）、`system` 的兩個區塊（平台 system prompt、租戶脈絡，各有一個快取斷點；租戶脈絡是成員輸入的文字，跳脫 `&`、`<`、`>` 後包在 `<tenant_context>` 標籤裡，system prompt 說明它是資料）、`messages`（整段歷史，加上頂層 `cache_control` 的自動快取）。另有 `thinking: {"type": "adaptive"}`、`output_config.effort` 與 `max_tokens`；不帶 `model`，由閘道依角色填入。
3. 模型要求工具時，同一回合的工具並行執行，各有時間上限；結果依呼叫順序放在一則使用者訊息交回。模型的這一回合連同工具結果一起加到歷史，然後回到第 2 步。
4. 模型回答時，回答加到歷史，問答結束。

一次提問最長到它的截止時間（見下方「代表使用者呼叫」）：時間到就以「提問超時」結束，不論正在呼叫模型或執行工具。歷史只在一個模型回合與它的工具結果都完成時才一起加入，所以在任何時點中斷都不會留下半個步驟。

步數沒有上限。限制來自 token 預算：每次呼叫處理的 token（輸入、快取寫入、快取讀取、輸出，閘道的每次嘗試都算）累計到 `budget.wind_down_percent`（預設 80%）時，下一次呼叫前加一則對話中的 system 訊息，請模型不再呼叫工具、用現有的資料回答；累計到整個預算時，模型再要求的工具不執行（每個呼叫回一個說明原因的錯誤結果），問答以「預算用完」結束。每次呼叫送出前先檢查放得進剩下的預算：輸入以模型上一次回報的歷史大小（`Conversation::measured`）加上之後新增訊息的高估值估計（JSON 每 2 位元組算 1 token，每張圖 5,000 token；對話的第一次呼叫連 system 與工具定義一起估），`max_tokens` 降到剩下的空間；不到 1,024 token 時不送出，以「預算用完」結束。供應商沒回報用量時，以請求或回應的位元組數估計（每 3 位元組算 1 token），不當 0。

### 歷史只往後加

- 每則訊息以收到或第一次寫出的 JSON 文字保存，之後原樣送回：thinking 區塊與 signature、工具呼叫、未知的區塊與欄位都不變。所以下一次請求的 `messages` 一定以上一次的 `messages` 開頭，逐位元組相同。
- system prompt 與版本、租戶脈絡、工具組、effort 與快取時效在對話開始時固定（`Preamble`），之後不變；新的 prompt 或工具只用在新對話。
- 收尾指示是對話中的 system 訊息，只跟著它引出的模型回覆一起加入歷史；呼叫失敗或拒答時不留下，避免 system 訊息之後直接接下一個問題。
- 工具步驟與結果在聊天畫面隱藏（`EntryKind::shown`），但留在歷史裡。
- 拒答或內容不完整的回合不加入歷史；之後的問題照常接在後面。

## 工具

工具在每次呼叫其他服務前，以這次提問的委派換一張受眾是該服務的 token（`Delegation::token_for`），帶在 `x-internal-identity`，呼叫的是 web 前端經入口 gateway 呼叫的同一組 API（`/api/...`）；`telemetry_sql` 是例外，呼叫只給 AI 用的內部入口 `POST /internal/telemetry/v1/sql`（入口 gateway 不轉送 `/internal`，瀏覽器送不了 SQL）。被呼叫端照一般請求檢查 token 的受眾、使用者的租戶與角色，所以工具讀得到的正是使用者自己讀得到的（決策 27、29）。

| 工具 | 呼叫 | 輸入 | 回給模型 |
| --- | --- | --- | --- |
| `telemetry_latest` | telemetry-query `GET /api/telemetry/v1/devices/{device_id}/latest` | `device_id`，選填 `points`（最多 100 個資料點） | 每個資料點一列 `[資料點, 時間, 值]`，依資料點排序，與總數 `total`；最多 200 列 |
| `telemetry_statistics` | telemetry-query `POST /api/telemetry/v1/statistics` | API 的請求本文：`series`（1 到 20 個 `{device_id, point}`）、`tz`（IANA），`period`（例如 `this_month`、`last_7_days`）或 `from` 加 `to`，選填 `interval`（`1m` … `1mo`、`total`，預設 `total`） | 實際查詢的範圍（`range`），每個資料點的筆數、最小與最大（含所在分鐘或小時的起點）、平均、第一與最後讀數；有 `interval` 時另有每個有讀數的區間一列 `[起點, 筆數, 最小, 最大, 平均, 最後讀數]` 與平均最高、最低的區間；`notes` 說明沒有資料（資料不足）與被截到可查期間；最多 500 列，超過時只留各資料點的數字 |
| `telemetry_usage` | telemetry-query `POST /api/telemetry/v1/usage` | 同 `telemetry_statistics` | 每個資料點的 `status`（`ok`、`no_data`、`not_numeric`、`not_cumulative`），`ok` 的有 `total`、讀數、重置、斷訊、可疑跳增與累計檢查；有 `interval` 時另有每個有讀數的區間一列 `[起點, 用量, 讀數, 重置, 斷訊後, 可疑跳增]` 與用量最高、最低的區間；`notes` 是每種狀況給模型的固定說明（改用 `telemetry_statistics`、資料不足、無法確認是累計值、總量可能含錶的誤讀）；沒有任何資料點算得出用量時是錯誤結果；最多 500 列 |
| `device_models` | assets `GET /api/assets/v1/device-models` | 選填 `name_contains` | 每個型號一列 `[型號 ID, 名稱, 版本, 資料點數, 更新時間]`，依名稱排序，與總數 `total`；最多 200 列 |
| `telemetry_sql` | telemetry-query `POST /internal/telemetry/v1/sql`（[services/telemetry-query](../telemetry-query/README.md)「AI 的唯讀 SQL」） | `sql`（一個 `SELECT`，最多 16 KiB）；列數上限固定送 200，模型改不了 | `columns`（名稱與 ClickHouse 型別）、`rows`、`row_count`、各 view 的 `queryable_from` 與 `queried_at`；服務印回的 SQL 不回給模型。超過 200 列時服務整個拒絕、不截斷 |

- **租戶與使用者只來自 `ToolContext.principal` 與 token**：輸入的 schema 沒有租戶或使用者欄位（`additionalProperties: false`），多出任何欄位（例如 `tenant_id`）時工具不呼叫、回錯誤說明可用的欄位；被呼叫端收不到租戶參數。
- **呼叫前先檢查**：`device_id` 要是 UUID，資料點 key 要符合服務的規則，時間要是 RFC 3339。語意工具另檢查：1 到 20 個不重複的資料點、`period` 與 `from`/`to` 只給一種、`period` 的寫法、`from` 至少早於 `to` 一分鐘、列數（資料點數乘區間數，以名目長度估計）不超過服務的 10,000 列、範圍要碰得到可查期間（與 telemetry-query 以帳號時間窗算的 1 分鐘表起點相同：3 個月的時間窗在 5 分鐘後的起點，這 5 分鐘跨過月底往回跳的午夜時取午夜前的起點，單元測試逐分鐘比對兩年的每個月底；相對期間裡只有 `last_year` 可能整段在可查期間之前）、`to` 不晚於現在之後 367 天（telemetry-query 的 `range::MAX_AHEAD`；相對期間最長到 366 天後，所以不會碰到）。不符合時不呼叫，回錯誤說明怎麼改。這些常數與 telemetry-query 的相同，`tests/delegated_tools.rs` 比對兩邊；語意工具的輸入 schema 也與 `api/openapi.json` 的 `SeriesQuery` 比對欄位、必填、上限、pattern 與 enum，所以工具的輸入就是 API 的本文，儀表板可以原樣存下再送。
- **語意工具的結果**：時間都帶 `tz` 的時差；`notes` 是工具自己的固定文字（只帶資料點 key、設備 ID 與數字），說明每個資料點的狀況與要對使用者說的話，例如 `not_cumulative`（讀數下降的次數對上升的次數太多，是瞬時值，改用 `telemetry_statistics`、不要把讀數相加）、`no_data`（資料不足，不要估計）、`too_few_readings`（無法確認是累計值，數字要說是未確認的）、可疑跳增（總量可能含錶的誤讀，要一併說明）。
- **結果**：只放被呼叫端回給這個使用者的欄位，精簡成表格；超過工具結果的大小上限（`tools.max_text_bytes`）時，`ToolRegistry` 改回錯誤並附各工具的縮小方式。被呼叫端的回應以本服務自己的型別解析，未知欄位忽略；`tests/delegated_tools.rs` 以 telemetry-query 與 assets 自己的回應型別產生回應，確認兩邊對得上。
- **裝置寫的文字是不可信的輸入**：文字讀數、資料點的 key（設備回報的）與型號名稱（成員輸入的）原樣進到模型的 context，可能夾帶要模型做別的事的指示（間接 prompt injection）。工具結果裡每個字串最多 256 個字元（`tools::MAX_RESULT_TEXT_CHARS`），超過的截斷並以 `…[cut]` 結尾；物件的 key 也一樣（例如 SQL 結果裡 ClickHouse `Map` 值的 key），截斷後與已有的 key 相同時加上 `#2`、`#3`…，不丟掉值。工具說明告訴模型這個上限，也說這些文字是資料、不是指示。上限擋得住的是單一值塞滿結果、或夾帶長篇指示；擋不住短的指示（256 個字元足以寫一句「忽略使用者的問題」），也不能讓模型分辨資料與指示。實際的保護是權限：每個工具只以提問者的身分讀資料，沒有寫入工具，模型被說服也拿不到使用者本來拿不到的東西（[docs/architecture/ai-assistant.md](../../docs/architecture/ai-assistant.md)「安全」）。
- **`telemetry_sql`**（Laysi/trustiot-v2#143）：工具說明列出三個 view 的欄位與型別、可查期間（原始讀數最多 7 天、1 分鐘與 1 小時彙總最多 3 個月，實際由方案與資料庫決定）、SQL 的規則（一個 SELECT、只讀這三個 view、時間函式要寫時區、CAST 的型別）與 200 列的上限。服務拒絕時回 422，`details` 是封閉的拒絕類別（`contract::api::telemetry_query::SqlRefusal`）：`Callee::post` 只在本文正好是錯誤本文的三個欄位、`code` 是 `validation_failed`、`details` 解析後再序列化完全相同時才接受，多一個欄位、型別不對或這個版本不認得的類別、子句、`kind` 都當成失敗。工具把類別換成固定的英文說明（該改什麼），只帶數字與模型自己寫的表名或函式名；名稱先以服務的識別字規則（`[A-Za-z_][A-Za-z0-9_]*`、每段最多 128 bytes、最多 3 段）再檢查一次，不符合就當成失敗，所以服務的文字不會進到模型。這個工具只給雲端模型；地端模型之後只用語意層工具（決策 16）。
- **錯誤**（`src/callee.rs`，GET 與 POST 同一條路徑）：只讀成功回應的本文，以及 `telemetry_sql` 的 422（上一點的嚴格解析；它的 `Post` 設 `Unprocessable::Refusal`，語意工具設 `Unprocessable::Rejected`，422 與 400 一樣只回固定說明）。其他錯誤回應只看狀態碼，本文不讀，所以不會進到模型或日誌：

| 被呼叫端 | 工具結果 | 日誌的 `outcome` |
| --- | --- | --- |
| 400、422 | 工具自己的固定說明（參數的規則與正確寫法）；`telemetry_sql` 的 422 是該拒絕類別的固定說明，不是正好的拒絕型別時改當失敗 | `input_error` |
| `telemetry_sql` 的 404（租戶的帳號還沒建立）、429（查詢額度用完）、504（查詢超過時間上限） | 各自的固定說明：這次提問不要再查、或改讀彙總表與縮小範圍 | `input_error` |
| 403、404（例如設備屬於別的租戶、不存在） | 固定訊息：服務拒絕了這個使用者的請求，不要以同樣的輸入再試 | `denied` |
| 401、其他 4xx、5xx、逾時、連線失敗、回應無法解析或超過 8 MiB | 固定訊息：工具失敗，可以再試一次 | `failed` |
| 身分服務拒絕換 token（委派被撤銷） | 提問結束，見下方 | `revoked` |

## 代表使用者呼叫

工具以 Laysi/trustiot-v2#551 的 run 委派代表使用者呼叫其他服務（[services/identity/README.md](../identity/README.md)「agent 執行期間的委派」）。聊天 API（Laysi/trustiot-v2#146）處理使用者的請求時：

1. 以請求的 `UserIdentity` 與對話的 `Principal` 呼叫 `QuestionRuns::start`（`src/delegation.rs`）。請求的使用者或租戶與對話的不同時拒絕（`StartError::NotTheConversationsUser`，M1 `permission_denied`），不呼叫身分服務。它產生 run ID 與這次 run 的 Ed25519 金鑰，在請求的 token 還有效時向身分服務建立委派（`identity.run_lifetime`，預設與上限都是 900 秒）；身分服務拒絕時回 `StartError::Delegation`，M1 code 照 `RunDelegationError::code`（session 已結束 `unauthenticated`、已不是租戶成員 `permission_denied`）。
2. 把得到的 `QuestionRun` 交給 `Agent::ask`。這次提問的日誌以委派的 run ID 記錄，與身分服務的日誌對得上。

**截止時間**：提問的截止時間是委派到期前 30 秒（`RUN_END_MARGIN`，依身分服務回的 `expires_at` 計算，不晚於要求的效期）。30 秒涵蓋兩台主機時鐘的差距與最後一次換 token 的往返，所以截止前開始的工具呼叫都換得到 token。時間到時提問以 `AgentError::RunExpired` 結束；建立時已經沒有時間的提問不送出任何東西、歷史不變。截止時間也會切斷進行中的模型呼叫（`model.call_timeout` 只限制單次呼叫）。

**撤銷**：身分服務以 `unauthenticated`（登出、session 被撤銷或過期、委派過期）或 `permission_denied`（使用者被移出租戶；或部署的 `mesh.calls` 沒有這個呼叫）拒絕換 token 時，該工具呼叫回 `ToolError::Revoked`：同一步其他還在跑的工具呼叫被取消，這一步與它的結果都不加入歷史，不再呼叫模型，提問以 `AgentError::AccessRevoked` 結束。`QuestionRun` 記住被拒絕，之後不再向身分服務要 token，不重試。身分服務逾時或無法使用時只有該次工具呼叫失敗，模型看到固定的錯誤訊息，提問繼續。

service-kit 把這兩種拒絕都記成 INFO（`the identity service refused the run delegation`），對提問來說 `mesh.calls` 少了一筆和撤銷看起來一樣，都以「存取已改變」結束。委派是在使用者的 session 有效時剛建立的，所以一次提問在換到任何 token 之前就被拒絕時，比較可能是 `mesh.calls` 少了這個呼叫：本服務對這種情況另記一行 WARN（`the identity service refused the first token exchange of a question's run`，只有 `run_id`、`audience` 與 `code`）。這行 WARN 持續出現時要檢查 chart 的 `mesh.calls`。

**不是這個對話的 run**：`QuestionRun` 記住它是為哪個對話的租戶與使用者建立的（`Delegation::principal`）。`Agent::ask` 收到為別人的對話建立的 run 時，在呼叫模型或任何工具之前以 `AgentError::NotTheConversationsRun` 拒絕，不換 token、歷史不變。

**沒有設定時**：沒有 `identity.exchange_url` 時沒有任何工具（工具只能以使用者身分呼叫），`build` 回的 `Assistant::runs` 是 `None`，提問以 `NoDelegation` 進行；沒有 `telemetry_query.url` 或 `assets.url` 時少掉對應的工具。每種情況在啟動時記一行 INFO。設了被呼叫端卻沒設 `identity.exchange_url` 是設定錯誤，啟動失敗。

## 對話的儲存（Postgres）

對話與歷史存在 assistant 自己的 schema（`migrations/postgres/0001_assistant.sql`，Laysi/trustiot-v2#553），由 migration Job 套用（`src/schema.rs` 登錄在 `tools/migrate`）。兩張表都是租戶資料，依決策 29 啟用 RLS；沒有平台角色，沒有東西跨租戶讀對話。

| 表 | 一列是 | 內容 |
| --- | --- | --- |
| `assistant.conversations` | 一個對話 | 擁有者（租戶、使用者）與開始時固定的 `Preamble`：system prompt 全文與版本、租戶脈絡（已跳脫、包好標籤的區塊）、工具定義（JSON 陣列，依名稱排序）、effort、快取時效 |
| `assistant.messages` | 歷史的一則訊息 | 位置（`seq`，從 0 起、依送給模型的順序）、`EntryKind`、格式（`anthropic_messages`），以及訊息本身：送給模型的 JSON 文字的位元組，存成 `bytea` |

- **只能新增**：服務角色 `assistant_app` 對兩張表只有 SELECT 與 INSERT，沒有 UPDATE、DELETE、TRUNCATE，資料庫直接拒絕改寫或刪除（`tests/history_store.rs`）。訊息對對話的外鍵是 NO ACTION，沒有串聯刪除。刪除對話怎麼做由 Laysi/trustiot-v2#146 決定，屆時另加 migration。
- **原樣保存**：訊息不用 jsonb（會重排鍵、改寫數字），也不用 text（位元組受資料庫與連線的編碼影響）。讀回時檢查是 UTF-8、剛好一個 JSON 值，而且長度與存的相同，所以送回模型的就是存進去的位元組，thinking 區塊與 `signature` 不變。不符的列讓載入失敗（`StoreError::Corrupt`），不會送出改過的歷史。system prompt 存全文而不只存版本，新版本上線後舊對話照樣以原本的文字繼續。工具定義寫入前先確認讀回後序列化的結果相同，做不到就拒絕儲存（`StoreError::Unstorable`）。
- **只往後加、不分岔**：`store::append(tx, 對話, stored)` 從第 `stored` 則開始寫。已經存在、內容相同的訊息略過，所以 commit 結果不明時可以安全重試；同一個位置已有不同的訊息（另一個寫入者先加了）回 `StoreError::Conflict`，不覆寫。從儲存的結尾之後開始寫會留下空洞，回 `StoreError::Gap`。主鍵 `(tenant_id, conversation_id, seq)` 讓兩個同時寫同一位置的寫入者只有一個成功，不需要鎖。
- **租戶與擁有者**：每個函式都收 service-kit 的 `TenantTransaction`，交易的租戶不是對話的租戶時回 `StoreError::WrongTenant`。`store::load` 只回傳屬於該使用者的對話，同租戶的其他使用者拿到 `None`：對話的工具以擁有者的身分執行，別人接著問會變成以擁有者的身分讀資料。`store::create` 與 `store::append` 也自己核對已存的擁有者與 `Preamble`，不同就回 `StoreError::Conflict`、什麼都不寫，不靠呼叫端怎麼建出 `Conversation`（單元測試 `append_refuses_a_conversation_of_another_owner_or_preamble`）。誰可以檢視別人的對話由 Laysi/trustiot-v2#146 決定。
- 模型上一次回報的歷史大小（`Conversation::measured`）不儲存，載入後的第一次呼叫重新估計。

服務啟動時建立服務角色 `assistant_app` 的連線池（verify-full 的 TLS，`database.*` 與平台設定，見「設定」），不在啟動時連線（Laysi/trustiot-v2#592）。readiness 有三項：`postgres` 以服務角色開一條通過 service-kit session 檢查的連線，migration 檢查（`MigrationCheck` 與 `schema::SCHEMA`）確認資料庫有這一版預期的每個 migration 檔案，`internal_identity_keys` 確認已讀到可用的身分服務公鑰（公開 API 以它驗證每個請求，見「儀表板」）；任一項不過時 `/readyz` 回 503，日誌寫出原因（[service-kit 的 migration 檢查](../../crates/service-kit/README.md#migration-檢查migrations)）。readiness 不呼叫 AI 閘道或其他服務。chart 在 `mesh.calls.assistant` 列了 `postgres` 時給它帳密與連線池大小（`services.assistant.database.maxConnections`，算進 chart 的連線預算，見 chart README 的「連線預算」，Laysi/trustiot-v2#575），migration Job 讓 `assistant_app` 能登入。對話串的 API 在 Laysi/trustiot-v2#146 呼叫 `store::create`、`store::append`、`store::load`。

## 儀表板

儀表板（決策 33，Laysi/trustiot-v2#150；設計在 [docs/architecture/ai-assistant.md](../../docs/architecture/ai-assistant.md)「儀表板」）是一個頁面與它顯示的資料集，存成只往後加的版本。有兩種：

- **草稿**屬於一個對話：只有 `dashboard` 工具寫它的內容，只有對話的擁有者看得到，對話被清除時一起刪除。
- **已儲存的儀表板**屬於租戶：使用者把草稿發布進去，租戶的每個成員都看得到；它不屬於任何對話，只會封存與取消封存，不會刪除。

草稿記得它取自或發布到哪個已儲存的儀表板，以及以那個儀表板的哪一版為基礎。公開 API 沒有寫內容的路由：沒有任何請求本文有 SQL、語意查詢或頁面的欄位，瀏覽器寫不了會被執行的東西。

### API

路徑都在 `/api/assistant/v1` 之下；本文與回應的型別在 `contract::api::dashboard::public`，OpenAPI 在 `api/openapi.json`（`assistant::openapi()`）。

| 請求 | 誰 | 成功 |
| --- | --- | --- |
| `GET /dashboards`（`?include_archived=true`、`?cursor=`） | viewer 以上 | 200 `DashboardList`：已儲存的儀表板，最近更新的在前，一頁 50 個 |
| `GET /conversations/{conversation_id}/dashboards`（`?cursor=`） | 對話的擁有者 | 200 `DashboardList`：對話的草稿 |
| `GET /dashboards/{dashboard_id}`（`?version=`，沒有時是最新版） | 看得到它的人 | 200 `DashboardView`：儀表板、那一版的內容、呼叫者可以做的事（`permissions`） |
| `GET /dashboards/{dashboard_id}/versions`（`?cursor=`） | 看得到它的人 | 200 `VersionList`：新的在前，一頁 50 個 |
| `POST /dashboards/{dashboard_id}/publish`（草稿） | editor 以上，而且是草稿所屬對話的擁有者 | 201 `VersionCreated`：已儲存儀表板的新版本（第一次發布建立已儲存的儀表板） |
| `POST /dashboards/{dashboard_id}/restore` | 已儲存的儀表板：editor 以上；草稿：對話的擁有者，任何角色 | 201 `VersionCreated`：把舊版本複製成新的一版 |
| `PUT`、`DELETE /dashboards/{dashboard_id}/archive`（已儲存的儀表板） | editor 以上 | 200 `ArchiveState`：封存、取消封存 |
| `POST /dashboards/{dashboard_id}/dataset-runs` | 看得到它的人，以呼叫者的身分執行 | 200 `DatasetResult`：一個資料集的表格（見「資料集的執行」） |

- **身分**：每個請求都驗證入口 gateway 帶來的內部身分 token（決策 27），使用者、租戶與角色每次都取自 token，不取自本文或存下的副本。token 沒有租戶、或角色是這一版不認得的，回 403 `permission_denied`。
- **看不到等於不存在**：看不到的儀表板與不存在的回一樣的 404（本文相同），路徑的 ID 不是 UUID 也是 404。同租戶的其他使用者看不到別人的草稿，也看不到別人對話的草稿列表。
- **嚴格的請求**：本文要是 `application/json`（只有一個 `Content-Type`，大小寫不拘，可以有參數；其他網域的頁面不經 CORS preflight 送不出這個型別，而沒有東西回應 preflight）、最多 4 KiB、正好是請求的 JSON 物件，未知欄位、重複的欄位、陣列寫法與多餘的值都回 400 `invalid_request`。query string 有未知參數、值不對或 `cursor` 不是本服務寫的，也是 400。
- **不快取**：每個回應都有 `cache-control: no-store`，包括驗證失敗的 401。
- 寫入（發布、還原、封存）各記一行 INFO（`published a dashboard`、`restored a dashboard version`、`changed whether a dashboard is archived`），只有使用者、租戶、儀表板與版本，不記標題、spec 或頁面。

錯誤都是 M1 本文；儀表板自己的 `message_key` 在 `contract::api::dashboard::public`（zh-TW 與 en 的翻譯在 `frontend/src/i18n/locales/`）：

| 情況 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 沒有租戶或不認得的角色 | 403 `permission_denied` |
| viewer 發布草稿、還原或封存已儲存的儀表板 | 403 `permission_denied`、`dashboard.editor_required` |
| 不是對話的擁有者卻發布或還原草稿（看得到草稿的只有擁有者，所以目前只在 Laysi/trustiot-v2#650 之後出現） | 403 `permission_denied`、`dashboard.owner_required` |
| 看不到或不存在的儀表板、不是 UUID 的 ID | 404 `not_found` |
| 沒有這一版 | 404 `not_found`、`dashboard.version_not_found` |
| 還原時 `base_version` 不是最新版 | 409 `conflict`、`dashboard.version_conflict`，`details.latest` |
| 發布時已儲存的儀表板在草稿的基礎版本之後有新版本 | 409 `conflict`、`dashboard.saved_changed`，`details.latest` |
| 已封存的儀表板加版本（發布、還原） | 409 `conflict`、`dashboard.archived` |
| 同一個儀表板的兩個寫入同時進行、或版本數到上限（10,000） | 409 `conflict` |
| 發布已儲存的儀表板、封存草稿 | 422 `validation_failed`、`dashboard.draft_required`、`dashboard.saved_required` |
| 資料集執行的錯誤 | 見「資料集的執行」 |
| 資料庫失敗 | 503 `unavailable`，什麼都沒寫；日誌只有 SQLSTATE 或錯誤的種類 |

### 儲存

`migrations/postgres/0002_dashboards.sql`，與對話同一個 schema，由 migration Job 套用。兩張表都是租戶資料，依決策 29 啟用 RLS（只隔開租戶；租戶內誰看得到什麼由服務決定，見下方）：

| 表 | 一列是 | 內容 |
| --- | --- | --- |
| `assistant.dashboards` | 一個儀表板 | 種類（`draft`、`saved`）、草稿的對話、草稿的已儲存儀表板與基礎版本、建立者、建立時間、已儲存儀表板的封存時間 |
| `assistant.dashboard_versions` | 一個版本 | 版本號（從 1 起、沒有空洞，外鍵指向前一版）、來源（`agent`、`publish`、`restore`，還原時記下還原的版本）、沙盒 runtime 版本、標題、說明、spec（服務寫的精簡 JSON 文字，讀回逐位元組相同）、頁面、要求的使用者、工具寫入時的 run ID、建立時間 |

- **權限**：服務角色 `assistant_app` 對兩張表有 SELECT 與指定欄位的 INSERT（時間由資料庫的時鐘決定）；`assistant.dashboards` 另外只能 UPDATE 草稿的已儲存儀表板、基礎版本與封存時間。沒有 DELETE 與 TRUNCATE。觸發器對每個角色（包括 owner）再限制：草稿的已儲存儀表板只設一次、基礎版本只升不降、封存時間是資料庫的時鐘（再封存一次保留原本的時間）、其他欄位不變；版本寫入後不能改（`42501`，`tests/dashboards_store.rs`）。
- **刪除**：對話被清除時，它的草稿與草稿的版本以外鍵串聯刪除；已儲存的儀表板不屬於對話，不受影響。
- **一個操作一個交易**：`dashboards::store` 的每個函式是一個完整的操作，在呼叫者租戶的 `TenantTransaction` 裡單獨呼叫，成功才 commit。寫入先鎖儀表板的列（`FOR UPDATE`）再讀最新版本，所以同一個儀表板的寫入排隊，以舊版本為基礎的寫入被拒；發布先鎖草稿再鎖已儲存的儀表板，沒有反過來的順序。還原與發布在資料庫裡複製版本，所以複本與來源逐位元組相同。錯誤是 `StoreError`，不帶資料庫的文字或內容；日誌只記 `StoreError::log_kind`。
- **誰看得到**：已儲存的儀表板是租戶的每個成員；草稿只有對話的擁有者。草稿的可見性只在一個地方決定：`store::conversation_access` 的一個查詢（`CONVERSATION_OWNER`）。Laysi/trustiot-v2#644（對話的軟刪除）在那個查詢加上 `AND deleted_at IS NULL`，並在寫入時以 `FOR SHARE` 鎖住對話（服務角色現在對對話沒有 UPDATE，鎖不了；`tests/dashboards_store.rs` 有一個等 #644 的 `#[ignore]` 測試）；Laysi/trustiot-v2#650（共享的讀者）在同一個函式加上讀者的存取。
- **誰能改**：`store::permissions` 是唯一的規則，API 回的 `permissions` 與 store 的檢查都用它：草稿的擁有者可以還原（任何角色）、editor 以上的擁有者可以發布；已儲存的儀表板由 editor 以上還原與封存。

### 資料集的執行

`POST /dashboards/{dashboard_id}/dataset-runs`（本文 `{version, dataset}`）執行存下的那一版的一個資料集，`dashboards::runner::DatasetRunner` 做實際的呼叫（設計紀錄「資料集的執行」）：

1. 在請求租戶的短交易裡讀出那一版的資料集（看不到儀表板是 404，沒有這一版是 404 `dashboard.version_not_found`，沒有這個資料集是 422 `dashboard.dataset_unknown`），交易在呼叫 telemetry-query 之前結束，執行期間不佔資料庫連線。
2. 以請求的 token 向身分服務換一張受眾是 telemetry-query 的 token（`OnBehalfOf`，service-kit 的 `OnBehalf`，每次執行都換新的），所以 telemetry-query 以**打開儀表板的人**的租戶、角色與可查期間讀資料，不是作者的。沒有 `identity.exchange_url` 或 `telemetry_query.url` 時每次執行回 503 `dashboard.dataset_failed`。
3. SQL 資料集送到 `POST /internal/telemetry/v1/sql`，列數上限是資料集的 `max_rows`（最多 2,000）；統計與用量資料集把存下的請求本文原樣送到 `POST /api/telemetry/v1/statistics` 或 `/usage`，回答攤平成表格（`dashboards::flatten`）。
4. 結果檢查形狀與大小：超過 `max_rows` 列回 422 `dashboard.dataset_too_many_rows`（`details.max`，請縮小範圍或改用彙總），結果超過 2 MiB 回 422 `dashboard.dataset_result_too_large`，dashboard 用不了的欄位名稱或型別回 422 `dashboard.dataset_refused`。

- **上限**（`dashboards::caps`）：每個租戶每分鐘最多開始 60 次、同時最多 2 個；第三個最多等 10 秒拿到空位，等不到回 429。超過時回 429 `rate_limited`、`dashboard.runs_limited`，帶 `Retry-After`（整數秒，至少 1）與 `details.retry_after`。上限以**每個副本**計：N 個副本時一個租戶每分鐘最多 N × 60 次，telemetry-query 自己的每租戶額度（每分鐘 120 次）仍然有效。被拒的執行不呼叫任何服務、也不算進每分鐘的次數。`dashboard` 工具的執行用同一個 `DatasetRunner`，與瀏覽器共用上限。
- **不重用結果**：沒有快取，每次執行都重新查詢。打開儀表板與執行它的資料集不呼叫 AI 閘道。
- **telemetry-query 的錯誤**只讀狀態碼與四個轉送的 key（`FORWARDED_TELEMETRY_KEYS`）及它們有型別的 details，其他文字不會進到回應或日誌：

| telemetry-query | 回應 |
| --- | --- |
| 429（租戶的查詢額度用完） | 429 `rate_limited`、`dashboard.query_quota_exhausted`（沒有 `Retry-After`） |
| 404 `telemetry.account_not_ready`（租戶的 ClickHouse 帳號還沒建立） | 503 `unavailable`、`telemetry.account_not_ready`（在這個 API 裡 404 是看不到的儀表板） |
| 403 `telemetry.outside_queryable_period` | 403 `quota_exceeded`、同一個 key，`details.earliest` |
| 422 `telemetry.query_too_heavy`、`telemetry.too_many_points`（語意資料集），SQL 的 `query_too_heavy` | 422 `validation_failed`、同一個 key，`details.parameter`（只有 `interval`、`series`）與 `details.max` |
| SQL 的 `too_many_rows`、`result_too_large` | 422 `dashboard.dataset_too_many_rows`（`details.max` 是資料集的 `max_rows`）、`dashboard.dataset_result_too_large` |
| 其他 422（例如現在不允許的函式、多了欄位或不認得的拒絕）、400 | 422 `validation_failed`、`dashboard.dataset_refused` |
| 504、呼叫逾時 | 504 `timeout`、`dashboard.dataset_timeout` |
| 其他（401、其他 403 與 404、5xx、連線失敗、無法解析的回答、換不到 token） | 503 `unavailable`、`dashboard.dataset_failed` |

- **日誌**：呼叫失敗由 `Callee` 記一行，只有租戶、`operation`（`dataset_run`，或工具的名稱）、`peer`、`kind` 與狀態碼的類別：400、403、404、422 記 INFO `a service refused a dataset run`，其他記 WARN `a dataset run's call to a service failed`。不記 SQL、請求本文、回答或 token。

## 設定

設定來自 `TRUSTIOT__` 開頭的環境變數（`__` 分隔層級、名稱轉小寫；時間長度寫成 `30s`、`10m`），或 `TRUSTIOT_CONFIG_FILE` 指的 TOML 檔，規則與 service-kit 相同（[crates/service-kit/README.md](../../crates/service-kit/README.md)）。只在啟動時讀；錯誤訊息只寫設定名稱，不寫值。

| 設定 | 變數 | 預設 | 說明 |
| --- | --- | --- | --- |
| 平台設定 | `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 必填 | Postgres 的位置；只接受 `verify-full`，`_CA_FILE` 是伺服器憑證唯一的信任錨。由部署設定（chart、`bun scripts/dev.ts env`） |
| 平台設定 | `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 必填 | 身分服務公鑰（JWK Set）的檔案，公開 API 以它驗證每個請求的內部身分 token；執行期間每 5 秒重讀，讀到可用的公鑰之前 readiness 不過。chart 在身分服務開啟時給每個服務（身分服務沒開啟時本服務拒絕啟動），本機是 `bun scripts/dev.ts env` |
| `database.username`、`database.password` | `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | 必填 | 服務角色 `assistant_app` 的帳密；chart 的密碼來自 Secret `<release>-assistant-password` 的 `assistant_app` key。沒有平台角色 |
| `database.max_connections` | `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | `4` | 服務角色連線池每個副本的上限，2 到 100；readiness 與儀表板 API 的短交易共用（資料集執行呼叫 telemetry-query 時不佔連線），聊天 API（Laysi/trustiot-v2#146）存對話時再調整。取得連線最多等 5 秒（`ACQUIRE_TIMEOUT`）。chart 以 `services.assistant.database.maxConnections`（預設 4）設定，算進 chart 的連線預算 |
| `ai_gateway.url` | `TRUSTIOT__AI_GATEWAY__URL` | 必填 | AI 閘道的 `http://<host>[:<port>]`，不能有路徑、帳密或 query；mesh 內由 ztunnel 加上 mTLS |
| `ai_gateway.role` | `TRUSTIOT__AI_GATEWAY__ROLE` | `main` | 主 agent 用的模型角色 |
| `ai_gateway.max_response_bytes` | `TRUSTIOT__AI_GATEWAY__MAX_RESPONSE_BYTES` | 32 MiB | 回應本文超過就算失敗；1 KiB 到 256 MiB |
| `model.effort` | `TRUSTIOT__MODEL__EFFORT` | `medium` | `low`、`medium`、`high`、`xhigh`、`max`；對話開始時固定（之後的分流 Laysi/trustiot-v2#444 會逐對話選） |
| `model.max_output_tokens` | `TRUSTIOT__MODEL__MAX_OUTPUT_TOKENS` | `16000` | 一次回應的 `max_tokens`；1 到 128000 |
| `model.call_timeout` | `TRUSTIOT__MODEL__CALL_TIMEOUT` | `10m` | 一次模型呼叫的上限，涵蓋閘道換模型的所有嘗試；不超過 1 小時 |
| `model.cache_ttl` | `TRUSTIOT__MODEL__CACHE_TTL` | `5m` | 快取斷點的時效：`5m` 或 `1h` |
| `budget.tokens` | `TRUSTIOT__BUDGET__TOKENS` | `2000000` | 一個問題能處理的 token；1 到 100,000,000 |
| `budget.wind_down_percent` | `TRUSTIOT__BUDGET__WIND_DOWN_PERCENT` | `80` | 到這個百分比時請模型收尾；1 到 99 |
| `tools.call_timeout` | `TRUSTIOT__TOOLS__CALL_TIMEOUT` | `60s` | 一次工具呼叫的上限 |
| `tools.max_calls_per_step` | `TRUSTIOT__TOOLS__MAX_CALLS_PER_STEP` | `8` | 一次模型回應裡同時執行的工具呼叫數；之後的呼叫回錯誤、不執行；1 到 32 |
| `tools.max_text_bytes` | `TRUSTIOT__TOOLS__MAX_TEXT_BYTES` | 64 KiB | 一個工具結果的文字上限；超過時改回錯誤，告訴模型怎麼縮小範圍 |
| `tools.max_images` | `TRUSTIOT__TOOLS__MAX_IMAGES` | `4` | 一個工具結果的圖片數上限；不超過 20 |
| `tools.max_image_bytes` | `TRUSTIOT__TOOLS__MAX_IMAGE_BYTES` | 5 MiB | 一張圖（base64 文字）的上限；不超過 5 MiB |
| `identity.exchange_url` | `TRUSTIOT__IDENTITY__EXCHANGE_URL` | 無 | 身分服務 token 交換埠的 `http://<host>[:<port>]`（`http://<release>-identity-exchange:8082`），不能有路徑、帳密或 query；沒有就沒有工具，儀表板的資料集執行也都回 503。呼叫身分服務的時間上限是 15 秒（`IDENTITY_TIMEOUT`） |
| `identity.run_lifetime` | `TRUSTIOT__IDENTITY__RUN_LIFETIME` | `15m` | 一次提問的委派效期；整數秒，2 到 15 分鐘（身分服務的上限 900 秒）。提問在到期前 30 秒結束 |
| `telemetry_query.url` | `TRUSTIOT__TELEMETRY_QUERY__URL` | 無 | telemetry-query 的 `http://<host>[:<port>]`；沒有就沒有 `telemetry_latest`、`telemetry_statistics`、`telemetry_usage`、`telemetry_sql`，儀表板的資料集執行也都回 503。設了就要有 `identity.exchange_url` |
| `assets.url` | `TRUSTIOT__ASSETS__URL` | 無 | assets 的 `http://<host>[:<port>]`；沒有就沒有 `device_models`。設了就要有 `identity.exchange_url` |

工具呼叫其他服務的時間上限與 `tools.call_timeout` 相同（`ToolRegistry` 的上限才是真正的界線），連線最多 10 秒，成功回應的本文最多 8 MiB，`telemetry_sql` 讀的 422 本文最多 32 KiB（`MAX_REFUSAL_BYTES`）。儀表板的資料集執行用同一個 client 與時間上限，成功回應的本文最多 4 MiB（`runner::MAX_READ_BYTES`，結果本身最多 2 MiB）。mesh 內是純 HTTP，由 ztunnel 加上 mTLS；不用環境變數的 proxy，不跟隨轉址（token 的 header 會跟著轉過去）。chart 從 `mesh.calls.assistant` 填 `ai_gateway.url`、`identity.exchange_url`、`telemetry_query.url` 與 `assets.url`，`services.assistant.env` 不能設定這四個（[chart README](../../deploy/chart/README.md)「AI 助理」）；授權矩陣在同一份 README 的「呼叫矩陣」。

## 錯誤

使用者只會看到 `AgentError` 的固定訊息（逾時、暫時無法使用、無法回答、拒答、對話太長、預算用完、提問超時 `RunExpired`、存取已改變 `AccessRevoked`、不是這個對話的 run `NotTheConversationsRun`），不含閘道、供應商或被呼叫服務的錯誤文字（J2）。閘道的錯誤只取 HTTP 狀態與錯誤 `type`（簡短識別字）記在日誌，`message` 一律不讀。504 算逾時；429、529 與其他 5xx、連線失敗算暫時無法使用（閘道已經依角色換過模型）；其他 4xx 與無法解析的回應算無法回答。

## 日誌與指標

| 事件 | 等級 | 欄位 |
| --- | --- | --- |
| `model call completed` | INFO | `run_id`、`conversation_id`、`tenant_id`、`step`、`llm_call_id`、`model`、`duration_ms`、`stop_reason`、`tool_calls`、`output_limit`、`usage.input`、`usage.cache_write`、`usage.cache_read`、`usage.output`、`usage.estimated`、`budget.spent`、`budget.limit` |
| `model call failed` | WARN | `run_id`、`conversation_id`、`tenant_id`、`step`、`llm_call_id`、`duration_ms`、`kind`、`status`、`error_type` |
| `model call not sent: it does not fit the budget` | INFO | `run_id`、`conversation_id`、`tenant_id`、`input_estimate`、`budget.spent`、`budget.limit` |
| `tool call completed` | INFO | `run_id`、`conversation_id`、`tenant_id`、`tool`、`outcome`（`ok`、`input_error`、`failed`、`denied`、`revoked`、`timeout`、`unknown_tool`、`too_large`、`invalid_image`、`over_call_limit`）、`duration_ms`、`result_bytes` |
| `a service refused a tool call` | INFO | `run_id`、`conversation_id`、`tenant_id`、`tool`、`peer`（被呼叫的服務）、`kind`、`status_class`（`4xx`）；400、403、404、422 |
| `a tool call to a service failed` | WARN | 同上；`kind` 是 `timeout`、`connect`、`transport`、`error_status`、`too_large`、`invalid_response`、`invalid_token`，`status_class` 只有收到回應時才有 |
| `question run started` | INFO | `run_id`、`tenant_id`、`lifetime_secs`、`deadline_in_secs` |
| `the identity service refused the first token exchange of a question's run: check that mesh.calls lists this call` | WARN | `run_id`、`audience`、`code`；見「代表使用者呼叫」的撤銷 |
| `agent run finished` | INFO | `run_id`、`conversation_id`、`tenant_id`、`steps`、`outcome`（`answered` 或 `AgentError` 的種類，含 `run_expired`、`access_revoked`、`not_the_conversations_run`）、`duration_ms`、`budget.spent`、`budget.limit`、`usage.estimated` |
| `a service refused a dataset run` | INFO | `tenant_id`、`operation`（`dataset_run`）、`peer`、`kind`、`status_class`；儀表板 API 的資料集執行收到 400、403、404、422（`dashboard` 工具的執行記成上面的工具呼叫，`tool` 是 `dashboard`） |
| `a dataset run's call to a service failed` | WARN | 同上；`kind` 與工具呼叫的相同 |
| `a dataset run cannot call telemetry-query: identity.exchange_url is not set` | WARN | `tenant_id` |
| `published a dashboard`、`restored a dashboard version` | INFO | `user_id`、`tenant_id`、`from`（發布的草稿或還原的儀表板）、`dashboard_id`、`version`（新的版本） |
| `changed whether a dashboard is archived` | INFO | `user_id`、`tenant_id`、`dashboard_id`、`archived` |
| `refused: no tenant or no known role in it` | INFO | `user_id`、`action`；公開 API 的 403 |
| `could not …`（例如 `could not publish a dashboard`、`could not open a transaction for an API request`） | ERROR | `user_id`、`dashboard_id`（有的時候）、`error_kind`（SQLSTATE 或錯誤的種類，沒有資料庫的文字）；公開 API 的 503 與 500 |

不記問題、回答、工具輸入與結果、錯誤訊息，也不記被呼叫服務的 URL、查詢參數、回應本文與 token（只記狀態碼的類別）。欄位名稱都不會被 service-kit 的遮蔽規則擋掉（不用 `token`、`tokens`、`prompt`、`content` 這些字）。指標 `assistant.model_calls` 依 `outcome` 與 `cache`（`hit` 或 `miss`）計數；`cache_read` 長期為 0（只有 `miss`）表示快取失效，要接到監控（Laysi/trustiot-v2#554 以真實 API 驗證快取）。

## 之後接上的地方

| 項目 | 介接點 |
| --- | --- |
| 聊天 API 與串流（Laysi/trustiot-v2#146） | `Agent::start_conversation`、`Agent::ask` |
| 聊天 API 開始一個問題（Laysi/trustiot-v2#146） | 處理請求時以 `delegation::QuestionRuns::start(&UserIdentity, conversation.principal())` 取得 `QuestionRun`，交給 `Agent::ask`；`Assistant::runs` 為 `None`（沒有設定 `identity.exchange_url`）時改用 `tools::NoDelegation`。`StartError` 已實作 `IntoResponse`（見「代表使用者呼叫」） |
| 更多工具（語意層、唯讀 SQL、儀表板） | 實作 `tools::Tool`，放進 `ToolRegistry`（最多 10 個），以 `callee::Callee` 呼叫服務；工具從 `ToolContext.principal` 取租戶與使用者，不從輸入取 |
| 對話存進 Postgres（Laysi/trustiot-v2#553 已完成儲存，連線池由 Laysi/trustiot-v2#592 接上） | `store::create`、`store::append`、`store::load`，在對話租戶的 `TenantTransaction` 裡呼叫，交易取自 `start` 建立的 `TenantDb`（現在給 readiness 與儀表板 API 用），見[對話的儲存](#對話的儲存postgres) |
| `dashboard` 工具（Laysi/trustiot-v2#150） | 在問題租戶的交易裡呼叫 `dashboards::store::create_draft`、`write_draft`、`fork`（`Author` 是對話的擁有者與 run ID，`NewVersion` 是檢查過的內容）與讀取用的 `open`、`versions`、`list_saved`、`list_drafts`、`dataset`；資料集以 `Assistant::runner`（與 API 共用上限的 `DatasetRunner`）和 `ToolContext::caller("dashboard")` 執行，`RunError::Revoked` 時結束提問。見[儀表板](#儀表板) |
| 對話的軟刪除（Laysi/trustiot-v2#644） | `dashboards::store` 的 `CONVERSATION_OWNER` 加上 `AND deleted_at IS NULL`，寫入時以 `FOR SHARE` 鎖對話；拿掉 `tests/dashboards_store.rs` 那個測試的 `#[ignore]` |
| 對話共享的讀者（Laysi/trustiot-v2#650） | `dashboards::store::conversation_access` 回讀者的存取（新的 `Access`），`store::permissions` 決定讀者能做什麼 |
| 每次呼叫預扣額度（Laysi/trustiot-v2#140） | 每次呼叫的 `ModelRequest::call_id`（帳本的 `llm_call_id`，現在只記在日誌）；閘道拒絕時以 `ModelError` 回到迴圈 |

## 測試

`cargo test -p assistant`（`tests/history_store.rs`、`tests/startup.rs`、`tests/dashboards_store.rs`、`tests/dashboards_api.rs` 與 `tests/cross_tenant.rs` 以 podman 或 docker 起一個拋棄式 TLS Postgres，以 migration Job 套用整個產品的 schema；沒有容器執行環境時在本機略過，設了 `CI` 時失敗）。測試以本機 loopback 上的替身 AI 閘道回放 `tests/fixtures/` 的回應（取自官方文件，見該目錄的 README），不呼叫供應商，也沒有 API key。

| 檔案 | 內容 |
| --- | --- |
| `tests/agent.rs` | 60 步的長分析不受步數限制；接近預算時收尾並回答；放不進剩餘預算的呼叫不送出、輸出上限降到剩下的空間；用量超過估計而預算用完時工具不執行；第二個問題的請求逐位元組以前一次請求的歷史開頭（thinking 與 signature、system、工具定義不變，租戶脈絡包在標籤裡）；兩個使用者同時發問互不等待；模型逾時與閘道錯誤只給固定訊息、日誌沒有錯誤文字；拒答不留在歷史；每一步的時間與快取讀取都記在日誌 |
| `tests/tools.rs` | 工具組規則（最多 10 個、名稱、schema）；並行執行、結果依呼叫順序；一步超過呼叫數上限的呼叫不執行；輸入錯誤、失敗、panic、逾時、不存在的工具各自回錯誤給模型；結果超過大小與圖片上限時請模型縮小範圍；工具拿到的身分來自對話；圖片照 Anthropic 格式送給模型 |
| `tests/delegated_tools.rs` | 四個呼叫服務的工具各以使用者身分、受眾正確且在呼叫當下交換的 token 呼叫服務，請求裡沒有租戶或使用者，超過 60 秒後的呼叫仍成功、舊 token 會被拒（替身以「每次模型呼叫過 61 秒」的虛擬時鐘模擬，不真的等）；服務回 403、404、422、503 與非 JSON 時模型與日誌都看不到回應本文；身分服務拒絕交換（撤銷）時該步停止、問題結束、不再重試；身分服務暫時無法使用只讓該次呼叫失敗；問題在委派到期前結束、沒有剩餘時間的問題不開始；只有對話的使用者能開始問題，為別人的對話建立的 run 在送出任何東西前被拒；裝置寫的長文字截到 256 個字元、加上截斷標記才給模型；工具輸入不收租戶、使用者與多餘欄位；工具的路徑與上限跟 telemetry-query、assets 的程式一致；語意工具以 POST 送出的本文是 API 的 `SeriesQuery`（替身照服務的型別解析、拒絕未知欄位），輸入 schema 與 `telemetry_query::openapi()` 的 `SeriesQuery` 一致；列數超過服務上限時不呼叫 |
| `tests/history_store.rs` | 主 agent 跑完的對話（thinking 與 signature、工具呼叫與結果）存進去、讀回來逐位元組相同，載入的對話接著問時請求的前段與歷史和原本的逐位元組相同；服務角色的 UPDATE、DELETE、TRUNCATE 在租戶交易內外都被資料庫拒絕（`42501`）；重複寫入不改變什麼，兩個寫入者分岔時後者被拒、不覆寫，空洞與沒建立的對話被拒；其他租戶與同租戶的其他使用者載入不到；損壞的列讓載入失敗 |
| `tests/startup.rs` | 執行檔本身：以 `assistant_app` 登入、讀到身分服務的公鑰檔、資料庫有這一版的 migration 時 `/readyz` 回 200，日誌記下工具數與可以執行資料集且不含密碼；密碼錯誤時 readiness 記下失敗、回 503；少了一個 migration 檔案時日誌寫出缺的檔案、回 503，補回後恢復 |
| `tests/chart_connections.rs` | chart 的 Postgres 連線預算（`trustiot.postgres.connectionBudgets`）與程式開的連線池一致：沒有其他連線池，`database.maxConnections` 的範圍與服務接受的相同；程式多了一個連線池時失敗 |
| `tests/cross_tenant.rs` | 跨租戶負向測試（決策 29，`crates/testkit`）：對話、訊息、儀表板與版本四張表的讀、寫、搬移、插入探測與覆蓋檢查；對執行檔的每個儀表板端點（發布與對話的草稿列表除外，原因寫在豁免裡，由 `tests/dashboards_api.rs` 手寫測試），資料集執行的正向對照經替身的 token 交換與 telemetry-query 真的跑完，沒有呼叫其他服務 |
| `tests/dashboards_store.rs` | `dashboards::store` 對真的 schema：版本只往後加、舊版本讀得回來；發布第一次建立已儲存的儀表板、之後加版本，已儲存的儀表板在草稿的基礎版本之後有新版本或已封存時拒絕；誰可以發布、還原、封存；工具只寫自己對話的草稿（別的對話與別的使用者都是不存在）；列表分頁與只列看得到的；服務角色改不了寫過的東西（觸發器與權限，`42501`）、留不下空洞；清除對話時刪掉草稿、留下已發布的；同一個草稿同時寫入時排隊、後者衝突；軟刪除的對話（等 Laysi/trustiot-v2#644，`#[ignore]`） |
| `tests/dashboards_api.rs` | 公開 API 對真的 schema 與替身的 token 交換、telemetry-query：沒有 token、偽造的 token、沒有租戶時每個路由都拒絕；本文、query string 與 content type 不對時 400 而且什麼都沒寫（含 SQL、頁面等欄位、陣列寫法）；回應都是 `no-store`；請求的角色決定誰能發布、還原、封存（同一個使用者升降級後立即生效）；同租戶的其他使用者與其他租戶的 admin 對別人的草稿每個路由都是 404，與不存在的相同；版本、還原、發布衝突與封存；資料集以打開的 viewer 身分執行（token 的使用者、租戶、角色、受眾與 `act`），請求本文原樣、SQL 的列數上限 2,000；超過列數時拒絕並附上限；第 61 次執行與第三個同時執行回 429 而其他租戶照常、`dashboard` 工具的執行算進同一個上限；telemetry-query 的錯誤只以封閉的 key 回給使用者；沒有 token 交換時 503；打開儀表板並執行全部資料集不呼叫模型、日誌沒有標題、SQL 或頁面 |
| `tests/dashboard_world/` | 儀表板測試共用的世界：產品的 Postgres、測試用的身分簽發者、替身的身分服務 token 交換與 telemetry-query（記下每個呼叫、其他路徑都算意外）、啟動執行檔 |
| `tests/telemetry_sql.rs` | `telemetry_sql` 經工具組呼叫：對 telemetry-query 真正的路由（Postgres 與 ClickHouse 是計數的 loopback 埠），系統表、table function、白名單外的函式、`SET ROLE`、`SET DEFAULT ROLE`、多個敘述、`SETTINGS`、CAST、沒寫時區、沒讀 view、引號識別字、反斜線、語法錯誤與太深的巢狀各自回固定的類別說明，兩個資料庫都沒有連線，日誌沒有 SQL；對替身，答案（長文字截斷）、執行後的拒絕、多欄位或不認得的類別與名稱、404、429、504、403、400、5xx 與非 JSON 各自回固定訊息，模型與日誌都看不到服務的本文；每次呼叫換新 token、請求正好是 SQL 加 200 列 |
| `src/` 的單元測試 | 設定檢查（含被呼叫服務的位址、委派時長與資料庫帳密、連線池上限）、截止時間的計算、`StartError` 的錯誤碼、字串上限、時間區間的粒度選擇與查詢範圍、狀態碼類別、422 拒絕本文的嚴格解析、`telemetry_sql` 每個拒絕類別的說明與名稱的再檢查、預算門檻、租戶脈絡的跳脫、狀態碼對應、錯誤 `type` 過濾、沒有用量時的估計、送出前的訊息估計（圖片按張數）、工具定義與訊息讀回的位元組檢查；儀表板：執行上限（第 61 次、第三個同時執行、等到空位、`Retry-After` 的秒數）、語意回答的攤平、telemetry-query 每種回答對應的錯誤、權限表、分頁 cursor |
