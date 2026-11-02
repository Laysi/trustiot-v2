# Token 用途

這份文件記錄每個 token 拿來做什麼；**數值只寫在 [`tokens.css`](tokens.css)**，這裡不另存一份。用途說明裡出現的色碼與對比值是撰寫時的快照，和 `tokens.css` 不一致時以 `tokens.css` 為準，並順手更新這裡。改 token 的數值時，兩個主題都要重新確認對比（[README.md](README.md) 的「色彩」）。

`frontend/src/styles/global.css` 也定義的 token，數值必須與 `tokens.css` 一致；目前只有圖表的系列色不一致，見 [README.md](README.md) 的「未同步」。

## 顏色

淺色是 `:root` 與 `[data-theme="light"]`，深色是 `[data-theme="dark"]`；沒有另外標示的 token 兩個主題各有一個值（有些只是別名）。

### 品牌

| token | 用途 |
| --- | --- |
| `brand` | 【品牌】TrustIoT 的品牌色薄荷綠，依官網色票（mint #0d9b76，2026-11-01 維護者決定，取代標誌檔的 #a0eed6）。兩個主題同值，只當圖形與大字：標誌、規則線、圖示、大面積品牌區塊（提問泡泡、封面）。對白底 3.52:1、深色 background 5.43:1、card 5.09:1：可以當圖形（3:1），不可當小字或細框線；上面的字用 on-brand（5.37:1）。不要放在 brand-soft 上當圖形（2.94:1）。 |
| `brand-strong` | 【品牌】給按鈕、連結、選取與焦點框的品牌色。淺色是官網 mint-deep #086650：對 background／card 6.94:1、muted 6.32:1、accent 6.09:1、brand-soft 5.79:1。官網 mint-text #0a7a5c 對白底 5.31:1，但按鈕 hover（90%）的白字只有 4.42:1、在 brand-soft 上 4.44:1，不過 AA，所以不用。深色主題官網沒有對應值，取同一色相（OKLCH H168）的亮薄荷綠 oklch(0.87 0.09 168)（約 #98e8c9）：對 background 13.38:1、card 12.53:1、popover 11.97:1、muted 10.54:1、accent 9.88:1、brand-soft 10.34:1。 |
| `brand-soft` | 【品牌】帶品牌色調的表面（淺色是官網 mint-soft）：AI 回答裡的強調區、建議問題 hover、已釘選的元件。和底色差很少，一定配 border。上面的字：foreground 14.80:1／13.16:1、muted-foreground 5.01:1／5.96:1、brand-strong 5.79:1／10.34:1。不可做成小藥丸形狀：那是 StatusBadge 的樣子。 |
| `on-brand` | 【品牌】brand（#0d9b76）填色上的文字與圖示，兩個主題都是近黑的深色字：5.37:1。白字在 brand 上只有 3.52:1，不可用。 |
| `on-brand-strong` | 【品牌】brand-strong 填色上的文字與圖示：淺色白字 6.94:1（hover 90% 時 5.55:1），深色是深色字 #03140f 13.22:1（hover 10.79:1）。 |

### 介面

| token | 用途 |
| --- | --- |
| `background` | 頁面底色（body）。也是 outline 按鈕、輸入框與對話框的底色。 |
| `foreground` | 主要文字，用在 background、card、popover、muted 上（淺色 17.8:1、深色 16.0:1 對 card）。 |
| `card` | Card、StatTile 與圖表的底色；圖表色票的對比都以它為準。 |
| `card-foreground` | Card 內的文字，用在 card 上。 |
| `popover` | DropdownMenu、圖表 tooltip 與 SourcePopover 的底色。 |
| `popover-foreground` | popover 上的文字。 |
| `primary` | 主要動作（預設 Button）、連結、目前選取，指向品牌色 brand-strong：淺色 #086650（對 background、card 6.94:1，最差的面 accent 6.09:1），深色 oklch(0.87 0.09 168)（對 card 12.53:1，最差的面 accent 9.88:1）。舊的藍色值記在 primary-legacy。單一序列的折線不用 primary，改用 chart-series-1。 |
| `primary-foreground` | primary 填色上的文字與圖示：淺色白字 6.94:1，深色是 #03140f 深色字 13.22:1；不要寫死白色。舊值記在 primary-legacy-foreground。 |
| `secondary` | secondary Button 的底色。 |
| `secondary-foreground` | secondary 上的文字。 |
| `muted` | 低調的底：表格表頭、SQL 區塊、篩選列（bg-muted/40）。 |
| `muted-foreground` | 次要文字：說明、提示、座標軸文字、時間戳；用在 card（6.0:1 / 7.3:1）與 muted（5.5:1 / 6.1:1）上。 |
| `accent` | hover 與鍵盤焦點的底：ghost／outline Button、選單項目、目前導覽項、選取的表格列（bg-accent/60）。 |
| `accent-foreground` | accent 上的文字。 |
| `destructive` | 破壞性動作與錯誤：destructive Button、錯誤 Alert、欄位錯誤文字、aria-invalid 的框線；當文字用在 card 上（5.4:1 / 5.7:1）。 |
| `destructive-foreground` | destructive 填色上的文字。 |
| `success` | 成功與完成的文字與圖示（設定已儲存）；用在 card 上（5.6:1 / 8.5:1）。 |
| `border` | 所有元件的預設框線與分隔線、圖表格線。只當區隔用，不承載意義（1.3:1）。 |
| `input` | 輸入框、選單與 outline Button 的框線；對 background 3.1:1 / 3.3:1，符合控制項框線 3:1。 |
| `ring` | 鍵盤焦點的 outline（2px 實線、offset 2px），跟著品牌色 brand-strong：淺色最差的面（accent）6.09:1、brand-soft 5.79:1；深色最差的面（accent）9.88:1。舊的藍色值記在 ring-legacy。 |

### 舊預設（前端已改用品牌色）

| token | 用途 |
| --- | --- |
| `primary-legacy` | 舊預設，前端已改用品牌色（Laysi/trustiot-v2#929），`global.css` 沒有這個值。`primary` 的藍是新前端起步時的佔位色，不是品牌色。只為對照保留；新設計不要用。對 card 5.5:1 / 6.7:1。 |
| `primary-legacy-foreground` | 舊預設，前端已改用品牌色。primary-legacy 上的文字。 |
| `ring-legacy` | 舊預設，前端已改用品牌色。原本的 ring 藍。 |

### 遮罩

| token | 用途 |
| --- | --- |
| `overlay` | AlertDialog 背後的遮罩（bg-black/50），兩個主題相同。 |

### 狀態

| token | 用途 |
| --- | --- |
| `status-normal` | 【新增】狀態「正常」的圖示與文字；一律配 CircleCheck 圖示與「正常」字樣。 |
| `status-normal-soft` | 【新增】StatusBadge「正常」的底；status-normal 在上面 5.1:1 / 6.8:1。 |
| `status-warning` | 【新增】狀態「警告」的圖示、文字與警告門檻線；一律配 TriangleAlert 圖示與「警告」字樣。用在 card 上 5.0:1 / 10.1:1。 |
| `status-warning-soft` | 【新增】StatusBadge「警告」的底；status-warning 在上面 4.6:1 / 7.8:1。 |
| `status-critical` | 【新增】狀態「嚴重」的圖示、文字與嚴重門檻線；一律配 OctagonAlert 圖示與「嚴重」字樣。 |
| `status-critical-soft` | 【新增】StatusBadge「嚴重」的底；status-critical 在上面 4.8:1 / 4.6:1。 |
| `status-offline` | 【新增】狀態「離線」（閘道器或設備沒有回報）；一律配 CloudOff 圖示與「離線」字樣。 |
| `status-offline-soft` | 【新增】StatusBadge「離線」的底；status-offline 在上面 5.5:1 / 6.1:1。 |

### 圖表：分類色（序列）

| token | 用途 |
| --- | --- |
| `chart-series-1` | 【新增】分類色 1（藍）。多序列圖表依序指派 1→6，不循環、不依數值排名；單一序列的圖也用它。對 card 5.5:1 / 5.2:1。它和舊的 primary 藍同值，但 primary 已改成品牌色，兩者不再相關。 |
| `chart-series-2` | 【新增】分類色 2（橘）。對 card 3.4:1 / 3.6:1。 |
| `chart-series-3` | 【新增】分類色 3（青綠），2026-11-01 隨官網色票移到品牌色相（OKLCH H168，原 #218d74／#37a98d 是 H173），亮度與彩度不變。對 card 4.09:1 / 6.18:1。散佈圖等任兩點可能相鄰的圖，最多用 1–3。代表好壞的序列用狀態色而不是它。 |
| `chart-series-4` | 【新增】分類色 4（赭）。對 card 3.3:1 / 3.8:1；與 status-warning 同色系，同一張圖不可同時出現。 |
| `chart-series-5` | 【新增】分類色 5（紫）。對 card 7.5:1 / 5.5:1。 |
| `chart-series-6` | 【新增】分類色 6（洋紅）。對 card 4.0:1 / 3.5:1。第 7 個以上的序列併入「其他」或拆成小圖。 |

### 圖表：連續色階

| token | 用途 |
| --- | --- |
| `chart-seq-1` | 【新增】連續色階第 1 階（最小值）。深色主題反轉：小值靠近底色。只用在熱度圖這類連續數值，不能當獨立標記。 |
| `chart-seq-2` | 【新增】連續色階第 2 階。 |
| `chart-seq-3` | 【新增】連續色階第 3 階；有序分組（等級、區間）從這一階開始用（對 card ≥ 2:1）。 |
| `chart-seq-4` | 【新增】連續色階第 4 階。 |
| `chart-seq-5` | 【新增】連續色階第 5 階。 |
| `chart-seq-6` | 【新增】連續色階第 6 階。 |
| `chart-seq-7` | 【新增】連續色階第 7 階（最大值）。 |

### 圖表：發散色階

| token | 用途 |
| --- | --- |
| `chart-div-low-3` | 【新增】發散色階：低於基準最多（藍端）。例：與設定值的偏差、與上週同期比較。 |
| `chart-div-low-2` | 【新增】發散色階：低於基準。 |
| `chart-div-low-1` | 【新增】發散色階：略低於基準。 |
| `chart-div-mid` | 【新增】發散色階的中點：等於基準，中性灰，不帶色相。 |
| `chart-div-high-1` | 【新增】發散色階：略高於基準。 |
| `chart-div-high-2` | 【新增】發散色階：高於基準。 |
| `chart-div-high-3` | 【新增】發散色階：高於基準最多（橘端）。 |

### 圖表：外觀

| token | 用途 |
| --- | --- |
| `chart-grid` | 【新增】水平格線，1px 實線，不用虛線。與設備頁現有圖表相同（stroke-border）。 |
| `chart-axis` | 【新增】座標軸刻度線與刻度文字（12px、tabular-nums）。 |
| `chart-crosshair` | 【新增】十字準線：1px、4 4 虛線（foreground/50，與設備頁相同），吸附到最近的資料點。 |
| `chart-band` | 【新增】最小值到最大值的範圍帶（chart-series-1 藍的 15%，設備頁現況；原本寫作 primary/15，primary 改成品牌色後改依 chart-series-1）。 |
| `chart-selection` | 【新增】拖曳縮放（brush）時的選取範圍（chart-series-1 藍的 10%／12%）；邊線用 primary。 |
| `chart-tooltip` | 【新增】圖表 tooltip 的底，框線用 border、陰影用 shadow-md。 |
| `chart-point-ring` | 【新增】資料點與端點外圈 2px 的底色環，讓點壓在線上時仍清楚。 |

## 間距

Tailwind 4 的 0.25rem 級距，只列元件實際用到的；名稱就是 Tailwind 的數字（space-1.5 = p-1.5）。

| token | 用途 |
| --- | --- |
| `space-0.5` | Alert 標題與說明的列距（gap-y-0.5）、StatusBadge 的上下內距。 |
| `space-1` | 選單外框內距（p-1）、圖示與小字的間距。 |
| `space-1.5` | 選單項目與表格列的上下內距（py-1.5）、CardHeader 的列距。 |
| `space-2` | Button 內圖示與文字、FormField 內各列、選單項目左右內距（gap-2、px-2）。 |
| `space-2.5` | LatestCard 表格列的上下內距（py-2.5）。 |
| `space-3` | 輸入框左右內距、sm Button、表格儲存格（px-3）、Alert 圖示與文字的間距。 |
| `space-4` | Button 左右內距、Alert 左右內距、AlertDialog 內各區塊、StatTile 內距、頁面左右邊界（px-4）。 |
| `space-5` | HistoryCard 內各區塊（gap-5）。 |
| `space-6` | Card 上下與左右內距、Card 內各區塊、AlertDialog 內距（p-6、gap-6）、lg Button 左右內距。 |
| `space-8` | sm Button 高度（h-8）、摘要數值之間（gap-x-8）、頁面上下內距（py-8）。 |
| `space-9` | Button、輸入框的高度（h-9）；icon Button 的邊長。 |
| `space-10` | lg Button 高度（h-10）。 |

## 圓角

一個基準 --radius: 0.625rem，其他由它加減（@theme inline）。

| token | 用途 |
| --- | --- |
| `radius` | 基準值（10px）；改它會連動下面四階。 |
| `radius-sm` | calc(var(--radius) - 4px)：選單項目、StatusBadge。 |
| `radius-md` | calc(var(--radius) - 2px)：Button、輸入框、選單外框、SQL 區塊、圖表的焦點框。 |
| `radius-lg` | var(--radius)：Alert、AlertDialog、表格外框、空狀態、SourcePopover。 |
| `radius-xl` | calc(var(--radius) + 4px)：Card、StatTile。 |

## 陰影

Tailwind 4 的預設陰影，兩個主題相同；深色主題主要靠 border 區隔。

| token | 用途 |
| --- | --- |
| `shadow-xs` | 輸入框與選單（shadow-xs）。 |
| `shadow-sm` | Card、StatTile（shadow-sm）。 |
| `shadow-md` | DropdownMenu、圖表 tooltip、SourcePopover（shadow-md）。 |
| `shadow-lg` | AlertDialog（shadow-lg）。 |

## 透明度

透明度。

| token | 用途 |
| --- | --- |
| `opacity-disabled` | 停用的控制項（disabled:opacity-50）；停用時游標維持箭頭。 |
| `opacity-chart-area` | 【新增】面積圖的填色：序列色 10%。 |
| `opacity-chart-muted` | 【新增】圖例關掉以外、被 hover 序列以外的其他序列淡化程度。 |
| `opacity-refetch` | 【新增】重新查詢時保留上一張圖，降到這個透明度，不閃爍、不換骨架。 |

## 字體

字型家族是 `tokens.css` 的 `--font-sans` 與 `--font-mono`，字級、行高與粗細是同一個檔案裡的類別（`.title`、`.body-sm` 等）。

### 介面文字

系統字型，中文落在 Noto Sans TC、PingFang TC 或 Microsoft JhengHei；尺寸是 Tailwind 4 的預設級距。

| token | 用途 |
| --- | --- |
| `title` | CardTitle（text-lg leading-snug font-semibold）。 |
| `dialog-title` | AlertDialogTitle（text-lg font-semibold）。 |
| `body` | 窄螢幕的輸入框文字（text-base，避免手機自動放大）、數值摘要。 |
| `body-sm` | 介面預設：說明、表格、Alert、選單、寬螢幕的輸入框（text-sm）。 |
| `body-sm-medium` | Button、表頭、Alert 標題、選單標籤（text-sm font-medium）。 |
| `label` | 表單標籤 Label（text-sm leading-none font-medium）。 |
| `caption` | 座標軸刻度、StatusBadge、次要的中繼資料（text-xs）。 |

### 儀表板數字（新增）

新增：儀表板的大數字。用預設的比例數字，只有直欄對齊的表格與座標軸才用 tabular-nums。

| token | 用途 |
| --- | --- |
| `stat-value` | StatTile 的數值（text-3xl font-semibold）。 |
| `hero-value` | 一個儀表板最多一個主數字（text-5xl）。 |

### 程式碼

| token | 用途 |
| --- | --- |
| `code-sm` | 資料點的鍵（LatestCard 的 font-mono font-medium）。 |
| `code` | SourcePopover 的 SQL（text-xs font-mono）。 |
