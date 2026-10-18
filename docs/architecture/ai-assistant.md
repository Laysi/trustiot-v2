# AI 助理與 AI 閘道

2026-10-18 定案（決策 15、26、28、31、32）：主 agent 加工具，AI 閘道照各供應商的原生協定轉送，影片與聲音交給 Gemini。先做 SaaS；地端排在 SaaS 全部完成之後，屆時要處理的事列在最後一節。文中的模型與價格是 2026-10-18 查證的值；模型由 platform admin 依模型角色更換（J8），各角色用哪個模型由題庫決定。

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
    telemetry["語意層查詢<br>telemetry-query"]
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
  main --> telemetry & assets & write & docs & media & graph
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

1. **照原生協定轉送，不轉換格式**（決策 15）：呼叫端（assistant、knowledge、alerts）以內部 API 指定模型角色，請求本文用該角色供應商的原生格式；閘道依角色填入模型與憑證、轉送到供應商，回應與串流原樣交回。SaaS 支援三種協定：Anthropic Messages、Vertex AI 的 Gemini（生成、嵌入、顯式快取）、OpenAI Decisions；地端的 OpenAI 相容端點在 SaaS 完成後加入。不採用相容層的理由：Anthropic 的 OpenAI 相容層官方定位不是給正式環境用的，不支援 prompt caching、用量的快取欄位永遠是空的、不回傳 thinking、`strict` 與 effort 被忽略，也用不到 tool search 與 context editing；Gemini 的相容端點仍在 beta，不能用 Files API，影片輸入沒有文件。LiteLLM、Envoy AI Gateway 等閘道支援快取，也是靠自己轉成原生協定。
2. **角色、備援與逾時**：platform admin 隨時可以更換角色的模型（J8），換成同一協定的模型不必改程式，換協定要呼叫端支援。每個角色設一串備援模型，429、過載或逾時時改用下一個（例如 main 從 Opus 5.5 換成 Sonnet 5.5）；快取依模型分開，換模型後從零開始。供應商的拒答（例如 Anthropic 的 `stop_reason: refusal`）照常記帳，交回呼叫端，由它回通用訊息（J2）。
3. **帳本**（決策 28）：以 llm\_call\_id 去重，依供應商回報的分類記錄用量：一般輸入、快取寫入（5 分鐘與 1 小時分開）、快取讀取、輸出（含 thinking），以及 Gemini 各模態的輸入（文字、圖片、影片、音訊）。單價表依這些類別定價，另外記 Gemini 顯式快取的儲存費（token 數乘以時數）；Decisions 只收輸入費；Embedding 2 依模態計價（文字 token、圖片張數、音訊秒數、影片格數）。供應商沒回報用量時（含串流中途中斷，J8），輸入與已產生的輸出（含 thinking）分別在本地估算並標記，絕不記 0：輸入用 `count_tokens` 或本地 tokenizer 計算，輸出依已收到的內容計算；thinking 只回傳摘要或不回傳時看不到實際長度，這筆估算標為下限。Claude 的 token 數以供應商回報或 `count_tokens` 為準，本地 tokenizer 會偏差。
4. **額度**：預扣與結算照決策 28；agent 迴圈裡的每一次模型呼叫各自預扣、各自結算。
5. **憑證**（[密鑰與權限](secrets.md)）：Anthropic 與 OpenAI 用 API key；Vertex AI 優先用 Workload Identity Federation，不放長期金鑰（密鑰規則 8），不可行時才用服務帳號金鑰，在接入 Vertex AI 的 issue（Laysi/trustiot-v2#440）決定。都只給 `ai-gateway`。
6. **資料處理者**：Anthropic（主 agent、worker）、Google Vertex AI（影音、嵌入）、OpenAI（分流、重排）都會收到租戶資料，隱私與資料處理說明要列出（[風險與待定事項](../roadmap/risks.md)）。Gemini 走 Vertex AI，不走 Gemini API：Gemini API 付費層的濫用監控會保留資料 55 天、不保證零資料保留、不能指定處理區域；Vertex AI 可以申請零資料保留並選 us 或 eu 區域，處理區域在接入時決定（Laysi/trustiot-v2#440）。Decisions API 的零資料保留與資料駐留（美國、EEA 與瑞士）要符合資格才有，接入時申請。

## 主 agent

### 工具

| 工具 | 內容 | 執行的服務 | 里程碑 |
| --- | --- | --- | --- |
| 語意層查詢 | 用電量、異常區段、斷訊、停機、比較，回傳精簡的表格與圖表規格；語意層工具不夠時，雲端模型可用經語法解析的唯讀 SQL（J3） | telemetry-query | MS3 |
| 設備與知識 | 設備、型號、資料點、knowledge、assetNotes（J6） | assets | MS3 |
| 圖表 | 長條圖、折線圖（J4） | assistant | MS3 |
| 寫入 | AI 設備上架、修改型號；先暫停，確認畫面由服務依參數產生（J4） | assets | MS3 |
| 文件檢索 | 召回與重排，回傳文件名、頁碼、裁切座標與頁面圖，由主 agent 自己讀（[知識與文件庫](rag.md)） | knowledge | MS5 |
| 影音 | 找片段，再讓 Gemini 重看指定片段（[知識與文件庫](rag.md)的「影音」） | knowledge | MS5 |
| 知識圖 | Cypher（[現場脈絡與知識圖](field-context.md)） | field-context | MS5 |

工具的設計規則：

1. 依領域分組，相近的功能用參數合併，總數控制在 10 個以內。超過 10 個、或工具定義超過 10K token 時（例如 J9 的 MCP 加入後）才啟用 tool search，最常用的幾個不延遲載入。
2. 回傳精簡：只放高訊號的欄位與穩定的 ID；查詢結果有筆數與大小上限，超過時告訴模型怎麼縮小範圍（例如縮短時間區間）；錯誤訊息說明該怎麼改，附正確的參數範例；底層的原始錯誤不回給模型（J2）。
3. 工具結果可以帶圖片：文件檢索直接回傳頁面圖，由主 agent 自己讀（決策 26）。
4. 租戶範圍、可查期間（B3）與權限由服務依內部身分注入，模型看不到也改不了（決策 27、29）。

### 子 agent

子 agent 只用在「要讀很多、最後只需要摘要」的工作（例如跨很多頁比對手冊），由 worker 執行並回傳結構化的摘要；查詢與草擬不再固定交給子 agent（決策 31）。Anthropic 的實測是單一 agent 約耗用一般對話 4 倍的 token、多 agent 約 15 倍，所以能做成確定性工具的就不做成子 agent。

### 對話歷史只往後加

Claude Opus 5.5 的 thinking 綁定產生它的對話：system prompt、工具定義與先前的每一則訊息都要與產生時完全相同，否則先前的 thinking 失效；2026-08-31 之後建立的帳號，改過歷史的請求直接回 400。所以：

1. 對話歷史（含 thinking 區塊）原樣保存、原樣送回，不刪也不改先前的訊息。舊系統「完成後清掉中間的工具與 SQL 訊息」改成只在畫面上隱藏（J2 只串流最終回答）。
2. 每個對話固定一個 system prompt 版本（J8 已規定修改只對新對話生效）。對話中途要加的指示或更新的脈絡（例如使用者改了語言設定）附加在歷史後面，不改最前面的 system prompt，用 Anthropic 的[對話中 system 訊息](https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages)（`messages` 裡的 `role: "system"`；2026-10-18 查證 Opus 5.5、Sonnet 5.5、Haiku 5.5 都支援，不需 beta header，Sonnet 5 不支援）。它必須緊接在使用者訊息之後，並且是最後一則或緊接著 assistant 回覆。它代表營運方的指示、優先於使用者訊息，所以只放服務產生的內容，不放使用者輸入、工具結果或文件內容。
3. 每個對話的工具集合固定，從第一個請求就宣告；要動態增加時用 tool search 的延遲載入，不替換工具清單。
4. 重新產生最後一則回答（J1）只捨棄最後一輪，前面的訊息不動。

### Prompt caching

1. 排列順序：工具定義、system prompt（全平台共用）、租戶脈絡（設備、資料點、方案權益、語言與時區）、對話。前兩段在所有租戶之間共用一份快取。第一個快取斷點放在 system prompt 結尾，第二個放在租戶脈絡結尾，對話部分用自動快取。
2. 快取依模型分開，換模型（含備援）從零開始；分流把簡單問題交給 worker 時，主 agent 的快取不受影響。
3. 預設 5 分鐘 TTL；使用者常隔很久才回覆的對話，依實測的間隔分布決定是否改用 1 小時。
4. 每次呼叫記下 `cache_read_input_tokens`；長時間為 0 代表前綴被改動，列入監控。
5. 依 2026-10-18 的官方單價，Opus 5.5 的快取讀取是一般輸入的 0.05 倍：呼叫 10 次、每次重送 20K token 的固定前綴，前綴的費用從 $0.80 降到約 $0.14。

### Effort 與預算

1. 主 agent 的 effort 由分流決定（low、medium、high），以逐則訊息的 effort 設定送出，不讓快取失效；這是 Anthropic 的 beta 功能，無法使用時整個對話固定一個 effort。
2. 每一題有 token 預算，接近時要求主 agent 收尾，不以步數上限中斷長分析（J2、[#99](https://github.com/Laysi/trustiot/issues/99)）；每一次模型呼叫各自預扣額度（決策 28）。

## 分流

1. 一次 Decisions 請求問兩題：predicate「這則訊息不需要查資料、讀文件或執行動作就能完整回答」（回傳機率），以及 score「需要多少推理」（low、medium、high，對應主 agent 的 effort）。輸入是最新的訊息、最近幾輪的文字與附圖的縮圖；Decisions 只收 base64 圖片，不收音訊與影片，影音只送檔案類型與長度。
2. 只有機率超過門檻才交給 worker，門檻用 MS3 題庫校準；其他的、不確定的，以及分流失敗（逾時、錯誤、beta 期間的介面變更）一律交給主 agent。
3. worker 只回答簡單訊息，沒有查詢工具，只有 `escalate(reason)`：覺得超出能力時，把對話交給主 agent。主 agent 接手同一個對話，worker 的回答以一般訊息附加在歷史後面。
4. 分流要與「全部交給主 agent、用低 effort」比較（MS3 題庫）。依 2026-10-18 的單價估算，分流每題只省約 $0.01–0.02，主要的好處是簡單問題回得快；比不過就拿掉分流。Decisions API 仍在 public beta（2026-10-06 推出），沒有 SLA。

## 安全

主 agent 權力變大，依 Meta 的 Rule of Two 劃邊界：同一個 session 裡，「不可信內容」、「私有資料」、「改變狀態或對外通訊」三項最多同時具備兩項。主 agent 三項都會碰到（使用者訊息、手冊與影音、租戶資料、設備上架），所以：

1. 使用者訊息也是不可信輸入：可能夾帶從別處貼上的指示，或想藉 AI 越權。主 agent 能做的事不超過該使用者自己在介面上能做的：每個工具呼叫都以該使用者的內部身分執行（A4、決策 27），租戶、角色權限（A2）與方案權益（B3）由服務檢查，與使用者直接呼叫 API 相同，模型被說服也拿不到更多權限。分流被誘導只會改變由誰回答與 effort：worker 沒有查詢與寫入工具，花費受每題預算與額度限制。
2. 其他不可信內容（使用者上傳的文件與影音、手冊、使用者輸入的設備名稱與備註）只經工具進來，標明是資料、不是指示。影音理解與長文件閱讀由沒有工具的模型處理，只回傳固定格式的欄位。
3. 寫入一律先暫停、等使用者確認，確認畫面由服務依參數產生，不顯示模型寫的文字（J4）。
4. 主 agent 沒有對外通道：不能抓網址、寄信或呼叫外部 Webhook；報表與通知由警報服務經自己的流程寄出。
5. 前端不自動載入 AI 回答裡的外部圖片與連結（常見的資料外洩管道）：回答的 Markdown 只渲染文字、表格與我們自己網域的資源。
6. 題庫有一組注入測試（使用者訊息，以及夾帶指示的手冊、圖片、音訊、設備名稱），預期結果是不執行、不外洩、不越權。租戶隔離、角色權限與寫入確認以確定性的測試驗證，不交給 LLM 評分：以構造的工具參數直接呼叫工具執行層，確認跨租戶、權限不足與未經確認的寫入都被服務拒絕（決策 29）。

## 驗證

| 題庫 | 比較什麼 | issue |
| --- | --- | --- |
| MS3 分析題庫 | 主 agent 用 Opus 5.5 或 Sonnet 5.5、有沒有分流、effort；指標是答對率、連續 3 次都答對的比例、每個答對任務的成本、延遲（p50、p95）與工具呼叫數 | Laysi/trustiot-v2#223、Laysi/trustiot-v2#161 |
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

地端排在 SaaS 全部完成之後（2026-10-18）。屆時要處理：

1. agent 迴圈加上 OpenAI 相容的實作，接客戶的 vLLM 或 Ollama（決策 16）。對模型的呼叫從一開始就集中在一個介面後面，不讓 Anthropic 的格式散到工具與對話的程式裡。
2. 沒有 Decisions API：分流關閉，或改用客戶模型的結構化輸出；重排用 Qwen3-VL-Reranker（開放權重，vLLM 可以跑）。
3. 嵌入用 EmbeddingGemma 2（決策 26）；影音預設關閉，除非客戶有能處理影音的端點。
4. 小模型的工具數量要縮減，寫入工具一律確認或關閉；每個模型等級跑同一份題庫，結果列成支援矩陣。
