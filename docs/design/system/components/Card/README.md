# Card

頁面上的一個區塊：標題、說明、右上角的動作、內容與頁尾。

手寫自 `frontend/src/components/ui/card.tsx`。

## 使用者提供

- `CardHeader` 內放 `CardTitle`（`title` 樣式；用 `as` 選 h1–h3，預設 h2）與 `CardDescription`。
- `CardAction`：標題列右端的小東西，例如儲存狀態、重新整理、圖表的工具列。
- `CardContent`、`CardFooter`：左右 24px（`space-6`）。

## 規則

- 外框 `radius-xl`、`border`、`card` 底、`shadow-sm`；上下內距與區塊間距都是 24px。
- 卡片之間不要再包卡片；需要分區時用卡片內的 `muted` 底區塊（例如篩選列 `bg-muted/40`、`radius-lg`）。
- 儀表板的 StatTile 是比較緊湊的卡片（16px 內距），圖表卡片把間距縮到 16px（`space-4`）。
