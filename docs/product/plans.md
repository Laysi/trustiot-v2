# 方案與權益

方案分四級，以權益定義（B3）：基本方案好用，升級牆看得見。下表的數字只是舉例，具體數字與價格在 MS6 訂；系統實際使用的值與欄位見下方「權益清單」。底線見 B3：方案限制不擋資料寫入、不停用已啟用的警報、不無預警刪資料，超過上限時只擋新增。

| 權益 | Starter | Business | Enterprise | Enterprise On-Prem |
| --- | --- | --- | --- | --- |
| 對象 | 小廠、單一產線 | 中型廠、多產線 | 集團、多廠（SaaS） | 自建機房的集團 |
| 閘道器數 | 1 | 5 | 依合約 | 依合約 |
| 可查期間：原始資料 | 例：7 天 | 例：90 天 | 平台保存期全開（180 天） | 客戶自訂 |
| 可查期間：彙總資料 | 例：3 個月，看不到年增率 | 例：3 年 | 平台保存期全開（1 分鐘 13 個月、1 小時 4 年） | 客戶自訂 |
| 最短採樣間隔 | 1 分鐘 | 10 秒 | 1 秒 | 1 秒 |
| 即時圖表 | — | ✓ | ✓ | ✓ |
| AI 額度 | 小 | 中 | 依合約 | 接客戶自己的 LLM 端點，不設額度 |
| 進階 AI（AI 寫警報規則、現場脈絡與每件耗電、文件 RAG） | — | ✓ | ✓ | ✓ |
| 警報規則數與通道 | —（不含警報） | 多，Email 與 Webhook | 不限 | 不限 |
| 成員數 | 少 | 中 | 依合約 | 依合約 |
| 報表（碳排月報、排程報表） | — | ✓ | ✓ | ✓ |
| API、MCP | — | — | ✓ | ✓ |
| SSO（OIDC、SAML） | — | — | ✓ | ✓ |
| 匯出自己的原始資料（CSV） | ✓ | ✓ | ✓ | ✓ |

- 匯出可查期間內的自己資料各方案都可以，不設牆，客戶不會覺得資料被扣住；報表與 API 才是升級牆。
- Starter 的彙總資料只能查約 3 個月，想看去年同期就要升級；資料其實都在，升級後立即看得到。
- 實際保存期限全平台統一，原始資料 180 天（[資料保存期限與磁碟水位](../architecture/data-retention.md)，2026-10-21 訂）。Enterprise 可查到全部保存期限，其他方案的可查期間都比它短；MS6 訂各方案天數時不能超過保存期限。
- 彙總資料的保存期限也全平台統一，1 分鐘彙總 13 個月、1 小時彙總 4 年（2026-10-25 訂，同一份文件）。可查期間依解析度分別套用：超過 13 個月的範圍只有 1 小時的解析度，Business 例的 3 年由 1 小時彙總涵蓋；MS6 訂的彙總可查期間，1 分鐘不能超過 13 個月、1 小時不能超過 4 年。
- 可查期間由 ClickHouse 強制（2026-10-26 決定，Laysi/trustiot-v2#143）：每個租戶的唯讀帳號帶每張表的時間窗設定，資料庫濾掉窗外的列，所以圖表、語意工具與 AI 寫的 SQL 都看到同一個範圍，查詢怎麼寫都繞不過。方案變更從該租戶的下一個查詢起生效：升級後立即看得到更早的資料，降級後立即看不到，資料本身不動。可查期間依讀值時間計算（保存期限依收到時間）。機制見[資料保存期限與磁碟水位](../architecture/data-retention.md)「可查期間」；目前所有租戶固定為 Starter 例的值，依方案的值由 Laysi/trustiot-v2#453 接上。
- Enterprise 的權益可依合約，對單一租戶覆寫方案預設。
- Enterprise On-Prem 只靠合約約束，不做技術上的授權檢查；權益預設全開，上限由客戶的平台管理員依合約設定。從 SaaS 搬到地端用租戶匯出與匯入工具（MS6）。
- 不做試用；Starter 本身做得像試用：1 台閘道器、不含警報、可查期間短、AI 額度小；是否收費在 MS6 訂。
- AI 額度只隨方案給，不開放加購；用完就等下個月或升級方案。platform admin 可以當月臨時調高（B2），只用在客服處理，不對外販售。

## 權益清單

身分服務管理每個租戶的方案與權益（Laysi/trustiot-v2#111；機制見 [services/identity/README.md](../../services/identity/README.md)「方案權益」）。下表是程式裡各方案的預設值（`services/identity/src/entitlements.rs` 的 `Plan::defaults`，單元測試確認兩邊一致，改一邊就要改另一邊）。數字是上表的例子值；上表沒有數字的項目先給可設定的暫定值，標「暫定」；全部在 MS6 換成正式數字。「不限」表示沒有上限；✓ 有、— 沒有。

| 欄位 | 意義 | Starter | Business | Enterprise | Enterprise On-Prem |
| --- | --- | --- | --- | --- | --- |
| `max_gateways` | 可綁定的閘道器數 | 1 | 5 | 20（暫定，依合約覆寫） | 不限 |
| `max_members` | 成員數 | 3（暫定） | 20（暫定） | 100（暫定） | 不限 |
| `max_alert_rules` | 警報規則數 | 0 | 50（暫定） | 不限 | 不限 |
| `raw_queryable_days` | 原始資料可查期間（天，1–36500） | 7 | 90 | 180 | 36500 |
| `minute_queryable_months` | 1 分鐘彙總可查期間（月，1–1200） | 3 | 13 | 13 | 1200 |
| `hour_queryable_months` | 1 小時彙總可查期間（月，1–1200） | 3 | 36 | 48 | 1200 |
| `min_sampling_interval_ms` | 最短採樣間隔（毫秒，100–86400000） | 60000 | 10000 | 1000 | 1000 |
| `realtime_charts` | 即時圖表 | — | ✓ | ✓ | ✓ |
| `email_notifications` | 警報通知：Email | — | ✓ | ✓ | ✓ |
| `webhook_notifications` | 警報通知：Webhook | — | ✓ | ✓ | ✓ |
| `ai_alert_rules` | AI 寫警報規則 | — | ✓ | ✓ | ✓ |
| `field_context` | 現場脈絡與每件耗電 | — | ✓ | ✓ | ✓ |
| `document_rag` | 文件 RAG | — | ✓ | ✓ | ✓ |
| `reports` | 報表（碳排月報、排程報表） | — | ✓ | ✓ | ✓ |
| `api_and_mcp` | API、MCP | — | — | ✓ | ✓ |
| `sso` | SSO（OIDC、SAML） | — | — | ✓ | ✓ |

- Enterprise On-Prem 預設全開、上限不限，可查期間是查詢時間窗允許的最大值（實際範圍由客戶自己的保存期限決定）；不做技術上的授權檢查，上限由平台管理員依合約以覆寫值設定。
- 覆寫只給 Enterprise 與 Enterprise On-Prem：平台管理員對單一租戶設定任一欄位，覆寫值優先於方案預設；其他方案帶覆寫值會被拒絕（API 與資料庫都拒絕）。
- AI 月額度不在權益快照裡：額度的單位與數字隨價格表訂（決策 28），由 Laysi/trustiot-v2#110 接上時再以新增的選填欄位加入。匯出自己的原始資料（CSV）各方案都可以，不是權益。

### 權益怎麼送到各服務

- 每個租戶的權益是一份快照（方案預設套上覆寫值），帶一個只增不減的版本。新租戶建立時就是 Starter 第 1 版；這個機制上線前建立的租戶，由身分服務在啟動時與每 10 分鐘補上第 1 版。
- 每次快照改變，身分服務在同一個交易寫出事件 `identity.tenant_entitlements_changed`（完整快照與版本，[事件契約](../../crates/contract/README.md)）。各服務訂閱這個事件，保留版本較大的一份，在自己的伺服器端檢查，不在請求路徑上呼叫身分服務。還沒收到任何快照的服務，對該租戶的新增與功能一律當成沒有權益。
- 平台管理員以 `GET`／`PUT /api/identity/v1/platform/tenants/{tenant_id}/entitlements` 讀取與設定租戶的方案和覆寫值；租戶成員以 `GET /api/identity/v1/entitlements` 讀自己租戶的權益，給畫面顯示升級牆。
- 方案的預設值在新版本改變時，不會自動套用到既有租戶：既有租戶保留上一個事件帶的快照，平台管理員對該租戶再設定一次同一個方案才會換成新值。這樣每次權益改變都有事件，同一個版本在所有服務看到的值都一樣。
- 拒絕的回應：功能不在方案內回 403 `quota_exceeded`、訊息鍵 `entitlement.feature_not_in_plan`、`details.entitlement` 是欄位名；新增會超過上限回 403 `quota_exceeded`、訊息鍵 `entitlement.limit_reached`、`details.entitlement` 與 `details.max`（`contract::entitlements::feature_refusal`、`limit_refusal`）。

### 底線

方案變更（含降級）只影響之後的新增與功能使用：

- 不擋資料寫入：遙測寫入、設備回報、閘道器連線都不看權益。
- 不停用已啟用的警報：降級後超過上限的既有規則照常執行，只是不能再新增（由 Laysi/trustiot-v2#167 驗收）。
- 不刪資料：可查期間縮短只讓查詢看不到窗外的資料，升級後立即又看得到；超過上限的成員、閘道器與規則都保留。
- 超過上限時只擋新增；已經超過的數量不會被減少。

### 各服務的檢查點

各檢查點在各自的 issue 實作，都在伺服器端，依收到的快照判斷：

| 權益 | 檢查的服務與時機 | Issue |
| --- | --- | --- |
| `max_gateways` | certs：客戶自助綁定閘道器時，與數量配額同一個原子檢查 | Laysi/trustiot-v2#114、Laysi/trustiot-v2#110 |
| `max_members` | identity：把使用者加入租戶時 | Laysi/trustiot-v2#110 |
| `max_alert_rules` | alerts：建立或啟用規則時（已啟用的規則不受影響） | Laysi/trustiot-v2#110、Laysi/trustiot-v2#167 |
| `raw_queryable_days`、`minute_queryable_months`、`hour_queryable_months` | telemetry-query：租戶 ClickHouse 唯讀帳號的時間窗，下一個查詢起生效 | Laysi/trustiot-v2#453 |
| `min_sampling_interval_ms` | assets：建立或修改設備的輪詢間隔時 | Laysi/trustiot-v2#116 |
| `realtime_charts` | 即時圖表雲端：開啟即時畫面時；即時頻率等於最短採樣間隔 | Laysi/trustiot-v2#120 |
| `email_notifications`、`webhook_notifications` | 通知：建立通知通道時 | Laysi/trustiot-v2#169 |
| `ai_alert_rules` | AI 寫規則的排程 agent：開始產生規則前 | Laysi/trustiot-v2#171 |
| `field_context` | AI 計算每件耗電與週期時間時 | Laysi/trustiot-v2#179 |
| `document_rag` | 文件庫：上傳文件與以文件回答時 | Laysi/trustiot-v2#41 |
| `reports` | 報表：建立排程報表時 | Laysi/trustiot-v2#173 |
| `api_and_mcp`、`sso` | 對外 API、MCP 與 SSO 的入口 | MS6，尚未開 issue |
