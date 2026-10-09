# 知識與文件庫（RAG）

RAG 照 [#28](https://github.com/Laysi/trustiot/issues/28)、[#124](https://github.com/Laysi/trustiot/issues/124)、[#44](https://github.com/Laysi/trustiot/issues/44)、[#34](https://github.com/Laysi/trustiot/issues/34) 的既定作法重建：視覺原生的多模態設備文件庫，位在回答層級第 1.5 層。檢索作法全部保留，只有綁在 AWS 的部分要隨去 AWS 化調整。

照原案保留：

1. PDF 固定逐頁轉圖，必要時加重疊裁切；頁面圖是閱讀與引用的單位，不做 OCR 或切塊。SaaS 另加 LLM 預處理，但它產生的文字只用來找頁面（見下一段）。
2. `Qwen3-VL-Embedding-8B` 改為 SaaS 用 Gemini Embedding 2、地端用 EmbeddingGemma 2（見下一段）；`Qwen3-VL-Reranker-8B` 以「問題加候選頁面圖」重排序，留作地端的備案（SaaS 改用快速 LLM 重排，見下一段）；最後由可看圖的模型閱讀頁面並回答。這些模型都透過 API 呼叫：SaaS 用雲端 API，地端接客戶自己的端點，我們不自備 GPU。
3. 引用必附文件名與頁碼，裁切時附座標。
4. Postgres 是文件、版本、處理狀態與稽核的唯一來源；向量庫只存向量與最小引用資訊，可由 Postgres 加原始檔重建。
5. 單一共用 collection，以 tenant\_id 分區；每筆向量帶租戶、型號、文件、版本，點 ID 由文件、版本、頁碼、裁切座標穩定導出。租戶條件由知識庫服務的存取層依內部身分加上，查詢或寫入沒帶租戶條件一律拒絕；跨租戶負向測試列入 CI。
6. 檢索範圍由服務注入，AI 看不到也改不了；所有讀寫只能經過唯一的存取層，未實作的操作一律拒絕；跨租戶負向測試必備。
7. 重新轉圖或重新產生向量時，用並行 collection 加 alias 切換；新版本向量數驗證後才切換。
8. 背景處理用 Postgres 工作佇列（SKIP LOCKED、租約、回收、重試上限），跑在知識庫服務內；逐頁處理、記憶體有上限，重試不得產生重複向量。
9. 文件與 model.knowledge 分開存放；檢索結論經使用者核可才寫回 knowledge。
10. 處理與檢索的模型用量計入 AI 額度。
11. 取代並移除舊的共用文件庫（addDocs、getDocs）。

與原案不同：LLM 預處理與多路檢索（SaaS，2026-10-09 決定）。原案拒絕文字化，是因為 docling、markitdown 在故障碼表與掃描頁上失真；看圖 LLM 不同，而且它的文字只用來找頁面。knowva 拿掉 VLM 轉錄是受本機資源限制，改用 API 後沒有這個限制。

1. LLM 預處理：每頁以高解析度送看圖 LLM，產出轉錄、頁面描述、中英關鍵詞與故障碼；記下模型與版本，換模型時重跑。
2. 多路索引：同一筆點位存三種索引——頁面圖加描述的向量、轉錄文字的向量、故障碼與型號的關鍵字索引（Qdrant 的 named vector 與 sparse vector）。
3. 嵌入模型：SaaS 用 Gemini Embedding 2，維度 768 或 1536 由題庫決定；地端用 EmbeddingGemma 2（開放權重、Apache 2.0，CPU 就能跑）。通用基準上 Gemini 明顯較強（技術報告 MMTEB 約 69.9，EmbeddingGemma 2 官方為 61.36）。兩邊向量空間不同，租戶在 SaaS 與地端之間搬遷時重新產生向量，匯出檔不帶向量。
4. 召回：三路各取前幾十名，合併去重，不做 RRF 融合；knowva 實測 RRF 會把只有向量找得到的頁面擠出候選。
5. 重排：把合併後的候選（轉錄與描述文字加頁面 ID）一次送給快速模型，例如 Claude Haiku 5.5 或 GPT-6 Luna，用結構化輸出回傳排序後的 ID。回傳不存在的 ID 就丟棄，呼叫失敗時退回向量分數的順序。走一般的聊天 API，模型由設定指定，不另接專用的 rerank API。
6. 閱讀與引用：前幾名的頁面圖交給看圖模型閱讀並回答；LLM 產生的文字不顯示、不當答案來源，引用仍是文件名、頁碼與座標。
7. 地端：預處理與 LLM 重排接客戶自己的模型；模型不夠強時關掉預處理、改用 VL reranker，流程照常。
8. 驗證：用自己的手冊建 60–100 題的題庫，涵蓋故障碼表、掃描頁、小字、中文問英文手冊；量測正確頁面進入候選與排第一的比例、答案正確率、延遲與每次查詢成本。題庫用來選模型、決定各路取幾名，也當回歸測試。

與去 AWS 化衝突、需要調整的部分：

| 原案 | 衝突 | 待決定 |
| --- | --- | --- |
| 向量庫用 Qdrant Managed Cloud，經 AWS Marketplace 付款 | 依賴 AWS，地端無法使用；向量存放在第三方 | 已定：SaaS 用 Qdrant Cloud（Free 起步，不足時升 Standard），地端在 k3s 自架 |
| 原始 PDF 與頁面圖存 S3，依前綴限制 IAM | 依賴 AWS | 已定：S3 相容的託管物件儲存 |
| Embedding 與 rerank 走 IONOS，經 Cloudflare AI Gateway | Cloudflare AI Gateway 已由自建 AI 閘道取代（決策 28）；地端不能把頁面圖送出去 | 已定：embedding 與重排都經 AI 閘道呼叫並記入用量；SaaS 用 Gemini Embedding 2 加快速 LLM 重排，地端接客戶端點（見上一段） |
| Database API key 分權、IP 白名單、Secrets Manager | Secrets Manager 依賴 AWS；地端改為叢集內部存取 | SaaS：檢索用唯讀 key、背景處理用寫入 key，金鑰放 k8s Secret（細分權限的 key 需 v1.11 以上，Free 是否支援待確認）；地端：只在叢集內可達，讀寫仍分帳號 |

SaaS 的向量存放在 Qdrant 雲端，原案「向量存放在第三方」的信任例外保留；向量庫只存向量與最小引用資訊，原始檔與頁面圖仍在物件儲存。地端自架則沒有這個例外。

SaaS 用 Qdrant Cloud 的前提（列為驗收條件）：

1. 了解 Free 的限制：單一節點、0.5 vCPU、1 GB RAM、4 GB 磁碟，沒有高可用；閒置 1 週自動暫停，4 週內未重新啟用就刪除；只能用 API 手動做快照；升級版本會停機。產線預估：改用 768 或 1536 維後，含原始向量與 int8 副本，4 GB 約可放 25 萬到 50 萬頁（粗估，未計索引開銷），接近上限前升 Standard。
2. 檢索可以降級：Qdrant 暫停或連不上時，回答略過第 1.5 層並註明文件庫暫時無法使用，其他功能照常；資料遺失時由 Postgres 加原始檔重建，重新產生向量要花 embedding API 費用。
3. 只用兩邊都支援的功能：同一版本線的 Qdrant API，不用 Cloud 專屬的推論服務。
4. 自架 Qdrant 的路徑放進 CI 持續測試。
5. Cluster 選離 SaaS 主機最近的區域，避免檢索延遲。
