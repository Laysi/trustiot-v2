# 登入與設定畫面的 bundle 大小、載入時間與程式碼切分（2026-11-02）

Laysi/trustiot-v2#948，來自 Laysi/trustiot-v2#675 的第 5 項，起因是 Laysi/trustiot-v2#124：登入與個人設定畫面讓 SPA 的 JavaScript 超過 Vite 的 500 kB 提醒，之後的功能還會再加。這份報告記下建置輸出的大小、經 `services/web` 在節流網路下的載入時間、是否以路由切分程式碼的決定與理由，以及切分後的重新量測。

## 環境

- **程式碼**：基底是 main 的 `d2e47c2f`（量測「變更前」的建置，2026-11-01）；「變更後」是這個 PR 的建置（2026-11-02）。選項比較（2026-11-01）用的各方案建置見「選項與決定」。
- **機器**：建置主機（x86_64 Linux），Ubuntu 26.04，工具版本是 `.prototools` 釘選的版本（Bun 1.4.2；建置 Vite 8.3.4，即 Rolldown）。服務與瀏覽器都在同一台主機上，經 loopback 連線。
- **服務**：`cargo build --release --locked -p web` 的執行檔，吃 `frontend/dist/`，所以是 `services/web` 的真實標頭與快取政策（見「標頭」）。
- **瀏覽器**：headless Chromium 131.0.6778.33，來自容器映像 `mcr.microsoft.com/playwright:v1.49.1-noble`（x86_64，manifest digest `sha256:ad57c625d284e8d287abcd40d18434582b2e354de71c8428cb080112b0c45960`），以 rootless podman 的 `--network host` 執行；`--no-sandbox` 與 `--disable-dev-shm-usage` 是容器內必要的（沒有後者，連續開了二十幾個頁面後渲染程序會崩潰，量測腳本會偵測到並報錯）。視窗 1280x800，CPU 不節流。
- **入口的替代**：量測腳本（`frontend/tools/measure-load.ts`）內的小型 HTTP 伺服器：`/api`、`/auth` 回罐頭答案（已登入的身分服務：session 與偏好設定；未登入：401），沒有延遲；其他請求原樣轉給 `services/web`，頁面看到的是它自己的標頭。可選擇像有壓縮的入口那樣壓縮文字檔（`--compress gzip|br`，gzip 等級 6、Brotli 品質 5）。
- **網路節流**：CDP 的 `Network.emulateNetworkConditions`。`fast4g` 是 WebPageTest 的「4G」（下載 9 Mbit/s、上傳 1.5 Mbit/s、往返 170 ms）；`slow4g` 是 Lighthouse 的「slow 4G」（下載 1.6 Mbit/s、上傳 750 kbit/s、往返 150 ms）；`none` 不節流。
- **量測單位**：每個「頁面 x 網路 x 入口」重複 7 次（選項比較 5 次），每次一個新的瀏覽器 context（空快取、沒有 cookie）。冷快取是該 context 的第一次導覽；熱快取是在同一個 context 再導覽一次同一個網址（`/static/` 的檔案來自快取，`index.html` 與 `favicon.svg` 因為 `no-cache` 而重新驗證，Chrome 把 304 回報成 200，傳輸約 800 位元組）；「部署新版後再訪」是熱快取之後，入口改送另一個建置（只改了一句首頁文字）再導覽一次。表中是中位數。
- **頁面與 ready 的定義**：`login`（`/login`，未登入）、`home`（`/`，已登入）、`settings`（`/settings`，已登入）。ready 是頁面第一次出現使用者要看的東西：登入頁的 email 欄位、首頁與設定頁的 `h1`。`load`（load 事件）、FCP、LCP 的值都在原始資料。
- **日期**：2026-11-01（變更前、選項比較）、2026-11-02（變更後、舊 chunk 檢查）。

### 量測方法的限制

- `Network.emulateNetworkConditions` 的延遲加在子資源與 API 請求上，不加在文件請求（導覽的 `responseStart` 約 1 ms）；腳本在頁面載入後以三個不走快取的小請求量出「探測 RTT」（原始資料的 `probeMs`）：`fast4g` 約 174 ms、`slow4g` 約 157 ms，確認節流生效。所以每次載入大約少算了文件請求的一次往返，變更前後相同，比較不受影響，絕對值偏樂觀。
- 不模擬 TCP 與 TLS 的握手、封包遺失；瀏覽器到入口替代是 HTTP/1.1。正式環境經 Istio 與 Cloudflare，可能是 HTTP/2 或 HTTP/3；這些頁面的檔案只有個位數，連線數不是瓶頸。
- 入口替代的 API 沒有延遲，所以首頁與設定頁的 ready 比 FCP 晚的部分（`session` 與 `preferences` 兩次依序的往返，約 150 至 170 ms 各一）是頁面自己的 API 瀑布，與 bundle 無關。
- 單一瀏覽器（Chromium）、桌面視窗、CPU 不節流：低階手機的解析與執行時間不在這些數字裡（`--cpu-slowdown` 可以加，這次沒量）。

## 指令

在 repo 根目錄。第 1、2 步在建置主機上同一個工作內做完（啟動、量測、停止都由腳本負責，腳本只停自己啟動的程序）：

```sh
# 1. 服務與前端
cargo build --release --locked -p web
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend build

# 2. 量測：每種入口（none、gzip、br）各一次；--upgrade-site 是「部署新版」的另一個建置
for compress in none gzip br; do
  bun frontend/tools/measure-load.ts \
    --site frontend/dist --web target/release/web --out <輸出目錄>/$compress \
    --runs 7 --compress $compress [--upgrade-site <另一個建置的目錄>] -- \
    podman run --rm --init --network host mcr.microsoft.com/playwright:v1.49.1-noble \
    /ms-playwright/chromium-1148/chrome-linux/chrome \
    --headless --no-sandbox --disable-gpu --disable-dev-shm-usage --user-data-dir=/tmp/profile
done

# 3. 舊分頁碰到被移除的 chunk（真實瀏覽器）
bun frontend/tools/measure-load.ts --site frontend/dist --web target/release/web \
  --out <輸出目錄> --stale-chunk -- <同樣的瀏覽器指令>
```

「變更前」是在基底 `d2e47c2f` 的原始碼上跑同樣的指令（不帶 `--upgrade-site`）。另一個建置用 `bun --bun vite build --outDir <目錄>`，在 `src/i18n/locales/en.json` 把 `page.home.body` 結尾的句點改成驚嘆號後建置（一個典型的小改動，所有 chunk 的內容雜湊都會跟著換）。各選項（A、B1、B2、B2C）是同一份原始碼加上對應的變更後各自建置。

檢查：

```sh
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
```

## 結果

原始資料在 [data/2026-11-02-948-bundle-size-load-time/](data/2026-11-02-948-bundle-size-load-time/)：`baseline-<入口>.json`（變更前）、`after-<入口>.json`（變更後）、`variants-br-<方案>.json`（選項比較）、`stale-chunk.json`、`entry-composition-before.txt`。

### 建置輸出（`bun run --cwd frontend build`）

變更前（`d2e47c2f`，Vite 的 gzip 是等級 9）：

| 檔案 | 大小 | gzip |
| --- | --- | --- |
| `index.html` | 0.58 kB | 0.36 kB |
| 樣式表 | 36.51 kB | 7.71 kB |
| 唯一的 JS | 718.49 kB | 219.71 kB |

變更後：

| 檔案 | 大小 | gzip | 內容 |
| --- | --- | --- | --- |
| `index.html` | 0.58 kB | 0.36 kB | |
| 樣式表 | 36.51 kB | 7.71 kB | |
| 主 bundle | 674.20 kB | 208.01 kB | 每個訪客都載入 |
| 設定頁的 chunk | 14.00 kB | 4.31 kB | 第一次進 `/settings` 時載入 |
| 設備頁的 chunk | 31.42 kB | 10.10 kB | 第一次進 `/devices/<id>` 時載入 |
| 圖表套件（ECharts）的 chunk | 還不能量 | | Laysi/trustiot-v2#895 還沒進 main |

建置仍會印出「chunk 超過 500 kB」的提醒（主 bundle 674 kB）。主 bundle 的成分（變更前，以 source map 把每個位元組歸給原始檔，見 `entry-composition-before.txt`）：

| 套件或目錄 | 約佔 |
| --- | --- |
| react-dom | 29.4%（207 kB） |
| react-router | 13.4%（95 kB） |
| zod（jitless） | 13.0%（92 kB） |
| 應用程式本身（頁面、版面、session、API） | 約 17%（設備頁 28.5 kB、其他頁面 13.6 kB、session 8.4 kB、`components/ui` 8.0 kB 等） |
| @tanstack/query-core | 4.6% |
| tailwind-merge | 3.9% |
| 其餘（Radix、floating-ui、lucide、openapi-fetch 等） | 約 10% |

框架與函式庫約 650 kB，是每個頁面都要的下限；路由切分最多只能切走頁面自己的程式碼。

### 標頭（`services/web` 的真實回應）

冷快取登入頁的實際回應（`baseline-none.json` 的第一筆）：`/login` 是 `Cache-Control: no-cache` 加 ETag；`/static/*.js` 與 `/static/*.css` 是 `public, max-age=31536000, immutable`；每個回應都帶 `services/web` README 規定的 CSP。熱快取時 `/static/` 的檔案不再發請求（表中「requests」括號內是來自快取的數量），只剩 `index.html`、圖示與 API。

### 沒有壓縮：`services/web` 送出未壓縮的 JavaScript

`services/web` 與 chart 沒有任何壓縮設定（repo 內搜尋 `compress`、`gzip`、`brotli`、`content-encoding` 都沒有相關設定），所以直連時使用者下載 718 kB 的 JavaScript（含標頭 762 kB）。入口或 CDN 壓縮後只有 219 kB。這比任何程式碼切分都大：

| 變更前，登入頁，冷快取 | 傳輸 KB | fast4g ready ms | slow4g ready ms |
| --- | --- | --- | --- |
| 沒有壓縮（直連 `services/web`） | 762.4 | 1068 | 4143 |
| gzip（等級 6） | 233.6 | 600 | 1511 |
| Brotli（品質 5） | 218.6 | 587 | 1435 |

SaaS 前面的 Cloudflare 會壓縮文字檔，但地端部署（客戶自己的入口）不一定。這一點需要決定，見「未決事項」。

### 載入時間

以下的表格由 `results.json` 的中位數產生；ready 的定義與限制見「環境」。

#### 第一次進入（冷快取）：頁面可用的時間，變更前後

| 入口 | 頁面 | 網路 | 變更前 ready ms | 變更後 ready ms | 差 | 變更前 KB | 變更後 KB |
| --- | --- | --- | --- | --- | --- | --- | --- |
| none | login | fast4g | 1068 | 1029 | -39 (-3.6%) | 762.4 | 718.1 |
| none | login | slow4g | 4143 | 3925 | -218 (-5.3%) | 762.4 | 718.1 |
| none | home | fast4g | 1252 | 1211 | -41 (-3.3%) | 762.9 | 718.6 |
| none | home | slow4g | 4314 | 4089 | -225 (-5.2%) | 762.9 | 718.6 |
| none | settings | fast4g | 1261 | 1220 | -42 (-3.3%) | 762.9 | 733.5 |
| none | settings | slow4g | 4321 | 4177 | -144 (-3.3%) | 762.9 | 733.5 |
| gzip | login | fast4g | 600 | 587 | -13 (-2.1%) | 233.6 | 221.9 |
| gzip | login | slow4g | 1511 | 1448 | -63 (-4.2%) | 233.6 | 221.9 |
| gzip | home | fast4g | 785 | 771 | -13 (-1.7%) | 234.1 | 222.4 |
| gzip | home | slow4g | 1674 | 1613 | -60 (-3.6%) | 234.1 | 222.4 |
| gzip | settings | fast4g | 792 | 778 | -14 (-1.8%) | 234.1 | 227.6 |
| gzip | settings | slow4g | 1684 | 1646 | -39 (-2.3%) | 234.1 | 227.6 |
| br | login | fast4g | 587 | 576 | -10 (-1.7%) | 218.6 | 208.1 |
| br | login | slow4g | 1435 | 1380 | -55 (-3.8%) | 218.6 | 208.1 |
| br | home | fast4g | 771 | 755 | -15 (-2.0%) | 219.1 | 208.5 |
| br | home | slow4g | 1599 | 1546 | -53 (-3.3%) | 219.1 | 208.5 |
| br | settings | fast4g | 779 | 765 | -14 (-1.8%) | 219.1 | 213.6 |
| br | settings | slow4g | 1608 | 1579 | -30 (-1.8%) | 219.1 | 213.6 |

#### 再訪（熱快取）：頁面可用的時間，變更前後

| 入口 | 頁面 | 網路 | 變更前 ready ms | 變更後 ready ms |
| --- | --- | --- | --- | --- |
| none | login | fast4g | 235 | 236 |
| none | login | slow4g | 216 | 219 |
| none | home | fast4g | 420 | 420 |
| none | home | slow4g | 394 | 385 |
| none | settings | fast4g | 432 | 428 |
| none | settings | slow4g | 396 | 387 |
| br | login | fast4g | 239 | 237 |
| br | login | slow4g | 219 | 216 |
| br | home | fast4g | 422 | 418 |
| br | home | slow4g | 388 | 379 |
| br | settings | fast4g | 430 | 425 |
| br | settings | slow4g | 397 | 391 |

#### 選項比較（br，5 次中位數，ready ms；括號內是傳輸 KB；A 與 B1 是目前實作的前後，B2、B2C 是沒採用的方案）

| 方案 | 頁面 | 網路 | 冷 | 熱 | 部署新版後再訪 |
| --- | --- | --- | --- | --- | --- |
| A | login | fast4g | 585 (219) | 237 (3) | 577 (209) |
| A | login | slow4g | 1433 (219) | 222 (3) | 1384 (209) |
| A | home | fast4g | 769 (219) | 421 (3) | 761 (210) |
| A | home | slow4g | 1598 (219) | 393 (3) | 1555 (210) |
| A | settings | fast4g | 778 (219) | 430 (3) | 771 (210) |
| A | settings | slow4g | 1606 (219) | 394 (3) | 1563 (210) |
| B1 | login | fast4g | 580 (208) | 237 (3) | - |
| B1 | login | slow4g | 1382 (208) | 217 (3) | - |
| B1 | home | fast4g | 761 (209) | 423 (3) | - |
| B1 | home | slow4g | 1546 (209) | 386 (3) | - |
| B1 | settings | fast4g | 768 (214) | 433 (3) | - |
| B1 | settings | slow4g | 1578 (214) | 400 (3) | - |
| B2 | login | fast4g | 720 (186) | 244 (3) | 711 (176) |
| B2 | login | slow4g | 1439 (186) | 227 (3) | 1384 (176) |
| B2 | home | fast4g | 745 (211) | 422 (3) | 743 (202) |
| B2 | home | slow4g | 1561 (211) | 378 (3) | 1513 (202) |
| B2 | settings | fast4g | 894 (220) | 429 (3) | 892 (210) |
| B2 | settings | slow4g | 1621 (220) | 394 (3) | 1573 (210) |
| B2C | login | fast4g | 721 (188) | 245 (3) | 584 (39) |
| B2C | login | slow4g | 1447 (188) | 224 (3) | 693 (39) |
| B2C | home | fast4g | 747 (214) | 423 (3) | 618 (65) |
| B2C | home | slow4g | 1576 (214) | 379 (3) | 823 (65) |
| B2C | settings | fast4g | 898 (222) | 429 (3) | 767 (73) |
| B2C | settings | slow4g | 1635 (222) | 394 (3) | 888 (73) |

#### 部署新版後再訪（變更後的建置，7 次中位數，ready ms；括號內是傳輸 KB）

| 入口 | 頁面 | 網路 | 冷 | 熱 | 部署新版後再訪 |
| --- | --- | --- | --- | --- | --- |
| none | login | fast4g | 1029 (718) | 236 (3) | 998 (678) |
| none | login | slow4g | 3925 (718) | 219 (3) | 3734 (678) |
| none | home | fast4g | 1211 (719) | 420 (3) | 1180 (679) |
| none | home | slow4g | 4089 (719) | 385 (3) | 3897 (679) |
| none | settings | fast4g | 1220 (733) | 428 (3) | 1194 (694) |
| none | settings | slow4g | 4177 (733) | 387 (3) | 3981 (694) |
| gzip | login | fast4g | 587 (222) | 238 (3) | 581 (212) |
| gzip | login | slow4g | 1448 (222) | 217 (3) | 1400 (212) |
| gzip | home | fast4g | 771 (222) | 421 (3) | 765 (212) |
| gzip | home | slow4g | 1613 (222) | 386 (3) | 1565 (212) |
| gzip | settings | fast4g | 778 (228) | 428 (3) | 771 (218) |
| gzip | settings | slow4g | 1646 (228) | 388 (3) | 1602 (218) |
| br | login | fast4g | 576 (208) | 237 (3) | 569 (199) |
| br | login | slow4g | 1380 (208) | 216 (3) | 1333 (199) |
| br | home | fast4g | 755 (209) | 418 (3) | 751 (199) |
| br | home | slow4g | 1546 (209) | 379 (3) | 1503 (199) |
| br | settings | fast4g | 765 (214) | 425 (3) | 757 (204) |
| br | settings | slow4g | 1579 (214) | 391 (3) | 1540 (204) |

### 舊分頁碰到被移除的 chunk

真實瀏覽器（變更後的建置）：開著首頁的舊分頁，部署新版移除它還沒載入的兩個 chunk（`removedChunks: 2`，原始資料 `stale-chunk.json`）：

- 以 `pushState` 加 `popstate` 導向 `/settings`（路由去載入設定頁的 chunk，伺服器回 404）：頁面自己重新載入一次（`performance.timeOrigin` 改變，`sessionStorage` 的 `trustiot.preload-reload-at` 有時間戳），重新載入後設定頁正常顯示（`h1` 是 `Settings`）。
- 一分鐘內再碰到另一個被移除的 chunk（導向 `/devices/1`）：不再重新載入（2 秒後 `timeOrigin` 不變），畫面是路由的錯誤頁（`Something went wrong`），不是空白。

單元層級：`src/app/preload-chunks.test.ts` 建置真正的應用程式，檢查每個切出的 chunk 的動態 `import()` 都包在 Vite 的 preload helper 裡、helper 會送出 `vite:preloadError`、處理程式在同一份建置裡；`src/app/routes.test.tsx` 檢查切出的頁面以深連結載入、chunk 載入失敗時顯示錯誤頁；`preload-error.ts` 的重新載入與一分鐘保護，原有的測試在 `src/app/paths.test.ts`。

## 選項與決定

| 方案 | 內容 | 結果（br，slow4g，登入頁 ready） |
| --- | --- | --- |
| A | 不切（變更前） | 1433 ms，219 KB |
| B1 | 設定頁與設備頁用 `lazy` 切出 | 1382 ms，208 KB（-51 ms） |
| B2 | B1 加上登入頁與版面也切出（`Brand` 移到自己的檔案，登入頁才不會把版面拉進來） | 1439 ms，186 KB：多了一次往返，抵消省下的位元組；fast4g 反而慢 135 ms |
| B2C | B2 加上把 react、react-dom、react-router、zod、TanStack Query 等放進一個固定的 `framework` chunk（`rolldownOptions.output.codeSplitting.groups`） | 冷快取與 B2 相同；部署新版後再訪只下載 39 至 73 KB，A 是 209 KB（見「選項比較」表） |

**決定：以路由切分設定頁與設備頁（B1）。** 理由：

1. 結構上該有：主 bundle 不再隨每一頁成長。設備頁接下來會加入儀表板元件與圖表套件（Laysi/trustiot-v2#895，依 README 以 lazy import 載入），之後的頁面也一樣；現在把「重的頁面用 `lazyPage()` 載入」變成規則，新增頁面時不必再回頭拆。
2. 量到的好處是小的：br 下登入頁快 55 ms（3.8%）、首頁快 53 ms，設定頁（要多載一個 chunk）快 30 ms；沒有壓縮時登入頁快 218 ms（5.3%，位元組少了 44 kB）。
3. 登入頁與版面不切（B2）：量測顯示沒有好處（登入頁 ready 持平或更慢），所以不做。
4. 範圍限在路由（`src/app/`）。固定的 `framework` chunk（B2C）屬於建置設定，影響每次部署的下載量，列在未決事項，不在這個 PR。

這個 PR 不改 Vite 的設定、也不調高 `chunkSizeWarningLimit`：主 bundle 仍超過 500 kB，提醒保留，它指的是 react-dom、router 與 zod 這個下限，不是頁面的程式碼。

## 結論

逐條對照 Laysi/trustiot-v2#948 的驗收條件：

- **建置輸出的大小與載入時間的量測、是否切分的決定與理由存進 `docs/test-reports/`**：通過，就是這份報告與 `data/` 底下的原始資料。圖表套件的 chunk 待測：Laysi/trustiot-v2#895 還沒進 main；`frontend/README.md` 已寫明它以 lazy import 載入、進自己的 chunk，進來後量它的大小與載入時間。
- **決定切分時，變更已合併，`typecheck`、`lint`、`test`、`build`、`check:dist` 都通過，並重新量測**：變更與重新量測（上表）完成；檢查的結果見 PR；合併由人決定。
- **舊 chunk 不存在的處理仍涵蓋新的 chunk**：通過（真實瀏覽器與單元測試，見上節）。

## 未決事項

1. **壓縮**：`services/web` 送未壓縮的 JavaScript，直連時 slow4g 的登入頁 ready 是 4.1 秒，經壓縮的入口是 1.4 秒。選項：（a）`services/web` 啟動時預先壓縮、依 `Accept-Encoding` 送出（跨 `services/web`，要改它的 README 與測試）；（b）在入口設定壓縮（Istio 的 EnvoyFilter 或客戶的入口），文件寫明；（c）只依賴 SaaS 的 Cloudflare。建議 a 或 b 另開 issue 決定，影響的是地端。
2. **固定的 `framework` chunk（B2C 的做法）**：部署新版後再訪，下載量從 209 KB 降到 39 至 73 KB（br），首次進入不變。代價是多兩個請求、`vite.config.ts` 多一組 `codeSplitting` 設定。建議另開 issue，連同 B1 的建置再量一次後採用。
3. **API 瀑布**：首頁與設定頁在 `session` 之後才讀 `preferences`（兩次依序的往返）。與 bundle 無關，列出來供之後的畫面最佳化參考。
