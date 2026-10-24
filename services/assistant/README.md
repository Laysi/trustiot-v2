# assistant

AI 助理（決策 31）的主 agent 迴圈：使用者對自己租戶的資料發問，主 agent 透過工具讀資料、回答。對話歷史只往後加、依 prompt caching 的順序排列，每個問題以 token 預算收斂，不設步數上限。只經由 AI 閘道（[services/ai-gateway](../ai-gateway/README.md)）呼叫 Claude，模型藏在一個介面後面（決策 15）。設計在 [docs/architecture/ai-assistant.md](../../docs/architecture/ai-assistant.md)；本服務由 Laysi/trustiot-v2#145 建立。

目前沒有自己的 API：聊天 API 與串流是 Laysi/trustiot-v2#146，程式啟動後只提供 service-kit 的健康檢查端點。

## 檔案

| 路徑 | 內容 |
| --- | --- |
| `src/agent.rs` | 主 agent 迴圈：呼叫模型、執行工具、收尾、錯誤與日誌 |
| `src/model.rs` | 模型介面 `ModelClient` 與中性型別（`ModelRequest`、`ModelTurn`、`TokenCount`、`ModelError`）；原生格式的訊息是不透明的 `NativeMessage` |
| `src/model/anthropic.rs` | 經 AI 閘道的 Anthropic Messages：請求的快取排列、回應解析、錯誤對應 |
| `src/history.rs` | 對話：固定的前段（`Preamble`）與只往後加的歷史 |
| `src/tools.rs` | `Tool` trait、`ToolRegistry`（上限、並行、逾時、結果大小）、`ToolContext` 與 `Delegation` |
| `src/tools/telemetry.rs` | 工具 `telemetry_series`、`telemetry_latest`（telemetry-query 的歷史 API） |
| `src/tools/device_models.rs` | 工具 `device_models`（assets 的設備型號列表） |
| `src/callee.rs` | 工具以使用者身分呼叫其他服務的 HTTP client：每次呼叫換 token、錯誤只看狀態碼 |
| `src/delegation.rs` | 一次提問的 run 委派：`QuestionRuns::start`（聊天 API 的入口）、`QuestionRun`（截止時間、撤銷） |
| `src/budget.rs` | 每個問題的 token 預算 |
| `src/prompt.rs` | 平台 system prompt（有版本）與對話中的固定指示 |
| `src/config.rs` | 設定與啟動檢查 |
| `src/store.rs` | 對話與歷史的 Postgres 儲存：只往後加、原樣保存、依租戶隔離，捨棄最後一輪；錯誤型別 `StoreError` |
| `src/store/threads.rs` | 對話串：清單、標題、封存、軟刪除、已刪除清單、還原、30 天後的清除 |
| `src/store/runs.rs` | 提問紀錄：開始（一個對話同時只有一個進行中）、結束、要求停止、查詢 |
| `src/schema.rs` | 本版本預期的 migration 檔（`migrations/postgres/`），`tools/migrate` 也編進它 |

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

工具在每次呼叫其他服務前，以這次提問的委派換一張受眾是該服務的 token（`Delegation::token_for`），帶在 `x-internal-identity`，呼叫的是 web 前端經入口 gateway 呼叫的同一組 API（`/api/...`）。被呼叫端照一般請求檢查 token 的受眾、使用者的租戶與角色，所以工具讀得到的正是使用者自己讀得到的（決策 27、29）。

| 工具 | 呼叫 | 輸入 | 回給模型 |
| --- | --- | --- | --- |
| `telemetry_series` | telemetry-query `GET /api/telemetry/v1/devices/{device_id}/series` | `device_id`、`point`、`from`、`to`（RFC 3339）、`tz`（IANA），選填 `interval`（`raw`、`1m` … `1d`） | 實際查詢的範圍與間隔、`truncated`、`earliest`，以及每個有讀數的區間一列 `[起點, 筆數, 最小, 最大, 平均, 最後讀數]`；最多 500 列 |
| `telemetry_latest` | telemetry-query `GET /api/telemetry/v1/devices/{device_id}/latest` | `device_id`，選填 `points`（最多 100 個資料點） | 每個資料點一列 `[資料點, 時間, 值]`，依資料點排序，與總數 `total`；最多 200 列 |
| `device_models` | assets `GET /api/assets/v1/device-models` | 選填 `name_contains` | 每個型號一列 `[型號 ID, 名稱, 版本, 資料點數, 更新時間]`，依名稱排序，與總數 `total`；最多 200 列 |

- **租戶與使用者只來自 `ToolContext.principal` 與 token**：輸入的 schema 沒有租戶或使用者欄位（`additionalProperties: false`），多出任何欄位（例如 `tenant_id`）時工具不呼叫、回錯誤說明可用的欄位；被呼叫端收不到租戶參數。
- **呼叫前先檢查**：`device_id` 要是 UUID，時間要是 RFC 3339、`from` 早於 `to`，範圍要碰得到可查期間（原始讀數 7 天、彙總 3 個月，與 telemetry-query 相同，測試比對兩邊的常數）。沒給 `interval` 時，工具選不超過 500 個區間的最細間隔（服務自己的上限是 1,500，模型讀不了那麼多）；指定的間隔超過 500 個區間、或原始讀數超過 500 筆時，回錯誤請模型改用較粗的間隔或較短的範圍。
- **結果**：只放被呼叫端回給這個使用者的欄位，精簡成表格；超過工具結果的大小上限（`tools.max_text_bytes`）時，`ToolRegistry` 改回錯誤並附各工具的縮小方式。被呼叫端的回應以本服務自己的型別解析，未知欄位忽略；`tests/delegated_tools.rs` 以 telemetry-query 與 assets 自己的回應型別產生回應，確認兩邊對得上。
- **裝置寫的文字是不可信的輸入**：文字讀數、資料點的 key（設備回報的）與型號名稱（成員輸入的）原樣進到模型的 context，可能夾帶要模型做別的事的指示（間接 prompt injection）。工具結果裡每個字串最多 256 個字元（`tools::MAX_RESULT_TEXT_CHARS`），超過的截斷並以 `…[cut]` 結尾，工具說明告訴模型這個上限，也說這些文字是資料、不是指示。上限擋得住的是單一值塞滿結果、或夾帶長篇指示；擋不住短的指示（256 個字元足以寫一句「忽略使用者的問題」），也不能讓模型分辨資料與指示。實際的保護是權限：每個工具只以提問者的身分讀資料，沒有寫入工具，模型被說服也拿不到使用者本來拿不到的東西（[docs/architecture/ai-assistant.md](../../docs/architecture/ai-assistant.md)「安全」）。
- **錯誤**（`src/callee.rs`）：只讀成功回應的本文。錯誤回應只看狀態碼，本文不讀，所以不會進到模型或日誌：

| 被呼叫端 | 工具結果 | 日誌的 `outcome` |
| --- | --- | --- |
| 400、422 | 工具自己的固定說明（參數的規則與正確寫法） | `input_error` |
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

對話、歷史與對話串存在 assistant 自己的 schema（`migrations/postgres/0001_assistant.sql`，Laysi/trustiot-v2#553；`0002_conversation_threads.sql`，Laysi/trustiot-v2#644），由 migration Job 套用（`src/schema.rs` 登錄在 `tools/migrate`）。四張表都是租戶資料，依決策 29 啟用 RLS。唯一跨租戶的是清除用的平台角色 `assistant_platform`：它只看得到、只刪得掉刪除超過 30 天的對話。

| 表 | 一列是 | 內容 |
| --- | --- | --- |
| `assistant.conversations` | 一個對話 | 擁有者（租戶、使用者）與開始時固定的 `Preamble`：system prompt 全文與版本、租戶脈絡（已跳脫、包好標籤的區塊）、工具定義（JSON 陣列，依名稱排序）、effort、快取時效；對話串的標題與來源、封存時間、刪除時間、最後活動時間 |
| `assistant.messages` | 歷史的一則訊息 | 位置（`seq`，從 0 起、依寫入的順序）、`EntryKind`、格式（`anthropic_messages`），以及訊息本身：送給模型的 JSON 文字的位元組，存成 `bytea`，最多 32 MiB |
| `assistant.discarded_rounds` | 捨棄的一輪 | 從這一輪的問題到它最後一則訊息的位置範圍（含兩端） |
| `assistant.runs` | 一次提問 | run ID（run 委派的 ID）、對話、提問者、狀態（`running`、`answered`、`failed`、`cancelled`、`interrupted`）、失敗原因代碼、開始、截止、結束與要求停止的時間；不存問題本身 |

### 權限

| 角色 | `conversations` | `messages` | `discarded_rounds` | `runs` |
| --- | --- | --- | --- | --- |
| `assistant_app`（服務角色，交易的租戶內） | SELECT；INSERT 擁有者與 `Preamble` 的欄位；UPDATE 只限 `title`、`title_source`、`archived_at`、`deleted_at`、`last_activity_at`，刪除超過 30 天的列連鎖定都做不到（restrictive policy `expired_stay_deleted`） | SELECT；INSERT（不含 `created_at`） | SELECT；INSERT（不含 `created_at`） | SELECT；INSERT run ID、對話、提問者與截止時間；UPDATE 只限 `status`、`result_code`、`ended_at`、`cancel_requested_at` |
| `assistant_platform`（清除，跨租戶） | 只看得到刪除超過 30 天的列（policy `platform_access`），只讀 `tenant_id`、`conversation_id`、`deleted_at`；DELETE 只刪得掉這些列（`platform_purge`）；restrictive policy `platform_expired_only` 讓交易設的租戶不多開任何列 | 無 | 無 | 無 |

兩個角色都沒有 TRUNCATE；服務角色在哪張表都沒有 DELETE，訊息也沒有 UPDATE。時間由資料庫填：`created_at`、`started_at` 不能寫入（沒有那一欄的 INSERT 權限），觸發器 `conversations_clock`、`runs_clock` 要求 `archived_at`、`deleted_at`、`last_activity_at`、`ended_at`、`cancel_requested_at` 改成交易的 `now()`，`last_activity_at` 不往回走，已刪除的對話在還原前 `deleted_at` 不能換成新的時間，已結束的 run 不能再改；違反時回 `42501`。所以服務角色不能把刪除時間往前改讓對話提早被清除，也不能一直往後延讓它永遠不被清除。

### 規則

- **只能新增**：歷史的訊息只會加入，不會被改寫或個別刪除，資料庫直接拒絕（`tests/history_store.rs`）。對話只能整個離開資料庫：刪除滿 30 天後由清除連同訊息、捨棄紀錄與提問紀錄一起刪掉（外鍵 `ON DELETE CASCADE`，以表的擁有者執行，不受 RLS 影響）。
- **原樣保存**：訊息不用 jsonb（會重排鍵、改寫數字），也不用 text（位元組受資料庫與連線的編碼影響）。讀回時檢查是 UTF-8、剛好一個 JSON 值，而且長度與存的相同，所以送回模型的就是存進去的位元組，thinking 區塊與 `signature` 不變。不符的列讓載入失敗（`StoreError::Corrupt`），不會送出改過的歷史。system prompt 存全文而不只存版本，新版本上線後舊對話照樣以原本的文字繼續。工具定義寫入前先確認讀回後序列化的結果相同，做不到就拒絕儲存（`StoreError::Unstorable`）。
- **大小上限**：一則訊息最多 32 MiB（`store::MAX_MESSAGE_BYTES`，資料庫的 `messages_body_size` 也擋）。訊息會出現在之後的每個請求裡，而 AI 閘道拒絕超過 32 MiB 的請求（`max_request_bytes`，Anthropic Messages 的上限），更大的訊息永遠送不出去。預設的工具結果上限是 64 KiB 文字加 4 張 5 MiB 的圖（約 20.06 MiB），其餘空間給 JSON 外框與同一步的其他結果；推導寫在 migration 裡。
- **只往後加、不分岔**：`store::create`、`store::load`、`store::append` 都回傳 `Stored`：記憶體裡的對話有幾則已經存了、下一則在資料庫的位置、讀取時捨棄過幾輪。下一次 `store::append(tx, 對話, stored)` 從那裡接著寫。`append` 先以 `FOR NO KEY UPDATE` 鎖住對話那一列，所以與刪除、捨棄、開始提問依序執行。已經存在、內容相同的訊息略過，commit 結果不明時可以安全重試；同一個位置已有不同的訊息（另一個寫入者先加了）、或 `stored` 之後有人捨棄了一輪，回 `StoreError::Conflict`，不覆寫，呼叫端重新載入。從儲存的結尾之後開始寫會留下空洞，回 `StoreError::Gap`。
- **租戶與擁有者**：每個函式（清除除外）都收 service-kit 的 `TenantTransaction` 與執行者的 `Principal`，交易的租戶不是對話的租戶時回 `StoreError::WrongTenant`。對話只屬於開始它的使用者：同租戶其他使用者的對話一律當作不存在（`StoreError::NotStored`、`None` 或清單裡沒有），處理請求的程式因此不會洩漏對話是否存在。對話的工具以擁有者的身分執行，別人接著問會變成以擁有者的身分讀資料。`store::create` 與 `store::append` 也自己核對已存的擁有者與 `Preamble`，不同就回 `StoreError::Conflict`、什麼都不寫，不靠呼叫端怎麼建出 `Conversation`（單元測試 `append_refuses_a_conversation_of_another_owner_or_preamble`）。分享給別人看是 Laysi/trustiot-v2#650。
- **軟刪除**：`store::delete` 記下刪除時間，回傳標題、刪除時間與可還原的期限（刪除後 30 天，`store::DELETED_RETENTION_DAYS`）。之後除了 `store::list_deleted`（擁有者的「已刪除」清單，只有標題與時間，沒有訊息）與 `store::restore`，所有函式都當它不存在，擁有者也一樣：讀不到、改不了名、不能封存、不能捨棄、不能開始提問，`store::append` 回 `StoreError::NotStored`、不再寫入任何訊息。刪除與正在進行的 `append` 依對話那一列的鎖排序：不是訊息先寫完再刪除，就是 `append` 因已刪除而失敗，不會刪除後還多出訊息。
- **還原**：`store::restore` 只在刪除後 30 天內有效，對話原樣回來：刪除不動訊息、捨棄紀錄與提問紀錄，還原後歷史逐位元組相同。滿 30 天的對話在擁有者的已刪除清單消失、還原回 `StoreError::NotStored`，資料庫也拒絕服務角色對它的任何更動（`expired_stay_deleted`），程式有錯也救不回來。
- **清除**：`store::purge_expired(PlatformTransaction, 上限)` 以平台角色 `assistant_platform`（service-kit 的 `PlatformDb`，每個交易記一行 `platform database access`）刪掉最多「上限」個刪除超過 30 天的對話，從最舊的開始，連同訊息、捨棄紀錄與提問紀錄；回傳刪了幾個，少於上限時表示清完了。restrictive policy `platform_expired_only` 讓平台角色不論交易設了什麼租戶，都只看得到、刪得掉刪除滿 30 天的對話（0001 的 `tenant_isolation` 沒有指定角色，也套用到平台角色，沒有這條 policy 時，設了租戶的平台交易看得到、刪得掉那個租戶的每個對話），所以清除即使寫錯也刪不到還在 30 天內或沒刪除的對話；它對訊息與其他表沒有任何權限。還原與清除同時發生在同一個對話時只有一個結果：先拿到列鎖的一方贏，另一方等它 commit 後重新檢查條件，什麼都不做。選平台角色而不是在每個租戶的交易裡以服務角色刪除：服務角色看不到哪些租戶有過期的對話，而且那樣服務角色就得有 DELETE。代價是多一個資料庫角色與帳密：清除的排程與它的連線池是 Laysi/trustiot-v2#645，在那之前 chart 不給 `assistant_platform` 帳密（`tools/migrate/tests/registry.rs` 的 `UNBOUND_PLATFORM_ROLES`）。
- **捨棄最後一輪**（重新產生回答）：`store::discard_last_round(tx, principal, id, from, stored)`，`from` 是最後一輪的問題在 `store::load` 回傳的歷史中的位置，`stored` 是 `load` 或 `append` 回傳的值，歷史在那之後不能變過（否則 `StoreError::Conflict`）。只能捨棄最後一輪：`from` 不是問題、後面還有問題或超出範圍時回 `StoreError::NotLastRound`。訊息留在資料庫，只在 `discarded_rounds` 記下範圍；`load` 略過它們，所以送給模型的歷史與那一輪之前逐位元組相同。下一輪寫在被捨棄的訊息之後（位置繼續往後數，所以位置不再等於歷史中的順序）。捨棄後要重新 `load`：手上的 `stored` 與記憶體裡的對話還含有被捨棄的一輪，`append` 會拒絕。
- **提問紀錄**：`store::start_run` 鎖住對話那一列，先把截止時間已過、還在 `running` 的提問改成 `interrupted`（服務在提問中重啟），還有進行中的提問時回 `StoreError::AlreadyRunning`；唯一索引 `runs_one_running_per_conversation` 在資料庫守同一條規則，所以同時開始的兩個提問只有一個成功。截止時間是 run 委派的截止時間，最多一小時後。`store::finish_run` 記下結果（`answered`、`failed` 加原因代碼、`cancelled`），對話在提問中被刪除時也照樣能結束；已結束的回 `StoreError::RunEnded`。`store::request_cancel` 記下第一次要求停止的時間，`store::run_status` 查詢。失敗、取消或提問中被刪除的那一輪不寫進歷史（呼叫端的規則，Laysi/trustiot-v2#646），只在這裡留下結果。
- **標題與清單**：`store::set_title` 的來源有三種：`User`（使用者改名）一律取代；`Model`（模型產生）只取代空的或暫定的標題；`Provisional`（取自第一個問題）只填空的標題。標題 1 到 200 個字元、不含控制字元。`store::archive` 封存或取消封存，封存的對話仍可使用。`store::list` 依最後活動時間（寫入訊息的時間）由新到舊，預設不含封存的，每頁最多 `store::MAX_PAGE`（100）個，以回傳的 `next` 接著取下一頁；`store::metadata` 取單一對話。
- **什麼時候真正消失**：被捨棄的訊息與刪除的對話在清除之前都還在資料庫（與備份）裡；清除之後，資料仍留在清除前做的備份裡，直到那些備份的保存期限屆滿。刪除的對話在滿 30 天後的下一次清除時刪掉；沒刪除的對話，連同它捨棄的回合，保存多久由 Laysi/trustiot-v2#652 決定，在那之前一直保存。
- **錯誤不含資料**：Postgres 的錯誤訊息與 DETAIL 會帶欄位值（例如重複鍵的租戶與對話 ID），`StoreError` 只留 SQLSTATE 或失敗的類別（`DatabaseFailure`），`Display` 與 `Debug` 都可以直接寫進日誌或回應。
- 模型上一次回報的歷史大小（`Conversation::measured`）不儲存，載入後的第一次呼叫重新估計。

### 給聊天 API 的介面

| 誰 | 用哪些函式 |
| --- | --- |
| 聊天 API（Laysi/trustiot-v2#645） | 開始對話 `create`；繼續 `load`、`append`；對話串 `list`、`metadata`、`set_title`、`archive`、`delete`、`list_deleted`、`restore`；清除的排程 `purge_expired`（平台角色的連線池算進連線預算） |
| 提問（Laysi/trustiot-v2#646） | `start_run` 後才呼叫模型，結束時 `finish_run`；停止 `request_cancel`；重新產生回答 `discard_last_round` 後重新 `load` 再問 |
| 分享（Laysi/trustiot-v2#650） | `load` 只給擁有者；分享另開一條唯讀的路徑，同樣要把刪除的對話當作不存在 |

呼叫端要遵守：同一個請求的多個呼叫放在同一個 `TenantTransaction`，由呼叫端 commit；只有回答完成的一輪才 `append`；`discard_last_round` 之後重新 `load`；`StoreError::NotStored` 對使用者一律是「找不到」，不區分不存在、別人的或已刪除。

服務啟動時建立服務角色 `assistant_app` 的連線池（verify-full 的 TLS，`database.*` 與平台設定，見「設定」），不在啟動時連線（Laysi/trustiot-v2#592）。readiness 有兩項：`postgres` 以服務角色開一條通過 service-kit session 檢查的連線，migration 檢查（`MigrationCheck` 與 `schema::SCHEMA`）確認資料庫有這一版預期的每個 migration 檔案；任一項不過時 `/readyz` 回 503，日誌寫出原因（[service-kit 的 migration 檢查](../../crates/service-kit/README.md#migration-檢查migrations)）。readiness 不呼叫 AI 閘道或其他服務。chart 在 `mesh.calls.assistant` 列了 `postgres` 時給它帳密與連線池大小（`services.assistant.database.maxConnections`，算進 chart 的連線預算，見 chart README 的「連線預算」，Laysi/trustiot-v2#575），migration Job 讓 `assistant_app` 能登入。聊天 API（Laysi/trustiot-v2#645）呼叫上面「給聊天 API 的介面」列的函式。

## 設定

設定來自 `TRUSTIOT__` 開頭的環境變數（`__` 分隔層級、名稱轉小寫；時間長度寫成 `30s`、`10m`），或 `TRUSTIOT_CONFIG_FILE` 指的 TOML 檔，規則與 service-kit 相同（[crates/service-kit/README.md](../../crates/service-kit/README.md)）。只在啟動時讀；錯誤訊息只寫設定名稱，不寫值。

| 設定 | 變數 | 預設 | 說明 |
| --- | --- | --- | --- |
| 平台設定 | `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 必填 | Postgres 的位置；只接受 `verify-full`，`_CA_FILE` 是伺服器憑證唯一的信任錨。由部署設定（chart、`bun scripts/dev.ts env`） |
| `database.username`、`database.password` | `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | 必填 | 服務角色 `assistant_app` 的帳密；chart 的密碼來自 Secret `<release>-assistant-password` 的 `assistant_app` key。沒有平台角色 |
| `database.max_connections` | `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | `4` | 服務角色連線池每個副本的上限，2 到 100；聊天 API（Laysi/trustiot-v2#146）之前只有 readiness 使用。取得連線最多等 5 秒（`ACQUIRE_TIMEOUT`）。chart 以 `services.assistant.database.maxConnections`（預設 4）設定，算進 chart 的連線預算 |
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
| `identity.exchange_url` | `TRUSTIOT__IDENTITY__EXCHANGE_URL` | 無 | 身分服務 token 交換埠的 `http://<host>[:<port>]`（`http://<release>-identity-exchange:8082`），不能有路徑、帳密或 query；沒有就沒有工具。呼叫身分服務的時間上限是 15 秒（`IDENTITY_TIMEOUT`） |
| `identity.run_lifetime` | `TRUSTIOT__IDENTITY__RUN_LIFETIME` | `15m` | 一次提問的委派效期；整數秒，2 到 15 分鐘（身分服務的上限 900 秒）。提問在到期前 30 秒結束 |
| `telemetry_query.url` | `TRUSTIOT__TELEMETRY_QUERY__URL` | 無 | telemetry-query 的 `http://<host>[:<port>]`；沒有就沒有 `telemetry_series`、`telemetry_latest`。設了就要有 `identity.exchange_url` |
| `assets.url` | `TRUSTIOT__ASSETS__URL` | 無 | assets 的 `http://<host>[:<port>]`；沒有就沒有 `device_models`。設了就要有 `identity.exchange_url` |

工具呼叫其他服務的時間上限與 `tools.call_timeout` 相同（`ToolRegistry` 的上限才是真正的界線），連線最多 10 秒，成功回應的本文最多 8 MiB。mesh 內是純 HTTP，由 ztunnel 加上 mTLS；不用環境變數的 proxy，不跟隨轉址（token 的 header 會跟著轉過去）。chart 從 `mesh.calls.assistant` 填 `ai_gateway.url`、`identity.exchange_url`、`telemetry_query.url` 與 `assets.url`，`services.assistant.env` 不能設定這四個（[chart README](../../deploy/chart/README.md)「AI 助理」）；授權矩陣在同一份 README 的「呼叫矩陣」。

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

不記問題、回答、工具輸入與結果、錯誤訊息，也不記被呼叫服務的 URL、查詢參數、回應本文與 token（只記狀態碼的類別）。欄位名稱都不會被 service-kit 的遮蔽規則擋掉（不用 `token`、`tokens`、`prompt`、`content` 這些字）。指標 `assistant.model_calls` 依 `outcome` 與 `cache`（`hit` 或 `miss`）計數；`cache_read` 長期為 0（只有 `miss`）表示快取失效，要接到監控（Laysi/trustiot-v2#554 以真實 API 驗證快取）。

## 之後接上的地方

| 項目 | 介接點 |
| --- | --- |
| 聊天 API 與串流（Laysi/trustiot-v2#146） | `Agent::start_conversation`、`Agent::ask` |
| 聊天 API 開始一個問題（Laysi/trustiot-v2#146） | 處理請求時以 `delegation::QuestionRuns::start(&UserIdentity, conversation.principal())` 取得 `QuestionRun`，交給 `Agent::ask`；`Assistant::runs` 為 `None`（沒有設定 `identity.exchange_url`）時改用 `tools::NoDelegation`。`StartError` 已實作 `IntoResponse`（見「代表使用者呼叫」） |
| 更多工具（語意層、唯讀 SQL、儀表板） | 實作 `tools::Tool`，放進 `ToolRegistry`（最多 10 個），以 `callee::Callee` 呼叫服務；工具從 `ToolContext.principal` 取租戶與使用者，不從輸入取 |
| 對話存進 Postgres（Laysi/trustiot-v2#553 儲存、Laysi/trustiot-v2#644 對話串與提問紀錄已完成，連線池由 Laysi/trustiot-v2#592 接上） | `store` 的函式，在對話租戶的 `TenantTransaction` 裡呼叫，交易取自 `start` 建立的 `TenantDb`（現在只給 readiness 用）；清除另用 `assistant_platform` 的 `PlatformDb`（Laysi/trustiot-v2#645 建立），見[給聊天 API 的介面](#給聊天-api-的介面) |
| 每次呼叫預扣額度（Laysi/trustiot-v2#140） | 每次呼叫的 `ModelRequest::call_id`（帳本的 `llm_call_id`，現在只記在日誌）；閘道拒絕時以 `ModelError` 回到迴圈 |

## 測試

`cargo test -p assistant`（`tests/history_store.rs`、`tests/conversation_threads.rs`、`tests/startup.rs` 與 `tests/cross_tenant.rs` 以 podman 或 docker 起一個拋棄式 TLS Postgres，以 migration Job 套用整個產品的 schema；沒有容器執行環境時在本機略過，設了 `CI` 時失敗）。測試以本機 loopback 上的替身 AI 閘道回放 `tests/fixtures/` 的回應（取自官方文件，見該目錄的 README），不呼叫供應商，也沒有 API key。

| 檔案 | 內容 |
| --- | --- |
| `tests/agent.rs` | 60 步的長分析不受步數限制；接近預算時收尾並回答；放不進剩餘預算的呼叫不送出、輸出上限降到剩下的空間；用量超過估計而預算用完時工具不執行；第二個問題的請求逐位元組以前一次請求的歷史開頭（thinking 與 signature、system、工具定義不變，租戶脈絡包在標籤裡）；兩個使用者同時發問互不等待；模型逾時與閘道錯誤只給固定訊息、日誌沒有錯誤文字；拒答不留在歷史；每一步的時間與快取讀取都記在日誌 |
| `tests/tools.rs` | 工具組規則（最多 10 個、名稱、schema）；並行執行、結果依呼叫順序；一步超過呼叫數上限的呼叫不執行；輸入錯誤、失敗、panic、逾時、不存在的工具各自回錯誤給模型；結果超過大小與圖片上限時請模型縮小範圍；工具拿到的身分來自對話；圖片照 Anthropic 格式送給模型 |
| `tests/delegated_tools.rs` | 三個工具各以使用者身分、受眾正確且在呼叫當下交換的 token 呼叫服務，請求裡沒有租戶或使用者，超過 60 秒後的呼叫仍成功、舊 token 會被拒（替身以「每次模型呼叫過 61 秒」的虛擬時鐘模擬，不真的等）；服務回 403、404、422、503 與非 JSON 時模型與日誌都看不到回應本文；身分服務拒絕交換（撤銷）時該步停止、問題結束、不再重試；身分服務暫時無法使用只讓該次呼叫失敗；問題在委派到期前結束、沒有剩餘時間的問題不開始；只有對話的使用者能開始問題，為別人的對話建立的 run 在送出任何東西前被拒；裝置寫的長文字截到 256 個字元、加上截斷標記才給模型；工具輸入不收租戶、使用者與多餘欄位；工具的路徑與上限跟 telemetry-query、assets 的程式一致 |
| `tests/history_store.rs` | 主 agent 跑完的對話（thinking 與 signature、工具呼叫與結果）存進去、讀回來逐位元組相同，載入的對話接著問時請求的前段與歷史和原本的逐位元組相同；服務角色的 UPDATE、DELETE、TRUNCATE 在租戶交易內外都被資料庫拒絕（`42501`）；重複寫入不改變什麼，兩個寫入者分岔時後者被拒、不覆寫，空洞與沒建立的對話被拒；其他租戶與同租戶的其他使用者載入不到；損壞的列讓載入失敗 |
| `tests/startup.rs` | 執行檔本身：以 `assistant_app` 登入、資料庫有這一版的 migration 時 `/readyz` 回 200，日誌記下工具數且不含密碼；密碼錯誤時 readiness 記下失敗、回 503；少了一個 migration 檔案時日誌寫出缺的檔案、回 503，補回後恢復 |
| `tests/chart_connections.rs` | chart 的 Postgres 連線預算（`trustiot.postgres.connectionBudgets`）與程式開的連線池一致：沒有其他連線池，`database.maxConnections` 的範圍與服務接受的相同；程式多了一個連線池時失敗 |
| `tests/conversation_threads.rs` | 對話串（Laysi/trustiot-v2#644）：刪除的對話對擁有者與同租戶的人都不存在、只在擁有者的已刪除清單出現，還原後每一列與刪除前相同；滿 30 天的不能還原，清除連同訊息、捨棄紀錄與提問紀錄刪掉（跨租戶），沒過期的與沒刪除的不受影響；清除角色只看得到、刪得掉過期的對話，碰不到訊息，服務角色什麼都刪不掉（`42501`）；還原與清除、刪除與寫入同時發生時兩種順序各測一次（以 `pg_blocking_pids` 確認後到的一方在等鎖，不靠時間）；捨棄最後一輪後歷史與之前逐位元組相同、下一輪接在後面，捨棄不是最後一輪的被拒；同時開始兩個提問只有一個成功，過了截止時間的提問在下一次開始時變成 `interrupted`；同租戶的其他使用者讀、改名、封存、刪除、還原、捨棄、開始與停止提問都當作不存在、資料不變；資料庫拒絕指定 `created_at`、`started_at` 與往前改的時間（`42501`），拒絕超過 32 MiB 的訊息（`23514`）；`StoreError` 不含 Postgres 的訊息與 DETAIL；標題規則、封存與清單分頁 |
| `tests/cross_tenant.rs` | 跨租戶負向測試（決策 29，`crates/testkit`）：四張表（對話、訊息、捨棄的回合、提問紀錄）的讀、寫、搬移、插入探測與覆蓋檢查 |
| `src/` 的單元測試 | 設定檢查（含被呼叫服務的位址、委派時長與資料庫帳密、連線池上限）、截止時間的計算、`StartError` 的錯誤碼、字串上限、時間區間的粒度選擇與查詢範圍、狀態碼類別、預算門檻、租戶脈絡的跳脫、狀態碼對應、錯誤 `type` 過濾、沒有用量時的估計、送出前的訊息估計（圖片按張數）、工具定義與訊息讀回的位元組檢查、migration 與程式的上限一致（30 天、32 MiB、200 個字元）、資料庫錯誤只留類別、失敗原因代碼的格式 |
