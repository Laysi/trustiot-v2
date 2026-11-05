# 型號與設備管理畫面的元件測試與檢查（2026-11-05）

對應 issue：Laysi/trustiot-v2#126（型號與設備管理畫面，C1、C2）。這次只驗畫面：元件測試走 HTTP 層的記憶體假服務（`frontend/src/test/fake-devices.ts`，回的 body 與 `api/openapi.json` 產生的型別一致），沒有接真的後端，也沒有閘道器。程式的說明在 `frontend/README.md` 的「型號與設備管理畫面」，與設計稿不同之處在 `docs/design/tenant-admin/design.md` 第 10 節。

## 環境

- 分支 `feat/126-model-device-management`（建立自 main，合併 main 後含這次尚未提交的變更），工具版本以 `.prototools` 為準。
- 前端的檢查在共用建置主機的 arm64 Linux VM 上執行（`rcheck`，每次都是工作目錄的全新複本，`CI=1`）。
- 元件測試：bun test、happy-dom、Testing Library；`DEBUG_PRINT_LIMIT=0`（失敗時不印整份 DOM，否則輸出過大會拖慢測試）。

## 指令

```sh
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
DEBUG_PRINT_LIMIT=0 bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
bun install --cwd scripts --frozen-lockfile && bun test scripts
```

## 結果

| 指令 | 結果 |
| --- | --- |
| `api:check`、`typecheck`、`lint`（Biome）、`build`、`check:dist` | 通過 |
| `test` | 1144 個通過、0 個失敗（65 個檔案）；其中這次新增或改寫的在 `src/pages/models/`、`src/pages/device/DeviceForms.test.tsx`、`DevicesPage.test.tsx`、`AppHeader.test.tsx` |
| `bun test scripts` | 2024 個通過、0 個失敗 |

這次全套 `test` 有一次出現 1 個失敗：`display mode: the port > events the page dispatches itself explain nothing ...`（等 30 秒的事件沒出現），是沙箱頁的計時測試，與這次的變更無關，建置主機同時有別的工作在跑；同一份程式重跑全套 1144 個全過，`src/dashboard` 單獨跑 124 個全過。

測試涵蓋的畫面狀態（每個畫面都有）：一般、空、載入中、載入失敗（可重試）、檢視者（唯讀、寫入網址顯示無權限）、確認對話框、衝突（改版衝突、名稱重複、設備已結束綁定、型號使用中）。

- 設備表單（`DeviceForms.test.tsx`）：新增（含從閘道器帶入）、設定頁、逐欄的後端驗證錯誤（JSON Pointer 對到欄位，錯誤摘要取得焦點）、改版衝突時保留使用者輸入並列出「你的變更」、刪除。
- 型號（`ModelsPage.test.tsx`、`ModelEditPage.test.tsx`、`ModelImportDialog.test.tsx`）：清單與列選單、資料點表單（桌面表格、手機卡片）、依資料類型改變可填欄位、逐筆資料點的錯誤、匯出下載、匯入（瀏覽器端檢查檔案格式與版本、預覽、名稱重複時在對話框改名、服務端逐行錯誤）、刪除（使用中時導向使用它的設備）。
- `/devices` 空狀態（`DevicesPage.test.tsx`）：編輯者看到第 2、3 步的連結（`/models/new`、`/devices/new`），「只能檢視設備」那一行不見了；檢視者沒有連結。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 端到端測試：建立型號與設備，閘道器收到新設定版本 | 待測：需要真的後端與閘道器；這次的假服務不產生設定版本。畫面送出的請求本文由元件測試對照 API 型別驗過 |
| 後端驗證錯誤在畫面上逐欄顯示（測試） | 通過（`DeviceForms.test.tsx`、`ModelEditPage.test.tsx`、`ModelImportDialog.test.tsx`） |
| `/devices` 空狀態第 2、3 步連到建立型號與新增設備，「只能檢視設備」那一行拿掉（測試） | 通過（`DevicesPage.test.tsx`） |
