# 儀表板元件的圖表套件：功能、效能與外觀的實測（2026-11-01）

Laysi/trustiot-v2#927。前一份報告是 [CSP、大小與互動的實測](2026-11-01-927-chart-library-csp.md)。2026-11-01 維護者在 PR Laysi/trustiot-v2#932 的審查中改了評比方式：「不需要理會大小，找功能最強性能最強的，最好看的，以最佳體驗解找答案」。所以這一輪不把套件大小當條件，也不要求輸出 SVG，改用下面的條件與評比，重新實測更多候選。

**硬性條件**：

- 在 SPA 的正式 CSP 下沒有 enforced 違規，Trusted Types 也不違規（可以靠設定做到也算）。
- 授權可用於商業 SaaS。
- 能在 React 裡用。

元件模式不經 `dashboard-check`，所以 canvas 與 WebGL 都可以用，但「點數字看來源」與無障礙（鍵盤、螢幕閱讀器）要做得到。

**評比順序**：

1. 功能與體驗：tooltip、十字線、圖例切換、滾輪縮放、框選縮放、範圍導覽、動畫、大量資料時的流暢度、觸控、深淺主題。
2. 效能：10k 與 100k 點。
3. 外觀：能不能照設計系統的 token 做出好看的預設樣式。

結論寫在 [frontend/README.md](../../frontend/README.md)「儀表板元件的圖表套件」。

## 環境

- **程式碼**：`frontend/` 是 main 的 `b160413e`。實驗的改動只在拋棄式的副本裡，不進 repo（見「指令」）。
- **建置與瀏覽器**：建置主機（arm64 Linux VM，12 vCPU）。工具版本是 `.prototools` 釘選的版本（Bun 1.4.2）。前端相依以 `frontend/bun.lock` 鎖定，候選套件以 `bun add` 加入精確版本。
- **瀏覽器**：Debian trixie 的 `chromium-headless-shell` 156.0.8078.47-1~deb13u1，與 `deploy/images/dashboard-check` 同一個 Debian snapshot。在容器裡執行，沒有 GPU，以 `--use-angle=swiftshader --enable-unsafe-swiftshader` 讓 WebGL 由軟體執行，以 DevTools protocol 操作。
- **候選版本**：
  - `echarts` 6.1.0（Apache-2.0），canvas 與 SVG 兩種 renderer。
  - `uplot` 1.6.32（MIT，canvas）。
  - `plotly.js-strict-dist-min` 4.1.2（MIT）。這是 Plotly 為嚴格 CSP 出的 bundle，不用 function constructor。SVG trace（`scatter`）與 WebGL trace（`scattergl`）各量一次。4.2.0 發布未滿兩週，所以用 4.1.2。
  - `ag-charts-community` 14.2.0（MIT，canvas）。
  - `recharts` 3.10.1（MIT，SVG）與它要的 peer dependency `react-is` 19.3.1。
  - d3 自己寫（基準）：`d3-zoom` 3.0.0、`d3-brush` 3.0.0、`d3-transition` 3.0.1，其餘沿用 `frontend/package.json` 的版本。
- **沒有實測的候選**：
  - visx：和「d3 自己寫」是同一種做法，React 畫 SVG、互動自己寫，結果會與 d3 基準相同。
  - 商業授權的套件：見「商業授權的套件」。
- **日期**：2026-11-01。

## 指令

1. `bun install --cwd frontend --frozen-lockfile`，再以 `bun add --cwd frontend` 一次加入所有候選。
2. 加入實驗頁 `src/chart-lab/`。每個候選一條 lazy route `/__chart-lab/<候選>`，在版面之外，不需要登入。
   - 頁面畫同一張時間序列：兩條線（Line A、Line B），預設是一天 5 分鐘一點，共 288 點。每個值都有一位小數，所以不會和座標軸刻度相同。`?n=` 改點數，`?series=1` 只畫一條線，`?anim=0` 關動畫。
   - 主題：頁面等 app 依 `prefers-color-scheme` 套上 `data-theme` 之後，才從 `src/styles/global.css` 讀出 token（以 1×1 canvas 把 oklch 轉成 rgb），給每個候選同樣的顏色：
     - 背景、文字、`--muted-foreground`、`--border`、`--popover`。
     - 字型：`--font-sans`。
     - 暫定的圖表色：淺色是深一點的薄荷綠 `oklch(0.6 0.12 170)`，深色是品牌色 `#a0eed6`，第二條線是 `--primary`。設計系統還沒有圖表 token。
   - 每個候選都接上 tooltip、十字線、圖例切換、滾輪縮放、框選縮放、範圍導覽、點資料點回報來源列，以及進場動畫（`prefers-reduced-motion` 時關閉），能用內建的就用內建的。
   - 沒有內建鍵盤操作的候選，外面包一層我們的 `KeyNav`：整張圖一個 tab stop，方向鍵移動選取的列，再呼叫該套件顯示 tooltip 的 API，`aria-live` 讀出那一列。
   - 各候選的做法：
     - **ECharts**：`echarts/core` 逐模組 import（`LineChart`、Grid、Tooltip、Legend、DataZoom（inside 與 slider）、Toolbox、Aria、renderer）。
       - tooltip 設 `showContent: false`：ECharts 仍畫十字線並送出 `showTip`／`hideTip` 事件，tooltip 本身是我們的 React 元件，用設計系統的 class。
       - 框選用 toolbox 的 dataZoom select 模式。這個模式只在桌面開；觸控時會搶走單指拖曳，所以觸控不開，第二輪改成這樣。
       - 大量資料用 `sampling: "lttb"`。
       - 第二輪起，給圖表的根元素一個自己的 `innerHTML` 屬性：寫入空字串時改成 `replaceChildren()`，其他內容一律拒絕（原因見「結果」）。
     - **uPlot**：內建 cursor、十字線、會隨游標更新數值的圖例（點一下切換線）、x 方向的拖曳框選縮放。tooltip 是我們的 React 元件。沒有滾輪縮放、範圍導覽、動畫與觸控手勢，這些沒有自己補。
     - **Plotly**：
       - 用 strict bundle。Plotly 的程式在寫自己的 `<style>` 之前，會先找 id 為 `plotly.js-style-global` 的元素，它有 class `no-inline-styles` 時就不寫。所以實驗頁先放這樣一個元素，樣式改由 `plotly.js` 4.1.2 發布的 `dist/plotly.css` 提供，以 Vite 打包成樣式檔。
       - 內建 spike line（十字線）、`rangeslider`、`scrollZoom`、`dragmode: "zoom"`；modebar 關掉。
       - 鍵盤以 `Plotly.Fx.hover` 顯示 hover。
     - **AG Charts Community**：以 `AgCharts.create` 建立，註冊 `AllCommunityModule`。同時打開 zoom、navigator、crosshair 與 animation，看哪些要商業的 `ag-charts-enterprise`。鍵盤是它自己的。
     - **Recharts**：`LineChart`、tooltip（內容是我們的 React 元件）、游標線、`Legend`（切換線用 `hide`）、`Brush`（範圍導覽），`accessibilityLayer` 預設開。沒有滾輪縮放與框選縮放。
     - **d3**：與前一份報告相同。React 畫 SVG，d3 給 scale、path、`d3-zoom`、`d3-brush`、`d3-transition`。另外自己寫了每個像素欄取最小、最大值的抽樣，讓大量資料也只畫一條短的 path。
3. `bun run --cwd frontend build`，再 `bun run --cwd frontend check:dist`。
4. 以 Bun 提供 `dist/`：CSP 與 Report-Only 的 header 從 `services/web/src/headers.rs` 讀出，與正式環境逐字相同。
5. 以 DevTools protocol 操作 Chromium。在頁面的 script 執行前註冊 `securitypolicyviolation` 收集器，enforce 與 Trusted Types report 都收，也收 console 與例外。每個候選依序跑：
   - **互動**（1280 × 900）：
     - 第一次畫完，與 2 秒後各截一次圖，不同就表示有進場動畫。
     - 指到中央：確認 hover 那一列 Line A 的值出現在 DOM 裡，並截圖。
     - 鍵盤：Tab 進入圖表，按 3 次 ArrowRight，記錄焦點元素、tooltip 的列與 `aria-live` 的內容。
     - 點圖例的 Line B。
     - 在中央滾動滾輪。
     - 拖曳框選（d3 在範圍條上）。
     - 點中央的資料點。
     - 記錄 `<style>` 元素。
     - 最後卸載圖表，收卸載時才出現的違規。
   - **深色**：`prefers-color-scheme: dark`，指到中央後截圖。
   - **觸控**（390 × 844，deviceScaleFactor 2，touch emulation）：點一下中央、雙指張開、單指水平拖曳，以 `Input.dispatchTouchEvent` 送出。
   - **效能**：一條線 10,000 點與 100,000 點（1 分鐘一點），動畫關閉，每種各重新載入 3 次，取中位數。
     - 首次繪製：從掛上元件，到套件回報畫完之後的第二個 frame。
     - idle：靜置 1 秒。
     - hover：61 次 pointer 移動，間隔 16 ms，橫越繪圖區。
     - 縮放：滾輪縮放候選是 20 次滾輪，10 次放大、10 次縮小，間隔 16 ms；uPlot 是 20 步的拖曳框選。
     - 這三段期間記錄 `requestAnimationFrame` 的時間戳與 long animation frame（超過 50 ms 的 frame）。
     - 全部再以 `Emulation.setCPUThrottlingRate` 4 倍降速跑一次，模擬比建置主機慢的筆電或手機。AG Charts 已在第一輪排除，降速時沒有跑。

一共跑了兩輪：

- 第一輪：所有候選。
- 第二輪：ECharts 加上根元素的保護、觸控不開框選模式，並以 `?noshim=1` 不加保護再跑一次對照；Plotly 再跑一次，查剩下的 `<style>` 是誰寫的。

效能的數字是改完第二輪之後量的。實驗的程式只在拋棄式的副本裡執行，副本與容器映像在結束時刪除，不進 repo。

## 結果

原始資料：

- [ux-runs.json](data/2026-11-01-927-chart-library-ux/ux-runs.json)：互動、深色與觸控，`round` 是 A 或 B。
- [perf-runs.json](data/2026-11-01-927-chart-library-ux/perf-runs.json)：每次效能量測，`throttle` 是 1 或 4。

截圖不進 repo：每個候選有淺色、深色、手機寬度三張，ECharts 另有雙指縮放後一張。

### 建置與 CSP

`build` 與 `check:dist` 在加入全部候選後都通過。

| 候選 | enforced 違規 | Trusted Types（report-only） | 說明 |
| --- | --- | --- | --- |
| ECharts canvas，加根元素保護 | 0 | 0（含卸載） | 沒有保護時：初始化 1 個、卸載 1 個，都是 zrender 以 `root.innerHTML = ""` 清空根元素。保護讓這個寫入不經過 sink |
| ECharts SVG，加根元素保護 | 0 | 卸載時 1 | SVG renderer 卸載時還剩 1 個空字串的 `innerHTML` 寫入，沒有追到是哪個元素（沒有保護時是 2 個） |
| uPlot | 0 | 0 | 需要它的 `uPlot.min.css`，以樣式檔載入 |
| Plotly（SVG 與 WebGL） | 1（`style-src-elem`） | 2 | 照文件放了 `no-inline-styles` 的標記，Plotly 自己的 `<style>` 確實不寫了。但 strict bundle 裡的地圖 trace（maplibre）在載入時仍插入自己的 `<style>`；`&times;` 以 `innerHTML` 寫入。WebGL trace 在 strict bundle 下能執行 |
| AG Charts Community | 2（`style-src-elem`） | 12–14 | 樣式只能以 `<style>` 注入（`styleNonce` 要每個回應不同的 nonce，SPA 的 CSP 沒有）；版面、tooltip 都以 `innerHTML` 寫入。樣式被擋下後，圖例的無障礙文字疊在圖上，tooltip 不出現 |
| Recharts | 0 | 0 | |
| d3 自己寫 | 0 | 0 | |

### 功能（288 點，桌面與觸控）

標記：「內建」是套件自己的功能；「我們的」是實驗頁自己寫的；「沒有」是套件不提供，這一輪也沒有補。

| | ECharts | uPlot | Plotly | AG Charts Community | Recharts | d3 |
| --- | --- | --- | --- | --- | --- | --- |
| tooltip，值出現在 DOM | 是（ECharts 驅動，React 畫） | 是（我們的） | 是（內建，SVG 文字） | 不出現（樣式被擋） | 是（內建，內容是 React） | 是（我們的） |
| 十字線 | 內建（x 與 y，含座標標籤） | 內建 | 內建（spike line） | 要 Enterprise | 內建（只有垂直線） | 我們的 |
| 圖例切換 | 內建 | 內建 | 內建 | 點了沒有反應 | 我們的（`hide`） | 我們的 |
| 滾輪縮放 | 內建 | 沒有 | 內建 | 要 Enterprise | 沒有 | 我們的（`d3-zoom`） |
| 框選縮放 | 內建（toolbox） | 內建（x 方向拖曳） | 內建 | 要 Enterprise | 沒有 | 我們的（`d3-brush`） |
| 範圍導覽 | 內建（slider，帶資料縮圖） | 沒有 | 內建（rangeslider） | 要 Enterprise | 內建（`Brush`） | 我們的 |
| 點資料點看來源 | 可以（`convertFromPixel`） | 可以 | 可以（`plotly_click`） | 沒有回報 | 可以 | 可以 |
| 進場動畫 | 有 | 沒有 | 沒有（只有更新時的 transition） | 要 Enterprise | 有 | 有（我們的） |
| 鍵盤 | 我們的 `KeyNav` 加 `dispatchAction("showTip")`，實測可用 | 我們的 `KeyNav` 加 `setCursor`，可用 | `Fx.hover` 沒有觸發 hover 事件，只有我們的 live region | 內建，方向鍵可移動 | 內建，方向鍵可移動，但沒有讀出值；`Brush` 的 aria-label 是「Min value: undefined」 | 我們的，可用 |
| 螢幕閱讀器 | `aria` 產生圖的描述（文字可自訂）加我們的 live region | 我們的 live region | 我們的 live region | 內建的描述與圖例按鈕 | 圖例只有 icon 的名稱 | 我們的 live region |
| 觸控：點一下出 tooltip | 是 | 是 | 否 | 否 | 是 | 是 |
| 觸控：雙指縮放、單指平移 | 都可以（第二輪，框選模式不開時） | 都沒有 | 都可以 | 都沒有 | 都沒有 | 都可以（我們的） |
| 深色主題 | 可以（option 給 token 的顏色） | 可以 | 可以 | 可以（baseTheme） | 可以 | 可以 |
| 時區 | 只有瀏覽器的時區或 UTC（`useUTC`） | 任何時區（`tzDate`） | 沒有時區（以 UTC 顯示） | 沒有查 | 我們自己格式化 | 我們自己格式化 |

AG Charts 的 console 列出要 `ag-charts-enterprise` 的模組：`AnimationModule`、`ZoomModule`、`NavigatorModule`、`CrosshairModule`。這與 AG 的價目頁一致（見「商業授權的套件」）。

### 效能

一條線，3 次載入的中位數。

- **首次繪製**：毫秒。
- **hover**：hover 期間的 fps，括號裡是最長的 frame 間隔（ms）。
- **縮放**：縮放期間的 fps，括號裡是最長的 frame 間隔（ms）。

AG Charts 在第一輪已排除，降速時沒有跑。

| 候選 | 10k 首次繪製 | 100k 首次繪製 | 100k hover | 100k 縮放 | 4× 降速 10k 首次繪製 | 4× 降速 100k 首次繪製 | 4× 降速 100k hover | 4× 降速 100k 縮放 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ECharts canvas | 34 | 64 | 60（16.8） | 60（16.8） | 132 | 219 | 60（16.8） | 50（83） |
| ECharts SVG | 37 | 67 | 60（16.8） | 60（16.8） | 142 | 234 | 60（16.8） | 43（100） |
| uPlot | 28 | 29 | 60（16.8） | 60（16.8），拖曳 | 35 | 45 | 60（16.8） | 60（16.8），拖曳 |
| Plotly SVG | 39 | 85 | 60（16.8） | 59（33） | 158 | 332 | 60（33） | 49（100） |
| Plotly WebGL | 73 | 113 | 60（16.8） | 8（2117） | 268 | 406 | 60（33） | 9（2383） |
| AG Charts Community | 48 | 79 | 60（16.8） | 沒有縮放 | — | — | — | — |
| Recharts | 201 | 1352 | 57（67） | 沒有縮放 | 755 | 5066 | 17（117） | 沒有縮放 |
| d3 自己寫（含抽樣） | 26 | 29 | 60（16.8） | 60（16.8） | 44 | 67 | 60（16.8） | 60（16.8） |

- 沒降速時，hover 與縮放都維持約 60 fps，例外是 Recharts 100k 的 hover 與 Plotly WebGL 100k 的縮放。long animation frame 只出現在 Plotly 100k 的縮放（1 到 3 個）與 Recharts 100k 的 hover（1 個）。
- 4 倍降速時才分出差異：
  - ECharts canvas 縮放時有 10 個 long frame，阻塞共 82 ms，仍有 50 fps；SVG renderer 是 43 fps。
  - Plotly WebGL 的軟體 WebGL 在縮放時每個 frame 要 1 到 2 秒，在有 GPU 的瀏覽器會好很多，但這裡量不到。
  - Recharts 100k 首次繪製 5 秒，hover 只剩 17 fps。

頁面載入的 JS（解壓後，含 SPA 本身約 0.72 MB）供參考，不是條件：

| 候選 | 頁面載入的 JS |
| --- | --- |
| ECharts | 1.33 MB |
| uPlot | 0.78 MB |
| Plotly | 5.69 MB |
| AG Charts | 2.11 MB |
| Recharts | 1.10 MB |
| d3 | 0.81 MB |

### 外觀（截圖的觀察）

- **ECharts**：token 的顏色、字型都套得上，與 React 的 tooltip 放在一起也一致。深色、手機寬度都正常。範圍導覽的 slider 帶資料縮圖，顏色可以換成品牌色。要改的預設：
  - 圖例 icon 是空心圓加線。
  - 十字線的座標標籤用瀏覽器的時區、完整日期格式，要給 formatter。
- **uPlot**：乾淨、資訊密度高，數值顯示在圖下方的圖例表格裡。預設刻度是英文的 12 小時制，也沒有動畫，看起來是工具感，不是產品的儀表板。
- **Plotly**：有它一貫的樣子：每條線各一個 hover 標籤，範圍導覽是整張縮小的圖。時間軸只能以 UTC 顯示，與使用者的時區不一致。
- **AG Charts Community**：在 SPA 的 CSP 下版面是壞的。
- **Recharts**：乾淨的 SVG，`Brush` 的預設是灰色的條，要自己調整。
- **d3**：完全照設計系統（Tailwind 的 class），但每一個細節都是我們的程式。

### 商業授權的套件

條件是「授權可用於商業 SaaS」，商業授權要寫明費用，並且排除，除非沒有同等的開源選擇。2026-11-01 查各家的價目頁：

| 套件 | SaaS 的授權與費用 | 說明 |
| --- | --- | --- |
| Highcharts | SaaS 授權每位開發者 US$366（Core），Stock（範圍導覽）再加 US$366，可選年費或永久授權。一個授權涵蓋一個對外應用 | 來源：Highcharts 網路商店 |
| amCharts 5 | 「Single App or SaaS website」每位開發者年費 US$280，或永久授權 US$650 | 來源：amCharts 網路商店 |
| AG Charts Enterprise | 價目頁沒有公開價格，對外應用要洽業務。Zoom、Navigator、Crosshairs、Animations 都只在 Enterprise | 來源：AG Charts 的授權與價格頁。Community 版的 CSP 問題 Enterprise 一樣有（樣式只能以 `<style>` 注入） |

ECharts（Apache-2.0）的功能涵蓋這些商業套件在本案用得到的部分：縮放、範圍導覽、十字線、動畫、觸控。所以三者都排除，沒有實測。

## 結論

**選 ECharts 6，canvas renderer。**

- **硬性條件**：canvas renderer 加上根元素的保護後，enforced 與 Trusted Types 違規都是 0，卸載時也是 0。Apache-2.0，可以在 React 裡用。
- **功能**：唯一內建所有項目的開源候選：
  - tooltip（由它驅動、React 畫）、x 與 y 的十字線、圖例切換。
  - 滾輪縮放、框選縮放、帶資料縮圖的範圍導覽。
  - 進場與更新動畫。
  - 觸控的點按、雙指縮放與平移。
  - 深淺主題。
- **其他候選的不足**：
  - uPlot 最快，但沒有動畫、滾輪縮放、範圍導覽與觸控手勢。
  - Plotly 有 CSP 違規、沒有時區與進場動畫、觸控點按不出 tooltip。
  - AG Charts Community 有 CSP 違規，縮放、導覽、十字線、動畫要付費。
  - Recharts 沒有縮放，100k 點時明顯變慢。
  - d3 每一項都要自己寫。
- **效能**：100k 點在建置主機上首次繪製 64 ms，hover 與縮放 60 fps；4 倍降速時首次繪製 219 ms，縮放 50 fps。比 uPlot 與我們的 d3 慢，但互動流暢。
- **外觀**：顏色、字型、主題都能用 token 設定，tooltip 是設計系統的元件。
- **canvas 與 SVG**：canvas 在 4 倍降速時縮放較順（50 對 43 fps），而且沒有 Trusted Types 違規；SVG renderer 卸載時還剩一個。

ECharts 要守的規則寫在 [frontend/README.md](../../frontend/README.md)「儀表板元件的圖表套件」。

## 未驗證與延後

- 只量了 Chromium。Firefox、Safari 與實機的觸控沒有量（跨瀏覽器的重複驗證，依 issue 驗證分級延後）。
- 效能是在沒有 GPU 的建置主機上量的，Plotly WebGL 的數字不代表有 GPU 的瀏覽器。
- 長條、KPI、表格與狀態元件沒有量。這一輪只量時間序列，因為大量資料與互動的差異都在它上面。
- 這份實測沒有自動化。ECharts 的版本更新時要重跑「CSP」與「功能」兩項，這件事應做成自動測試（另開 issue）。
