# Spinner

旋轉的 LoaderCircle；有 `label` 時是 `role="status"` 並讀出文字，沒有時只是旁邊文字的裝飾。

手寫自 `frontend/src/components/ui/spinner.tsx`。

## 使用者提供

- `label`：選填，整頁或整區載入時給，例如「正在載入資料點…」。
- `className`：調整尺寸（預設 16px）。

## 規則

- 放在按鈕裡時取代原本的圖示，按鈕文字同時改成進行式（「登出中…」），按鈕停用。
- 轉一圈 1 秒、linear；`prefers-reduced-motion` 時停住。
- 資料重新查詢時不要用 Spinner 蓋掉圖表：保留上一張圖並降到 `opacity-refetch`，在卡片標題列顯示「正在更新…」。
