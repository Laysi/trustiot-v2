# 輸入框與提問的驗證（2026-11-04）

對應 issue：Laysi/trustiot-v2#1044（對話首頁的輸入框與提問：送出、等待回答、停止與錯誤）。後端是 Laysi/trustiot-v2#646（提問 API，2026-11-04 併入 main `c12a2d64`）。畫面規格與和設計稿不同的地方在 `docs/design/chat-dashboards/design.md` 的「輸入框與提問」，程式的說明在 `frontend/README.md` 的「提問」。

## 環境

- 建置主機（arm64 Linux）。程式是 `feat/1044-composer` 分支，基底是 main `922dda9f`；Bun、Rust 與其他工具是 `.prototools` 釘選的版本。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像（`mcr.microsoft.com/playwright:v1.64.1-noble`，以 digest 釘選，linux/arm64）裡的 Chromium，`--network host`、丟掉所有 capability；語言 en-US、時區 UTC；桌機 1280 × 900 的淺色主題，最後一段是手機 390 × 844 的深色主題。
- API 是真的：`services/assistant/tests/spa_e2e_1044.rs` 的測試（被忽略的測試，不進 repo，副本是 `data/.../scripts/spa_e2e_1044.rs.txt`）在容器裡啟動 Postgres（TLS，migration Job 套用產品的 schema），以產品的方式啟動 assistant 執行檔，驗證每個請求的內部身分 token。**模型是替身，沒有任何真的模型呼叫**：一個替身 AI 閘道（問題含 `[hold]` 時，模型呼叫停在那裡直到被取消；含 `[fail 500]` 時回 500；其餘回錄好的回答）與一個替身身分服務（核發與計數提問的 run 委派）。
- 預覽頁（不進 repo，副本在 `data/.../scripts/`）：以 `frontend/vite.config.ts` 的設定建置 SPA 真正的 `App` 與 `ROUTES`，對話與儀表板的 API 用真的 `conversationApi(createApiClient())` 與 `dashboardApi(...)`；身分服務以測試用的記憶體版本代替（編輯者角色，工作階段不是這次要驗的）。一個 Bun 伺服器提供建置結果，每個回應帶 `services/web/src/headers.rs` 的 `CSP`（強制）與 `CSP_REPORT_ONLY`（Trusted Types，只回報），並把 `/api/assistant/` 轉給 API、補上對話擁有者的內部身分 token。頁面載入前註冊 `securitypolicyviolation` 的收集器，也收主控台的錯誤與警告、頁面例外與所有送往其他來源的請求。

## 指令

```sh
# 前端的檢查，repo 根目錄
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
bun install --cwd scripts --frozen-lockfile && bun test scripts
cargo test --locked -p repo-check

# 端對端：把 data/.../scripts/ 的 spa_e2e_1044.rs.txt 放成 services/assistant/tests/spa_e2e_1044.rs，
# 其餘檔案（去掉 .txt）放進 frontend/.preview-1044/，然後在 repo 根目錄
bash frontend/.preview-1044/run.sh
```

## 結果

原始資料：[e2e-result.json](data/2026-11-04-1044-composer-asking/e2e-result.json)（每一步看到的內容、違規、主控台、其他來源的請求、經過代理的 API 請求）、[final-state.json](data/2026-11-04-1044-composer-asking/final-state.json)（瀏覽器流程後 API 的列表與每個對話）、[proxied-api-requests.txt](data/2026-11-04-1044-composer-asking/proxied-api-requests.txt)。截圖不附（repo 不放圖檔）。

### 端對端的流程

| 步驟 | 畫面 | API |
| --- | --- | --- |
| 首頁 | 輸入框「Ask TrustIoT」；送出鈕在輸入框空白時停用；還沒有對話，所以沒有「最近的對話」 | `GET …/conversations?limit=5` |
| 在首頁提問「Did every meter on floor 3 report?」（Enter） | 打開對話頁；串列有問題與回答（「Yes. All twelve meters on floor 3 reported in the last hour.」）；輸入框是空的 | `POST …/conversations` 201、`POST …/questions` 202；`latest_run` 是 `answered`，標題是問題的前面（`provisional`） |
| 提問「And the next floor? [hold]」 | 問題顯示在串列最後，旁邊是「Looking up readings…」；沒有送出鈕，只有「Stop」 | 替身閘道的呼叫停在閘門 |
| 按「Stop」 | 「Stopped. Nothing of this question was kept…」；問題文字放回輸入框；串列仍是一個回答 | `POST …/runs/{id}/cancel` 202；`latest_run` 是 `cancelled`、`stop_requested_at` 有值、`message_key` 是 `question.cancelled`；訊息仍是 2 則 |
| 提問「Break it [fail 500]」 | 警示寫「The assistant is unavailable right now. Please try again later.」；問題放回輸入框 | `latest_run` 是 `failed`、`result` 是 `model_unavailable`、`message_key` 是 `question.model_unavailable`；整段對話的回應裡沒有替身閘道的錯誤文字（`stand-in failure`） |
| 再提問「Second question」 | 回答出現，警示消失；停止與失敗之後歷史照常往後加（4 則訊息） | `latest_run` 是 `answered` |
| 送出 16,001 個字元 | 警示「The question was not sent: Enter a question of 1 to 16,000 characters. Your text is still in the box.」；16,001 個字元都還在輸入框 | `POST …/questions` 422（欄位 `/question`） |
| 提問「Delete me while it runs [hold]」，再從選單刪除 | 確認視窗寫「…deleting stops it, and its answer is not kept.」；刪除後回到對話紀錄 | `DELETE` 204；對話在已刪除清單 |
| 儀表板列表的「Create in conversation」 | 打開首頁，輸入框是「Create a dashboard: 」，沒有送出任何 POST；網址乾淨 | |
| 網址 `/?q=<img src=x onerror=alert(1)> Change the dashboard` | 輸入框裡是同樣的文字，沒有任何元素（`img[src='x']` 0 個） | |
| 手機、深色：首頁與對話頁 | 輸入框在視窗內、沒有水平溢位；對話頁的輸入框與送出鈕同一列；頂列的「New conversation」回到首頁的輸入框 | |

替身閘道共收到 6 次模型呼叫、替身身分服務核發 6 個委派（`final-state.json`）。

### CSP、Trusted Types 與主控台

首頁、對話頁（提問、停止、失敗、被拒絕、刪除）、儀表板列表、首頁的網址帶入問題，以及手機寬度深色主題的首頁與對話頁：`securitypolicyviolation`（強制的與 Trusted Types 的只回報）0 件，頁面例外 0 個，送往其他來源的請求 0 個。主控台只有一則錯誤：被拒絕的那個 422 請求的「Failed to load resource」，是瀏覽器對失敗回應的固定記錄，不是程式的例外。

### 前端與其他檢查

在建置主機上，樹含本分支的全部變更（日期 2026-11-04）：

| 指令 | 結果 |
| --- | --- |
| `api:check`、`typecheck`、`lint`（Biome）、`build`、`check:dist` | 通過 |
| `test` | 1078 個通過、0 個失敗 |
| `bun test scripts` | 2024 個通過、0 個失敗 |
| `cargo test --locked -p repo-check` | 通過（26 與 11 個測試） |

等待的測試（問題在背景回答，頁面每 1.5 秒重讀一次）用「等到條件成立」的上限 60 秒，不靠固定的短時間：建置主機負載很高的一次執行裡，兩個等待的測試在舊的 20 秒上限逾時（單獨與整套各重跑兩次都通過），上限因此放寬。

單元測試（`renderAt` 加 `src/test/fake-questions.ts`，在 `FakeConversationServer` 上加提問端點，回應照 `api/openapi.json`）涵蓋：

- `ConversationTurn.test.tsx`：送出後問題、等待的一行與停止鈕；回答出現在前面的訊息之後；Enter、Shift + Enter 與輸入法組字中的 Enter；空白不能送；送出中送出鈕停用、文字留著；送不出去的各種原因（403、429、503、409 歷史已滿、422 超過上限、沒有連線、另一個分頁已有問題在回答）都留下文字並說明原因；停止（正在停止、已停止、問題放回輸入框、歷史不變）、停止失敗、停止時回答剛好完成；失敗與中斷的固定訊息（含前端不認得的鍵）與下一個問題照常；開啟時已在回答中的問題；網址帶入問題；沒有問題的對話；回答中刪除。
- `HomeComposer.test.tsx`：首頁提問後打開對話頁、顯示問題與回答；送不出去時文字留著，重試沿用已開始的對話；開始對話失敗；送出中的狀態；網址帶入問題與標記文字。
- `StartInConversation.test.tsx`（儀表板）：兩個入口、檢視者、封存的儀表板與草稿沒有入口。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 在首頁提問後打開對話頁，回答完成後顯示在提問下方（測試） | 通過：`HomeComposer.test.tsx`；端對端（真的 API） |
| 停止之後回答不再增加，畫面顯示已停止（測試） | 通過：`ConversationTurn.test.tsx`；端對端（`latest_run` 是 `cancelled`，訊息數不變） |
| 送出失敗時輸入的文字仍在輸入框，錯誤訊息說明原因（測試） | 通過：`ConversationTurn.test.tsx`、`HomeComposer.test.tsx`；端對端（422） |
| 儀表板頁的「在對話中編輯」與列表的「在對話中新增」打開對話並帶入問題（測試） | 通過：`StartInConversation.test.tsx`；端對端（列表的入口與網址帶入） |
| 以展示環境或本機的真 assistant 與真模型實測一次提問與停止，結果存進 `docs/test-reports/` | **部分**：真的 assistant（容器裡的 Postgres）與真的公開 API 測了提問、停止、失敗、刪除；模型是替身，沒有真的模型呼叫。用真模型的一次功能性呼叫要展示環境的閘道金鑰，留給展示驗收（Laysi/trustiot-v2#459） |
| `frontend` 的 `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist` 與 CI 通過 | 本機的建置主機通過（上表）；CI 在 PR 上 |

沒有涵蓋：Chromium 以外的瀏覽器、真的手機觸控（以 390 px 的視窗代替）、zh-TW 介面的端對端（單元測試涵蓋兩種語言的字串）、多個副本下的停止（屬於 Laysi/trustiot-v2#646 的 P2 驗證）、串流（Laysi/trustiot-v2#1046）。
