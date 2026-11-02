# 固定的 framework chunk：B1 與「B1 加 framework chunk」的載入量測（2026-11-03）

Laysi/trustiot-v2#977。Laysi/trustiot-v2#948 的報告（[2026-11-02-948-bundle-size-load-time.md](2026-11-02-948-bundle-size-load-time.md)）量到的 B2C 方案（把很少變動的套件固定成一個 `framework` chunk）建在 B2 上，不是最後採用的 B1（只切出設定頁與設備頁）。這份報告在 B1 的建置上再量一次，並依事先訂的規則決定是否採用。

## 決定

**採用。** 部署新版後再訪的 Brotli 下載量降 65% 至 67%（204 至 210 KB 降到 68 至 73 KB），所有冷快取與熱快取的格子都在容許範圍內，沒有一格變慢。理由與數字見「結果」。

## 決定規則（量測前訂的，2026-11-03）

採用的條件，兩個都要成立：

1. 部署新版後再訪的下載量（入口 `br`，傳輸的位元組）降低至少 30%。
2. 冷快取與熱快取的 ready 中位數，沒有任何一格比 B1 慢超過「該格的 run-to-run 差距」或「B1 中位數的 5%」，取較大者。差距取兩個建置各自 7 次的（最大 − 最小）中較大的。

不成立就不採用。判定由 `docs/test-reports/data/2026-11-03-977-framework-chunk/compare.ts` 算出（輸出存在同目錄的 `compare-output.txt`）。

## 環境

- **程式碼**：main 的 `0679ccbc`（B1 已在其中）。「B1 加 framework chunk」是同一份原始碼加上這個變更的 `vite.config.ts`。
- **機器**：建置主機（x86_64 Linux），工具版本是 `.prototools` 釘選的版本（Bun 1.4.2；建置 Vite 8.3.4，即 Rolldown）。兩個建置在同一個工作內依序量（`b1` 與 `fw` 輪流，每種入口各一輪），所以機器的負載相近。服務與瀏覽器都在同一台主機上，經 loopback 連線。
- **服務、瀏覽器、入口替代、網路節流、量測單位**：與 Laysi/trustiot-v2#948 報告「環境」一節相同（`services/web` 的 release 執行檔、headless Chromium 131.0.6778.33 於 `mcr.microsoft.com/playwright:v1.49.1-noble`、`measure-load.ts` 的入口替代與 CDP 節流、每格 7 次、每次一個新的瀏覽器 context）。
- **格子**：入口 `none` 與 `br`（Brotli 品質 5）；網路 Fast 4G 與 Slow 4G；頁面 `login`、`home`、`settings`；快取冷、熱、部署新版後再訪；每格 7 次，表中是中位數。
- **「部署新版」的建置**：只在 `src/i18n/locales/en.json` 把 `page.home.body` 結尾的句點改成驚嘆號（一個只動應用程式碼的小改動）。
- **日期**：2026-11-03。

### 與 Laysi/trustiot-v2#948 報告相同的限制

單一瀏覽器、桌面視窗、CPU 不節流；不含 TCP 與 TLS 握手、封包遺失。Slow 4G 下的 ready 時間主要由下載量決定，所以冷快取的格子對「多兩個請求」的代價最敏感。

## 建置內容

| 建置 | 檔案（未壓縮位元組） |
| --- | --- |
| B1 | 主 bundle 699,193；設定頁 31,421；設備頁 14,003；CSS 39,504 |
| B1 加 framework | `framework` 477,297；主 bundle 221,671；設定頁 31,467；設備頁 14,057；preload helper 599；CSS 39,504 |

`framework` chunk 放進這些套件（`vite.config.ts` 的 `FRAMEWORK_PACKAGES`）：`react`、`react-dom`、`scheduler`、`react-router`、`@tanstack/react-query`、`@tanstack/query-core`、`zod`、`openapi-fetch`、`tailwind-merge`。理由：它們合計是主 bundle 的大部分（react-dom、react-router、zod、query-core、tailwind-merge 依 Rolldown 的模組大小排前五），而且只在升級相依套件時才變，不隨應用程式碼變。刻意不放進去的：`lucide-react`（內容隨用到的圖示變）與 Radix 的套件（跟著用它們的元件變）；放進去會讓 chunk 的內容雜湊在加一個圖示時就換掉，失去固定的意義。

chunk 名稱：`output.chunkFileNames` 是 `static/[hash].js`（只含內容雜湊，見 `vite.config.ts` 的註解），所以名稱隨內容。只改應用程式碼時 framework chunk 的內容不變、名稱就不變：兩次只差一句文字的建置裡，`framework` 都是 `Dtj7_1q3.js`，其他檔案的名稱都換了（`frontend/src/app/framework-chunk.test.ts` 把這件事做成測試）。

## 指令

在 repo 根目錄，在建置主機上同一個工作內做完（量測腳本只停自己啟動的程序）。`<輸出目錄>` 與 `<site>` 都在 repo 外。

```sh
# 1. 四個建置：B1 與「B1 加 framework」各一個舊版與一個只改一句應用程式碼的新版。
#    B1 用 origin/main 的 vite.config.ts（vite build --config），framework 用這個變更的 vite.config.ts。
bun install --cwd frontend --frozen-lockfile
bun --bun vite build --config <B1 的 vite.config.ts> --outDir <輸出目錄>/builds/b1-old   # 在 frontend/ 下
bun --bun vite build --config vite.config.ts           --outDir <輸出目錄>/builds/fw-old   # 在 frontend/ 下
# en.json 的 page.home.body 結尾句點改成驚嘆號後，各再建一次到 b1-new 與 fw-new，之後還原 en.json

# 2. 服務
cargo build --release --locked -p web

# 3. 量測：入口 none 與 br 各一輪，每輪 b1 與 fw 依序
for compress in none br; do
  for v in b1 fw; do
    bun frontend/tools/measure-load.ts \
      --site <輸出目錄>/builds/$v-old --upgrade-site <輸出目錄>/builds/$v-new \
      --web target/release/web --out <輸出目錄>/out/$v-$compress \
      --runs 7 --profiles fast4g,slow4g --compress $compress -- \
      podman run --rm --init --network host mcr.microsoft.com/playwright:v1.49.1-noble \
      /ms-playwright/chromium-1148/chrome-linux/chrome \
      --headless --no-sandbox --disable-gpu --disable-dev-shm-usage --user-data-dir=/tmp/profile
  done
done

# 4. 判定
bun docs/test-reports/data/2026-11-03-977-framework-chunk/compare.ts
```

## 結果

原始資料：[`data/2026-11-03-977-framework-chunk/`](data/2026-11-03-977-framework-chunk/)（`b1-none`、`fw-none`、`b1-br`、`fw-br` 各有 `results.json` 與 `summary.md`；判定輸出是 `compare-output.txt`）。

### 部署新版後再訪（規則 1）

下載量是傳輸的位元組中位數（Fast 4G 與 Slow 4G 相同，只列 Fast 4G 的下載量）；ready 是再訪的 ready 中位數。

| 入口 | 頁面 | 網路 | B1 下載 KB | 加 framework 下載 KB | 降幅 | B1 再訪 ready ms | 加 framework 再訪 ready ms |
| --- | --- | --- | --- | --- | --- | --- | --- |
| none | login | fast4g | 703.4 | 225.9 | 68% | 1021 | 593 |
| none | login | slow4g | 703.3 | 225.9 | 68% | 3862 | 1467 |
| none | home | fast4g | 703.8 | 226.4 | 68% | 1207 | 776 |
| none | home | slow4g | 703.8 | 226.4 | 68% | 4024 | 1632 |
| none | settings | fast4g | 718.7 | 241.3 | 66% | 1218 | 791 |
| none | settings | slow4g | 718.7 | 241.3 | 66% | 4104 | 1707 |
| br | login | fast4g | 204.4 | 67.7 | 67% | 578 | 450 |
| br | login | slow4g | 204.4 | 67.7 | 67% | 1364 | 679 |
| br | home | fast4g | 204.9 | 68.2 | 67% | 762 | 634 |
| br | home | slow4g | 204.9 | 68.2 | 67% | 1528 | 850 |
| br | settings | fast4g | 209.9 | 73.3 | 65% | 769 | 641 |
| br | settings | slow4g | 209.9 | 73.3 | 65% | 1570 | 875 |

`br` 的最小降幅是 65.1%，高於門檻 30%。規則 1 成立。這與 Laysi/trustiot-v2#948 報告 B2C 的 39 至 73 KB 一致（在 B1 上，登入頁多帶了設定頁以外的應用程式碼，所以下限是 68 KB）。

### 冷快取與熱快取（規則 2）

ready 毫秒，中位數（最小-最大）。「容許」是 max（兩邊較大的 run-to-run 差距, B1 中位數的 5%）；「差」是加 framework 減 B1。

| 入口 | 頁面 | 網路 | 快取 | B1 | B1 加 framework | 差 | 容許 | 判定 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| none | login | fast4g | cold | 1055 (1055-1071) | 1056 (1055-1067) | +1 | 53 | 通過 |
| none | login | fast4g | warm | 239 (237-243) | 237 (236-239) | -2 | 12 | 通過 |
| none | login | slow4g | cold | 4062 (4060-4068) | 4075 (4072-4076) | +13 | 203 | 通過 |
| none | login | slow4g | warm | 219 (217-226) | 218 (213-225) | 0 | 11 | 通過 |
| none | home | fast4g | cold | 1240 (1237-1245) | 1240 (1237-1247) | +0 | 62 | 通過 |
| none | home | fast4g | warm | 424 (422-430) | 421 (420-423) | -3 | 21 | 通過 |
| none | home | slow4g | cold | 4231 (4222-4232) | 4239 (4238-4247) | +8 | 212 | 通過 |
| none | home | slow4g | warm | 387 (380-394) | 380 (379-394) | -7 | 19 | 通過 |
| none | settings | fast4g | cold | 1248 (1246-1251) | 1248 (1246-1255) | +1 | 62 | 通過 |
| none | settings | fast4g | warm | 434 (430-438) | 431 (424-435) | -3 | 22 | 通過 |
| none | settings | slow4g | cold | 4314 (4313-4314) | 4328 (4327-4337) | +14 | 216 | 通過 |
| none | settings | slow4g | warm | 394 (393-402) | 392 (386-394) | -2 | 20 | 通過 |
| br | login | fast4g | cold | 583 (583-593) | 586 (585-597) | +3 | 29 | 通過 |
| br | login | fast4g | warm | 239 (238-241) | 236 (235-238) | -2 | 12 | 通過 |
| br | login | slow4g | cold | 1413 (1412-1414) | 1433 (1432-1434) | +20 | 71 | 通過 |
| br | login | slow4g | warm | 219 (217-226) | 215 (214-222) | -4 | 11 | 通過 |
| br | home | fast4g | cold | 768 (765-773) | 767 (764-771) | -1 | 38 | 通過 |
| br | home | fast4g | warm | 425 (424-433) | 419 (416-423) | -7 | 21 | 通過 |
| br | home | slow4g | cold | 1577 (1576-1584) | 1598 (1595-1607) | +22 | 79 | 通過 |
| br | home | slow4g | warm | 388 (380-393) | 380 (379-387) | -8 | 19 | 通過 |
| br | settings | fast4g | cold | 777 (773-779) | 774 (771-779) | -2 | 39 | 通過 |
| br | settings | fast4g | warm | 434 (429-438) | 429 (425-433) | -5 | 22 | 通過 |
| br | settings | slow4g | cold | 1608 (1607-1611) | 1630 (1630-1631) | +22 | 80 | 通過 |
| br | settings | slow4g | warm | 399 (395-401) | 394 (386-399) | -5 | 20 | 通過 |

24 格中沒有一格慢於容許。唯一看得出的代價在 Slow 4G 冷快取：多兩個請求（登入頁 6 對 8，設定頁 8 對 10）、`br` 下多傳約 2.7 至 2.8 KB（設定頁 219.9 對 222.7 KB），ready 慢 8 至 22 ms（最多約 1.4%，低於 5% 的容許）。熱快取沒有變慢，多數格略快（最多 8 ms，在容許內，不算結論）。

## 結論

- 規則 1（再訪下載量降至少 30%）：成立，`br` 降 65% 至 67%。
- 規則 2（冷、熱快取沒有格子慢過容許）：成立，24 格皆通過。
- 決定：**採用**。`vite.config.ts` 加 `codeSplitting` 的 `framework` 群組（套件與理由見「建置內容」）；`frontend/src/app/framework-chunk.test.ts` 證明只改應用程式碼的兩次建置，`framework` chunk 的檔名相同；`preload-chunks.test.ts` 檢查 `framework` chunk 以靜態 import 載入、列在 `index.html` 的 `modulepreload`，不經動態 import；`src/app/preload-error.ts` 沒有改動（`vite:preloadError` 的處理照常，動態 import 的 chunk 仍全部經過 preload helper）。

## 未量的項目

- 低階手機的解析與執行時間（`--cpu-slowdown`）：與 Laysi/trustiot-v2#948 相同，沒量；framework chunk 讓 JavaScript 的總量不變，只換分法，預期不影響，但沒有數字。
- 部署新版時，framework chunk 因升級相依套件而換名的那一次，下載量與 B1 相同（整個 framework 要重抓）；只改應用程式碼的部署才有降幅。
