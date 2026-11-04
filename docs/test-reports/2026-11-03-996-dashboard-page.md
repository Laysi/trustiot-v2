# 儀表板頁與共用外框的驗證（2026-11-03）

對應 issue：Laysi/trustiot-v2#996（儀表板頁與共用外框：列表、深連結、讀取資料集、版本與封存，以元件模式畫出）。畫面的規格在 `docs/design/chat-dashboards/`（含這次補的「版本紀錄」一節）與 `docs/design/app-shell.md`，程式的說明在 `frontend/README.md` 的「應用程式外框」與「儀表板頁」。

## 環境

- 在開發者工作站，用 Docker 跑容器。程式是 `feat/996-dashboard-page` 分支（疊在 Laysi/trustiot-v2#987 的分支 `658bdd3a` 上）的前端，Bun 與其他工具的版本是 `.prototools` 的。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像（`mcr.microsoft.com/playwright:v1.64.1-noble`，linux/arm64）裡的 Chromium，視窗桌機 1280 × 900、手機 390 × 844，語言 zh-TW（端對端是 en-US），時區 Asia/Taipei（端對端是 UTC）。
- 預覽頁（不進 repo，腳本在 `data/2026-11-03-996-dashboard-page/scripts/`）：以 `frontend/vite.config.ts` 的設定建置 SPA 真正的 `App` 與 `ROUTES`，後面接測試用的記憶體伺服器（`src/test/fake-dashboards.ts`、`fake-identity.ts`），所以路由、頁面、元件與真的 ECharts 都是真的。每個回應帶 `services/web/src/headers.rs` 的 `CSP`（強制）與 `CSP_REPORT_ONLY`（Trusted Types，只回報），字串逐字相同；頁面載入前註冊 `securitypolicyviolation` 的收集器，也收主控台的錯誤與警告。
- 端對端：同一個建置，但儀表板的 API 是真的：`services/assistant/tests/spa_e2e_996.rs` 的測試（容器裡的 Postgres 與 ClickHouse，telemetry-query 的路由與 assistant 的公開 API 在同一個行程）寫入 48 筆讀數與一個儲存的儀表板；一個 Bun 伺服器提供建置結果，並把 `/api/assistant/` 轉給這個 API、補上檢視者的內部身分 token（入口會補的那個標頭）。身分服務以測試用的記憶體版本代替（工作階段不是這次要驗的）。測試與腳本不進 repo（`scripts/spa_e2e_996.rs.txt` 是副本）；瀏覽器自動化另有 Laysi/trustiot-v2#946 與 Laysi/trustiot-v2#960。

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

# 預覽頁：把 scripts/ 裡的 index.html、preview-main.tsx（改名 main.tsx）、preview.css、
# preview-vite.config.ts（改名 vite.config.ts）、preview-serve.ts（改名 serve.ts）放進 frontend/.preview/，然後
bun --bun vite build --config .preview/vite.config.ts
PORT=28901 bun frontend/.preview/serve.ts &
# 每個狀態與兩個主題、兩種寬度的截圖與 CSP、主控台、水平溢位的檢查；互動流程與對比
docker run --rm --add-host=host.docker.internal:host-gateway -v $PWD/frontend/node_modules:/pw:ro \
  -v <scripts 目錄>:/work -v <輸出目錄>:/out -w /work <Playwright 映像> node states.mjs
docker run ... node flow.mjs

# 端對端（更新資料）：先啟動測試，等它寫出 api、token 與 dashboard 檔，再啟動伺服器與瀏覽器腳本
E2E_DIR=<空目錄> cargo test -p assistant --test spa_e2e_996 -- --ignored --nocapture
E2E_DIR=<同一個目錄> PORT=28902 bun frontend/.preview/e2e-serve.ts &
docker run ... -v <同一個目錄>:/e2e node e2e.mjs
```

## 結果

原始資料：[e2e-result.json](data/2026-11-03-996-dashboard-page/e2e-result.json)、[flow-result.json](data/2026-11-03-996-dashboard-page/flow-result.json)、[states-results.json](data/2026-11-03-996-dashboard-page/states-results.json)、[token-contrast.md](data/2026-11-03-996-dashboard-page/token-contrast.md)。截圖（PNG）不進 repo（repo 不放圖檔），也不附在 PR（2026-11-03 負責人決定）：102 張是 26 種狀態 × 淺色與深色 × 桌機與手機（帳號選單只有桌機），只用在下面的檢查，每張的結果在 `states-results.json`。

### 前端的檢查

| 指令 | 結果 |
| --- | --- |
| `api:check`、`typecheck`、`build`、`check:dist` | 通過 |
| `lint`（Biome） | 通過 |
| `test` | 843 個通過；2 個失敗，是 `dashboardFormat` 與 `table` 裡兩個依賴 ICU 版本的測試，與這次的變更無關，main 在開發者工作站上也一樣（CI 通過） |

新增或改寫的測試：`src/app/AppHeader.test.tsx`（12 個：外框、選單、目前頁面、回到對話、Escape 與焦點、帳號、手機形式、選單清單的規則）、`src/pages/dashboards/DashboardPage.test.tsx`（31 個：列表的分頁、搜尋、封存、檢視者；頁面的骨架、更新資料、單一資料集失敗、429、來源面板、`custom_page` 佔位；版本紀錄、回到舊版、封存、檢視者）、`src/styles/design-tokens.test.ts`（`warning` 與 `warning-soft` 對到設計系統的 `status-warning` 與 `status-warning-soft`，兩個主題）。

### 端對端：更新資料（`e2e-result.json`）

| 步驟 | 畫面 | ClickHouse（直接查） |
| --- | --- | --- |
| 打開儀表板 | 尖峰功率 64.954、讀數 48；表格兩個電表的筆數、尖峰、合計 | 64.954、48；兩個電表同 |
| 在 ClickHouse 加入兩筆新讀數（97.25 kW 與 12.5 kW） | | |
| 按「更新資料」，請求延遲 2.5 秒 | 出現「Updating…」；尖峰仍是 64.954；內容區 `opacity-60`、`aria-busy=true`；沒有骨架；按鈕停用 | |
| 完成後 | 尖峰功率 97.25、讀數 50；表格的筆數、尖峰與合計換成新的 | 97.25、50 |

打開時 2 次、更新後共 4 次 `POST …/dataset-runs`（每個資料集各一次，沒有沿用先前的結果）。

### CSP 與主控台（`flow-result.json`、`states-results.json`）

在強制的 CSP 下，淺色與深色、桌機與手機各一遍的互動流程（打開列表、換分頁、搜尋、點進儀表板、打開版本紀錄、看第 2 版的深連結、打開與關閉回到舊版的視窗、打開來源面板、更新資料、打開選單、組織選單與帳號選單、含 `custom_page` 的儀表板）：`securitypolicyviolation` 0 件，主控台的錯誤與警告 0 則，頁面例外 0 個，`/sandbox` 的請求 0 個。102 張截圖的每個狀態也都是 0 件違規、0 則主控台輸出。

### 外框與版面（`flow-result.json`）

- Escape 關閉選單後焦點回到「選單」按鈕。
- 手機（390 px）頁首高 52 px（加 1 px 下緣線），所有按鈕在一行（上緣 8 與 10 px），左右各 8 px；列表頁與儀表板頁都沒有水平溢位（最後一輪的 102 個狀態都是 0 px）。
- 列表的表格在窄螢幕裡橫向捲動，表格外框要 `relative`：視覺隱藏的 caption 與表頭儲存格是絕對定位，沒有定位的外框會讓它們落在捲出去的表格右邊，整頁因此多出水平捲動（第一輪截圖抓到，已修）。

### 對比（WCAG 2.2 AA：文字 4.5:1，圖形 3:1）

新增的 token（`token-contrast.md`，由 token 的值計算）與瀏覽器裡實際算出的顏色（`flow-result.json` 的 `contrast`，半透明的背景逐層疊上去）：

| 組合 | 淺色 | 深色 | 需要 |
| --- | --- | --- | --- |
| `--warning` 對 `--warning-soft`（看舊版的標籤） | 4.55 | 7.76 | 4.5 |
| `--warning` 對 `--card`、`--background` | 4.98 | 10.14／10.82 | 4.5 |
| 頁首的文字（選單、組織、回到對話） | 17.76 | 17.08 | 4.5 |
| 選單：目前項目的文字／圖示／色條 | 15.54／6.07／6.07 | 12.64／9.94／9.94 | 4.5／3／3 |
| 選單：其他項目的文字／圖示 | 17.76／6.02 | 15.22／6.90 | 4.5／3 |
| 麵包屑、資料時間（`--muted-foreground`） | 6.02 | 7.74 | 4.5 |
| 回到舊版的提示文字 | 17.76 | 15.99 | 4.5 |

頁首與選單的值和 `app-shell.md` 的表格一致。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 頁首照 `app-shell.md`（沒有導覽列、選單只列有權限且已上線的頁面並標出目前頁面、次要頁面有回到對話、Escape、手機一行） | 通過：`AppHeader.test.tsx`；手機的一行與 Escape 的焦點在上面的瀏覽器檢查 |
| 列表（分頁、搜尋、深連結重新載入仍是同一個儀表板與版本） | 通過：`DashboardPage.test.tsx` |
| 打開與「更新資料」都以 `dataset-runs` 重新查詢；ClickHouse 加入新資料後更新；更新期間保留舊內容與「正在更新…」 | 通過：端對端（上表） |
| 來源面板的 SQL 或工具參數與查詢時間和 API 回傳的一致 | 通過：`DashboardPage.test.tsx`（數字與圖表資料點的來源） |
| 回到舊版後顯示舊版的元件與資料集 | 通過：`DashboardPage.test.tsx` |
| viewer 看不到回到舊版與封存，editor 看得到 | 通過：`DashboardPage.test.tsx` |
| 一個資料集失敗只影響用到它的元件，顯示原因與重試；429 稍後再試 | 通過：`DashboardPage.test.tsx` |
| `custom_page` 顯示佔位，沒有 iframe 與 `/sandbox/` 的請求 | 通過：`DashboardPage.test.tsx`；瀏覽器流程 0 個 `/sandbox` 的請求 |
| 版本紀錄的畫面與和設計稿不同的地方寫進 `design.md`；PR 附每個狀態的截圖 | 文件通過；截圖不附（2026-11-03 負責人決定） |
| 新增的 token 與設計系統一致、對比檢查重跑 | 通過：`design-tokens.test.ts`、上表 |
| 在正式的 CSP 下打開儀表板、切換版本、打開來源面板與選單都沒有 CSP 違規 | 通過：上面的瀏覽器流程 |
| 前端的檢查 | 通過，除了與這次無關的 2 個 ICU 測試（上表） |

沒有涵蓋：Chromium 以外的瀏覽器、真的手機的觸控（用 390 px 的視窗代替）、資料過時的標示（這次沒有做，原因與選項在 PR 說明與 `chat-dashboards/design.md` 的「和設計稿不同的實作」）。
