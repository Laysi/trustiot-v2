# NativeSelect

瀏覽器原生的 `<select>`，套上和 Input 相同的樣式，右側加一個 ChevronDown 圖示。

手寫自 `frontend/src/components/ui/native-select.tsx`。

## 為什麼不用 Radix Select

Radix 的 `SelectViewport` 會寫入 inline `<style>`，產品的 CSP 擋下它。原生 select 也免費提供鍵盤 type-ahead（長清單如時區）與手機的選擇器。下拉選單一律用 NativeSelect。

## 使用者提供

- 原生 `<select>` 的屬性與 `<option>`；放在 FormField 裡取得標籤。
- 右側預留 36px（`space-9`）給圖示，圖示 `muted-foreground`、不接收指標事件。
