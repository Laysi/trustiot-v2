# 歷史圖缺區間標籤畫在資料線上層（2026-11-07）

對應 issue：Laysi/trustiot-v2#1186（設備歷史圖上，窄缺區間的標籤被資料線壓過）。

## 環境

- 建置主機（arm64 Linux VM）。測的程式是分支 `fix/1186-narrow-gap-label`：PR Laysi/trustiot-v2#1152 的分支（`a20def08`）併入 PR Laysi/trustiot-v2#1168 的分支（`7b5bb789`，合併 commit `f7779b1c`），再加本次變更。「變更前」的畫面取自 `f7779b1c`（把本次的 `gaps.ts`、`options.ts`、`historyOption.ts` 還原）。
- Bun 與其他工具是 `.prototools` 釘選的版本；`bun install --cwd frontend --frozen-lockfile` 之後啟動 UI 預覽（`bun --bun vite --mode ui-preview`，自己的埠）。沒有後端，API 由預覽的假服務（`frontend/src/test/fake-*.ts`）在頁面裡回答。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像裡的 Chromium，語言 zh-TW、時區 Asia/Taipei、淺色，視窗 853×400 與 1280×720（CSS 像素）。
- ECharts 6.1.0。

## 指令

```sh
# repo 根目錄
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
# 瀏覽器：issue 的重現（chiller-2 的 7 天）與停滯設備 boiler-1 的 24 小時
bash docs/test-reports/data/2026-11-07-1186-gap-label-above-line/run.sh
# 展示劇本步驟 2 的情況：1 小時範圍、每分鐘一筆讀數、最後 5 分鐘沒有讀數
CASE=demo bash docs/test-reports/data/2026-11-07-1186-gap-label-above-line/run.sh
```

[`run.sh`](data/2026-11-07-1186-gap-label-above-line/run.sh) 啟動預覽、在 Playwright 容器裡執行 [`check.mjs`](data/2026-11-07-1186-gap-label-above-line/check.mjs)（切到範圍、等圖畫好、量標籤方塊的寬度、截圖），最後停掉預覽。`CASE=demo` 在執行期間改 `frontend/src/test/fake-devices.ts`（預覽原本每 10 分鐘一筆讀數，缺口的判斷是取樣間隔的 3 倍，所以 1 小時圖上不可能出現 4 分鐘的缺口），結束時還原。腳本每一步印一行 JSON；輸出（變更後與變更前各一次，共 32 行）：[results.jsonl](data/2026-11-07-1186-gap-label-above-line/results.jsonl)，`label` 欄位分 `after` 與 `before`。截圖只給審查的人看，不進 repo。

標籤畫在 canvas 上，DOM 量不到，所以標籤方塊的寬度用同字型（12 px、粗細 600、左右各 7 px 內距與 1 px 邊框）的 `measureText` 算，缺口的寬度從截圖量。

## 結果

### 程式檢查

`api:check`、`typecheck`、`lint`、`build`、`check:dist` 都以 0 結束；`bun run --cwd frontend test`：`All shards: 1966 pass, 0 fail, 1966 tests across 134 files in 8 shards.`；`cargo test -p repo-check`：26 與 11 個測試通過。

### 單元測試

- `gaps.test.ts`：斜線底涵蓋每個缺口且不帶標籤；缺口佔範圍 2.5 % 以上才有標籤、2.4 % 沒有；呼叫端沒給文字的缺口沒有標籤；貼著範圍左右端的標籤靠邊、中間的置中；軸上終點不在起點之後的缺口既不畫也不標；標籤是自己的系列（沒有資料、沒有 tooltip、與資料序列同名）而且 z 高於線（2）與限制線（5）。
- `historyOption.test.ts`（真的 ECharts，SVG renderer 的伺服器端模式）：2 小時範圍、640 px 寬、結尾 5 分鐘（4.2 %）的缺口，標籤在 SVG 裡出現在線的最後一筆筆畫之後（疊在線上），斜線底在線的第一筆筆畫之前（仍在線下）；範圍中間的缺口一樣；圖例隱藏序列時標籤一起消失。
- `options.test.ts`、`DashboardComponent.test.tsx`、`DevicePage.test.tsx`：改為從標籤系列讀標籤（儀表板每個圖仍只標最長的缺口、zh-TW 的措辭、標籤系列與線同名、`z` 高於線）。

### 瀏覽器

標籤方塊的寬度：「5 小時 45 分鐘沒有讀數」144 px、「3 天 23 小時沒有讀數」132 px、展示情況的「4 分鐘 19 秒沒有讀數」約 134 px（從截圖量）。圖（canvas）寬：853 寬的視窗 772 px，1280 寬的視窗 726 px（頁面版面在 1280 寬時給圖的寬度反而較窄），繪圖區比 canvas 窄約 50 px（1280 寬時約 674 px）。

| 情況 | 缺口寬度（約） | 變更前 | 變更後 |
| --- | --- | --- | --- |
| chiller-2、7 天、5 小時 45 分鐘（佔 3.4 %） | 約 24 px | 標籤比缺口寬，往左伸過有資料的地方，線從字上穿過（兩個視窗都是） | 標籤在線的上層，字完整可讀；標籤蓋住它底下的一小段線（峰值） |
| chiller-2、7 天、3 天 23 小時 | 約 390–410 px | 標籤在缺口裡 | 同變更前 |
| boiler-1（停滯）、24 小時、結尾 49 分鐘（佔 3.4 %） | 約 23 px | 標籤靠右邊緣，往左伸到有資料處，線從字上穿過（1280×720 的截圖看得到） | 標籤在線的上層，字完整可讀 |
| 展示：cold-room-1、1 小時、結尾 4 分鐘 19 秒（約 7 %） | 約 52 px | 標籤約 134 px，靠右邊緣往左伸過約 80 px 的讀數 | 同樣的位置，畫在線的上層 |

展示情況的標籤仍比缺口寬（約 134 px 對 52 px）：變更前後都往左伸過最後約 7 分鐘的讀數，變更後它蓋住那段讀數（若線正好在圖的上緣），而不是被線穿過；這次的假資料裡線在圖的下緣，所以兩張截圖看起來一樣。劇本步驟 2 第 4 點指著的標籤仍然在、仍然可讀。

## 沒做與已知限制

- 標籤不量文字寬度（選項建好時不知道圖有多寬），所以相鄰兩個缺口的標籤仍可能互相重疊（設備頁每個至少 2.5 % 的缺口都有標籤）。chiller-2 的兩個缺口不重疊；沒有找到會重疊的預覽資料，沒有另外做。
- 標籤方塊是卡片底色，會蓋住它底下的線；圖的上緣有資料時，會看到線被方塊切掉一小段（chiller-2 的 7 天圖右側的峰值）。issue 的另一個做法（標籤比缺口寬就不畫）沒有採用：展示劇本要指著窄缺口的標籤。
- 手機寬度、深色、放大比例沒有量（與標籤的圖層無關，沒有新增的版面計算）。

## 結論

- 驗收條件 1（窄缺區間的標籤不被線穿過，寬的照舊）：**通過**。chiller-2 的 7 天（853 與 1280 寬）、boiler-1 的 24 小時與展示情況的截圖。
- 驗收條件 2（單元測試涵蓋標籤顯示與不顯示的條件；853 與 1280 寬截圖）：**通過**。
- 驗收條件 3（`api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist` 與 CI）：本機（建置主機）全部通過；CI 待 PR 開立後確認。
