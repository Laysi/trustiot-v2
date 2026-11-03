# StatTile

一個重點數字：標籤、數值與單位、狀態、與前一期的差異、最近一段的趨勢線；數值本身是按鈕，點開 SourcePopover 看它從哪裡來。

新增（儀表板）。

## 使用者提供

- `label`：設備與資料點，例如「冷卻水塔 2 號・出水溫度」（標籤不加冒號）。
- `value`、`unit`：已格式化的數值與單位（`stat-value` 樣式 30px、600，比例數字；單位 `body-sm`、`muted-foreground`）。
- `status`：選填，顯示 StatusBadge（soft）。
- `delta`：選填，`{ text, direction: up | down, judgement: good | bad | neutral }`；文字一律 `muted-foreground`，只有箭頭圖示依判斷上 `status-normal` 或 `status-critical`。溫度上升這類沒有好壞的變化用 neutral。
- `trend`：選填，最近 12 個以上的點；趨勢線 1.5px `muted-foreground`，最後一點 `chart-series-1`、外圈 2px `chart-point-ring`。
- `source`：資料來源物件，給 SourcePopover。

## 規則

- 外框同 Card：`radius-xl`、`border`、`card`、`shadow-sm`，內距 16px（`space-4`），各列間距 8px。
- 數值按鈕 hover、focus 或開啟時出現點狀底線與 Database 圖示；它的可及名稱是「標籤：數值 單位，查看資料來源」。
- 資料過舊或離線時數值改成 `muted-foreground`，差異列改寫「最後讀數 11/01 11:20，3 小時前」。
- 並排時用 `repeat(auto-fit, minmax(220px, 1fr))`、間距 16px；一個儀表板最多一個 `hero-value` 主數字。
