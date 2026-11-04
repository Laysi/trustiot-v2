# TrustIoT 設計系統

2026-11-02 起，這個目錄是設計系統的來源：規則（本文件）、token（[`tokens.css`](tokens.css) 的數值與 [`tokens.md`](tokens.md) 的用途說明）與 19 個元件的規格和預覽（[`components/`](components/)）。設計系統是 2026-11-01 由維護者做的，原本放在私人的設計檔裡；現在照原樣收進 repo，**之後以 repo 為準**：要改規則、token 或元件，直接改這裡，同一個 PR 一併更新受影響的規格、預覽與 `tokens.css`。各組畫面設計稿（[`../README.md`](../README.md)）只管畫面，token 與元件照這裡。

| 檔案 | 內容 |
| --- | --- |
| [`index.html`](index.html) | 入口頁：連到每個元件的預覽，分組列出，淺色與深色各一個連結 |
| `README.md`（本文件） | 規則：文案、色彩、字體、間距、互動、無障礙、儀表板、對話、標誌，以及和前端的差異（「未同步」） |
| [`tokens.css`](tokens.css) | token 的**唯一數值來源**：淺色、深色與兩個主題共用的值，加上字級類別 |
| [`tokens.md`](tokens.md) | 每個 token 的用途說明，不另存數值 |
| `components/<名稱>/README.md` | 元件規格：用途、使用者提供什麼、規則 |
| `components/<名稱>/preview.html` | 元件預覽：靜態頁，不連網路 |
| [`components/bundle.css`](components/bundle.css) | 預覽用的元件樣式，數值全部取自 `tokens.css` |
| [`components/index.d.ts`](components/index.d.ts) | 元件屬性的型別，當文件讀；已有的元件對照 `frontend/src/components/ui/`，儀表板與對話的元件是提案中的介面 |

怎麼看：clone 之後用瀏覽器直接打開 `index.html`（或任一個 `preview.html`），網址加 `?theme=dark` 看深色，不加時跟隨系統。預覽是自足的靜態頁，不連網路，也不需要建置。

## 概觀

TrustIoT 是工業 IoT 的監測 SaaS：組織把廠內的 Modbus 設備接到 TrustIoT 閘道器，平台保存讀數、畫出歷史並組成儀表板。讀這份系統的人是廠務、設備工程師與管理者，他們要的是可信、可查證的數字。產品以對話為主軸：打開 TrustIoT 看到的就是對話框，圖表與儀表板出現在 AI 的回答裡（見「對話為主軸」）。介面安靜、以資料為主，品牌色是官網色票的薄荷綠 `brand`（#0d9b76），按鈕、連結與焦點框用它的深色版 `brand-strong`，`primary` 指向它；其餘是中性灰與保留給狀態的顏色。

## 內容基礎

- 介面預設繁體中文（zh-TW），另有英文（en）。稱呼使用者用「你」，不用「您」。
- 句子短，陳述事實，接著說下一步。真實文案：「還沒有已存的讀數」「請確認閘道器在線，以及上方的連線設定正確。」「結束時間要晚於開始時間。」
- 硬體一律叫「TrustIoT 閘道器」（英文 TrustIoT gateway），上下文清楚時簡稱「閘道器」。不可用任何開發板的名稱稱呼它；上游軟體名稱（例如 Raspberry Pi OS）照原文。
- 按鈕以動詞開頭並說清楚做什麼：「顯示」「更新資料」「登出所有裝置」。確認對話框的標題是問句（「要登出所有裝置嗎？」），執行按鈕重述動作。
- 進行中用全形刪節號：「儲存中…」「正在載入資料點…」「登出中…」。
- 錯誤說發生什麼與怎麼做：「伺服器回應逾時，請再試一次。」不道歉、不用驚嘆號、不用 emoji。
- 標點用全形（，。：；「」（）），中文與英數字之間留一個半形空格（「最近 7 天」「閘道器 GW-0042」）。數字與單位之間空一格：28.4 °C、42.7 A、61 %RH、1,284 kWh；百分比不空格（-3.1%）。
- 數字與日期一律用 `Intl` 依語言與使用者時區格式化。完整時間寫絕對時間與時區：「2026-11-01 14:32:05（UTC+8）」；圖表的時間軸用 24 小時制 14:30。
- 設備與資料點的名稱是使用者的資料，照原樣顯示；資料點的鍵用 `code-sm`（例如 `temp_out`）。

## 色彩

- 顏色以用途命名（shadcn 的名稱），定義在 `:root` 與 `[data-theme="dark"]`。深色主題是另外挑選的值，不是自動反轉。主題由 `<html data-theme>` 切換，作業系統設定只在腳本執行前生效。
- 內文用 `foreground`，放在 `background`、`card`、`popover` 上；次要文字用 `muted-foreground`。卡片內的文字用 `card-foreground`。
- `primary` 只給一個區塊裡最主要的動作、連結與目前選取；它上面的字用 `primary-foreground`（深色主題是深色字，不要寫死白色）。2026-11-01 起 `primary` 與 `ring` 指向品牌色，見下面「品牌色」。
- `destructive` 只代表破壞性動作與錯誤；`success` 只代表完成。
- `border` 是 1px 的分隔線，只區隔不表意；控制項的框線用 `input`（對底 3:1）；焦點用 `ring`。
- 半透明疊色照原始碼的做法：hover 的 `primary` 90%、`secondary` 80%，選取列 `accent` 60%，遮罩 `overlay`。
- 每組文字與背景、輸入框框線與焦點框都符合 WCAG 2.2 AA（文字 4.5:1，框線與焦點 3:1）；改任何顏色都要在兩個主題重新確認。

### 品牌色

2026-11-01 決定：品牌色依 TrustIoT 官網的色票（mint #0d9b76、mint-text #0a7a5c、mint-deep #086650、mint-soft #dcefe6），取代先前取自標誌檔的 #a0eed6。官網沒有深色主題，深色主題的值是依同一色相（OKLCH H168）推導的。原本 `primary` 的藍是新前端起步時的佔位色，不是品牌色。舊值沒有刪：記在 `primary-legacy`、`primary-legacy-foreground`、`ring-legacy`，標為「舊預設（前端已改用品牌色）」。前端 `global.css` 的品牌色改版（Laysi/trustiot-v2#929）用同一組值，所以 `global.css` 沒有這三個 token。

| token | 淺色 | 深色 | 用途與對比 |
| --- | --- | --- | --- |
| `brand` | #0d9b76 | #0d9b76 | 官網 mint。只當圖形與大字：標誌、規則線、圖示、大面積品牌區塊（封面）。對白底 3.52:1、深色 `background` 5.43:1、`card` 5.09:1：過圖形的 3:1，不可當小字或細框線；不要當圖形放在 `brand-soft` 上（2.94:1） |
| `brand-strong` | #086650 | oklch(0.87 0.09 168)（約 #98e8c9） | 按鈕、連結、選取、焦點框，深色另外是提問泡泡的底。淺色是官網 mint-deep：對 `background`／`card` 6.94:1、`muted` 6.32:1、`accent` 6.09:1、`brand-soft` 5.79:1。深色對 `background` 13.38:1、`card` 12.53:1、`popover` 11.97:1、`muted` 10.54:1、`accent` 9.88:1、`brand-soft` 10.34:1 |
| `on-brand-strong` | #ffffff | #03140f | `brand-strong` 上的字：淺色 6.94:1（hover 90% 時 5.55:1）；深色 13.22:1（hover 10.79:1） |
| `brand-soft` | #dcefe6 | #0e2d25 | 帶品牌色調的表面（淺色是官網 mint-soft；建議問題 hover、已釘選、淺色的提問泡泡）。上面 `foreground` 14.80:1／13.16:1、`muted-foreground` 5.01:1／5.96:1；和底色差很少，一定配 `border` |
| `on-brand` | #03140f | #03140f | `brand` 上的字：5.37:1（白字只有 3.52:1，不可用） |

- `brand-strong` 與 `brand` 同色相（OKLCH H168）：淺色壓暗到能當文字與按鈕，深色提亮到能當深色背景上的文字與按鈕。官網的 mint-text #0a7a5c 對白底 5.31:1，但按鈕 hover（`primary` 90%）的白字只有 4.42:1、放在 `brand-soft` 上 4.44:1，不過 AA，所以淺色的 `brand-strong` 用 mint-deep #086650。
- `primary` 的新寫法是別名：`primary` = `{brand-strong}`、`primary-foreground` = `{on-brand-strong}`、`ring` = `{brand-strong}`，兩個主題同一個寫法，值由品牌 token 決定。建議所有新設計都用它；`primary-legacy` 只為對照保留。
- 品牌色和「正常」的綠是同一個色族：淺色 `brand-strong`（OKLab L 0.46）與 `status-normal`（L 0.50）亮度對比只有 1.23:1，深色（L 0.87 對 0.75）1.47:1，所以它們靠角色分開，不靠顏色：狀態一律 StatusBadge（圖示加「正常」字樣），品牌色只出現在按鈕、連結、選取與標誌。`brand-soft` 和 `status-normal-soft` 很接近，所以 `brand-soft` 不可以做成小藥丸。

## 字體

- 只用系統字型：`system-ui, -apple-system, "Segoe UI"`，中文落在 Noto Sans TC、PingFang TC 或 Microsoft JhengHei。等寬字用 `ui-monospace` 系列。
- 介面以 14px 為主（`body-sm`、`body-sm-medium`），表單標籤用 `label`，卡片標題 `title`，對話框標題 `dialog-title`，座標軸與徽章 `caption`。窄螢幕的輸入框用 16px（`body`），避免手機自動放大。
- 會直欄對齊的數字（表格、座標軸、讀數列）加 `tabular-nums`；大數字（`stat-value`、`hero-value`）保持比例數字。
- 粗細只有三種：400、500（按鈕、標籤、表頭）、600（標題與數值）。

## 間距與版面

- 間距是 Tailwind 的 4px 級距，用 `space-*`。控制項高 36px（`space-9`），小按鈕 32px，大按鈕 40px。
- Card 的內距與區塊間距都是 24px（`space-6`）；儀表板的 StatTile 與圖表卡片收緊到 16px（`space-4`）。
- 頁面內容最寬 72rem（1152px），左右 16px、上下 32px；頁首最小高 56px。
- 儀表板：篩選列在最上方一列（時間範圍放第一個），下面是 StatTile 列（`repeat(auto-fit, minmax(220px, 1fr))`、間距 16px），再下面是圖表與表格，寬螢幕可兩欄。篩選套用到下面所有元件，數字彼此一致。

## 圓角、框線、陰影

- 圓角由一個基準 `radius`（0.625rem）推出：`radius-sm` 選單項目與徽章、`radius-md` 按鈕與輸入框、`radius-lg` Alert、對話框、表格外框與面板、`radius-xl` 卡片。
- 用框線分區，陰影很輕：`shadow-xs` 輸入框、`shadow-sm` 卡片、`shadow-md` 選單與 tooltip、`shadow-lg` 對話框。深色主題的陰影幾乎看不見，靠 `border` 區隔。
- 不用漸層、不用彩色左邊框的卡片、不用裝飾圖片。

## 互動狀態

- 鍵盤焦點：2px `ring` 實線 outline、offset 2px，所有控制項共用（強制色彩模式下也看得到）。
- hover：填色按鈕變 90% 或 80%，outline 與 ghost 按鈕、選單項目換成 `accent` 底。
- 停用：`opacity-disabled`，游標維持箭頭。可以點的元素一律顯示手形，由全域樣式處理，元件不自己寫游標。
- 欄位錯誤：控制項 `aria-invalid`，框線 `destructive`，下面寫錯誤文字。

## 動態

動態不是 token，數值記在這裡：

- 顏色轉換 150ms、`cubic-bezier(0.4, 0, 0.2, 1)`（Tailwind 的 `transition-colors`）。
- 選單與對話框進出場 150ms、`ease`：淡入加從 95% 縮放（`tw-animate-css`），選單另外從觸發方向滑入 8px。
- Spinner 一圈 1 秒、linear。
- 【新增】圖表：hover 與圖例淡化 120ms；換資料直接重畫、不做形變動畫；重新查詢時保留舊圖並降到 `opacity-refetch`，不換成骨架。tooltip 跟著指標，不加延遲。
- `prefers-reduced-motion: reduce` 時所有動畫與轉換縮成 0.01ms（全域樣式），新元件不必另外處理，但不能用 JavaScript 自己跑動畫。

## 圖示

- 用 `lucide-react`，線條 2px、跟著文字色（currentColor）。按鈕、選單與 Alert 裡 16px，StatusBadge 與 tooltip 裡 14px。
- 常用：CircleCheck（成功、正常）、TriangleAlert（錯誤、警告）、CloudOff（離線）、LoaderCircle（載入）、RefreshCw（更新資料）、ChevronDown、Check、LogOut、Settings、Building2（組織）。【新增】OctagonAlert（嚴重）、Database（資料來源）、ZoomOut（重設縮放）、TrendingUp／TrendingDown（差異）。
- 裝飾性圖示 `aria-hidden`；只有圖示的按鈕一定要 `aria-label`。不用 emoji，不自己畫圖示。
- 標誌是 `frontend/public/favicon.svg`，規則見下面的「標誌」。
- 【新增】對話用的圖示：ArrowUp（送出）、Square（停止）、Pin（釘選）、Check（已釘選）、History（跟上週比）、PanelRightOpen（開啟儀表板面板）、Menu（選單）、Zap、Thermometer、Router、CalendarDays（建議問題的類別）。
- 這份系統的預覽把 lucide 的路徑手寫進 SVG，產品程式一律從 `lucide-react` 匯入。

## 標誌

標誌檔是 repo 裡的 [`frontend/public/favicon.svg`](../../../frontend/public/favicon.svg)，前端的 favicon 與這份系統的預覽用的是同一個檔案。它是 TrustIoT 的標誌：單色，墨色是官網色票的薄荷綠 #0d9b76（`brand`），寫死在檔案裡，不隨主題變。不要重畫、拉伸或加效果。

- 兩個主題都直接畫在背景上，沒有底色方塊（`ti-logo`：頁首 32px、首頁空狀態 64px）：對白底 3.52:1、深色 `background` 5.43:1、`card` 5.09:1，都過圖形的 3:1。
- 不要放在 `brand-soft`（2.94:1）或 `brand` 填色上。
- 頁首在標誌右邊放 font-weight 600 的「TrustIoT」字樣，間距 8px。

## 無障礙

- 目標 WCAG 2.2 AA。顏色從不單獨表意：狀態是圖示加文字，序列有圖例與直接標籤，錯誤有文字。
- 每個控制項都有標籤（`Label` 一定有 `htmlFor`）；說明與錯誤以 `aria-describedby` 連到控制項。
- 錯誤 Alert 用 `role="alert"`，一般通知用 `role="status"`；有標籤的 Spinner 是 `role="status"`。
- 全部可以用鍵盤操作。圖表是 slider，方向鍵逐點讀出；每張圖都有同樣資料的表格版本。
- 產品的 CSP 只允許同源的 script 與 style：產品裡不能有 inline `style`、`<style>` 或 `innerHTML`，圖表以 SVG 屬性與 class 繪製（這份系統的預覽為了簡潔用了 inline style，產品程式不可照抄）。下拉選單用 NativeSelect，不用 Radix Select。

## 新增（儀表板）

2026-11-01 產品決定：儀表板由平台寫好的互動圖表元件繪製，AI 只寫宣告式的 spec（要哪些資料、哪種圖、哪些門檻）。原本前端只有設備頁的單序列折線，沒有多序列配色、狀態色、資料來源與圖表互動的規則，所以補上這一節與對應的 token 和元件（StatTile、TimeSeriesChart 的互動、BarChart、Gauge、DataTable、StatusBadge、SourcePopover）。spec 只能指定序列名稱與狀態，不能指定顏色：顏色一律由元件依下列規則決定。

### 選圖

- 隨時間變化 → TimeSeriesChart（折線；面積只用在單一序列或累計值）。
- 類別之間比大小 → BarChart。
- 一個重點數字 → StatTile；整個儀表板最多一個 `hero-value`。
- 一個數字和門檻比 → Gauge。
- 要看明細或每張圖的完整數值 → DataTable。
- 不用雙 Y 軸（不同單位拆成兩張圖），不用 3D，不用圓餅圖。

### 分類色（序列）

依序指派 `chart-series-1` 到 `chart-series-6`，不循環、不依數值排名；顏色跟著實體走，關掉或篩掉一個序列時其他序列不換色。超過 6 個併入「其他」或拆成小圖；散佈圖這類任兩點都可能相鄰的圖最多用前 3 個。相鄰兩色的亮度交錯，色覺障礙者也分得出來。

| token | 淺色 | 對 `card` | 深色 | 對 `card` |
| --- | --- | --- | --- | --- |
| `chart-series-1` 藍 | #1463dc | 5.5:1 | #3e89f7 | 5.2:1 |
| `chart-series-2` 橘 | #df6815 | 3.4:1 | #b85207 | 3.6:1 |
| `chart-series-3` 青綠 | #2b8d6e | 4.1:1 | #40a986 | 6.2:1 |
| `chart-series-4` 赭 | #b88513 | 3.3:1 | #966c13 | 3.8:1 |
| `chart-series-5` 紫 | #6737b7 | 7.5:1 | #9b7be9 | 5.5:1 |
| `chart-series-6` 洋紅 | #d64992 | 4.0:1 | #c13480 | 3.5:1 |

已用色票驗證工具確認（OKLab ΔE×100，Machado 2009 色覺模擬）：相鄰兩色在紅色盲、綠色盲下最差 9.2（淺）、13.4（深），一般視覺最差 17.7、18.5；前 3 色兩兩之間也通過（色覺 9.2／13.7，一般 23.2／21.4）。所有序列色對 `card` 至少 3:1。指令與輸出見 [`docs/test-reports/2026-11-02-1003-chart-series-palette.md`](../../test-reports/2026-11-02-1003-chart-series-palette.md)。

- 2026-11-01 把 `chart-series-3` 從 H185 的青（#018d82／#23a99c）移到品牌色相，同日隨官網色票再從 H173（#218d74／#37a98d）移到 H168，亮度與彩度不變（上面的驗證數字是 2026-11-02 以 H168 重新量的），讓圖表裡有一個和品牌一致的顏色。品牌色不放第 1 位：第 1 位是單一序列與最常出現的顏色，藍色對 `card` 5.5:1，而且離「正常」的綠最遠；若讓青綠當第 1 位，單一序列的圖旁邊放一個「正常」徽章時會像同一種意思。

- 標記用序列色，文字不用：數值、圖例、標籤都用 `foreground` 或 `muted-foreground`，旁邊放一個色塊或線段表示身分。
- `chart-series-4`（赭）和 `status-warning` 同色系：同一張圖不能同時出現。代表好壞的序列（錯誤率、合格率）用狀態色，不用分類色。

### 連續與發散色階

- 連續（數量大小，例如熱度圖）：`chart-seq-1` 到 `chart-seq-7`，單一藍色相、亮度單調。淺色主題小值淺、大值深；深色主題反過來，小值靠近底色。有序分組（等級、區間）只用 `chart-seq-3` 以上（對底至少 2:1）。
- 發散（高於或低於基準，例如與設定值的偏差）：`chart-div-low-3` 到 `chart-div-high-3`，藍端代表低、橘端代表高，中點 `chart-div-mid` 是中性灰。兩臂階數相同，不用紅綠。

### 狀態

| 狀態 | 圖示 | 文字 | 前景 | 底 |
| --- | --- | --- | --- | --- |
| 正常 | CircleCheck | 正常 | `status-normal` | `status-normal-soft` |
| 警告 | TriangleAlert | 警告 | `status-warning` | `status-warning-soft` |
| 嚴重 | OctagonAlert | 嚴重 | `status-critical` | `status-critical-soft` |
| 離線 | CloudOff | 離線 | `status-offline` | `status-offline-soft` |

- 狀態一律圖示加文字（StatusBadge），顏色只是第三層提示。前景對底與對 `card` 都至少 4.5:1。
- 狀態色保留給狀態：不能當序列色。正常與嚴重沿用 `success`、`destructive`，警告是新增的琥珀色，離線是中性灰（閘道器或設備沒有回報，不代表故障）。
- 門檻線用 1px 的 `status-warning`、`status-critical` 實線，右端標「警告 32°」。

### 圖表外觀

- 格線 `chart-grid` 1px 實線、只畫水平線；刻度與刻度文字 `chart-axis`、12px、`tabular-nums`；BarChart 的基線 `input`。
- 線 2px、圓角接頭；資料點半徑至少 4px，外圈 2px `chart-point-ring`；面積 `opacity-chart-area`；最小到最大的範圍帶 `chart-band`。
- 直條最粗 24px、頂端 4px 圓角、底端方角；相鄰直條之間留 2px 底色間隙。
- 標籤要挑：只標端點、極值或關鍵那一條，不在每個點上寫數字。

### 互動

- Tooltip（`chart-tooltip`、`border`、`shadow-md`、`radius-md`）：十字準線（`chart-crosshair`，1px、4 4 虛線）吸附到最近的時間點，列出該點所有可見序列；值在前（14px、600），名稱在後（`muted-foreground`），每列以線段表示序列。直條圖每一條本身是目標，不畫十字準線。標籤一律用 `textContent` 放入。
- 圖例是切換按鈕（`aria-pressed`）；hover 或聚焦某一項時其他序列降到 `opacity-chart-muted`。
- 縮放：在圖上拖曳選取（`chart-selection`），放開後放大；工具列出現「重設縮放」。鍵盤使用者用時間範圍篩選。
- 鍵盤與觸控能做到 hover 能做的事：圖表可聚焦，方向鍵移動、Escape 取消；tooltip 只是加強，所有數值也在 DataTable 裡。
- 重新查詢時保留舊圖並降到 `opacity-refetch`，卡片標題列寫「正在更新…」。

### 資料來源

- 儀表板上每個數字都能點開看它從哪裡來。StatTile 與 Gauge 的數值本身是按鈕：hover、聚焦時出現點狀底線與 Database 圖示；圖表與表格在卡片標題列放「資料來源」按鈕。
- 點開的 SourcePopover 列出資料集名稱、一兩句說明、查詢時間（含時區）、資料範圍、筆數與實際的 SQL，可以複製 SQL。

## 對話為主軸

2026-11-01 產品決定：TrustIoT 的核心是對話。打開 TrustIoT 的首頁就是對話框；儀表板、設備狀態、圖表與表格都是 AI 回答裡的互動元件，可以釘選成儲存的儀表板。設定、設備管理、閘道器與成員放在選單裡，是次要的。

### 版面原則

- 對話是主畫面。首頁是全寬的一欄對話：文字內容欄最寬 768px 置中，回答裡的內嵌元件可以放寬到 960px。沒有側邊導覽。
- 頂列細、56px：左邊 `ti-logo` 加「TrustIoT」字樣與組織切換，右邊一個「選單」（設備、閘道器、成員、設定）與帳號。
- 空狀態：64px 標誌、一句問候（「午安，想先看哪一台設備？」）、ChatComposer、SuggestedQuestions（4–6 個依該組織的設備產生的問題）。點建議只會填進輸入框。
- 對話開始後，ChatComposer 固定在內容欄底部，上面是 MessageAnswer 串。
- 回答裡的元件就是「新增（儀表板）」那一組（StatTile、TimeSeriesChart、BarChart、Gauge、DataTable、StatusBadge、SourcePopover），配色、互動、資料來源規則完全相同；不做「聊天版」的圖。
- 釘選：「釘選到儀表板」存的是元件的宣告式 spec，不是截圖，所以儀表板上的數字會繼續更新。成功時以 `role="status"` 回報「已釘選到「冷卻水塔週報」」，並給「開啟儀表板」。
- 儀表板從對話開啟：寬螢幕是右側面板（最寬 560px，`border` 分隔，對話仍在左邊、可以繼續問），面板頂端有「在新頁開啟」，開成原本的儀表板頁；窄螢幕直接整頁，返回回到對話。
- 窄螢幕（< 768px）：同一欄，內嵌圖表撐滿寬度，StatTile 一列兩個，輸入框貼底。

### 誰在說話

- 使用者的提問：右側的泡泡（`ti-ask`），淺色與深色看起來都是淺薄荷底、深色字，用 `ask`、`on-ask`、`ask-border` 三個 token。淺色是 `ask`（= `brand-soft`）底、`on-ask`（= `foreground`）字，14.80:1，外加 1px 的 `ask-border`（= `border`）：`brand-soft` 和頁面底色差很少，一定配框線。深色是 `ask`（= `brand-strong`）底、`on-ask`（= `on-brand-strong`）字，13.22:1，框線與底同色。原本是 `brand` 底配 `on-brand` 字（5.37:1），2026-11-02 維護者看過畫面，覺得綠底黑字不好閱讀，改成現在的配色。
- AI 的回答：不加泡泡，左邊 32px 標誌（直接畫在背景上）標示說話者（`ti-answer`）。結論在前、一到三句，數字照「內容基礎」。
- 產生中說明正在查什麼；錯誤說發生什麼與「再試一次」。

### 元件

- ChatComposer：輸入框、送出（產生中換成「停止」）。
- SuggestedQuestions：首頁卡片與回答下的追問。
- MessageAnswer：結論、內嵌元件、「釘選到儀表板」「資料來源」與追問。

## 未同步

設計系統與 repo 的前端目前不一致的地方（2026-11-02，比對 `frontend/src/styles/global.css`）。補齊或改完之後，把對應的條目刪掉。

- **兩邊都定義的 token 一致，其他的還沒進前端**：兩邊都定義的 token（品牌、介面與 `ring` 的顏色，圖表的系列色（`chart-series-N` ↔ 前端的 `--chart-N`），淺色與深色；`--radius` 與推導出的四階圓角；`--font-sans`）數值相同。設計系統另有提問泡泡（`ask`、`on-ask`、`ask-border`）、狀態色（`status-*`）、連續與發散色階（`chart-seq-*`、`chart-div-*`）、圖表外觀（`chart-grid`、`chart-axis` 等）、`overlay`、間距、陰影、透明度、字級類別與 `--font-mono`，`global.css` 沒有；`primary-legacy` 等三個舊預設值是對照用，前端也不會有。第一個用到其中一個 token 的畫面把它補進 `global.css`，數值照這裡，兩個主題都補。`overlay`、`shadow-*` 與間距在前端由 Tailwind 的工具類別給（`bg-black/50`、`shadow-*`、`p-*`），設計系統的值就是 Tailwind 4 的預設。
- **儀表板的元件**：前端的 `frontend/src/dashboard/`（Laysi/trustiot-v2#895）已有圖表、數值、狀態與表格元件，名稱和這裡的元件規格不同（例如沒有叫 `StatusBadge` 的元件），它們與各元件規格的其他細節尚未逐項比對；比對時以本目錄為準。
- **對話為主軸的三個元件**：ChatComposer、SuggestedQuestions、MessageAnswer 是 2026-11-01 的設計決定，前端還沒有實作。
- **預覽是靜態重現**：元件預覽是依原始碼手寫的靜態重現（HTML 加 `components/bundle.css`），不是建置後的 React 元件：Radix 的焦點鎖、portal、定位、捲動鎖與 `tw-animate-css` 的進出場動畫沒有重現。預覽為了簡潔用了 inline style 與 inline script，產品程式不可照抄（見「無障礙」）。
- **不在元件集裡**：設備頁的 `EmptyState` 與頁首的 `Layout`、`TenantSwitcher`。共用外框（頁首、對話與選單的版面）的規格由 Laysi/trustiot-v2#1000 決定。
