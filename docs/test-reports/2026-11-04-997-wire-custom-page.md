# 儀表板頁接上自訂頁面的 frame：瀏覽器實測（2026-11-04）

對應 issue：Laysi/trustiot-v2#997 的接線部分（`frontend/src/pages/dashboards/CustomPageSlot.tsx` 改成渲染 PR Laysi/trustiot-v2#1057 合併的 `CustomPageFrame`）。沙箱本身與顯示時比對的測試報告是 `2026-11-03-997-custom-page-sandbox.md`；這份只驗證儀表板頁、`services/web` 的標頭與 SPA 的 CSP 接起來之後，真的瀏覽器看到的結果。

## 環境

- 建置主機（x86_64 Linux），分支 `feat/997-wire-custom-page`（疊在 main `0d3130dd` 上）；工具版本是 `.prototools` 的。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像裡的 Chromium，`--site-per-process`（桌面版 Chrome 的預設），視窗 1280 × 900，語言 en-US，時區 UTC。
- 被測的部分都是真的：SPA 的 `App`、路由、儀表板頁、`CustomPageFrame` 與 `SandboxHost`（真的 iframe）、`services/web`（真的二進位檔，真的標頭與 `frame-src`）、真的沙箱檔案（`bun run --cwd frontend build` 的 `dist/sandbox`）。
- 替身：儀表板與身分的 API 是 `src/test/fake-dashboards.ts`、`fake-identity.ts` 的記憶體版本（與 `2026-11-03-996-dashboard-page.md` 的預覽相同做法），所以沒有經過入口、也沒有真的 assistant 服務。`services/web` 直接聽在 `127.0.0.1`，瀏覽器的 `Host` 就是 `127.0.0.1:<埠>`，`frame-src` 因此對得到。
- 儀表板的版本有兩項：KPI 與 `custom_page`；頁面是 `<p>Monday <b id="v"></b> kWh</p>` 加一段 `tq.text(…, "c|daily|kwh|0", ",.1f")`。

## 指令

一次性的腳本不進 repo，副本在 `data/2026-11-04-997-wire-custom-page/scripts/`（加 `.txt`）。步驟：把 `main.tsx`、`index.html`、`preview.css`、`vite.config.ts`、`check.mjs`、`run.sh` 去掉 `.txt` 放進 `frontend/.preview-997/`，然後在 repo 根目錄：

```sh
bash frontend/.preview-997/run.sh
```

`run.sh` 依序：`bun install --cwd frontend --frozen-lockfile`；`bun run --cwd frontend build`（取 `dist/sandbox`）；以預覽的設定建置 SPA；`cargo build --locked -p web` 並以 `TRUSTIOT__ROOT_DIR`、`TRUSTIOT__HTTP__LISTEN_ADDR`、`TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR` 啟動；等 `/readyz`；在 Playwright 映像裡跑 `check.mjs`。

## 結果

原始資料：[result.json](data/2026-11-04-997-wire-custom-page/result.json)。

| 檢查 | 結果 |
| --- | --- |
| 沙箱檔案的回應 | `sandbox.html`、`runtime.js`、`d3.js`、`sandbox.css` 都是 200；`sandbox.html` 帶沙箱的 CSP（`sandbox allow-scripts; …; frame-ancestors 'self'`），沒有 `X-Frame-Options`；四個檔案都是 `Cross-Origin-Resource-Policy: cross-origin` |
| SPA 的 CSP | 含 `frame-src 127.0.0.1:<埠>/sandbox/v1/sandbox.html`；Trusted Types 的 report-only 政策在 |
| frame 的初始狀態 | 第一次取樣（35 ms）：`opacity 0`、`pointer-events none`、`aria-hidden true`、`inert`，也就是比對乾淨之前是藏起來的 |
| 比對乾淨後 | 352 ms 時 `opacity 1`、`pointer-events auto`、`aria-hidden false`、不 `inert`；5 秒後仍然顯示 |
| 標記的值 | frame 裡 `data-tq="c\|daily\|kwh\|0"` 顯示 `120.5`（資料集第一列 120.5），沒有 `data-tq-flag` |
| 卡片 | 一張卡片（沒有再包一層）：標題「Custom page check」、「Custom chart」、「Drawn by code the AI wrote, not a platform component」、「This version passed the check before it was saved on …」與「Source」按鈕；外層只有「Dashboard contents」那個清單 |
| CSP 與 Trusted Types 違規 | 外層頁面與 frame 的 `securitypolicyviolation` 事件都是 0 件（顯示前後各查一次） |
| 主控台與例外 | 錯誤與警告 0 則，頁面例外 0 個 |
| 對外請求 | 對 `services/web` 以外的請求 0 個 |

### 前端的檢查（建置主機）

`bun install --cwd frontend --frozen-lockfile` 後，`api:check`、`typecheck`、`lint`、`build`、`check:dist` 都通過（exit 0）；`test`：1036 個通過、0 個失敗。

## 結論

| 驗收條件（接線部分） | 結果 |
| --- | --- |
| 儀表板頁的自訂頁面項目顯示 `CustomPageFrame` 的卡片 | 通過：`DashboardPage.test.tsx` 與上面的瀏覽器實測 |
| 版本沒有 `page` 或 `runtime` 時這一項不畫 | 通過：`DashboardPage.test.tsx` |
| 乾淨比對後才顯示 frame、標記的值顯示 | 通過：瀏覽器實測 |
| 沒有 CSP 或 Trusted Types 違規 | 通過：瀏覽器實測 |

沒有涵蓋：經入口與 Istio 網域的整套流程與 `scripts/browser-e2e/run.ts` 的完整流程（延後到 Laysi/trustiot-v2#1055）、Chromium 以外的瀏覽器（Laysi/trustiot-v2#946）、真的 assistant API 與資料集（API 是記憶體替身）。
