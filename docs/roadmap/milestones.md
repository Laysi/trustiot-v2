# 里程碑

依相依順序切分（2026-10-09 定案）：資料面先做，AI 助理用 lab 錄下的資料並行；MS3 之後先做現場上線與警報規則。編號用 MS，避免與清單的 M1–M8 混淆。每個完成條件都寫成自動測試，不靠人工驗收；MS0 的每個條件由哪些測試證明、在哪裡執行，見 [MS0 完成條件與測試](ms0-acceptance.md)。

| 里程碑 | 內容 | 完成條件 |
| --- | --- | --- |
| MS0 契約與骨架 | 服務邊界與 contract crate、各服務的 schema 與資料庫角色、事件（outbox）框架；API 閘道與服務間 mTLS（Istio ambient）、SaaS 經 Cloudflare Tunnel 對外、身分驗證與簽章的內部身分；Postgres 的 RLS 租戶隔離；共用資料格式 crate（M4）；Postgres 以 CloudNativePG 部署（映像用 CloudNativePG 的映像加 AGE 自建，之後的備份與副本不必換部署方式，M8）、schema 與 migration Job；ClickHouse 窄表與 row policy 機制；錯誤格式（M1）；密鑰規則（M3）；CI（M5）；本地一鍵啟動（M6）；Helm chart 與 SaaS、地端兩份 values；tracing 基線；補做 TPM mTLS 可行性測試 | 服務骨架經 API 閘道對外、彼此以事件互通；未經 AuthorizationPolicy 允許的服務呼叫被拒絕；身分服務停掉時入口一律拒絕，除了明列的例外，每條路由都要經過驗證；偽造、過期或受眾不符的內部身分被拒絕；未設定租戶的交易讀不到任何列；同一份 chart 搭配兩份 values 都部署得起來；migration 以 Job 執行；CI 全綠 |
| MS1 資料打通 | 自建 CA 與憑證換發（HTTPS 換發端點）；出廠登錄與佈建（D1、D2）；Mosquitto 加 dynsec 與 MQTT 的 WebSocket 轉接；閘道器採集（F1、F2、F4、F5）；離線緩衝與端到端確認（G1）；資料接收與窄表（I1、I2，含原始資料的保存期限）；設定下發（E1），設定一開始就由資產服務經事件產生；資產服務的型號、設備與閘道器綁定 API（沒有畫面，C1、C2、D3）；測試用 Modbus 端點與 lab 資料集（M7）；閘道器映像強化（D5，含資料分割區的 LUKS 加密）。這一階段沒有畫面，只用 API 操作 | lab 的閘道器把現場設備（含同一台 RS485 轉換器下的多台設備）的資料送進新系統；拔網路再恢復、或寫入後主機強制重開，都不掉資料；重送的資料不重複寫入；斷電重開後設定自動收斂；閘道器憑證到期前自動換發，憑證已過期的閘道器經換發端點恢復連線。以上用測試內的假 Modbus 端點加故障注入寫成自動測試 |
| MS2 可登入的多租戶產品 | 登入與權限（A1–A4）；租戶與配額（B1、B2）；方案權益模型與伺服器端檢查（B3，先用例子值）；建立租戶時建 ClickHouse 唯讀帳號與查詢上限；型號與設備的畫面（C1、C2，API 在 MS1）、閘道器列表與在線狀態（C3）；客戶自助綁定與閘道器數量配額檢查（D3）、解綁、除役與憑證撤銷（D4）；閘道器與設備健康（H1）；遙測畫面與即時圖表（I3）；匯出自己的原始資料（CSV）；SPA 與多語系（L1、L2）；首頁營運總覽與新租戶設定清單（L3）；平台後台與操作紀錄（K1、K2，事件層在這一階段建好）；備份與還原（M8） | 新租戶從畫面上綁定閘道器，一路做到看見即時值與圖表；跨租戶負向測試全部通過；停權後閘道器斷線，恢復後自動換發憑證、重連並補送；還原演練成功一次 |
| MS3 AI 助理對齊 | AI 閘道服務（Anthropic 與 OpenAI Decisions 的原生協定轉送、帳本、額度，決策 15、28）與主 agent 迴圈、分流（決策 31）；對話串、逐字串流與斷線續播（J1、J2）；語意層工具、1 分鐘與 1 小時彙總表、SQL 語法解析與 AI 查詢的可查期間限制（J3；每租戶唯讀帳號在 MS2 建好）；圖表（J4；ISO 50001 報告另行設計）；分析規則（J5）；knowledge 與 assetNotes（J6）；AI 用量與 system prompt 後台編輯（J8）；輸入補全（J11）；情境 AI 按鈕與建議問題（L3）；AI 設備上架 | 用題庫比對，舊系統答得出的分析題目新系統也答得一樣好；跨租戶查詢測試全部通過；注入測試全部不執行、不外洩、不越權；單一租戶跑重查詢時，資料接收與其他租戶不受影響；以代表性負載重測主機峰值，訂出 SaaS 的最低規格（地端的最低規格排在 SaaS 全部完成之後）。達成後新系統即具備舊系統的功能 |
| MS4 現場上線 | OTA 與分批推送、閘道器程式的發佈與簽章；警報規則（Rhai、回測、通知）；現場脈絡服務：機台、料號與生產紀錄（C4、C5），存在 Apache AGE；首頁的 AI 個人化與公司概況（L3）；對話內回報客服（J10）；報表（碳排月報、排程報表，共用警報服務的排程與 Email）；Grafana 與 Sentry；閘道器日誌上傳；遠端維護通道（D6 的 [#134](https://github.com/Laysi/trustiot/issues/134)）；SaaS 正式上線：從零重建主機的計時演練並訂出恢復時間目標（M8）、不跑在同一台主機的外部監控、舊系統的型號定義匯出成初始資料（決策 3）、舊 stage 下線 | 不用到現場就能更新與維護閘道器，壞掉的版本自動回滾；AI 寫的規則通過回測後自動啟用並送出通知；換料後 AI 能依機台與料號算出每件耗電；以含實際 graph 與角色的備份還原到乾淨叢集，AGE extension、角色、權限、資料與查詢隔離都正確；從零重建主機的計時演練完成並訂出恢復時間目標；主機、入口或 Cloudflare Tunnel 故障時外部監控會通知人；舊系統的型號定義已匯入成初始資料；舊 stage 已下線 |
| MS5 知識與記憶 | RAG；附件（J7）；影音（附件與知識庫，Gemini 經 Vertex AI，決策 32）；LLM 自由寫 Cypher（每租戶 graph 配角色）與 AI 自產記憶；C4 的 AI 抽取與候選關係 | 文件與影音題庫達到門檻；注入測試全部不執行、不外洩、不越權；Opus 5.5 產生的跨租戶 Cypher 攻擊全數被資料庫擋下 |
| MS6 商業化與企業地端 | 自助註冊（A5）；方案的具體數字、價格、金流與訂閱狀態（B3；權益模型在 MS2）；MCP（J9）；OIDC、SAML；離線安裝包；租戶匯出與匯入；其餘閘道器進階安全（D6：閘道器的 secure boot、唯讀根目錄、復原金鑰託管、雙人核准、出廠映像），與整機 A/B 更新一起設計；SaaS 全部完成後補上的地端項目（Laysi/trustiot-v2#446） | 依第一個簽約客戶的需求訂 |

- **關鍵路徑是 MS1**：自建 CA、TPM 與端到端確認風險最高，AI、警報與畫面也都要用它送上來的資料。MS0 先把窄表與資料格式定下來，MS3 就能用 lab 錄下的資料與 MS1 同時進行。
- **剩下的可行性測試各擋一個里程碑**：Istio ambient 在 k3s 與 Cloudflare Tunnel（含 MQTT 的 WebSocket 轉接）已於 2026-10-10 完成，TPM mTLS 與憑證換發已於 2026-10-12 完成，閘道器上的雙層 TLS 與閘道器的記憶體用量（評估採購規格）已於 2026-10-13 完成；OTA 自動回滾，以及 Apache AGE 的租戶隔離與 LLM 寫 Cypher 的成功率，都在 MS4 前完成；RAG 與影音題庫在 MS5 前完成。警報「觸發次數異常多」的門檻在 MS4 前決定。
- **分工**：MS0 的契約（資料格式、schema、API 規格、測試框架）一次定好；之後 MS2 的 CRUD 與 SPA 依規格與自動測試驗收，不需要逐項人工審查；MS1 的邊緣與 MS3 的 agent 是最難的兩塊。
- **SaaS 上線併入 MS4**（2026-10-10 決定）：SaaS 對外與現場閘道器正式接上是同一件事，所以標示「SaaS 上線前完成」的工作（從零重建計時演練、外部監控）都是 MS4 的完成條件；決策 3 的型號定義匯出與舊 stage 下線也在 MS4。
- **舊系統**：不再投入，不修補也不止血；MS3 達成後新系統即具備舊系統的功能。
- **企業地端的額外需求延到 MS6**：但 MS0 的 SaaS 與地端兩份 values 與 MS1 的自有網域規則從一開始就要守住，之後才搬得動。
- **地端排在 SaaS 全部完成之後**（2026-10-18 決定）：MS1–MS5 還沒開始的地端專用工作，以及各 issue 裡的地端驗收條件，都等 SaaS 全部完成後再做，清單集中在 Laysi/trustiot-v2#446（MS6）。照常保留、不等 SaaS 完成的有三類：既有的地端檢查（CI 裡 chart 對 values-onprem 的測試與 nightly 以兩份 values 部署的冒煙測試，以及在 lab 跑的 CloudNativePG onprem 形態 e2e）；[部署與可搬移性](../architecture/deployment.md)的四條可搬移規則，新程式一樣要遵守；同時是 SaaS 退路的部分，例如自架 Qdrant 的 CI 路徑（[知識與文件庫](../architecture/rag.md)「SaaS 用 Qdrant Cloud 的前提」第 4 點）。

## 第一批可行性測試

這十五項驗證後的結果會決定後續實作細節，已完成十一項，其餘四項排在對應的里程碑之前（見上方里程碑表下的說明）。

- [x] MQTT broker 選型（首選 Mosquitto）：驗證 mTLS、憑證撤銷清單、依身分限制可用的 topic，以及能否在 443 提供服務。
- [x] Istio ambient 在 k3s 上：設 global.platform=k3s 後 CNI 與 ztunnel 正常運作；入口 gateway 以 ext\_authz 把 cookie 交給身分服務並回傳內部身分；AuthorizationPolicy 依 ServiceAccount 擋下未允許的服務呼叫；istiod、ztunnel 與入口 gateway 的記憶體用量。結果見[可行性測試結果（2026-10-10）](../feasibility/2026-10-10-edge-poc.md)。
- [x] Cloudflare Tunnel：主機上的 cloudflared 把所有產品流量交給入口 gateway 的 ClusterIP（不開主機埠），入口只接受本機來源，cloudflared 在 systemd 沙箱設定下正常運作；閘道器的 mTLS 包在 WebSocket 內，經 tunnel、入口 gateway 與 MQTT 轉接服務連到 broker，客戶端憑證、CRL 與 dynsec 照常運作；WebSocket 閒置多久會被切斷（MQTT keepalive 要設多短）；Cloudflare 維護時斷線後自動重連，並靠離線緩衝補送；HTTP 串流在 125 秒逾時下靠心跳維持。結果見[可行性測試結果（2026-10-10）](../feasibility/2026-10-10-edge-poc.md)；閘道器上的雙層 TLS 拆成下一項。
- [x] 雙層 TLS 在閘道器上的 CPU 用量與延遲（從 Cloudflare Tunnel 一項拆出）：WebSocket 內包 mTLS 在 lab 的閘道器上，以 SaaS 與地端兩條路徑、方案的採樣頻率實測，CPU 與延遲都遠在可接受範圍內；兩層 TLS 都把 ChaCha20-Poly1305 排第一並開 session resumption。結果見[可行性測試結果（2026-10-13）](../feasibility/2026-10-13-double-tls-gateway.md)。
- [x] 閘道器 Rust 程式用 TPM 金鑰做 mTLS：先試 rustls 外部簽章，不行就改走 tpm2-pkcs11；並驗證用 TPM 金鑰簽伺服器挑戰以換發憑證，以及資料分割區的 LUKS 由 TPM 自動解鎖。2026-10-12 在 lab 的閘道器通過，採用 rustls 外部簽章，結果見[可行性測試結果（2026-10-12）](../feasibility/2026-10-12-tpm-poc.md)。
- [x] 閘道器（trustiot-gw）的記憶體用量：在 lab 的閘道器上量 OS 基線，加上 Rust 閘道器程式（WebSocket 內的 mTLS、MQTT、離線緩衝、Modbus 輪詢）在不同設備數與緩衝量下的尖峰，用來評估閘道器採購的記憶體規格。結果見[可行性測試結果（2026-10-13）](../feasibility/2026-10-13-gateway-memory.md)：OS 基線約 160 MiB，程式尖峰約 9 MiB 加緩衝上限；建議 G1 的記憶體上限預設 16 MiB、上限 64 MiB，採購最低 1 GB、建議 2 GB（2026-10-14 使用者同意）。
- [x] 薄的 OpenAI 相容 client 搭配 vLLM、Ollama：工具呼叫、用量、推理欄位、串流取消、區分 429 與逾時，以及地端模型做 SQL 分析的品質。
- [x] 警報腳本 Rhai 與 WASM 比較：AI 寫對的比例、沙箱限制（規則改為只在雲端執行，不需在閘道器上測）。
- [ ] 自建 OTA 在 lab 的閘道器上驗證自動回滾。
- [x] RAG：embedding 與 reranker 改用 API（地端接客戶端點），並驗證自架 Qdrant 的檢索品質與備份還原；測試資料涵蓋掃描頁、小字故障碼表、中英混排。向量處理已在 knowva 專案驗證，Qdrant 部分見[可行性測試結果](../feasibility/2026-10-09-cloud-poc.md)。
- [ ] RAG 檢索題庫：用自己的手冊建 60–100 題，比較「只嵌入頁面圖」與「LLM 預處理加三路索引」、重排方式（Decisions API 逐頁打分與每批候選數、Haiku 5.5 一次排序），並決定嵌入維度（768 或 1536）；地端的 VL reranker 與 EmbeddingGemma 2 的比較排在 SaaS 完成後。
- [ ] 影音題庫：20–30 支影片或錄音，配上有標準答案的事實題，比較 Gemini 3.8 Flash 與 Gemini 4 Argon、解析度、取樣率與切段長度，並量每題成本；在 MS5 前完成（決策 32）。
- [x] Cypher 解析與租戶改寫器（在自架 Neo4j 2026.09 Community 上實測；2026-10-09 改用 Apache AGE 後不再需要改寫器）：評估可用的 Rust Cypher 解析器或自寫，用對抗性查詢驗證租戶隔離。
- [ ] Apache AGE：每租戶一個 graph 配專屬登入角色的隔離（逐項測[現場脈絡與知識圖](../architecture/field-context.md)護欄第 5 條的權限路徑，攻擊查詢由 Opus 5.5 產生）、LLM 在 AGE 上寫 Cypher 的成功率、含 AGE 的 Postgres 映像。
- [x] 單節點資源實測：整套 k3s 堆疊在目標主機上的記憶體、CPU 與磁碟用量，訂出 SaaS 與地端的最低規格。

2026-10-09 已在 Mac mini M1 實驗機測完第一批 8 項的雲端側，結果見 [可行性測試結果](../feasibility/2026-10-09-cloud-poc.md)。TPM 與 OTA 依指示只測雲端側而未測，TPM 於 2026-10-12 在 lab 的閘道器補測，結果見[可行性測試結果（2026-10-12）](../feasibility/2026-10-12-tpm-poc.md)；RAG 的 embedding 與 reranker 已決定改用 API，向量處理已在 knowva 專案驗證。Istio ambient、Cloudflare Tunnel、Apache AGE 與 RAG 題庫是後加的項目；2026-10-10 測完 Istio ambient 與 Cloudflare Tunnel，結果見[可行性測試結果（2026-10-10）](../feasibility/2026-10-10-edge-poc.md)；2026-10-13 在 lab 的閘道器上測完雙層 TLS 與閘道器的記憶體用量，結果分別見[雙層 TLS 的可行性測試結果](../feasibility/2026-10-13-double-tls-gateway.md)與[閘道器記憶體用量的可行性測試結果](../feasibility/2026-10-13-gateway-memory.md)；Apache AGE 與 RAG 題庫尚待驗證。
