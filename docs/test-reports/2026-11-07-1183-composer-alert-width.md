# 輸入框上方「已停止」與送出失敗提示的寬度（2026-11-07）

對應 issue：Laysi/trustiot-v2#1183（對話頁輸入框上方的兩個提示，文字被放進寬 0 的欄，一字一行，黏在底部的輸入框區塊長到 528 px）。

## 環境

- 建置主機（arm64 Linux VM）。測的程式是分支 `fix/1183-composer-alert-width`，基底 main `bcadc8e2`；「修正前」是同一份檢出把 `Composer.tsx` 兩個 `Alert` 的 `className="grid-cols-[1fr]"` 拿掉，「修正後」是原樣（`before-after.sh` 在一次執行裡先後做這兩件事）。
- Bun 與其他工具是 `.prototools` 釘選的版本；`bun install --cwd frontend --frozen-lockfile` 之後啟動 UI 預覽（`vite --mode ui-preview`，自己的埠 5193，埠已有回應時拒絕執行）。沒有後端，API 由預覽的假服務回答。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像裡的 Chromium，podman 參數與 Laysi/trustiot-v2#1179 的走查相同。語言 zh-TW、時區 Asia/Taipei，CSS 視窗 1280×720 與 853×400，放大 100%。
- 與 #1179 的走查不同：這份量測沒有隱藏預覽才畫的東西（預覽工具列、輸入框的補全提示與範本按鈕），所以輸入框區塊的絕對高度包含它們；修正前後用同一份預覽，差值才是修正的效果。

## 指令

```sh
# repo 根目錄；檢出是拋棄式的，因為腳本會改 Composer.tsx 再還原
bun install --cwd frontend --frozen-lockfile
bash docs/test-reports/data/2026-11-07-1183-composer-alert-width/before-after.sh
```

腳本：[run.sh](data/2026-11-07-1183-composer-alert-width/run.sh)、[browser-check.mjs](data/2026-11-07-1183-composer-alert-width/browser-check.mjs)、[before-after.sh](data/2026-11-07-1183-composer-alert-width/before-after.sh)。流程：從首頁問一個問題進入對話；在對話送出加了 `!slow` 的問題並按「停止」，量「已停止」提示；預覽工具列「Fail next request」選 503 後再送一個問題，量送出失敗提示。

## 結果

原始輸出：[output.jsonl](data/2026-11-07-1183-composer-alert-width/output.jsonl)。兩個視窗大小的數字相同（提示寬度由內容欄決定）。

| 提示 | 狀態 | 提示框（寬×高） | 文字欄寬 | 文字行數 | Grid 欄 | 輸入框區塊高 |
| --- | --- | --- | --- | --- | --- | --- |
| 已停止 | 修正前 | 768×390 | 0 px | 16 | `0px 734px` | 528 px |
| 已停止 | 修正後 | 768×49 | 734 px | 1 | `734px` | 187 px |
| 送出失敗 | 修正前 | 768×458 | 0 px | 19 | `0px 734px` | 596 px |
| 送出失敗 | 修正後 | 768×49 | 734 px | 1 | `734px` | 187 px |

修正前文字欄寬 0，文字溢出到欄外，行數是以段落高度除以行高算出的；issue 記載的 28 px 寬是走查時另一個量法，結論相同。

## 結論

- 「已停止」與送出失敗提示佔滿輸入框的寬度，1280×720 與 853×400 都是一行（不超過兩行）：通過。
- 修正前會失敗的測試：`frontend/src/components/ui/alert-columns.test.ts`，修正前列出 `chat/Composer.tsx` 兩處，修正後通過：通過。
- UI 預覽的瀏覽器實測：本報告，通過。
- `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist` 與 CI：見 PR。
