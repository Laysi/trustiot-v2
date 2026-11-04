# SourcePopover

「這個數字從哪裡來」的面板：資料集名稱、說明、查詢時間、資料範圍、筆數與 SQL，可以複製 SQL。

新增（儀表板）。產品決定儀表板上任何數字都可以點開看來源，讓使用者能檢查 AI 寫的儀表板。

## 使用者提供

- `title`：資料集名稱，例如「冷卻水塔 2 號・出水溫度（5 分鐘平均）」。
- `description`：一兩句說明資料怎麼來（取樣間隔、彙總方式）。
- `queriedAt`：查詢時間，絕對時間加時區「2026-11-01 14:32:05（UTC+8）」。
- `range`、`rowCount`：選填的資料範圍與筆數。
- `sql`：實際執行的查詢；以 `textContent` 放入，不可用 innerHTML。

## 規則

- 觸發點：StatTile 與 Gauge 的數值按鈕、圖表與表格卡片標題列的「資料來源」按鈕（Database 圖示）。觸發按鈕 `aria-haspopup="dialog"`、`aria-expanded`。
- 面板是非強制的 `role="dialog"`：開啟時焦點移到關閉鈕，Escape 或關閉鈕收起並把焦點還給觸發點。
- 外框 `popover` 底、`radius-lg`、16px 內距、`shadow-md`、最寬 384px。
- SQL 區塊 `muted` 底、`code` 樣式、最高 160px 內部捲動；複製結果用 `role="status"` 回報「已複製 SQL。」
