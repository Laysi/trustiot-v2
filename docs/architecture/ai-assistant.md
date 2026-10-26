# AI 助理與 AI 閘道

2026-10-18 定案（決策 15、26、28、31、32）：主 agent 加工具，AI 閘道照各供應商的原生協定轉送，影片與聲音交給 Gemini。2026-10-20 加入儀表板（決策 33）：圖表一律做成儀表板，由使用者以自然語言叫出。先做 SaaS；地端排在 SaaS 全部完成之後，屆時要處理的事列在最後一節。文中的模型與價格是 2026-10-18 查證的值；模型由 platform admin 依模型角色更換（J8），各角色用哪個模型由題庫決定。

## 架構

```mermaid
flowchart TB
  user["使用者<br>文字、圖片、文件、影片、聲音"]
  subgraph assistant["AI 助理：services/assistant"]
    router["分流 router<br>是否為簡單問題、主 agent 的 effort"]
    worker["worker<br>只回答簡單問題，可 escalate 給主 agent"]
    main["主 agent main<br>一個模型、一個迴圈，自己決定呼叫哪些工具"]
  end
  subgraph tools["工具：由各服務執行，租戶範圍由服務注入"]
    telemetry["語意層查詢、唯讀 SQL<br>telemetry-query"]
    dash["儀表板：資料集與頁面<br>assistant，發布前由 dashboard-check 檢查"]
    assets["設備、型號、knowledge、assetNotes<br>assets"]
    write["寫入：設備上架、修改型號<br>先暫停等使用者確認"]
    docs["文件檢索：召回、重排、回傳頁面圖<br>knowledge"]
    media["影音：找片段、Gemini 重看片段<br>knowledge"]
    graph["知識圖 Cypher<br>field-context"]
  end
  gw["AI 閘道：services/ai-gateway<br>原生協定轉送、逾時與備援、帳本、額度"]
  providers["Anthropic Messages、Vertex AI（Gemini）、OpenAI Decisions"]
  user --> router
  router -->|"確定是簡單問題"| worker
  router -->|"其他或不確定"| main
  worker -->|"escalate"| main
  main --> telemetry & assets & write & docs & media & graph & dash
  router & worker & main & docs & media --> gw
  gw --> providers
```

## 模型角色

取代舊系統的 main、tiny、decision 三個角色（J8）：tiny 改名為 worker；decision（最終覆審，依查到的資料改寫草稿）不再是獨立角色，改由主 agent 在最終回答前自己核對數字（J5），名稱也不會與 OpenAI 的 Decisions API 混淆。

| 角色 | 用途 | SaaS 預設（2026-10-18） | 協定 | 里程碑 |
| --- | --- | --- | --- | --- |
| router | 分流：是否為簡單問題、主 agent 該用的 effort | OpenAI Decisions API（`gpt-6-luna`） | OpenAI Decisions | MS3 |
| main | 主 agent | Claude Opus 5.5；候選 Claude Sonnet 5.5 | Anthropic Messages | MS3 |
| worker | 簡單問題、子 agent | Claude Haiku 5.5 | Anthropic Messages | MS3 |
| complete | 輸入補全（J11） | 依 J11 實測的首字延遲選定 | 依選定的供應商 | MS3 |
| rerank | 文件頁面與影音片段的重排 | Decisions API 逐項打分，或 Haiku 5.5 一次排序（RAG 題庫決定） | OpenAI Decisions 或 Anthropic Messages | MS5 |
| embedding | 文件頁面與影音片段的向量 | Gemini Embedding 2 | Vertex AI | MS5 |
| media | 影音理解 | Gemini 3.8 Flash；困難問題用 Gemini 4 Argon | Vertex AI | MS5 |

## AI 閘道

內部 API、設定（供應商端點、key 的環境變數、角色與備援、時間上限）、錯誤與日誌見 [services/ai-gateway/README.md](../../services/ai-gateway/README.md)（2026-10-22，Laysi/trustiot-v2#139：Anthropic Messages 與 OpenAI Decisions）。

1. **照原生協定轉送，不轉換格式**（決策 15）：呼叫端（assistant、knowledge、alerts）以內部 API 指定模型角色，請求本文用該角色供應商的原生格式；閘道依角色填入模型與憑證、轉送到供應商，回應與串流原樣交回。SaaS 支援三種協定：Anthropic Messages、Vertex AI 的 Gemini（生成、嵌入、顯式快取）、OpenAI Decisions；地端的 OpenAI 相容端點在 SaaS 完成後加入。不採用相容層的理由：Anthropic 的 OpenAI 相容層官方定位不是給正式環境用的，不支援 prompt caching、用量的快取欄位永遠是空的、不回傳 thinking、`strict` 與 effort 被忽略，也用不到 tool search 與 context editing；Gemini 的相容端點仍在 beta，不能用 Files API，影片輸入沒有文件。LiteLLM、Envoy AI Gateway 等閘道支援快取，也是靠自己轉成原生協定。
2. **角色、備援與逾時**：platform admin 隨時可以更換角色的模型（J8），換成同一協定的模型不必改程式，換協定要呼叫端支援。chart 的 `ai.roles` 決定有哪些角色、各自的協定、上限與預設模型；更換只是模型清單的覆寫，存在 Postgres 的 schema `ai_gateway`，部署不會丟掉它，重設才回到預設模型；每次更換與重設在同一個交易寫進 append-only 的變更紀錄。每個轉送請求都確認設定世代，所以更換的呼叫回 200 之後才開始的請求，在任何副本都用新的模型；讀不到設定時以 503 拒絕，不呼叫供應商（2026-10-23，Laysi/trustiot-v2#537、Laysi/trustiot-v2#605；做法見 [services/ai-gateway/README.md](../../services/ai-gateway/README.md)「資料庫」與「多副本」，管理 API 在 Laysi/trustiot-v2#606）。每個角色設一串備援模型，429、過載或逾時時改用下一個（例如 main 從 Opus 5.5 換成 Sonnet 5.5）；快取依模型分開，換模型後從零開始。供應商的拒答（例如 Anthropic 的 `stop_reason: refusal`）照常記帳，交回呼叫端，由它回通用訊息（J2）。
3. **帳本**（決策 28）：以 llm\_call\_id 去重，依供應商回報的分類記錄用量：一般輸入、快取寫入（5 分鐘與 1 小時分開）、快取讀取、輸出（含 thinking），以及 Gemini 各模態的輸入（文字、圖片、影片、音訊）。單價表依這些類別定價，另外記 Gemini 顯式快取的儲存費（token 數乘以時數）；Decisions 只收輸入費；Embedding 2 依模態計價（文字 token、圖片張數、音訊秒數、影片格數）。供應商沒回報用量時（含串流中途中斷，J8），輸入與已產生的輸出（含 thinking）分別在本地估算並標記，絕不記 0：輸入用 `count_tokens` 或本地 tokenizer 計算，輸出依已收到的內容計算；thinking 只回傳摘要或不回傳時看不到實際長度，這筆估算標為下限。Claude 的 token 數以供應商回報或 `count_tokens` 為準，本地 tokenizer 會偏差。
4. **額度**：預扣與結算照決策 28；agent 迴圈裡的每一次模型呼叫各自預扣、各自結算。
5. **憑證**（[密鑰與權限](secrets.md)）：Anthropic 與 OpenAI 用 API key；Vertex AI 優先用 Workload Identity Federation，不放長期金鑰（密鑰規則 8），不可行時才用服務帳號金鑰，在接入 Vertex AI 的 issue（Laysi/trustiot-v2#440）決定。都只給 `ai-gateway`。
6. **資料處理者**：Anthropic（主 agent、worker）、Google Vertex AI（影音、嵌入）、OpenAI（分流、重排）都會收到租戶資料，隱私與資料處理說明要列出（[風險與待定事項](../roadmap/risks.md)）。Gemini 走 Vertex AI，不走 Gemini API：Gemini API 付費層的濫用監控會保留資料 55 天、不保證零資料保留、不能指定處理區域；Vertex AI 可以申請零資料保留並選 us 或 eu 區域，處理區域在接入時決定（Laysi/trustiot-v2#440）。Decisions API 的零資料保留與資料駐留（美國、EEA 與瑞士）要符合資格才有，接入時申請。

## 主 agent

### 工具

| 工具 | 內容 | 執行的服務 | 里程碑 |
| --- | --- | --- | --- |
| 語意層查詢 | 最新值（`telemetry_latest`）、統計（`telemetry_statistics`：筆數、最小、最大、平均、第一與最後讀數）與用量（`telemetry_usage`：用電量等累計值的增量，附累計檢查與可疑跳增），依使用者時區的相對期間（`period`，週從星期一開始）或時間範圍、以區間回傳精簡的表格（J3，Laysi/trustiot-v2#142；計算方式見 [services/telemetry-query](../../services/telemetry-query/README.md)「語意查詢」）。異常區段、斷訊、停機與比較在 Laysi/trustiot-v2#455 | telemetry-query | MS3 |
| 唯讀 SQL | 經語法解析、以租戶唯讀帳號執行的 SELECT；語意層工具不夠時用來回答，也是儀表板資料集的主要來源（J3、決策 33）。工具 `telemetry_sql` 呼叫內部入口 `POST /internal/telemetry/v1/sql`：只接受 AI 助理換出的 token（Laysi/trustiot-v2#150 的儀表板資料集也由 AI 助理以使用者的身分送出，不另加呼叫者），入口 gateway 不轉送，瀏覽器送不了 SQL；viewer、editor、admin 都可以（2026-10-26 決定，Laysi/trustiot-v2#143）。只給雲端模型（下方「地端」第 6 點） | telemetry-query | MS3 |
| 設備與知識 | 設備、型號、資料點、knowledge、assetNotes（J6） | assets | MS3 |
| 儀表板 | `dashboard`：列出、讀取與寫入對話的儀表板草稿（資料集、計算、常數與頁面程式）；寫入時以提問者的身分執行資料集，經平台檢查通過才存成新版本（J4，見下方「儀表板」） | assistant | MS3 |
| 寫入 | AI 設備上架、修改型號；先暫停，確認畫面由服務依參數產生（J4） | assets | MS3 |
| 文件檢索 | 召回與重排，回傳文件名、頁碼、裁切座標與頁面圖，由主 agent 自己讀（[知識與文件庫](rag.md)） | knowledge | MS5 |
| 影音 | 找片段，再讓 Gemini 重看指定片段（[知識與文件庫](rag.md)的「影音」） | knowledge | MS5 |
| 知識圖 | Cypher（[現場脈絡與知識圖](field-context.md)） | field-context | MS5 |

工具的設計規則：

1. 依領域分組，相近的功能用參數合併，總數控制在 10 個以內。超過 10 個、或工具定義超過 10K token 時（例如 J9 的 MCP 加入後）才啟用 tool search，最常用的幾個不延遲載入。
2. 回傳精簡：只放高訊號的欄位與穩定的 ID；查詢結果有筆數與大小上限，超過時告訴模型怎麼縮小範圍（例如縮短時間區間）；錯誤訊息說明該怎麼改，附正確的參數範例；底層的原始錯誤不回給模型（J2）。
3. 工具結果可以帶圖片：文件檢索直接回傳頁面圖，由主 agent 自己讀（決策 26）。
4. 租戶範圍、可查期間（B3）與權限由服務依內部身分注入，模型看不到也改不了（決策 27、29）。
5. 工具數上限管的是工具組，不是呼叫次數：一次模型回應可以要求任意多個呼叫，所以每一步只同時執行前幾個（預設 8 個），之後的呼叫回錯誤、請模型下一步再叫，一次回應造成的後端負載有上限（Laysi/trustiot-v2#145）。
6. 工具以使用者的身分呼叫服務：每個問題開始時以請求的身分建立 run 委派（Laysi/trustiot-v2#551），每次呼叫前換一張受眾是被呼叫服務的 token；問題在委派到期前 30 秒結束，委派被撤銷（登出、被移出租戶）時當下這一步取消、問題結束、不重試。被呼叫服務的錯誤只看狀態碼，回應本文不進模型也不進日誌；唯一的例外是唯讀 SQL 的拒絕（422），本文嚴格解析成封閉的拒絕類別，模型只拿到每個類別固定的說明（Laysi/trustiot-v2#143）；儀表板檢查的判定同樣嚴格解析成封閉的問題類別，每個類別有固定的說明，問題裡的參照與值都要對得到 assistant 送出的資料；模型從判定拿到的只來自封閉的集合、對 assistant 送出的頁面與資料做過的檢查，或 assistant 自己的資料：script 錯誤只給錯誤類別、不給訊息，座標軸刻度不帶值，值一律取自 assistant 自己的資料集結果（Laysi/trustiot-v2#150）。設備與成員寫的文字（文字讀數、資料點 key、型號名稱）是不可信的輸入，每個字串截到 256 個字元；這限制長度，不讓文字變得可信。目前的工具共 6 個：遙測的最新值與設備型號列表（Laysi/trustiot-v2#591）、遙測的統計與用量（Laysi/trustiot-v2#142；統計取代 Laysi/trustiot-v2#591 的時間序列工具，兩者是同一種功能，依規則 1 合併）、唯讀 SQL（`telemetry_sql`，Laysi/trustiot-v2#143）與儀表板（`dashboard`，Laysi/trustiot-v2#150）。唯讀 SQL 只給雲端模型，地端模型沒有它（「地端」第 6 點）；地端是否開放儀表板依「地端」第 5 點決定（[services/assistant](../../services/assistant/README.md)「工具」與「代表使用者呼叫」）。語意工具的輸入就是 telemetry-query 語意查詢 API 的請求本文，儀表板的資料集可以原樣存下、以檢視者身分重送。

### 儀表板

2026-10-20 決定照 Claude Dashboards（Anthropic 2026-10-08 推出的 beta）的做法（決策 33）：使用者用自然語言描述想看的東西，主 agent 建立儀表板；舊系統在回答裡附的長條圖與折線圖改由儀表板取代。

1. **儀表板由資料集與頁面組成**，存在 assistant 服務的 Postgres（RLS 依租戶，決策 29），每次修改存成新版本，可以回到任一舊版。儀表板屬於租戶（2026-10-26，Laysi/trustiot-v2#150）：
   - **草稿與發布**：主 agent 的工具只寫屬於對話的草稿，在對話側邊直接顯示；使用者以 API 把草稿發布到租戶的儀表板列表，列表裡的儀表板有自己的網址。要修改列表裡的儀表板，主 agent 在對話裡做一份草稿，使用者再把它發布成新版本；列表裡的儀表板在這期間被別人改過時，發布回衝突。工具從不直接改列表裡的儀表板。
   - **草稿跟著對話**：對話被刪除（還能還原）時隱藏、還原時回來，清除對話時一起刪除；已發布的不受影響，複製對話不帶儀表板。列表裡的儀表板只能封存與取消封存，MS3 不做刪除。
   - **角色**（A2）：viewer 可以看列表裡的儀表板、執行它的資料集、在自己的對話做草稿；editor 與 admin 另外可以發布、回到舊版、封存。角色在每個請求從請求的內部身分讀取。
   - **分享的讀者**：對話分享給租戶後（Laysi/trustiot-v2#650），讀者唯讀看到對話的草稿，資料集以讀者自己的身分執行。
2. **資料集存查詢，不存結果**：每個資料集是一段 AI 寫的唯讀 SQL（J3 的語法解析與租戶唯讀帳號），或一次語意層工具呼叫（工具與參數），附標題與一句說明。每次打開儀表板，都以檢視者的內部身分重新執行（決策 27），租戶、角色與方案的可查期間照檢視者套用，所以資料保持最新，權限較少的成員看到的也較少。運算（加總、差值、比例、分桶）盡量寫在 SQL 裡，讓人看得到算法；每個資料集的結果最多 2,000 列、2 MiB，超過時要求縮小範圍或先彙總。每次打開與重新整理都重新查詢，不沿用先前的結果（2026-10-26）。
3. **頁面由主 agent 寫**：HTML 加 script，畫圖用平台提供的 d3，不從外部載入任何程式。頁面裡不寫任何數字：每個數字、圖表與表格都從資料集讀取，並標記來源（哪個資料集、哪個欄位、哪一列）；跨資料集才算得出的值（例如兩個查詢的比值）宣告成具名的計算，輸入只能是資料集，算式公開給使用者看；算式只有聚合、指定列與四則運算（[frontend/sandbox](../../frontend/sandbox/README.md)「計算語言」）。使用者選定的固定參考值（例如目標線）可以寫成常數。
4. **平台檢查後才發布**：assistant 以對話使用者的身分執行每個資料集，把頁面與結果交給檢查元件（`dashboard-check`，獨立的 pod）。檢查元件以與前端相同的沙箱文件、runtime 與 CSP，在無頭 Chromium 執行頁面，從頁面碰不到的 isolated world 比對每個標記的值與資料，並找出對不到資料的標記、沒有標記的數字與 script 錯誤。不通過時把問題回給主 agent 修正，使用者看到的仍是上一版。檢查在工具呼叫裡同步完成，不需要使用者開著瀏覽器。檢查元件只接受 assistant 的呼叫，沒有任何對外連線（NetworkPolicy 拒絕所有 egress），沒有 ServiceAccount token 與密鑰。Chromium 在 restricted Pod Security 與 RuntimeDefault seccomp 下無法啟用自己的沙箱，所以每次檢查開新的瀏覽器行程，同一時間只檢查一個。檢視時的資料與檢查時不同，前端的 runtime 在每次顯示時再比對一次，對不到資料的值不照常顯示（2026-10-26，Laysi/trustiot-v2#150）。
5. **沙箱**：頁面是模型寫的程式，只在沙箱 iframe 執行：不給同源權限、不能讀 cookie 與儲存空間，CSP 禁止頁面對其他主機發出請求（含圖片、字型與表單），沒有外部 script 與巢狀 iframe。同一主機只允許載入 script 與樣式表（runtime 自己的檔案），這種請求不帶 cookie；頁面另外請求檔案時，發布前的檢查不通過。資料由外層頁面以 postMessage 交給它，資料值一律當文字顯示，不當 HTML 解讀。CSP 管不到的外送通道在 Laysi/trustiot-v2#699 處理，儀表板頁（Laysi/trustiot-v2#462）上線前要處理完。沙箱文件、runtime 與頁面能用的 API 見 [frontend/sandbox](../../frontend/sandbox/README.md)。
6. **看得到來源**：點任何數字或圖表，顯示它的資料集：標題、說明、SQL 或工具與參數，以及這次查詢的時間；使用者可以請 AI 解釋。對話中的中間步驟與內部 prompt 照舊不外送（J2），只有儀表板資料集的查詢公開。主 agent 的回答不放 SQL 與 HTML：回答送出前以程式移除，畫面標示移除了內容，system prompt 也要求不輸出（2026-10-26，J5）。
7. **修改走對話**：使用者在對話中提出（「改成每週」「加上 B 線」），主 agent 讀目前的版本，只改要改的資料集或頁面，再經第 4 點的檢查。
8. **成本**：建立與修改儀表板呼叫模型，計入 AI 額度；打開與重新整理儀表板只重跑查詢，不呼叫模型，不計 AI 額度，查詢受每租戶的 ClickHouse 查詢上限限制（決策 14）。儀表板最多用到這個上限的一半：每個租戶每分鐘最多 60 次資料集執行，超過時請使用者稍後再試；唯讀 SQL 的檢查名額，一個租戶同時最多佔一半（2026-10-26）。
9. **檢查不被一個租戶佔滿**：每個 assistant 副本裡，每個租戶同時只有一次 AI 寫入在執行資料集與檢查，另一次最多等 30 秒，之後回「檢查忙碌」，不算進問題的寫入次數；每個副本同時最多送出 2 次檢查（檢查元件執行一個、佇列一個），依先後取得。不另設每分鐘的檢查次數：每次檢查都用掉問題的 6 次寫入之一（每次是一次模型呼叫，計入 AI 額度），一個租戶最多佔一個名額，每分鐘的上限只會在檢查元件閒置時多拒絕。這些上限與第 8 點的一樣以每個副本計（Laysi/trustiot-v2#150；數值在 Laysi/trustiot-v2#694 調整）。

### 子 agent

子 agent 只用在「要讀很多、最後只需要摘要」的工作（例如跨很多頁比對手冊），由 worker 執行並回傳結構化的摘要；查詢與草擬不再固定交給子 agent（決策 31）。Anthropic 的實測是單一 agent 約耗用一般對話 4 倍的 token、多 agent 約 15 倍，所以能做成確定性工具的就不做成子 agent。

### 對話歷史只往後加

Claude Opus 5.5 的 thinking 綁定產生它的對話：system prompt、工具定義與先前的每一則訊息都要與產生時完全相同，否則先前的 thinking 失效；2026-08-31 之後建立的帳號，改過歷史的請求直接回 400。所以：

1. 對話歷史（含 thinking 區塊）原樣保存、原樣送回，不刪也不改先前的訊息。舊系統「完成後清掉中間的工具與 SQL 訊息」改成只在畫面上隱藏（J2 只串流最終回答）。
2. 每個對話固定一個 system prompt 版本（J8 已規定修改只對新對話生效）。對話中途要加的指示或更新的脈絡（例如使用者改了語言設定）附加在歷史後面，不改最前面的 system prompt，用 Anthropic 的[對話中 system 訊息](https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages)（`messages` 裡的 `role: "system"`；2026-10-18 查證 Opus 5.5、Sonnet 5.5、Haiku 5.5 都支援，不需 beta header，Sonnet 5 不支援）。它必須緊接在使用者訊息之後，並且是最後一則或緊接著 assistant 回覆。它代表營運方的指示、優先於使用者訊息，所以只放服務產生的內容，不放使用者輸入、工具結果或文件內容。
3. 每個對話的工具集合固定，從第一個請求就宣告；要動態增加時用 tool search 的延遲載入，不替換工具清單。
4. 重新產生最後一則回答（J1）只捨棄最後一輪，前面的訊息不動。
5. 實作（Laysi/trustiot-v2#145，[services/assistant](../../services/assistant/README.md)）：每則訊息以收到或第一次寫出的 JSON 文字保存、原樣寫回請求，所以下一次請求的 `messages` 逐位元組以上一次的開頭。模型的一回合與它回應的訊息（工具結果、收尾指示）一起加入歷史：呼叫失敗、拒答或中途取消時不留下半套，對話中的 system 訊息不會落在下一個問題之前。拒答的回合不加入歷史。
6. 儲存（Laysi/trustiot-v2#553）：對話與歷史存在 assistant 的 Postgres schema，依租戶啟用 RLS（決策 29）。每則訊息一列，存送給模型的 JSON 文字的位元組（`bytea`，不用 jsonb，它會重排鍵、改寫數字），讀回時確認位元組不變才送出；對話開始時固定的 system prompt 全文、租戶脈絡、工具定義、effort 與快取時效存在對話那一列，新版本的 prompt 不影響舊對話。服務角色只有 SELECT 與 INSERT，資料庫拒絕任何 UPDATE、DELETE、TRUNCATE；寫入依位置只往後加，同一位置已有不同訊息時拒絕、不覆寫，所以兩個寫入者不會讓歷史分岔。刪除對話的做法由對話串（Laysi/trustiot-v2#146）決定。細節見 [services/assistant](../../services/assistant/README.md) 的「對話的儲存」。

### Prompt caching

1. 排列順序：工具定義、system prompt（全平台共用）、租戶脈絡（設備、資料點、方案權益、語言與時區）、對話。前兩段在所有租戶之間共用一份快取。第一個快取斷點放在 system prompt 結尾，第二個放在租戶脈絡結尾，對話部分用自動快取。
2. 快取依模型分開，換模型（含備援）從零開始；分流把簡單問題交給 worker 時，主 agent 的快取不受影響。
3. 預設 5 分鐘 TTL；使用者常隔很久才回覆的對話，依實測的間隔分布決定是否改用 1 小時。
4. 每次呼叫記下 `cache_read_input_tokens`；長時間為 0 代表前綴被改動，列入監控。assistant 記在日誌 `model call completed` 的 `usage.cache_read`，並以指標 `assistant.model_calls` 的 `cache`（`hit`、`miss`）計數。
5. 依 2026-10-18 的官方單價，Opus 5.5 的快取讀取是一般輸入的 0.05 倍：呼叫 10 次、每次重送 20K token 的固定前綴，前綴的費用從 $0.80 降到約 $0.14。

### Effort 與預算

1. 主 agent 的 effort 由分流決定（low、medium、high），以逐則訊息的 effort 設定送出，不讓快取失效；這是 Anthropic 的 beta 功能，無法使用時整個對話固定一個 effort。
2. 每一題有 token 預算，接近時要求主 agent 收尾，不以步數上限中斷長分析（J2、[#99](https://github.com/Laysi/trustiot/issues/99)）；每一次模型呼叫各自預扣額度（決策 28）。
3. 預算的算法（Laysi/trustiot-v2#145）：累計每次呼叫處理的 token，即輸入、快取寫入、快取讀取與輸出（含 thinking），閘道換模型時每次嘗試都算；快取讀取雖然便宜，仍佔用 context，也反映分析的長度。供應商沒回報用量時依位元組數估計，不當 0。累計到預算的 80%（可設定）時，下一次呼叫前附加一則對話中的 system 訊息，請主 agent 不再呼叫工具、用現有資料回答，只送一次；到 100% 時，主 agent 再要求的工具不執行（回說明原因的錯誤結果），這一題以「預算用完」結束。每次呼叫送出前先確認放得進剩下的預算：輸入以模型上一次回報的歷史大小，加上之後新增訊息的高估值（JSON 每 2 位元組算 1 token，每張圖算 5,000 token）估計，`max_tokens` 降到剩下的空間；剩下不到 1,024 token 的輸出空間時不送出，這一題以「預算用完」結束。所以單次呼叫不會把一題帶到遠超過預算，只有模型的實際用量超過估計（例如閘道換模型重試）時才會略超。effort 在分流（Laysi/trustiot-v2#444）完成前由設定決定，整個對話固定。
4. 額度預扣（Laysi/trustiot-v2#140）的介接點：每次模型呼叫有自己的 UUIDv7（帳本的 `llm_call_id`，先記在日誌），閘道拒絕時以模型錯誤回到迴圈。

## 分流

1. 一次 Decisions 請求問兩題：predicate「這則訊息不需要查資料、讀文件或執行動作就能完整回答」（回傳機率），以及 score「需要多少推理」（low、medium、high，對應主 agent 的 effort）。輸入是最新的訊息、最近幾輪的文字與附圖的縮圖；Decisions 的圖片收 base64 data URL 或公開網址（我們的圖片不公開，所以只送 base64，crates/ai-clients 送出前檢查），不收音訊與影片，影音只送檔案類型與長度。
2. 只有機率超過門檻才交給 worker，門檻用 MS3 題庫校準；其他的、不確定的，以及分流失敗（逾時、錯誤、beta 期間的介面變更）一律交給主 agent。
3. worker 只回答簡單訊息，沒有查詢工具，只有 `escalate(reason)`：覺得超出能力時，把對話交給主 agent。主 agent 接手同一個對話，worker 的回答以一般訊息附加在歷史後面。
4. 分流要與「全部交給主 agent、用低 effort」比較（MS3 題庫，Laysi/trustiot-v2#444）。依 2026-10-18 的單價估算，分流每題只省約 $0.01–0.02，主要的好處是簡單問題回得快；比不過就拿掉分流。Decisions API 仍在 public beta（2026-10-06 推出），沒有 SLA。

## 安全

主 agent 權力變大，依 Meta 的 Rule of Two 劃邊界：同一個 session 裡，「不可信內容」、「私有資料」、「改變狀態或對外通訊」三項最多同時具備兩項。主 agent 三項都會碰到（使用者訊息、手冊與影音、租戶資料、設備上架），所以：

1. 使用者訊息也是不可信輸入：可能夾帶從別處貼上的指示，或想藉 AI 越權。主 agent 能做的事不超過該使用者自己在介面上能做的：每個工具呼叫都以該使用者的內部身分執行（A4、決策 27；分析跑得比 60 秒的 token 久，所以以 run 委派每次換一張 token，身分服務每次重查 session 與角色，登出或被移出租戶後下一個工具呼叫就失敗，Laysi/trustiot-v2#551），租戶、角色權限（A2）與方案權益（B3）由服務檢查，與使用者直接呼叫 API 相同，模型被說服也拿不到更多權限。分流被誘導只會改變由誰回答與 effort：worker 沒有查詢與寫入工具，花費受每題預算與額度限制。
2. 其他不可信內容（使用者上傳的文件與影音、手冊、使用者輸入的設備名稱與備註）只經工具進來，標明是資料、不是指示。租戶脈絡（設備、資料點等成員輸入的名稱）是例外，放在 system 的第二段以便快取：內容先跳脫 `&`、`<`、`>`，再包在 `<tenant_context>` 標籤裡，system prompt 說明標籤內是資料，所以內容關不掉標籤、冒充不了平台的指示（Laysi/trustiot-v2#145）。影音理解與長文件閱讀由沒有工具的模型處理，只回傳固定格式的欄位。
3. 寫入一律先暫停、等使用者確認，確認畫面由服務依參數產生，不顯示模型寫的文字（J4）。
4. 主 agent 沒有對外通道：不能抓網址、寄信或呼叫外部 Webhook；報表與通知由警報服務經自己的流程寄出。主 agent 寫的儀表板頁面只在沙箱執行，CSP 禁止它對其他主機發出請求（「儀表板」第 5 點）；發布前的檢查在沒有任何對外連線的 pod 裡執行（「儀表板」第 4 點）。在使用者的瀏覽器裡，CSP 管不到的外送通道還沒處理完（Laysi/trustiot-v2#699），所以還不能說被注入的模型無法把畫面上的資料送出去；儀表板頁（Laysi/trustiot-v2#462）上線前要處理完。
5. 前端不自動載入 AI 回答裡的外部圖片與連結（常見的資料外洩管道）：回答的 Markdown 只渲染文字、表格與我們自己網域的資源。
6. 題庫有一組注入測試（使用者訊息，以及夾帶指示的手冊、圖片、音訊、設備名稱），預期結果是不執行、不外洩、不越權。儀表板沙箱另以確定性測試驗證：頁面發出的對外請求（fetch、圖片、字型、表單、導向）全部被擋下，讀不到 cookie 與外層頁面。租戶隔離、角色權限與寫入確認以確定性的測試驗證，不交給 LLM 評分：以構造的工具參數直接呼叫工具執行層，確認跨租戶、權限不足與未經確認的寫入都被服務拒絕（決策 29）。

## 驗證

| 題庫 | 比較什麼 | issue |
| --- | --- | --- |
| MS3 分析題庫 | 主 agent 用 Opus 5.5 或 Sonnet 5.5、有沒有分流、effort；指標是答對率、連續 3 次都答對的比例、每個答對任務的成本、延遲（p50、p95）與工具呼叫數 | Laysi/trustiot-v2#223、Laysi/trustiot-v2#444、Laysi/trustiot-v2#161 |
| RAG 題庫 | 重排（Decisions 逐頁打分與每批候選數、Haiku 5.5 一次排序）、嵌入維度 | Laysi/trustiot-v2#192 |
| 影音題庫 | 20–30 支影片或錄音，配上有標準答案的事實題；比較 Gemini 3.8 Flash 與 Gemini 4 Argon、解析度、取樣率、切段長度與每題成本 | Laysi/trustiot-v2#441 |
| 注入題庫 | 不執行、不外洩、不越權 | Laysi/trustiot-v2#445 |

## 成本估算（2026-10-18，依官方單價推算）

| 情境 | 估算 |
| --- | --- |
| 複雜問題：Opus 5.5，呼叫模型 6 次，有快取 | 約 $0.17／題 |
| 同上，沒有快取 | 約 $0.62／題 |
| 簡單問題：主 agent 用低 effort | 約 $0.01–0.02／題 |
| 簡單問題：分流後交給 Haiku 5.5 | 約 $0.002／題 |
| 影音建索引 | 每小時影片約 $2–4，主要是影片向量，片段切得越短越貴 |
| 只看 2 分鐘片段的提問（Gemini 3.8 Flash） | 約 $0.01／次 |
| 對整支 1 小時影片提問（Gemini 3.8 Flash） | 不用快取約 $0.27／次；用顯式快取約 $0.03／次，另加儲存費每小時約 $0.18 |

假設：工具定義加 system prompt 共 15K token、租戶脈絡 2K token、每次工具結果約 2K token、每次輸出（含 thinking）約 600 token；影片以低解析度計，約每秒 100 token。最大的省錢手段是 prompt caching 與 effort，不是分流。Gemini 4 Argon 從 2026-12-01 起、Gemini 3.8 Flash 從 2027-01-01 起，單價約漲一倍。

## 地端（SaaS 全部完成後處理）

地端排在 SaaS 全部完成之後（2026-10-18，全專案的清單在 Laysi/trustiot-v2#446）。屆時要處理：

1. agent 迴圈加上 OpenAI 相容的實作，接客戶的 vLLM 或 Ollama（決策 16）。對模型的呼叫從一開始就集中在一個介面後面，不讓 Anthropic 的格式散到工具與對話的程式裡。
2. 沒有 Decisions API：分流關閉，或改用客戶模型的結構化輸出；重排用 Qwen3-VL-Reranker（開放權重，vLLM 可以跑）。
3. 嵌入用 EmbeddingGemma 2（決策 26）；影音預設關閉，除非客戶有能處理影音的端點。
4. 小模型的工具數量要縮減，寫入工具一律確認或關閉；每個模型等級跑同一份題庫，結果列成支援矩陣。
5. 儀表板的頁面程式與 SQL 由小模型寫不可靠：依支援矩陣決定地端是否開放建立儀表板，或只提供固定的儀表板範本。
6. 唯讀 SQL 工具（`telemetry_sql`）只給雲端模型，地端模型只用語意層工具（決策 16）；接上地端模型時不註冊這個工具（`services/assistant/src/lib.rs`）。
