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
| `src/tools/dashboard.rs` | 工具 `dashboard`（Laysi/trustiot-v2#150）：列出、讀取、寫入儀表板；寫入前執行資料集與檢查頁面，問題是封閉的類別與固定說明 |
| `src/answer_guard.rs` | 回答的過濾：移除回答裡的 fenced code、含程式碼的 inline code、HTML 與 SQL，回報移除的類別；`find` 與 `pending` 給串流（Laysi/trustiot-v2#147）用 |
| `src/callee.rs` | 工具與儀表板的資料集執行以使用者身分呼叫其他服務的 HTTP client（GET 與 JSON 本文的 POST）：每次呼叫換 token、錯誤只看狀態碼（`telemetry_sql` 的 422 例外，嚴格解析成封閉的拒絕型別）；`Caller` 是呼叫代表誰、日誌記成哪個工具或資料集執行 |
| `src/delegation.rs` | 一次提問的 run 委派：`QuestionRuns::start`（聊天 API 的入口）、`QuestionRun`（截止時間、撤銷）；`OnBehalfOf` 以公開 API 請求的身分換 token（資料集執行） |
| `src/api.rs` | 公開 API（`/api/assistant/v1`）的共用部分：每個請求驗證內部身分、`cache-control: no-store`、M1 錯誤、嚴格的請求本文；`openapi()` 的文件 |
| `src/api/dashboards.rs` | 儀表板的路由：列表、開啟、版本、發布、還原、封存、執行資料集 |
| `src/dashboards/store.rs` | 儀表板與版本的 Postgres 儲存：一個函式一個完整操作、鎖、誰看得到與誰能改（草稿的可見性只由 `conversation_access` 決定） |
| `src/dashboards/runner.rs` | 資料集的執行：以呼叫者的身分呼叫 telemetry-query，回答與錯誤對應成封閉的結果 |
| `src/dashboards/caps.rs` | 資料集執行的上限：每個租戶、每個副本每分鐘 60 次、同時 2 個 |
| `src/dashboards/flatten.rs` | 語意資料集（統計、用量）的回答攤平成表格 |
| `src/dashboards/checker.rs` | 檢查服務（services/dashboard-check）的 client：每個副本同時最多 2 次檢查、送出檢查、收不到可用判定的每種情況對應成封閉的原因 |
| `src/dashboards/limits.rs` | 每個鍵（租戶）的名額與有上限的等待：`dashboard` 工具每個租戶同時一次寫入；公開 API 的發布與還原每個使用者、每個租戶每分鐘的次數（`VersionBudget`） |
| `src/budget.rs` | 每個問題的 token 預算 |
| `src/prompt.rs` | 平台 system prompt（有版本）與對話中的固定指示 |
| `src/config.rs` | 設定與啟動檢查 |
| `src/store.rs` | 對話與歷史的 Postgres 儲存：只往後加、原樣保存、依租戶隔離 |
| `src/schema.rs` | 本版本預期的 migration 檔（`migrations/postgres/`：`0001_assistant.sql` 對話、`0002_dashboards.sql` 儀表板），`tools/migrate` 也編進它 |

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

主 agent 以一個工具、三個動作做儀表板（`src/tools/dashboard.rs`，設計紀錄「主 agent 的 `dashboard` 工具」），不發布、不還原、不封存：那些是使用者在公開 API 做的。

**寫入**（`write`）只改輸入寫到的部分：`datasets`、`computations`、`constants` 各是 `{upsert: [...], remove: [名稱]}`（先移除、再依名稱取代或加在最後），`page` 換掉整個頁面，`page_edits` 是 `[{old, new}]`，每個 `old` 在套用前面的修改後的頁面裡要正好出現一次；`title`、`description`（`null` 移除）、`locale`（`zh-TW` 預設或 `en`）、`tz`（預設 `UTC`）是檢查呈現頁面的方式。依序：

1. **驗證**：輸入嚴格解析（多一個欄位就整個拒絕，例如 `tenant_id`）；合併後的版本以 contract 的 `validate_version` 檢查（上限、名稱、計算語言的語法與參照，spec 加頁面最多 56 KiB、頁面最多 40 KiB、最多 8 個資料集），`tz` 依檢查服務的形式，標題、說明、頁面不能有 U+0000。
2. **授權**：沒有 `dashboard_id` 是這個對話的新草稿；這個對話的草稿要給 `base_version`，而且要是最新版；已儲存的儀表板（未封存）以讀到的版本成為這個對話的新草稿，一個對話對同一個已儲存的儀表板只有一份草稿（再寫時告訴模型那份草稿的 ID）；`rebase_saved_version` 只用於取自已儲存儀表板的草稿（合併了它的新版本，之後發布不再衝突），而且只接受這次提問讀過（`read`）的那個已儲存儀表板的版本：每個問題記得它讀過的已儲存版本（最近 64 個，`MAX_SAVED_READS`）到截止時間，沒讀過的回固定的說明（請它先讀那一版、合併後再寫），不執行任何東西、不算進寫入次數。store 讓使用者看得到自己所有對話的草稿，工具只認這個對話的：別的對話、別的使用者與別的租戶的草稿都是「不存在」。
3. **輪到這個租戶**：每個租戶在每個副本同時只有一次寫入走第 4 到 7 步；另一次最多等 30 秒（`WRITE_WAIT`），等不到回 `check_busy`，不執行任何東西、不算進問題的寫入次數。所以一個租戶同時最多一次檢查，也只有一次寫入拿著資料集結果。
4. **執行資料集**：以提問者的身分、這次提問的委派（`ToolContext::caller("dashboard")`）經與公開 API 共用的 `DatasetRunner` 執行（同一組每租戶上限），一次兩個，一個跑完就接下一個。從呼叫開始算 120 秒（含讀取要改的儀表板）還沒跑完的資料集各是一個 `dataset_timeout` 問題，跑完的結果留給同一個問題的下一次寫入，那次只執行沒跑完的。同一個問題裡同一個資料集（來源加 `max_rows`）只執行一次，之後的寫入重用結果；結果保留到問題的截止時間，所有問題合計最多 64 MiB，滿了時最久沒寫的問題的結果先讓出。委派被撤銷時提問結束（`ToolError::Revoked`）。
5. **計算**：公式以資料集實際回來的欄位再檢查一次（`Spec::check_formulas`）。
6. **檢查**：送 `CheckRequest`（runtime 版本、頁面、公開的 spec（沒有 SQL 與語意請求）、資料集結果、locale、tz）到檢查服務（`dashboards::checker`），判定經 `CheckVerdict::validate` 確認每個參照、值與標籤都來自送出的資料。每個副本同時最多 2 次檢查（`MAX_CHECKS_IN_FLIGHT`：檢查服務執行一個、佇列一個），從組請求本文到確認判定都佔著名額，依先後取得；等 30 秒（`CHECK_QUEUE_WAIT`）沒有名額時回 `check_busy`，不呼叫服務。
7. **寫入**：只有判定通過才在問題租戶的交易裡寫一版（`create_draft`、`write_draft` 或 `fork`），寫入者是對話的擁有者與這次的 run ID。

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
| `check_busy`、`check_unreachable`、`check_timeout`、`check_refused`、`check_failed`、`check_invalid_verdict` | 沒有可用的判定：這個租戶的另一次寫入或這個副本的檢查名額等不到、503、連不上、85 秒內沒有判定、400（或請求本身不合 contract）、其他狀態、不是判定或判定不符送出的資料 |

**上限**：一個問題最多 6 次到達資料集執行的寫入（驗證不過的不算），第 7 次在執行任何東西前拒絕。每一段有自己的時限，到了就以模型能處理的結果結束：等這個租戶的另一次寫入 30 秒、資料集 120 秒（含讀取，不含前面的等待）、檢查名額 30 秒加檢查 85 秒、存檔 30 秒（逾時時交易回滾、回固定的失敗）；一次呼叫的時限是這些加 10 秒，305 秒（`Tool::call_timeout`，其他工具仍是 `tools.call_timeout`），只在各段沒依時限結束時才用到。**每個副本**計的名額：每個租戶同時一次寫入、所有租戶同時 2 次檢查；N 個副本時一個租戶同時最多 N 次檢查。不另設每租戶每分鐘的檢查次數：每次檢查都用掉問題 6 次寫入之一（每次寫入是一次模型呼叫，計入 AI 額度與預算），一個租戶同時最多佔一個名額、名額依先後給，而每分鐘的上限會在檢查服務閒置時也拒絕。仍在提問的截止時間內，問題剩不到 30 秒時不開始寫入。`read` 的 spec 與頁面完整給模型，所以設定了 `dashboard_check.url` 時 `tools.max_text_bytes` 至少要 64 KiB。工具的拒絕（`ToolError::Input`）是固定說明：不存在的儀表板、舊的 `base_version`（說最新是第幾版）、沒給 `base_version`、封存了、已有草稿、第 7 次寫入、時間不夠。

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

草稿記得它取自或發布到哪個已儲存的儀表板；草稿的每個版本記得它的內容以那個儀表板的哪一版為基礎（見下方「發布的衝突」）。公開 API 沒有寫內容的路由：沒有任何請求本文有 SQL、語意查詢或頁面的欄位，瀏覽器寫不了會被執行的東西。

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
- **發布與還原的次數**（`dashboards::limits::VersionBudget`）：版本只往後加、不會刪除，所以每個使用者在任何一分鐘最多以發布與還原加 10 個版本；同一個租戶的使用者合計最多加 30 個到已儲存的儀表板，另外最多 30 個到草稿。超過時回 429 `rate_limited`、`dashboard.versions_limited`，帶 `Retry-After`（整數秒，至少 1，等到使用者與租戶都有空位）與 `details.retry_after`，記一行 INFO（`refused a dashboard version: ...`，只有使用者、租戶、動作與秒數），什麼都不寫。store 在交易裡做完其他每一項檢查、正要寫入時才計數（`admit`），所以因為身分被拒（viewer 發布、別人的草稿）或因為儀表板的狀態被拒（衝突、封存、沒有改變、版本數到上限）的請求都不算，用不掉別人共用的上限；被這個上限拒絕的也不算。已計數而交易之後失敗的仍算。草稿與已儲存的儀表板分開計，所以只能改自己草稿的 viewer 用不掉 editor 發布與還原的上限。上限以**每個副本**計：N 個副本時一個使用者每分鐘最多 N × 10 個版本。
- **發布的衝突**：發布草稿的一版時，比較的是那一版自己的基礎版本（寫入時記在版本上，之後不變），不是草稿目前的：已儲存的儀表板的最新版不是它就回 409 `conflict`、`dashboard.saved_changed`。所以草稿合併了新版本之後，合併前的舊版本仍不能發布；還原草稿的舊版本時，新的一版帶著舊版本的基礎版本，草稿接下來的版本也從那裡開始（草稿的 `saved.version` 跟著降）。發布草稿的最新版之後，草稿接下來的版本以發布出去的那一版為基礎，已發布的那一版本身不再發布一次；發布較舊的版本時草稿的基礎版本不變。草稿第一次發布之前寫的版本不以任何已儲存的版本為基礎：已儲存的儀表板建立之後，它們（包括發布出去的那一版）與從它們還原的版本都不能再發布；第一次發布的不是最新版時，草稿接下來的版本也不以任何版本為基礎（`saved` 沒有 `version`）。這時要請 AI 助理把已儲存儀表板的最新版併入草稿，再發布新的一版。
- **回到相同的內容**：還原的版本與最新版的內容（runtime、標題、說明、spec、頁面）逐位元組相同時回 409 `conflict`、`dashboard.restore_unchanged`，不加版本；與其他「和目前狀態衝突」的情況一樣是 409。
- **版本數各自計算**：版本號與版本數以儀表板計（主鍵是租戶、儀表板、版本號）。草稿是自己的一個儀表板，草稿的還原只加草稿的版本；已儲存的儀表板只由 editor 以上的發布與還原加版本，viewer 對自己草稿做的事用不掉它的版本。
- **10,000 個版本的上限**：一個儀表板最多 10,000 個版本（資料表的 CHECK 與 `store::MAX_VERSIONS`），到了之後發布與還原都回 409 `conflict`、`dashboard.too_many_versions`，什麼都不寫；已有的版本照常讀、照常執行資料集。editor 可以請 AI 助理以它的最新版本在對話裡做一個新的儀表板（新草稿），發布成新的已儲存儀表板，再封存舊的。以每個使用者每分鐘 10 次計，一個副本時一個人要連續 16 小時以上才到得了上限。

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
| 發布時已儲存的儀表板在這個草稿版本的基礎版本之後有新版本 | 409 `conflict`、`dashboard.saved_changed`，`details.latest` |
| 已封存的儀表板加版本（發布、還原） | 409 `conflict`、`dashboard.archived` |
| 還原的版本與最新版的內容相同 | 409 `conflict`、`dashboard.restore_unchanged` |
| 儀表板已有 10,000 個版本（發布、還原） | 409 `conflict`、`dashboard.too_many_versions` |
| 同一個儀表板的兩個寫入同時進行 | 409 `conflict` |
| 使用者在一分鐘內已以發布與還原加了 10 個版本，或租戶合計已加 30 個到已儲存的儀表板（草稿另計 30 個）（每個副本） | 429 `rate_limited`、`dashboard.versions_limited`，`Retry-After` 與 `details.retry_after`；只在其他檢查都通過時才計數 |
| 發布已儲存的儀表板、封存草稿 | 422 `validation_failed`、`dashboard.draft_required`、`dashboard.saved_required` |
| 資料集執行的錯誤 | 見「資料集的執行」 |
| 資料庫失敗 | 503 `unavailable`，什麼都沒寫；日誌只有 SQLSTATE 或錯誤的種類 |

### 儲存

`migrations/postgres/0002_dashboards.sql`，與對話同一個 schema，由 migration Job 套用。兩張表都是租戶資料，依決策 29 啟用 RLS（只隔開租戶；租戶內誰看得到什麼由服務決定，見下方）：

| 表 | 一列是 | 內容 |
| --- | --- | --- |
| `assistant.dashboards` | 一個儀表板 | 種類（`draft`、`saved`）、草稿的對話、草稿的已儲存儀表板與下一版從它的哪一版開始、建立者、建立時間、已儲存儀表板的封存時間 |
| `assistant.dashboard_versions` | 一個版本 | 版本號（從 1 起、沒有空洞，外鍵指向前一版）、來源（`agent`、`publish`、`restore`，還原時記下還原的版本）、草稿版本的基礎版本（已儲存儀表板的哪一版；已儲存儀表板的版本與草稿第一次發布前的版本沒有）、沙盒 runtime 版本、標題、說明、spec（服務寫的精簡 JSON 文字，讀回逐位元組相同）、頁面、要求的使用者、工具寫入時的 run ID、建立時間 |

- **權限**：服務角色 `assistant_app` 對兩張表有 SELECT 與指定欄位的 INSERT（時間由資料庫的時鐘決定）；`assistant.dashboards` 另外只能 UPDATE 草稿的已儲存儀表板、下一版的基礎版本與封存時間。沒有 DELETE 與 TRUNCATE。觸發器對每個角色（包括 owner）再限制：草稿的已儲存儀表板只設一次、封存時間是資料庫的時鐘（再封存一次保留原本的時間）、其他欄位不變；版本寫入後不能改，包括它的基礎版本（`42501`，`tests/dashboards_store.rs`）。草稿的基礎版本可以降或變成沒有（還原舊版本時），發布比較的是版本上不會變的那個。沒有已儲存儀表板的草稿與已發布的版本（`publish`）都不能有基礎版本（CHECK）。
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
| `tools.call_timeout` | `TRUSTIOT__TOOLS__CALL_TIMEOUT` | `60s` | 一次工具呼叫的上限，也是一次資料集呼叫 telemetry-query 的上限；`dashboard` 工具自己的上限是 305 秒（見「`dashboard` 工具」） |
| `tools.max_calls_per_step` | `TRUSTIOT__TOOLS__MAX_CALLS_PER_STEP` | `8` | 一次模型回應裡同時執行的工具呼叫數；之後的呼叫回錯誤、不執行；1 到 32 |
| `tools.max_text_bytes` | `TRUSTIOT__TOOLS__MAX_TEXT_BYTES` | 64 KiB | 一個工具結果的文字上限；超過時改回錯誤，告訴模型怎麼縮小範圍。設了 `dashboard_check.url` 時至少 64 KiB（`read` 給完整的儀表板） |
| `tools.max_images` | `TRUSTIOT__TOOLS__MAX_IMAGES` | `4` | 一個工具結果的圖片數上限；不超過 20 |
| `tools.max_image_bytes` | `TRUSTIOT__TOOLS__MAX_IMAGE_BYTES` | 5 MiB | 一張圖（base64 文字）的上限；不超過 5 MiB |
| `identity.exchange_url` | `TRUSTIOT__IDENTITY__EXCHANGE_URL` | 無 | 身分服務 token 交換埠的 `http://<host>[:<port>]`（`http://<release>-identity-exchange:8082`），不能有路徑、帳密或 query；沒有就沒有工具，儀表板的資料集執行也都回 503。呼叫身分服務的時間上限是 15 秒（`IDENTITY_TIMEOUT`） |
| `identity.run_lifetime` | `TRUSTIOT__IDENTITY__RUN_LIFETIME` | `15m` | 一次提問的委派效期；整數秒，2 到 15 分鐘（身分服務的上限 900 秒）。提問在到期前 30 秒結束 |
| `telemetry_query.url` | `TRUSTIOT__TELEMETRY_QUERY__URL` | 無 | telemetry-query 的 `http://<host>[:<port>]`；沒有就沒有 `telemetry_latest`、`telemetry_statistics`、`telemetry_usage`、`telemetry_sql`，儀表板的資料集執行也都回 503。設了就要有 `identity.exchange_url` |
| `assets.url` | `TRUSTIOT__ASSETS__URL` | 無 | assets 的 `http://<host>[:<port>]`；沒有就沒有 `device_models`。設了就要有 `identity.exchange_url` |
| `dashboard_check.url` | `TRUSTIOT__DASHBOARD_CHECK__URL` | 無 | 檢查服務的 `http://<host>[:<port>]`（chart 設成 `http://<release>-dashboard-check:8080`）；沒有就沒有 `dashboard` 工具。以 assistant 自己的身分呼叫（不帶使用者的 token），一次檢查最多 85 秒（服務的佇列等待 `queue.max_wait` 預設 10 秒、最多可設 60 秒，檢查 15 秒、清理 5 秒，加上餘裕），判定最多 4 MiB |

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
| `refused a dashboard version: the user or tenant used the minute's budget` | INFO | `user_id`、`tenant_id`、`action`（`publish`、`restore`）、`retry_after`（秒）；公開 API 的 429 `dashboard.versions_limited` |
| `refused: no tenant or no known role in it` | INFO | `user_id`、`action`；公開 API 的 403 |
| `could not …`（例如 `could not publish a dashboard`、`could not open a transaction for an API request`） | ERROR | `user_id`、`dashboard_id`（有的時候）、`error_kind`（SQLSTATE 或錯誤的種類，沒有資料庫的文字）；公開 API 的 503 與 500 |

不記問題、回答、工具輸入與結果、錯誤訊息、儀表板的標題、SQL 或頁面，也不記被呼叫服務的 URL、查詢參數、回應本文與 token（只記狀態碼的類別）。欄位名稱都不會被 service-kit 的遮蔽規則擋掉（不用 `token`、`tokens`、`prompt`、`content` 這些字）。指標 `assistant.model_calls` 依 `outcome` 與 `cache`（`hit` 或 `miss`）計數；`cache_read` 長期為 0（只有 `miss`）表示快取失效，要接到監控（Laysi/trustiot-v2#554 以真實 API 驗證快取）。

## 之後接上的地方

| 項目 | 介接點 |
| --- | --- |
| 聊天 API 與串流（Laysi/trustiot-v2#146） | `Agent::start_conversation`、`Agent::ask` |
| 回答被過濾的標示（Laysi/trustiot-v2#645、#646） | `Answer::removed`（移除的類別，空集合時沒有移除）隨回答回給前端，畫面顯示 `answer_guard::CONTENT_REMOVED`（`assistant.answer_content_removed`）。歷史存的是過濾後的文字，所以重新載入對話時要再顯示標示，就要把 `removed` 也存下來（例如回答訊息旁的欄位）：由這兩個 issue 決定欄位與 migration |
| 串流的過濾（Laysi/trustiot-v2#147） | `answer_guard::find`：對目前為止的文字執行，暫緩送出從第一個延伸到結尾的發現處開始的文字；回答結束時以 `guard` 的結果為準 |
| 聊天 API 開始一個問題（Laysi/trustiot-v2#146） | 處理請求時以 `delegation::QuestionRuns::start(&UserIdentity, conversation.principal())` 取得 `QuestionRun`，交給 `Agent::ask`；`Assistant::runs` 為 `None`（沒有設定 `identity.exchange_url`）時改用 `tools::NoDelegation`。`StartError` 已實作 `IntoResponse`（見「代表使用者呼叫」） |
| 更多工具 | 實作 `tools::Tool`，放進 `ToolRegistry`（最多 10 個），以 `callee::Callee` 呼叫服務；工具從 `ToolContext.principal` 取租戶與使用者，不從輸入取 |
| 對話存進 Postgres（Laysi/trustiot-v2#553 已完成儲存，連線池由 Laysi/trustiot-v2#592 接上） | `store::create`、`store::append`、`store::load`，在對話租戶的 `TenantTransaction` 裡呼叫，交易取自 `start` 建立的 `TenantDb`（現在給 readiness 與儀表板 API 用），見[對話的儲存](#對話的儲存postgres) |
| 對話的軟刪除（Laysi/trustiot-v2#644） | `dashboards::store` 的 `CONVERSATION_OWNER` 加上 `AND deleted_at IS NULL`，寫入時以 `FOR SHARE` 鎖對話；拿掉 `tests/dashboards_store.rs` 那個測試的 `#[ignore]` |
| 對話共享的讀者（Laysi/trustiot-v2#650） | `dashboards::store::conversation_access` 回讀者的存取（新的 `Access`），`store::permissions` 決定讀者能做什麼 |
| 每次呼叫預扣額度（Laysi/trustiot-v2#140） | 每次呼叫的 `ModelRequest::call_id`（帳本的 `llm_call_id`，現在只記在日誌）；閘道拒絕時以 `ModelError` 回到迴圈 |

## 測試

`cargo test -p assistant`（`tests/history_store.rs`、`tests/startup.rs`、`tests/dashboards_store.rs`、`tests/dashboards_api.rs` 與 `tests/cross_tenant.rs` 以 podman 或 docker 起一個拋棄式 TLS Postgres，以 migration Job 套用整個產品的 schema；沒有容器執行環境時在本機略過，設了 `CI` 時失敗）。`tests/dashboard_acceptance.rs` 的兩個驗收測試預設 `#[ignore]`，因為要檢查服務的映像檔：`bun test deploy/images/dashboard-check` 建出 `localhost/trustiot-e2e/dashboard-check:image-test` 之後，`DASHBOARD_CHECK_IMAGE=<映像檔> cargo test -p assistant --test dashboard_acceptance -- --ignored`（加 `--nocapture` 印出每個比對的值）；以 docker（沒有時 podman）起檢查服務、Postgres 與 ClickHouse，一次跑一個，沒有映像檔或容器執行環境時略過，設了 `CI` 時失敗。CI 目前不跑這兩個測試。測試以本機 loopback 上的替身 AI 閘道回放 `tests/fixtures/` 的回應（取自官方文件，見該目錄的 README），不呼叫供應商，也沒有 API key。

| 檔案 | 內容 |
| --- | --- |
| `tests/agent.rs` | 60 步的長分析不受步數限制；接近預算時收尾並回答；放不進剩餘預算的呼叫不送出、輸出上限降到剩下的空間；用量超過估計而預算用完時工具不執行；第二個問題的請求逐位元組以前一次請求的歷史開頭（thinking 與 signature、system、工具定義不變，租戶脈絡包在標籤裡）；兩個使用者同時發問互不等待；模型逾時與閘道錯誤只給固定訊息、日誌沒有錯誤文字；拒答不留在歷史；每一步的時間與快取讀取都記在日誌 |
| `tests/tools.rs` | 工具組規則（最多 10 個、名稱、schema）；並行執行、結果依呼叫順序；一步超過呼叫數上限的呼叫不執行；輸入錯誤、失敗、panic、逾時、不存在的工具各自回錯誤給模型；結果超過大小與圖片上限時請模型縮小範圍；工具拿到的身分來自對話；圖片照 Anthropic 格式送給模型 |
| `tests/delegated_tools.rs` | 四個呼叫服務的工具各以使用者身分、受眾正確且在呼叫當下交換的 token 呼叫服務，請求裡沒有租戶或使用者，超過 60 秒後的呼叫仍成功、舊 token 會被拒（替身以「每次模型呼叫過 61 秒」的虛擬時鐘模擬，不真的等）；服務回 403、404、422、503 與非 JSON 時模型與日誌都看不到回應本文；身分服務拒絕交換（撤銷）時該步停止、問題結束、不再重試；身分服務暫時無法使用只讓該次呼叫失敗；問題在委派到期前結束、沒有剩餘時間的問題不開始；只有對話的使用者能開始問題，為別人的對話建立的 run 在送出任何東西前被拒；裝置寫的長文字截到 256 個字元、加上截斷標記才給模型；工具輸入不收租戶、使用者與多餘欄位；工具的路徑與上限跟 telemetry-query、assets 的程式一致；語意工具以 POST 送出的本文是 API 的 `SeriesQuery`（替身照服務的型別解析、拒絕未知欄位），輸入 schema 與 `telemetry_query::openapi()` 的 `SeriesQuery` 一致；列數超過服務上限時不呼叫 |
| `tests/history_store.rs` | 主 agent 跑完的對話（thinking 與 signature、工具呼叫與結果）存進去、讀回來逐位元組相同，載入的對話接著問時請求的前段與歷史和原本的逐位元組相同；服務角色的 UPDATE、DELETE、TRUNCATE 在租戶交易內外都被資料庫拒絕（`42501`）；重複寫入不改變什麼，兩個寫入者分岔時後者被拒、不覆寫，空洞與沒建立的對話被拒；其他租戶與同租戶的其他使用者載入不到；損壞的列讓載入失敗 |
| `tests/startup.rs` | 執行檔本身：以 `assistant_app` 登入、讀到身分服務的公鑰檔、資料庫有這一版的 migration 時 `/readyz` 回 200，日誌記下工具數（設了 `dashboard_check.url` 時六個）與可以執行資料集且不含密碼；密碼錯誤時 readiness 記下失敗、回 503；少了一個 migration 檔案時日誌寫出缺的檔案、回 503，補回後恢復 |
| `tests/chart_connections.rs` | chart 的 Postgres 連線預算（`trustiot.postgres.connectionBudgets`）與程式開的連線池一致：沒有其他連線池，`database.maxConnections` 的範圍與服務接受的相同；程式多了一個連線池時失敗 |
| `tests/cross_tenant.rs` | 跨租戶負向測試（決策 29，`crates/testkit`）：對話、訊息、儀表板與版本四張表的讀、寫、搬移、插入探測與覆蓋檢查；對執行檔的每個儀表板端點（發布與對話的草稿列表除外，原因寫在豁免裡，由 `tests/dashboards_api.rs` 手寫測試），資料集執行的正向對照經替身的 token 交換與 telemetry-query 真的跑完，沒有呼叫其他服務 |
| `tests/dashboards_store.rs` | `dashboards::store` 對真的 schema：版本只往後加、舊版本讀得回來；發布第一次建立已儲存的儀表板、之後加版本，已儲存的儀表板在發布的版本的基礎版本之後有新版本或已封存時拒絕；誰可以發布、還原、封存；工具只寫自己對話的草稿（別的對話與別的使用者都是不存在）；列表分頁與只列看得到的；服務角色改不了寫過的東西（觸發器與權限，`42501`，包括版本的基礎版本；草稿的基礎版本可以降或清掉）、沒有已儲存儀表板的草稿與已發布的版本不能有基礎版本、留不下空洞；清除對話時刪掉草稿、留下已發布的；同一個草稿同時寫入時排隊、後者衝突；軟刪除的對話（等 Laysi/trustiot-v2#644，`#[ignore]`） |
| `tests/dashboards_api.rs` | 公開 API 對真的 schema 與替身的 token 交換、telemetry-query：沒有 token、偽造的 token、沒有租戶時每個路由都拒絕；本文、query string 與 content type 不對時 400 而且什麼都沒寫（含 SQL、頁面等欄位、陣列寫法）；回應都是 `no-store`；請求的角色決定誰能發布、還原、封存（同一個使用者升降級後立即生效）；同租戶的其他使用者與其他租戶的 admin 對別人的草稿每個路由都是 404，與不存在的相同；版本、還原、發布衝突與封存；發布比較那一版自己的基礎版本：合併了新版本之後，合併前的版本不能發布到較新的已儲存版本上，還原舊版本帶回它的基礎版本（再發布與之後工具寫的版本都回 409），還原合併過的版本則能發布；第一次發布前寫的版本（包括發布出去的那一版）與從它們還原的版本不能再發布，之後工具寫的版本也一樣，直到合併已儲存的最新版；發布與還原每個使用者每分鐘第 11 個版本、每個租戶的草稿第 31 個版本回 429 與 `Retry-After`、什麼都沒寫，已儲存的儀表板另計（草稿用完時仍能發布）；被 store 拒絕的請求（舊的基礎版本、viewer 的 40 次發布、不存在的版本）不算，其他租戶照常；回到與最新版內容相同的版本回 409、不加版本，草稿的還原不加已儲存儀表板的版本；10,000 個版本的儀表板不能再還原或發布、回 409 與原因；資料集以打開的 viewer 身分執行（token 的使用者、租戶、角色、受眾與 `act`），請求本文原樣、SQL 的列數上限 2,000；超過列數時拒絕並附上限；第 61 次執行與第三個同時執行回 429 而其他租戶照常、`dashboard` 工具的執行算進同一個上限；telemetry-query 的錯誤只以封閉的 key 回給使用者；沒有 token 交換時 503；打開儀表板並執行全部資料集不呼叫模型、日誌沒有標題、SQL 或頁面 |
| `tests/dashboard_tool.rs` | `dashboard` 工具對真的 schema、替身的 token 交換、telemetry-query 與檢查服務：寫入以提問的使用者身分（委派、受眾、`act`）執行每個資料集，把頁面、公開的 spec（沒有 SQL 與語意請求）與結果送去檢查，通過後才存成這個對話的草稿，`list` 與 `read` 看得到（頁面整份）；檢查不通過（數值對不上、沒對應的圖形、較新的問題種類）與每種拿不到判定的情況（503、400、500、不是判定、引用了頁面沒有的資料集、值不在資料裡（不通過與通過的判定都是）、連不上、逾時）都什麼都不存、草稿的最新版本不變，模型與日誌都看不到檢查服務的本文；模型從判定只拿到封閉的值與工具自己的資料：script 錯誤的類別而不是訊息、刻度只給軸、超出結果的列只給自己的列數、頁面文字沒有的數字不給、`too_large` 的上限是 contract 的，通過時標記的值取自自己的資料、不含刻度；別的對話、別的使用者、別的租戶的草稿都是不存在，`base_version` 舊了或沒給、輸入格式不對、`page` 與 `page_edits` 同時給、頁面過大、資料集太多各自回固定的問題，第 7 次寫入被拒、下一個問題重新計算；已儲存的儀表板只變成這個對話的一個草稿（第二次指出既有的草稿）、封存的不能改；`rebase_saved_version` 只接受這個問題讀過的已儲存版本（讀了別的版本、別的問題讀過或只讀了草稿都拒絕，不送檢查），讀過之後寫入成功、草稿以那一版為基礎；telemetry-query 拒絕 SQL 時回 `telemetry_sql` 的類別說明，拒絕本文多了欄位時只回通用說明；撤銷的工作階段結束整個呼叫；剩下的時間不夠時不開始；同一個問題的同一個資料集只執行一次；資料集在寫入的時限內（測試設 3 秒）沒跑完時各回 `dataset_timeout`、不送檢查，跑完的留給同一個問題的下一次寫入、那次只執行沒跑完的；工具自己的呼叫時限大於資料集、檢查與存檔時限的總和；一個租戶的第二次寫入在第一次檢查時等到測試設的時限後回 `check_busy`，沒執行資料集、沒送檢查、不算進寫入次數，同時別的租戶的寫入照常存下；檢查名額設成 1 時別的租戶的檢查也等到時限後回 `check_busy` |
| `tests/dashboard_acceptance.rs` | 跨服務的驗收（Laysi/trustiot-v2#150），經 `ask` 與照腳本回答的替身 AI 閘道，`dashboard` 工具呼叫以映像檔跑的真正檢查服務（chart 的限制：唯讀根目錄、chart 大小的記憶體 /tmp、拿掉所有 capability、no-new-privileges、預設 seccomp、chart 的記憶體上限、UID 65532），中間的 loopback 代理記下服務送出的每個判定。驗收 2：沙箱 runtime 的固定樣本（`frontend/sandbox/fixtures`）與照樣本回答的 telemetry-query 替身，模型先寫好的頁面（第 1 版），再寫一根長條只畫一半高的頁面（`mark_geometry`）與標記了不存在的列、資料集和沒對應圖形的頁面（`unmapped`），三個判定都符合 cases.json 的案例，模型收到問題，公開 API 看到的仍是第 1 版與好的頁面，資料集在問題裡只執行一次。驗收 1：合成 lab 資料集（`datasets/lab/synthetic`）載入拋棄式 ClickHouse，telemetry-query 的路由在測試程序裡以產品的 schema 執行資料集，檢查報告裡的每個值都等於以讀全部列的測試帳號直接查 `trustiot.telemetry` 算出的同一個量：原始讀數 view 的每部電表筆數、最低、最高與總和，1 小時格 view 的每小時最高與平均，統計 API 的筆數、最小、最大、平均與最後讀數，用量 API 的總量（清零一次的電能、32 位元回捲的脈衝）與讀數數，以及 `max`、`sum`、跨欄 `max − min`、索引與算術、`last` 的計算 |
| `tests/check_stand_in/` | 檢查服務的替身：記下每個請求，預設回報告每個資料集列數的通過判定，測試可以改成任何狀態與本文 |
| `tests/answer_guard.rs` | 經 `ask` 與替身 AI 閘道：回答裡的 SQL 程式碼區塊、句子中的 SQL 敘述、HTML 與含程式碼的行內程式碼，到呼叫端與歷史時都已移除，回報移除了哪幾類、日誌只記類別不記內容，thinking 區塊逐位元組不變，下一個問題送回的歷史就是過濾後存下的那一則；改變 schema 的敘述（`CREATE OR REPLACE VIEW`、`CREATE TABLE`、`DROP TABLE`、`ALTER TABLE`、`TRUNCATE TABLE`）從回答中移除；放在 `tests/`，因為 `cargo test -p repo-check` 不允許 `src/` 出現這些文字 |
| `tests/dashboard_world/` | 儀表板測試共用的世界：產品的 Postgres、測試用的身分簽發者、替身的身分服務 token 交換與 telemetry-query（記下每個呼叫、其他路徑都算意外）、啟動執行檔、問題的委派（每次呼叫簽發 `act` 是 assistant 的 token）與照服務組起來的 `dashboard` 工具 |
| `tests/telemetry_sql.rs` | `telemetry_sql` 經工具組呼叫：對 telemetry-query 真正的路由（Postgres 與 ClickHouse 是計數的 loopback 埠），系統表、table function、白名單外的函式、`SET ROLE`、`SET DEFAULT ROLE`、多個敘述、`SETTINGS`、CAST、沒寫時區、沒讀 view、引號識別字、反斜線、語法錯誤與太深的巢狀各自回固定的類別說明，兩個資料庫都沒有連線，日誌沒有 SQL；對替身，答案（長文字截斷）、執行後的拒絕、多欄位或不認得的類別與名稱、404、429、504、403、400、5xx 與非 JSON 各自回固定訊息，模型與日誌都看不到服務的本文；每次呼叫換新 token、請求正好是 SQL 加 200 列 |
| `src/` 的單元測試 | 設定檢查（含被呼叫服務的位址、委派時長與資料庫帳密、連線池上限）、截止時間的計算、`StartError` 的錯誤碼、字串上限、時間區間的粒度選擇與查詢範圍、狀態碼類別、422 拒絕本文的嚴格解析、`telemetry_sql` 每個拒絕類別的說明與名稱的再檢查、預算門檻、租戶脈絡的跳脫、狀態碼對應、錯誤 `type` 過濾、沒有用量時的估計、送出前的訊息估計（圖片按張數）、工具定義與訊息讀回的位元組檢查；儀表板：執行上限（第 61 次、第三個同時執行、等到空位、`Retry-After` 的秒數）、每個鍵的名額與等待、發布與還原每分鐘的版本數（每個使用者、每個租戶的已儲存儀表板與草稿各自計、等到兩者都有空位的秒數、清掉閒置的使用者與租戶）、語意回答的攤平、telemetry-query 每種回答對應的錯誤、權限表、分頁 cursor；`dashboard` 工具的輸入解析、問題的寫入上限與資料集快取的預算、判定的問題與標記怎麼對應（錯誤類別、每個種類名稱與 contract 相同、標記只取解得出的參照）；回答過濾（程式碼區塊、含程式碼的行內程式碼、HTML 標籤與 script/style、各種 SQL 敘述，以及要留下的文字：不是標籤的 `<`、提到 SQL 字詞的句子、反引號裡的名稱、中文與數字、撇號）、移除後留下的標記與整理、`find` 給串流用的範圍；`rewrite_text` 只改文字區塊、其他區塊逐位元組不變；提示詞要求回答不含程式碼並說明儀表板 |
