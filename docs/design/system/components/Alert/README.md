# Alert

頁面內的訊息框；錯誤用 `role="alert"`（立刻讀出），不需要注意的通知改傳 `role="status"`。

手寫自 `frontend/src/components/ui/alert.tsx`。

## 使用者提供

- `variant`：`default`（`card` 底、`card-foreground`）或 `destructive`（框線 `destructive` 50%、文字 `destructive`）。
- 選填的第一個子元素是 16px 的 lucide 圖示（成功 CircleCheck、錯誤 TriangleAlert），有圖示時第一欄 16px、間距 12px。
- `AlertTitle`（font-weight 500）與 `AlertDescription`（`body-sm`、`muted-foreground`，段落行高 1.625）。

## 規則

- 內距 12px × 16px、圓角 `radius-lg`。
- 錯誤訊息寫發生什麼與下一步：「無法載入歷史：伺服器回應逾時，請再試一次。」
- 需要使用者動作時，把按鈕放在右側欄（`grid-cols-[1fr_auto]`），例如「再試一次」。
