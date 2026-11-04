# AlertDialog

打斷操作的確認對話框，只用在難以復原的動作；焦點移入並鎖住，Escape 與「取消」關閉，背景捲動鎖住。

手寫自 `frontend/src/components/ui/alert-dialog.tsx`（Radix Alert Dialog）。預覽把開啟狀態畫在原地，不重現 Radix 的焦點鎖、portal 與進出場動畫。

## 使用者提供

- `AlertDialogTrigger`（通常 `asChild` 包一個 Button）。
- `AlertDialogContent` 內：`AlertDialogHeader`（`AlertDialogTitle` 用 `dialog-title` 樣式、`AlertDialogDescription`）與 `AlertDialogFooter`。
- `AlertDialogCancel`（outline 樣式）與一個執行動作的 `Button`（通常 `destructive`）。

## 規則

- 標題是問句，說明寫清楚後果；執行按鈕重述動作：「要登出所有裝置嗎？」→「登出所有裝置」。
- 遮罩 `overlay`、框 `background` 底、`radius-lg`、24px 內距、`shadow-lg`、最大寬 512px。
- 640px 以下按鈕直排、主要動作在上；以上靠右橫排、主要動作在右。
- 動作失敗時在對話框內放一個 destructive Alert，不關閉對話框。
