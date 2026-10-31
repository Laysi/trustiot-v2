# assistant

AI 助理（決策 31）的主 agent 迴圈：使用者對自己租戶的資料發問，主 agent 透過工具讀資料、回答。對話歷史只往後加、依 prompt caching 的順序排列，每個問題以 token 預算收斂，不設步數上限。只經由 AI 閘道（[services/ai-gateway](../ai-gateway/README.md)）呼叫 Claude，模型藏在一個介面後面（決策 15）。設計在 [docs/architecture/ai-assistant.md](../../docs/architecture/ai-assistant.md)；本服務由 Laysi/trustiot-v2#145 建立。

API 在 `/api/assistant/v1`（見 [API](#api)）：使用者自己的對話串，開始、列出、讀取、改名、封存、刪除與還原（Laysi/trustiot-v2#645），以及儀表板的讀取、發布、還原、封存與資料集的執行（Laysi/trustiot-v2#150；儀表板的儲存與執行見「儀表板」）。在對話裡發問與串流是 Laysi/trustiot-v2#646、Laysi/trustiot-v2#147。

## 檔案

| 路徑 | 內容 |
| --- | --- |
| `src/api.rs` | API 的路由組合、OpenAPI 文件（`ApiDoc`）與各資源共用的部分：請求身分換成 `Principal`、M1 錯誤回應、請求的交易、本文與分頁規則、cursor 的編碼；每個資源一個子模組，在 `api::routes` 合併 |
| `src/api/conversations.rs` | 對話串的端點與請求、回應型別 |
| `src/api/dashboards.rs` | 儀表板的路由：列表、開啟、版本、發布、還原、封存、執行資料集 |
| `src/api/held_body.rs` | 資料集執行的回應本文：分段交給連線、連線寫出一段才給下一段，持有結果的記憶體到最後一段交給連線或本文被丟棄；連線在 60 秒（`HELD_BODY_TIMEOUT`）內沒取走整個本文時，放回記憶體、中斷回應並關閉連線 |
| `src/agent.rs` | 主 agent 迴圈：呼叫模型、執行工具、收尾、錯誤與日誌 |
| `src/model.rs` | 模型介面 `ModelClient` 與中性型別（`ModelRequest`、`ModelTurn`、`TokenCount`、`ModelError`）；原生格式的訊息是不透明的 `NativeMessage` |
| `src/model/anthropic.rs` | 經 AI 閘道的 Anthropic Messages：請求的快取排列、回應解析、錯誤對應 |
| `src/history.rs` | 對話：固定的前段（`Preamble`）與只往後加的歷史 |
| `src/tools.rs` | `Tool` trait、`ToolRegistry`（上限、並行、逾時、結果大小）、`ToolContext` 與 `Delegation` |
| `src/tools/telemetry.rs` | 工具 `telemetry_latest`（telemetry-query 的歷史 API） |
| `src/tools/semantic.rs` | 工具 `telemetry_statistics`、`telemetry_usage`（telemetry-query 的語意查詢 API，Laysi/trustiot-v2#142） |
| `src/tools/device_models.rs` | 工具 `device_models`（assets 的設備型號列表） |
| `src/tools/telemetry_sql.rs` | 工具 `telemetry_sql`（telemetry-query 的內部 SQL 入口），拒絕類別對應的固定說明 |
| `src/tools/dashboard.rs` | 工具 `dashboard`（Laysi/trustiot-v2#150）：列出、讀取、寫入儀表板；寫入前執行資料集與檢查頁面，問題是封閉的類別與固定說明 |
| `src/answer_guard.rs` | 回答的過濾：移除回答裡的 fenced code、含程式碼的 inline code、HTML 與 SQL，回報移除的類別；`find` 與 `pending` 給串流（Laysi/trustiot-v2#147）用 |
| `src/callee.rs` | 工具與儀表板的資料集執行以使用者身分呼叫其他服務的 HTTP client（GET 與 JSON 本文的 POST）：每次呼叫換 token、錯誤只看狀態碼（`telemetry_sql` 的 422 例外，嚴格解析成封閉的拒絕型別）；`Caller` 是呼叫代表誰、日誌記成哪個工具或資料集執行 |
| `src/delegation.rs` | 一次提問的 run 委派：`QuestionRuns::start`（聊天 API 的入口）、`QuestionRun`（截止時間、撤銷）；`OnBehalfOf` 以 API 請求的身分換 token（資料集執行） |
| `src/dashboards/store.rs` | 儀表板與版本的 Postgres 儲存：一個函式一個完整操作、鎖、誰看得到與誰能改（草稿的可見性只由 `conversation_access` 決定） |
| `src/dashboards/runner.rs` | 資料集的執行：以呼叫者的身分呼叫 telemetry-query，回答到了才預留記憶體，回答只在結果的上限內讀出，回答與錯誤對應成封閉的結果，結果以 JSON 帶著它的預留交出 |
| `src/dashboards/caps.rs` | 資料集執行的上限：每個租戶、每個副本每分鐘 60 次、同時 2 個 |
| `src/dashboards/memory.rs` | 每個副本的結果記憶體預算（以位元組計、一個租戶最多一半、有上限的等待），執行、回應本文與 AI 寫入共用；每個租戶的回應空間（預算的十六分之一） |
| `src/dashboards/flatten.rs` | 語意資料集（統計、用量）的回答攤平成表格，最多 `max_rows` 列 |
| `src/dashboards/checker.rs` | 檢查服務（services/dashboard-check）的 client：每個副本同時最多 2 次檢查、送出檢查、判定解析前先數值、收不到可用判定的每種情況對應成封閉的原因、一次檢查佔的記憶體 |
| `src/dashboards/limits.rs` | 每個鍵（租戶）的名額與有上限的等待：`dashboard` 工具每個租戶同時一次寫入；API 的發布與還原每個使用者、每個租戶每分鐘的次數（`VersionBudget`）；不組出 JSON 就算出長度（`json_len`） |
| `src/budget.rs` | 每個問題的 token 預算 |
| `src/prompt.rs` | 平台 system prompt（有版本）與對話中的固定指示 |
| `src/config.rs` | 設定與啟動檢查 |
| `src/store.rs` | 對話與歷史的 Postgres 儲存：只往後加、原樣保存、依租戶隔離，捨棄最後一輪；錯誤型別 `StoreError` |
| `src/store/threads.rs` | 對話串：清單、標題、封存、軟刪除、已刪除清單、還原、30 天後的清除 |
| `src/store/runs.rs` | 提問紀錄：開始（一個對話同時只有一個進行中）、結束、要求停止、查詢 |
| `src/purge.rs` | 排程清除：每個副本排程、同一時間一個副本以平台角色分批刪除過期的對話 |
| `src/schema.rs` | 本版本預期的 migration 檔（`migrations/postgres/`：`0001_assistant.sql` 對話、`0002_conversation_threads.sql` 對話串、`0003_dashboards.sql` 儀表板），`tools/migrate` 也編進它 |

## 一次問答怎麼跑

1. 問題以使用者訊息加到歷史最後。
2. 呼叫模型。請求本文依序是：`tools`（依名稱排序）、`system` 的兩個區塊（平台 system prompt、租戶脈絡，各有一個快取斷點；租戶脈絡是成員輸入的文字，跳脫 `&`、`<`、`>` 後包在 `<tenant_context>` 標籤裡，system prompt 說明它是資料）、`messages`（整段歷史，加上頂層 `cache_control` 的自動快取）。另有 `thinking: {"type": "adaptive"}`、`output_config.effort` 與 `max_tokens`；不帶 `model`，由閘道依角色填入。
3. 模型要求工具時，同一回合的工具並行執行，各有時間上限；結果依呼叫順序放在一則使用者訊息交回。模型的這一回合連同工具結果一起加到歷史，然後回到第 2 步。
4. 模型回答時，回答先經過「回答的過濾」（下方）再加到歷史，問答結束；歷史與呼叫端拿到的是同一份過濾後的文字。

一次提問最長到它的截止時間（見下方「代表使用者呼叫」）：時間到就以「提問超時」結束，不論正在呼叫模型或執行工具。歷史只在一個模型回合與它的工具結果都完成時才一起加入，所以在任何時點中斷都不會留下半個步驟。

步數沒有上限。限制來自 token 預算：每次呼叫處理的 token（輸入、快取寫入、快取讀取、輸出，閘道的每次嘗試都算）累計到 `budget.wind_down_percent`（預設 80%）時，下一次呼叫前加一則對話中的 system 訊息，請模型不再呼叫工具、用現有的資料回答；累計到整個預算時，模型再要求的工具不執行（每個呼叫回一個說明原因的錯誤結果），問答以「預算用完」結束。每次呼叫送出前先檢查放得進剩下的預算：輸入以模型上一次回報的歷史大小（`Conversation::measured`）加上之後新增訊息的高估值估計（JSON 每 2 位元組算 1 token，每張圖 5,000 token；對話的第一次呼叫連 system 與工具定義一起估），`max_tokens` 降到剩下的空間；不到 1,024 token 時不送出，以「預算用完」結束。供應商沒回報用量時，以請求或回應的位元組數估計（每 3 位元組算 1 token），不當 0。

### 歷史只往後加

- 每則訊息以收到或第一次寫出的 JSON 文字保存，之後原樣送回：thinking 區塊與 signature、工具呼叫、未知的區塊與欄位都不變。所以下一次請求的 `messages` 一定以上一次的 `messages` 開頭，逐位元組相同。
- system prompt 與版本、租戶脈絡、工具組、effort 與快取時效在對話開始時固定（`Preamble`），之後不變；新的 prompt 或工具只用在新對話。
- 收尾指示是對話中的 system 訊息，只跟著它引出的模型回覆一起加入歷史；呼叫失敗或拒答時不留下，避免 system 訊息之後直接接下一個問題。
- 工具步驟與結果在聊天畫面隱藏（`EntryKind::shown`），但留在歷史裡。
- 拒答或內容不完整的回合不加入歷史；之後的問題照常接在後面。

### 回答的過濾

驗收條件「回答文字不出現 SQL 或 HTML」（Laysi/trustiot-v2#150）：使用者讀回答、儀表板顯示資料，回答不帶儀表板的 SQL 或頁面，也不帶其他程式碼。system prompt 這樣要求，`answer_guard` 讓它不論模型寫了什麼都成立。

- **移除什麼**（封閉的三類，`answer_guard::Removed`）：`code` 是 fenced code（```` ``` ```` 或 `~~~`，列表與引用裡也算，沒關閉的到結尾）與含程式碼的 inline code（有 `=`、`;`、`{`、`}`、`->` 或 `f(x)` 這種呼叫；資料集名稱、單位、時區這類反引號裡的名稱保留）；`html` 是 HTML 的 tokenizer 會當成標籤的任何開始或結束標籤：`<` 或 `</` 加英文字母開頭，一直到結束它的 `>`（引號內屬性值裡的 `>` 不算），不論長度與名稱：`<widget>`、`<svg:path>`、含其他文字的名稱與 `<device>` 這種佔位字都算，因為解析器一樣當成元素；`<` 加字母到下一個 `>` 之間的文字對解析器就是標籤，所以「電壓<V1時跳脫，電流>5A」會少掉「<V1時跳脫，電流>」（寧可多移除）；後面沒有 `>` 的 `<` 加字母（`x<y`）保留，解析器也不會產生標籤，但已經有引號屬性值的（`<img src="x"` 到結尾）是被截斷的標籤，移除到結尾；CommonMark 的自動連結只有 `http`、`https`、`mailto` 與 e-mail（`<https://…>`、`<ops@example.com>`）保留成連結，其他 scheme 對解析器是標籤；還有註解（到 `-->`）、`<![CDATA[…]]>`、`<!DOCTYPE>`、`<?xml …?>`、`<!ENTITY …>`、`</ …>` 這類宣告（到引號外的 `>`），沒結束的都到結尾；以字元參照寫的標籤（`&lt;div&gt;`、`&#60;b&#62;`），`<script>`、`<style>` 連內容一起；`sql` 是以敘述關鍵字開頭、帶有對應子句（`SELECT … FROM`、`INSERT INTO`、`UPDATE … SET` …）的敘述，以及單獨一行或在程式碼裡、後面只有運算式的 `SELECT`（`SELECT 1`、`select now()`；在句子中間時運算式要有呼叫、運算子或字串，例如「請執行 select max(kwh) 看尖峰」），inline code 裡不分大小寫，文字裡要有更多 SQL 的特徵（句子中間的大寫 `SELECT 欄位 FROM 表`、小寫但欄位名稱帶 `_` 或逗號的 `select device_id, ts from t`、`SELECT` 或 `FROM` 其中之一大寫的 `SELECT kwh from devices` 也算；首字大寫的 `Select device_id From devices` 要有 SQL 的結構且沒有英文冠詞；行首、冒號後或表格儲存格裡只有關鍵字與單純名稱的敘述不分大小寫、不論怎麼結尾都算，例如 `Query: select value from devices.`、`delete from devices`），表格的儲存格開頭與行首一樣、`|` 結束敘述；ClickHouse 與 MySQL 以反引號括住的名稱（`` SELECT `device_id` FROM `devices` ``）算在敘述裡，兩個以上反引號開頭的 inline code 到同樣長度的反引號才結束（CommonMark 的規則），所以「Select the meter from the list」、「超過 <5% 的電表」、中文裡的 select 這類文字保留（單元測試列出這些邊界）。
- **怎麼套用**：模型的回答回合在加入歷史前，每個文字區塊各自過濾（`ModelClient::rewrite_text`），thinking 區塊與 signature 等其他區塊逐位元組不變；整段都被移除時，文字是「…」。日誌記一行 INFO `removed content from an answer`（只有移除的類別）。
- **給聊天 API**：`Answer::removed` 是移除的類別（序列化成 `["code", "html", "sql"]` 的子集），聊天畫面據此顯示 `assistant.answer_content_removed`（zh-TW 與 en 的翻譯在 `frontend/src/i18n/locales/`）。本服務還沒有聊天 API：回應帶出這個欄位、存進對話歷史讓重新載入時也顯示，由 Laysi/trustiot-v2#645、#646 接上（見「之後接上的地方」）。
- **串流**：`answer_guard::find` 是同一個偵測器，回傳每段要移除的位置；`answer_guard::pending` 回傳目前為止的文字從哪裡開始還不確定，Laysi/trustiot-v2#147 送出它之前、去掉 `find` 找到的部分，其餘暫緩。它是下列最早一處所在段落（最後一個空行 `\n\n` 之後；除了標籤，每種掃描都在空行結束）的開頭：文字結尾（最後一段裡的敘述、inline code、以字元參照寫的標籤可能還會變長）、第一個延伸到文字結尾的發現處（沒關閉的 fence、註解、宣告，或停在引號屬性值裡的標籤）、最後一個 fence 之後第一個還沒被 `>` 結束的 `<` 加字母（標籤可能幾段之後才結束）。單元測試對幾段回答的每個前綴檢查：`pending` 之前的發現處與整段回答的相同。
- **長度**：每種掃描都是線性的（所有候選標籤一起掃一遍：tokenizer 的下一個狀態只看目前狀態與位元組，同一位置、同一狀態的候選合併成一組，同時最多 20 組；宣告的結尾以引號與 `>` 的索引找到；一個敘述最多讀 32 KiB），一長串重複的 `select`、未關閉的標籤、宣告、註解、字元參照或反引號不會讓過濾變慢（單元測試）。

## 工具

工具在每次呼叫其他服務前，以這次提問的委派換一張受眾是該服務的 token（`Delegation::token_for`），帶在 `x-internal-identity`，呼叫的是 web 前端經入口 gateway 呼叫的同一組 API（`/api/...`）；`telemetry_sql` 是例外，呼叫只給 AI 用的內部入口 `POST /internal/telemetry/v1/sql`（入口 gateway 不轉送 `/internal`，瀏覽器送不了 SQL）。被呼叫端照一般請求檢查 token 的受眾、使用者的租戶與角色，所以工具讀得到的正是使用者自己讀得到的（決策 27、29）。

| 工具 | 呼叫 | 輸入 | 回給模型 |
| --- | --- | --- | --- |
| `telemetry_latest` | telemetry-query `GET /api/telemetry/v1/devices/{device_id}/latest` | `device_id`，選填 `points`（最多 100 個資料點） | 每個資料點一列 `[資料點, 時間, 值]`，依資料點排序，與總數 `total`；最多 200 列 |
| `telemetry_statistics` | telemetry-query `POST /api/telemetry/v1/statistics` | API 的請求本文：`series`（1 到 20 個 `{device_id, point}`）、`tz`（IANA），`period`（例如 `this_month`、`last_7_days`）或 `from` 加 `to`，選填 `interval`（`1m` … `1mo`、`total`，預設 `total`） | 實際查詢的範圍（`range`），每個資料點的筆數、最小與最大（含所在分鐘或小時的起點）、平均、第一與最後讀數；有 `interval` 時另有每個有讀數的區間一列 `[起點, 筆數, 最小, 最大, 平均, 最後讀數]` 與平均最高、最低的區間；`notes` 說明沒有資料（資料不足）與被截到可查期間；最多 500 列，超過時只留各資料點的數字 |
| `telemetry_usage` | telemetry-query `POST /api/telemetry/v1/usage` | 同 `telemetry_statistics` | 每個資料點的 `status`（`ok`、`no_data`、`not_numeric`、`not_cumulative`），`ok` 的有 `total`、讀數、重置、斷訊、可疑跳增與累計檢查；有 `interval` 時另有每個有讀數的區間一列 `[起點, 用量, 讀數, 重置, 斷訊後, 可疑跳增]` 與用量最高、最低的區間；`notes` 是每種狀況給模型的固定說明（改用 `telemetry_statistics`、資料不足、無法確認是累計值、總量可能含錶的誤讀）；沒有任何資料點算得出用量時是錯誤結果；最多 500 列 |
| `device_models` | assets `GET /api/assets/v1/device-models` | 選填 `name_contains` | 每個型號一列 `[型號 ID, 名稱, 版本, 資料點數, 更新時間]`，依名稱排序，與總數 `total`；最多 200 列 |
| `telemetry_sql` | telemetry-query `POST /internal/telemetry/v1/sql`（[services/telemetry-query](../telemetry-query/README.md)「AI 的唯讀 SQL」） | `sql`（一個 `SELECT`，最多 16 KiB）；列數上限固定送 200，模型改不了 | `columns`（名稱與 ClickHouse 型別）、`rows`、`row_count`、各 view 的 `queryable_from` 與 `queried_at`；服務印回的 SQL 不回給模型。超過 200 列時服務整個拒絕、不截斷 |
| `dashboard` | 本服務的儀表板儲存；寫入時 telemetry-query（資料集）與檢查服務 | `action`：`list`、`read`（`dashboard_id`、選填 `version`）、`write`（見下方「`dashboard` 工具」） | `list`：這個對話的草稿與租戶未封存的已儲存儀表板，最多 50 個；`read`：那一版的 metadata 與完整的 spec（第一個文字區塊）、完整的頁面（第二個）；`write`：存下的版本、各資料集的列數與前 3 列、標記的值（取自自己的資料、不含刻度），或最多 20 個問題 |

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

### `dashboard` 工具

主 agent 以一個工具、三個動作做儀表板（`src/tools/dashboard.rs`，設計紀錄「主 agent 的 `dashboard` 工具」），不發布、不還原、不封存：那些是使用者在 API 做的。

**寫入**（`write`）只改輸入寫到的部分：`datasets`、`computations`、`constants` 各是 `{upsert: [...], remove: [名稱]}`（先移除、再依名稱取代或加在最後），`page` 換掉整個頁面，`page_edits` 是 `[{old, new}]`，每個 `old` 在套用前面的修改後的頁面裡要正好出現一次；`title`、`description`（`null` 移除）、`locale`（`zh-TW` 預設或 `en`）、`tz`（預設 `UTC`）是檢查呈現頁面的方式。依序：

1. **驗證**：輸入嚴格解析（多一個欄位就整個拒絕，例如 `tenant_id`）；合併後的版本以 contract 的 `validate_version` 檢查（上限、名稱、計算語言的語法與參照，spec 加頁面最多 56 KiB、頁面最多 40 KiB、最多 8 個資料集），`tz` 依檢查服務的形式，標題、說明、頁面不能有 U+0000。
2. **授權**：沒有 `dashboard_id` 是這個對話的新草稿；這個對話的草稿要給 `base_version`，而且要是最新版；已儲存的儀表板（未封存）以讀到的版本成為這個對話的新草稿，一個對話對同一個已儲存的儀表板只有一份草稿（再寫時告訴模型那份草稿的 ID）；`rebase_saved_version` 只用於取自已儲存儀表板的草稿（合併了它的新版本，之後發布不再衝突），而且只接受這次提問讀過（`read`）的那個已儲存儀表板的版本：每個問題記得它讀過的已儲存版本（最近 64 個，`MAX_SAVED_READS`）到截止時間，沒讀過的回固定的說明（請它先讀那一版、合併後再寫），不執行任何東西、不算進寫入次數。store 讓使用者看得到自己所有對話的草稿，工具只認這個對話的：別的對話、別的使用者與別的租戶的草稿都是「不存在」。
3. **輪到這個租戶**：每個租戶在每個副本同時只有一次寫入走第 4 到 7 步，不分成員：一位成員（包括 viewer）的寫入慢，同租戶其他成員的 AI 寫入就要等它。先等這個租戶的另一次寫入，再為這次要重用的同一問題的結果 JSON 預留記憶體（見「記憶體」），兩段合計最多 30 秒（`WRITE_WAIT`），等不到回 `check_busy`，不執行任何東西、不算進問題的寫入次數。所以一個租戶同時最多一次檢查。所有租戶的寫入沒有總名額：別的租戶的資料集執行慢（在等 telemetry-query）不會讓這次寫入忙碌。檢查名額（第 6 步）與副本的記憶體預算（見「記憶體」）仍由所有租戶共用，等不到時回 `check_busy`。
4. **執行資料集**：以提問者的身分、這次提問的委派（`ToolContext::caller("dashboard")`）經與 API 共用的 `DatasetRunner` 執行（同一組每租戶上限），一次兩個，一個跑完就接下一個。從呼叫開始算 120 秒（含讀取要改的儀表板）還沒跑完的資料集各是一個 `dataset_timeout` 問題，跑完的結果留給同一個問題的下一次寫入，那次只執行沒跑完的。同一個問題裡同一個資料集（來源加 `max_rows`）只執行一次，之後的寫入重用結果；結果以 JSON 保留到問題的截止時間，以實際佔用的位元組（key 加 JSON）計算，所有問題合計最多 64 MiB（`MAX_CACHED_BYTES`），一個租戶的問題合計最多其中四分之一，16 MiB（`MAX_CACHED_BYTES_PER_TENANT`，放得下 8 個 2 MiB 結果中的 7 個）：先讓出這個租戶最久沒寫的問題的結果，整個快取滿了時再讓出任何租戶最久沒寫的問題的結果，所以一個租戶最多為自己的份額騰出空間，拿不走其他租戶的全部快取；讓出的結果下次要用時重新執行。一次寫入在資料集執行期間只拿著結果的 JSON，跑完才讀回成值給計算與檢查。每次執行在 telemetry-query 回答後才預留自己的記憶體，等不到是那個資料集的 `runs_limited` 問題。委派被撤銷時提問結束（`ToolError::Revoked`）。
5. **計算**：先為結果讀回成值預留記憶體（`values_bytes`），讀回後公式以資料集實際回來的欄位再檢查一次（`Spec::check_formulas`）。
6. **檢查**：先為檢查請求本文與判定預留記憶體（`check_bytes`），再送 `CheckRequest`（runtime 版本、頁面、公開的 spec（沒有 SQL 與語意請求）、資料集結果、locale、tz）到檢查服務（`dashboards::checker`），判定經 `CheckVerdict::validate` 確認每個參照、值與標籤都來自送出的資料。第 5、6 步的記憶體合計最多等 30 秒（`MEMORY_WAIT`），等不到回 `check_busy`；這時資料集已經執行，所以算進問題的寫入次數，結果留給下一次寫入。每個副本同時最多 2 次檢查（`MAX_CHECKS_IN_FLIGHT`：檢查服務執行一個、佇列一個），從組請求本文到確認判定都佔著名額，依先後取得；等 30 秒（`CHECK_QUEUE_WAIT`）沒有名額時回 `check_busy`，不呼叫服務。
7. **寫入**：只有判定通過才在問題租戶的交易裡寫一版（`create_draft`、`write_draft` 或 `fork`），寫入者是對話的擁有者與這次的 run ID。

**頁面的文字要讀得清楚**：工具的說明告訴模型這些規則；檢查與沙箱的 runtime 都拒絕不符合的頁面，所以模型照著寫就不必為此多花寫入次數。標籤水平或最多斜 45°（刻度標籤擠時斜 −45°，不直立）；文字與圖形不鏡像、不拉伸，圓餅圖保持圓形；字級讀得到、字距正常、顏色實在（不淡到幾乎透明）；文字用一般的文字元素、整段放在一起，不逐字定位（SVG 的 `x`、`y`、`dx`、`dy`、`rotate` 清單）、不用 `textLength`、`textPath`、透視或 3D。

**模型從判定得知的**：只有三種來源，其他的都丟掉：封閉的集合、對工具自己送出的東西（頁面文字、宣告的名稱、資料集結果）做過的檢查、工具自己的資料。通過時，`check` 是標記的數量與最多 40 個標記的參照與值，值取自工具自己的資料集結果（`CheckRequest::value_of`），不含座標軸刻度（`t|…`，它的值是頁面寫的，不是資料）；判定報的值與自己的資料不同時判定不可用（`check_invalid_verdict`），不存。不通過時每個問題的欄位：

- `script_error`：只有 `error_class`，以訊息開頭的名稱對應 JavaScript 的錯誤類別（`Error`、`AggregateError`、`EvalError`、`RangeError`、`ReferenceError`、`SyntaxError`、`TypeError`、`URIError`），其他或沒有訊息是 `other`；不傳訊息本身。判定不帶行號與欄號，所以也不給。
- 參照（`unmapped`、`mismatch`、`mark_geometry`）：在資料裡解得出的是 `reference`；刻度只給它所在的軸 `axis`（`dataset`、`column`），不給刻度的值；超出結果的列給 `row_past_end`（`dataset`、`column` 與自己結果的 `row_count`），不給列號。`mismatch` 的 `value` 是該參照在自己資料裡的值。
- `literal_number` 的 `literal` 只在頁面文字裡有那串字時才給；`tag` 只取 contract 的元素清單；`too_large` 的 `max` 是 contract 的上限，不是判定寫的數字；`place`、`channel`、`phase`、`limit`、`content` 是 contract 的封閉值。
- 對應後相同的問題只留一個。工具對每個種類與欄位逐一處理、不用萬用分支，contract 新增的種類或欄位要先在這裡處理才編譯得過。

**問題**：沒有存下時，工具結果是錯誤結果 `{saved: false, problems, note}`，最多 20 個問題（多的只給數量），每個是封閉的 `kind`、固定的英文 `hint` 與該類的欄位；沒有服務或資料庫的文字。使用者看到的仍是上一版。

| `kind` | 什麼時候 |
| --- | --- |
| `invalid_spec`（帶 `path` 與 contract 的 `problem`）、`unknown_name`、`page_edit_not_found`、`page_edit_ambiguous`、`incomplete`、`unstorable_text` | 驗證不過；什麼都不執行 |
| `dataset_refused`（SQL 的拒絕類別用 `telemetry_sql` 的說明）、`dataset_too_many_rows`、`dataset_result_too_large`、`dataset_timeout`、`query_too_heavy`、`too_many_points`、`outside_queryable_period`、`account_not_ready`、`runs_limited`、`query_quota_exhausted`、`dataset_failed` | 資料集執行失敗（帶 `dataset`）；不送檢查 |
| contract 的檢查問題（`not_rendered`、`navigated`、`requested`、`script_error`、`unmapped`、`mismatch`、`literal_number`、`mark_geometry`、`timeout`、`too_large`、`unsupported_content`、`unrecognized`） | 判定不通過；欄位見上方「模型從判定得知的」 |
| `check_busy`、`check_unreachable`、`check_timeout`、`check_refused`、`check_failed`、`check_invalid_verdict` | 沒有可用的判定：這個租戶的另一次寫入、這個副本的記憶體或檢查名額等不到、503、連不上、85 秒內沒有判定、400（或請求本身不合 contract）、其他狀態、不是判定、值多於 `MAX_VERDICT_VALUES` 或判定不符送出的資料 |

**上限**：一個問題最多 6 次到達資料集執行的寫入（驗證不過的不算），第 7 次在執行任何東西前拒絕。每一段有自己的時限，到了就以模型能處理的結果結束：等這個租戶的另一次寫入與重用結果的記憶體合計 30 秒、資料集 120 秒（含讀取，不含前面的等待）、值與檢查的記憶體合計 30 秒、檢查名額 30 秒加檢查 85 秒、存檔 30 秒（逾時時交易回滾、回固定的失敗；存檔的每個陳述式等列鎖最多 2 秒，與 API 的請求相同（`api::REQUEST_LOCK_TIMEOUT`），等不到時也回同樣的失敗，連線立即放回連線池）；一次呼叫的時限是這些加 10 秒，335 秒（`Tool::call_timeout`，其他工具仍是 `tools.call_timeout`），只在各段沒依時限結束時才用到。**每個副本**計的上限：每個租戶同時一次寫入、2 次檢查、所有租戶共用的記憶體預算（見「記憶體」）；N 個副本時一個租戶同時最多 N 次檢查。不另設每租戶每分鐘的檢查次數：每次檢查都用掉問題 6 次寫入之一（每次寫入是一次模型呼叫，計入 AI 額度與預算），一個租戶同時最多佔一個名額、名額依先後給，而每分鐘的上限會在檢查服務閒置時也拒絕。仍在提問的截止時間內，問題剩不到 30 秒時不開始寫入；截止時間（委派到期前 30 秒）到時，進行中的呼叫連同它還沒送出的請求一起取消（`Agent::ask` 在截止時間丟下整個提問，這一步的呼叫跟著取消）。所以設定了 `dashboard_check.url` 時 `identity.run_lifetime` 至少要 365 秒（`config::MIN_RUN_LIFETIME_WITH_DASHBOARDS`：一次呼叫的 335 秒加這 30 秒），較短的委派會讓每次寫入都被截止時間切斷。`read` 的 spec 與頁面完整給模型，所以設定了 `dashboard_check.url` 時 `tools.max_text_bytes` 至少要 64 KiB。工具的拒絕（`ToolError::Input`）是固定說明：不存在的儀表板、舊的 `base_version`（說最新是第幾版）、沒給 `base_version`、封存了、已有草稿、第 7 次寫入、時間不夠。

**哪些對話有它**：工具組在對話開始時固定（`Preamble`），所以只有新對話有 `dashboard` 工具，已開始的對話照原本的五個工具繼續。system prompt 的版本 `2026-10-26` 加上做儀表板的方式（相對期間、每個數字都經標記、選定的參考值與帶數字的名稱放常數、跨資料集的值放計算）與回答不放 SQL、HTML 或程式碼；進行中的對話保留開始時的版本。

**沒有設定時**：需要 `identity.exchange_url`、`telemetry_query.url`、`dashboard_check.url` 與資料庫；少一個就沒有這個工具，啟動時記一行 INFO（例如 `no dashboard tool: dashboard_check.url is not set`）。

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
- **軟刪除**：`store::delete` 記下刪除時間，回傳標題、刪除時間與可還原的期限（刪除後 30 天，`store::DELETED_RETENTION_DAYS`）。之後除了 `store::list_deleted`（擁有者的「已刪除」清單，只有標題與時間，沒有訊息）與 `store::restore`，所有函式都當它不存在，擁有者也一樣：讀不到、改不了名、不能封存、不能捨棄、不能開始提問，`store::append` 回 `StoreError::NotStored`、不再寫入任何訊息。刪除與正在進行的 `append` 依對話那一列的鎖排序：不是訊息先寫完再刪除，就是 `append` 因已刪除而失敗，不會刪除後還多出訊息。有進行中、截止時間未過的提問時，刪除回 `StoreError::AlreadyRunning`、什麼都不改：`delete` 先鎖住對話那一列（與 `start_run` 同一把鎖），再以新的查詢檢查提問，所以同時發生的刪除與開始提問只有一個結果，不是提問先開始而刪除被拒，就是先刪除而提問因已刪除回 `StoreError::NotStored`。截止時間已過的提問（服務在提問中停掉）不擋刪除；截止時間與 `statement_timestamp()`（等到列鎖之後的時間）比較，不用交易開始的 `now()`，所以等鎖時才過了截止時間的提問也不擋。刪除不改提問紀錄：那樣的提問維持 `running`，還原後下一次 `start_run` 把它改成 `interrupted`，清除時隨對話刪掉。
- **還原**：`store::restore` 只在刪除後 30 天內有效，對話原樣回來：刪除不動訊息、捨棄紀錄與提問紀錄，還原後歷史逐位元組相同。滿 30 天的對話在擁有者的已刪除清單消失、還原回 `StoreError::NotStored`，資料庫也拒絕服務角色對它的任何更動（`expired_stay_deleted`），程式有錯也救不回來。
- **清除**：`store::purge_expired(PlatformTransaction, 上限, 略過的)` 以平台角色 `assistant_platform`（service-kit 的 `PlatformDb`，每個交易記一行 `platform database access`）刪掉最多「上限」個刪除超過 30 天的對話，從最舊的開始，連同訊息、捨棄紀錄與提問紀錄；回傳刪了幾個，少於上限時表示清完了。restrictive policy `platform_expired_only` 讓平台角色不論交易設了什麼租戶，都只看得到、刪得掉刪除滿 30 天的對話（0001 的 `tenant_isolation` 沒有指定角色，也套用到平台角色，沒有這條 policy 時，設了租戶的平台交易看得到、刪得掉那個租戶的每個對話），所以清除即使寫錯也刪不到還在 30 天內或沒刪除的對話；它對訊息與其他表沒有任何權限。還原與清除同時發生在同一個對話時只有一個結果：先拿到列鎖的一方贏，另一方等它 commit 後重新檢查條件，什麼都不做。清除等列鎖最多 `store::PURGE_LOCK_TIMEOUT`（5 秒，交易內的 `lock_timeout`），等不到時以 `55P03`（`store::LOCK_NOT_AVAILABLE`）失敗、這一批什麼都不刪：清除不該無限期卡住。平台角色沒有 UPDATE 權限，不能用 `FOR UPDATE SKIP LOCKED` 跳過被鎖的列（資料庫回 `42501`，測試裡有），所以一筆被鎖住的過期對話不能讓其他的也刪不掉：`store::expired_conversations` 讀出這一批的鍵（不鎖任何列；整張表被鎖住時也只等 `PURGE_LOCK_TIMEOUT`），`store::purge_one` 一次刪一個，每個一個交易、等鎖最多 `store::PURGE_ROW_LOCK_TIMEOUT`（1 秒），等不到的就略過；`purge_expired` 與 `expired_conversations` 的「略過的」參數讓之後的批次不再選到它們（`store::ExpiredConversation` 的租戶與 ID，以兩個 `uuid` 陣列傳入）。被略過的對話留到下一輪再刪。選平台角色而不是在每個租戶的交易裡以服務角色刪除：服務角色看不到哪些租戶有過期的對話，而且那樣服務角色就得有 DELETE。代價是多一個資料庫角色與帳密，以及它的連線池；誰在什麼時候呼叫見下面的[排程清除](#排程清除)。
- **捨棄最後一輪**（重新產生回答）：`store::discard_last_round(tx, principal, id, from, stored)`，`from` 是最後一輪的問題在 `store::load` 回傳的歷史中的位置，`stored` 是 `load` 或 `append` 回傳的值，歷史在那之後不能變過（否則 `StoreError::Conflict`）。只能捨棄最後一輪：`from` 不是問題、後面還有問題或超出範圍時回 `StoreError::NotLastRound`。訊息留在資料庫，只在 `discarded_rounds` 記下範圍；`load` 略過它們，所以送給模型的歷史與那一輪之前逐位元組相同。下一輪寫在被捨棄的訊息之後（位置繼續往後數，所以位置不再等於歷史中的順序）。捨棄後要重新 `load`：手上的 `stored` 與記憶體裡的對話還含有被捨棄的一輪，`append` 會拒絕。
- **提問紀錄**：`store::start_run` 鎖住對話那一列，先把截止時間已過、還在 `running` 的提問改成 `interrupted`（服務在提問中重啟），還有進行中的提問時回 `StoreError::AlreadyRunning`（`delete` 也一樣，見上面的軟刪除）；唯一索引 `runs_one_running_per_conversation` 在資料庫守同一條規則，所以同時開始的兩個提問只有一個成功。截止時間是 run 委派的截止時間，最多一小時後。`store::finish_run` 記下結果（`answered`、`failed` 加原因代碼、`cancelled`），對話在提問中被刪除時也照樣能結束；已結束的回 `StoreError::RunEnded`。`store::request_cancel` 記下第一次要求停止的時間，`store::run_status` 查詢。失敗、取消或提問中被刪除的那一輪不寫進歷史（呼叫端的規則，Laysi/trustiot-v2#646），只在這裡留下結果。
- **標題與清單**：`store::set_title` 的來源有三種：`User`（使用者改名）一律取代；`Model`（模型產生）只取代空的或暫定的標題；`Provisional`（取自第一個問題）只填空的標題。標題的規則只有一份（`store::title_is_valid`），API 與 `set_title` 都用它：1 到 200 個字元；不含控制字元（Cc）、雙向文字的嵌入、覆寫與隔離控制字元（U+202A–U+202E、U+2066–U+2069）、已停用的格式字元（U+206A–U+206F）與分行、分段字元（U+2028、U+2029），它們會改變標題與後面文字顯示的順序或斷行；至少一個看得見的字元，不能只有空白或不顯示的字元（Unicode 的 Default_Ignorable_Code_Point，例如零寬字元、軟連字號、BOM、變體選擇字元，加上其他格式字元 Cf 與點字空白 U+2800）。零寬連接字元（ZWJ、ZWNJ）與方向標記（LRM、RLM、ALM）在有看得見的字元時可以用：emoji 序列與波斯文需要它們。資料庫的 CHECK 只管長度（`char_length(title) BETWEEN 1 AND 200`），沒有改。`store::archive` 封存或取消封存，封存的對話仍可使用。`store::list` 依最後活動時間（寫入訊息的時間）由新到舊，只列沒封存的或只列封存的（`store::ListFilter`），每頁最多 `store::MAX_PAGE`（100）個，以回傳的 `next` 接著取下一頁；`store::metadata` 取單一對話。
- **開始對話的上限**：每個使用者在每個租戶，以資料庫的時鐘算 24 小時內（`store::START_WINDOW_HOURS`）最多開始 `store::MAX_STARTS_PER_DAY`（100）個對話；再開始時 `store::create` 回 `StoreError::StartLimited`（帶最舊的那個離開 24 小時還要幾秒）、什麼都不存。這是產品規則，不是設定：開始對話就存一列，裡面有整段開頭（system prompt 與每個工具的說明與 schema，幾 KB 到幾十 KB），平台目前也沒有 HTTP 的流量限制，沒有上限時一個帳號跑迴圈就能塞滿所有服務共用的 Postgres。100 個遠超過一個人手動開始的數量。計算的是這個使用者在這個租戶存著的每個對話的 `created_at`（資料庫填的，服務角色沒有那一欄的 INSERT 權限），已刪除的也算（清除前都還存著），所以邊開邊刪逃不過上限；已經存了的同一個對話再存一次（重試）不算新的。計數與寫入在同一個交易，之前先拿這個使用者在這個租戶的 advisory lock（`pg_advisory_xact_lock` 的兩個 `int4` 形式：第一個鍵固定為 `1634956148`，ASCII 的 `asst`，第二個是租戶與使用者的雜湊），交易結束才放開，所以同一個使用者同時開始的對話依序計數，上限是精確的（`tests/api.rs` 讓 24 個請求搶最後 5 個名額）。Postgres 文件說一個 `bigint` 與兩個 `int4` 的鍵空間不重疊，所以這把鎖碰不到 service-kit `JobLock`、身分服務、assets 與 outbox 的單鍵鎖，第一個鍵也與 certs 的兩鍵鎖（`1953983347`）不同；兩個使用者雜湊相同只是互相等一下。等這把鎖也受請求的 `lock_timeout` 限制。計數分成沒刪除的與已刪除的兩部分，各自走 0002 的部分索引 `conversations_by_activity` 與 `conversations_deleted`；查詢計畫以 `(tenant_id, user_id)` 為兩個索引的條件，只讀這個使用者在這個租戶的列，不碰別的使用者（測試資料庫上兩個租戶各四萬列、各兩百個使用者時確認過），再以 `created_at` 篩掉 24 小時前的，所以成本只隨這個使用者自己存著的對話數增加。沒有加新的索引。
- **什麼時候真正消失**：被捨棄的訊息與刪除的對話在清除之前都還在資料庫（與備份）裡；清除之後，資料仍留在清除前做的備份裡，直到那些備份的保存期限屆滿。刪除的對話在滿 30 天後的下一次清除時刪掉；沒刪除的對話，連同它捨棄的回合，保存多久由 Laysi/trustiot-v2#652 決定，在那之前一直保存。
- **錯誤不含資料**：Postgres 的錯誤訊息與 DETAIL 會帶欄位值（例如重複鍵的租戶與對話 ID），`StoreError` 只留 SQLSTATE 或失敗的類別（`DatabaseFailure`），`Display` 與 `Debug` 都可以直接寫進日誌或回應。
- 模型上一次回報的歷史大小（`Conversation::measured`）不儲存，載入後的第一次呼叫重新估計。

### 給聊天 API 的介面

| 誰 | 用哪些函式 |
| --- | --- |
| 對話串的 API（Laysi/trustiot-v2#645，`src/api/conversations.rs`） | 開始對話 `create`；讀取 `metadata`、`load`；對話串 `list`、`set_title`、`archive`、`delete`、`list_deleted`、`restore` |
| 提問（Laysi/trustiot-v2#646） | `load` 後 `start_run` 才呼叫模型，回答完成的一輪 `append`，結束時 `finish_run`；停止 `request_cancel`；重新產生回答 `discard_last_round` 後重新 `load` 再問 |
| 分享（Laysi/trustiot-v2#650） | `load` 只給擁有者；分享另開一條唯讀的路徑，同樣要把刪除的對話當作不存在 |
| 排程清除（Laysi/trustiot-v2#645，`src/purge.rs`） | `purge_expired`，在 `assistant_platform` 的 `PlatformTransaction` 裡 |

呼叫端要遵守：同一個請求的多個呼叫放在同一個 `TenantTransaction`，由呼叫端 commit；只有回答完成的一輪才 `append`；`discard_last_round` 之後重新 `load`；`StoreError::NotStored` 對使用者一律是「找不到」，不區分不存在、別人的或已刪除。

服務啟動時建立服務角色 `assistant_app` 與平台角色 `assistant_platform` 的連線池（verify-full 的 TLS，`database.*` 與平台設定，見「設定」），不在啟動時連線（Laysi/trustiot-v2#592）。平台角色的連線池固定 2 條（`PLATFORM_CONNECTIONS`：清除的交易與 readiness），只給清除用。readiness 有三項：`postgres` 以兩個角色各開一條通過 service-kit session 檢查的連線，migration 檢查（`MigrationCheck` 與 `schema::SCHEMA`）確認資料庫有這一版預期的每個 migration 檔案，`internal_identity_keys` 確認已讀到可用的身分服務公鑰（API 以它驗證每個請求）；任一項不過時 `/readyz` 回 503，日誌寫出原因（[service-kit 的 migration 檢查](../../crates/service-kit/README.md#migration-檢查migrations)）。readiness 不呼叫 AI 閘道或其他服務。chart 在 `mesh.calls.assistant` 列了 `postgres` 時給它兩個角色的帳密與服務角色連線池的大小（`services.assistant.database.maxConnections`；平台角色的 2 條另算，都算進 chart 的連線預算，見 chart README 的「連線預算」，Laysi/trustiot-v2#575），migration Job 讓 `assistant_app` 與 `assistant_platform` 都能登入。API 的每個請求在這個連線池開一個請求租戶的交易，呼叫上面「給聊天 API 的介面」列的函式；儀表板的請求與 `dashboard` 工具的寫入也在這個連線池開短交易（見「儀表板」）。

### 排程清除

`src/purge.rs`（Laysi/trustiot-v2#645）每 `purge.interval`（預設 1 小時）跑一輪清除，服務啟動後馬上跑第一輪。每個副本都排程，同一時間只有一個副本執行：一輪在 service-kit 的 `JobLock`（Postgres advisory lock，job 名稱 `purge`，在服務角色的連線池上，service-kit 的 job lock 都用服務角色）底下執行，拿不到鎖的副本跳過這一輪（DEBUG `scheduled job skipped; another replica holds its lock`），不開平台交易。

一輪以 `purge::PURGE_BATCH`（100）個對話為一批呼叫 `store::purge_expired`，每批一個 `assistant_platform` 的交易（service-kit 每個交易記一行 `platform database access`，`purpose` 是 `purge expired conversations`），直到一批少於 100 個；一批連同訊息最多上千列，交易短、鎖得少，中途失敗時之前的批次已經刪掉。

**被鎖住的列**：一批等列鎖超過 5 秒（`55P03`，見上面的清除）時，這一批什麼都不刪；這一輪接著把這一批的對話一次刪一個（每個一個交易，等鎖最多 1 秒），還鎖著的略過、記下，之後的批次不再選它們，其他過期的對話照樣刪掉。所以一筆一直被鎖住的列不會讓清除停擺，只是它自己留到某一輪發現它沒被鎖時才刪，通常是下一輪。每一筆一直被鎖住的列讓一輪多等最多 6 秒（批次的 5 秒加上單筆的 1 秒）。誰會鎖住過期的列：服務角色連鎖定都做不到（`expired_stay_deleted`），只有剛好在滿 30 天前開始的還原（毫秒內 commit）、或卡住的維運工作階段；清除連帶刪除的草稿列例外：指名某個草稿的請求（發布、還原、封存或工具的寫入）在檢查可見性之前先對那一列取 `FOR UPDATE`，所以服務角色可能短暫鎖住過期對話的草稿列，這時由批次的 5 秒上限與一次刪一個的做法處理。

**整張表被鎖住**（`LOCK TABLE`、不是 `CONCURRENTLY` 的 `CREATE INDEX`、`ALTER TABLE`、`VACUUM FULL`，或刪除連帶刪到的 `messages` 被這樣鎖住）時，這一輪提早結束，下一輪再試：一次刪一個也一個都沒刪掉時（每個都還鎖著），或讀這一批的鍵等鎖超過 5 秒時（ACCESS EXCLUSIVE 連讀取都擋），這一輪就停，不是錯誤。一批的每個對話都還鎖著，是整張表被鎖住的跡象，不是個別卡住的列：服務角色鎖不住過期的列，要一個一個鎖住一整批得是維運的人刻意為之。不停的話，這一輪會以每個對話約 1 秒的速度走完整個過期的積壓、什麼都刪不掉，而且走過的每一個都留到下一輪，即使鎖馬上就放開。所以整張表被鎖住時，一輪最多等 5 秒加上一批 100 個各 1 秒，然後停。略過清單只在一次刪一個有刪掉東西時才會變長，而且只多出被個別鎖住的對話，所以不另設上限。死結（`40P01`）或其他錯誤讓這一輪以錯誤結束（`JobLock` 的 WARN），下一個間隔再試。

每一輪結束時記一行，欄位 `job`、`purged`（這一輪刪了幾個，沒有也記 0）與 `skipped`（被鎖住、留到下一輪的個數）：都沒略過時是 INFO `purged expired conversations`，有略過時是 WARN `purged expired conversations; some stayed locked and wait for the next round`，被鎖提早結束時是 WARN `purge round stopped early: a lock held up its deletions; the next round tries again`（`purge::STOPPED_EARLY`）；不記標題、ID、租戶或內容。一次刪一個的每個交易也各有一行 service-kit 的 `platform database access`。無法登入或其他資料庫錯誤由 `JobLock` 記 WARN `scheduled job failed`，錯誤只有類別。

排程只決定滿 30 天的對話多久後真的離開資料庫：能不能還原由 `deleted_at` 與資料庫的 policy 決定，與排程無關。所以刪除的對話在滿 30 天後最多再過一個 `purge.interval`（加上那一輪本身的時間）被刪掉；所有副本都停著時不會清除，重新啟動後的第一輪補上。

## API

路徑都在 `/api/assistant/v1` 底下；入口的路由是 Laysi/trustiot-v2#647。型別與每個狀態碼在 `api/openapi.json`（tag `assistant`，`assistant::openapi()`），前端的 client 由它產生；儀表板的本文與回應型別在 `contract::api::dashboard::public`。對話（`src/api/conversations.rs`）與儀表板（`src/api/dashboards.rs`）的路由合併在 `api::routes`，兩者都照下面的規則，用 `api` 的共用函式（身分、錯誤回應、交易）。

- **身分**：請求要有身分服務簽的內部身分 token（`x-internal-identity`，入口的 ext_authz 由 session cookie 換來；決策 27），受眾是 `assistant`；服務以身分服務的公鑰檔（`TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`）驗證，檔案換了會重新讀（[service-kit 的公鑰與輪替](../../crates/service-kit/README.md#公鑰與輪替)），沒有可用的公鑰時 readiness 不過。使用者、租戶與角色每次都取自 token，不取自本文或存下的副本。
- **角色**：每個路由先以 service-kit 的共用檢查（`UserIdentity::require`）要求它在權限矩陣的角色，在讀 body 與資料庫之前：發布、封存與取消封存儀表板要租戶 editor，其他都是租戶 viewer（[docs/architecture/permissions.md](../../docs/architecture/permissions.md) 的「assistant」，由 `tests/permissions.rs` 對照）。token 沒有租戶、角色是這一版不認得的或低於要求的，回 403 `permission_denied`，日誌記 INFO `refused to <動作>`；這個 403 只看 token，不透露任何對話或儀表板，不存在的 ID 也是同一個 403。之後誰看得到、誰能改什麼由 store 決定（見下方各小節的「誰看得到」）。
- **看不到等於不存在**：不存在的、看不到的（別人的、別的租戶的）與路徑的 ID 不是 UUID 的，回應逐位元組相同：404 `not_found`（`api::not_found`），所以回應不透露對話或儀表板存不存在。
- **不快取**：每個回應都帶 `cache-control: no-store`，包括 401 與 403：對話與草稿是個人的資料，service-kit 不設快取標頭。
- **寫入要 JSON**：有本文的寫入（對話的 `POST` 與 `PATCH`，儀表板的 `POST`）要 `Content-Type: application/json`，少了它或不對都回 400 `invalid_request`（防 CSRF：其他網域的頁面不經 CORS preflight 送不出這個型別，而沒有東西回應 preflight；[身分服務的規則](../identity/README.md#租戶)）。本文最多 4 KiB，要正好是該路由的 JSON 物件，不認得的欄位也回 400。`DELETE` 與封存的 `PUT` 沒有本文（跨 origin 的 `DELETE` 與 `PUT` 本來就要 preflight）。
- **鎖的等待**：每個請求的交易以 `api::begin` 開始，設 `lock_timeout` 2 秒（`api::REQUEST_LOCK_TIMEOUT`）。其他請求只把對話或儀表板那一列鎖住一個短交易（毫秒），所以同一列的兩個寫入排隊，後者再依目前的狀態判斷；等超過 2 秒表示有卡住的工作階段，請求回 503 `unavailable`、什麼都沒改，把連線還給連線池，不陪它等，日誌記錯誤的類別（SQLSTATE `55P03`）。
- **資料庫失敗**：503 `unavailable`，什麼都沒寫；日誌只記錯誤的類別（SQLSTATE 或錯誤的種類），不記 Postgres 的訊息、SQL、標題或內容。

### 對話

| 方法與路徑 | 做什麼 | 成功時 |
| --- | --- | --- |
| `GET /conversations` | 自己的對話，依最後活動時間由新到舊；預設只列沒封存的，`archived=true` 只列封存的 | 200 `ConversationList` |
| `POST /conversations` | 開始一個空的對話；送 `Content-Type: application/json`，本文是空的、`null` 或 `{}`；每個使用者在每個租戶 24 小時內最多 100 個 | 201 `Conversation`，`Location` 指向它 |
| `GET /conversations/{id}` | 對話與聊天顯示的訊息 | 200 `ConversationDetail` |
| `PATCH /conversations/{id}` | 改名（`title`）、封存或取消封存（`archived`），至少一項，在同一個交易 | 200 `Conversation` |
| `DELETE /conversations/{id}` | 刪除，30 天內可以還原（期限見已刪除清單的 `restorable_until`） | 204，沒有本文 |
| `POST /conversations/{id}/restore` | 還原 30 天內刪除的對話；送 `Content-Type: application/json`，本文是空的、`null` 或 `{}` | 200 `Conversation` |
| `GET /deleted-conversations` | 還能還原的已刪除對話，依刪除時間由新到舊，只有標題與時間 | 200 `DeletedConversationList` |

- **誰看得到**：對話只屬於開始它的使用者。每個請求以 token 的使用者在目前的租戶行事，租戶的每個角色（viewer、editor、admin）都一樣，只能用自己的對話，租戶管理者也看不到別人的對話；檢查的是 store（租戶加使用者）。已刪除的對話除了還原之外也是 404。
- **讀取只給問答**：`GET /conversations/{id}` 的 `messages` 只有問題與回答（`EntryKind::shown`），依時間順序；回答只取 `text` 區塊（`NativeMessage::shown_text`）。thinking、工具呼叫與結果、對話中的指示，以及開頭固定的 system prompt、租戶脈絡、工具定義都不回。捨棄的回合不在裡面。
- **開始對話**：`Agent::start_conversation` 固定這一版的 system prompt、工具組與模型設定，`store::create` 存在請求租戶的交易裡。租戶脈絡（租戶的設備、資料點、方案、語言、時區）目前沒有來源，傳空字串；它在對話開始時固定，所以現在開始的對話會一直沒有租戶脈絡。每個使用者在每個租戶 24 小時內最多開始 100 個對話，已刪除的也算（見「規則」的開始對話的上限）；超過時回 429，`Retry-After` 是最舊的那個離開 24 小時還要幾秒，什麼都不存，日誌記一行 WARN。
- **改名與封存**：`title` 去掉前後的空白後要符合標題的規則（見「規則」的標題與清單：1 到 200 個字元，至少一個看得見的字元，不含控制、雙向格式與分行字元），標題來源記為 `user`，之後模型產生的標題不會取代它。封存的對話照常可以讀、可以繼續。
- **分頁**：照身分服務操作紀錄的慣例：`limit`（1 到 100，預設 50）、`cursor`（上一頁的 `next_cursor`），回應 `{"conversations": [...], "next_cursor": ...}`，最後一頁的 `next_cursor` 是 `null`。順序是（時間、ID）由新到舊，時間相同時看 ID，所以翻頁不重複、不遺漏。`next_cursor` 對客戶端是不透明的，內容是清單的種類、資料庫的時間（微秒，Postgres 的精度）與 ID；沒封存的、封存的與已刪除的三個清單的 cursor 只能用在自己的清單。cursor 不簽章：store 只看呼叫者自己的對話，改過的 cursor 只會移動自己清單的起點。
- **刪除與還原**：刪除只記下刪除時間（`store::delete`），30 天內 `POST .../restore` 原樣還原，之後清除會刪掉它。有提問進行中、截止時間未過時，刪除回 409，什麼都不改；刪除先鎖住對話那一列才檢查提問，所以與開始提問同時發生時只有一個結果（見「規則」的軟刪除）。
- **沒有欄位的寫入**：開始對話與還原沒有欄位，也要送 `Content-Type: application/json`，本文是空的、`null` 或 `{}`。

### 儀表板

儀表板是什麼、存在哪裡見下方「儀表板」。API 沒有寫內容的路由：只有 `dashboard` 工具寫版本的內容，沒有任何請求本文有 SQL、語意查詢或頁面的欄位，瀏覽器寫不了會被執行的東西。

| 請求 | 誰 | 成功 |
| --- | --- | --- |
| `GET /dashboards`（`?include_archived=true`、`?cursor=`） | viewer 以上 | 200 `DashboardList`：已儲存的儀表板，最近更新的在前，一頁 50 個 |
| `GET /conversations/{conversation_id}/dashboards`（`?cursor=`） | 對話的擁有者 | 200 `DashboardList`：對話的草稿 |
| `GET /dashboards/{dashboard_id}`（`?version=`，沒有時是最新版） | 看得到它的人 | 200 `DashboardView`：儀表板、那一版的內容、呼叫者可以做的事（`permissions`） |
| `GET /dashboards/{dashboard_id}/versions`（`?cursor=`） | 看得到它的人 | 200 `VersionList`：新的在前，一頁 50 個 |
| `POST /dashboards/{dashboard_id}/publish`（草稿） | editor 以上，而且是草稿所屬對話的擁有者 | 201 `VersionCreated`：已儲存儀表板的新版本（第一次發布建立已儲存的儀表板） |
| `POST /dashboards/{dashboard_id}/restore` | 已儲存的儀表板：editor 以上；草稿：對話的擁有者，任何角色 | 201 `VersionCreated`：加一個新的版本，內容是舊版本的（新的一列指向存著那份內容的列，不複製內容） |
| `PUT`、`DELETE /dashboards/{dashboard_id}/archive`（已儲存的儀表板） | editor 以上 | 200 `ArchiveState`：封存、取消封存 |
| `POST /dashboards/{dashboard_id}/dataset-runs` | 看得到它的人，以呼叫者的身分執行 | 200 `DatasetResult`：一個資料集的表格（見「資料集的執行」） |

- **誰看得到**：已儲存的儀表板是租戶的每個成員；草稿與對話的草稿列表只有對話的擁有者，同租戶的其他使用者看到的與不存在的相同。對話被刪除後，它的草稿與草稿列表在每個路由都是同一個 404，對話還原後回來；從草稿發布的已儲存儀表板不受影響（見「儲存」的誰看得到）。路由的角色之外，誰能發布、還原、封存由 store 再判斷（`store::permissions`，見「儲存」的誰能改）：store 以 `UserIdentity::meets` 再檢查一次發布與封存的 editor，還原已儲存的儀表板要 editor，由 store 看過種類之後檢查（403 `dashboard.editor_required`）。
- **嚴格的請求**：`Content-Type` 只能有一個，大小寫不拘，可以有參數；重複的欄位、陣列寫法與多餘的值也回 400 `invalid_request`。query string 有未知參數、值不對或 `cursor` 不是本服務寫的，也是 400。
- 寫入（發布、還原、封存）各記一行 INFO（`published a dashboard`、`restored a dashboard version`、`changed whether a dashboard is archived`），只有使用者、租戶、儀表板與版本，不記標題、spec 或頁面。
- **發布與還原的次數**（`dashboards::limits::VersionBudget`）：版本只往後加、不會刪除，所以每個使用者在任何一分鐘最多以發布與還原加 10 個版本；同一個租戶的使用者合計最多加 30 個到已儲存的儀表板，另外最多 30 個到草稿。超過時回 429 `rate_limited`、`dashboard.versions_limited`，帶 `Retry-After`（整數秒，至少 1，等到使用者與租戶都有空位）與 `details.retry_after`，記一行 INFO（`refused a dashboard version: ...`，只有使用者、租戶、動作與秒數），什麼都不寫。store 在交易裡做完其他每一項檢查、正要寫入時才計數（`admit`），所以因為身分被拒（viewer 發布、別人的草稿）或因為儀表板的狀態被拒（衝突、封存、沒有改變、版本數到上限）的請求都不算，用不掉別人共用的上限；被這個上限拒絕的也不算。已計數而交易之後失敗的仍算。草稿與已儲存的儀表板分開計，所以只能改自己草稿的 viewer 用不掉 editor 發布與還原的上限。上限以**每個副本**計：N 個副本時一個使用者每分鐘最多 N × 10 個版本。
- **發布的衝突**：發布草稿的一版時，比較的是那一版自己的基礎版本（寫入時記在版本上，之後不變），不是草稿目前的：已儲存的儀表板的最新版不是它就回 409 `conflict`、`dashboard.saved_changed`。所以草稿合併了新版本之後，合併前的舊版本仍不能發布；還原草稿的舊版本時，新的一版帶著舊版本的基礎版本，草稿接下來的版本也從那裡開始（草稿的 `saved.version` 跟著降）。發布草稿的最新版之後，草稿接下來的版本以發布出去的那一版為基礎，已發布的那一版本身不再發布一次；發布較舊的版本時草稿的基礎版本不變。草稿第一次發布之前寫的版本不以任何已儲存的版本為基礎：已儲存的儀表板建立之後，它們（包括發布出去的那一版）與從它們還原的版本都不能再發布；第一次發布的不是最新版時，草稿接下來的版本也不以任何版本為基礎（`saved` 沒有 `version`）。這時要請 AI 助理把已儲存儀表板的最新版併入草稿，再發布新的一版。
- **回到相同的內容**：還原的版本與最新版的內容（runtime、標題、說明、spec、頁面）逐位元組相同時回 409 `conflict`、`dashboard.restore_unchanged`，不加版本；與其他「和目前狀態衝突」的情況一樣是 409。
- **版本數各自計算**：版本號與版本數以儀表板計（主鍵是租戶、儀表板、版本號）。草稿是自己的一個儀表板，草稿的還原只加草稿的版本；已儲存的儀表板只由 editor 以上的發布與還原加版本，viewer 對自己草稿做的事用不掉它的版本。
- **10,000 個版本的上限**：一個儀表板最多 10,000 個版本（資料表的 CHECK 與 `store::MAX_VERSIONS`），到了之後發布與還原都回 409 `conflict`、`dashboard.too_many_versions`，什麼都不寫；已有的版本照常讀、照常執行資料集。editor 可以請 AI 助理以它的最新版本在對話裡做一個新的儀表板（新草稿），發布成新的已儲存儀表板，再封存舊的。以每個使用者每分鐘 10 次計，一個副本時一個人要連續 16 小時以上才到得了上限。

### 錯誤

錯誤都是 M1 本文。對話的 `message_key` 在 `api::conversations`，儀表板的在 `contract::api::dashboard::public`；zh-TW 與 en 的翻譯在 `frontend/src/i18n/locales/`。

| 路由 | 情況 | 回應 |
| --- | --- | --- |
| 全部 | 沒有有效的內部身分 token（沒有、簽章不對、受眾不是 `assistant`、過期） | 401 `unauthenticated` |
| 全部 | token 沒有租戶、在租戶的角色是這一版不認得的，或低於路由要求的（發布、封存、取消封存儀表板要 editor）；只看 token，在讀 body 與資料庫之前 | 403 `permission_denied` |
| 全部 | 不存在或看不到的對話或儀表板（別人的、別的租戶的），路徑的 ID 不是 UUID | 404 `not_found`，逐位元組相同 |
| 全部 | 有本文的寫入，`Content-Type` 不是 `application/json` | 400 `invalid_request` |
| 全部 | 等另一個工作階段鎖住的列（或其他鎖）超過 2 秒（`api::REQUEST_LOCK_TIMEOUT`） | 503 `unavailable`，什麼都沒改 |
| 全部 | 資料庫失敗 | 503 `unavailable`，什麼都沒寫；日誌只記錯誤的類別 |
| 對話 | 已刪除的對話（還原除外） | 404 `not_found`，與不存在的相同 |
| 對話 | 本文不是只有該端點欄位的 JSON 物件（開始與還原沒有欄位），或超過 4096 bytes | 400 `invalid_request`，`conversation.body_invalid` |
| 對話 | 更新沒有 `title` 也沒有 `archived` | 400 `invalid_request`，`conversation.update_empty` |
| 對話 | 標題不符「改名與封存」的規則 | 422 `validation_failed`，欄位 `/title`，`conversation.title_invalid`，`{"max": 200}` |
| 對話 | 清單的 `archived` 不是 `true` 或 `false` | 400 `invalid_request` |
| 對話 | `limit` 不是 1 到 100 | 400 `invalid_request`，`conversation.limit_invalid`，`{"max": 100}` |
| 對話 | `cursor` 不是同一個清單的 `next_cursor` | 400 `invalid_request`，`conversation.cursor_invalid` |
| 對話 | 刪除時有提問進行中 | 409 `conflict`，`conversation.question_running`；什麼都沒改 |
| 對話 | 24 小時內已開始 100 個對話（`store::MAX_STARTS_PER_DAY`，已刪除的也算）時再開始 | 429 `rate_limited`，`conversation.start_limited`，`{"max": 100}`，`Retry-After`（秒）；什麼都沒存 |
| 儀表板 | 本文超過 4 KiB 或不是正好那個請求，query string 有未知參數、值不對或 `cursor` 不是本服務寫的 | 400 `invalid_request` |
| 儀表板 | viewer 還原已儲存的儀表板 | 403 `permission_denied`、`dashboard.editor_required` |
| 儀表板 | 不是對話的擁有者卻發布或還原草稿（看得到草稿的只有擁有者，所以目前只在 Laysi/trustiot-v2#650 之後出現） | 403 `permission_denied`、`dashboard.owner_required` |
| 儀表板 | 沒有這一版 | 404 `not_found`、`dashboard.version_not_found` |
| 儀表板 | 還原時 `base_version` 不是最新版 | 409 `conflict`、`dashboard.version_conflict`，`details.latest` |
| 儀表板 | 發布時已儲存的儀表板在這個草稿版本的基礎版本之後有新版本 | 409 `conflict`、`dashboard.saved_changed`，`details.latest` |
| 儀表板 | 已封存的儀表板加版本（發布、還原） | 409 `conflict`、`dashboard.archived` |
| 儀表板 | 還原的版本與最新版的內容相同 | 409 `conflict`、`dashboard.restore_unchanged` |
| 儀表板 | 儀表板已有 10,000 個版本（發布、還原） | 409 `conflict`、`dashboard.too_many_versions` |
| 儀表板 | 同一個儀表板的兩個寫入同時進行 | 409 `conflict` |
| 儀表板 | 使用者在一分鐘內已以發布與還原加了 10 個版本，或租戶合計已加 30 個到已儲存的儀表板（草稿另計 30 個）（每個副本） | 429 `rate_limited`、`dashboard.versions_limited`，`Retry-After` 與 `details.retry_after`；只在其他檢查都通過時才計數 |
| 儀表板 | 發布已儲存的儀表板、封存草稿 | 422 `validation_failed`、`dashboard.draft_required`、`dashboard.saved_required` |
| 儀表板 | 資料集執行的錯誤 | 見「資料集的執行」 |

## 儀表板

儀表板（決策 33，Laysi/trustiot-v2#150；設計在 [docs/architecture/ai-assistant.md](../../docs/architecture/ai-assistant.md)「儀表板」）是一個頁面與它顯示的資料集，存成只往後加的版本。有兩種：

- **草稿**屬於一個對話：只有 `dashboard` 工具寫它的內容，只有對話的擁有者看得到，對話被清除時一起刪除。
- **已儲存的儀表板**屬於租戶：使用者把草稿發布進去，租戶的每個成員都看得到；它不屬於任何對話，只會封存與取消封存，不會刪除。

草稿記得它取自或發布到哪個已儲存的儀表板；草稿的每個版本記得它的內容以那個儀表板的哪一版為基礎（見「API」的儀表板的「發布的衝突」）。使用者以 API 讀、發布、還原、封存儀表板與執行它的資料集（見「API」的儀表板），內容只由 `dashboard` 工具寫。

### 儲存

`migrations/postgres/0003_dashboards.sql`，與對話同一個 schema，由 migration Job 套用。兩張表都是租戶資料，依決策 29 啟用 RLS（只隔開租戶；租戶內誰看得到什麼由服務決定，見下方）：

| 表 | 一列是 | 內容 |
| --- | --- | --- |
| `assistant.dashboards` | 一個儀表板 | 種類（`draft`、`saved`）、草稿的對話、草稿的已儲存儀表板與下一版從它的哪一版開始、建立者、建立時間、已儲存儀表板的封存時間 |
| `assistant.dashboard_versions` | 一個版本 | 版本號（從 1 起、沒有空洞，外鍵指向前一版）、來源（`agent`、`publish`、`restore`，還原時記下還原的版本與存著它內容的版本 `content_version`）、草稿版本的基礎版本（已儲存儀表板的哪一版；已儲存儀表板的版本與草稿第一次發布前的版本沒有）、內容：沙盒 runtime 版本、標題、說明、spec（服務寫的精簡 JSON 文字，讀回逐位元組相同）、頁面（還原的列沒有內容，見下方）；要求的使用者、工具寫入時的 run ID、建立時間 |

- **權限**：服務角色 `assistant_app` 對兩張表有 SELECT 與指定欄位的 INSERT（時間由資料庫的時鐘決定）；`assistant.dashboards` 另外只能 UPDATE 草稿的已儲存儀表板、下一版的基礎版本與封存時間。沒有 DELETE 與 TRUNCATE。觸發器對每個角色（包括 owner）再限制：草稿的已儲存儀表板只設一次、封存時間是資料庫的時鐘（再封存一次保留原本的時間）、其他欄位不變；版本寫入後不能改，包括它的基礎版本（`42501`，`tests/dashboards_store.rs`）。草稿的基礎版本可以降或變成沒有（還原舊版本時），發布比較的是版本上不會變的那個。沒有已儲存儀表板的草稿與已發布的版本（`publish`）都不能有基礎版本（CHECK）。草稿的建立者一定是對話的擁有者：草稿指向對話的外鍵含擁有者（`(tenant_id, conversation_id, created_by)` 指向對話的 `(tenant_id, conversation_id, user_id)`，對話表為此多一個唯一限制 `conversations_owner`），別人寫不進去（`23503`）。
- **刪除**：對話被清除時，它的草稿與草稿的版本以外鍵串聯刪除；已儲存的儀表板不屬於對話，不受影響。
- **還原不複製內容**：還原加的一列只有版本的資料（來源 `restore`、還原的版本、基礎版本、使用者、時間），沒有 runtime、標題、說明、spec 與頁面，`content_version` 指向存著那份內容的版本：還原的版本本身，或它也是還原時它指向的那一版，所以永遠只隔一層。每次讀版本（開啟、版本列表、列表的標題、資料集、發布、工具的 `read`）都從那一列取內容，結果與存著內容的版本逐位元組相同；`restored_from` 仍是使用者選的版本。資料表替每個角色守住：還原的列一定指向同一個儀表板裡、還原的版本的內容所在的那一列（外鍵 `(tenant_id, dashboard_id, restored_from, content_version)` 指向 `(tenant_id, dashboard_id, version, content_at)`，`content_at` 是每一列內容所在的版本），自己沒有任何內容；其他來源的列一定有完整的內容（CHECK）。
- **儲存只隨新內容增加**：內容只在兩種時候寫進資料庫：工具的寫入（每次都經過模型呼叫、資料集執行與檢查，計入 AI 額度）與發布（要草稿以已儲存儀表板的最新版為基礎）。還原只加一列沒有內容的列，所以在兩個版本之間來回還原也不增加內容的儲存量，只有每分鐘的版本數（上方「發布與還原的次數」）限制列數。一個版本的內容最多是 spec 與頁面合計 56 KiB（頁面最多 40 KiB）、標題 120 字、說明 500 字，約 60 KiB：工具寫入前由 `validate_version` 檢查，資料表的 CHECK 對每個角色再檢查一次。
- **一個操作一個交易**：`dashboards::store` 的每個函式是一個完整的操作，在呼叫者租戶的 `TenantTransaction` 裡單獨呼叫，成功才 commit。寫入先鎖儀表板的列（`FOR UPDATE`）再讀最新版本，所以同一個儀表板的寫入排隊，以舊版本為基礎的寫入被拒；發布先鎖草稿再鎖已儲存的儀表板，沒有反過來的順序。發布在資料庫裡複製版本的內容，所以複本與來源逐位元組相同。錯誤是 `StoreError`，不帶資料庫的文字或內容；日誌只記 `StoreError::log_kind`。
- **誰看得到**：已儲存的儀表板是租戶的每個成員；草稿只有對話的擁有者，而且只在對話沒有被刪除時。草稿的可見性只在一個地方決定：`store::conversation_access` 的一個查詢（`CONVERSATION_OWNER`，條件含 `deleted_at IS NULL`）。對話被刪除（軟刪除）後，它的草稿對每個函式都與不存在的相同，擁有者也一樣：開啟、草稿列表、版本列表、資料集、發布、還原、封存與工具的寫入都回 `StoreError::NotFound`，什麼都不改；對話還原後草稿原樣回來。已儲存儀表板的草稿（有 `saved_dashboard_id` 的）也跟著對話隱藏；從草稿發布的已儲存儀表板不屬於對話，一直看得到。Laysi/trustiot-v2#650（共享的讀者）在同一個函式加上讀者的存取。
- **寫入與刪除對話的順序**：寫草稿或從草稿寫版本的函式（工具的三種寫入、發布、還原）在自己的交易裡以 `FOR SHARE` 鎖住草稿的對話那一列（`ConversationLock::Share`），讀取不鎖。對話的刪除（`store::delete`）以 `FOR NO KEY UPDATE` 鎖同一列，所以兩者排隊：寫入先 commit 時，刪除等它，之後連新的版本一起隱藏；刪除先 commit 時，寫入等它，之後找不到對話（`NotFound`），什麼都不寫。刪除最多等一個這樣的交易：工具在資料集跑完、檢查回答之後才開交易，交易裡只有寫入與 commit，所以刪除不等資料集或檢查。有進行中的提問時刪除本來就回 409，所以這把鎖目前保護的是截止時間已過的提問，以及之後在提問之外寫草稿的呼叫端；任何呼叫端都不能在提問期間握著對話那一列的鎖，否則工具的寫入會一直等到工具的時限。清除只刪除刪除滿 30 天的對話，它們的列不符合 `deleted_at IS NULL`，`FOR SHARE` 不會去鎖，所以清除與寫入之間不會互相等待。
- **誰能改**：`store::permissions` 是唯一的規則，API 回的 `permissions` 與 store 的檢查都用它：草稿的擁有者可以還原（任何角色）、editor 以上的擁有者可以發布；已儲存的儀表板由 editor 以上還原與封存。

### 資料集的執行

`POST /dashboards/{dashboard_id}/dataset-runs`（本文 `{version, dataset}`）執行存下的那一版的一個資料集，`dashboards::runner::DatasetRunner` 做實際的呼叫（設計紀錄「資料集的執行」）：

1. 在請求租戶的短交易裡讀出那一版的資料集（看不到儀表板是 404，沒有這一版是 404 `dashboard.version_not_found`，沒有這個資料集是 422 `dashboard.dataset_unknown`），交易在呼叫 telemetry-query 之前結束，執行期間不佔資料庫連線。
2. 以請求的 token 向身分服務換一張受眾是 telemetry-query 的 token（`OnBehalfOf`，service-kit 的 `OnBehalf`，每次執行都換新的），所以 telemetry-query 以**打開儀表板的人**的租戶、角色與可查期間讀資料，不是作者的。沒有 `identity.exchange_url` 或 `telemetry_query.url` 時每次執行回 503 `dashboard.dataset_failed`。
3. SQL 資料集送到 `POST /internal/telemetry/v1/sql`，列數上限是資料集的 `max_rows`（最多 2,000）；統計與用量資料集把存下的請求本文原樣送到 `POST /api/telemetry/v1/statistics` 或 `/usage`，回答攤平成表格（`dashboards::flatten`）。
4. 結果檢查形狀與大小：超過 `max_rows` 列回 422 `dashboard.dataset_too_many_rows`（`details.max`，請縮小範圍或改用彙總），結果超過 2 MiB 回 422 `dashboard.dataset_result_too_large`，dashboard 用不了的欄位名稱或型別回 422 `dashboard.dataset_refused`。

- **上限**（`dashboards::caps`）：每個租戶每分鐘最多開始 60 次、同時最多 2 個；API 的執行先等租戶的回應空間最多 10 秒，再等租戶的空位最多 10 秒，telemetry-query 回答後再等記憶體最多 10 秒（見「記憶體」；所有租戶沒有執行的總名額，等 telemetry-query 回答時不佔記憶體）。超過或等不到時回 429 `rate_limited`、`dashboard.runs_limited`，帶 `Retry-After`（整數秒，至少 1）與 `details.retry_after`。上限以**每個副本**計：N 個副本時一個租戶每分鐘最多 N × 60 次，telemetry-query 自己的每租戶額度（每分鐘 120 次）仍然有效。因每分鐘次數、空位或回應空間被拒的執行不呼叫任何服務、也不算進每分鐘的次數；因記憶體被拒的執行已經呼叫 telemetry-query、用了它的額度，所以算進每分鐘的次數，回答不讀。`dashboard` 工具的執行用同一個 `DatasetRunner`，與瀏覽器共用上限與記憶體。
- **不重用結果**：沒有快取，每次執行都重新查詢。打開儀表板與執行它的資料集不呼叫 AI 閘道。
- **讀回答的上界**：telemetry-query 的回答最多讀 4 MiB（`MAX_READ_BYTES`），宣告了長度的回答照長度配置、宣告超過上限的不讀，沒宣告的以倍增成長、不超過上限；SQL 回答的欄位與列在讀出時就以結果的上限為界（最多 64 欄、資料集的 `max_rows` 列、每列不多於欄數、每格是 null、布林、數字或字串），到了就停，回應與之後檢查結果時相同（多的欄、用不了的型別、陣列或物件的格是 `dashboard.dataset_refused`，多的列是 `dashboard.dataset_too_many_rows`，比欄數寬的列是 `dashboard.dataset_failed`）；統計與用量回答的表格在組出之前就以 `max_rows` 為界（多的列是 `dashboard.dataset_too_many_rows`）。所以一次執行佔的記憶體取決於結果的上限，而不是回答裡的格子有多小；這個量在回答的狀態與標頭到了、讀本文之前預留（`answer_bytes`），結果檢查完只留結果 JSON 的長度，回應本文寫出或丟棄時放回。
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

### 記憶體

儀表板的結果在每個副本共用一個以位元組計的記憶體預算（`dashboards::memory`，`RESULT_MEMORY` 576 MiB），一個租戶最多佔一半（`RESULT_MEMORY_PER_TENANT` 288 MiB）：一個租戶拿不走全部，兩個租戶都到份額時預算就滿。用到結果的地方都先預留、用完放回；等不到就拒絕，每段等待都有上限，中途放棄時已預留的全部放回：

- **資料集執行**：等 telemetry-query 回答時只佔租戶的執行空位，不佔記憶體。回答的狀態與標頭到了、讀本文之前，才預留讀取與解析可能用到的量（`answer_bytes`，有宣告長度時依長度），最多等 10 秒；結果檢查完只留結果 JSON 的長度。API 的回應本文持有這份預留，直到最後一段交給連線、本文被丟棄（用戶端離開），或從回應做好起過了 60 秒（`HELD_BODY_TIMEOUT`）；連線在時限內沒取走整個本文時就放回記憶體、中斷回應並關閉連線，讀者只拿到不完整的本文。這個時限讓慢的讀者仍拿得到最大的本文（2 MiB 在 60 秒內只需約 34.1 KiB/s，約 280 kbit/s）。不讀的讀者佔多少另有每個租戶的回應空間限制：API 的執行開始前，先從租戶的回應空間（`ANSWER_MEMORY_PER_TENANT`，預算的十六分之一，36 MiB）取最大結果的 2 MiB，最多等 10 秒，結果檢查完只留 JSON 的長度，與記憶體一起由回應本文持有到同樣的時候。所以一個租戶還沒被讀者取走的回應合計最多 36 MiB，超過時只有這個租戶新的執行等待、等不到回 429，不影響別的租戶的執行與寫入；因回應空間被拒的執行不呼叫任何服務、不算進每分鐘的次數。回應空間不是另一份記憶體：記憶體仍是結果的預留，在租戶的份額與預算內。本文以複本分段（每段最多 64 KiB）交給連線，連線把一段寫進 socket 才給下一段，所以連線的寫入緩衝裡最多兩段（128 KiB），連線取走最後一段時其餘都已寫進 socket；讀者還沒讀的部分在 socket 的送出緩衝裡，與其他回應一樣不在預算裡。等不到回 429 `dashboard.runs_limited`（`Retry-After: 1`）。
- **AI 寫入**（`dashboard` 工具）：重用的結果 JSON、每次執行帶回的結果、結果讀回的值，到寫入結束前都持有；檢查請求本文與判定的預留在判定變成標記或問題時就放回，不持有到存檔；等不到回 `check_busy`。
- **判定**：解析前先數 JSON 值（不配置任何東西），超過 `MAX_VERDICT_VALUES`（3 × 10,000 + 64）就不解析，回 `check_invalid_verdict`，所以解析的用量取決於 contract 的上限，而不是判定裡的值有多小。

取得的順序固定：租戶的寫入空位、記憶體、檢查名額。持有記憶體時唯一等的別的東西，是寫入的下一個資料集等租戶的執行空位，最多 10 秒，所以寫入與執行不會互相等下去。

各部分的上界（由上限推得的估計，不是實測；實測與調整在 Laysi/trustiot-v2#694）：

| 部分 | 每次的上界 | 依據 |
| --- | --- | --- |
| 資料集執行，讀與解析時 | SQL 約 26 MiB，統計與用量約 54 MiB，錯誤回答約 4 MiB | 回答最多 4 MiB，沒宣告長度時讀取最多佔 8 MiB。SQL：最多 2,000 列，每列以 64 欄配置（每格 32 bytes，`serde_json::Value`），每個字串一份配置開銷（約 32 bytes），文字與解析暫存各不超過回答。統計與用量：有型別的解析每 byte 最多 8 bytes（`flatten::PARSE_FACTOR`，單元測試對照型別的大小），2,000 列每列約 2 KiB，文字。都再加結果 JSON 最多 2 MiB |
| 一個結果，執行之後 | 結果 JSON 的長度，最多 2 MiB | 回應本文的最後一段交給連線、本文被丟棄或 60 秒（`HELD_BODY_TIMEOUT`）到了，或 AI 寫入結束前一直佔著 |
| 一次 AI 寫入 | 約 164 MiB | 資料集執行期間只有結果 JSON（最多 8 × 2 MiB）。之後：8 個結果讀回的值，各約 15.9 MiB（2,000 × 64 格，每格兩倍的 32 bytes 加配置開銷，加文字與暫存），共約 127 MiB；檢查請求本文最多 17 MiB（先算出長度、照長度配置）；判定約 19.7 MiB（讀取 8 MiB、文字與暫存各 4 MiB、最多 30,064 個值各 128 bytes） |
| 一個租戶同時最多 | 約 272 MiB，在 288 MiB 的份額內 | 最大的一次寫入加租戶同時的 2 次最大執行。單元測試 `a_worst_case_write_fits_a_tenants_share` 確認放得下，所以閒置的副本上最大的寫入也做得完 |

整個副本：

| 部分 | 上界 | 依據 |
| --- | --- | --- |
| 結果記憶體預算 | 576 MiB | `RESULT_MEMORY`，上表所有的預留 |
| 結果快取 | 64 MiB | `MAX_CACHED_BYTES`，以 key 加 JSON 的位元組計（寫入正在用的結果另算在預算裡） |
| 其他（runtime、連線池、HTTP client、進行中的提問） | 約 64 MiB | 估計，未實測 |
| 合計 | 約 704 MiB | |

chart 的記憶體上限（`services.assistant.resources`，requests 128Mi、limits 768Mi，[deploy/chart/README.md](../../deploy/chart/README.md)「AI 助理」）依上表設定：limits 比合計多的 64 MiB 留給配置器（多執行緒時的碎片）。requests 是閒置的服務加上常見的快取用量的估計。調整上面任何一個上界時，一起調 chart 的值與這張表。

## 設定

設定來自 `TRUSTIOT__` 開頭的環境變數（`__` 分隔層級、名稱轉小寫；時間長度寫成 `30s`、`10m`），或 `TRUSTIOT_CONFIG_FILE` 指的 TOML 檔，規則與 service-kit 相同（[crates/service-kit/README.md](../../crates/service-kit/README.md)）。只在啟動時讀；錯誤訊息只寫設定名稱，不寫值。

| 設定 | 變數 | 預設 | 說明 |
| --- | --- | --- | --- |
| 平台設定 | `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 必填 | Postgres 的位置；只接受 `verify-full`，`_CA_FILE` 是伺服器憑證唯一的信任錨。由部署設定（chart、`bun scripts/dev.ts env`） |
| 平台設定 | `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 必填 | 身分服務公鑰（JWK Set）的檔案，API 以它驗證每個請求的內部身分 token；執行期間每 5 秒重讀，讀到可用的公鑰之前 readiness 不過。chart 在身分服務開啟時給每個服務（身分服務沒開啟時本服務拒絕啟動），本機是 `bun scripts/dev.ts env` |
| `database.username`、`database.password` | `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | 必填 | 服務角色 `assistant_app` 的帳密；chart 的密碼來自 Secret `<release>-assistant-password` 的 `assistant_app` key |
| `database.platform_username`、`database.platform_password` | `TRUSTIOT__DATABASE__PLATFORM_USERNAME`、`TRUSTIOT__DATABASE__PLATFORM_PASSWORD` | 必填 | 平台角色 `assistant_platform` 的帳密，只給清除用；不能與服務角色相同。chart 的密碼來自同一個 Secret 的 `assistant_platform` key |
| `database.max_connections` | `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | `8` | 服務角色連線池每個副本的上限，4 到 100：readiness 最多同時兩條（`postgres` 與 migration 檢查）、清除的 job lock 一條，其餘給 API 的請求（每個請求一條；儀表板的資料集執行呼叫 telemetry-query 時不佔連線）與 `dashboard` 工具寫入的短交易。取得連線最多等 5 秒（`ACQUIRE_TIMEOUT`），等不到時請求回 503。chart 以 `services.assistant.database.maxConnections`（預設 8）設定，算進 chart 的連線預算 |
| `purge.interval` | `TRUSTIOT__PURGE__INTERVAL` | `1h` | 排程清除的間隔，1 分鐘到 24 小時（見[排程清除](#排程清除)） |
| `ai_gateway.url` | `TRUSTIOT__AI_GATEWAY__URL` | 必填 | AI 閘道的 `http://<host>[:<port>]`，不能有路徑、帳密或 query；mesh 內由 ztunnel 加上 mTLS |
| `ai_gateway.role` | `TRUSTIOT__AI_GATEWAY__ROLE` | `main` | 主 agent 用的模型角色 |
| `ai_gateway.max_response_bytes` | `TRUSTIOT__AI_GATEWAY__MAX_RESPONSE_BYTES` | 32 MiB | 回應本文超過就算失敗；1 KiB 到 256 MiB |
| `model.effort` | `TRUSTIOT__MODEL__EFFORT` | `medium` | `low`、`medium`、`high`、`xhigh`、`max`；對話開始時固定（之後的分流 Laysi/trustiot-v2#444 會逐對話選） |
| `model.max_output_tokens` | `TRUSTIOT__MODEL__MAX_OUTPUT_TOKENS` | `16000` | 一次回應的 `max_tokens`；1 到 128000 |
| `model.call_timeout` | `TRUSTIOT__MODEL__CALL_TIMEOUT` | `10m` | 一次模型呼叫的上限，涵蓋閘道換模型的所有嘗試；不超過 1 小時 |
| `model.cache_ttl` | `TRUSTIOT__MODEL__CACHE_TTL` | `5m` | 快取斷點的時效：`5m` 或 `1h` |
| `budget.tokens` | `TRUSTIOT__BUDGET__TOKENS` | `2000000` | 一個問題能處理的 token；1 到 100,000,000 |
| `budget.wind_down_percent` | `TRUSTIOT__BUDGET__WIND_DOWN_PERCENT` | `80` | 到這個百分比時請模型收尾；1 到 99 |
| `tools.call_timeout` | `TRUSTIOT__TOOLS__CALL_TIMEOUT` | `60s` | 一次工具呼叫的上限，也是一次資料集呼叫 telemetry-query 的上限；`dashboard` 工具自己的上限是 335 秒（見「`dashboard` 工具」） |
| `tools.max_calls_per_step` | `TRUSTIOT__TOOLS__MAX_CALLS_PER_STEP` | `8` | 一次模型回應裡同時執行的工具呼叫數；之後的呼叫回錯誤、不執行；1 到 32 |
| `tools.max_text_bytes` | `TRUSTIOT__TOOLS__MAX_TEXT_BYTES` | 64 KiB | 一個工具結果的文字上限；超過時改回錯誤，告訴模型怎麼縮小範圍。設了 `dashboard_check.url` 時至少 64 KiB（`read` 給完整的儀表板） |
| `tools.max_images` | `TRUSTIOT__TOOLS__MAX_IMAGES` | `4` | 一個工具結果的圖片數上限；不超過 20 |
| `tools.max_image_bytes` | `TRUSTIOT__TOOLS__MAX_IMAGE_BYTES` | 5 MiB | 一張圖（base64 文字）的上限；不超過 5 MiB |
| `identity.exchange_url` | `TRUSTIOT__IDENTITY__EXCHANGE_URL` | 無 | 身分服務 token 交換埠的 `http://<host>[:<port>]`（`http://<release>-identity-exchange:8082`），不能有路徑、帳密或 query；沒有就沒有工具，儀表板的資料集執行也都回 503。呼叫身分服務的時間上限是 15 秒（`IDENTITY_TIMEOUT`） |
| `identity.run_lifetime` | `TRUSTIOT__IDENTITY__RUN_LIFETIME` | `15m` | 一次提問的委派效期；整數秒，2 到 15 分鐘（身分服務的上限 900 秒）。提問在到期前 30 秒結束。設了 `dashboard_check.url` 時至少 365 秒：一次 `dashboard` 工具呼叫的上限 335 秒加這 30 秒 |
| `telemetry_query.url` | `TRUSTIOT__TELEMETRY_QUERY__URL` | 無 | telemetry-query 的 `http://<host>[:<port>]`；沒有就沒有 `telemetry_latest`、`telemetry_statistics`、`telemetry_usage`、`telemetry_sql`，儀表板的資料集執行也都回 503。設了就要有 `identity.exchange_url` |
| `assets.url` | `TRUSTIOT__ASSETS__URL` | 無 | assets 的 `http://<host>[:<port>]`；沒有就沒有 `device_models`。設了就要有 `identity.exchange_url` |
| `dashboard_check.url` | `TRUSTIOT__DASHBOARD_CHECK__URL` | 無 | 檢查服務的 `http://<host>[:<port>]`（chart 設成 `http://<release>-dashboard-check:8080`）；沒有就沒有 `dashboard` 工具。以 assistant 自己的身分呼叫（不帶使用者的 token），一次檢查最多 85 秒（服務的佇列等待 `queue.max_wait` 預設 10 秒、最多可設 60 秒，檢查 15 秒、清理 5 秒，加上餘裕），判定最多 4 MiB |

工具呼叫其他服務的時間上限與 `tools.call_timeout` 相同（`ToolRegistry` 的上限才是真正的界線），連線最多 10 秒，成功回應的本文最多 8 MiB，`telemetry_sql` 讀的 422 本文最多 32 KiB（`MAX_REFUSAL_BYTES`）。儀表板的資料集執行用同一個 client 與時間上限，成功回應的本文最多 4 MiB（`runner::MAX_READ_BYTES`，結果本身最多 2 MiB）。mesh 內是純 HTTP，由 ztunnel 加上 mTLS；不用環境變數的 proxy，不跟隨轉址（token 的 header 會跟著轉過去）。chart 從 `mesh.calls.assistant` 填 `ai_gateway.url`、`identity.exchange_url`、`telemetry_query.url` 與 `assets.url`，`services.assistant.env` 不能設定這四個（[chart README](../../deploy/chart/README.md)「AI 助理」）；授權矩陣在同一份 README 的「呼叫矩陣」。

## 問答的錯誤

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
| `the dashboard tool wrote a version` | INFO | `run_id`、`conversation_id`、`tenant_id`、`dashboard_id`、`version`、`target`（`new`、`draft`、`fork`） |
| `the dashboard tool saved nothing` | INFO | `run_id`、`conversation_id`、`tenant_id`、`target`、`problems`（個數）、`kinds`（問題的類別，逗號分隔） |
| `the dashboard tool refused a write: the question wrote the most it may` | INFO | `run_id`、`conversation_id`、`tenant_id`、`writes` |
| `the dashboard tool's write waited too long for its tenant's write in progress` | INFO | `run_id`、`conversation_id`、`tenant_id`；模型收到 `check_busy` |
| `the dashboard tool could not use the database` | ERROR | `run_id`、`conversation_id`、`tenant_id`、`action`、`error_kind`（SQLSTATE 或 `StoreError::log_kind`） |
| `dashboard check finished` | INFO | `run_id`、`conversation_id`、`tenant_id`、`outcome`（`passed`、`failed`）、`problems`、`duration_ms` |
| `a dashboard check gave no verdict` | WARN | `run_id`、`conversation_id`、`tenant_id`、`outcome`（`check_busy` … `check_invalid_verdict`）、`status_class`（沒有時是這個副本的檢查名額等不到，或請求沒送出）、`duration_ms` |
| `removed content from an answer` | INFO | `run_id`、`conversation_id`、`tenant_id`、`removed`（`code`、`html`、`sql`，逗號分隔） |
| `published a dashboard`、`restored a dashboard version` | INFO | `user_id`、`tenant_id`、`from`（發布的草稿或還原的儀表板）、`dashboard_id`、`version`（新的版本） |
| `changed whether a dashboard is archived` | INFO | `user_id`、`tenant_id`、`dashboard_id`、`archived` |
| `refused a dashboard version: the user or tenant used the minute's budget` | INFO | `user_id`、`tenant_id`、`action`（`publish`、`restore`）、`retry_after`（秒）；儀表板 API 的 429 `dashboard.versions_limited` |
| `dashboard request failed` | ERROR | `user_id`、`dashboard_id`（有的時候）、`action`、`error_kind`（SQLSTATE 或錯誤的種類，沒有資料庫的文字）；儀表板 API 的 503 與 500 |
| `conversation started`、`conversation updated`、`conversation deleted`、`conversation restored` | INFO | `user_id`、`tenant_id`、`conversation_id`；`conversation updated` 另有 `renamed`（有沒有改名）與 `archived`；不記標題 |
| service-kit 的 `refused to <動作>` | INFO | `user_id`、`tenant_id`、`action`、`reason`（`no_tenant`、`unrecognized_role`、`role_too_low`）、`exchanged`、`actor`：對話與儀表板 API 的請求不符角色要求（403，[service-kit 的 README](../../crates/service-kit/README.md)） |
| `conversation start refused: the user reached the daily limit` | WARN | `user_id`、`tenant_id`、`action`、`max`（100）、`retry_after_secs`：開始對話超過每日上限（429），每次被拒一行 |
| `could not start a database transaction`、`conversation request failed` | ERROR | `user_id`、`action`、`error`（只有類別，例如 `database error (SQLSTATE 23505)`），後者另有 `conversation_id`；API 回 503（前者也是儀表板的請求） |
| `purged expired conversations` | INFO | `job`（`purge`）、`purged`（這一輪刪了幾個對話）、`skipped`（0）；每一輪一行 |
| `purged expired conversations; some stayed locked and wait for the next round` | WARN | `job`、`purged`、`skipped`（被鎖住、留到下一輪的個數）；取代上一行，每一輪一行。等鎖逾時的批次另有 sqlx 超過慢查詢門檻的 WARN（只有 SQL，沒有值） |
| `purge round stopped early: a lock held up its deletions; the next round tries again` | WARN | `job`、`purged`、`skipped`：整張表被鎖住，一輪提早結束（見「排程清除」）；取代上面兩行，每一輪一行 |
| service-kit 的 `platform database access`、`scheduled job failed` | INFO、WARN | 清除每一批的平台交易（`purpose`）；一輪失敗時的 `job` 與只有類別的 `error`（[service-kit 的 README](../../crates/service-kit/README.md)） |

不記問題、回答、工具輸入與結果、錯誤訊息、儀表板的標題、SQL 或頁面，也不記被呼叫服務的 URL、查詢參數、回應本文與 token（只記狀態碼的類別）。欄位名稱都不會被 service-kit 的遮蔽規則擋掉（不用 `token`、`tokens`、`prompt`、`content` 這些字）。指標 `assistant.model_calls` 依 `outcome` 與 `cache`（`hit` 或 `miss`）計數；`cache_read` 長期為 0（只有 `miss`）表示快取失效，要接到監控（Laysi/trustiot-v2#554 以真實 API 驗證快取）。

## 之後接上的地方

| 項目 | 介接點 |
| --- | --- |
| 在對話裡發問與串流（Laysi/trustiot-v2#646、Laysi/trustiot-v2#147） | `api::conversations` 加端點，以 `store::load` 讀出對話、`Agent::ask` 回答，提問紀錄與寫入見[給聊天 API 的介面](#給聊天-api-的介面) |
| 回答被過濾的標示（Laysi/trustiot-v2#646） | `Answer::removed`（移除的類別，空集合時沒有移除）隨回答回給前端，畫面顯示 `answer_guard::CONTENT_REMOVED`（`assistant.answer_content_removed`）。歷史存的是過濾後的文字，所以重新載入對話時要再顯示標示，就要把 `removed` 也存下來（例如回答訊息旁的欄位）：由這兩個 issue 決定欄位與 migration |
| 串流的過濾（Laysi/trustiot-v2#147） | `answer_guard::find`：對目前為止的文字執行，暫緩送出從第一個延伸到結尾的發現處開始的文字；回答結束時以 `guard` 的結果為準 |
| API 的其他資源 | `api` 底下加一個模組，在 `api::routes` 合併它的路由、在 `ApiDoc` 列出它的端點；身分、錯誤回應、交易、分頁與 cursor 用 `api` 的共用函式 |
| 租戶脈絡的來源 | 開始對話時傳給 `Agent::start_conversation` 的文字，現在是空字串（`api::conversations` 的 `TENANT_CONTEXT`）；它在對話開始時固定 |
| 發問時開始一個問題（Laysi/trustiot-v2#646） | 處理請求時以 `delegation::QuestionRuns::start(&UserIdentity, conversation.principal())` 取得 `QuestionRun`，交給 `Agent::ask`；`Assistant::runs` 為 `None`（沒有設定 `identity.exchange_url`）時改用 `tools::NoDelegation`。`StartError` 已實作 `IntoResponse`（見「代表使用者呼叫」） |
| 更多工具 | 實作 `tools::Tool`，放進 `ToolRegistry`（最多 10 個），以 `callee::Callee` 呼叫服務；工具從 `ToolContext.principal` 取租戶與使用者，不從輸入取 |
| 對話存進 Postgres（Laysi/trustiot-v2#553 儲存、Laysi/trustiot-v2#644 對話串與提問紀錄已完成，連線池由 Laysi/trustiot-v2#592 接上） | `store` 的函式，在對話租戶的 `TenantTransaction` 裡呼叫，交易取自 `start` 建立的 `TenantDb`（API 的每個請求一個交易）；清除另用 `assistant_platform` 的 `PlatformDb`（`src/purge.rs`，見[排程清除](#排程清除)），見[給聊天 API 的介面](#給聊天-api-的介面) |
| 對話共享的讀者（Laysi/trustiot-v2#650） | `dashboards::store::conversation_access` 回讀者的存取（新的 `Access`），`store::permissions` 決定讀者能做什麼 |
| 每次呼叫預扣額度（Laysi/trustiot-v2#140） | 每次呼叫的 `ModelRequest::call_id`（帳本的 `llm_call_id`，現在只記在日誌）；閘道拒絕時以 `ModelError` 回到迴圈 |

## 測試

`cargo test -p assistant`（`tests/history_store.rs`、`tests/conversation_threads.rs`、`tests/api.rs`、`tests/purge_job.rs`、`tests/purge_locks.rs`、`tests/startup.rs`、`tests/dashboards_store.rs`、`tests/dashboards_api.rs`、`tests/permissions.rs` 與 `tests/cross_tenant.rs` 以 podman 或 docker 起一個拋棄式 TLS Postgres，以 migration Job 套用整個產品的 schema；沒有容器執行環境時在本機略過，設了 `CI` 時失敗）。`tests/dashboard_acceptance.rs` 的兩個驗收測試預設 `#[ignore]`，因為要檢查服務的映像檔：`bun test deploy/images/dashboard-check` 建出 `localhost/trustiot-e2e/dashboard-check:image-test` 之後，`DASHBOARD_CHECK_IMAGE=<映像檔> cargo test -p assistant --test dashboard_acceptance -- --ignored`（加 `--nocapture` 印出每個比對的值）；以 docker（沒有時 podman）起檢查服務、Postgres 與 ClickHouse，一次跑一個，沒有映像檔或容器執行環境時略過，設了 `CI` 時失敗。CI 目前不跑這兩個測試。測試以本機 loopback 上的替身 AI 閘道回放 `tests/fixtures/` 的回應（取自官方文件，見該目錄的 README），不呼叫供應商，也沒有 API key。

| 檔案 | 內容 |
| --- | --- |
| `tests/agent.rs` | 60 步的長分析不受步數限制；接近預算時收尾並回答；放不進剩餘預算的呼叫不送出、輸出上限降到剩下的空間；用量超過估計而預算用完時工具不執行；第二個問題的請求逐位元組以前一次請求的歷史開頭（thinking 與 signature、system、工具定義不變，租戶脈絡包在標籤裡）；兩個使用者同時發問互不等待；模型逾時與閘道錯誤只給固定訊息、日誌沒有錯誤文字；拒答不留在歷史；每一步的時間與快取讀取都記在日誌 |
| `tests/tools.rs` | 工具組規則（最多 10 個、名稱、schema）；並行執行、結果依呼叫順序；一步超過呼叫數上限的呼叫不執行；輸入錯誤、失敗、panic、逾時、不存在的工具各自回錯誤給模型；結果超過大小與圖片上限時請模型縮小範圍；工具拿到的身分來自對話；圖片照 Anthropic 格式送給模型；一步被丟下時（提問的截止時間）還在跑的呼叫跟著取消，沒送出任何東西 |
| `tests/delegated_tools.rs` | 四個呼叫服務的工具各以使用者身分、受眾正確且在呼叫當下交換的 token 呼叫服務，請求裡沒有租戶或使用者，超過 60 秒後的呼叫仍成功、舊 token 會被拒（替身以「每次模型呼叫過 61 秒」的虛擬時鐘模擬，不真的等）；服務回 403、404、422、503 與非 JSON 時模型與日誌都看不到回應本文；身分服務拒絕交換（撤銷）時該步停止、問題結束、不再重試；身分服務暫時無法使用只讓該次呼叫失敗；問題在委派到期前結束、沒有剩餘時間的問題不開始；只有對話的使用者能開始問題，為別人的對話建立的 run 在送出任何東西前被拒；裝置寫的長文字截到 256 個字元、加上截斷標記才給模型；工具輸入不收租戶、使用者與多餘欄位；工具的路徑與上限跟 telemetry-query、assets 的程式一致；語意工具以 POST 送出的本文是 API 的 `SeriesQuery`（替身照服務的型別解析、拒絕未知欄位），輸入 schema 與 `telemetry_query::openapi()` 的 `SeriesQuery` 一致；列數超過服務上限時不呼叫 |
| `tests/history_store.rs` | 主 agent 跑完的對話（thinking 與 signature、工具呼叫與結果）存進去、讀回來逐位元組相同，載入的對話接著問時請求的前段與歷史和原本的逐位元組相同；服務角色的 UPDATE、DELETE、TRUNCATE 在租戶交易內外都被資料庫拒絕（`42501`）；重複寫入不改變什麼，兩個寫入者分岔時後者被拒、不覆寫，空洞與沒建立的對話被拒；其他租戶與同租戶的其他使用者載入不到；損壞的列讓載入失敗 |
| `tests/api.rs` | 對話串的 API，經執行檔本身（`tests/service/mod.rs` 起子行程）、真的身分驗證（測試簽發者簽的 token 與公鑰檔）與產品 schema：沒有 token、別的簽發者、給 assets 的 token 在每個端點都 401，沒有租戶 403，都帶 `no-store`、什麼都沒建立；同租戶的其他使用者（viewer、editor、admin）三個清單都是空的，對擁有者的對話（一般、封存、已刪除）的每個 ID 請求與不存在的 ID 逐位元組相同（404），資料不變；擁有者自己的請求也一樣：路徑 ID 不是 UUID 的每個請求、已刪除對話的讀取、改名、封存與刪除、還原沒刪除的與刪除超過 30 天的，回應都與同一個請求對不存在的 ID 逐位元組相同（狀態、本文、`content-type`、`cache-control`）；真的主 agent 經替身模型思考、呼叫工具、回答後存起來，讀取只回問題與回答的文字，thinking、工具輸入與結果、工具定義、租戶脈絡、system prompt 的標記都不在回應裡（但都在資料庫裡）；改名去空白、各種 400 與 422、封存與取消封存、預設與 `archived=true` 的清單、12 個對話（5 個最後活動時間相同）以每頁 5 個走完不重複不遺漏、錯的 `limit` 與錯清單或改寫過的 cursor 回 400；刪除後離開清單、讀取 404、在已刪除清單（`restorable_until` 是 30 天後），還原後讀取與每一列都和刪除前相同；有提問進行中時刪除回 409、資料不變；超過 30 天的不在已刪除清單、還原 404，一輪清除把它連同訊息與提問紀錄刪掉，其他對話不受影響；開始對話要 JSON 的 `Content-Type`，接受沒有本文、`null` 與 `{}`，回 201 與 `Location`，存的租戶脈絡是空的；日誌沒有標題；資料庫失敗（測試加的唯一索引讓第二次開始對話以 `23505` 失敗）時回 503，回應與日誌都沒有 Postgres 的訊息與 DETAIL（`duplicate key`、索引名稱、`Key (`、`already exists`），日誌只有 `database error (SQLSTATE 23505)`，什麼都沒存；那個索引以 `CONCURRENTLY` 建立與刪除，同一個執行檔的其他測試共用資料庫，它們的請求不會排在這些 DDL 後面：刪除等著測試開著的交易時，同租戶另一個使用者的讀取與開始對話立刻回 200 與 201（一般的 `DROP INDEX` 拿 ACCESS EXCLUSIVE，會讓它們等到 lock timeout 回 503，測試確認過）；另一個工作階段鎖住對話那一列時，改名與刪除在 `REQUEST_LOCK_TIMEOUT` 後回 503、日誌記 `55P03`、資料不變，鎖放開後同樣的請求成功；24 小時內開始了 100 個對話（其中 40 個已刪除）後再開始回 429 `rate_limited`、`conversation.start_limited`、`details.max` 100、`Retry-After`（依最舊的那個，一小時前開始的是 23 小時）與 `no-store`，什麼都沒存、每次記一行 WARN（使用者與租戶），刪除不會還回名額，同租戶的其他使用者與同一個使用者在別的租戶不受影響，把一個的開始時間移到 24 小時前後可以再開始一個；24 個請求同時搶最後 5 個名額時剛好 5 個成功、存的不超過 100 |
| `tests/startup.rs` | 執行檔本身：以 `assistant_app` 登入、有身分服務的公鑰、資料庫有這一版的 migration 時 `/readyz` 回 200，日誌記下工具數（設了 `dashboard_check.url` 時六個）與可以執行資料集且不含密碼；服務角色或平台角色的密碼錯誤時 readiness 記下失敗、回 503，日誌沒有密碼；少了一個 migration 檔案時日誌寫出缺的檔案、回 503，補回後恢復 |
| `tests/purge_job.rs` | 排程清除，經兩個執行檔共用一個資料庫：第一個副本的一輪被測試加的觸發器擋在 DELETE 裡（`pg_sleep` 等測試開閘，不靠時間）時，job lock 在服務角色的連線上，第二個副本記下跳過這一輪、沒開平台交易；開閘後第一個副本刪掉過期的對話連同訊息，記 `purged` 為 1；這個對話有帶標記的標題，兩個副本的日誌（stdout 與 stderr）都沒有它的 ID、租戶與標題；兩個副本的 readiness 都是 200。觸發器會擋住其他測試的清除，所以是自己的測試執行檔、自己的 Postgres |
| `tests/purge_locks.rs` | 清除遇到整張表被鎖住：superuser 以 `LOCK TABLE ... IN SHARE MODE` 擋住所有 DELETE（讀取不擋）時，一輪在一次刪一個的那一批後就停（只開了批次、讀鍵與每個對話各一個平台交易，沒有第二批），`Round { purged: 0, skipped: 3 }`，一行 WARN `purge round stopped early: ...`，只有計數；以 `ACCESS EXCLUSIVE` 連讀取都擋時，一輪在鎖還在的時候就結束（讀鍵等 `PURGE_LOCK_TIMEOUT` 後放棄），什麼都沒刪，同樣一行 WARN；鎖放開後下一輪全部刪掉，記 INFO。表鎖會擋住共用資料庫的其他測試，所以是自己的測試執行檔、自己的 Postgres；兩個測試輪流執行 |
| `tests/chart_connections.rs` | chart 的 Postgres 連線預算（`trustiot.postgres.connectionBudgets`）與程式開的連線池一致：服務角色的連線池以 `database.maxConnections` 設定、範圍與服務接受的相同，平台角色的連線池（`PLATFORM_CONNECTIONS`）是 `otherPools`；程式多了一個連線池時失敗 |
| `tests/conversation_threads.rs` | 對話串（Laysi/trustiot-v2#644）：刪除的對話對擁有者與同租戶的人都不存在、只在擁有者的已刪除清單出現，還原後每一列與刪除前相同；滿 30 天的不能還原，清除連同訊息、捨棄紀錄與提問紀錄刪掉（跨租戶），沒過期的與沒刪除的不受影響；清除角色只看得到、刪得掉過期的對話（它的交易自己設定租戶也一樣），碰不到訊息，服務角色什麼都刪不掉（`42501`）；還原與清除、刪除與寫入、刪除與開始提問同時發生時兩種順序各測一次（以 `pg_blocking_pids` 確認後到的一方在等鎖，不靠時間）；捨棄最後一輪後歷史與之前逐位元組相同、下一輪接在後面，捨棄不是最後一輪的被拒；同時開始兩個提問只有一個成功，過了截止時間的提問在下一次開始時變成 `interrupted`；有進行中提問的對話刪除被拒、資料不變，過了截止時間（包括在刪除的交易開始之後才過）的不擋；清除的一批等不到列鎖時在 `PURGE_LOCK_TIMEOUT` 後以 `55P03` 失敗、什麼都不刪；最舊的過期對話整輪都被另一個工作階段鎖住時，`purge::round` 刪掉較新的那個、略過被鎖的（`skipped` 為 1，一行 WARN，沒有 ID 與租戶），鎖放開後下一輪刪掉它，記一行 INFO，平台角色的 `SKIP LOCKED` 被拒（`42501`）；同租戶的其他使用者讀、改名、封存、刪除、還原、捨棄、開始與停止提問都當作不存在、資料不變；資料庫拒絕指定 `created_at`、`started_at` 與往前改的時間（`42501`），拒絕超過 32 MiB 的訊息（`23514`）；`StoreError` 不含 Postgres 的訊息與 DETAIL；到開始對話的上限時 `store::create` 回 `StartLimited`（秒數依最舊的那個）、什麼都不存，重試已存的對話不算新的；標題規則、封存、只列封存的與清單分頁 |
| `tests/permissions.rs` | 權限矩陣（A2，[docs/architecture/permissions.md](../../docs/architecture/permissions.md) 的「assistant」）：對話與儀表板的 16 個 operation 各自宣告要求（發布、封存、取消封存是租戶 editor，其他是租戶 viewer），宣告與 OpenAPI 文件、矩陣文件逐列對照；經執行檔本身，對每個 operation 以沒有 token、偽造的 token、給別的服務的 token、沒有租戶、沒有租戶的平台管理員、每個租戶角色、不認得的角色與換發的 token 各送一次請求，被拒的都在讀 body 與資料庫之前；探測前後對話、訊息、捨棄的回合、提問紀錄、儀表板與版本的筆數不變；身分服務的 token 交換、telemetry-query、assets 與檢查服務的位址是只計算連線數的本機埠，探測後都沒有連線；viewer 與沒有租戶的呼叫者被拒時記 `refused to <動作>` 與原因 |
| `tests/cross_tenant.rs` | 跨租戶負向測試（決策 29，`crates/testkit`）：六張表（對話、訊息、捨棄的回合、提問紀錄、儀表板、儀表板的版本）的讀、寫、搬移、插入探測與覆蓋檢查；API 的每個 operation 都宣告：對話的清單、讀取、改名與封存、刪除、開始對話，已儲存儀表板的清單、讀取、版本列表、還原、封存、取消封存與資料集執行，以及草稿的發布，經執行檔本身探測（資料集執行的正向對照經替身的 token 交換與 telemetry-query 真的跑完，沒有呼叫其他服務）；對話與草稿是同一個人在兩個租戶各自擁有的（`owned`），所以只靠租戶分開；已刪除清單與還原只對已刪除的對話有作用，suite 種不出來，由同一個檔案的 `deleted_conversations_stay_in_their_tenant` 手動探測：在租戶 A 看不到、還原不了租戶 B 刪除的對話（與不存在的 ID 逐位元組相同的 404，資料不變），沒有租戶時都 403，在租戶 B 照常列出與還原；對話的草稿列表的路徑參數是對話的 ID，suite 指定不了，由 `a_conversations_drafts_stay_in_their_tenant` 以同一個人在兩個租戶手動探測：在租戶 A，租戶 B 的對話的草稿列表、B 的草稿與它的版本列表都與不存在的 ID 逐位元組相同（404），沒有租戶時 403，在租戶 B 照常看得到 |
| `tests/dashboards_store.rs` | `dashboards::store` 對真的 schema：版本只往後加、舊版本讀得回來；發布第一次建立已儲存的儀表板、之後加版本，已儲存的儀表板在發布的版本的基礎版本之後有新版本或已封存時拒絕；誰可以發布、還原、封存；工具只寫自己對話的草稿（別的對話與別的使用者都是不存在）；列表分頁與只列看得到的；服務角色改不了寫過的東西（觸發器與權限，`42501`，包括版本的基礎版本；草稿的基礎版本可以降或清掉）、沒有已儲存儀表板的草稿與已發布的版本不能有基礎版本、留不下空洞；草稿的建立者不是對話的擁有者時資料庫拒絕；排程清除（`purge::round`）過期的對話時刪掉草稿與它的版本（包括還原的列；版本之間的兩個外鍵是 NO ACTION，在敘述結束時才檢查）、已儲存的儀表板與它的版本不變；草稿被別的交易鎖住時那個對話這一輪略過（`skipped`）、下一輪清掉，記錄只有 `job`、`purged`、`skipped`；還原只加沒有內容的列、指向存著內容的版本（交替還原兩個版本時只隔一層），每一版（開啟、資料集、版本列表、列表的標題）讀回自己的內容，發布還原的版本時複製內容；資料表拒絕指向另一個還原、與還原的版本不符、帶著內容、沒有內容也沒有指向，或不是還原卻指向內容的列；同一個草稿同時寫入時排隊、後者衝突；軟刪除的對話的草稿對每個函式（開啟、草稿列表、版本列表、資料集、工具的三種寫入、發布、還原、封存）都是 `NotFound`、什麼都不改，從它發布的已儲存儀表板不變、每個成員照常看得到，對話還原後草稿原樣回來、可以再寫；寫草稿的交易與對話的刪除互相等待，兩種順序各測一次（以 `pg_blocking_pids` 確認後到的一方在等鎖，不靠時間）：寫入先時刪除等它 commit、之後連新版本一起隱藏，刪除先時寫入等它、之後找不到對話、什麼都沒寫 |
| `tests/dashboards_api.rs` | 儀表板的 API 對真的 schema 與替身的 token 交換、telemetry-query：沒有 token、偽造的 token、沒有租戶時每個路由都拒絕；本文、query string 與 content type 不對時 400 而且什麼都沒寫（含 SQL、頁面等欄位、陣列寫法）；回應都是 `no-store`；請求的角色決定誰能發布、還原、封存（同一個使用者升降級後立即生效）；同租戶的其他使用者與其他租戶的 admin 對別人的草稿每個路由都是 404，與不存在的相同；另一個工作階段鎖住對話的列時，同租戶的 admin 對別人草稿的發布與還原仍在那個工作階段結束前回同樣的 404，擁有者自己的發布則等那一列、在請求的鎖等待上限後回 503；版本、還原、發布衝突與封存；發布比較那一版自己的基礎版本：合併了新版本之後，合併前的版本不能發布到較新的已儲存版本上，還原舊版本帶回它的基礎版本（再發布與之後工具寫的版本都回 409），還原合併過的版本則能發布；第一次發布前寫的版本（包括發布出去的那一版）與從它們還原的版本不能再發布，之後工具寫的版本也一樣，直到合併已儲存的最新版；發布與還原每個使用者每分鐘第 11 個版本、每個租戶的草稿第 31 個版本回 429 與 `Retry-After`、什麼都沒寫，已儲存的儀表板另計（草稿用完時仍能發布）；被 store 拒絕的請求（舊的基礎版本、viewer 的 40 次發布、不存在的版本）不算，其他租戶照常；回到與最新版內容相同的版本回 409、不加版本，草稿的還原不加已儲存儀表板的版本；10,000 個版本的儀表板不能再還原或發布、回 409 與原因；資料集以打開的 viewer 身分執行（token 的使用者、租戶、角色、受眾與 `act`），請求本文原樣、SQL 的列數上限 2,000；超過列數時拒絕並附上限；第 61 次執行與第三個同時執行回 429 而其他租戶照常、`dashboard` 工具的執行算進同一個上限；四個租戶各有 2 次執行在等很慢的 telemetry-query 時，第五個租戶的執行立即完成；執行的回應本文還沒讀時佔著結果 JSON 長度的記憶體，讀完放掉或沒讀就丟棄都放回，經 TCP 時用戶端拿到整個本文後放回；約 1.5 MB 的回應本文，讀取的讀者逐位元組拿到整個本文，停止讀取的讀者（小的 socket 緩衝，測試把時限設成 1 秒）一直不讀，伺服器仍在時限後關閉它的連線，記憶體這時已放回，再讀時只拿到不完整的本文；租戶的份額被佔滿時它的執行在 telemetry-query 回答後回 429（`Retry-After: 1`）、不讀回答、算進每分鐘的次數，其他租戶照常，記憶體放回後再執行；租戶還沒被讀取的回應佔滿它的回應空間時，它新的執行回 429（`Retry-After: 1`）、不呼叫 telemetry-query、不算進每分鐘的次數，其他租戶的執行照常，讀者離開後再執行；telemetry-query 的錯誤只以封閉的 key 回給使用者；沒有 token 交換時 503；打開儀表板並執行全部資料集不呼叫模型、日誌沒有標題、SQL 或頁面；以 API 刪除對話後，它的草稿在每個儀表板路由（草稿列表、開啟、版本列表、資料集、還原、發布、封存與取消封存）的回應都與不存在的儀表板與對話逐位元組相同（404），什麼都沒寫，以 API 還原對話後原樣回來，已儲存儀表板的列表一直不變 |
| `tests/dashboard_tool.rs` | `dashboard` 工具對真的 schema、替身的 token 交換、telemetry-query 與檢查服務：寫入以提問的使用者身分（委派、受眾、`act`）執行每個資料集，把頁面、公開的 spec（沒有 SQL 與語意請求）與結果送去檢查，通過後才存成這個對話的草稿，`list` 與 `read` 看得到（頁面整份）；檢查不通過（數值對不上、沒對應的圖形、較新的問題種類）與每種拿不到判定的情況（503、400、500、不是判定、引用了頁面沒有的資料集、值不在資料裡（不通過與通過的判定都是）、連不上、逾時）都什麼都不存、草稿的最新版本不變，模型與日誌都看不到檢查服務的本文；模型從判定只拿到封閉的值與工具自己的資料：script 錯誤的類別而不是訊息、刻度只給軸、超出結果的列只給自己的列數、頁面文字沒有的數字不給、`too_large` 的上限是 contract 的，通過時標記的值取自自己的資料、不含刻度；別的對話、別的使用者、別的租戶的草稿都是不存在，`base_version` 舊了或沒給、輸入格式不對、`page` 與 `page_edits` 同時給、頁面過大、資料集太多各自回固定的問題，第 7 次寫入被拒、下一個問題重新計算；已儲存的儀表板只變成這個對話的一個草稿（第二次指出既有的草稿）、封存的不能改；`rebase_saved_version` 只接受這個問題讀過的已儲存版本（讀了別的版本、別的問題讀過或只讀了草稿都拒絕，不送檢查），讀過之後寫入成功、草稿以那一版為基礎；telemetry-query 拒絕 SQL 時回 `telemetry_sql` 的類別說明，拒絕本文多了欄位時只回通用說明；撤銷的工作階段結束整個呼叫；剩下的時間不夠時不開始；同一個問題的同一個資料集只執行一次；資料集在寫入的時限內（測試設 3 秒）沒跑完時各回 `dataset_timeout`、不送檢查，跑完的留給同一個問題的下一次寫入、那次只執行沒跑完的；工具自己的呼叫時限大於資料集、檢查與存檔時限的總和；一個租戶的第二次寫入在第一次檢查時等到測試設的時限後回 `check_busy`，沒執行資料集、沒送檢查、不算進寫入次數，同時別的租戶的寫入照常存下；檢查名額設成 1 時別的租戶的檢查也等到時限後回 `check_busy`；兩個租戶的寫入在等很慢的 telemetry-query 時，第三個租戶的寫入不等待就檢查並存下；租戶的份額被佔滿時重用結果的寫入立即回 `check_busy`、不執行資料集、不送檢查、不算次數，只剩結果與值的空間而沒有檢查的空間時在資料集之後回 `check_busy`、不送檢查、算一次，新問題的資料集因記憶體被拒時是 `runs_limited`，每次寫入結束後都不佔記憶體；寫入在等檢查時以 API 刪除對話，刪除立即回 204（工具還沒開交易），寫入之後被拒，模型拿到的文字與不存在的儀表板相同，什麼都沒存；判定變成標記後就放回檢查的記憶體，存檔等連線時只佔著值的記憶體；另一個工作階段鎖住對話的列時，存檔在 API 的鎖等待上限後以 `55P03` 回固定的失敗（在存檔的 30 秒之前、那個工作階段結束之前），之後沒有陳述式還在等那一列，列放開後下一次寫入存得下 |
| `tests/dashboard_acceptance.rs` | 跨服務的驗收（Laysi/trustiot-v2#150），經 `ask` 與照腳本回答的替身 AI 閘道，`dashboard` 工具呼叫以映像檔跑的真正檢查服務（chart 的限制：唯讀根目錄、chart 大小的記憶體 /tmp、拿掉所有 capability、no-new-privileges、預設 seccomp、chart 的記憶體上限、UID 65532），中間的 loopback 代理記下服務送出的每個判定。驗收 2：沙箱 runtime 的固定樣本（`frontend/sandbox/fixtures`）與照樣本回答的 telemetry-query 替身，模型先寫好的頁面（第 1 版），再寫一根長條只畫一半高的頁面（`mark_geometry`）與標記了不存在的列、資料集和沒對應圖形的頁面（`unmapped`），三個判定都符合 cases.json 的案例，模型收到問題，API 看到的仍是第 1 版與好的頁面，資料集在問題裡只執行一次。驗收 1：合成 lab 資料集（`datasets/lab/synthetic`）載入拋棄式 ClickHouse，telemetry-query 的路由在測試程序裡以產品的 schema 執行資料集，檢查報告裡的每個值都等於以讀全部列的測試帳號直接查 `trustiot.telemetry` 算出的同一個量：原始讀數 view 的每部電表筆數、最低、最高與總和，1 小時格 view 的每小時最高與平均，統計 API 的筆數、最小、最大、平均與最後讀數，用量 API 的總量（清零一次的電能、32 位元回捲的脈衝）與讀數數，以及 `max`、`sum`、跨欄 `max − min`、索引與算術、`last` 的計算 |
| `tests/check_stand_in/` | 檢查服務的替身：記下每個請求，預設回報告每個資料集列數的通過判定，測試可以改成任何狀態與本文 |
| `tests/answer_guard.rs` | 經 `ask` 與替身 AI 閘道：回答裡的 SQL 程式碼區塊、句子中的 SQL 敘述、HTML 與含程式碼的行內程式碼，到呼叫端與歷史時都已移除，回報移除了哪幾類、日誌只記類別不記內容，thinking 區塊逐位元組不變，下一個問題送回的歷史就是過濾後存下的那一則；改變 schema 的敘述（`CREATE OR REPLACE VIEW`、`CREATE TABLE`、`DROP TABLE`、`ALTER TABLE`、`TRUNCATE TABLE`）從回答中移除；放在 `tests/`，因為 `cargo test -p repo-check` 不允許 `src/` 出現這些文字 |
| `tests/dashboard_world/` | 儀表板測試共用的世界：產品的 Postgres、測試用的身分簽發者、替身的身分服務 token 交換與 telemetry-query（記下每個呼叫、其他路徑都算意外）、啟動執行檔、問題的委派（每次呼叫簽發 `act` 是 assistant 的 token）與照服務組起來的 `dashboard` 工具 |
| `tests/telemetry_sql.rs` | `telemetry_sql` 經工具組呼叫：對 telemetry-query 真正的路由（Postgres 與 ClickHouse 是計數的 loopback 埠），系統表、table function、白名單外的函式、`SET ROLE`、`SET DEFAULT ROLE`、多個敘述、`SETTINGS`、CAST、沒寫時區、沒讀 view、引號識別字、反斜線、語法錯誤與太深的巢狀各自回固定的類別說明，兩個資料庫都沒有連線，日誌沒有 SQL；對替身，答案（長文字截斷）、執行後的拒絕、多欄位或不認得的類別與名稱、404、429、504、403、400、5xx 與非 JSON 各自回固定訊息，模型與日誌都看不到服務的本文；每次呼叫換新 token、請求正好是 SQL 加 200 列 |
| `src/` 的單元測試 | API 的 cursor（往返資料庫的時間、拒絕別的清單與其他寫法）、`limit`、JSON 的 `Content-Type`、更新本文與標題的規則；聊天顯示的文字只取問題與回答的 `text` 區塊；設定檢查（含被呼叫服務的位址、委派時長（有 `dashboard` 工具時至少 365 秒）與兩個角色的資料庫帳密、連線池上限、清除間隔）、截止時間的計算、`StartError` 的錯誤碼、字串上限、時間區間的粒度選擇與查詢範圍、狀態碼類別、422 拒絕本文的嚴格解析、`telemetry_sql` 每個拒絕類別的說明與名稱的再檢查、預算門檻、租戶脈絡的跳脫、狀態碼對應、錯誤 `type` 過濾、沒有用量時的估計、送出前的訊息估計（圖片按張數）、工具定義與訊息讀回的位元組檢查、migration 與程式的上限一致（30 天、32 MiB、200 個字元）、資料庫錯誤只留類別、失敗原因代碼的格式；儀表板：執行上限（第 61 次、第三個同時執行、等到空位、`Retry-After` 的秒數）、每個鍵的名額與等待、發布與還原每分鐘的版本數（每個使用者、每個租戶的已儲存儀表板與草稿各自計、等到兩者都有空位的秒數、清掉閒置的使用者與租戶）、語意回答的攤平、telemetry-query 每種回答對應的錯誤、權限表、分頁 cursor；SQL 回答只在結果的上限內讀出（多的欄、多的列、比欄數寬的列、陣列與物件的格在讀出時就停，讀出的值與 contract 的型別相同）、統計與用量的表格在 `max_rows` 停、有型別的解析不超過 `PARSE_FACTOR`（對照最短的 JSON）、結果記憶體（一個租戶最多它的份額、等到放回的記憶體、縮小時放回其餘、等不到時什麼都不佔）、執行的回應本文（分段逐位元組交出後放回、連線的緩衝裡最多兩段、寫出一段才給下一段、時限到了放回並喚醒等著的連線、以錯誤結束、沒讀就丟棄時立即放回、不讀的讀者佔的量讓預算仍留得下另一個租戶的份額）、讀回答時有宣告長度照長度、沒有時倍增不超過上限、判定解析前先數值（最多標記的判定可解析、陣列值與過多的標記不解析）與每個值的大小、檢查佔的記憶體、結果讀回成值的估計、最大的寫入加 2 次最大的執行放得進一個租戶的份額、`json_len` 與 JSON 的長度相同；`dashboard` 工具的輸入解析、問題的寫入上限與資料集快取的預算（以位元組計、一個租戶的份額、一個租戶的許多問題拿不走別的租戶的結果、整個快取滿了時讓出最久沒寫的）、判定的問題與標記怎麼對應（錯誤類別、每個種類名稱與 contract 相同、標記只取解得出的參照）；回答過濾（程式碼區塊、含程式碼的行內程式碼、HTML 標籤與 script/style、各種 SQL 敘述，以及要留下的文字：不是標籤的 `<`、提到 SQL 字詞的句子、反引號裡的名稱、中文與數字、撇號）、移除後留下的標記與整理、`find` 給串流用的範圍；`rewrite_text` 只改文字區塊、其他區塊逐位元組不變；提示詞要求回答不含程式碼並說明儀表板 |
