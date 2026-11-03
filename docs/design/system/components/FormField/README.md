# FormField

把 Label、控制項、說明與錯誤串起來的欄位外框：說明一直在，錯誤加在下面，控制項以 `aria-describedby` 同時指向兩者。

手寫自 `frontend/src/components/ui/form-field.tsx` 與 `label.tsx`。

## 使用者提供

- `label`：欄位名稱（`label` 樣式，14px、行高 1、font-weight 500）。
- `description`：選填的說明（`body-sm`、`muted-foreground`），例如時區提示。
- `error`：選填的錯誤字串（`body-sm`、500、`destructive`），出現時控制項被標成 `aria-invalid`。
- `children(control)`：render prop，把收到的 `id`、`aria-describedby`、`aria-invalid` 傳給 Input 或 NativeSelect。

## 規則

- `Label` 一定要有 `htmlFor`；不要用 placeholder 代替標籤。
- 欄位內各列間距 8px（`space-2`）；同一列的多個欄位用 `content-start` 對齊，旁邊有錯誤時控制項不會跟著下移。
- 錯誤句型：說要怎麼改，例如「結束時間要晚於開始時間。」
