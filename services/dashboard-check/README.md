# dashboard-check

儀表板的發布前檢查（Laysi/trustiot-v2#150）：assistant 把 AI 寫的頁面與資料集的結果交給它，它在無頭 Chromium 執行頁面，從頁面碰不到的 isolated world 比對每個標記的值與資料，回傳判定。決定與理由見 [AI 助理與 AI 閘道](../../docs/architecture/ai-assistant.md)「儀表板」第 4 點。

目前只是 workspace 成員的骨架，服務本身、映像與 chart 隨 Laysi/trustiot-v2#150 的其餘部分加入。

- API 的型別、上限與判定格式：`crates/contract/src/api/dashboard/check.rs`。
- 沙箱檔案的路徑與 response header：`crates/contract/src/api/dashboard/sandbox.rs`。
- runtime、比對程式與檢查的流程：[frontend/sandbox](../../frontend/sandbox/README.md)。
