# 儀表板元件的圖表套件：CSP、大小與互動的實測（2026-11-01）

Laysi/trustiot-v2#927：儀表板改成「平台的互動元件 + AI 寫規格」（決策 33 的修訂，[AI 助理與 AI 閘道](../architecture/ai-assistant.md)「儀表板」）。元件畫在 SPA 自己的頁面裡，所以元件內部用的圖表套件要能在 SPA 的 CSP 下執行（[frontend/README.md](../../frontend/README.md)「CSP 的限制」）。2026-11-01 維護者決定三個候選都實測後再選：ECharts（SVG renderer、逐模組 import）、Observable Plot、以現有 d3 模組自己寫。這份報告是實測的方法與數字，結論寫在 [frontend/README.md](../../frontend/README.md)「儀表板元件的圖表套件」。

## 環境

- **程式碼**：`frontend/` 是 main 的 `b160413e`，實驗的改動只在拋棄式的副本裡，不進 repo（見「指令」）。
- **建置與瀏覽器**：建置主機。工具版本是 `.prototools` 釘選的版本（Bun 1.4.2），前端相依以 `frontend/bun.lock` 鎖定，候選套件以 `bun add` 加入精確版本。
- **瀏覽器**：Debian trixie 的 `chromium-headless-shell` 156.0.8078.47-1~deb13u1（與 `deploy/images/dashboard-check` 同一個 Debian snapshot 與版本），在容器裡執行，以 DevTools protocol 操作。回報的版本是 `HeadlessChrome/156.0.8078.47`。
- **候選版本**：`echarts` 6.1.0（Apache-2.0，相依 `zrender` 6.1.0、`tslib`）、`@observablehq/plot` 0.6.17（ISC，相依整包 `d3` 7）、`d3-zoom` 3.0.0、`d3-brush` 3.0.0、`d3-transition` 3.0.1（ISC；其餘 d3 模組沿用 `frontend/package.json` 已鎖定的版本）。
- **日期**：2026-11-01。

## 指令

每個候選各做一份 SPA 的建置，候選以外完全相同：

1. `bun install --cwd frontend --frozen-lockfile`，再以 `bun add --cwd frontend` 加入候選：`echarts@6.1.0`；`@observablehq/plot@0.6.17`；或 `d3-zoom@3.0.0 d3-brush@3.0.0 d3-transition@3.0.1`。
2. 加入一頁實驗頁 `src/chart-lab/`，以 lazy route `/__chart-lab` 掛在 `src/app/routes.tsx`（在版面之外，不需要登入）。頁面畫兩張圖，資料固定：
   - 時間序列：兩條線（Line A、Line B），一天 5 分鐘一點，共 288 點，每個值都有一位小數，所以不會與座標軸的刻度相同。
   - 長條：12 個類別。
   每個候選都實作 tooltip、十字線、圖例切換、滾輪縮放、框選縮放、點資料點回報來源列，以及進場動畫（`prefers-reduced-motion: reduce` 時關閉）。做法：
   - ECharts：`echarts/core` 加 `LineChart`、`BarChart`、`GridComponent`、`TooltipComponent`、`LegendComponent`、`DataZoomComponent`、`ToolboxComponent`、`SVGRenderer`，`echarts.init(el, undefined, { renderer: "svg" })`；tooltip 預設用 `renderMode: "richText"`（畫在 SVG 裡），另以 `?tooltip=html` 量 ECharts 預設的 HTML tooltip；框選用 toolbox 的 dataZoom select 模式；點資料點用 `getZr().on("click")` 加 `convertFromPixel`；`animation` 依 `prefers-reduced-motion` 設定。
   - Plot：`Plot.plot` 加 `lineY`、`crosshairX`、`tip` 與 `pointerX`；Plot 沒有圖例切換、縮放與框選，這幾項在實驗頁自己寫（圖例的 swatch 加 click、在 svg 上聽 wheel 與 pointerdown／pointerup，以 `fig.scale("x").invert` 換算）；Plot 沒有動畫。
   - d3：React 畫 SVG（同設備頁 `TimeSeriesChart.tsx` 的做法），d3 只給 scale、path、`d3-zoom`（滾輪縮放）、`d3-brush`（圖下方的範圍條框選）與 `d3-transition`（進場動畫）；tooltip 與十字線是 React 畫的 SVG，圖例是按鈕。
3. `bun run --cwd frontend build`，再 `bun run --cwd frontend check:dist`。
4. 量 `dist/static/` 每個 JS 與 CSS 檔的原始大小與 gzip 後的大小（zlib level 6 與 9）。
5. 以 Bun 提供每份建置的 `dist/`：送出的 `Content-Security-Policy` 與 `Content-Security-Policy-Report-Only` 從 `services/web/src/headers.rs` 讀出，與正式環境逐字相同；`/api`、`/auth` 回 404，其他路徑回 `index.html`。
6. 以 DevTools protocol 操作 headless Chromium（視窗 1280 × 1100），每個候選在 `prefers-reduced-motion: no-preference` 與 `reduce` 各跑一次，ECharts 另跑一次 `?tooltip=html`：
   - 在頁面的 script 執行前，先註冊 `securitypolicyviolation` 的收集器（`Page.addScriptToEvaluateOnNewDocument`），enforce 與 report（Trusted Types）的違規都收；同時收 console、Log 與例外。
   - 等實驗頁回報畫完，再以真的滑鼠事件（`Input.dispatchMouseEvent`）依序：指到圖的中央，確認 hover 的那一列 Line A 的值（以同一份資料計算）出現在圖裡、hover 前沒有；點圖例的「Line B」；在中央滾動滾輪；拖曳框選（ECharts 與 Plot 在繪圖區，d3 在範圍條）；點中央的資料點。每一步等實驗頁回報的狀態改變，最多 5 到 10 秒。
   - 最後讀 `<style>` 元素與帶 `style` 屬性的元素數量。

實驗的程式只在拋棄式的副本裡執行，副本與容器映像在結束時刪除，不進 repo；`/__chart-lab` 與實驗頁也沒有進 `frontend/`。

## 結果

原始資料：[sizes.json](data/2026-11-01-927-chart-library-csp/sizes.json)（每份建置每個檔案的大小）、[browser-runs.json](data/2026-11-01-927-chart-library-csp/browser-runs.json)（每次瀏覽器執行的步驟結果、違規與 console）。

### 建置與 `check:dist`

五份建置（不加套件的基準、ECharts、不含 toolbox 的 ECharts、Plot、d3）的 `bun run --cwd frontend build` 與 `bun run --cwd frontend check:dist` 都通過。`check:dist` 只看 HTML、SVG 與 CSS，所以下一節的瀏覽器實測才是 JavaScript 的 CSP 結果。

### 大小

基準（目前的 SPA）只有一個 JS 檔：718,499 bytes，gzip 219,711 bytes。加入候選後，基準的 JS 檔只多了 lazy route（原始大小多 723–829 bytes，gzip 多 135–165 bytes），候選與實驗頁都在 lazy 載入的另一個檔案：

| 候選 | lazy 檔案原始大小 | gzip（level 6） | 相對於目前整個 SPA 的 JS |
| --- | ---: | ---: | ---: |
| ECharts（含 toolbox，框選要它） | 614,800 | 208,874 | 95% |
| ECharts（不含 toolbox，沒有框選） | 579,917 | 196,838 | 90% |
| Plot | 275,608 | 93,643 | 43% |
| d3（`d3-zoom`、`d3-brush`、`d3-transition` 與既有模組） | 92,595 | 31,651 | 14% |

lazy 檔案也包含實驗頁自己的程式與資料。CSS 檔在各份建置之間只差 Tailwind 掃到的幾個 class（gzip 7,618–7,697 bytes）。

### 在 SPA 的 CSP 下

| 候選 | CSP 違規（enforce） | Trusted Types 違規（report） | `<style>` 元素 | 帶 `style` 屬性的元素 |
| --- | ---: | ---: | ---: | ---: |
| ECharts，richText tooltip | 0 | 0 | 0 | 38 |
| ECharts，HTML tooltip（`?tooltip=html`） | 0 | 22 | 0 | 39 |
| Plot | 5 | 0 | 3 | 1 |
| d3 | 0 | 0 | 0 | 3 |

- **ECharts**：richText tooltip 時沒有任何違規。ECharts 預設的 HTML tooltip 每次更新都以 `innerHTML` 寫入內容（例如 `10:50<br/>Line A: 46.9<br/>Line B: 24.9`），Trusted Types 目前只回報，改成強制後這一步會丟出例外，tooltip 就不會顯示。帶 `style` 屬性的 38 個元素是 ECharts 與 zrender 以 CSSOM 設定的定位與尺寸，CSP 允許（與 Radix 的 popper 相同，見「CSP 的限制」）。
- **Plot**：每張圖與每個圖例都插入 `<style>`（`src/plot.js` 的 `svg.append("style")`、`src/legends/swatches.js` 的 `div.insert("style", "*")`，沒有關掉的選項），CSP 擋下了 5 次（`style-src-elem`）。被擋下之後 Plot 預設的樣式全都沒有套用：tooltip 是黑底黑字、看不到值，長條是黑色，圖例沒有排成一列。
- **d3**：沒有任何違規。3 個 `style` 屬性是 `d3-zoom` 以 CSSOM 設定的 `touch-action` 與 `-webkit-tap-highlight-color`，CSP 允許。

三個候選都沒有出現 `eval` 或 `new Function` 的違規（`script-src` 沒有 `'unsafe-eval'`）。每次執行另外都有兩筆與候選無關的 console 記錄：實驗頁不需要的工作階段請求回 404（靜態伺服器沒有 API），以及 React Router 的 `HydrateFallback` 警告。

### 互動

每格是瀏覽器裡以真的滑鼠事件觀察到的結果，`no-preference` 與 `reduce` 兩次相同（動畫一列除外）：

| 項目 | ECharts | Plot | d3（自己寫） |
| --- | --- | --- | --- |
| tooltip：hover 那一列的值出現在圖裡、hover 前沒有 | 是（第 143 列，52.4） | 是（第 143 列，52.4），但被擋下的樣式讓它看不到 | 是（第 144 列，53.8） |
| 十字線 | 是（內建 `axisPointer`） | 是（內建 `crosshairX`） | 是（自己畫） |
| 點圖例切換系列 | 是（內建） | 沒有觀察到：自己加的 click 沒有收到事件 | 是（自己寫的按鈕） |
| 滾輪縮放 | 是（內建 dataZoom，4.5%–95.5%） | 是（自己寫，重畫） | 是（`d3-zoom`） |
| 框選縮放 | 是（toolbox 的 dataZoom select，31.8%–59.1%） | 沒有觀察到：Plot 的 pointer 在 pointerdown 呼叫 `stopImmediatePropagation()`（`src/interactions/pointer.js`），自己加的 pointerdown 收不到，要改在 capture 階段聽 | 是（`d3-brush`） |
| 點資料點回報來源列 | 是（第 130 列） | 沒有觀察到：同上 | 是（第 100 列） |
| 動畫，`reduce` 時關閉 | 是（`animation` 依設定為 true／false） | 沒有動畫 | 是（`d3-transition`，`reduce` 時不執行） |

Plot 已因 `<style>` 不符合條件，沒有再追沒觀察到的兩項。

## 結論

| 條件 | ECharts | Plot | d3（自己寫） |
| --- | --- | --- | --- |
| 在 SPA 的 CSP 下執行：不注入 `<style>`、不用 `eval`／`new Function`、不寫 `innerHTML` | 通過，限 richText tooltip（HTML tooltip 寫 `innerHTML`） | **不通過**：一定插入 `<style>`，被擋下後畫面不能用 | 通過 |
| 授權可用於 SaaS | Apache-2.0 | ISC | ISC |
| SVG 輸出 | SVG renderer | SVG | SVG |
| tree-shaking | 逐模組 import，仍約 197–209 KB（gzip） | 相依整包 d3，94 KB（gzip） | 32 KB（gzip） |
| 互動 | 全部內建 | tooltip 與十字線內建，其餘要自己寫 | 全部要自己寫，實驗頁證明做得到 |

- Plot 不符合 CSP 的條件，排除。
- ECharts 與 d3 都符合 issue 列的四個條件，差別是取捨：ECharts 的互動全部內建，但多載入約 200 KB（gzip，約是目前整個 SPA 的 JS），而且要守住 richText tooltip；d3 小很多、畫面與無障礙完全由我們控制，但互動要自己寫與測試。比較與建議寫在 [frontend/README.md](../../frontend/README.md)「儀表板元件的圖表套件」，選擇由維護者在 PR 審查決定。
- 驗收條件「`frontend/README.md` 寫明元件用的圖表套件，以及它在 SPA 的 CSP 下的實測結果（`bun run --cwd frontend build` 與 `check:dist` 通過）」：實測結果與建置、`check:dist` 通過已記錄；套件的最終選擇待維護者決定。

待測（進階驗證，依組織的驗證分級延後）：

- 以 Firefox 與 Safari 重做 CSP 與互動的實測（跨瀏覽器）。
- 選定的套件以 2,000 列（資料集的上限）量繪製與互動的時間（效能）。
- 把本報告的瀏覽器實測做成自動化測試：屬於實作元件的 issue。
