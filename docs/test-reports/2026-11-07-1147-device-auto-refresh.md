# 設備頁自動更新的瀏覽器實測（Laysi/trustiot-v2#1147）

2026-11-07。驗收條件「在瀏覽器實測一次：開著頁面時新的讀數自己出現；停止送值後『資料停滯』與圖右側的缺口自己出現；恢復後自己消失」的證據。單元測試（假計時器、請求數、暫停規則、失敗保留資料）在 `frontend/src/pages/device/DevicePage.test.tsx` 的 `DevicePage automatic refresh`，由 `bun run --cwd frontend test` 執行。

## 環境

- 建置主機（arm64 Linux 虛擬機）；`.prototools` 釘住的 Bun；`frontend/e2e/image.ts` 釘住的 Playwright 映像（Chromium 在容器裡），分支 `feat/1147-device-auto-refresh`。
- 頁面是 UI 預覽（`bun run --cwd frontend ui-preview`，設備的假服務在頁面裡回答，沒有碰展示環境或任何真的服務）。預覽的假服務原本只有每小時一筆的靜態讀數，所以腳本在頁面裡用 `patch.ts` 在主機上的複本暴露假服務，替設備灌每分鐘一筆的讀數，並在實測的各階段加讀數或停止加讀數；這個暴露只在主機上的複本，不在 repo 的程式碼裡。
- 頁面的時鐘用 Playwright 的 `page.clock`：`install` 之後以 `runFor(60 秒)` 前進，所以「一分鐘」是頁面時間，不是等一分鐘。

## 指令

從 repo 根目錄的複本執行（`run.sh.txt` 啟動預覽的開發伺服器、套用 `patch.ts.txt`、在映像裡跑 `e2e.mjs.txt`）：

```bash
bun .e2e-1147-src/patch.ts           # 暴露預覽的設備假服務（只在複本）
bun --bun vite --mode ui-preview --host 127.0.0.1 --port 5174 &
podman run --rm --network host ... "$PLAYWRIGHT_IMAGE" node e2e.mjs
```

三個檔案放在 [`data/2026-11-07-1147-device-auto-refresh/`](data/2026-11-07-1147-device-auto-refresh/)：`run.sh.txt`、`patch.ts.txt`、`e2e.mjs.txt`，輸出是 `browser-output.txt`。

## 結果

腳本只在設定時點了「1 小時」與「Refresh」各一次（讓頁面載入灌入的讀數、並讓缺口在 1 小時的尺度看得到），之後沒有點擊、沒有重新載入、網址沒有變，只前進頁面的時鐘。

| 階段 | 頁面自己做到的事 |
| --- | --- |
| 設定完成 | 「Data is coming in · 53 seconds ago」，目前讀數 4.10 |
| 一筆新讀數存進去，頁面時鐘前進 1 分鐘 | 目前讀數自己變成 9.90；「Updated」時間往前一分鐘；這一分鐘送出 4 個請求（設備、型號、`latest`、`series` 各一個）；「Refresh」鈕沒有被停用 |
| 讀數停止，再前進 4 分鐘 | 頁首標籤自己變成「Data stalled · last reading 4 minutes ago」，出現「No new readings for 4 minutes 49 seconds」橫幅；圖的右側出現一段斜線底、標著「4 minutes 21 seconds without readings」的缺口 |
| 閘道器補送讀數，再前進 1 分鐘 | 標籤自己變回「Data is coming in · 46 seconds ago」，橫幅消失，圖的缺口消失（「No gaps」），補送的讀數畫在圖上 |

腳本的檢查全部通過（9 項 ok，主控台與頁面錯誤 0 筆，見 `browser-output.txt`）；停滯與恢復兩個階段的圖我看過截圖，沒有放進 repo。

## 結論

- 開著頁面時新的讀數自己出現：通過。
- 停止送值後「資料停滯」與圖右側的缺口自己出現：通過。
- 恢復後兩者自己消失：通過。
- 沒測：真的閘道器與真的 API（只用 UI 預覽的假服務，依 issue 的規定不碰展示環境）；ECharts 在背景分頁的行為與長時間（數小時）開著頁面的記憶體，屬進階驗證，不在這一次。
