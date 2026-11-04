# Button

觸發動作的按鈕，六種變體、四種尺寸；預設 `type="button"`，送出表單的按鈕要明寫 `type="submit"`。

手寫自 `frontend/src/components/ui/button.tsx`（`buttonVariants`）。預覽是靜態重現，數值與 Tailwind class 一致。

## 使用時機

- `default`（`primary` 填色）：一個區塊裡最主要的那一個動作，例如篩選列的「顯示」。同一區塊只放一個。
- `outline`：次要動作，例如「更新資料」；破壞性的觸發按鈕可以是 outline 加 `destructive` 文字色（設定頁的「登出所有裝置」）。
- `secondary`、`ghost`：低調的動作；ghost 用在工具列與選單觸發（圖表的「重設縮放」「資料來源」）。
- `destructive`：確認對話框裡真正執行破壞性動作的按鈕。
- `link`：行內的文字動作，例如「顯示最近 7 天」。

## 使用者提供

- `variant`：`default | destructive | outline | secondary | ghost | link`；`size`：`default`（h-9）、`sm`（h-8）、`lg`（h-10）、`icon`（36 × 36）。
- `children`：文字，可以前置一個 lucide 圖示（自動 16px）；只有圖示時一定要 `aria-label`。
- `asChild`：把唯一的子元素（連結）套成按鈕樣式。

## 規則

- 焦點是 2px `ring` outline、offset 2px（`focusRing`），強制色彩模式下仍看得到；不要改回 box-shadow。
- 停用時 `opacity-disabled`、游標維持箭頭；進行中把圖示換成 Spinner、文字改成「登出中…」。
- 不自己寫 `cursor-*`，手形游標由 `global.css` 統一處理。
