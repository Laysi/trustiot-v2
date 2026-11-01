# 儀表板元件的驗證（2026-11-01）

對應 issue：Laysi/trustiot-v2#895（儀表板的元件模式：平台的互動元件與預設樣式）。元件的用法與規則在 `frontend/README.md` 的「儀表板元件」與「儀表板元件的圖表套件」。

## 環境

- 程式碼：本次的變更是 `frontend/src/dashboard/`、`frontend/src/test/fake-chart.ts`、`frontend/src/styles/global.css` 的系列色、zh-TW 與 en 的訊息、`frontend/package.json` 與 `frontend/bun.lock` 加入 `echarts` 6.1.0。
  - 瀏覽器實測與截圖：基於 main 的 `0627c59a`。之後提交前又改了兩處：來源面板最多逐列列出 6 列（原本 5 列）、規格的標題與單位長度以字元計算；兩者都不影響瀏覽器檢查的項目（預覽頁每張圖最多兩個系列，標題都很短），由元件的測試涵蓋。
  - 前端檢查：rebase 到 main 的 `0c293740` 之後的最終版本。
- 前端檢查與瀏覽器實測都在建置主機（arm64 Linux VM）：Bun 1.4.2（`.prototools` 的版本）、rootless podman。
- 瀏覽器：Debian trixie 的 `chromium-headless-shell` 156.0.8078.47-1~deb13u1（snapshot.debian.org 的 `20261022T170820Z` 快照），字型 `fonts-noto-core` 與 `fonts-noto-cjk`，在容器裡以 SwiftShader 繪圖。兩個執行個體：
  - 桌面：`--blink-settings=primaryPointerType=4,availablePointerTypes=4,primaryHoverType=2,availableHoverTypes=2`，頁面看到 `pointer: fine` 與 `hover: hover`；視窗 1280 px 寬。
  - 觸控：沒有上面的設定，頁面看到 `pointer: coarse`、`hover: none`；視窗 390 px 寬，DevTools Protocol 的 `mobile`、2 倍像素與觸控模擬。
- 預覽頁（不進 repo）：以 SPA 的 `vite.config.ts` 建置，只放六張卡片，資料都是 `frontend/src/dashboard/fixtures.ts`：
  1. 時間序列「Power by line」：兩條線（Line A、Line B），一條門檻「Limit」，有缺值、負值、小於 1 的值。
  2. 長條「Energy by day」：兩個系列（Site 1、Site 2），Wed 缺 Site 2。
  3. KPI「Energy this week」：具名計算 `sum(daily.kwh)` 的值、比較值（具名計算）與迷你走勢（中間缺一天，最後一天單獨一點）。
  4. 狀態「Meter 1」：資料集的一格，兩條門檻（Warning、Limit）。
  5. 錯誤「Power by line (type changed)」：同一個時間序列，但 `kw` 欄位的型別改成文字。
  6. 表格「Meters」：數字、文字、是否、日期與帶毫秒的時間，含缺值。
- 伺服器：測試程式以 Bun 的 HTTP 伺服器在 127.0.0.1 提供建置結果。每個回應帶 `services/web/src/headers.rs` 的 `CSP`（強制）與 `CSP_REPORT_ONLY`（Trusted Types，只回報），字串逐字相同：

  ```text
  Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'
  Content-Security-Policy-Report-Only: require-trusted-types-for 'script'; trusted-types 'none'
  ```

  元件還沒有接到任何路由（儀表板頁是 Laysi/trustiot-v2#462），所以頁面不是經 `services/web` 提供。

## 指令

```sh
# 前端的檢查，在建置主機上，repo 根目錄
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist

# 只跑儀表板元件的測試
bun run --cwd frontend test src/dashboard
```

瀏覽器實測的程式是一次性的，不進 repo；把它自動化屬於進階驗證，另開 issue（Playwright，沿用 Laysi/trustiot-v2#945 的做法）。重做的步驟：

1. 建置預覽頁：一個 `index.html` 與 `main.tsx`，以 `DashboardEnvProvider`（en、UTC）畫出上面六張卡片，`?theme=` 設定 `<html data-theme>`；一個 `@import "../src/styles/global.css"` 加上 `@source "../src"` 的樣式表；`vite.config.ts` 沿用 `frontend/vite.config.ts`，只改 `root` 與 `outDir`。放在 `frontend/` 底下的暫時目錄，以 `bun --bun vite build --config <目錄>/vite.config.ts` 建置。
2. 瀏覽器映像與兩個執行個體：

   ```sh
   # Containerfile：FROM docker.io/library/debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
   # 以 snapshot.debian.org/archive/debian/20261022T170820Z（與 debian-security 同一時間）為來源，
   # apt-get install chromium-headless-shell=156.0.8078.47-1~deb13u1 fonts-noto-core fonts-noto-cjk
   common="--no-sandbox --headless --remote-debugging-address=127.0.0.1 --use-angle=swiftshader --enable-unsafe-swiftshader --hide-scrollbars --font-render-hinting=none"
   podman run -d --name desktop --network host <映像> chromium-headless-shell $common --remote-debugging-port=9333 \
     --blink-settings=primaryPointerType=4,availablePointerTypes=4,primaryHoverType=2,availableHoverTypes=2 about:blank
   podman run -d --name touch --network host <映像> chromium-headless-shell $common --remote-debugging-port=9334 about:blank
   # 結束後：podman rm --force --volumes desktop touch
   ```

3. 測試程式經 DevTools Protocol 操作：`Emulation.setEmulatedMedia` 切換 `prefers-color-scheme` 與 `prefers-reduced-motion`；真的滑鼠事件（`Input.dispatchMouseEvent`：移動、滾輪、按下拖曳放開、點擊）、鍵盤事件與觸控事件（`Input.dispatchTouchEvent`：點按、上下滑、雙指、左右拖曳）。頁面載入前（`Page.addScriptToEvaluateOnNewDocument`）註冊 `securitypolicyviolation` 的收集器（強制與只回報都收），並記錄主控台、`Log` 與例外。期望值以 `dashboardFormat("en", "UTC")` 從同一份 fixtures 算出，畫面上的字要落在這個集合裡。
4. 動畫：每個畫面把時間序列的 canvas 縮成 96×48 取雜湊，計算載入後 1.5 秒內、以及切換圖例後 0.9 秒內出現幾種不同的畫面。
5. 對比：以頁面算出的實際顏色（`getComputedStyle`，半透明的背景一層層疊上去），依 WCAG 2.2 的公式計算。

系列色另以資料視覺化的色盤檢查（OKLab 色差，Machado 2009 的色盲模擬，嚴重度 1.0）計算：每色的亮度與彩度範圍、順序相鄰兩色在紅色盲與綠色盲模擬下的色差（目標 ≥ 8，下限 6）、一般視覺下的色差（≥ 15）、對底色的對比（≥ 3:1）。

## 結果

原始資料：[browser-desktop.json](data/2026-11-01-895-dashboard-components/browser-desktop.json)（桌面的 87 項檢查、對比、畫面數與違規清單）、[browser-touch.json](data/2026-11-01-895-dashboard-components/browser-touch.json)（觸控的 13 項）、[palette.txt](data/2026-11-01-895-dashboard-components/palette.txt)（系列色的檢查輸出）、[frontend-checks.txt](data/2026-11-01-895-dashboard-components/frontend-checks.txt)（前端檢查的輸出摘要）。

### 元件的測試

`frontend/src/dashboard/DashboardComponent.test.tsx` 35 個、`format.test.ts` 3 個，都通過。ECharts 換成 `frontend/src/test/fake-chart.ts` 的假引擎（happy-dom 沒有 canvas），測試讀取元件交給 ECharts 的 option 與 action，並送出 ECharts 會送的事件；期望值以同一個 `dashboardFormat` 從 fixtures 算出：

| 元件 | 畫面上的數字等於資料格式化的結果 | 型別不符時顯示錯誤、不畫 |
| --- | --- | --- |
| 時間序列 | 交給 ECharts 的點等於列的值（依時間排序、依系列分線）；tooltip 的時間與每個值；門檻標籤；鍵盤的 `aria-valuetext`；點擊後來源的每一格 | y 變成文字、x 不是時間、同一系列同一時間兩列（不加總）：錯誤訊息與來源按鈕，沒有 canvas |
| 長條 | 每個類別的值；tooltip 的每個值；點擊後的列（六個系列都有長條時，六列都列在來源裡） | y 不是數字；系列超過 6 個（不重複用顏色） |
| KPI | 具名計算的值與比較值；迷你走勢的點（含單獨一點） | 一格的值不是數字；計算讀的欄位型別變了 |
| 表格 | 每一格依欄位型別格式化（數字、文字、是否、日期、時間）；點擊後的列 | 結果少了一欄 |
| 狀態 | 值、所在區間的文字與每條門檻 | 值的欄位不是數字；門檻不是由低到高 |

另外：讀取中與讀取失敗、結果沒有列、zh-TW 的數字與訊息、規格拒絕不認得的種類與欄位（含顏色與樣式）、標題與單位的長度照 contract 以字元計算（不是 UTF-16 單位）、根元素只能以空字串清空、卸載時 `dispose()`、`prefers-reduced-motion` 時 `animation: false`、沒有精確指標時單指平移且不取消 touchmove。`format.test.ts` 以寫死的字串檢查數字的位數、毫秒與座標軸的標籤。

### 前端檢查

最後一次修改程式碼與 `frontend/README.md`（Tailwind 會掃描它）並 rebase 到 main 的 `0c293740` 之後重跑，全部通過，輸出摘要在 [frontend-checks.txt](data/2026-11-01-895-dashboard-components/frontend-checks.txt)：

| 指令 | 結果 |
| --- | --- |
| `api:check` | 通過 |
| `typecheck` | 通過 |
| `lint` | 通過（159 個檔案） |
| `test` | 37 個檔案、721 個測試全部通過 |
| `build` | 通過；超過 500 kB 的警告是 SPA 原本的主 chunk，還沒有頁面 import 元件，所以 ECharts 不在這次的建置裡 |
| `check:dist` | 通過 |

ECharts 是 lazy 載入的獨立 chunk：預覽建置裡是 575,482 bytes，gzip 後 192,095 bytes，只有畫圖的頁面才下載。

### 瀏覽器：桌面（1280 px，精確指標）

87 項全部通過。

| 元件 | 檢查 |
| --- | --- |
| 時間序列 | 滑鼠移到圖中央：tooltip 的值（`0.000123 kW`、`—`）與時間（`Nov 1, 2026, 12:15 AM`）都是資料格式化的結果，tooltip 在圖的範圍內，canvas 畫出十字線；滾輪縮放後出現「Show the full range」，按下後回到全範圍；拖曳框選時顯示選取框、放開後縮放，這次拖曳不算點擊；點圖例「Line B」：`aria-pressed="false"`、canvas 改變；點中央的點打開來源，每一格都是資料格式化的結果，SQL 是資料集的 `SELECT ts, line, kw FROM power ORDER BY ts`；聚焦後按兩次左鍵，tooltip 與 `aria-valuetext` 跟著移動，按 Enter 打開來源 |
| 長條 | tooltip 的值（`133.333 kWh`、`101 kWh`）；hover 畫出陰影；圖例切換 Site 2；點長條打開那個類別的列（Row 3、Row 4） |
| KPI | 迷你走勢的 tooltip 值；大字的值等於 `sum(daily.kwh)` 的結果（`453.583`）；點值打開具名計算的來源（算式 `sum(daily.kwh)` 與它讀的資料集） |
| 表格 | 15 個格子都是資料格式化的結果；點格子打開那一列 |
| 狀態 | 區間的文字「Below Warning 100 kW」；點值打開那一列 |
| 錯誤 | 型別不符的卡片顯示「Column kw of dataset “Line power” is now text; this component needs a number.」，沒有 canvas |
| 整頁 | 文件裡沒有 `<style>`；按下卸載後沒有任何 canvas |

### 瀏覽器：觸控（390 px）

13 項全部通過：

| 檢查 | 結果 |
| --- | --- |
| 提示文字 | 「Pinch to zoom, drag to pan, tap a point to see its source.」 |
| 手機寬度 | 頁面沒有橫向捲動；表格在卡片裡左右捲動，沒有格子換行 |
| 上下滑過時間序列與長條 | 交給頁面捲動：`touch-action: pan-y`，5 次 touchmove 沒有一次被取消（修正前時間序列 7 次全被取消，見下方「實測中找到並修正的問題」） |
| 點按 | 顯示 tooltip（值是資料格式化的結果），tooltip 在圖的範圍內，同時打開那一點的列 |
| 雙指 | 縮放（出現回到全範圍的按鈕）；放大後上下滑仍交給頁面 |
| 單指左右拖曳 | 平移放大後的圖：範圍從 33.3–100% 移動、寬度不變，頁面不動 |
| CSP 與 Trusted Types | 淺色與深色都沒有違規 |

### 動畫

| 情況 | 不同的畫面數 | 判斷 |
| --- | --- | --- |
| 載入，允許動畫 | 3 | 有進場轉場 |
| 載入，`prefers-reduced-motion: reduce` | 1 | 直接畫出結果 |
| 切換圖例，允許動畫 | 4 | 有更新轉場 |
| 切換圖例，`prefers-reduced-motion: reduce` | 2（切換前與切換後） | 沒有中間畫面 |

### CSP 與 Trusted Types

每一輪的 `securitypolicyviolation`（強制與只回報）、主控台訊息與例外都是空的：

| 輪次 | 內容 | 違規 |
| --- | --- | --- |
| 淺色、桌面 | 載入、上面所有互動、卸載 | 0 |
| 深色、桌面 | 載入、hover、打開來源 | 0 |
| 淺色、減少動態 | 載入、切換圖例 | 0 |
| 淺色與深色、觸控 | 載入、上面所有觸控操作 | 0 |

ECharts 以 canvas 繪圖，tooltip 是我們的 React 元件；zrender 清空根元素時經過我們的 `innerHTML` setter（`replaceChildren()`），所以沒有字串寫進 Trusted Types 的 sink。

### 對比（WCAG 2.2 AA）

以頁面實際的顏色計算，文字的門檻 4.5:1，圖形（系列色、區間標示）3:1，兩個主題各 27 組全部通過，完整清單在 `browser-desktop.json` 的 `contrastLight` 與 `contrastDark`：

| 配對 | 淺色 | 深色 | 門檻 |
| --- | --- | --- | --- |
| 標題、KPI 與表格的值、tooltip 的時間與值 | 17.76 | 15.99（tooltip 15.22） | 4.5 |
| 次要文字（單位、提示、表頭、門檻、tooltip 的系列名稱、座標軸標籤） | 6.02 | 7.25（tooltip 6.90） | 4.5 |
| 來源面板的文字／欄名 | 17.14／5.81 | 15.09／6.84 | 4.5 |
| 錯誤的標題與訊息 | 5.41 | 5.69 | 4.5 |
| 系列色 1–6 對卡片 | 4.66、4.42、4.04、5.77、4.82、4.92 | 5.58、5.72、5.15、5.74、5.20、5.85 | 3 |
| 狀態的區間標示（主色）對卡片 | 6.94 | 12.57 | 3 |

### 系列色

`global.css` 的 `--chart-1`～`--chart-6`，第一色是品牌色相；淺色 `#0b8466, #2a78d6, #d4572a, #6250d6, #c2427a, #8a6d00`，深色 `#21a27f, #4f93ea, #d9693a, #9085e9, #d9628f, #b38f12`。

| 檢查（順序相鄰的兩色） | 淺色（卡片 `#ffffff`） | 深色（卡片 `#14171e`） |
| --- | --- | --- |
| 亮度與彩度範圍 | 通過 | 通過 |
| 紅色盲、綠色盲模擬下最接近的一對 | 11.0（色 5 與 6，綠色盲） | 12.4（色 5 與 6，綠色盲） |
| 一般視覺下最接近的一對 | 19.1（色 1 與 2） | 17.7（色 4 與 5） |
| 對底色 3:1 | 通過 | 通過（頁面底色 `#0d0f15` 也通過） |

不相鄰的配對比較接近：藍（色 2）與紫（色 4）在一般視覺下只差 10.5（淺色）與 8.0（深色），色盲模擬下更近，所以「任意兩色」的檢查不通過。折線與長條以相鄰配對為準；隱藏中間的系列後兩者可能相鄰，所以圖例、tooltip 與來源一律寫出系列名稱，不只靠顏色。

### 截圖

截圖檔不進 repo（報告的原始資料只放文字），以下逐張描述：

| 截圖 | 內容 |
| --- | --- |
| 淺色、桌面 | 六張卡片。時間序列的 tooltip 在 00:15（Line A `0.000123 kW`、Line B `—`），十字線的直線與橫線，門檻「Limit 1,245.5 kW」的虛線與標籤；x 軸從「11/1」開始每 5 分鐘一個刻度，y 軸 −300 到 1.5K。長條三個類別，Wed 只有很矮的 Site 1。KPI 大字 `453.583 kWh`、比較值「Last week 196.5 kWh」，迷你走勢前三天連成線、缺一天、最後一天是單獨的點。狀態 `42.123 kW`、「Below Warning 100 kW」，三段裡第一段以主色標出。錯誤卡片是紅框的說明與「See the source: Line power」按鈕。表格三列：缺值是 `—`、是否是 Yes／No、時間 `Nov 1, 2026, 1:06:07.250 AM` 帶毫秒。長條、KPI、狀態與表格的來源面板打開：SQL 在灰底的區塊、查詢時間、點到的列逐欄列出 |
| 淺色、桌面、時間序列的來源 | 點 00:15 的點之後：Row 3 與 Row 4 的 `ts`、`line`、`kw`，SQL 與查詢時間 |
| 深色、桌面 | 與淺色同樣的內容，卡片、tooltip 與來源面板都是深色；系列色是深色那一組，比淺色亮 |
| 淺色、手機寬度（雙指放大後） | 每張卡片佔滿寬度；時間序列出現「Show the full range」，提示改成觸控的說法，tooltip 在 00:30（`-7.25 kW`、`1,240.125 kW`）；長條的 tooltip「Tue」；表格右側被卡片切掉、可以左右捲動 |
| 淺色、手機寬度、點按之後 | 點 00:15：tooltip 在圖裡，下面打開那一點的兩列 |
| 深色、手機寬度 | 深色的同一頁，沒有橫向捲動 |

### 實測中找到並修正的問題

瀏覽器實測找到下面的問題，都已修正，修正後重跑全部檢查：

1. 框選縮放之後的下一次真正的點擊被吞掉：拖曳後 zrender 不一定送出 click，用來略過那次 click 的標記一直留著。改成每次按下時清除。
2. 有門檻時數值軸的上端是不整齊的數字（例如 133.33）：原本以函式設定軸的兩端。改成在門檻的值放看不見的點，讓軸自己取整齊的刻度。
3. 迷你走勢最後一個單獨的值看不到（前後都沒有值就沒有線）。改成另外畫成小點。
4. 手機上表格的日期與時間換成好幾行。改成數字、時間與短文字不換行，表格左右捲動。
5. 寬的卡片上，來源面板的欄名與值分得太開。限制來源列的寬度。
6. 放大到兩點之間時，十字線的軸標籤顯示一個沒有任何一列的時間。拿掉軸標籤，時間由 tooltip 寫出。
7. 放大後座標軸出現重複的標籤（例如兩個 00:20）。標籤改成依刻度取最細的單位（秒、毫秒）。
8. 觸控時時間序列取消圖上的每個 touchmove（7 次都取消），手機上手指放在圖上整頁捲不動。改成 `preventDefaultMouseMove: false`，上下滑交給頁面（0 次取消）。
9. 手機上 tooltip 超出圖的範圍。改成放在指標兩側放得下的一邊，都放不下時貼齊圖的邊緣。
10. 長條 hover 的陰影蓋在長條上，顏色變淡。陰影改在長條下面。
11. 有負值時 x 軸線停在 0 的高度，與底部的標籤分開。軸線固定在底部。
12. 時間的毫秒被截掉。有毫秒時照實顯示。
13. 可以一直放大到兩點之間的空白。最小的縮放範圍改成相鄰兩點最小間隔的兩倍。

另外，提交前的程式碼審查找到：來源面板最多逐列列出 5 列，但一個點或一組長條最多有 6 個系列的列，第 6 列看不到。改成最多列出 `MAX_SERIES` 列，並加上六個系列的測試。

### 觀察到、沒有處理的

- 手機寬度時，x 軸的第一個標籤（日期「11/1」）被 ECharts 的重疊規則隱藏，日期只在 tooltip 與來源裡看得到。
- 桌面上滑鼠在時間序列上時，滾輪縮放圖而不捲動頁面（設計文件要求滾輪縮放）；圖很多的儀表板頁可能會卡住頁面的捲動，接上儀表板頁（Laysi/trustiot-v2#462）時再看是否要改成按住修飾鍵才縮放。
- 觸控時點按一個點同時顯示 tooltip 與打開來源。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 五種元件都有測試：畫面上的每個數字（含 tooltip 與表格的格子）都等於資料值格式化的結果；欄位型別不符時顯示錯誤、不畫 | 通過：「元件的測試」一節，38 個測試，五種元件都有兩類檢查 |
| 瀏覽器實測（以正式的 CSP header）：每種元件的 tooltip、十字線、圖例切換、縮放與框選、點數字看來源都可用，主控台沒有 CSP 或 Trusted Types 的違規；`prefers-reduced-motion: reduce` 時沒有動畫 | 通過：桌面 87 項、觸控 13 項；標頭與 `services/web` 的字串逐字相同，頁面由預覽伺服器提供（元件還沒接到路由）。縮放與框選只有時間序列有（設計如此），長條的十字線是陰影，KPI 的互動在迷你走勢與值，表格與狀態是點值看來源 |
| 每種元件在亮色與暗色主題的截圖存進測試報告，文字與背景的對比符合 WCAG 2.2 AA | 通過：截圖以文字描述於「截圖」一節（圖檔不進 repo），對比兩個主題各 27 組全部達到門檻 |
| `bun run --cwd frontend typecheck`、`lint`、`test`、`build`、`check:dist` 全部通過 | 通過：`api:check` 也通過，`test` 是 721 個全部通過；見「前端檢查」與 frontend-checks.txt |

待測：

- 瀏覽器只測了 Chromium；Firefox 與 WebKit 待測（跨瀏覽器另見 Laysi/trustiot-v2#946 的做法）。
- 經真正的 `services/web` 與儀表板頁：元件接上頁面時（Laysi/trustiot-v2#462）再測。
- 螢幕閱讀器實際朗讀（NVDA、VoiceOver）待測；這次只確認 ARIA 屬性（`role="slider"`、`aria-valuetext`、`aria-pressed`、按鈕與表格的語意）。
- 這次的瀏覽器檢查自動化：延後，另開 issue。
