# 設備頁重做的測試報告（2026-11-04）

對應 issue：`Laysi/trustiot-v2#781`（驗收條件 2、3、4）。設計與決定在 `docs/design/device-page/`，前端說明在 `frontend/README.md` 的「設備頁」。

## 環境

- 建置主機：Linux（經 rcheck 佇列執行；本報告的所有指令都在那裡跑，不在工作站跑）。
- 分支 `feat/781-device-page-redesign`，在合併 `origin/main`（`922dda9f`，含 #1066、#1070、#1073）之後測試。
- 工具版本以 `.prototools` 為準（Bun、Biome、TypeScript 由 `frontend/` 的鎖檔決定）。
- 瀏覽器檢查：`mcr.microsoft.com/playwright:v1.64.1-noble@sha256:47b510dd9230bf0b9bfc9b20102241e577ea7a99334ab4c3ca19f19770e9b8ea`（Chromium），以 podman 執行，`--cap-drop ALL --security-opt no-new-privileges`，網路為 host，只掛載 `node_modules`（唯讀）、預覽目錄（唯讀）與輸出目錄。
- 預覽的做法：用 Vite 把真正的 `App`（路由、`DeviceApiProvider`、設備頁全部是產品程式碼）建成一個臨時的 SPA，API 換成記憶體內的假伺服器（`frontend/src/test/` 的 `FakeDeviceServer` 等），再用與產品相同的 CSP 標頭與 Trusted Types（report-only）提供靜態檔，所以頁面的 CSP 行為與產品一致。預覽只存在這次測試中，不進 `frontend/`。
- 時區 `Asia/Taipei`、語言 zh-TW；視窗 1280×900（桌面）與 390×844（手機，有觸控）；淺色與深色各一次。

## 指令

單元與整合檢查（從 repo 根目錄，經 rcheck）：

```
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
bun install --cwd scripts --frozen-lockfile
bun test scripts
```

瀏覽器檢查：把 [`data/2026-11-04-781-device-page-redesign/preview/`](data/2026-11-04-781-device-page-redesign/preview/) 複製到 `frontend/.preview-781/`，從 repo 根目錄執行：

```
bash frontend/.preview-781/run.sh
```

`run.sh` 建預覽、啟動帶 CSP 標頭的伺服器、在 Playwright 映像裡用 `shots.mjs` 逐個狀態截圖並記錄 console、CSP 違規、頁面錯誤與水平溢位，最後把結果檔印出來（tar 加 base64）。跑完刪掉 `frontend/.preview-781/`，否則 `bun run --cwd frontend lint` 會檢查到它。

## 結果

單元與整合檢查（合併 `origin/main` 之後，一次執行，`rcheck: exit 0 at 22:51:24`）：

| 指令 | 結果 |
| --- | --- |
| `api:check` | 通過 |
| `typecheck` | 通過 |
| `lint` | `Checked 245 files`，沒有問題 |
| `test` | 1081 pass，0 fail，5537 expect()（60 個檔案） |
| `build` | 成功 |
| `check:dist` | 通過（輸出的版面與 CSP 規則） |
| `bun test scripts` | 2024 pass，9 skip，0 fail |

瀏覽器檢查（`rcheck: exit 0 at 23:05:47`）：92 張截圖，`with console/CSP output or overflow: 0`。也就是每一張都沒有 console 訊息（error、warning）、沒有 CSP 或 Trusted Types 違規、沒有頁面錯誤、沒有水平溢位（`document.documentElement.scrollWidth - clientWidth` 大於 0 才算溢位，每張都是 -15，即沒有）。每個狀態逐張的結果在 [`results.json`](data/2026-11-04-781-device-page-redesign/results.json)。

涵蓋的狀態（桌面與手機，各淺色與深色）：`normal`、`hover`（桌面：框選縮放、縮放後的統計標「平均（放大範圍）」、tooltip 出現；手機：見下）、`bool`、`text`、`loading`、`loading-history`、`stale`、`never`、`never-older`（按「查看更早的歷史」）、`never-model-failed`、`nopoints`、`outside`、`truncated`、`rangeempty`、`custom`、`custom-edit`（手機，自訂範圍從網址進來，按「修改範圍」開表單）、`error`、`model-failed`、`pageerror`、`notfound`、`long`、`long-more`、`table`；`set-range`（只有桌面）。

互動檢查（腳本內的斷言，輸出在執行日誌）：

| 檢查 | 結果 |
| --- | --- |
| 桌面框選縮放後出現「已放大到…」與「平均（放大範圍）」，並出現 tooltip | `{"zoomed":1,"label":1,"tooltip":1}`（淺色與深色相同） |
| 「把這段設為範圍」把放大的視窗寫進網址的 `from`、`to` 並重新查詢 | 網址帶 `from`、`to`，圖表仍在 |
| 手機的自訂範圍從網址進來時，按「修改範圍」開啟表單 | `{"form":1}` |
| 手機點圖後出現 tooltip | **未觀察到**：`tooltipAfterTap` 與 `tooltipAfterClick` 都是 0 |

手機點圖的 tooltip：Playwright 合成的 tap 與 click 沒有觸發共用圖表元件的 tooltip。同一個元件（`InteractiveChart`）在 #895 以真正的 CDP 觸控事件驗證過；本次沒有重做真實觸控的檢查（見結論的「待測」）。

`never-model-failed` 的預覽畫面顯示的是沒有讀數的檢查清單，不是型號載入失敗：預覽用的 QueryClient 與產品一樣會重試，第一次失敗的型號請求在截圖前已重試成功。「型號失敗且沒有讀數時，保留錯誤與重試，不顯示清單也不畫圖」由 `DevicePage.test.tsx` 的單元測試涵蓋（重試關閉）。

## 結論

逐條對照 `Laysi/trustiot-v2#781` 的驗收條件：

- 條件 2（照設計稿做，每個狀態在 390 與 1280 寬各有截圖）：通過。所有狀態都有截圖（淺色；深色也有）。與設計稿不同的地方（共用圖表的顏色 `--chart-1`、縮放的介面、不做固定的手機讀值列、缺區間標籤的門檻、文字資料點的時段表、沒有「查看閘道器」與「問 AI」、`never` 狀態不預先查歷史）都記在 `docs/design/device-page/design.md` 的「實作與設計稿不同之處（2026-11-04）」。
- 條件 3（正式 CSP 加 Trusted Types report-only 下沒有 console 與 CSP 違規）：通過，92 張截圖零違規。
- 條件 4（`typecheck`、`lint`、`test`、`build`、`check:dist` 通過）：通過，見上表。

待測（不是略過）：

- 手機上點圖出現 tooltip，用真實觸控事件：預覽的合成事件沒有觸發。共用元件在 #895 已用真實觸控驗證過，這次的差別只在設備頁用它的方式，風險低；真正的觸控檢查需要手機或模擬觸控的瀏覽器工作階段，另開最低優先級的 issue 補驗。
- 瀏覽器檢查目前是手動執行的腳本，沒有進 CI：把它自動化屬於進階驗證，延後。
- 其他瀏覽器（Firefox、Safari）與實機：沒有測，屬於跨平台的重複驗證，延後。
