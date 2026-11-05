# 儀表板頁接上自訂頁面的沙箱外層（2026-11-04）

對應 issue：Laysi/trustiot-v2#997（沙箱外層與顯示時比對）。Laysi/trustiot-v2#996 的儀表板頁先合併，`CustomPageSlot` 只是佔位；沙箱外層 `CustomPageFrame` 隨後合併，兩邊約定後合併的那個要把它們接起來，這次補上：版本的 `custom_page` 那一項在儀表板頁裡以沙箱 frame 顯示。程式的說明在 `frontend/README.md` 的「儀表板頁」。

## 環境

- 程式是 `feat/997-custom-page-wiring` 分支（建立自 main 的 `0d3130dd`，含這次尚未提交的變更），工具的版本是 `.prototools` 的；瀏覽器在容器裡執行（`scripts/run.sh`）。2026-11-04 在同一個分支加上「沒有動作」（沙箱不播放 CSS 轉場與動畫，[2026-11-04-997-custom-page-motion](2026-11-04-997-custom-page-motion.md)）之後，重跑了下面每一項，結果與 `output/` 是重跑的。之後只收窄了 `frontend/sandbox/src/dom.ts` 對已結束動畫的排除（同一份報告），這裡的頁面沒有動畫，所以沒有再重跑。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像裡的 Chromium（以 `--site-per-process` 啟動，與桌面版 Chrome 的預設相同），視窗 1280 × 900，語言 en-US，時區 UTC。
- 被測的是真的：`frontend/dist/`（SPA 的正式建置，`bun run --cwd frontend build` 與 `check:dist`），由真的 `services/web`（`cargo build -p web` 的 debug 版）提供，所以 SPA 文件的 CSP（含 `frame-src`）與 `/sandbox/v1/` 的標頭都是它真正送出的。腳本只回答 API 的 JSON（身分服務的工作階段與偏好、儀表板的內容與資料集的結果），其他請求一律走 `services/web`。API 的內容由 `scripts/gen-fixtures.ts` 以元件測試的記憶體伺服器（`src/test/fake-dashboards.ts`）產生，所以 body 與 SPA 的 schema 一致。這次不驗後端；資料集的真實執行見 Laysi/trustiot-v2#996 的報告。

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

# 瀏覽器檢查：產生 API 的內容，啟動 services/web 提供建置結果，再在容器裡跑腳本
bun docs/test-reports/data/2026-11-04-997-custom-page-wiring/scripts/gen-fixtures.ts <工作目錄>/fixtures.json
cargo build -p web
cp -R frontend/dist <工作目錄>/site-dist
TRUSTIOT__ROOT_DIR=<工作目錄>/site-dist TRUSTIOT__HTTP__LISTEN_ADDR=127.0.0.1:29197 \
  TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR=127.0.0.1:29198 TRUSTIOT__SHUTDOWN__DRAIN_DELAY=100ms ./target/debug/web &
# WORK 放 scripts/e2e.mjs、一個 {"private":true,"type":"module"} 的 package.json 與 playwright-core 的 node_modules；
# PROXY 放 scripts/proxy.mjs；IMAGE 是 frontend/e2e/image.ts 的映像
WORK=<…> OUT=<空目錄> FIXTURES=<工作目錄>/fixtures.json PROXY=<…> IMAGE=<…> PORT=29197 sh docs/test-reports/data/2026-11-04-997-custom-page-wiring/scripts/run.sh
```

## 結果

### 前端的檢查

| 指令 | 結果 |
| --- | --- |
| `api:check`、`typecheck`、`lint`（Biome）、`build`、`check:dist` | 通過 |
| `test` | 1045 個通過（重跑；多出的 11 個是「沒有動作」的單元測試）；2 個失敗：`dashboardFormat > a time shows the precision it has, milliseconds included` 與 `table > every cell is the row's value formatted by its column's type`，是依賴 ICU 版本的測試，與這次的變更無關，main 在開發者工作站上也一樣（CI 通過） |

`src/pages/dashboards/DashboardPage.test.tsx`（32 個全過）：「`custom_page` 是佔位」的測試改成「這一項在原位置顯示沙箱 frame」（iframe 的 `sandbox` 有 `allow-scripts` 而沒有 `allow-same-origin`，`src` 是 `/sandbox/v1/sandbox.html`，標題與「自訂圖表」標示在 frame 的卡片裡，與 KPI 的順序不變，整列寬）；新增「版本沒有 `page` 時顯示無法顯示的卡片、不載入 frame」；「在元件清單出現前寫入的版本」那個測試改成它的頁面也顯示在 frame 裡。

### 瀏覽器：真的建置、真的 `services/web`（`output/result.json`、`output/run.log`）

乾淨的頁面（儀表板「Plant power」：一個 KPI 加一個 `custom_page`，頁面把第一筆 `power.kw` 與 `power.line` 寫進文件）：

| 檢查 | 結果 |
| --- | --- |
| `custom_page` 那一項是 iframe，`sandbox="allow-scripts"`（沒有 `allow-same-origin`），`src="/sandbox/v1/sandbox.html"` | 通過 |
| frame 一開始藏起（`opacity: 0`、`inert`、`aria-hidden=true`），runtime 回報乾淨的比對後顯示（`opacity: 1`、不 `inert`） | 通過 |
| frame 裡的頁面畫出「Power 12.35 kW on Line A」（`,.2f` 格式的第一筆值） | 通過 |
| 卡片是 frame 自己的：標題「Plant power」、「Custom chart」標示、通過檢查的時間；整頁只有一張這種卡片；順序是 `kpi`、`custom_page`；「這一項無法顯示」不見了 | 通過 |
| 沙箱文件回 200，帶 `services/web` 的沙箱標頭（`sandbox allow-scripts; default-src 'none'; …`），沒有 `X-Frame-Options` | 通過 |
| SPA 的 CSP 含 `frame-src localhost:29197/sandbox/v1/sandbox.html` | 通過 |
| 按「Refresh data」：資料集重新執行，frame 再次顯示，值仍是 12.35 | 通過 |
| `securitypolicyviolation` 0 件；主控台錯誤與頁面例外 0 則 | 通過 |

頁面寫了資料沒有的值（儀表板「Plant power (tampered)」：同一個頁面，載入約 2 秒後把第一個值改成 `999.99`）：

- 一開始 frame 藏起，runtime 乾淨的回報後顯示（值 12.35）；約 2.1 秒頁面改值，下一個取樣（相隔 50 ms 左右）frame 已藏起；之後頁面的值被 runtime 換成「—」，frame 一直藏著，卡片顯示「1 values on this page don't match the data, so it isn't shown.」。
- 20 秒內 255 次取樣（每次讀 frame 是否顯示與 frame 裡的值），沒有任何一次是「frame 顯示且值為 999.99」。取樣只有 50 ms 的解析度，這一項只證明接線沒有改變 `CustomPageFrame` 的行為；顯示時比對的時間上限由 `frontend/e2e/custom-page.e2e.ts` 以每個 animation frame 的紀錄量測（`docs/test-reports/2026-11-03-997-custom-page-sandbox.md`）。

截圖（乾淨頁面與改值頁面）只留在本機，沒有放進 repo（原始資料只收文字）。

## 結論

- `custom_page` 那一項在儀表板頁以沙箱 frame 顯示（真的 SPA 建置與真的 `services/web` 的 CSP 與沙箱標頭）：通過。
- frame 只在乾淨的比對後顯示，頁面寫了資料沒有的值時藏起並說明原因：通過（接線層級；時間上限見 `2026-11-03-997-custom-page-sandbox.md`）。
- 前端的檢查：通過，兩個依賴 ICU 的既有失敗除外（與 main 相同）。
- 未測：真實後端（assistant 執行資料集）與這個頁面的組合；資料集的執行在 Laysi/trustiot-v2#996 的報告驗過，這次的 API 回應是測試用的記憶體伺服器產生的。
