# DropdownMenu

按鈕打開的動作選單，可放標籤、分隔線、一般項目、破壞性項目與單選項目。

手寫自 `frontend/src/components/ui/dropdown-menu.tsx`（Radix Dropdown Menu）。預覽畫出開啟狀態，不重現 Radix 的定位、鍵盤導覽與進出場動畫。

## 使用者提供

- `DropdownMenuTrigger`、`DropdownMenuContent`（預設離觸發點 4px）。
- `DropdownMenuItem`（`variant="destructive"` 為紅字）、`DropdownMenuRadioGroup` + `DropdownMenuRadioItem`、`DropdownMenuLabel`、`DropdownMenuSeparator`。

## 規則

- 外框 `popover` 底、`radius-md`、4px 內距、`shadow-md`、最小寬 8rem。
- 項目 6px × 8px 內距、`radius-sm`；鍵盤焦點與 hover 都是 `accent` 底。圖示 16px、`muted-foreground`。
- 停用項目用 `data-disabled`，透明度 `opacity-disabled`、游標維持箭頭。
- 只放動作；要從清單選一個值（例如時間範圍）用 NativeSelect。
