# 資料來源面板的收起：Escape、再按一次、aria-expanded 與圖的 tooltip

對應 issue：`Laysi/trustiot-v2#1201`。依據：`docs/design/chat-dashboards/design.md`「元件與互動」的資料來源面板（Escape 或關閉鈕收起，焦點回到觸發它的數字；再按一次同一個觸發點也會收起）；展示劇本步驟 3、4（PR `Laysi/trustiot-v2#1146`）。

## 環境

- 建置主機。
- 瀏覽器：`frontend/e2e/image.ts` 釘住的 Playwright 映像（Chromium），zh-TW、Asia/Taipei、縮放 100 %、淺色主題。視窗（CSS 像素）：1280×720 與 1920×1080。
- 頁面：UI 預覽（`vite --mode ui-preview`）。對話頁的回答卡片（KPI「Energy, week of Oct 26」與圖「Hourly average power on Wednesday」）只在 `Laysi/trustiot-v2#1195` 的預覽改動裡（分支 `feat/1195-two-column-walkthrough` 的 `e976c301`），本分支不含它們。瀏覽器實測把那個提交裡預覽專用的檔案（`frontend/src/test/fake-conversations.ts`、`frontend/src/test/fake-dashboards.ts`、`frontend/src/ui-preview/` 下的 `collect.ts`、`contract.ts`、`controls.ts`、`preview.tsx`）以 `git apply` 疊在工作樹上量，量完還原，沒有提交；單元測試、型別檢查、lint、建置都在沒有那些檔案的最終樹上跑。草稿面板沒有量：它與回答裡的卡片用同一個 `DashboardComponent`，開關的程式相同，單元測試以同一個元件涵蓋。
- 樹：
  - 修正前：本分支的起點 `09fbf9e9`（`Laysi/trustiot-v2#1184` 的分支，含 `Laysi/trustiot-v2#1178`、`Laysi/trustiot-v2#1197`、`Laysi/trustiot-v2#1190` 與 `#1184` 的修正），加上述預覽檔案。
  - 修正後：同一棵樹加上本次的變更。「修正前」是把本次改到的五個元件檔（`DashboardComponent.tsx`、`values.tsx`、`SourcePanel.tsx`、`custom-page/CustomPageFrame.tsx`、`chart/InteractiveChart.tsx`）換回起點的版本再量，其餘（包括腳本）完全相同。
- 兩次用同一份腳本（本報告資料目錄的 `browser-check.mjs`、`run.sh`，改自 `Laysi/trustiot-v2#1184` 的走查腳本）。腳本不下判斷，只記錄每一步之後的狀態，由本報告讀。

## 指令

從工作樹的根目錄（在建置主機上以 `CI=1` 執行）：

```
bun install --cwd frontend --frozen-lockfile
bash docs/test-reports/data/2026-11-07-1201-source-panel-close/run.sh
```

`VIEWPORTS=1280x720` 只跑一個視窗，`SHOTS=1` 截圖（只給審查者看，不進 repo）。資料在 `docs/test-reports/data/2026-11-07-1201-source-panel-close/`：`browser-check-before.jsonl`（修正前）、`browser-check-after.jsonl`（修正後），每一步一行，共 36 步 × 2 個視窗。

前端檢查（同一個工作樹，沒有預覽檔案）：

```
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
```

腳本在每個視窗跑兩個頁面、各一張 KPI 卡片與一張圖卡片：儀表板頁「Plant energy overview」（KPI「Energy this week」、圖「Power by line」）與對話頁預覽對話的最後一則回答。每一步前先把卡片的頂端捲到頂端列下方（簡報者先指出元件、再按）。情境：

- KPI（儀表板頁與回答各一）：按數字再按 Escape；按數字再按一次數字；按數字再按卡片的「資料來源」；鍵盤（聚焦數字按 Enter，再聚焦數字按 Enter）。
- 圖（儀表板頁與回答各一）：滑鼠移到圖上、點資料點，再用滑鼠按關閉鈕，然後把滑鼠移開；點資料點再按 Escape；點資料點再點同一個點；按卡片的「資料來源」、再以鍵盤（先按一個鍵）聚焦圖，按 Escape。

每一步之後記錄：面板是否打開、焦點在哪（是否在面板裡）、圖的 tooltip 文字（沒有則 null）、卡片裡 `aria-expanded` 為 `true` 與 `false` 的觸發點各幾個。

## 結果

沒有任何一步失敗，兩次各 72 步（36 步 × 2 個視窗）都沒有 console 錯誤；1280×720 與 1920×1080 的每一步結果都相同。下表是一個視窗的結果（另一個視窗相同）；「儀表板頁」與「回答」兩處的結果相同的列合併，不同處另外寫。

| 情境 | 修正前 | 修正後 |
| --- | --- | --- |
| KPI：面板裡按 Escape | 面板仍開著，焦點在面板標題 | 面板收起，焦點回到數字 |
| KPI：再按一次數字 | 面板仍開著，焦點移到數字 | 面板收起，焦點在數字 |
| KPI：鍵盤 Enter 再按一次 | 面板仍開著 | 面板收起，焦點在數字 |
| KPI：打開後按卡片的「資料來源」 | 面板仍開著，焦點在標題 | 儀表板頁：面板收起（數字與按鈕是同一個來源），焦點在按鈕；回答：面板仍開著（數字指向被點的列、按鈕指向整個資料集，是另一個來源，所以換成它），焦點在標題 |
| KPI：`aria-expanded`（數字、比較值、卡片按鈕） | 沒有 | 打開時儀表板頁 2 個 `true`、1 個 `false`（數字與卡片按鈕指向同一個來源）；回答 1 個 `true`、1 個 `false`；收起後全部 `false` |
| 圖：點資料點打開、滑鼠按關閉鈕 | 面板收起，焦點在圖，**圖跳出最新一點的 tooltip**（儀表板頁 2026-11-01 00:45 Line B；回答 2026-10-28 23:00），滑鼠移開後仍在 | 面板收起，焦點在圖，**沒有 tooltip**（滑鼠還在原位與移開後都是） |
| 圖：點資料點打開、Escape | 面板仍開著 | 面板收起，焦點在圖，顯示最新一點的 tooltip（鍵盤回到圖的行為，與 Tab 相同） |
| 圖：同一個點再點一次 | 面板仍開著 | 面板收起，焦點在圖，tooltip 是滑鼠所在的那一點 |
| 圖：按卡片的「資料來源」打開（`aria-expanded`） | 沒有 | 按鈕 1 個 `true`；圖本身（`role="slider"`）沒有 |
| 圖：面板開著、鍵盤聚焦圖，按 Escape | 圖的 tooltip 消失，面板仍開著 | 同左：tooltip 消失，面板仍開著（面板的 Escape 只在焦點在面板裡處理） |

重點：

- 修正前的「點資料點、滑鼠關閉」重現了 issue 描述的問題（最新一點的 tooltip），修正後兩個頁面、兩個視窗都沒有。
- 用鍵盤收起（Escape、或鍵盤按關閉鈕）仍在圖上顯示最新一點，與 Tab 回到圖相同；單元測試涵蓋鍵盤按關閉鈕。
- 圖聚焦時的 Escape 沒有被面板吃掉，也不收面板。
- 圖的點不是按鈕，所以圖本身沒有 `aria-expanded`（`role="slider"` 不支援它）；卡片的「資料來源」按鈕有。這與其他和設計稿不同之處一起寫在 `docs/design/chat-dashboards/design.md` 的「和設計稿不同的實作（Laysi/trustiot-v2#1201）」。
- 自訂頁面（`CustomPageFrame`）沒有在瀏覽器量：預覽沒有自訂頁面；它的沙箱 frame 與訊息協定沒有改，單元測試以標題列的來源按鈕、頁面回報的 `source` 訊息與焦點回到 frame 涵蓋。

## 單元測試

- `src/dashboard/DashboardComponent.test.tsx`「closing the source panel (Laysi/trustiot-v2#1201)」：面板裡按 Escape 收起並把焦點還給打開它的值；面板裡任何控制項按 Escape 都收起，已經用掉這個鍵（`preventDefault`）的不收；其他鍵不收；圖聚焦時按 Escape 取消逐點查看、面板仍開著；再按一次同一個值、卡片的來源按鈕收起，焦點留在被按的那個；同一張卡片的另一個值換成那個來源，再按它收起；點擊沒有把焦點給按鈕的瀏覽器（Safari）也收起、不搶焦點；`aria-expanded`（KPI 的數字、比較值、卡片按鈕；表格的格子；狀態元件的門檻）；圖：滑鼠打開、滑鼠關閉沒有 tooltip，Escape 與鍵盤按關閉鈕關閉則顯示最新一點，同一個點再點一次收起、另一個點換來源。
- `src/dashboard/custom-page/CustomPageFrame.test.tsx`「closing」：Escape 收起並把焦點還給卡片的來源按鈕；再按一次來源按鈕收起，`aria-expanded` 跟著變；頁面回報同一個標記兩次收起、另一個標記換來源；頁面打開的面板收起後焦點回到 frame。
- 既有的 `SourcePanel.test.tsx`、`InteractiveChart.test.tsx` 與焦點測試維持原樣通過。

## 結論

- 驗收 1（焦點在面板裡按 Escape，面板收起、焦點回到打開它的值或按鈕；焦點在圖上時 Escape 仍是取消逐點查看）：通過。KPI 與圖、兩個視窗、兩個頁面的 Escape 實測見上表；圖聚焦按 Escape 的實測與單元測試。
- 驗收 2（再按一次同一個觸發點收起；同一張卡片的另一個值換成那個來源）：通過。數字、卡片按鈕、圖上的同一個點都實測；另一個值換來源見單元測試與回答裡的「數字再按卡片按鈕」（來源不同，換成它）。
- 驗收 3（觸發點有 `aria-expanded`，打開時是 `true`）：通過，範圍是按鈕觸發點（數字、比較值、門檻、表格的格子、卡片的來源按鈕、錯誤提示與空狀態裡的來源按鈕）；圖的點與自訂頁面 frame 裡的標記沒有，原因寫在 `design.md`。
- 驗收 4（滑鼠從圖上的點打開、按關閉鈕收起，圖不會跳出最新一點的 tooltip）：通過，兩個頁面、兩個視窗實測：修正前有、修正後沒有。
- 驗收 5（單元測試；UI 預覽的瀏覽器實測，zh-TW，1280×720，記錄在測試報告）：通過，見上。
- 驗收 6（`frontend` 的 `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist`）：見下節；CI 在 PR 開出後由協調者讀。

## 前端檢查

最終樹上，在建置主機上跑（`CI=1`）：

| 指令 | 結果 |
| --- | --- |
| `bun run --cwd frontend api:check` | 通過 |
| `bun run --cwd frontend typecheck` | 通過 |
| `bun run --cwd frontend lint` | 通過（647 個檔案，沒有修正） |
| `bun run --cwd frontend test` | 2029 個測試、138 個檔案、8 個分片全部通過。另有一次完整執行有 1 個失敗：`ConversationAsk.test.tsx`「pressing one asks in the same conversation…」，與本變更無關（沒有碰那個頁面，單獨重跑 7 個測試全過、再跑一次完整測試全過），判斷是負載高時的時序問題，沒有為它改任何東西 |
| `bun run --cwd frontend build` | 通過 |
| `bun run --cwd frontend check:dist` | 通過 |
