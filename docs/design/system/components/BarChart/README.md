# BarChart

類別之間比大小的直條圖：一條基線、直條最粗 24px、頂端 4px 圓角、底端方角，數值標在頂端。

新增（儀表板）。

## 使用者提供

- `categories`：`[{ name, value }]`；單一序列全部用 `chart-series-1`，不要依數值上色。
- `unit`、`formatValue`；`source`。
- 多序列時用 `series`，依序取色，加圖例；相鄰直條之間留 2px 的底色間隙。

## 規則

- 從 0 開始；Y 軸刻度取整數、千分位；格線 `chart-grid`，基線 `input`。
- 類別超過約 8 個或名稱很長時改橫條。類別有順序（等級、區間）時用連續色階 `chart-seq-3…7`。
- 每一條（含上方空白）都是 hover 與聚焦目標（`tabindex="0"`、`aria-label` 寫類別與數值），聚焦時畫 2px `ring` 外框並顯示 tooltip。
- 值標籤用 `foreground` 文字，不用序列色。
