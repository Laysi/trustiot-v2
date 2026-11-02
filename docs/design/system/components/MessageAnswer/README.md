# MessageAnswer

AI 的一則回答：一到三句結論、內嵌的互動元件，以及「釘選到儀表板」「資料來源」與追問。

新增（對話為主軸，2026-11-01）。TrustIoT 的儀表板、設備狀態、圖表與表格都先出現在回答裡；使用者覺得有用就釘選，存成儀表板。

## 使用者提供

- `text`：結論在前的短文字，數字照「內容基礎」的格式；以 `textContent` 放入，粗體只用在一個關鍵數字。
- `embeds`：內嵌元件的宣告式 spec，與儀表板存的是同一份（StatTile 列、TimeSeriesChart、BarChart、Gauge、DataTable）。元件就是「新增（儀表板）」那一組，規則完全相同，不另外做聊天版。
- `source`：整則回答用到的資料；「資料來源」按鈕打開 SourcePopover。元件自己的數字按鈕也照樣能開。
- `followups`：一到三個追問（SuggestedQuestions 的 `followups`）。
- `pinnedTo`、`onPin`、`onOpenDashboard`：釘選狀態。

## 規則

- 版面：左邊 32px 的標誌（#0d9b76 薄荷綠，直接畫在背景上）標示說話者，回答本身不加泡泡；使用者的提問是右側 `brand` 底、`on-brand` 字的泡泡（5.37:1）。文字 16px（`body`），內容欄最寬 768px，內嵌元件可以放寬到 960px。
- 內嵌元件放在 `ti-embed`（`card` 底、`border`、`radius-xl`、16px 內距）；一則回答最多三個元件，更多就建議「做成儀表板」。
- 動作列順序固定：「釘選到儀表板」（`default`，一則回答唯一的 primary 動作）、「資料來源」（ghost，Database 圖示）、追問（outline，小尺寸）。
- 釘選成功：按鈕換成 `brand-strong` 字的「已釘選到「冷卻水塔週報」」（Check 圖示）加「開啟儀表板」連結，以 `role="status"` 宣告；元件外框換成 `brand-strong`。
- 產生中：文字欄位置放 Spinner 加「正在查詢冷卻水塔 2 號的讀數…」，說明在查什麼；元件以骨架佔位。錯誤時放 destructive Alert 與「再試一次」。
- 不貼 SQL、不道歉、不寫「好的！」；SQL 只在資料來源裡。
