# 不帶進新系統的東西

以下功能與元件在重建時捨棄；有替代做法的寫在冒號後。

- 沒有租戶隔離的共用文件庫（addDocs、getDocs），以及 AI 分類中的「文件」類型。
- 沒在用的 AI 流程（test\_get\_docs、openai\_reasoning、test\_gen\_ui，目前登入使用者仍可直接呼叫）、AI 記憶工具、mcp-clickhouse。
- 手動建立閘道器與下載安裝包：閘道器都是我們出廠的硬體，只留出廠登錄一條佈建路徑；由雲端產生私鑰的安裝包也違反「金鑰只在 TPM 內」的原則。
- 出廠登錄的單筆畫面與 CSV 批次登錄：只留產線工作站呼叫的 API。
- `/dashboard/config` 除錯頁（租戶設定 JSON 檢視）。
- 停權期間每 60 秒重查、恢復後自動解鎖：停權改為 API 回 403 時顯示說明頁。前端每 45 分鐘續期 session：改由伺服器延長期限。
- 超額或停權時保存被擋下的訊息：直接拒絕，草稿留在前端。
- 編輯已送出的訊息、在重新產生的不同版本之間切換：只留重新產生最後一則回答。
- 資料點編輯器的 YAML 模式：保留表單與 JSON 匯入匯出。
- 設備與型號的數量配額：只留 AI 額度與閘道器數量。
- 自訂的綁定失敗計數與鎖定：改用 API 閘道的通用限流。
- 模擬器（5 種 Modbus 設備）、行為模擬庫、測試資料產生器（iot-data-generator）與 mock 設備類型：自動測試改在測試程式內起假端點，AI 開發與警報回測用 lab 錄下的資料，並在測試程式內插入人工異常片段（見 M7）。
- 警報規則在閘道器上執行：只在雲端執行，日後放回邊緣的條件見[AI 警報規則](../architecture/alert-rules.md)。
- AI 自產記憶的核可流程，以及回答層級的第 2.5、3 層。
- 網路搜尋草稿層（[#6](https://github.com/Laysi/trustiot/issues/6)）：有客戶需要再做。
- 舊版單一設定檔範例：`appconfig.json`、`example.ts`、`update_appconfig.sh`。
- AWS 託管服務：AppConfig、EventBridge、Lambda、SQS、Cognito、Greengrass、IoT Core 與 Shadow、IoT Fleet Provisioning（claim 憑證與佈建 hook）、API Gateway、Secrets Manager、S3、SSM、CloudWatch；以及依賴它們的 aiot-dev 與 AWS 維運腳本。
- NodeRuntime 以 `curl | bash` 安裝 Node 的方式，以及 `com.laysi.*` 舊元件名稱。
- 舊聊天紀錄與 stage 上的所有資料（型號定義除外）。
