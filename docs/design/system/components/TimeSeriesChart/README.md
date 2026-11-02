# TimeSeriesChart

隨時間變化的折線圖（或單一序列的面積圖），含十字準線與 tooltip、圖例切換、拖曳縮放、門檻線、鍵盤逐點讀取與資料來源。

延伸自 `frontend/src/pages/device/TimeSeriesChart.tsx`（設備頁的單序列 SVG 折線：格線 `border`、軸文字 `muted-foreground` 12px、線 2px、範圍帶 `primary/15`、選取點 r 4.5、虛線十字準線、slider 角色與方向鍵）。新增（儀表板）的部分：多序列、浮動 tooltip、圖例切換、拖曳縮放、門檻線、資料來源按鈕。

## 使用者提供

- `series`：`[{ name, points: [{ time, value | null }] }]`，依序取得 `chart-series-1…6`；`null` 或缺少的區間畫成斷線，不連過去。
- `from`、`to`、`intervalMs`、`timeZone`：時間軸範圍與區間長度。
- `unit`、`formatValue`、`formatTick`：數值與刻度的格式化（`Intl`）。
- `thresholds`：選填，`[{ value, status: warning | critical }]`，畫成 1px 狀態色實線並在右端標「警告 32°」。
- `label`：給輔助技術的名稱與用法說明。
- `source`：資料來源，給標題列的「資料來源」按鈕。

## 規則

- 高 260px；格線只畫水平線；Y 軸刻度取整數；時間軸 24 小時制，約每 110px 一個刻度。
- 線 2px、圓角接頭；面積只用在單一序列或累計值，`opacity-chart-area`。不用雙 Y 軸：不同單位拆成兩張圖。
- 超過 6 個序列：併成「其他」或拆成小圖。圖例一律顯示（兩個以上序列時），單一序列由標題說明，不放圖例。
- Tooltip：值在前、名稱在後，每列前面是 2px 的線段鍵；列出該時間點所有可見序列，沒有讀數的寫「沒有讀數」；靠右邊界時翻到左側。
- 圖例按鈕 `aria-pressed`；hover 或聚焦一個圖例時其他序列降到 `opacity-chart-muted`；關掉的序列不重新配色、Y 軸不重算。
- 拖曳超過 6 個區間放大，`chart-selection` 顯示範圍；「重設縮放」回到全範圍。鍵盤使用者改時間範圍篩選。
- 鍵盤：圖是 slider；左右鍵逐區間、PageUp/PageDown 跳十分之一、Home/End、Escape 取消；`aria-valuetext` 是該點的完整讀數。完整資料以 DataTable 提供。
- 重新查詢時保留舊圖，降到 `opacity-refetch`。
