# DataTable

明細表：表頭固定、數字靠右、可排序；它也是每張圖表的完整無障礙版本。

延伸自 `frontend/src/pages/device/HistoryCard.tsx` 的歷史表（`max-h-96` 捲動、`radius-lg` 外框、`muted` 表頭 sticky、儲存格 6px × 12px、`tabular-nums`）。新增（儀表板）的部分：可排序表頭、狀態欄、選取列。

## 使用者提供

- `columns`：`[{ key, header, align: start | end, sortable }]`；數字欄 `end`。
- `rows`；選填 `selectedKey`（`aria-selected`，底色 `accent` 60%）。
- `caption`：給螢幕閱讀器的表名（畫面上隱藏）。
- `source`：表的資料來源，由所在卡片標題列的「資料來源」按鈕開啟。

## 規則

- 列首用 `<th scope="row">`；排序按鈕放在 `<th>` 裡，`aria-sort` 在 `<th>` 上，圖示 ChevronsUpDown／ArrowDown／ArrowUp。
- 狀態欄用 StatusBadge 的 plain 樣式。
- 文字過長截斷時加 `title`，數值欄不截斷。
- 超過高度 384px 時表內捲動，表頭固定。
