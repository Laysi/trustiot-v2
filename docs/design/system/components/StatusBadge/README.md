# StatusBadge

設備、閘道器或數值的狀態標籤：正常、警告、嚴重、離線，一律是圖示加文字，從不只靠顏色。

新增（儀表板）。依產品 2026-11-01 的儀表板決定設計，現有前端還沒有這個元件。

## 使用者提供

- `status`：`normal | warning | critical | offline`。文字與圖示由元件決定，使用者不能自訂顏色。
- `variant`：`soft`（預設，淺底，用在卡片標題列）或 `plain`（無底，用在表格的狀態欄）。
- `label`：選填，覆寫顯示的字，例如「離線 3 小時」。

## 對應

| status | 圖示（lucide） | 文字 | 前景 / 底 |
| --- | --- | --- | --- |
| normal | CircleCheck | 正常 | `status-normal` / `status-normal-soft` |
| warning | TriangleAlert | 警告 | `status-warning` / `status-warning-soft` |
| critical | OctagonAlert | 嚴重 | `status-critical` / `status-critical-soft` |
| offline | CloudOff | 離線 | `status-offline` / `status-offline-soft` |

## 規則

- 12px（`caption`）、font-weight 500、圖示 14px、內距 2px × 8px、`radius-sm`。
- 前景對底與對 `card` 都至少 4.5:1（兩個主題都已確認）。
- 狀態色保留給狀態：不能拿來當圖表的第 N 個序列。
