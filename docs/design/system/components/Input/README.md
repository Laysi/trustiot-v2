# Input

單行文字輸入框，和 NativeSelect 共用同一組控制項樣式（`controlClasses`）。

手寫自 `frontend/src/components/ui/input.tsx`。

## 使用者提供

- 原生 `<input>` 的所有屬性；一定要有標籤，通常放在 FormField 裡由它給 `id`、`aria-describedby` 與 `aria-invalid`。
- 值被拒絕時設 `aria-invalid`，框線變成 `destructive`；錯誤文字由 FormField 顯示。

## 規則

- 高 36px（h-9）、左右 12px、圓角 `radius-md`、框線 `input`（對底色 3:1）、`shadow-xs`。
- 窄螢幕字級 16px（`body`），避免手機瀏覽器自動放大；768px 以上 14px（`body-sm`）。
- placeholder 用 `muted-foreground`，只放範例，不取代標籤。
