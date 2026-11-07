# frontend

Vite + React 的單頁應用程式（SPA，決策 2），頁面全在登入後，與 API 走同一個網域：入口把 `/api`、`/auth` 送到服務，其餘路徑由 `services/web` 提供這裡建置出來的檔案。以 Bun 安裝相依套件、執行腳本與測試，Bun 版本鎖在根目錄的 `.prototools`。不是 Cargo workspace 成員。2026-10-21 建立骨架（Laysi/trustiot-v2#123）：路由與深層連結、zh-TW 與 en、API client、錯誤頁、偏好設定、租戶切換、SSE 的連線處理。2026-10-25 加上第一批真正的畫面與 UI 基礎（Laysi/trustiot-v2#124）：email 驗證碼登入、登出與登出所有裝置、從身分服務讀取的工作階段與租戶切換、沒有目前租戶時的選擇、存在帳號裡的個人設定、組織停權頁。2026-10-30 加上設備頁（Laysi/trustiot-v2#128）：設備資訊、每個資料點最新的已存值與歷史圖表，2026-11-04 依設計稿改版（Laysi/trustiot-v2#781），見「設備頁」。2026-11-01 加上儀表板元件模式的五種元件（Laysi/trustiot-v2#895），見「儀表板元件」。

## 技術組合

| 用途 | 套件 |
| --- | --- |
| 建置與開發伺服器 | Vite 8（Rolldown）、`@vitejs/plugin-react` |
| UI | React 19；Radix UI 的 primitives 加上複製進 repo 的 shadcn 風格元件（`src/components/ui/`），圖示用 `lucide-react`；見「UI 元件與樣式」 |
| 樣式 | Tailwind CSS 4（`@tailwindcss/vite`，建置時編譯成靜態 CSS）與 `tw-animate-css`；`class-variance-authority`、`clsx`、`tailwind-merge` 組合 class |
| 路由 | React Router 8（data mode：`createBrowserRouter`，測試用 `createMemoryRouter`） |
| 伺服器狀態 | TanStack Query 5 |
| API client | `openapi-fetch`，型別由 `openapi-typescript` 從 `api/openapi.json` 產生 |
| 圖表 | ECharts 6（`echarts` 6.1.0，canvas renderer，只 import 用到的模組並 lazy 載入），儀表板元件與設備頁的歷史圖共用，規則見「儀表板元件的圖表套件」 |
| 儀表板沙箱 | d3 的八個模組（`d3-array`、`d3-axis`、`d3-format`、`d3-scale`、`d3-selection`、`d3-shape`、`d3-time`、`d3-time-format`），目前只打包進沙箱自己的建置，不進 SPA 的 bundle；見「儀表板沙箱」 |
| 外部資料的執行時檢查 | zod 4，一律經 `src/lib/zod.ts`（`jitless`，見「CSP 的限制」與「外部資料一律經 zod 解析」） |
| 型別檢查 | TypeScript 6（原因見下方「TypeScript 停在 6」） |
| lint 與格式 | Biome |
| 測試 | `bun test`，DOM 由 happy-dom 提供，元件以 Testing Library 操作；沙箱的測試另外自己開 happy-dom 視窗 |
| 瀏覽器端對端測試 | Playwright Test（`@playwright/test`），在釘選的 Playwright 容器映像裡跑 Chromium；測試在 `e2e/`，執行器在 `scripts/browser-e2e/`；見「瀏覽器端對端測試」 |

相依一律是精確版本（`bunfig.toml` 的 `exact = true`），`bun.lock` 進版控，`trustedDependencies` 是空的：任何相依的安裝腳本都不執行。新增相依只用 `bun add --cwd frontend <套件>@<版本>`，不用 npm、pnpm。

## 指令

都在 repo 根目錄執行：

| 指令 | 做什麼 |
| --- | --- |
| `bun install --cwd frontend --frozen-lockfile` | 依 `bun.lock` 安裝 |
| `bun run --cwd frontend dev` | 開發伺服器（熱更新）；後端見下方「開發伺服器」 |
| `bun run --cwd frontend ui-preview` | UI 預覽：不接後端，以假服務打開所有畫面（含後端還沒有的），`http://localhost:5174/`；見「UI 預覽」 |
| `bun run --cwd frontend api:generate` | 由 `api/openapi.json` 重新產生 `src/api/schema.gen.ts` |
| `bun run --cwd frontend api:check` | 進版控的 `schema.gen.ts` 與規格產生的結果不同就失敗 |
| `bun run --cwd frontend typecheck` | `tsc --noEmit`，含 `src/`、`sandbox/`、`tools/` 與 `vite.config.ts` |
| `bun run --cwd frontend lint` | `biome ci .`；`bun run --cwd frontend format` 修正格式與 import 順序 |
| `bun run --cwd frontend test` | 全部單元測試；單一測試：`bun run --cwd frontend test -t "<測試名稱的一部分>"` |
| `bun run --cwd frontend build` | Vite 建置 SPA 到 `frontend/dist/`，再以 `tools/build-sandbox.ts` 建置儀表板沙箱到 `dist/sandbox/`（都不進版控） |
| `bun run --cwd frontend check:dist` | 檢查 `dist/` 符合下方的版面與 CSP 規則，且不含只給開發用的程式（「UI 預覽」） |
| `bun scripts/browser-e2e/run.ts` | 瀏覽器端對端測試（Playwright、Chromium，真的身分服務與 `services/web`）；不在 CI 執行，前置與做法見「瀏覽器端對端測試」 |

CI 的 `frontend` job 依序跑 install、`api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist`（`docs/architecture/ci-release.md` 的「ci.yml 的 job」）。瀏覽器端對端測試不在 CI：它要起整套開發環境與兩個服務，執行位置的選項見「瀏覽器端對端測試」。

產生 API client 與檢查建置結果的 Bun 腳本放在 `tools/`，不叫 `scripts/`：在 repo 根目錄執行的 `bun test scripts` 以路徑子字串篩選測試檔，目錄叫 `scripts/` 會連前端的測試一起跑，而那時前端的相依不一定已安裝。

## UI 元件與樣式

決策 2（2026-10-25，Laysi/trustiot-v2#124）：shadcn 風格的元件加 Radix 的 primitives 與 Tailwind CSS 4，取代骨架時「只用一般 CSS 或 CSS Modules」的做法。

- **Tailwind 在建置時編譯成靜態 CSS**：`vite.config.ts` 的 `@tailwindcss/vite` 掃描原始碼用到的 class，和 `src/styles/global.css` 一起輸出成 `static/<hash>.css`。樣式只有這一個 stylesheet；不用 CSS-in-JS、不用 `style` 屬性，執行時不產生也不注入任何樣式（見「CSP 的限制」）。動畫用 `tw-animate-css`（純 CSS），`prefers-reduced-motion` 時全部縮成一瞬間。
- **主題是 CSS 變數**：顏色以用途命名（`--background`、`--primary`、`--destructive`……，shadcn 的名稱，複製來的元件直接可用），定義在 `:root` 與 `:root[data-theme="dark"]`，經 `@theme inline` 變成 Tailwind 的顏色；`dark:` 變體跟著 `<html data-theme>`，不跟作業系統。主題由 `PreferencesProvider` 設定，`main.tsx`（`src/app/start.tsx` 的 `startApp`）在第一次繪製前先套用瀏覽器裡的副本，所以不會閃一下淺色。每組文字與背景、輸入框框線與焦點框都符合 WCAG 2.2 AA（文字 4.5:1，框線 3:1）；改顏色時要重新確認。
- **品牌色以官網的 token 為準**（Laysi/trustiot-v2#929，2026-11-01 維護者決定）：官網的薄荷綠 `#0d9b76` 對白底只有 3.52:1，只能當圖形（logo、大面積色塊），**不能當小字**；`global.css` 的 token 由它推出，`--primary`、`--primary-foreground` 與 `--ring` 兩個主題都指向 `--brand-strong` 與 `--on-brand-strong`。官網沒有深色主題，深色值是 v2 自己推的，色相同為 168°（OKLCH）。官網的頁面底色是米白 `#f4f3ee`，v2 的底色維持白，不跟：

  | token | 淺色 | 深色 | 用法 |
  | --- | --- | --- | --- |
  | `--brand` | `#0d9b76`（官網 `--mint`；對白底 3.52:1） | `#0d9b76` | logo 的填色與大面積圖形；不當小字 |
  | `--brand-strong` | `#086650`（官網 `--mint-deep`；對白底 6.94:1） | `oklch(0.87 0.09 168)`（對深色背景 13.38:1） | 主色：按鈕底色、連結、焦點框、圖表線；等於 `--primary` 與 `--ring` |
  | `--brand-soft` | `#dcefe6`（官網 `--mint-soft`） | `#0e2d25` | 只當面的底色（選取、提示區塊），上面的文字用 `--foreground` |
  | `--on-brand` | `#03140f`（對 `--brand` 5.37:1） | `#03140f` | 放在 `--brand` 上的文字 |
  | `--on-brand-strong` | `#ffffff`（對 `--brand-strong` 6.94:1） | `#03140f`（13.22:1） | 放在 `--brand-strong` 上的文字；等於 `--primary-foreground` |

  淺色的 `--brand-strong` 選官網的 `--mint-deep`，不選 `--mint-text`（`#0a7a5c`）：後者對白底 5.31:1，但按鈕 hover（`primary/90`）下的白字只有 4.40:1，連結放在 `--brand-soft` 上只有 4.44:1，都不到 4.5:1。

  用法規則：元件一律用 `primary`、`ring` 等角色名稱，不直接寫 `brand-*`，除非要的是 logo 的原色或淡綠底；不寫死色碼。`--success` 與 `--destructive` 維持原值：「成功」只出現在帶文字的圖示（對勾加說明），不單靠顏色表達；淺色主題裡 `--success` 與 `--primary` 的亮度很接近（OKLab L 0.50 對 0.46，亮度對比 1.23:1），2026-11-01 維護者接受靠用途區分，所以不要把兩者放在同一個控制項裡靠顏色區分。各組對比數值在 `docs/test-reports/2026-11-01-929-brand-colors.md`；改任何品牌色都要重跑那份報告的檢查。
- **logo**：`public/favicon.svg` 就是 logo（官網的 `trustiot-mark.svg`，同一個 viewBox，單色 `#0d9b76`），直接畫在頁面上、不加底色方塊：對白底 3.52:1，對深色背景 5.43:1，兩個主題都超過圖形需要的 3:1，所以只要一份檔案；版面的 `Brand` 與瀏覽器分頁圖示是同一個檔案。SVG 不能有 `style` 屬性或 `<style>`（CSP，`check:dist` 檢查），顏色用 `fill` 屬性。
- **元件在 `src/components/ui/`**：從 shadcn 複製進來再依需要修改（原始碼是我們的，不是套件），只放畫面用到的；新畫面需要新元件時才加。Radix 的 primitives 是相依套件（`@radix-ui/react-*`，各自鎖定版本）。`src/lib/utils.ts` 的 `cn()` 合併 class，變體用 `class-variance-authority`。和 shadcn 原版不同的地方：焦點以 `outline` 顯示（`button.tsx` 的 `focusRing`，強制色彩模式下仍看得到）、`Button` 預設 `type="button"`、`Label` 一定要有 `htmlFor`、表單欄位以 `FormField` 串起標籤、說明與錯誤（`aria-describedby`、`aria-invalid`）。
- **游標**：可以點的元素（按鈕、下拉選單、選單項目、`role="button"` 等）顯示手形，停用時維持箭頭（2026-10-25 專案擁有者決定）。停用指的是 `disabled` 屬性、`aria-disabled="true"` 或 Radix 的 `data-disabled`，自己做的可點元素停用時要設其中一個。Tailwind 4 沿用瀏覽器的預設，只有連結顯示手形，所以這條規則寫在 `global.css` 的 base layer，新元件自動套用；元件不另外寫 `cursor-*`：從 shadcn 複製來的元件若自己設了游標（例如選單項目的箭頭、輸入框停用時的禁止符號），要拿掉那個 class，否則 utility 會蓋過這條規則，停用時的游標也會和其他控制項不一致。Tailwind 也會掃描這份 README，所以這裡不寫出完整的 class 名稱，免得建置出沒用到的 utility。
- **不用 Radix 的 Select**：它的 `SelectViewport` 以 `dangerouslySetInnerHTML` 寫入 inline `<style>` 隱藏捲軸，CSP 擋下，Trusted Types 也會回報。下拉選單一律用 `NativeSelect`（樣式化的原生 `<select>`），鍵盤、螢幕報讀與手機的選擇器都由瀏覽器處理。
- **Radix 的 modal 元件鎖捲動不注入 `<style>`**：`DropdownMenu`、`AlertDialog` 經 `react-remove-scroll` 鎖住背景捲動，它原本以 `react-style-singleton` 注入 `<style>`。`vite.config.ts` 把 `react-style-singleton` alias 到 `src/lib/no-style-singleton.ts`（同樣的 export、不碰 DOM），鎖捲動的規則改寫在 `global.css` 的 `body[data-scroll-locked]`（屬性仍由 `react-remove-scroll-bar` 設定），`html` 的 `scrollbar-gutter: stable` 讓鎖住時版面不跳動。`src/test/setup.ts` 以 `mock.module` 做同樣的替換，所以測試跑的與建置相同；`no-style-singleton.test.tsx` 確認打開選單與對話框時文件裡沒有 `<style>`。
- Radix 以 CSSOM（`element.style`）設定彈出位置與 focus guard，CSP 的 `style-src` 不管這種寫法，不必處理。
- **AI 對話畫面**（Laysi/trustiot-v2#149）用 assistant-ui 的官方元件（`@assistant-ui/react` 與它的 Markdown `@assistant-ui/react-markdown`），它們建在同一套 shadcn + Radix + Tailwind 上。Laysi/trustiot-v2#1042 安裝，並加上下面「CSP 的限制」提到的 micromark `decode-named-character-reference` alias；用法見「對話畫面」。

## 畫面設計稿

2026-11-01 定案的畫面設計稿在 `docs/design/`（對話首頁、租戶後台、平台後台、設備頁、MS4–MS5 的選單頁，各組的 `design.md` 與可以直接用瀏覽器打開的 HTML）；實作畫面的 issue 以那裡為準，目錄與各組對應的 issue 見 `docs/design/README.md`。

- **首頁是對話**：打開 TrustIoT 就是全幅的對話框，設備狀態、圖表與表格是 AI 回答裡的元件，設定與管理的畫面收在選單（`docs/requirements/rebuild-checklist.md` 的 L3，2026-11-01 維護者決定）。AI 不可用時（例如沒有設定 AI）首頁退回通用版面，MS2 的首頁也是它（`docs/design/home-ai/design.md`）。
- **與設計稿不同時**：PR 寫明哪裡不同與原因，並在同一個 PR 更新對應的 `design.md`；HTML 頁面只在畫面的行為或版面改變時才跟著改（`docs/design/README.md`「使用規則」）。
- **`--warning`（琥珀色）**：設計稿用它把「異常」與「錯誤」分開。儀表板頁的「看舊版」標籤（Laysi/trustiot-v2#996）是第一個用到它的畫面，補了 `--warning` 與 `--warning-soft`（淺色與深色兩個值，等於設計系統的 `status-warning` 與 `status-warning-soft`，`src/styles/design-tokens.test.ts` 的 `FRONTEND_TO_DESIGN` 把名稱對起來）；對比在 `docs/test-reports/2026-11-03-996-dashboard-page.md`。設計系統的其他狀態色（`status-normal`、`status-critical`、`status-offline` 與它們的 `-soft`）等有畫面用到再補，規則見 `docs/design/system/README.md` 的「狀態」。

## 建置結果的約定（與 `services/web`）

`services/web` 原樣提供 `dist/`，所以版面是兩邊的約定，`tools/check-dist.ts` 檢查：

- 根目錄只有 `index.html` 與 `public/` 的檔案（`robots.txt`、`favicon.svg`）。
- 其餘都在 `static/` 下，檔名只有內容的 hash（`static/<hash>.js`、`static/<hash>.css`），原始碼的模組名稱與路徑不會出現在映像裡；內容改變時檔名一定改變，所以這些檔案可以長期快取。
- 儀表板沙箱的檔案在 `sandbox/` 下，正好是 `tools/build-sandbox.ts` 寫出的那些（`sandbox/check.html`、`sandbox/check.js`，以及每個 runtime 版本的 `sandbox/v<版本>/` 下的 `sandbox.html`、`runtime.js`、`d3.js`、`sandbox.css`、`compare.js`）；兩份 HTML 沒有 inline script 或樣式，只引用同一目錄的檔案。
- 沒有 source map、沒有 `index.html` 與沙箱文件以外的 HTML，副檔名只限 html、js、css、svg、png、ico、woff2、txt、webmanifest。
- 資源以絕對路徑引用（`base: "/"`），所以 `/devices/42` 這種深層連結載入的資源與 `/` 相同。

保留給其他用途的路徑，前端路由不能使用（`src/app/paths.ts`，`routes.test.tsx` 檢查每個路由）：

- 前綴：`/api`、`/auth`（入口送到服務）、`/internal`（入口永遠不轉送）、`/static`（建置的資源）、`/sandbox`（儀表板沙箱）。
- 路徑：`/healthz`、`/readyz`、`/index.html`、`/robots.txt`、`/favicon.svg`。

其他路徑都由 `services/web` 回 `index.html`，再由前端路由決定畫面；不存在的路徑顯示前端的「找不到頁面」。

## CSP 的限制

頁面的 CSP 只允許同源的 script 與 style，所以：

- 不能有 inline script 或 inline style：不用 `style` 屬性、`<style>`、`on*=` 屬性與 `dangerouslySetInnerHTML`（Biome 擋下後者）。樣式全部寫在 CSS 檔，主題以 `<html data-theme>` 切換 CSS 變數。
- 不能有 `data:` URI：`assetsInlineLimit: 0`，小檔案也輸出成 `static/` 下的檔案。
- 只能框儀表板沙箱的文件：`services/web` 在 SPA 的 CSP 加上 `frame-src <請求的主機>/sandbox/v<N>/sandbox.html`（每個建置帶的 runtime 版本一個），其他 frame 一律擋下（`default-src 'none'`），沙箱 frame 也不能被導到別的地方（Laysi/trustiot-v2#997；`services/web/README.md`「安全標頭」）。沙箱的檔案有自己的標頭，不帶 SPA 的（「儀表板沙箱」）。
- 不用 Vite 的 modulepreload polyfill 與 React Router 的 `<ScrollRestoration>`，兩者都會產生 inline script。
- 不用 `eval`、`new Function`。zod 4 建立 `z.object` schema 時會以 `new Function("")` 探測能不能編譯解析器，沒有 `'unsafe-eval'` 的 CSP 會擋下它（zod 吞掉例外），但瀏覽器仍發出 `securitypolicyviolation`，`check:dist` 又不看 JavaScript。所以 zod 一律以 `jitless` 執行：`src/lib/zod.ts` 先 `z.config({ jitless: true })` 再匯出 `z`，其他模組只從它匯入（ES module 的求值順序保證設定早於任何 schema 的建立）；Biome 的 `noRestrictedImports` 禁止在它以外匯入 `zod`，也禁止 `zod/compile`（它的 AOT 編譯同樣用 `new Function`）。
- 不把字串寫進會解析成 HTML 或程式的 DOM 介面（`innerHTML`、`outerHTML`、`insertAdjacentHTML`、`document.write`、`srcdoc`）。頁面已經送出 Trusted Types（`require-trusted-types-for 'script'`），目前只回報，之後會改成強制，強制後這類寫入會直接丟出例外。套件這樣做時，改用它不碰 DOM 的版本（`vite.config.ts` 的 `resolve.alias`）或換掉它。

`check:dist` 只檢查建置輸出的 HTML、SVG 與 CSS，不看 JavaScript。套件在執行時才注入的 `<style>` 或寫進 `innerHTML` 的字串，只會在瀏覽器裡出現：被 CSP 擋下或被 Trusted Types 回報時，主控台會出現 violation。新增 UI 或 Markdown 相關的套件時，要在瀏覽器實際操作用到它的畫面，確認主控台沒有 violation。

2026-10-21 以 assistant-ui 實測時碰到的情況：

- Radix 的 modal 元件（`Dialog`、`DropdownMenu` 等）經 `react-remove-scroll` 注入 `<style>` 來鎖住背景捲動。CSP 會擋下，元件本身照常運作，只是背景沒有鎖住。2026-10-25 起以 alias 與靜態 CSS 處理（見「UI 元件與樣式」），沒有放寬 `style-src`。
- micromark（`react-markdown` 用的解析器）依賴的 `decode-named-character-reference`，瀏覽器版以 `innerHTML` 解碼 HTML 實體。`vite.config.ts` 的 `CHARACTER_REFERENCE_ALIAS` 把它 alias 到不碰 DOM 的 `index.js`（以 Node 的解析找到，Laysi/trustiot-v2#1042）；`src/chat/character-reference.test.ts` 檢查 alias 指到的檔案沒有 `innerHTML`，套件升級改了檔案時會失敗。bun test 本來就不用瀏覽器版，所以測試不必另外 mock。
- Radix 的 tooltip 與 popper 以 CSSOM 設定定位樣式，CSP 允許，不必處理。

部署新版後，舊頁面要載入的 chunk 可能已不存在：`vite:preloadError` 時重新整理一次頁面，60 秒內不重複（`src/app/preload-error.ts`），避免無限重新整理。路由以 `lazy` 切出的 chunk 也走 Vite 的 preload helper，所以同樣涵蓋；`src/app/preload-chunks.test.ts` 建置真正的應用程式，檢查每一個切出的 chunk 都經過那個 helper，`framework` chunk 則以靜態 import 載入並列在 `index.html` 的 `modulepreload`（見「程式碼切分與載入量測」）。

## 路由與錯誤頁

路由定義在 `src/app/routes.tsx`：`/login` 不套版面，其餘套 `Layout`（共用外框的頁首，見「應用程式外框」，和頁首以下的頁面）。沒有工作階段的訪客由 `Layout` 導向 `/login?next=<目前路徑>`。`/settings` 與「找不到頁面」不需要目前租戶；處理租戶資料的頁面都放在 `TenantGate` 底下，它先讓工作階段進入一個租戶（見「偏好設定、租戶與工作階段」），租戶停權時以停權頁取代這些頁面。每一層都有 `errorElement`，所以任何畫面出錯都留在版面內顯示錯誤頁，不會整頁空白。錯誤頁依錯誤分類（`src/api/errors.ts` 的 `classifyError`）：

| 情況 | 畫面 |
| --- | --- |
| 401 或 `unauthenticated` | 「工作階段已結束」，連到 `/login?next=<目前路徑>` |
| 403 或 `forbidden` | 沒有權限 |
| `tenant_suspended` | 組織已停權 |
| `session_changed` | 一般錯誤頁，訊息說明帳號或組織已在其他分頁變更；`SessionProvider` 同時重讀工作階段，頁面改為目前的帳號與組織（見「多個分頁」） |
| 404 或 `not_found` | 找不到頁面 |
| 503、504、網路錯誤、`unavailable`、`timeout` | 服務暫時無法使用，附重新整理按鈕 |
| 其他 | 一般錯誤；只有 API 的錯誤會顯示翻譯後的訊息，程式錯誤的內容不顯示給使用者 |

查詢或寫入收到 401 時直接導向登入頁（`src/app/query-client.ts`），`SessionProvider` 同時忘掉這個使用者快取的資料，把工作階段標成已登出：工作階段在別的裝置被登出或過期時，畫面不會留著前一個工作階段的資料。自己處理 401 的請求（登入表單：驗證碼錯誤就是 401；登出所有裝置，見「登入與登出」）在 TanStack Query 的 `meta` 標上 `handlesUnauthenticated`，不觸發這兩件事。登入後要回到的 `next` 來自網址列，只接受本 SPA 的路徑（同源、不在保留路徑、不是登入頁；`//`、`/\`、控制字元與其他網域一律回首頁），避免成為開放重新導向（`safeNextPath`；判斷本身是 `spaPath`，回答裡的連結也用它，見「對話畫面」）。原始字串與解析後的路徑各檢查一次：解析會處理 `.`、`..`、`%2e` 這些點段，也會把 `\` 換成 `/`，所以 `/.//other.example`、`/x/../\other.example` 解析後是 `//other.example`，同樣回首頁。登入頁的比對與路由一樣不分大小寫、不管結尾的 `/`（`/LOGIN`、`/login/` 也算登入頁）。伺服器對 `redirect_to` 的檢查另見 Laysi/trustiot-v2#677。

## 對話畫面

對話紀錄、每一段對話的頁面與首頁的「最近的對話」（Laysi/trustiot-v2#1042，清單 J1；畫面在 [`docs/design/chat-dashboards/design.md`](../docs/design/chat-dashboards/design.md)「對話紀錄與對話頁」）。路由 `/conversations`（`?tab=archived`、`?tab=deleted` 是另外兩個分頁）與 `/conversations/:conversationId`，程式在 `src/pages/conversations/` 與 `src/chat/`，API 在 `src/api/conversations.ts`（`ConversationApi`，zod 的 body 綁到 `api/openapi.json` 產生的型別）。提問、等待與停止見下面的「提問」（Laysi/trustiot-v2#1044），回答邊寫邊顯示與斷線續播見「回答的串流」（Laysi/trustiot-v2#1046）。

- **對話紀錄**（`ConversationsPage.tsx`）：三個分頁各自一個 infinite query，一頁 100 段（API 的上限），「載入更多」讀到最後一頁。API 沒有搜尋參數：有搜尋字時，頁面自己把那個分頁其餘的頁讀完再依顯示的標題篩選，所以搜尋涵蓋還沒載入的對話。改名、封存、取消封存、刪除與還原都經 `queries.ts` 的 `useConversationChange`，成功或失敗之後都重讀所有列表與那段對話。刪除先確認（`DeleteDialog.tsx`）；伺服器拒絕時（還有問題在回答，409）訊息在視窗裡。
- **對話頁**（`ConversationPage.tsx`）：ID 不是 UUID 時不發請求，直接是「找不到這段對話」；404 也是同一頁。問題與回答由 assistant-ui 畫（`src/chat/Thread.tsx`）：`useExternalStoreRuntime` 接 API 給的訊息，`isDisabled`，沒有輸入框；問題是原文（`whitespace-pre-wrap`），回答經 `MessagePrimitive.Parts` 的 `MarkdownText`。
- **複製與匯出**：「複製回答」以 Clipboard API 寫入 API 給的原文（不是畫出來的文字），結果寫在旁邊的 `role="status"`。「匯出 Markdown」（`src/chat/export.ts`）是 `# 標題` 加每則的「## 問題」或「## 回答」與原文，以 `Blob` URL 下載；檔名去掉路徑、Windows 不接受的字元、控制與方向字元，最長 80 字，Windows 的保留名稱（含 `con.txt` 這種帶副檔名的）改用預設名稱。複製與匯出都是原文，所以回答裡的圖片網址與外部連結原樣留在剪貼簿與檔案裡：畫面的規則只保護這個 SPA，貼進或打開在會載入遠端圖片的 Markdown 檢視器時，網址就會被請求。這是接受的風險（使用者要的是原文；Laysi/trustiot-v2#1042 的安全審查 L2），服務端的 answer guard 另外會移除回答裡的 HTML 與程式碼。
- **回答的 Markdown 是不可信的**：回答是模型寫的，讀過提示注入的模型可以寫任何東西，而 Markdown 的圖片一畫出來就會把網址裡的資料送出去，連結只差一次點擊（`docs/architecture/ai-assistant.md`「安全」第 5 點）。所以回答用 assistant-ui 的 `MarkdownTextPrimitive`（react-markdown 10 與 remark-gfm）時加上三層規則，全部在 `src/chat/`：
  1. `safe-markdown.ts` 的 rehype plugin `rehypeSafeAnswer`，在 Markdown 解析成 HTML 樹之後執行，所以看到的是瀏覽器會拿到的網址（實體與反斜線跳脫已解碼、reference-style 的連結與圖片已代入、網址已百分比編碼）：只留段落、標題、清單、引用、表格、程式碼與強調，其他元素只留內容；**沒有任何 `<img>`**，圖片換成「［圖片沒有顯示：替代文字］」；連結只有 `spaPath`（`src/app/paths.ts`，登入頁的 `next` 用的同一個檢查：同源、以單一 `/` 開頭、不是保留路徑與登入頁，解析後再檢查一次，而且像 router 一樣逐段解碼百分比跳脫後也不是：`/%6Cogin`、`/%61pi/x` 都不算；解碼不了或解出 `/`、`\`、控制字元的路徑也不算）接受的路徑才保留，其他網址（外部網址、autolink、`javascript:`、`data:`、`//host` 等）都是文字，後面以括號寫出網址（Markdown 裡的 `/\host` 被解析器百分比編碼成 `/%5Chost`，解碼後含反斜線，所以也是文字）；原始 HTML（`raw` 節點）一律變成文字，從不解析成元素；除了程式碼的 `language-*` class、表格對齊（改成 class，因為 `align` 會變成 inline style）與有序清單的 `start`，元素不保留任何屬性。
  2. react-markdown 自己的 `allowedElements`（同一份清單）、`unwrapDisallowed` 與 `urlTransform`（`spaPath` 或空字串）。
  3. `MarkdownText.tsx` 的元件：`img` 什麼都不畫，`a` 再檢查一次 `spaPath`，是 SPA 路徑才畫成 React Router 的 `Link`（帶著 `fromConversation`，給「回到對話」用），否則是 `span`。
  `src/chat/safe-markdown.test.tsx` 在對話頁上逐一測：外部、`data:`、同源與 reference-style 的圖片，`javascript:`（大小寫混合、實體、百分比編碼）、`data:`、`vbscript:`、`//host`、`/\host`、反斜線、保留路徑、reference-style 連結，Markdown 裡的 `img`、`a`、`iframe`、`script`、`style`、`svg`、`form`，與很長的網址；每一項都檢查沒有會載入或執行東西的元素、沒有離開 SPA 的連結、沒有 Markdown 帶來的屬性，也沒有任何對外請求。
- **對話的 token**：提問泡泡用設計系統的 `--ask`、`--on-ask`、`--ask-border`（`global.css`，Tailwind 的 `bg-ask`、`text-on-ask`、`border-ask-border`）。
- **這一頁的瀏覽器檢查**（真正的 API、CSP 與 Trusted Types）在 `docs/test-reports/2026-11-04-1042-conversation-history.md`。
- **對話裡的草稿**（Laysi/trustiot-v2#1045，清單 J1、J4；畫面在 `design.md`「對話裡的草稿」）：對話頁讀 `GET /api/assistant/v1/conversations/{id}/dashboards`（`drafts.ts` 的 `useConversationDrafts`，只讀第一頁），有草稿時右邊（`xl` 以下在下面）是 `DraftPanel.tsx`。草稿像儀表板頁一樣讀：`getDashboard(草稿 ID, 列表給的最新版)`，內容以 `pages/dashboards/VersionItems.tsx`（儀表板頁與草稿面板共用的元件清單與 `useVersionContent`）畫，資料集以使用者自己的身分對草稿的 ID 執行（`dataset-runs` 也收草稿），所以草稿的數字與發布後儀表板頁的相同。「儲存到儀表板列表」只給租戶角色是 editor 或 admin 的人（與儀表板列表的封存同一個判斷），送出面板顯示的那一版（`POST …/publish`，body 只有 `version`）；成功顯示新版本號與 `/dashboards/<ID>` 的連結，並重讀草稿、儀表板列表與那個儀表板的版本；被拒（草稿依據的版本已經不是最新 409、沒有權限、已封存、次數上限）一律顯示翻譯過的訊息，從不顯示伺服器的原文。「捨棄草稿」與發布時的版本說明欄位沒有：API 沒有這兩個操作。
- **回答裡的儀表板**（Laysi/trustiot-v2#1049，清單 J1、J4；畫面在 `design.md`「回答裡的儀表板」）：每則回答的 `draft_versions`（Laysi/trustiot-v2#1043）列出它寫入的草稿版本；`Thread` 只多一個 `renderAnswerExtras`，頁面（`ConversationPage.tsx`）交給它 `AnswerDashboards.tsx`，把回答下面接上儀表板，`Thread` 不認得儀表板。`shownDraftVersions` 只留每個草稿的最後一版、依第一次寫入的順序。每一張以 `getDashboard(草稿 ID, 版本)` 讀（`dashboardVersionKey`，與草稿面板同一個 key，同一版只存一份），內容仍是 `VersionItems`（`layout="answer"`，用容器查詢 `@container` 排版，所以窄的回答欄裡 KPI 不會換行）；一張讀不到（404 或其他錯誤）只在原處寫一行說明，不影響回答與其他儀表板。按鈕「儲存到儀表板列表」只在那一版的 `permissions.can_publish` 為真時畫（伺服器依角色與對話擁有者決定，viewer 沒有），送出的是那一整版，與草稿面板是同一個動作、同一組文字。
  - **兩處共用**：「儲存」的行為在 `savedDrafts.tsx`（`useSaveDraft`、`SaveButton`、`SaveOutcome`），草稿面板與回答都用它；已儲存的版本記在 `SavedDraftsProvider`（`ConversationDashboards` 提供，這一次打開頁面期間有效，換對話或重新整理就清掉），所以在一處儲存，另一處也顯示已儲存；只有按下按鈕的那一處把焦點移到「開啟儀表板」。`dashboard.saved_changed`（409）被拒時，原因後面多一句「如果你剛儲存過這一版，第 N 版就是它。」（`saveFailureReason`，N 是 `details.latest`）：已儲存過的版本再按一次得到的是同一個 409。重新整理後顯示為已儲存要 API 回報（Laysi/trustiot-v2#1096）。
  - **資料集何時執行**：`lib/use-near-viewport.ts`（`IntersectionObserver`，`rootMargin` 600 px，靠近過就保持，沒有這個 API 的環境視為靠近）。回答裡的儀表板靠近之前，`useVersionContent(…, enabled=false)`：資料集不執行、元件顯示讀取中；草稿面板已經執行過同一版時，回答直接用它的結果。「同時 2 次」是租戶的上限，所以 `ConversationDashboards` 為整個頁面提供一個 `RunLimiter`（`RunLimiterContext`），回答與草稿面板的所有執行共用它；沒有這個 context 時（儀表板頁）每個 `useDatasetRuns` 自己有一個。
  - **測試**：`AnswerDashboards.test.tsx`（以 `src/test/fake-viewport.ts` 的假 `IntersectionObserver` 控制何時靠近），`FakeDashboardServer` 的 `permissions.can_publish` 是「editor 或 admin 而且是草稿的擁有者」（`created_by`，`addDashboard` 的 `createdBy`）：別人建的草稿代表分享的對話的草稿，讀得到、權限全是 false，發布得到 403 `dashboard.owner_required`；沒有分享的對話伺服器回 404，這個假伺服器不模擬，所以它不是伺服器行為的完整複本。端對端在 `docs/test-reports/2026-11-05-1049-answer-dashboards.md`。

### 提問

首頁的輸入框、對話頁底部的輸入框、等待回答、停止與錯誤（Laysi/trustiot-v2#1044；畫面規格與和設計稿不同的地方在 `docs/design/chat-dashboards/design.md` 的「輸入框與提問」）。API 是 Laysi/trustiot-v2#646 的 `POST /conversations`、`POST .../questions`、`POST .../runs/{run_id}/cancel` 與對話的 `latest_run`，都在 `src/api/conversations.ts`（`createConversation`、`askQuestion`、`stopQuestion`），型別由 `api/openapi.json` 產生的 client 提供。

- **提問在背景跑**：`POST .../questions` 立刻回 202 與 run，問題與回答在回答完成時才一起存起來；頁面每 `QUESTION_POLL_MS`（1.5 秒，`ConversationPage.tsx`）重讀對話，直到 `latest_run.status` 不是 `running`。同時開著這個問題的回答串流，回答邊寫邊顯示（見下面的「回答的串流」）。在另一個分頁或重新整理後看得到已寫好的回答，看不到問題文字（它還沒存）。
- **誰持有狀態**：`src/chat/Composer.tsx` 只畫輸入框與按鍵，沒有自己的狀態。`HomeComposer.tsx`（開始對話、提問、打開對話頁）與 `ConversationTurn.tsx`（跟著一個 run 到它結束）各自持有文字、進行中的請求與錯誤，所以送出失敗時文字留在輸入框。首頁把問題文字與 run ID 放進路由的 state（`src/chat/questions.ts` 的 `AskedState`，zod 解析）帶到對話頁；沒有這個 state 時（重新整理、別的分頁）頁面看 `latest_run`，是 `running` 就跟著它。
- **結束沒有回答**：停止、失敗與中斷的一輪不在歷史裡，所以問題放回輸入框（輸入框是空的才放）。顯示的訊息是 `latest_run.message_key`（`question.<種類>`，翻譯在 `src/i18n/locales/`）；前端不認得的鍵用 `question.failed`，從不顯示原始的鍵或 `result`。送不出去的原因是 API 的錯誤（`askFailureReason`：欄位 `/question` 的訊息，否則錯誤本身的訊息）。
- **網址帶入問題**：`?q=`（`newConversationPath`、`useQuestionFromAddress`）。`StartInConversation` 是儀表板列表與儀表板頁的入口（檢視者、封存的儀表板與草稿沒有）。
- **等待中的問題留在看得到的地方**（Laysi/trustiot-v2#1182）：問題送出（或打開時已有問題在跑）時，等待中的列表（「正在回答的問題」）以 `scrollIntoView({ block: "nearest" })` 捲到頂端列與輸入框之間：頁面捲動容器的 `scroll-padding` 扣掉兩者的實際高度（見「應用程式外框」的「黏住的列」），列表自己的 `scroll-margin`（`scroll-my-4`）再留一點空白。回答串流時，讀者在底部（列表下緣離輸入框上緣 32 px 以內，`AT_BOTTOM_PX`）就跟著新的段落捲（`block: "end"`）；讀者往上捲離開底部就停止跟隨，回到底部（滾輪、End 鍵或拖捲軸都算）就恢復。手指還在螢幕上或滾輪停下不到 200 ms 時不捲，免得和讀者搶；偏好減少動態效果（`prefers-reduced-motion: reduce`）時捲動是瞬間的，否則是平滑的。決定在 `src/pages/conversations/follow.ts`（純函式，`follow.test.ts` 測跟隨、停止、恢復、讀者捲動中與減少動態效果），DOM 那一側（量測、事件、`ResizeObserver` 看列表與輸入框的大小變化）在 `useFollowNewest.ts`。瀏覽器實測在 `docs/test-reports/2026-11-07-1182-follow-up-scroll.md`。
- **測試用的替身**：`src/test/fake-questions.ts` 的 `FakeQuestionServer` 在 `FakeConversationServer` 上加這幾個端點與回答的串流，回應照 `openapi.json`；一個 run 停在 `running`，直到測試呼叫 `answer`、`fail`、`interrupt` 或（要求停止之後）`settleStop`。`write` 在 run 進行中送出一段回答（服務的部分回答），`cutStreams` 以 200 結束每個開著的串流（代理切斷），`stallStreams` 不再送資料但連線開著，`streamOpens` 記下每次開啟與它的 `Last-Event-ID`。它也是 UI 預覽的項目（`preview`；對話路徑只有它一個擁有者，`fake-conversations.ts` 只提供預覽的資料 `seedPreviewConversations`）：預覽的輸入框裡，含 `!fail`、`!slow`、`!limit`、`!denied` 的問題分別是模型錯誤、一直跑到被停止、到達同時提問的上限與被拒絕，其他問題幾秒後得到範例回答；每個問題在一半的時間先串流出範例回答的第一段。

### 回答的串流

Laysi/trustiot-v2#1046：對話頁跟著一個正在回答的問題時，開著它的回答串流（`GET .../runs/{run_id}/stream`，Laysi/trustiot-v2#147，協定見 [services/assistant](../services/assistant/README.md)「回答的串流」），回答邊寫邊顯示在問題下方。程式：`src/api/answer-stream.ts`（`followAnswer`：讀事件、組回答、續播）、`src/pages/conversations/useAnswerStream.ts`（頁面的 hook）、`src/chat/StreamingAnswer.tsx`（畫面）；`ConversationApi.streamAnswer` 經 `openEventStream`（見「SSE」）。

- **顯示**：收到第一段之前是「正在查詢讀數…」；之後是 TrustIoT 的標誌、已收到的回答（可近用名稱「TrustIoT 正在寫的回答」，`aria-busy`）與一行「正在寫回答…」（`role="status"`）。回答與完成的回答一樣經 assistant-ui 的 message part 與 `MarkdownText` 畫（同一套「回答的 Markdown 是不可信的」規則），沒有另一個 renderer；它有自己的 runtime，只放這一則，所以 `Thread.tsx` 在串流時不變。問題答完時，串列裡存下的回答取代它。
- **只顯示完整的段落**：服務在提問進行中只送寫完的段落（去掉結尾的空白，下一段的空行跟著下一段來），長的文字（續播、存下的回答的其餘部分）則切成每段最多 4,096 個 UTF-16 code unit。API 的契約（`api/openapi.json` 的 `AnswerStreamText`，服務的 README「回答的串流」）保證只在這個大小切：比 4,095 短的一段結束在服務送出它時有的文字的結尾。所以最後收到的一段比切的大小短時，就是服務當時有的全部，整個顯示；剛好是切的大小（4,095 或 4,096）時，可能切在段落中間，只顯示到最後一個空行之前（`shownAnswer`），未完成的 `**` 或半個表格不會短暫顯示成壞掉的 Markdown；提問答完（`end` 是 `answered`）就顯示全部。`TEXT_PIECE_UNITS` 與契約的數字由 `src/api/answer-stream.test.ts` 對 `api/openapi.json` 檢查，任一邊改了測試就失敗。
- **位置與重複**：`text` 事件的 `offset` 是這段在回答裡的開始位置（UTF-16 code unit，就是瀏覽器字串的長度）。頁面只接上超出自己結尾的部分，所以重新連線後與已有內容重疊的段落不會重複；開始位置超過自己結尾的段落（中間會缺一塊）或解析不了的事件，從頁面自己的位置重新開啟串流（連續最多 `MAX_RESYNCS` 次，收到新的文字就重新計算，所以網路不穩時長的回答也不會停止串流；解析失敗只以 `console.warn` 記事件類型與 issue 路徑，不記值）。
- **斷線續播**：`openEventStream` 在 75 秒沒有任何資料（伺服器預設每 20 秒一個 heartbeat）或回應以 200 結束卻沒有 `end`（Cloudflare 或代理切斷）時，帶最後一個 `id` 當 `Last-Event-ID` 重新連線，從那裡接著收。服務拒絕這個位置（400 `stream.resume_invalid`）時，丟掉已收到的內容、從回答開頭重新開一次；再被拒，或 404（對話已刪除、不是自己的）時停止串流，頁面照常靠 `latest_run` 結束這個問題。429、503 與其他錯誤照「SSE」的規則重試。
- **重新整理與重新開啟**：頁面不記住收到過的位置，每次掛載都從回答開頭開啟（不帶 `Last-Event-ID`），服務先送已寫好的部分，再接著送之後寫的。
- **結束**：`end` 是 `answered` 時顯示全部回答，並立刻重讀對話（不等下一次輪詢），存下的回答出現在串列；不是 `answered`（停止、失敗、中斷）時丟掉已收到的段落，訊息與問題放回輸入框照上面「提問」的規則。另一個分頁開始的較新的問題取代這一個時，舊的串流關閉，頁面跟著新的。
- **兩種模式**：服務的回答工具開關（`streams.answer_tool`）關閉時，提問進行中沒有任何段落，畫面一直是等待的那一行，答完時整個回答一次到（同一個串流，在 `end` 之前）；頁面不必知道開關。
- **輪詢仍在**：`latest_run` 的輪詢（1.5 秒）不變：停止中的狀態、另一個分頁的提問與串流停止時的結束都靠它；串流只負責回答的內容與提早知道結束。
- **測試**：`src/api/answer-stream.test.ts` 以手動時鐘測 heartbeat 逾時後帶 `Last-Event-ID` 重新連線、以 200 被切斷、重疊與缺口、解析不了的事件、停止、失敗與中斷、重新開啟頁面、400 與 404、缺口之間有新文字時重開的次數重新計算、沒有新文字就關閉的串流等待倍增到 30 秒，以及切的大小與 `api/openapi.json` 的契約相同；`src/pages/conversations/ConversationStream.test.tsx` 測頁面的每個狀態，包括串流開著時離開頁面：請求被中止、串流的計時器都清掉。瀏覽器端對端的驗證在 `docs/test-reports/2026-11-05-1046-answer-streaming.md`。

### 分享的對話（Laysi/trustiot-v2#1048，畫面在 `design.md`「分享給整個租戶」）

- **API**：`src/api/conversations.ts` 的 `shareConversation`、`unshareConversation`、`listShared`、`getShared` 與 `copyConversation`，與其他對話端點一樣綁 `api/openapi.json` 產生的型別（`Shared*` 的 schema 與 `Equal` 檢查）。對話自己有 `shared_at`，所以擁有者的分享狀態不另外查。
- **擁有者**：`pages/conversations/sharing/ShareControl.tsx` 是對話頁標題列的分享鈕與兩種確認（分享、取消分享）；兩種都寫明「取消分享或刪除不會收回副本」。
- **讀者**：對話紀錄的第四個分頁 `?tab=shared`（`SharedConversationsPage.tsx`，與其他分頁共用 `HistoryTabs.tsx`）與唯讀頁 `/shared-conversations/:conversationId`（`SharedConversationPage.tsx`，路由在 `src/app/routes.tsx`，選單的「對話紀錄」是它的目前項目）。唯讀頁用 `chat/Thread.tsx`（同一套安全的 Markdown），`linkBack={false}`：回答裡的連結不帶來源對話，因為那會回到讀者打不開的路徑。唯讀頁不畫擁有者的任何動作。對話的儀表板草稿（API 對讀者開放：`GET /conversations/{id}/dashboards`，`permissions` 全是 false，資料集以讀者的身分執行）由 `DraftPanel` 的 `readOnly` 模式畫在對話旁，同一個 `VersionItems`：不論讀者的角色，都沒有「儲存到儀表板列表」、發布或修改，只有一則唯讀的說明。唯讀頁不畫回答下面的儀表板（`renderAnswerExtras` 不給：那裡有「儲存到儀表板列表」，是擁有者的動作），沒有輸入框、停止與續傳，不讀擁有者的對話、問題、run 與回答串流端點；只包一層 `ConversationDashboards`（草稿面板的儲存記憶與資料集的並行上限），測試在 `SharedConversations.test.tsx` 的「the reader's page offers nothing of the owner's answers」。
- **測試**：`ShareControl.test.tsx` 與 `SharedConversations.test.tsx`，用 `src/test/fake-conversations.ts`：它依服務的規則（`services/assistant/src/api/sharing.ts`）處理分享、取消分享、「分享給我的」、唯讀檢視與複製，`add({ owner })` 加另一位成員的對話，`api(user)` 是那位成員的 API。UI 預覽的 `preview` 在 `fake-questions.ts`（對話路徑的唯一擁有者）：它多宣告 `/api/assistant/v1/shared-conversations` 這個前綴，並呼叫 `fake-conversations.ts` 的 `seedSharedPreviewConversations`（三段別人分享的對話與一段自己已分享的；路由是假服務類別的），所以「分享給我的」與唯讀頁在 `bun run --cwd frontend ui-preview` 裡可以點，複製也會出現在自己的對話紀錄，複製後的對話頁有輸入框；`fake-questions.ts` 的 `fetchAs(user, …)` 讓 `api(user)` 也走提問的端點，提問與停止只對擁有者的對話有效。預覽的假服務不回答 `/conversations/{id}/dashboards`，所以預覽裡兩種對話頁都沒有草稿面板。這些測試用 `src/test/until.ts` 等條件：RTL 的 `waitFor` 在等一個被擋住的請求放行時曾卡住事件迴圈。
- **這幾頁的瀏覽器檢查**（真正的 API、兩位成員、CSP 與 Trusted Types）在 `docs/test-reports/2026-11-04-1048-share-conversation.md`。

### 重新產生最後一則回答（Laysi/trustiot-v2#1047，畫面在 `design.md`「重新產生最後一則回答」）

**正式的對話頁就有這個按鈕**（後端是 Laysi/trustiot-v2#649，PR Laysi/trustiot-v2#1083 已合併，端點與訊息的 `seq` 都在 `api/openapi.json`）：

- **介面**：`src/api/conversations.ts` 的 `regenerateAnswer(conversationId, answer)`，與其他對話端點一樣走產生的 client（`pinnable`，請求帶頁面的使用者與租戶）。請求本文正好是 `{ "answer": <seq> }`，回 202 與 `ConversationRun`；`OwnMessageSchema` 的 `seq` 是必填，`Equal` 檢查對整個 `ConversationDetail`。
- **頁面**：`pages/conversations/regenerate.tsx` 的 `useRegeneration(conversation)` 用 `useConversationApi()`，沒有另外的 provider 或開關。分享的唯讀頁不用 `useRegeneration`，而且分享的 API 沒有 `seq`，所以不會有按鈕。UI 預覽走同一條路：請求經頁面的 `fetch` 到 `fake-questions.ts`（對話路徑的假服務，regenerate 路由已經是 HTTP 層的）。
- **按鈕**：只在最後一則回答的動作列（`Thread` 的 `renderAnswerActions`，在「複製回答」旁邊），條件是這則回答有 `seq`、前面有問題、沒有問題在回答（`ConversationTurn` 回報的 `answering` 與 `latest_run` 不是 `running`），而且不是這個頁面上一次失敗或被停止的重新產生留下來的（見下）。送出中按鈕停用並寫「正在重新產生…」，所以連按不會送兩次。
- **指名回答**：請求帶的是頁面最後一次讀到的最後一個回答的 `seq`。409 `question.regenerate_stale`（重試、重複按、過時的分頁）：頁面重讀對話、顯示現在的狀態與一則 `role="status"` 的說明，**從不**以新的位置自動重送；新的最後一個回答有它自己的按鈕，由使用者再按。
- **接上提問的流程**：202 之後頁面先重讀對話（舊的一輪已經捨棄），再把 run 與問題文字交給 `ConversationTurn`（`useRegenerationHandoff`），它像一般的提問那樣顯示問題與串流、可以停止。失敗、中斷或被停止時，舊的回答不會回來，對話停在前一輪：問題文字放回輸入框（既有的規則），訊息多一句「你要取代的回答已移除…」，**不再提供**重新產生：頁面記得它送過的 `seq`，最後一個回答的 `seq` 比它小時（那是更早一輪的回答，再重新產生會捨棄那一輪）按鈕不畫，直到有更新的回答（`seq` 更大）出現，例如使用者把問題重新送出且答完。重新整理頁面會忘掉這件事，這時最後一個回答就是一個普通的、使用者看得到的最後一則回答。
- **回應是錯誤但已經捨棄**（身分服務沒有建立委派：401、403、503、504，README「重新產生之後，舊的回答不會回來」）：頁面重讀對話，指名的回答不見了就把問題放回輸入框並說明；還在就只顯示原因，按鈕照舊可用。
- **測試**：`ConversationRegenerate.test.tsx`（只在最後一則、位置、產生中看不到、連按、`seq` 過時與不重送、失敗與停止後只能重新提問、捨棄後的錯誤、分享的唯讀頁）。`fake-questions.ts` 的 `FakeQuestionServer` 多了訊息的 `seq`（只在自己的對話的讀取裡：唯一、遞增、不重複使用、有缺號）與 `regenerate` 端點（`{answer}`、過時的 409、捨棄最後一輪再開始一個 run，`failNextOn("regenerate", …)`、`holdAsks`）。

## 程式碼切分與載入量測

2026-11-02 量測並決定（Laysi/trustiot-v2#948；方法、數字與理由在 [測試報告](../docs/test-reports/2026-11-02-948-bundle-size-load-time.md)）。規則：

- **路由層級切分**：設定頁與設備頁以 `src/app/routes.tsx` 的 `lazyPage()`（React Router 的 `lazy`）載入，各自一個 chunk，不進每個訪客都下載的主 bundle。新增的頁面如果有自己的重量（圖表、編輯器、大型表單），一樣用 `lazyPage()`；很小的頁面（首頁、找不到頁面）留在主 bundle。登入頁與版面不切：量測顯示切開它們省下的位元組，剛好被多出的一次往返吃掉（報告「選項與決定」）。
- **固定的 `framework` chunk**（2026-11-03，Laysi/trustiot-v2#977，[測試報告](../docs/test-reports/2026-11-03-977-framework-chunk.md)）：`vite.config.ts` 的 `FRAMEWORK_PACKAGES`（react、react-dom、scheduler、react-router、TanStack Query、zod、openapi-fetch、tailwind-merge）以 `codeSplitting` 群組進一個 chunk。檔名只含內容雜湊，這些套件只在升級相依時才變，所以只改應用程式碼的部署不會換掉它，快取著它的瀏覽器只重新下載應用程式的 chunk：再訪的 Brotli 下載量從 204 至 210 KB 降到 68 至 73 KB，冷、熱快取的 ready 沒有格子變慢超過容許（代價是多兩個請求）。隨介面變動的套件（lucide-react、Radix）不放進去，否則加一個圖示就換掉它的檔名。新增大型且少變動的相依套件時，先量再決定要不要加進 `FRAMEWORK_PACKAGES`。`src/app/framework-chunk.test.ts` 證明只改應用程式碼的兩次建置，framework chunk 的檔名相同。
- **第一次進入切出的頁面**：路由先載入 chunk，期間畫面顯示 `PageLoading`（`HydrateFallback`）；chunk 載入失敗時由該路由的 `errorElement` 顯示錯誤頁。
- **儀表板的圖表套件**（ECharts，見「儀表板元件的圖表套件」）以 lazy import 載入，進它自己的 chunk；它在 Laysi/trustiot-v2#895 加入後才有 chunk 可量，量測時記下它的大小與載入時間。
- **舊 chunk 不存在**：見「CSP 的限制」之後的 `vite:preloadError` 處理。`src/app/preload-chunks.test.ts` 以真正的建置檢查：每個動態 `import()` 都包在 `__vitePreload` 裡、該 helper 會送出 `vite:preloadError` 事件、事件的處理程式（`preload-error.ts`）在同一份建置裡。
- **量載入時間**：`tools/measure-load.ts` 經 `services/web`（正式的標頭與快取）和 headless Chromium 的網路節流（CDP 的 `Network.emulateNetworkConditions`），量登入頁與登入後的頁面，冷與熱快取各一次，也量部署新版後再訪的載入；`--stale-chunk` 檢查舊分頁碰到被移除的 chunk 時的行為。用法、參數與限制寫在檔案開頭的註解，執行的完整指令在測試報告。

`services/web` 不壓縮回應（`Content-Encoding` 由入口或 CDN 加），所以直連時使用者下載的是未壓縮的 JavaScript；量測用 `--compress none|gzip|br` 分別看直連與有壓縮的入口，報告列出兩者。

## 多語系

zh-TW 與 en 兩種語言，字串在 `src/i18n/locales/zh-TW.json` 與 `en.json`。程式裡只用 `t("<鍵>")`，鍵的格式與伺服器的 MessageKey 相同（`^[a-z0-9_]+(\.[a-z0-9_]+)+$`）。`bun run --cwd frontend test` 檢查：

- 兩種語言的鍵完全相同、沒有空字串、每個鍵的 placeholder（`{max}` 這種）相同。
- 程式裡每個 `t("…")` 的鍵都存在。
- 伺服器能送出的每個 message key 都有翻譯：掃描 `crates/contract/src` 與 `services/*/src` 裡的 `MessageKey::new("…")` 與 `message_key!("…")`（註解與 `#[cfg(test)]` 模組除外），以及 `api/openapi.json` 的每個錯誤碼。

所以服務新增 message key 時，zh-TW 與 en 的翻譯要在同一個 PR 加進這兩個檔案，否則 CI 的 `frontend` job 失敗（這些 Rust 檔案的變動會觸發它，見 `scripts/ci/changes.ts`）。伺服器的錯誤一律帶 `message_key` 與 `details`，前端只依 `message_key` 翻譯，`details` 填入 placeholder；不認得的鍵改用錯誤碼的預設訊息，沒有 M1 錯誤內容時依 HTTP 狀態選訊息（`src/i18n/errors.ts`）。

日期與數字以 `Intl` 依目前的語言與時區格式化（`src/i18n/format.ts`）。

## API client

型別由 `api/openapi.json` 產生到 `src/api/schema.gen.ts`（進版控，不手改）。規格由 `cargo run -p openapi` 從 Rust 程式碼產生，所以改了 API 型別的 PR 要依序：`cargo run -p openapi`、`bun run --cwd frontend api:generate`，兩個產物一起提交；`api:check` 與 rust job 的規格比對會擋下漏掉的一步。

`src/api/client.ts` 的 client：

- 只打同源（`baseUrl` 是空字串）、`credentials: "same-origin"`，靠 session cookie 驗證。
- 寫入（GET、HEAD、OPTIONS 以外）一律帶 `Content-Type: application/json`：身分服務以 cookie 驗證的寫入要求 JSON（簡單表單無法跨站送出），並另外檢查 `Origin` 與 `Sec-Fetch-Site`，跨站的寫入以 `security.cross_site_request` 拒絕。
- `unwrap(schema, client.GET(...))` 以 zod schema 解析成功回應，回傳 schema 的輸出型別，沒有 `as` 轉型；沒有本文的回應（204）用 `z.undefined()`。2xx 的本文不符合 schema 時丟出 `ResponseShapeError`（不是 `ApiError`：伺服器沒有回報失敗，再問一次答案也一樣），`classifyError` 歸為 `other`，查詢不重試；錯誤訊息與 `issues` 只有成員的路徑（JSON Pointer）與 zod 的 issue 代碼，不含收到的值，因為裡面可能有租戶資料。
- `createApiClient({ shownSession })` 以兩個標頭帶畫面顯示的對象，還沒有工作階段時都不帶（`expectedSessionHeaders`）：`trustiot-expected-user` 是使用者 ID，讀工作階段、登入與登出這個分頁（`USERLESS_PATHS`）以外的請求都帶，登出所有裝置也帶；`trustiot-expected-tenant` 是租戶 ID，不在任何租戶時是 `none`，讀工作階段、切換租戶、登入登出、偏好設定（`SESSION_PATHS`）與平台管理的端點（`isPlatformPath`）不帶。標頭的值是發出請求的頁面掛載時的使用者與租戶：`Layout` 與 `TenantGate` 以 `PinSession`（`src/session/PinSession.tsx`）在重新掛載頁面的邊界上固定它們，`useIdentityApi()` 與 `useDeviceApi()` 回的是綁在頁面上的副本（`pinSession`），所以頁面在 `await` 之後或 mutation 的下一步才送出的請求，帶的仍是頁面自己的使用者與租戶。這要 API 物件以 `pinnable(client, ...)` 建立（`identityApi`、`deviceApi`）；不是這樣建立的物件（測試的假物件）原樣使用。頁面以外的請求（`SessionProvider` 的切換與登出、偏好設定）帶送出當下快取裡的工作階段（`src/app/start.tsx` 的 `shownSession(queryClient)`）。新的頁面與 mutation 不必做任何事（見「多個分頁」）。SSE 不經這個 client，所以 `openEventStream` 有必填的 `session`（頁面以 `useStreamSession()` 取得，同樣帶頁面固定的使用者與租戶），每次連線（含重連）都帶兩個標頭，被拒絕（409）時通知 `SessionProvider` 重讀工作階段；不關於工作階段的串流要明確傳 `null`，所以租戶資料的串流不會漏掉。其他直接以 `fetch` 送出的請求要自己帶。
- `unwrap()` 把錯誤轉成 `ApiError`：狀態碼、解析後的 M1 錯誤內容，網路錯誤的狀態碼是 0；`retryable` 只對 `rate_limited`、`unavailable`、`timeout`（沒有內容時 429、503、504）與網路錯誤成立。查詢最多重試 3 次，寫入不重試。

### 外部資料一律經 zod 解析

API 回應、錯誤本文、SSE 事件資料與瀏覽器儲存（`localStorage`）的內容都是外部資料：型別只在編譯期成立，執行時什麼都可能進來（滾動更新時前後端版本不一致、舊頁面還開著、入口回了不是 M1 格式的內容）。所以它們一律以 zod schema 解析後才使用，不用 `as` 轉型（`JSON.parse(...) as X` 也不行）。

- **schema 與 `schema.gen.ts` 雙向綁定**：手寫的 schema 旁邊用 `src/lib/type-binding.ts` 的 `Assert<Equal<z.output<typeof S>, components["schemas"]["S"]>>` 綁定產生的型別，規格一變 `typecheck` 就失敗。只用 `satisfies z.ZodType<T>` 抓不到規格刪掉的欄位。`src/api/errors.ts` 是範例；`ErrorBody` 的 `details` 刻意比規格窄（只留字串與有限數字），所以該欄位只檢查可指派，其餘欄位仍用 `Equal`。`tools/api-client.ts` 傳 `emptyObjectsUnknown: true`：沒有 properties 的 object（`details`）產生成 `Record<string, unknown>`，不是「不能有任何鍵」的 `Record<string, never>`，再由 schema 收窄。
- **邊界的寬鬆行為**：錯誤本文與儲存的偏好都不用 `z.strictObject`，未知欄位忽略（M1 規定新欄位只以選填的方式加入）；不合格的 `details` 項目與 `field_errors` 項目丟掉，不讓整個本文失敗；儲存的偏好每個欄位以 `.catch()` 退回自己的預設值，版本欄位 `v` 不是 2 時回傳預設值。身分服務的回應（工作階段、偏好、登入）則嚴格解析：例如不認得的角色是契約的改變，以 `ResponseShapeError` 呈現，不默默當成別的角色。
- **SSE**：`sse.ts` 目前只交出 `data` 字串；串流畫面（Laysi/trustiot-v2#149 決定是否在 `sse.ts` 提供共用介面）自己以 schema 解析，解析失敗的事件丟掉並以 `console.warn` 記下事件類型與 issue 路徑（不記值），串流照常繼續。回答的串流（`src/api/answer-stream.ts`）的 `text` 綁定 `AnswerStreamText`、`end` 用 `ConversationRun` 的 schema；解析失敗時從頁面自己的位置重新開啟（見「回答的串流」）。

## SSE

即時圖表與 AI 回答的串流用 `src/api/sse.ts` 的 `openEventStream`（以 fetch 讀 `text/event-stream`，不用 `EventSource`，因為它看不到註解行與狀態碼）。與伺服器的約定：

- Cloudflare 在 125 秒沒有資料時切斷回應，狀態碼仍是 200，所以伺服器至少每 30 秒送一次 heartbeat（註解行即可）。
- 前端 75 秒沒收到任何資料就當作斷線，與串流結束一樣處理：帶 `Last-Event-ID` 重新連線，讓伺服器接續。
- 重新連線的等待從 1 秒倍增到 30 秒，加上隨機的抖動；伺服器的 `retry:` 是下限。連線讓串流有進展（`progressed`：預設是 `heartbeat` 以外的事件；回答的串流是收到新的文字），或開著至少 `HEALTHY_CONNECTION_MS`（30 秒，與最長的等待相同）時，下一次重新從 1 秒算；其他的連線（例如收到第一個 heartbeat 就被關掉）算失敗，等待繼續倍增，所以一直立刻關閉串流的伺服器，在等待到頂之後每 15 到 30 秒被問一次（最長的等待一半固定、一半隨機）。
- 401、403 停止重連並回報狀態；409（`session_changed`：另一個分頁已切換租戶或登入成另一個帳號，見「多個分頁」）也停止，狀態是 `session_changed`，並經 `session.changed` 讓工作階段重讀；串流自己的 `stopOn` 列出的狀態碼（回答的串流是 400 與 404）停止在 `refused`，並以 `onRefused` 告訴呼叫者是哪一個；其他狀態碼或不是 `text/event-stream` 的回應照樣重試。
- `lastEventId` 是第一次連線就帶的 `Last-Event-ID`（之後由事件的 `id` 取代；`id:` 在它的事件交出時才生效，所以在一個事件中間斷線不會從還沒收到的位置續播；比 WHATWG 嚴格的是，沒有 `data` 的 frame（不是事件）裡的 `id:` 也不單獨生效，續播的位置只隨事件前進），回答的串流從頁面自己的位置重新開啟時用它。處理事件的程式關閉串流之後，同一塊資料裡其餘的事件不再交給它。

計時器經由可注入的 `Clock`，測試以手動時鐘推進，不等待真實時間（`src/test/event-streams.ts` 的 `ManualClock`、可寫入的串流本文與依序回應的 fetch，`sse.test.ts` 與 `answer-stream.test.ts` 共用）。

## 登入與登出

登入只有 email 驗證碼一種（決策 13，[身分服務](../services/identity/README.md#登入)「登入」），在 `src/pages/LoginPage.tsx`：

1. 輸入 email，`POST /auth/login` 的 `send_code`。不論地址有沒有帳號，伺服器的回應都一樣，畫面也只說「如果這個地址有帳號，已寄出驗證碼」。空白的地址在送出前就擋下；伺服器的 422 欄位錯誤顯示在 email 欄位，其他錯誤（503 寄不出信等）顯示在表單上方。
2. 輸入 8 位數驗證碼（長度取自回應的 `code_length`；貼上時只留數字），`verify_code` 帶 `redirect_to`（網址列的 `next`，經 `safeNextPath` 檢查）。401（驗證碼錯誤或失效）與 `/code` 的 422 顯示在驗證碼欄位，`/redirect_to` 的 422 與其他錯誤顯示在表單上方。可以在回應的 `resend_after_secs` 之後重新寄送，或改用其他地址。
3. 成功後重新讀取工作階段，等畫面讀到新的工作階段再以前端路由前往伺服器檢查過、這裡又檢查一次的 `redirect_to`；不交給瀏覽器轉址。讀不到工作階段（瀏覽器沒有保留 cookie）時說明要允許 cookie。

登入頁的語言選單只改瀏覽器裡的副本，登入後以帳號的設定為準。

登出在帳號選單（`DELETE /api/identity/v1/sessions/current`；工作階段已經結束時也算成功）；登出所有裝置在個人設定頁「已登入的裝置」，先以對話框確認（`DELETE /api/identity/v1/sessions`，失敗時在對話框內說明原因）。兩者成功後都先換到登入頁、顯示已登出的提示，再清除快取，所以版面不會以「沒有工作階段」再導向一次登入頁。

登出所有裝置收到 401 表示這個工作階段已經結束，請求什麼都沒有結束，其他裝置仍然登入著。這個請求自己處理 401（`handlesUnauthenticated`），不走通用的導向：它導向 `/login?next=%2Fsettings`，history state 帶 `signOutEverywhereUnfinished`（與 `signedOut` 一樣經 zod 解析），登入頁在兩個步驟都提示「其他裝置還沒登出，登入後請再按一次」，不顯示「你已登出所有裝置」；登入後把同樣的 state 帶到 `/settings`，「已登入的裝置」再提示一次（`src/session/useSignOut.ts`）。否則使用者懷疑工作階段被盜而按下時，若自己的工作階段剛好過期，重新登入後會以為已經成功。

## 偏好設定、租戶與工作階段

- **工作階段**：`SessionProvider` 以 `identityApi`（`src/api/identity.ts`）讀 `GET /api/identity/v1/sessions/current`，401 表示沒有登入（不是錯誤）。身分服務的端點經 `useIdentityApi()` 取得，測試以 `src/test/fake-identity.ts` 的假服務注入。
- **切換租戶**：`PUT /api/identity/v1/sessions/current/tenant`，回應同前者。頁首的租戶選單與租戶清單（選租戶頁、停權頁）共用 `SessionProvider` 的 `switching`：任何一處在切換時，每個入口都停用。切換的 mutation 有 `scope`，一次只送一個、依發出的順序；回應在 mutation 的 `onSuccess` 寫進快取，下一個切換要等它完成才送出，所以伺服器依同樣的順序處理，快取裡的目前租戶就是伺服器最後的回應（否則先發的切換回應晚到時，會蓋掉伺服器實際所在的租戶）。切換時先取消租戶的進行中請求與工作階段的讀取，再把租戶的查詢全部重設：還開著的頁面會為新租戶重新查詢，不會短暫顯示前一個租戶的資料。工作階段與偏好屬於使用者、不屬於租戶，所以保留；偏好的讀取也不取消，否則第一次讀取被取消後會一直停在載入中（它永不過期，沒有東西會再查詢它；`src/session/TenantSwitcher.test.tsx` 測這個情況）。不用 `queryClient.clear()`：它移除查詢後，還在畫面上的元件仍拿著被移除的查詢，畫面停在舊租戶（測試抓到的問題）。分頁標題是「<租戶名稱> · TrustIoT」。
- **沒有目前租戶**（第一次登入，或已不是原本租戶的成員）：`TenantGate` 在使用者只屬於一個租戶時自動切換過去（失敗時說明並可重試），屬於多個時列出讓他選，一個都沒有時說明為什麼沒有內容，可以重新檢查或登出。
- **停權**：目前的租戶有兩個來源會被視為停權：工作階段回應裡它的 `state` 不是 `active`（`GET /api/identity/v1/sessions/current` 每個租戶都帶 `state`，所以首頁沒有任何租戶範圍的請求時也看得到，切換到已停權的租戶也一樣，不回錯誤），或任何請求收到 `tenant_suspended`（403）；兩者都由 `SessionProvider` 判斷，`TenantGate` 以停權頁取代它的頁面，列出使用者其他的租戶可以切換，或登出；切換租戶本身被拒絕時是那次切換的錯誤，不算停權。取代舊系統每 60 秒檢查一次的做法。
- **偏好設定**（介面語言、AI 回答的語言、時區、主題）存在帳號裡，`GET`／`PATCH /api/identity/v1/preferences`，以伺服器的值為準，每個登入的瀏覽器都一樣。前端的欄位與 API 的值一對一（`auto`、`zh-TW`、`en`；`auto` 或 IANA 名稱；`system`、`light`、`dark`），只在套用時解析 `auto` 與 `system`（`src/preferences/preferences.ts`），所以顯示的與儲存的不會不一致。
  - 個人設定頁每改一個欄位就送出只含那個欄位的 `PATCH`，立刻套用；伺服器的回應就是之後的值。一次只送一個，依修改的順序；還在儲存的修改顯示在較早的回應之上。伺服器拒絕時退回儲存的值，422 的欄位錯誤顯示在對應的欄位，其他錯誤也顯示在那個欄位；錯誤保留的是錯誤本身，換語言時重新翻譯。讀不到伺服器的偏好時不能修改，可以重試。
  - 瀏覽器的 `localStorage`（`trustiot.preferences`，格式版本 2）只是暫時的副本：登入前與伺服器回應前的畫面用它，伺服器每次回應都寫回去，登出後的登入頁也從它開始。讀寫失敗（隱私模式、配額）時用預設值而不出錯。預設值與伺服器相同（語言、AI 語言、時區都是 `auto`，主題 `system`）；`auto` 語言取瀏覽器語言清單中第一個 zh 或 en，都沒有時用 en。

## 多個分頁

同一個瀏覽器的分頁共用 session cookie，也就共用工作階段的使用者與目前的租戶（Laysi/trustiot-v2#678，決策 35）：在分頁 B 切換租戶、登出或以另一個帳號登入，分頁 A 的工作階段也跟著改變，但分頁 A 的畫面還是之前的樣子。若不處理，分頁 A 從前一個租戶的畫面送出的寫入，會以新的租戶執行；前一個使用者的畫面送出的寫入，會以新的使用者身分執行（兩個帳號在同一個租戶時，只比對租戶也擋不住）。

做法分兩層，第一層保證正確，第二層讓分頁早點知道：

1. **伺服器端的比對（保證不寫錯租戶或帳號）**：API client 以 `trustiot-expected-user` 與 `trustiot-expected-tenant` 帶畫面顯示的使用者與租戶（見「API client」）；入口的 cookie 檢查在任一個與工作階段的使用者或目前的租戶不同時回 409 `session_changed`，請求到不了任何服務（[身分服務](../services/identity/README.md)的「分頁顯示的使用者與租戶」）。標頭帶的是頁面掛載時的使用者與租戶（`PinSession`），不是送出當下的：分頁 A 在多步驟的 mutation 途中得知另一個分頁切換了租戶、改顯示新的租戶之後，前一個頁面的下一步仍帶前一個租戶而被拒，不會寫進新的租戶。`SessionProvider` 收到 `session_changed` 時重讀工作階段（自己正在切換、登入或登出時，等那次變更結束：它本身被拒時才重讀，否則那次變更本來就會重設頁面）。這一層不依賴瀏覽器的任何同步機制：分頁凍結在背景、瀏覽器沒有 `BroadcastChannel`，或通知在使用者按下送出之後才到，寫入都會被擋下。
2. **瀏覽器端的同步（讓分頁早點知道）**：分頁在自己切換租戶、登入或登出之後，經 `BroadcastChannel`（`trustiot.session`，`src/session/session-channel.ts`）通知同一個瀏覽器的其他分頁；訊息不帶任何內容，收到的分頁只重讀工作階段，所以偽造或亂掉的訊息最多造成一次讀取。分頁回到前景時也一律重讀工作階段（`refetchOnWindowFocus: "always"`），補上漏掉的通知。

重讀到的工作階段與畫面顯示的不同時（`SessionProvider` 的 `changedElsewhere`）：

- **另一個租戶**：取消並重設租戶的查詢（與自己切換相同），`TenantGate` 以租戶 ID 當 `Outlet` 的 key，頁面重新掛載，尚未送出的輸入一併捨棄，不會在新的租戶送出；版面上方說明「其他分頁已切換到某某組織」，可以關閉。工作階段離開了租戶（例如已不是成員）時說明這個租戶已關閉。
- **另一個使用者**：除了工作階段，前一個使用者快取的一切都移除，`Layout` 以使用者 ID 當 `Outlet` 的 key 重新掛載頁面，說明已改以另一個帳號登入。
- **已登出**：移除快取，`Layout` 導向登入頁。

自己造成的變更（切換、登入、登出）在發生的地方處理，不顯示提示；它進行時另一個分頁造成的變更，在它完成後一起比對。

取捨：沒有讓每個分頁各自持有工作階段（租戶放在網址或 `sessionStorage`、每個請求自己指定租戶），那要改工作階段模型與每個服務的授權；也沒有只做瀏覽器端的同步，因為它擋不住上面那些競態。標頭是選填的，所以只保護帶它們的請求（API client 與 SSE 串流）；入口的 istiod 要把這兩個標頭交給 cookie 檢查，沒有對齊時比對無聲地失效（[deploy/istio/README.md](../deploy/istio/README.md)）。登出這個分頁不帶使用者標頭：前一個使用者的分頁仍能結束這個瀏覽器目前的工作階段，這只會讓人少一個工作階段，刻意保留；登出所有裝置會撤銷使用者的每一個工作階段，所以帶使用者標頭。固定在頁面而不是在每個 mutation 開始時取快照：前者在 `Layout`、`TenantGate` 與 API 的 hook 一處做完，新的頁面不必記得；代價是 API 物件要以 `pinnable` 建立，偏好設定這類不在頁面裡的 mutation 帶送出當下的使用者。測試：`src/session/session-sync.test.tsx` 以兩個共用 cookie 的分頁測切換、寫入被擋（換租戶，同一個租戶裡換帳號，以及分頁已改顯示新的租戶之後 mutation 的下一步）、自己的切換因換帳號被拒、登出、換帳號、串流被擋與回到前景，`src/api/client.test.ts` 與 `src/api/sse.test.ts` 測標頭（包括兩種登出與綁在頁面上的副本）。

## 設備頁

`/devices/:deviceId`（`src/pages/device/`），放在 `TenantGate` 底下，從設備列表（見下）或網址進入。畫面照 [`docs/design/device-page/`](../docs/design/device-page/design.md)（Laysi/trustiot-v2#781；與設計稿不同之處列在該文件的「實作與設計稿不同之處」）：先回答「資料有沒有在進來」，再是現在的值與歷史走勢，只有出問題時才需要的設備資訊放最後。資料來源：

| 區塊 | API |
| --- | --- |
| 設備資訊（名稱、型號、閘道器、連線方式、取樣間隔、加入時間） | 資產服務 `GET /api/assets/v1/devices/{id}`，型號與資料點 `GET /api/assets/v1/device-models/{id}` |
| 資料狀態與目前讀數 | telemetry-query `GET /api/telemetry/v1/devices/{id}/latest`（只涵蓋原始資料的保存期間，目前 7 天） |
| 歷史圖表 | telemetry-query `GET /api/telemetry/v1/devices/{id}/series?point&from&to&tz`，不帶 `interval`，由伺服器挑出不超過 1500 個區間的最細聚合間隔 |

端點經 `useDeviceApi()`（`src/api/DeviceApiProvider.tsx`）取得，回應以 `src/api/devices.ts` 的 zod schema 解析，schema 與 `api/openapi.json` 的型別綁定。查詢的 key 都帶租戶（`src/pages/device/queries.ts`），切換租戶時一起重設。

- **檔案**：`DevicePage.tsx`（查詢、版面、網址）、`DeviceHeader.tsx`（頁首、狀態標籤、停滯橫幅）、`LatestCard.tsx`（目前讀數）、`HistoryCard.tsx`（範圍、統計、圖例、表格與各種說明狀態）、`HistoryChart.tsx` 與 `historyOption.ts`（圖）、`DeviceInfo.tsx`（設備資訊與兩種空狀態）、`health.ts`（停滯的判斷）、`chart.ts`（缺區間、統計、時段）、`time.ts`、`format.ts`。
- **網址**：`?point=<資料點>` 選圖表的資料點，`?range=1h|6h|24h|7d|30d|90d` 選最近一段時間，或以 `?from=<ISO>&to=<ISO>` 指定起訖；預設是 24 小時，不寫進網址。改選時以 `replace` 更新網址，可以直接分享或重新整理。不認得的值退回預設。「把這段設為範圍」把縮放的視窗寫成 `from`、`to`。
- **資料停滯**（2026-11-01 決定）：所有資料點最新的一筆讀數比 max(3 × 取樣間隔, 2 分鐘) 還舊就算停滯：頁首標籤、橫幅與該列讀數的時間改成警示色（`--warning`，圖示加文字，不只靠顏色）。只用現有 API，沒有健康 API（Laysi/trustiot-v2#454）。近 7 天沒有任何讀數是另一種狀態（還沒有讀數），不畫圖、不送歷史請求。頁面每 30 秒重讀時鐘（「N 秒前」與停滯的判斷），新讀數靠下一條的自動更新。
- **自動更新**（Laysi/trustiot-v2#1147，2026-11-07）：開著的頁面每 60 秒（`autoRefresh.ts` 的 `AUTO_REFRESH_MS`；閘道器每 60 秒取樣，維護者 2026-11-02 的決定）做一次與頁首「重新整理」鈕相同的更新（`DevicePage.tsx` 的 `reload`：範圍的結尾移到現在，重新查詢設備、型號、目前讀數與歷史），所以一個開著的設備頁每分鐘送出 4 個請求：設備、型號、`latest`、`series` 各一個（預設範圍的歷史以新的結尾查，舊結尾的那一筆不再查）。計時從上一次更新（範圍的結尾，手動更新、換範圍也會移動它）算起，不是從頁面打開算起。這些情況不更新：自訂範圍（網址的 `from`／`to`，是固定時段）；圖放大中（新的資料會讓放大回到整段範圍），回到整段範圍時若已超過一分鐘就立刻更新；頁面在背景（`document.visibilityState` 是 `hidden`），回到前景時同樣，超過一分鐘就立刻更新；設備本身還沒載入成功。不打擾正在看的人：歷史沿用 `placeholderData` 留著舊的線，沒有骨架、「更新中」字樣或忙碌狀態（這份安靜只到那次載入完成為止，之後讀者自己觸發的請求照常顯示）；「重新整理」鈕不停用、頁首的「更新時間」`aria-live="off"`，也就不朗讀；圖是同一個元素（`HistoryCard` 的 `ChartBlock` 以資料點與範圍種類為 key，不是以答案的起訖時間，後者每次更新都不同），所以焦點、tooltip、「以表格顯示」與選取的資料點都留著。失敗時畫面上的資料留著：設備與目前讀數沿用原有的錯誤說明（設備失敗時說明放在頁首下方，不換成錯誤頁），同一範圍的歷史重新整理失敗時，說明放在圖上方、圖留著（`kept`），一分鐘後照常再試。計時器經 `AutoRefreshClockContext` 取得（預設是真的 `setTimeout`），測試以 `src/test/manual-clock.ts` 的 `manualClock` 取代，不等真的一分鐘。
- **時區**：時間一律以使用者偏好的時區顯示，`tz` 也送這個時區，所以小時與日的區間對齊使用者的當地時間。自訂範圍的兩個 `datetime-local` 欄位是這個時區的牆上時間，換算時處理日光節約（跳過的時間往後移、重複的時間取第一次；`src/pages/device/time.ts`）。圖的 x 軸是儀表板元件的牆上時間（UTC 欄位），`historyOption.ts` 的 `toWall`／`fromWall` 在使用者時區與它之間換算。
- **沒有資料時一律說明原因與下一步**（清單 L3），不顯示空白圖表：

| 情況 | 畫面 |
| --- | --- |
| 範圍整段在可查詢期間之前（403 `telemetry.outside_queryable_period`） | 說明最早可查到哪個時間，按鈕改查「從最早到現在」 |
| 範圍開頭早於可查詢期間（`truncated`） | 照常畫圖，上方說明圖從哪裡開始 |
| 範圍內沒有讀值 | 說明沒有讀值；範圍短於 7 天時提供「最近 7 天」 |
| 設備從沒存過讀值（近 7 天） | 主欄的空狀態：三個檢查步驟、「再檢查一次」與「查看更早的歷史」（設 90 天範圍，歷史圖表接在下方；預設不查歷史） |
| 型號沒有資料點 | 主欄的文字說明，沒有連結與按鈕（型號頁是 Laysi/trustiot-v2#126） |
| 資料點是文字 | 不畫圖，列出把連續相同的值合併後的時段表 |
| 型號載入失敗 | 讀數清單照列（只有代號），上方說明與「再試一次」 |
| 設備本身載入失敗 | 麵包屑加錯誤與「再試一次」 |
| 設備不存在或網址的 ID 格式不對 | 「找不到設備」頁；格式不對時不送請求 |

- **圖表是儀表板的時間序列元件**（決定 3）：`historyOption.ts` 以 `dashboard/chart/options.ts` 的 `timeSeriesOption` 為底，加上最小與最大值的色帶（兩個堆疊的輔助系列）、缺區間的斜線底（`markArea`，canvas 圖樣；計算與斜線底與儀表板的時間序列共用 `dashboard/chart/gaps.ts`；缺口超過 3 個區間或 3 倍的取樣間距，缺口處的折線以一個 `null` 點斷開，停滯的設備缺口一路到範圍的結尾）與布林的階梯面積；tooltip、十字線、縮放與鍵盤的 slider 是 `InteractiveChart`（`onZoom` 與 `zoomActions` 讓設備頁在縮放時重算統計並提供「把這段設為範圍」）。沒有任何 `style` 屬性或注入的 `<style>`，也沒有新的相依。Enter 或點圖打開「以表格顯示」：圖沒有別的來源可看。
- **統計**：圖上方的平均（以每個區間的筆數加權）、最小與最大（附時間）、讀數筆數與缺漏的時間；縮放後改算放大的範圍。布林是目前狀態與開始時間、開啟比例、由關轉開的次數與讀數；這些由區間的 `avg`、`last` 與 `count` 算出，精度是區間（不是單筆讀數）。
- **不顯示單位**：型號沒有單位欄位（Laysi/trustiot-v2#982）。問 AI 的按鈕與「從對話進來」的時段標示跟對話首頁一起上線（Laysi/trustiot-v2#928），版面已預留，不放停用的按鈕。
- **無障礙**：讀數清單的每一列是 `aria-pressed` 的按鈕；圖本身 `aria-hidden`，外層是一個 `role="slider"`：聚焦後以方向鍵、Home、End、PageUp、PageDown 逐一選區間，`aria-valuetext` 是該區間的時間與數值。同一份資料可以展開成表格，是完整的替代內容。狀態標籤、橫幅與各種空狀態是 `role="status"`，錯誤是 `role="alert"`。
- **測試**：`src/test/fake-devices.ts` 是 HTTP 層的假服務，經真正的 `deviceApi(createApiClient(...))` 與 zod 解析，依 telemetry-query 的規則回答（保存期間、403、`truncated`、自動間隔、以時區對齊的小時與日區間）；`DevicePage.test.tsx` 以 `src/test/fake-chart.ts` 的假引擎（happy-dom 沒有 canvas）讀取交給 ECharts 的 option，與同一個 API 的回答比對畫面上的值與圖表；`historyOption.test.ts` 以 SVG renderer 的伺服器端模式跑真的 ECharts；`chart.test.ts`、`health.test.ts` 測缺區間、統計與停滯的判斷。ECharts 實際畫出的圖、CSP 與 Trusted Types 只能在瀏覽器確認，方法與結果在 [測試報告](../docs/test-reports/2026-11-04-781-device-page-redesign.md)。
- **UI 預覽的資料**：`fake-devices.ts` 的 `preview`（`normal`）有六台設備，設備頁的每個狀態都有對應的設備：`cold-room-1`、`line-2-meter`、`compressor-1`（正常，含布林點）、`boiler-1`（停滯 45 分鐘，有文字資料點）、`chiller-2`（歷史中間有一段空洞，畫成缺區間）、`line-4-meter`（從沒存過讀數）；另有一個沒有設備的型號 `flow-meter`。資料都是虛構的（IP 用 `192.0.2.0/24`）。`ui-preview/preview.test.tsx` 經預覽載入器確認停滯與沒有讀數的畫面。

### 設備列表

`/devices`（`DevicesPage.tsx`，Laysi/trustiot-v2#1051，清單 C2），選單第 2 組的「設備」；設備頁算在它底下，選單標示目前項目。唯讀：每個角色都看到 [`docs/design/tenant-admin/`](../docs/design/tenant-admin/design.md) 「3.2」檢視者的版本（`devices--devices-viewer`），沒有新增按鈕與列操作；新增、編輯、刪除與型號的畫面是 Laysi/trustiot-v2#126。

- **資料**：資產服務 `GET /api/assets/v1/devices`（一次回傳租戶所有設備，不分頁）與 `GET /api/assets/v1/device-models`（型號名稱與型號篩選）；查詢 key 是 `deviceListKey`、`modelListKey`。
- **欄位**：設備（名稱連到設備頁，下方是型號名稱）、閘道器、連線、取樣間隔，窄螢幕改成清單。沒有狀態欄，等健康資料（Laysi/trustiot-v2#118、#454）。閘道器欄顯示設備的 `hardware_id`，和設備頁相同，因為目前沒有 API 讓 SPA 取得閘道器名稱。
- **篩選**：搜尋（名稱、主機）、閘道器、型號都寫在網址（`?q=`、`?gateway=<hardware_id>`、`?model=<model_id>`），在前端過濾（設計稿的決定 6）；已啟用的條件顯示成可移除的標籤，另有「清除全部」。
- **狀態**：載入中是骨架；失敗顯示原因與「再試一次」；租戶沒有設備時顯示設定的三個步驟，只當文字，不放連結或按鈕，因為對應的頁面還不存在（Laysi/trustiot-v2#126、#127）。型號存在與否由資料判斷（有型號就標第二步完成、第三步是下一步）；閘道器是否已綁定頁面不知道，第一步不標記。
- **測試**：`DevicesPage.test.tsx` 以 `fake-devices.ts` 的假服務測列表、選單標示、網址條件與重新載入、沒有符合與沒有設備的狀態、載入失敗。

## 閘道器頁

2026-11-05 建立（Laysi/trustiot-v2#127）：`/gateways`，閘道器的列表、在線狀態與健康、管理員的自助綁定、配額已滿的說明。畫面以 `docs/design/tenant-admin/` 的 `gateways--*` 為準；和設計稿不同的地方在那份 `design.md` 的第 11 節。**目前只在 UI 預覽裡**（`src/pages/gateways/preview.ts`），不在 `routes.tsx` 與 `menu.ts`：自助綁定（Laysi/trustiot-v2#114）與健康資料（Laysi/trustiot-v2#118）的端點還不在 `api/openapi.json`，頁面沒有它們就不能完整使用。

- **API**：`src/api/gateways.ts` 是已存在的兩支：`GET /api/gateway-ops/v1/gateways`（列表與 presence）與 `GET /api/identity/v1/entitlements`（只讀方案與 `max_gateways`），照一般規則以 zod 綁定產生的型別。還沒有的兩支在 `src/api/pending/gateways.ts`（健康的批次讀取、以硬體 ID 與認領碼綁定），provider 在 `src/api/pending/GatewaysApiProvider.tsx`；`pages/gateways/available.tsx` 的 `useGatewaysAvailable()` 在有 provider（它掛 `GatewaysAvailable`）時為真，空的設備列表的第一步（「綁定閘道器」）靠它決定要不要連到 `/gateways?bind`，所以 provider 掛進 `AppProviders` 的同一個變更就會讓連結出現。
- **假服務**：`src/test/fake-gateways.ts`（`preview` 是 provider）。已存在的兩支以 HTTP 層回答，經真正的 API 模組；綁定與健康在記憶體裡。預覽裡這些硬體 ID 會刻意失敗，好讓每個失敗狀態都點得到：`tg-0005` 加認領碼 `7KQ2-M9XA-44TR` 綁定成功（Business 方案 5 台，綁完就是上限），`tg-network` 沒有回應，`tg-limited` 被限流 30 秒，其他組合是「無法綁定」。
- **誰能做什麼**：所有角色看得到列表與健康；只有管理員（決定 2）有綁定按鈕，`?bind` 對其他角色不開任何東西。角色從工作階段讀，不讀假服務。
- **自動更新**：列表與健康每 30 秒讀一次（`REFRESH_MS`，視窗不在前景時 TanStack Query 不輪詢）；更新時沒有骨架、也不放進 live region。讀取失敗而畫面上已有資料時保留舊資料，並說明那是幾點的狀態。
- **綁定的失敗**：輸入在每種失敗後都保留。不在設備池、已綁在別的租戶與認領碼不對是同一段說明（Laysi/trustiot-v2#114 的「不洩漏」）；沒有回應或 503 說「綁定沒有完成」並把按鈕改成「重試綁定」（重送是安全的）；`rate_limited` 依 `details.retry_after_secs`（沒有就 60 秒）倒數並停用按鈕；`quota_exceeded` 說明上限已滿。綁定進行中 Esc 與取消都不關閉對話框。
- **配額**：`max_gateways` 來自身分服務的權益；列表的台數達到上限時顯示說明並停用綁定按鈕（`aria-describedby` 指向說明），伺服器仍以 `quota_exceeded` 為準（兩個分頁同時綁定時）。
- **不用 style 屬性**：使用量條以 5% 為單位的固定 class 畫（`GatewayList.tsx` 的 `BAR_WIDTHS`），旁邊一定有精確的數字。

## 平台後台

`/platform/...`（`src/pages/platform/`，Laysi/trustiot-v2#132，清單 K1）：租戶列表、建立租戶、租戶詳情（概覽、成員、閘道器、操作紀錄）、停權與恢復、配額調整、健康總覽與設備池、佈建事件、操作紀錄。畫面以 [`docs/design/platform-admin/`](../docs/design/platform-admin/design.md) 為準；和設計稿不同的地方寫在那份文件的「和設計稿不同的實作」。**目前只在 UI 預覽出現**（`pages/platform/preview.ts`）：租戶列表與用量、邀請、配額、健康總覽、解綁／送修／除役在 `api/openapi.json` 還沒有，頁面接 `src/api/pending/platform.ts` 的介面與 `src/test/fake-platform.ts` 的假服務；介面的每個方法註明哪些已經在規格裡（`getPlatformTenantState`、`setPlatformTenantState`、`createTenant`、`setPlatformTenantEntitlements`、`setPlatformTenantMemberRole`、兩個 event-log、`listPoolGateways`、`bindGateway`、`revokeGateway`），哪些屬於哪個後端 issue。後端齊了之後照「UI 預覽」的「後端完成時要改的地方」處理，把頁面（`routes.ts` 的 `platformPages`）移進 `layout-route.ts` 的 `productPlatformRoutes`：它已經由 `routes.tsx` 掛在 `TenantGate` **旁邊**，不在它底下，平台管理員的目前租戶被停權時，這些頁面仍要能開，才能恢復那個租戶。

- **正式與預覽的路由**（2026-11-05 整合 Laysi/trustiot-v2#154 時定）：正式建置已有 `/platform`，因為 AI 組的模型角色有真正的 API：`layout-route.ts` 的 `productPlatformRoutes` 是外框、`/platform` 本身（`PlatformIndex`，導到建置裡的第一個區段，沒有任何區段時是找不到頁面）與 `ai/plugin.ts` 的 `aiRoutes`，由 `routes.tsx` 掛在 `TenantGate` 旁邊。UI 預覽把其餘頁面（`platformPages` 與 `ai/preview.ts` 的 `previewAiPages`）掛在 `TenantGate` 底下（預覽只能加在那裡），所以 `preview.ts` 給它們另一個沒有路徑的外框，路徑寫完整（`platform/tenants`），同一個網址不會有兩個路由；兩個外框是同一個元件，畫面一樣。測試用的 `platformRoutes` 是全部頁面在同一個外框下，等於後端齊了以後的樣子。
- **共用外框**：`PlatformLayout.tsx` 是 `/platform` 路由的元素，子路由是各頁。它只讓平台管理員看到頁面（session 的 `platformAdmin`；其他人看到「沒有權限」），頂端是一個名為「平台後台」的區塊：跨租戶的提示與區段切換（寬螢幕 ≥1024px 是依組分開的連結列，目前的區段 `aria-current="page"`；窄螢幕是原生 `<select>`）。區段清單在 `sections.ts`；外框只畫建置裡有 API 的區段（`available.tsx`）：提供某組 API 的 provider 同時掛 `PlatformSectionsAvailable`（`ModelRoleApiProvider` 在 `AppProviders` 裡，待接的 `PlatformApiProvider`、`PromptApiProvider`、`AiUsageApiProvider` 只在預覽與測試裡），所以正式建置現在只看到「模型角色」，預覽看到全部；待接的 provider 搬進 `AppProviders` 的同一個變更，區段就會出現。
- **AI 頁（Laysi/trustiot-v2#154）怎麼接上**：只改 `pages/platform/ai/plugin.ts`（與預覽的 `ai/preview.ts`）。`aiSections` 有每個頁面的區段（`group: "ai"`，路徑 `/platform/ai/...`），`aiRoutes` 是 API 已存在的頁面路由（相對於 `/platform`，變成外框路由的子路由，自動有權限檢查、提示與區段切換），API 待接的頁面在 `previewAiPages`。沒有任何 AI 區段時，AI 這一組不畫。AI 頁自己的 API、provider 與假服務放在 `pages/platform/ai/` 與 `src/test/fake-*.ts`。
- **元件**：這一組共用的 `Badge`、`Meter`（`<progress>`，寬度不用 `style` 屬性）、`Skeleton`、`StatTile`、`StackTable`（窄螢幕變卡片，欄名是真的文字，只在窄螢幕顯示）、`ConfirmByTyping`、`ChangePreview`、`ImpactList`、`AuditNote` 與 `ActionDialog` 在 `pages/platform/ui/`，不放 `components/ui/`，等其他頁面也要用時再搬。`ActionDialog` 是所有高風險確認的骨架：焦點先在「取消」、按鈕寫出動作與對象、送出中不能關閉也不能再按、拒絕留在對話框並說明有沒有改變任何東西（結果不明時要管理員先看操作紀錄）、成功後所有 `["platform", ...]` 的查詢重讀（不做樂觀更新）。
- **停權與「處理中」**：狀態以 `status` 顯示，`processing`（certs 還沒確認）絕不顯示成已停權，也不逾時；等待時間從 `state_changed_at` 算，畫面每秒更新、螢幕報讀每分鐘一次；超過 10 分鐘只多一則提示（決定 Q4）。列表與租戶頁在有處理中的租戶時每 5 秒重讀。停權與恢復在送出前先讀一次狀態，`state_version` 與開啟對話框時不同就停下來、什麼都不寫（決定 Q3；介面已帶 `expectedStateVersion`，後端接上後由伺服器擋）。
- **操作紀錄**：事件類型的名稱與摘要在 `audit/eventSummary.ts`，不認得的類型照列、顯示類型原文並可展開 payload；操作者的 email 經 `lookupUserEmails`（決定 Q14，目前待接），查不到時顯示 ID 的前後幾碼。時間範圍的輸入是瀏覽器的 `datetime-local`（瀏覽器的時區），顯示則照使用者偏好的時區。
- **測試**：`src/pages/platform/*.test.tsx` 以 `src/test/render-platform.tsx`（外框、假服務、`platformRoutes`）與 `src/test/fake-platform.ts` 測每個畫面的每個狀態與每個確認對話框，包含非平台管理員看不到頁面、停權從「處理中」到「已停權」再恢復，以及操作紀錄顯示剛才的操作。`fake-platform.ts` 守伺服器的規則（最終狀態 409、原因與狀態不符 422、不認得的 email 422、閘道器已被別的租戶綁住 409、配額已滿），並提供 `failNext`、`hold`、`confirmApplied`。UI 預覽的資料（`normal`）有六個租戶（正常、處理中、已停權、合約終止、超長名稱、AI 透支）與六台閘道器（在線、降級、離線、設備池、送修）。

## 開發伺服器

`bun run --cwd frontend dev` 只提供前端（不接後端看畫面，用「UI 預覽」）。要接後端時，先依 `scripts/README.md` 在本機啟動服務，再以 `TRUSTIOT_DEV_API_ORIGIN=<服務的 origin> bun run --cwd frontend dev` 啟動：`/api` 與 `/auth` 轉送到該 origin，保留原本的 Host，與正式環境一樣同源。

## UI 預覽

2026-11-04 建立（Laysi/trustiot-v2#1065）：`docs/design/` 的畫面全部先做出來，後端 API 還沒有的畫面先接假服務。正式的建置不能有假資料，後端還沒有的頁面也不能出現在正式的路由與選單，所以這些畫面只在 UI 預覽裡看得到。

### 怎麼啟動

`bun run --cwd frontend ui-preview`（`vite --mode ui-preview`），打開 `http://localhost:5174/`。不需要任何後端，也不轉送 `/api`：每個請求都在頁面裡由假服務回答，沒有假服務回答的路徑是 404 `not_found`，主控台會出現 `UI preview: no fake answers …` 的警告。埠和 `dev`（5173）不同，兩個可以同時開，瀏覽器的儲存也分開。

- **入口**：`ui-preview.html` 與 `src/ui-preview/`。開發伺服器把任何路由的頁面載入（例如重新整理 `/devices/42`）都交給 `ui-preview.html`（`tools/ui-preview-server.ts`），不是 `index.html`。
- **同一份 app**：預覽以 `src/app/start.tsx` 的 `startApp` 啟動，和 `main.tsx` 是同一個 router、外框與頁面，只換掉三樣：路由（正式的加上預覽的）、選單（`MENU_ITEMS` 加上預覽的，經 `menu.ts` 的 `MenuItemsContext`）、API client 的 `fetch`（假服務）；另外把 provider 形式的假服務包在 app 外面。SSE 直接呼叫 `fetch`，所以預覽也把同源的 `/api/` 與 `/auth/` 的 `window.fetch` 交給假服務。
- **工具列**（頁首上方的一列，不在 app 裡）：角色（admin、editor、viewer）、平台管理員、資料（`normal` 一般／`empty` 空的）、讓每個請求變慢（2 秒或 10 秒）、讓下一個請求失敗（503、500、403 或網路錯誤，用掉一次就解除；勾「Every request」則每個請求都失敗，直到選回 no）。query client 會重試 503 與網路錯誤，所以只失敗一次時看到的是重試成功的畫面；要看這兩種的錯誤狀態就勾「Every request」。角色、平台管理員與資料存在分頁的 sessionStorage，改了就重新載入，所有假服務以新設定重新建立，頁面上改過的資料會回到初始狀態；變慢立即生效。瀏覽器拒絕 sessionStorage 時（例如隱私視窗）只能用預設值（admin、一般資料）。
- **登入**：預覽的使用者（`lin.mei@example.com`，兩個虛構的組織）一開始就已登入。登出後要再登入時，驗證碼依送出的順序是 `10000000`、`10000001`……；重新整理也會回到已登入。

### 假服務與預覽頁面的約定

預覽以 `import.meta.glob` 自己找出下面的檔案，**沒有中央清單要改**：

- **`src/test/fake-*.ts` 匯出 `preview`** 的就是預覽的假服務（沒有 `preview` 的，例如 `fake-chart.ts`，不載入；預覽用真的 ECharts）。形狀（型別在 `src/ui-preview/contract.ts`，物件字面值即可，不必 import）：

  ```ts
  export const preview = {
    name: "<工具列顯示的短名稱>",
    // 二選一：
    http: { prefixes: ["/api/<service>/v1/<path>"], create: (seed, context) => ({ fetch: (request) => … }) },
    provider: (seed, context) => ComponentType<{ children: ReactNode }>,
  };
  ```

  - **API 已經在 `api/openapi.json`**：用 `http`。請求交給前綴最長、而且涵蓋路徑的假服務（前綴本身，或前綴後接 `/` 的路徑）；同一個前綴只能有一個假服務宣告。頁面照常經真正的 API 模組（zod 解析）讀它，和單元測試一樣。
  - **後端還沒有**：介面與型別放 `src/api/pending/<名稱>.ts`，第一行寫 `// Pending Laysi/trustiot-v2#<後端 issue>: replace with a module over the generated client when api/openapi.json has these endpoints.`，頁面經它旁邊的 context provider 取得；假服務以 `provider` 提供記憶體裡的實作。
  - `seed`：`"normal"` 要有足夠的資料畫出設計稿的正常狀態，`"empty"` 沒有資料。`context` 是工具列的 `role` 與 `platformAdmin`，也就是預覽的工作階段的角色：像伺服器一樣依角色回答的假服務（儀表板的 `permissions`）用它；**頁面一律照正式的方式從工作階段讀角色**，不讀假服務。
  - 資料全部虛構（`docs/design/README.md`「使用規則」）：網址用 `*.example.test`、email 用 `@example.com`、IP 用文件用的 `192.0.2.0/24`。
  - 宣告有錯（少了 `http` 與 `provider`、前綴重複）時預覽不啟動，錯誤訊息寫出檔名。
- **`src/pages/<area>/preview.ts`** 匯出只在預覽出現的路由與選單項目：`export const previewRoutes: RouteObject[]`（`react-router`）與 `export const previewMenu: MenuItem[]`（`app/menu.ts`，組號依 `docs/design/app-shell.md`）。路由掛在租戶頁面的路由下（`routes.tsx` 的 `TENANT_PAGES_ROUTE_ID`，`TenantGate` 底下），所以和正式的頁面一樣有外框、工作階段與租戶；頁面以 `lazy` 載入就和正式路由一樣切成自己的 chunk。選單項目的 `id` 不能和其他項目重複。

### 正式的路由與預覽的路由

| | 正式（`main.tsx`、`bun run build`） | 預覽 |
| --- | --- | --- |
| 路由 | `src/app/routes.tsx` 的 `ROUTES` | `ROUTES` 加上每個 `preview.ts` 的 `previewRoutes` |
| 選單 | `src/app/menu.ts` 的 `MENU_ITEMS`：只列已經上線的頁面 | `MENU_ITEMS` 加上每個 `previewMenu` |
| API | 真正的服務 | `fake-*.ts` 的 `preview` |

頁面只用到已經存在的 API、而且只靠它們就能完整使用時，才放進 `routes.tsx` 與 `menu.ts`；否則只放 `preview.ts`。

### 後端完成時要改的地方

1. 依「API client」在 `src/api/<名稱>.ts` 寫以產生的 client 實作的模組與 zod schema，provider 改用它（像 `DeviceApiProvider.tsx`），刪掉 `src/api/pending/<名稱>.ts`。
2. 假服務改成 HTTP 層（`fetch(request)` 回 `api/openapi.json` 的內容，像 `fake-dashboards.ts`），`preview` 從 `provider` 改成 `http`，單元測試改經真正的 API 模組。
3. 頁面的路由與選單項目從 `preview.ts` 搬進 `routes.tsx` 與 `menu.ts`；`preview.ts` 沒有東西了就刪掉。
4. 依頁面的 issue 對真正的服務驗證。

### 正式建置不含預覽與假服務

- `build` 的輸入只有 `index.html`（`vite.config.ts` 的 `rolldownOptions.input`）；`ui-preview.html` 只給開發伺服器。
- 只給開發用的模組（`src/test/`、`src/ui-preview/`、`src/api/pending/`、頁面目錄的 `preview.ts`、`*.test.ts(x)`）被建置帶進來時，`tools/dev-only.ts` 的 Vite plugin 在模組最後加一行寫入全域變數 `__TRUSTIOT_DEV_ONLY_MODULE__` 的敘述：寫全域是副作用，所以模組不會因為沒用到而被丟掉，壓縮也不會改掉屬性名稱。`check:dist` 看到任何 `.js` 含這個名稱就失敗。正式的程式不小心 import 了假服務或待接的 API 時，建置照樣完成，但 `check:dist`（CI 與 release）擋下來；`tools/dev-only.test.ts` 以真的建置混入一個假服務來確認。
- 預覽沒有放寬 SPA 的 CSP：工具列只用 class，沒有 `style` 屬性或 `<style>`。但 Vite 的開發伺服器本身不送 CSP 標頭，而且它的熱更新會注入 `<style>` 與 inline script（`dev` 也一樣），所以 CSP 與 Trusted Types 的違規只能在正式的建置（`services/web`，「瀏覽器端對端測試」）確認，預覽看不出來。
- 測試：`src/ui-preview/preview.test.tsx` 以 `main.tsx` 裡的 glob 樣式找出檔案，確認四個既有的假服務都被載入、首頁、對話紀錄、儀表板、設備清單與設備頁、設定都打得開；一個只寫在 `fake-*.ts` 與 `preview.ts` 的範例出現在路由與選單；viewer 看不到 editor 與 admin 的操作；工具列的角色與失敗開關。瀏覽器實測在 `docs/test-reports/2026-11-04-1065-ui-preview.md`。

## 型號與設備管理畫面

型號與設備的寫入畫面（Laysi/trustiot-v2#126；設計稿與不同之處在 `docs/design/tenant-admin/design.md` 的第 3.2 與第 10 節）：`/devices/new`、`/devices/:id/settings`、`/models`、`/models/new`、`/models/:id`，以及設備列表的列操作；選單的「型號」在第 2 組。

- **誰能寫**：編輯者與管理員（`canWriteAssets`，設計決定 2）；檢視者看得到列表與唯讀的型號頁（可以匯出），沒有按鈕與列操作，用連結打開寫入的網址時顯示 `WriteDenied`。權限由伺服器強制，畫面只是不放按鈕。
- **API**：`src/api/assets.ts` 的 `AssetsApi`（建立、更新、刪除、匯入、匯出，以及表單要讀的閘道器清單 `GET /api/gateway-ops/v1/gateways` 與方案最短取樣間隔 `GET /api/identity/v1/entitlements`），讀取仍走 `DeviceApi`。更新是整筆取代：表單每次送出完整內容與讀到的 `revision`。
- **錯誤逐欄顯示**：伺服器的 `field_errors`（JSON Pointer，例如 `/host`、`/points/2/address`）經 `pages/form-problems.ts` 對到輸入欄；沒有欄位的 409（名稱重複、站號重複）依 `message_key` 對到欄位。送出失敗時 `ErrorSummary` 取得焦點，每一行連到它的欄位。能在前端判斷的（必填、格式、範圍）先擋下，需要租戶資料的（名稱重複、站號重複、方案下限、位址重疊）交給伺服器。
- **成功後的訊息**：`FlashNotice` 讀路由 state 的訊息（`flashState`），顯示在跳轉後的頁面，關閉後取代歷史紀錄，所以重新載入不會再出現。
- **假服務**：`src/test/fake-devices.ts` 的 `FakeDeviceServer` 是 UI 預覽裡 `/api/assets/v1` 的唯一擁有者，寫入的規則在 `src/test/assets-rules.ts`（`server.assets`：名稱唯一、`revision` 衝突、使用中的型號不能刪、解除綁定的設備不能改，其餘用 `failNext` 指定）；它也回答閘道器清單與方案限制的路徑。`bun test` 以 `server.assetsApi()` 經真正的 API 模組使用它。
- **匯入 JSON**：`?import` 打開對話框；檔案在瀏覽器先檢查（JSON、`format`、`format_version`、資料點的形狀，1 MB 以內），預覽後才送出，伺服器的錯誤逐條列出。匯出以瀏覽器下載（`src/lib/download.ts`）。

## 成員頁

`/members`（管理員）與 `/invitations/accept#<token>`（接受邀請）在 `src/pages/members/`，Laysi/trustiot-v2#125，設計稿與實作的差別在 [`docs/design/tenant-admin/design.md`](../docs/design/tenant-admin/design.md) 的「成員畫面和設計稿不同的地方」。

**目前只在 UI 預覽出現**：身分服務還沒有成員與邀請的端點（Laysi/trustiot-v2#105、#110），所以路由與選單項目只在 `src/pages/members/preview.ts`（選單第 4 組的「成員」，只有租戶管理員看得到），不在 `routes.tsx` 與 `menu.ts`，正式的建置不含這些頁面。頁面經 `src/api/pending/MembersApiProvider.tsx` 取得介面（`src/api/pending/members.ts`），假服務是 `src/test/fake-members.ts`（`preview` 是 provider 形式，沒有 HTTP 前綴，不和別的假服務搶路徑）。端點上線時：寫一個包 `createApiClient` 的模組取代 `pending/members.ts`（頁面與測試不用改，假服務改成 HTTP 層的），路由與選單項目搬進 `routes.tsx` 與 `menu.ts`，接受邀請的路由放在 `TenantGate` **旁邊**而不是底下：被邀請的人可能還沒有任何組織。

**對後端的假設**（`api/pending/members.ts` 的註解有同樣的清單；#105 定案時要對）：

- 讀取：`listMembers` 一次回傳成員、待接受的邀請、方案與名額（`used` 含待接受的邀請，`limit` 可以是 `null`＝不限；設計稿決定 4）。方案名稱與上限日後可改讀 `GET /api/identity/v1/entitlements`（已存在），`used` 還是要由成員 API 給。
- `listAdmins`：非管理員也能讀的管理員名字，給「沒有權限」頁面用（決定 1）；`listMembers` 本身是管理員專用。
- 錯誤的 `message_key`：`member.last_admin`（409 `conflict`，最後一位管理員被降級或移除）、`member.already_member`（422，欄位 `/email`）、`member.quota_full`（403 `quota_exceeded`）、`invitation.invalid`（404，過期、撤銷與已接受不分）、`invitation.account_mismatch`（403，登入了別的帳號）；中英翻譯已在 locale 檔。
- 邀請的憑證只在 POST 的內容裡（`previewInvitation`、`acceptInvitation`），不在任何網址。
- 「你」的判斷用工作階段的使用者 ID，或與 email 相符（成員列表一定有 email）。

**接受邀請的安全做法**（決定 3）：頁面第一次繪製就把 `#<token>` 讀進記憶體，並用 `replace` 把網址換成沒有 fragment 的路徑，之後的重試用記憶體裡的值；預覽的查詢 key 不含 token，`gcTime` 為 0。接受之後（連結用掉了）不再重讀預覽，避免切換組織重設查詢時閃出「邀請無法使用」。**待處理**：沒有登入的人打開連結時，`Layout` 會導到 `/login?next=…`，`next` 帶著完整的路徑與 fragment，token 因此進了登入頁的查詢參數與伺服器的存取紀錄；路由正式上線前，登入導向要對這條路徑特別處理（例如先把 token 放進 `sessionStorage`，`next` 不帶 fragment）。

**預覽怎麼看**：成員頁在選單的「成員」（工具列選管理員角色）；`?invite` 直接開邀請對話框。接受邀請用 `/invitations/accept#preview-valid`（一般）、`#preview-mismatch`（帳號不符）、`#preview-invalid`（失效）；接受之後那個連結就用掉了，重新整理預覽才會回到初始。

**測試**：`MembersPage.test.tsx` 與 `AcceptInvitationPage.test.tsx` 經 `src/test/render-members.tsx` 把預覽的路由包在假服務的 provider 裡，涵蓋設計稿的每個狀態（列表、載入中、錯誤、只有自己、沒有權限、邀請與欄位錯誤、名額已滿、角色與最後一位管理員、移除確認、重新寄送與撤銷、接受邀請與兩種失敗），並檢查檢視者與編輯者看不到成員頁、token 從網址消失。假服務的規則寫在 `fake-members.ts` 檔頭（名額含待接受邀請、最後一位管理員的檢查與寫入是同一步、連結只能用一次）。

## 平台後台的 AI 畫面

平台管理員的模型角色、system prompt 與 AI 用量（Laysi/trustiot-v2#154；設計稿與不同之處在 `docs/design/platform-admin/design.md`，「實作與設計稿不同的地方」）。頁面都在 `src/pages/platform/ai/`，與平台後台的外框（Laysi/trustiot-v2#132 擁有 `src/pages/platform/` 與它的版面、選單）只在 `plugin.ts` 接上。

| 網址 | 內容 | API | 正式路由 |
| --- | --- | --- | --- |
| `/platform/ai/model-roles` | 角色的模型清單、改清單、還原預設 | `src/api/modelRoles.ts`（`/api/identity/v1/platform/ai/model-roles`，已在 `api/openapi.json`） | 是（`plugin.ts` 的 `aiRoutes`，經 `layout-route.ts` 掛在平台外框下、TenantGate 之外，沒有選單項目） |
| `/platform/ai/prompts`、`/platform/ai/prompts/:service/:role` | prompt 清單、編輯、發布、題庫試跑 | `src/api/pending/prompts.ts`（待 Laysi/trustiot-v2#153） | 否，只在 `preview.ts` 的 `previewAiPages` |
| `/platform/ai/prompts/:service/:role/versions` | 版本、比較、回復、預設更新 | 同上 | 否 |
| `/platform/ai/usage` | 月用量、各租戶額度、類別、對帳與發票金額 | `src/api/pending/aiUsage.ts`（待 Laysi/trustiot-v2#140） | 否 |

- **只有模型角色是正式的**：另外兩組 API 還沒有後端，所以只放進 `preview.ts`，用 `src/test/fake-prompts.ts` 與 `src/test/fake-ai-usage.ts` 的 `preview.provider`；模型角色的假服務 `fake-model-roles.ts` 是 HTTP 層，`/api/identity/v1/platform/ai/model-roles` 的唯一擁有者。後端完成時依「後端完成時要改的地方」處理。
- **誰能看**：只有平台管理員；其他人看到無權限頁（伺服器另外以 403 擋下）。這些 API 不帶租戶標頭。
- **寫入的安全**：模型角色以 `expected_version` 送出，409 時重新讀取並讓管理員比較後再送，504（結果不明）時重新讀取確認再說明，兩者都不自動重送；prompt 發布同樣以版本比對。確認對話框寫出改前改後、影響範圍與會寫入稽核紀錄。
- **prompt 是純文字**：固定段落唯讀，可編輯段落以 `<textarea>` 加一層只畫變數標示的疊層；內容、差異與試跑結果都當文字顯示，不經 HTML。草稿只在分頁記憶體裡，離開時要確認（設計決定 Q8）。
- **試跑是假的**：預覽的題庫試跑只在假服務裡推進；不得在畫面測試裡呼叫付費模型。
- **版面**：每一頁都在 #132 的平台外框裡（「平台後台」一節的「正式與預覽的路由」）：`aiRoutes` 掛在正式的 `/platform` 路由下，`previewAiPages` 由 `pages/platform/preview.ts` 掛進預覽的外框，`aiSections` 給外框的導覽，只畫建置裡有 API 的區段（正式建置只有模型角色）。2026-11-05 整合時刪掉了暫時的外框 `AiShell.tsx`。
- **元件**：`StatTile` 用 #132 的 `pages/platform/ui/stat-tile.tsx`（兩邊行為相同）。`ui.tsx` 的 Badge、Meter（SVG，沒有 `style` 屬性）、ChangePreview、ImpactList、AuditNote、Skeleton 與資料表和 `pages/platform/ui/` 的同名元件外觀或用法不同，所以兩組都留著（差別寫在 `ui.tsx` 開頭），Tabs 與 Loading 只有這裡有。
- **測試**：`bun run test` 的 `src/pages/platform/ai/*.test.tsx` 經真正的 API 模組與假服務；`diff.ts`（逐行與逐字差異、預設更新的套用）與 `reconciliation.ts`（差額與 CSV，含公式注入防護）有單元測試。

## 機台與生產紀錄畫面

`/machines`（列表）與 `/machines/:machineId`（詳情，`?run=<紀錄 ID>` 標示一筆生產紀錄），清單 C4、C5，Laysi/trustiot-v2#178；設計在 [`docs/design/ms4-ms5/`](../docs/design/ms4-ms5/design.md) 的第四節與「和設計稿不同的實作（機台與生產紀錄…）」，程式在 `src/pages/machines/`。

- **後端還沒有，只在 UI 預覽**：現場脈絡服務（Laysi/trustiot-v2#177）與它的 API 還不存在，所以介面與型別在 `src/api/pending/machines.ts`（`MachineApi`），頁面經 `MachineApiProvider` 取得；路由與選單項目只在 `src/pages/machines/preview.ts`（選單第 3 組，圖示 `factory`），不在 `routes.tsx` 與 `menu.ts`。後端完成時照「UI 預覽」的「後端完成時要改的地方」：寫以產生的 client 實作的模組，刪掉 `pending/machines.ts`，假服務改成 HTTP 層，路由與選單搬進正式的清單。
- **假服務**：`src/test/fake-machines.ts` 的 `FakeMachineServer` 實作 `MachineApi`：同一台機台同時只有一筆進行中的紀錄、紀錄的時間不重疊（409 `field_context.run_overlap`，`details` 帶撞到的那一筆）、一個資料點只屬於一台機台、換料以伺服器的時鐘記下時間、每次換料、補登、修正都寫一行變更紀錄。開關：`unavailable`（服務沒有回應）、`plan = "starter"`（403 `entitlement.feature_not_in_plan`）、`failNext`、`holdNext`。UI 預覽的 `preview` 是 provider 形式，資料是虛構的機台與一週的紀錄；機台歸屬的設備來自預覽的設備假服務（機台假服務第一次被呼叫時讀它的設備清單，分給前四台機台）。
- **權限**：編輯者與管理員（`canEditMachines`）看得到換料、結束生產、補登、修正、編輯、刪除、歸屬與關係的按鈕；檢視者只讀，沒有按鈕（不是停用的按鈕）。權限由伺服器強制，畫面只是不放按鈕。角色一律從工作階段讀。
- **換料不能填時間**（決策 Q10）：對話框只問料號、工單與目標，開始時間由伺服器記下；按晚了用「修正」，修正必填原因，補登的原因選填（決策 Q9）。補登與修正與別筆紀錄重疊時，伺服器拒絕，對話框寫出撞到的那一筆並連到「修正那一筆」。
- **狀態**：載入中（骨架，`role="status"`）、空（解釋機台與設備分開，編輯者有「新增機台」）、現場脈絡服務無法使用（寫出恢復前怎麼辦，有「再試一次」）、Starter 的升級牆（伺服器的 403 `entitlement.feature_not_in_plan`）、找不到機台、檢視者的唯讀頁。
- **查詢的 key** 都以租戶開頭（`queries.ts`），任何寫入都重設 `machinesKey(tenant)`。
- **測試**：`MachinesPage.test.tsx`、`MachinePage.test.tsx` 以 `test-support.tsx` 的 `openMachines`（`renderAt` 加上預覽的路由與 `MachineApiProvider`）測設計稿的每個狀態與角色；`preview.test.tsx` 以預覽真正的組裝（`buildPreview`）測虛構資料的列表、換料與空資料。**happy-dom 的限制**：Radix 的對話框開著或正在關閉時，`waitFor` 與角色查詢會卡住數十秒（也曾讓 bun 當掉），所以這些測試用 `test-support.tsx` 的 `press`（點擊並等它引起的請求做完）與 `until`（以一般計時器輪詢，逾時寫出在等什麼）。

## 應用程式外框

所有登入後的頁面共用同一個頁首，規格在 [`docs/design/app-shell.md`](../docs/design/app-shell.md)（Laysi/trustiot-v2#1000，實作在 Laysi/trustiot-v2#996）：頁首只有 logo、組織切換、「選單」與帳號，沒有常駐導覽列。

- **元件**：`src/app/AppHeader.tsx` 是頁首（桌機與 639 px 以下的手機各一種形式，各一行），`AccountMenu.tsx` 是帳號按鈕與選單（手機沒有帳號按鈕，帳號收進選單最後一組），組織切換是 `src/session/TenantSwitcher.tsx`（手機用 `compact`）。`src/app/Layout.tsx` 放頁首、通知與頁面。
- **選單的項目在一份清單**：`src/app/menu.ts` 的 `MENU_ITEMS`（UI 預覽經 `MenuItemsContext` 另外加上預覽頁面的項目，見「UI 預覽」）。每一項帶設計稿表格的組號、圖示、路徑、「這一項涵蓋哪些路徑」與「誰看得到」，選單、手機的選單與「目前頁面」的標示（`aria-current="page"`）都讀它；沒有項目可列的組連分隔線一起不出現。**只列已經上線的頁面**：目前是「對話紀錄」、「已存的儀表板」、「設備」、「型號」與「設定」，閘道器、成員、警報與通知、報表、機台與生產紀錄、文件庫、平台後台等頁面上線時，由各自的 issue 加進清單（要加的項目與權限在設計稿的表格）。
- **次要頁面與「回到對話」**：不是對話的頁面（`isSecondaryPage`：首頁與 `/conversations/<ID>` 以外的頁面，對話紀錄本身是次要頁面）有「回到對話」，桌機是文字按鈕，手機是頁首最左邊的返回圖示。目標由 `src/app/back-to-conversation.ts` 決定（Laysi/trustiot-v2#1042）：回答裡的連結以路由 state 的 `fromConversation` 帶著來源對話（`fromConversationState`），有它時回到那段對話；沒有時回到最近活動的使用中對話（與首頁的「最近的對話」同一個查詢），沒有對話時回到首頁。對話頁面不發這個查詢。logo 與手機的「新對話」仍連到首頁，首頁的輸入框就是開始對話的地方（Laysi/trustiot-v2#1044）。
- **桌機與手機用 `useMediaQuery("(min-width: 640px)")` 切換**（`src/lib/use-media-query.ts`），不是兩份標記靠 CSS 藏起來：同一時間無障礙樹裡只有一份，測試也只會找到一個按鈕。`matchMedia` 在第一次繪製前就有答案，所以不會先畫出另一種形式。
- **下拉選單用 Radix 的 `DropdownMenu`**：Escape 關閉並把焦點還給按鈕、方向鍵移動、`Tab` 關閉，都是它的行為；`src/app/AppHeader.test.tsx` 測 Escape 與焦點，其餘行為不重測。
- **黏住的列**（Laysi/trustiot-v2#1182）：頁首（`sticky top-0`）與對話頁底部的輸入框（`sticky bottom-0`）以 `src/lib/sticky-bars.ts` 的 `useStickyHeight` 量自己的高度（`ResizeObserver`，輸入框多行或上方出現提示時會變），寫成根元素的 CSS 變數 `--sticky-top` 與 `--sticky-bottom`（以 CSSOM 設定，CSP 允許），離開時移除；`global.css` 把它們設成 `html` 的 `scroll-padding-top` 與 `scroll-padding-bottom`。所以 `scrollIntoView`、聚焦造成的捲動與 `#片段` 都停在兩條列之間，呼叫的人不必知道頁面有哪些列。要讓某個東西捲進看得到的地方時（例如打開的資料來源面板，Laysi/trustiot-v2#1184），直接呼叫 `element.scrollIntoView({ block: "nearest" })`（高於可見帶時用 `"start"`），焦點用 `focus({ preventScroll: true })` 避免兩次捲動；要自己算位置時用 `visibleBand()`（同樣讀 `scroll-padding`）。不要再寫死 57 或 130。新的黏住的列也用 `useStickyHeight` 發布高度，同一個邊以最後掛上的為準。
- 和設計稿不同的地方記在 `docs/design/chat-dashboards/design.md`「和畫布與實作不同的地方」。

## 儀表板沙箱

`sandbox/` 是 AI 儀表板頁面的沙箱（Laysi/trustiot-v2#150）：沙箱文件、runtime（`window.tq`）、比對程式、host adapter 與檢查服務的 host 頁。它不是 SPA 的一部分，有自己的建置（`tools/build-sandbox.ts`，由 `bun run --cwd frontend build` 在 Vite 之後執行）與自己的 header；契約、檢查規則與固定樣本都在 [sandbox/README.md](sandbox/README.md)。

- `services/web` 隨 SPA 提供 `dist/sandbox/`，檢查服務（`services/dashboard-check`）的映像取同一個目錄。
- SPA 的自訂頁面是 `src/dashboard/custom-page/` 的 `CustomPageFrame`（Laysi/trustiot-v2#997）：以 `sandbox/src/host.ts` 的 `SandboxHost`（`base: "/sandbox"`）在檢視模式建立沙箱 frame、送出頁面與資料，只依 runtime 經 port 回報的比對結果顯示 frame（`guard.ts`；規則在 sandbox/README.md「顯示時比對的強制」）；沙箱在 window 送來的一切都當成頁面寫的內容。卡片的標題列照設計稿（`docs/design/chat-dashboards/states.html` 第 8 個狀態）標示「自訂圖表」、由 AI 寫的程式繪製、這一版寫入前已通過檢查與寫入的時間，並有資料集的來源按鈕；點頁面裡的標記開同一個來源面板（`SourcePanel`）。屬性：`page`、`runtime`、`data`（`DashboardData`）、`title`、`versionCreatedAt`（版本的 `created_at`），選填 `onRefresh`；儀表板頁（Laysi/trustiot-v2#996）經 `CustomPageSlot` 把它放在版面的 `custom_page` 那一項（見「儀表板頁」）。
- `services/web` 給 `/sandbox/v<N>/` 的檔案送 `crates/contract` 定義的沙箱標頭，取代 SPA 的（`Cross-Origin-Resource-Policy: cross-origin`、`frame-ancestors 'self'`、CSP `sandbox allow-scripts`，沒有 `X-Frame-Options`）；`/sandbox/` 底下其他路徑（含檢查服務的 `check.html`）是 404。
- 這八個 d3 模組目前只打包進沙箱，版本隨 runtime 凍結：升級 d3 會改變已檢查過的頁面的畫法，要開新的 runtime 版本（sandbox/README.md「建置結果」）。SPA 的儀表板元件不用 d3（下一節）。
- 沙箱的測試在 `sandbox/test/` 與 `sandbox/src/*.test.ts`，由 `bun run --cwd frontend test` 一起執行。
- 2026-11-01 起沙箱只給自訂頁面用：常用的圖由 SPA 自己的元件畫（下一節，決策 33 的修訂）。

## 儀表板頁

儀表板列表與每個儀表板自己的頁面（Laysi/trustiot-v2#996，清單 J4；設計在 [`docs/design/chat-dashboards/`](../docs/design/chat-dashboards/design.md)，版本紀錄的畫面沒有畫稿，設計寫在同一份 `design.md`）。路由 `/dashboards`（`?tab=archived` 是「已封存」分頁）與 `/dashboards/:dashboardId`（`?version=<n>` 是舊版的深連結），程式在 `src/pages/dashboards/`，API 在 `src/api/dashboards.ts`（`DashboardApi`，zod 的 body 綁到 `api/openapi.json` 產生的型別，同 `lib/type-binding.ts`）。

- **列表**（`DashboardsPage.tsx`）：「使用中」與「已封存」兩個分頁、依名稱搜尋（只比對已載入的）、一頁 50 個、「載入更多」。「已封存」用 `include_archived=true` 讀，再留下封存的。封存與取消封存立即生效（沒有確認視窗），狀態列（`role="status"`）說明做了什麼；檢視者（工作階段在這個租戶的角色）看不到這兩個按鈕。
- **頁面**（`DashboardPage.tsx`）：讀 `GET …/dashboards/{id}`（沒有 `?version=` 是最新版），依版本的元件清單順序畫出 `src/dashboard/` 的元件。第一次載入是骨架；打開時，版本的每個資料集以 `POST …/dataset-runs` 用檢視者的身分各跑一次（`useDatasetRuns.ts`：同時最多兩個，因為 assistant 對每個租戶同時只跑兩個，其餘在它那邊排隊；失敗的不自動重試，429 也一樣）。一個資料集失敗只影響用到它的元件，元件寫出原因並可「再試一次」。
- **更新資料**：重新執行全部資料集，請求照送，不沿用先前的結果；期間保留舊內容並淡化（`opacity-60`，設計系統的 `opacity-refetch`），標題列的 `role="status"` 寫「正在更新…」，完成後資料時間換成新的；失敗的資料集在那個元件顯示失敗，不留著舊值。封存、回到舊版這類寫入不會重跑資料集：它們只重讀儀表板與版本列表（`queries.ts` 把資料集的結果和這些分開）。
- **資料時間與起訖**：資料時間是已有結果的資料集裡最新的 `queried_at`；起訖是結果裡時間欄位的最早與最晚（查詢寫的牆上時間，不換時區，`dataFacts.ts`）。時間範圍的切換不做（API 的 `DatasetRunRequest` 只有版本與資料集，Laysi/trustiot-v2#1032）。
- **版本紀錄、回到舊版、封存**：依 `GET …/{id}` 回傳的 `permissions`（`can_restore`、`can_archive`）顯示，伺服器照樣檢查。`?version=` 看舊版時標題旁有警告色的標籤、頁面有說明與「看最新版」；`POST …/restore` 先確認（焦點在「取消」），衝突、已封存、與最新版相同等錯誤以翻譯過的訊息顯示在視窗裡，成功後回到最新版。封存的儀表板不能回到舊版（伺服器回 409）。
- **「建立者」**：API 只給使用者 ID，沒有以 ID 查成員的 API，所以頁面只能寫「你」或「其他成員」（`creator.ts`）。
- **自訂頁面（`custom_page`）**：`CustomPageSlot.tsx` 把這一項換成 `CustomPageFrame`（Laysi/trustiot-v2#997），整列寬；卡片、標題列與來源按鈕都是 frame 自己畫的，不再包一層。標題用版本的 `title`（`custom_page` 這一項沒有自己的標題），「重新整理」接 `useDatasetRuns` 的 `refresh`（讀取失敗時 frame 提供，重跑版本的所有資料集），語言、時區與主題由 frame 從 `useI18n` 與 `usePreferences` 取得。版本沒有 `page` 或 `runtime`（API 不會給：契約的 `validate_version` 保證有自訂頁面的版本一定有）時，這一項顯示「這一項無法顯示」的卡片，不載入沙箱。
- **這一頁的瀏覽器檢查**（CSP、對比、端對端）在 `docs/test-reports/2026-11-03-996-dashboard-page.md`；元件本身的在 `2026-11-01-895-dashboard-components.md`。

## 儀表板元件

`src/dashboard/` 是儀表板元件模式的五種元件（Laysi/trustiot-v2#895；設計在 [AI 助理與 AI 閘道](../docs/architecture/ai-assistant.md)「元件模式」與「兩條路的正確性」）：時間序列（折線）、長條、KPI 數字與迷你走勢、表格、狀態（門檻）。元件畫在 SPA 自己的頁面裡，不經沙箱。儀表板頁（Laysi/trustiot-v2#996，見「儀表板頁」）、對話的草稿面板與回答（Laysi/trustiot-v2#1045、#1049，見「對話畫面」）都用它們。

```tsx
<DashboardComponent spec={spec} data={data} />
```

- `spec` 是版本的元件清單裡的一項（`ComponentSpec`）：型別與 contract 的 `DashboardComponent`（`api/openapi.json` 產生的 `schema.gen.ts`）是同一個，兩邊以 `src/lib/type-binding.ts` 綁定，contract 一變 `typecheck` 就失敗（同 `src/api/devices.ts`；Laysi/trustiot-v2#987）。所以頁面把 `GET /api/assistant/v1/dashboards/{id}` 的 `version.spec.components` 逐項交給元件，資料集、計算與常數也直接用 `version.spec` 的，不需要轉換層。`custom_page` 是清單的一項，但不由元件畫：頁面以 `isPlatformComponent` 分辨，元件只收其他五種（`PlatformComponent`），自訂頁面連同版本的 `page` 交給沙箱。外部來的規格先經 `parseComponentSpec`（zod）才交給元件，不認得的種類與欄位一律拒絕，顏色與樣式也是。儀表板頁讀 API 時逐項解析（`src/api/dashboards.ts` 的 `ShownItem`）：解析不過的那一項顯示「這一項無法顯示」，其他項目照常。可選欄位 `series`、`unit`、`compare`、`trend` 允許 `null`，與沒有這個欄位相同。`data` 是版本的資料集、具名計算與常數，加上這次檢視每個資料集的狀態：讀取中、失敗（訊息已翻譯）或結果（`DashboardData`）。讀取資料集是頁面的工作，元件只畫拿到的東西。
- `DashboardComponent` 從偏好設定取語言與時區；測試或沒有工作階段的地方以 `DashboardEnvProvider` 包住 `DashboardComponentView`。
- schema 管得到的限制與 contract 一致（欄位名稱、`y` 1 到 6 欄、表格 1 到 16 欄、門檻最多 4 個、標題與單位的長度）。要看資料集才知道的限制在 `model.ts`：值的 `row` 要小於它的資料集的 `max_rows`（`row_out_of_range`），一張圖的系列數是 `y` 的欄數乘上系列欄位在結果裡的值數、最多 6（`series_too_many`）。長條與時間序列共用 contract 的 `SeriesChart`：`y` 可以有多欄，也可以同時有 `series`，每一組（系列值、`y` 欄）一個系列，名稱是「系列值 · 欄位」（只有一欄 `y` 時是系列值，沒有 `series` 時是欄位名稱）。時間序列只畫折線：contract 沒有選面積的欄位（2026-11-02，Laysi/trustiot-v2#987）。

| 檔案 | 內容 |
| --- | --- |
| `spec.ts`、`data.ts` | 規格的 schema（綁定 contract 的 `DashboardComponent`）；資料的型別（取自 `api/openapi.json` 產生的 client）與讀取函式 |
| `model.ts` | 唯一把規格與資料對應成畫面的地方：欄位存在、型別相符、同一個系列的同一個 x 只有一列、參照都存在；不符時是封閉的 `Problem`，訊息在 `problems.ts`：contract 寫入時也檢查的問題（欄位不存在、型別不符、同一個 x 兩列、系列太多、列號、引用不存在、門檻）用 contract 的訊息（`dashboard.spec.*`），所以使用者在助理的回覆與儀表板上讀到同一句話；只有檢視時才知道的（資料集失敗、x 是空的、計算算不出來）用 `dashboard.problem.*` |
| `format.ts` | 每個顯示的數字與時間都經過它（`DashboardFormat`） |
| `DashboardComponent.tsx` | 卡片：標題與單位，下面是資料、讀取中、錯誤或沒有資料的說明，最下面是來源；每張卡片各有一個 error boundary |
| `charts.tsx`、`values.tsx`、`parts.tsx` | 時間序列、長條與迷你走勢；KPI、狀態與表格；圖例與 tooltip 的一列 |
| `SourcePanel.tsx` | 看來源（Laysi/trustiot-v2#1175）：資料集的標題、說明、查詢時間、資料範圍（這個資料集結果裡時間欄位的最早與最晚，沒有時間欄位就不顯示）、筆數（整個結果的列數，不論有沒有點到列）、查詢本身與點到的列；SQL 與統計、用量查詢的原始參數各有複製鈕，成功與失敗（瀏覽器拒絕或沒有剪貼簿）都寫在一個 `role="status"` 裡（「已複製 SQL。」、「無法複製，請手動選取。」）；具名計算的算式與它讀的資料集；常數的值與說明。底部是頁面經 `explainSource` 加的「請 AI 解釋」（沒有提供就沒有） |
| `RequestSummary.tsx` | 統計與用量查詢的白話摘要：資料點（設備名稱，取不到時顯示 ID）、期間（相對期間，或起訖兩個時間點以檢視者的時區顯示；不認得的相對期間照原文）、彙總間隔（沒寫就是整段期間一筆）與時區；原始參數另外留在面板裡，也能複製 |
| `range.ts` | 結果的資料範圍：`resultTimeRange`（一個結果）與 `mergeTimeRanges`（儀表板頁的「資料時間」用，`pages/dashboards/dataFacts.ts` 的 `dataTimeRange`） |
| `chart/` | ECharts 的介面（`engine.ts`）與實際的載入（`echarts.ts`）、每張圖共用的互動（`InteractiveChart.tsx`）、預設樣式（`options.ts`）、資料缺口的計算與斜線底（`gaps.ts`，設備頁共用） |

- **數字只來自資料**：畫面上的每個數字（圖上的點、tooltip、表格的格子、KPI、門檻的標籤）都是結果的一格、具名計算的結果或常數，經 `DashboardFormat` 格式化：最多 3 位小數，絕對值小於 1 時取 3 位有效數字，只為顯示而四捨五入，不加總、不平均、不換算單位。依系列欄位分線或並排只是把列分開。缺值（`null`、NaN）顯示 `—`，折線在那裡斷開；前後都沒有值的一點另外畫成小點，不會消失。座標軸的刻度是位置，不是資料值。具名計算以沙箱的同一份程式（`sandbox/src/formula.ts`）求值。
- **資料缺口斷線**（Laysi/trustiot-v2#1144）：時間序列與 KPI 的走勢，同一個系列相鄰兩點的間隔超過該系列中位數間隔的 3 倍（至少 3 秒）時，折線在缺口中間以一個 `null` 資料點斷開，時間序列再把缺口畫成斜線底（`markArea`），並只在全圖最長的缺口標上「10 小時沒有資料」（標籤比大多數缺口寬，每天關機的一週有七個缺口，每個都標會疊在一起）；KPI 的走勢只斷線。規則與設備頁相同，由 `chart/gaps.ts` 計算，設備頁的 `gapsOf` 也用它。只看兩點之間的缺口，不在 x 軸兩端憑空畫缺口；補的 `null` 點只存在於交給 ECharts 的資料，所以 `xs`（鍵盤逐點移動）、tooltip 與來源仍只有實際的列，`charts.tsx` 經 `lineLayout` 在 ECharts 的資料索引與列之間轉換。有缺口的線不做 `lttb` 抽樣（抽樣可能丟掉斷開的點）；設備頁自己斷線，以 `ownBreaks` 告訴圖表不要再斷一次。
- **時間**：與沙箱相同（[sandbox/README.md](sandbox/README.md)「時間」），保留結果寫出的牆上時間，看到的是查詢當地的時間，精度照資料（有毫秒就顯示毫秒）。只有查詢時間（`queried_at`，時間點）以檢視者偏好的時區顯示。
- **檢視時不符就不畫**：資料集沒有那一欄、欄位型別與元件不符（例如時間序列的 y 變成文字）、同一個系列的同一個 x 有兩列、x 是空的、參照的列不存在、計算或常數不存在或不能用、門檻不是由低到高時，元件顯示原因與看來源的按鈕，不猜、不畫。資料集還在讀取時顯示佔位，讀取失敗時顯示頁面給的訊息；結果沒有列時說明並提供來源。
- **系列最多 6 個**（`MAX_SERIES`）：一個系列一個顏色，依順序配色，不循環、不產生新顏色；系列數是 `y` 的欄數乘上系列欄位在結果裡的值數（沒有系列欄位時就是 `y` 的欄數），超過時是錯誤，要 SQL 先分組。這也是沙箱的系列色數。隱藏一個系列時，其他系列的顏色不變。
- **顏色**：系列色是 `global.css` 的 `--chart-1`～`--chart-6`，淺色與深色各一組，是設計系統的 `chart-series-1`～`chart-series-6`（`docs/design/system/tokens.css`）：第一色是藍，品牌色相在第三。每色對卡片與頁面底色至少 3:1，順序相鄰的兩色在紅色盲與綠色盲的模擬下仍分得開；不相鄰的赭（色 4）與橘（色 2）最接近，一般視覺下只差 9.2（淺色）與 8.2（深色），綠色盲模擬下幾乎相同，所以系列一律有名稱（數值在 [測試報告](../docs/test-reports/2026-11-02-1003-chart-series-palette.md)，改顏色要重跑那份檢查）。文字不用系列色：圖例與 tooltip 的名稱旁放色塊，不只靠顏色辨識。狀態元件不把區間塗成好或壞（規格沒說哪個好），只以主色標出值所在的區間，並用文字寫出。
- **互動**：時間序列與長條有 tooltip（我們的 React 元件：時間或類別，與每個看得到的系列的值）、十字線（長條是陰影）與圖例按鈕（切換時保留縮放）。時間序列另有滾輪縮放、滑鼠拖曳框選縮放、觸控的雙指縮放與單指左右平移，縮放後出現回到完整範圍的按鈕；縮放只在已載入的結果裡。點圖上的點（聚焦時按 Enter）、KPI 與狀態的值、狀態的門檻、表格的格子都會打開來源，焦點移到來源的標題。
- **版面**：卡片寬度由頁面決定，可以放進 grid 的格子。圖高 288 px，迷你走勢 56 px。表格最高 384 px，超過時在卡片裡捲動、表頭固定；窄螢幕時表格左右捲動，數字、時間與短文字不換行，超過 24 個字元的文字才在有上限的寬度裡換行。手機寬度沒有橫向捲動。
- **元件出錯只影響自己的卡片**：元件本身的錯誤（render 時丟出的例外，或 ECharts 拒絕 option）由每張卡片的 error boundary 接住，那張卡片保留標題，改顯示無法顯示的說明，頁面的其他元件照常；沒有它時 React 會把錯誤交給路由的錯誤頁。新的規格或資料（下一次檢視）會再試一次。
- **資料的名稱不當成查表的 key**：資料集的結果以 `Object.hasOwn` 查，名稱叫 `constructor` 或 `__proto__` 的資料集不會查到 `Object.prototype` 的成員；其餘以名稱查的地方用陣列、`Map` 或 `Set`。
- **測試**：`contract.test.tsx` 讀 contract 的固定樣本（`crates/contract/tests/fixtures/dashboard/`：`spec-components.json`、`dashboard-view-components.json` 與各資料集的結果），確認樣本的每個元件都讀得進 schema、畫得出來，畫面上的數字等於資料值格式化的結果；`DashboardComponent.test.tsx` 以 `src/test/fake-chart.ts` 的假引擎（經 `ChartEngineContext`；happy-dom 沒有 canvas）讀取元件交給 ECharts 的 option 與 action，並送出 ECharts 會送的事件（tooltip、點擊、拖曳、縮放）；期望值以同一個 `dashboardFormat` 從 `fixtures.ts` 的資料算出。`chart/options.test.ts` 以 SVG renderer 的伺服器端模式（不需要 canvas）跑真的 ECharts，把每種圖的 option 與圖例動作交給它，包括系列、類別與欄位名稱是 `hasOwnProperty`、`__proto__`、`constructor`、`toString` 的資料（`fixtures.ts` 的 `prototypeNameData`）。`format.test.ts` 以寫死的字串檢查格式本身。`fixtures.ts` 的 `GAP_SPECS`（資料集 `GAP_DATASETS`、結果 `gapResults()`，不在 `fixtureData`）是一個冷藏室一週的溫度與濕度（每小時一筆，兩個晚上各有約 16 小時沒有列，另有一個剛好 3 倍間隔、不算缺口的小洞）與一個走勢有缺口的 KPI；UI 預覽的儀表板「Cold room climate (data gaps)」用它，可以在預覽看到斷線與斜線底。ECharts 實際畫出的圖、CSP 與 Trusted Types、動畫、觸控與對比只能在瀏覽器確認，方法與結果在測試報告。

## 儀表板元件的圖表套件

儀表板的元件模式（決策 33 的 2026-11-01 修訂，[AI 助理與 AI 閘道](../docs/architecture/ai-assistant.md)「儀表板」）：時間序列、長條、KPI 與迷你走勢、表格、狀態，由平台寫的元件畫在 SPA 自己的頁面裡（儀表板頁與對話的回答），不經沙箱與 `dashboard-check`，所以 canvas 可以用。元件內部用的圖表套件要符合三個硬性條件：在本頁的 CSP 下沒有 enforced 違規，Trusted Types 改成強制後也不違規（設定成不違規也算）；授權可用於商業 SaaS；能在 React 裡用。「點數字看來源」與無障礙（鍵盤、螢幕閱讀器）要做得到。符合條件之後依序比功能與體驗、效能、外觀；載入量不是條件。

**選定 ECharts 6，canvas renderer**（2026-11-01，Laysi/trustiot-v2#927）。比較了 ECharts（canvas 與 SVG）、uPlot、Plotly.js、AG Charts Community、Recharts 與自己寫的 d3，方法、數字與原始資料在 [測試報告](../docs/test-reports/2026-11-01-927-chart-library-ux.md)；只比 CSP 的第一輪在 [另一份報告](../docs/test-reports/2026-11-01-927-chart-library-csp.md)。選它的原因：

- 它是唯一內建全部互動的開源候選：tooltip、十字線、圖例切換、滾輪與框選縮放、範圍導覽、動畫、觸控的點按與雙指縮放、深淺主題。
- 加上下面的根元素保護後，沒有 CSP 與 Trusted Types 違規，卸載時也沒有。
- 10 萬點的時間序列在建置主機上 64 ms 畫完，縮放與 hover 60 fps；CPU 降速 4 倍時 219 ms，縮放 50 fps。
- 顏色與字型都能從設計系統的 token 設定，tooltip 是我們自己的 React 元件。

沒選的：uPlot 最快，但沒有動畫、滾輪縮放、範圍導覽與觸控手勢；Plotly.js 的地圖模組會插入 `<style>`、時間軸只能用 UTC；AG Charts Community 會插入 `<style>`，縮放與十字線要買 Enterprise；Recharts 沒有縮放，10 萬點時明顯變慢；d3 每一項都要自己寫。商業套件（Highcharts、amCharts）的功能 ECharts 都有，不買。

`package.json` 鎖定 `echarts` 6.1.0（Laysi/trustiot-v2#895），元件這樣使用它（`src/dashboard/chart/`）：

- **只 import 用到的模組**：`echarts.ts` 從 `echarts/core` 註冊折線、長條、grid、tooltip、axis pointer、legend（只用它的選取狀態，畫面上的圖例是我們的按鈕）、inside dataZoom、mark line、mark area（設備頁的缺區間）、aria 與 `CanvasRenderer`；不 import 整包 `echarts`，也不用 SVG renderer（卸載時仍有一個 Trusted Types 違規；只有測試以同一組模組加上 SVG renderer 跑伺服器端模式）。`engine.ts` 以 `import()` 載入 `echarts.ts`，所以 ECharts 是自己的 chunk，只有畫圖的頁面才下載（2026-11-01 的預覽建置：575,482 bytes，gzip 後 192,095 bytes）。元件只經 `engine.ts` 的 `ChartInstance` 介面使用它。
- **根元素保護**：zrender 以 `root.innerHTML = ""` 清空根元素，Trusted Types 強制後會被擋。`createChart` 建立圖表前以 `guardRoot` 在根元素上定義自己的 `innerHTML` setter：空字串或 `null` 時改用 `replaceChildren()`，其他字串一律丟出錯誤。
- **不讓 ECharts 寫 HTML**：tooltip 設 `showContent: false`，以 `showTip`／`hideTip` 事件驅動我們自己的 React tooltip，位置以 CSSOM 設定；不用 HTML tooltip、toolbox 的 `dataView`。ECharts 自己畫的文字只有座標軸刻度與門檻的標籤，formatter 回傳純文字，並經過 `chartText`（下一項）。十字線不在座標軸上顯示標籤：縮放到兩點之間時，它會顯示一個沒有任何一列的時間；那一點的時間由 tooltip 寫出。數字一律以元件的格式化函式產生（決策 33：只顯示資料集的值格式化後的結果）。
- **資料的名稱不交給 ECharts**：ECharts 把圖例的選取與系列的顏色存在以系列名稱為 key 的一般物件裡，還會呼叫它的 `hasOwnProperty`，所以系列值叫 `hasOwnProperty` 時 `setOption` 丟出錯誤。option 裡的系列名稱一律是元件產生的 `s0`～`s5`（顏色序號，`seriesName`），門檻的輔助系列叫 `thresholds`，mark line 不帶名稱，長條的類別以序號交給 ECharts、標籤由 formatter 畫出；資料的名稱只出現在我們的圖例、tooltip 與來源。ECharts 畫的標籤是資料的文字唯一進到它裡面的路：zrender 以一般物件快取每一行文字的寬度，以那行文字為 key，一行叫 `__proto__` 時，快取的鏈結串列會把 `prev` 與 `next` 寫到整頁的 `Object.prototype` 上。`chartText` 在這種行（`Object.prototype` 有的名稱）前面加一個不佔寬度、畫不出來的 U+2060（word joiner），讓它成為自己的 key。
- **新資料不移動目前的點**：tooltip 與鍵盤 slider 目前停在哪一點，以那一點的時間（長條是類別）保存（`stopKey`），不是停留點的序號，所以設備頁每分鐘更新、儀表板重新查詢時，同一個時間還在新資料裡就留在那一點，`aria-valuenow` 與 `aria-valuetext` 跟著它的新序號，tooltip 以 `showTip` 動作放回那一點的新位置（不是舊的滑鼠座標）；那個時間已不在，就關閉 tooltip，鍵盤位置回到最後一點（和沒選一樣）。`renderTip` 對某個停留點沒有內容時不畫 tooltip 框，不留空框。放大在新 option 進來時照舊重來。
- **點數字看來源**：tooltip 指到的位置（最後一次 `showTip` 的時間或類別）就是被點的位置；zrender 畫布的 `click` 打開那個位置上每個看得到的系列的列，鍵盤按 Enter 或空白鍵也一樣。滑鼠框選縮放結束的那次 click 不算；拖曳後 zrender 不一定送出 click，所以下一次按下時就清掉這個標記。
- **無障礙**：canvas 本身 `aria-hidden`，外面一層是 `role="slider"`，與設備頁的圖表相同：整張圖一個 tab stop，方向鍵、PageUp／PageDown、Home／End 逐點移動並以 `dispatchAction({ type: "showTip" })` 顯示 tooltip，`aria-valuetext` 是那一點的時間或類別與每個系列的值，Escape 收起 tooltip。canvas 的圖例關閉，改用 `aria-pressed` 的按鈕；ECharts 的 `aria` 打開，描述文字走 i18n。
- **時間**：結果的時間是牆上時間（上一節的「時間」），所以設 `useUTC: true`，以 UTC 欄位交給 ECharts。座標軸的標籤用我們的 formatter，依刻度取最細的單位：有毫秒的刻度顯示毫秒、午夜的刻度顯示日期、跨一年以上時每月 1 日顯示年月，所以放大時標籤不會重複。
- **大量資料**：折線設 `sampling: "lttb"`；一條線超過 60 點時不畫點（`showSymbol: false`），前後都沒有值的單獨一點另外以一個同名、不回應指標的輔助系列畫成小點。
- **門檻**：門檻是 mark line，加上一個看不見、不進 tooltip 的輔助系列在門檻的值放點，讓數值軸延伸到每條門檻；不固定軸的兩端，所以刻度仍是整齊的數字。輔助系列都排在資料系列之後，資料系列的序號就是它的顏色序號。
- **動畫**：進場 300 ms、更新 200 ms；`prefers-reduced-motion: reduce` 時 `animation: false`，直接畫出結果。
- **滑鼠與觸控**：一個 inside dataZoom，只縮放 x 軸。滑鼠以滾輪縮放、拖曳畫出範圍（選取框由我們畫，放開後送 `dataZoom`）。沒有精確指標（`pointer: fine`）時改成雙指縮放、單指左右拖曳平移（`moveOnMouseMove`）。圖的 `touch-action` 是 `pan-y`，並設 `preventDefaultMouseMove: false`：可以平移時 ECharts 預設會取消圖上的每個 touchmove，手機上手指放在圖上就捲不動整頁。最小的縮放範圍是相鄰兩點最小間隔的兩倍（`minValueSpan`），不會放大到兩點之間的空白。
- **主題**：每次設定 option 時從 CSS 變數讀取顏色與字型（`readTheme`），換成 `rgba()` 再交給 ECharts（zrender 自己解析顏色，不認得 `oklch()`）；`<html data-theme>` 改變時重新設定。
- **生命週期**：載入與建立分兩步（`ChartEngine` 載入後交回建立圖表的函式），只有仍然掛載的元件才建立圖表：StrictMode 或很快的重新掛載時，兩次載入可能都在第一次掛載清理之後才完成；如果兩次都建立圖表，ECharts 對同一個元素的第二次 `init` 會交回第一張圖，而第一次掛載會把它 dispose，留下一張不能用的圖。元件卸載時 `dispose()`，尺寸跟著容器的 `ResizeObserver` 呼叫 `resize()`。新的資料、主題或動畫設定以 `notMerge` 取代整個 option（縮放回到完整範圍，圖例的選取保留）；圖例切換只送 `legendSelect`／`legendUnSelect`，縮放不變。
- **升級**：每次升級 ECharts 都要在瀏覽器重跑 [元件的測試報告](../docs/test-reports/2026-11-01-895-dashboard-components.md) 的檢查：CSP 與 Trusted Types、互動、動畫、觸控與對比（把它自動化另開 issue）。資料的名稱與 `Object.prototype` 的檢查在 `chart/options.test.ts`，`bun run test` 會以新版本重跑。

d3 只給沙箱的自訂頁面用（「儀表板沙箱」），SPA 的元件不用 d3。

## 首頁的設定清單與通用首頁

Laysi/trustiot-v2#129，`src/pages/home/`；設計與和設計稿不同的地方在 [`docs/design/home-ai/design.md`](../docs/design/home-ai/design.md)。兩個畫面的 API 都還沒有，各有介面、內容提供者與假服務：

| 畫面 | 介面（`src/api/pending/`） | 假服務（`src/test/`） | 在哪裡出現 |
| --- | --- | --- | --- |
| 設定清單 `SetupChecklist` | `setup.ts`（`SetupApi`、`SetupApiProvider`、`useSetupApi`） | `fake-setup.ts` | 首頁 `HomePage` 的問候標題上方，經 `pages/home/HomeSlot.tsx` 的空位：正式版的空位是空的（`check:dist` 不允許正式的程式碼引用 `src/api/pending/`），UI 預覽的 setup 假服務（`provider`）把清單放進去；只給租戶 admin（決定 1） |
| 通用首頁 `GenericHome` | `home-overview.ts` | `fake-home-overview.ts` | 只在 UI 預覽的路由 `/overview`（`pages/home/preview.ts`，沒有選單項目；`?ai=paused` 顯示 AI 暫停提示） |

- **清單的規則**：五個步驟固定，完成由各自的服務端寫入，畫面只能略過或復原；寫入成功後才換畫面（回應就是新的狀態），失敗時步驟維持原狀並在步驟裡說明。第一筆資料的等待每 30 秒重新讀一次。全部完成或略過後換成一次性的「設定完成」提示，關閉由租戶決定（決定 2、3）。窄於 768 px 時是精簡版（進度加目前步驟，「全部步驟」展開）。
- **主按鈕只連到存在的頁面**：`useRouteExists` 以路由表判斷，目標頁面（閘道器、新增設備、成員）上線後按鈕自動出現。
- **通用首頁的每個區塊各自查詢、各自失敗**；四個都失敗才顯示整頁的連線錯誤。沒有閘道器、等待第一筆讀數時用 `EmptyState` 說明下一步。
- **測試**：`SetupChecklist.test.tsx`、`GenericHome.test.tsx` 以 `test-support.tsx` 的路由（提供假服務與指定存在的頁面）涵蓋設計稿畫出的每個狀態與角色；`preview.test.tsx` 照 `main.tsx` 的 glob 載入預覽，確認兩個假服務、`/overview` 路由、normal 與 empty 的資料，以及 viewer 看不到清單（角色來自工作階段）。
- **API 上線時**：照「UI 預覽」的「後端完成時要改的地方」，把兩個介面換成產生的 client 上的模組、假服務改成 `http`，`HomePage` 直接放 `SetupChecklist`（刪掉 `HomeSlot.tsx`），`/overview` 改進正式路由。

## 報表畫面

報表的三個畫面（Laysi/trustiot-v2#174；設計稿與不同之處在 `docs/design/ms4-ms5/design.md` 第九節）：`/reports`（排程卡片與已產生的報表）、`/reports/:reportId`（檢視）、`/reports/schedules/new` 與 `/reports/schedules/:scheduleId/edit`（表單；`?preset=weekly&from=onboarding` 是設定清單第 4 步的版本）。

- **後端還沒有（Laysi/trustiot-v2#173），所以只在 UI 預覽**：路由與選單項目在 `src/pages/reports/preview.ts`，不在 `routes.tsx` 與 `menu.ts`。API 介面與型別是 `src/api/pending/reports.ts`（第一行寫待接的 issue），頁面經 `ReportApiProvider` 取得；假服務是 `src/test/fake-reports.ts` 的 `FakeReportServer`（記憶體裡的實作，`seedNormal()` 是設計稿的正常狀態，`plan = "starter"`、`failNext`、`hold`、`finish()` 供測試），它的 `preview` 是 provider 形式。預覽工具列碰不到 provider 形式的假服務，所以失敗、載入中與 Starter 用網址參數：`?fake-reports=error|slow|starter`，改了網址後重新載入。
- **後端做好時**：照「UI 預覽」的「後端完成時要改的地方」：在 `src/api/reports.ts` 以產生的 client 寫模組與 zod schema、刪掉 `api/pending/reports.ts`，假服務改成 HTTP 層，路由與選單項目搬進 `routes.tsx` 與 `menu.ts`。介面裡後端沒有明講、由畫面先假設的規則（沒有報表權益的方案是 403 `entitlement.feature_not_in_plan`、建立與更改排程要 editor、`send_on` 是星期或日期、報表內容是存下來的資料集）列在 `api/pending/reports.ts` 的開頭。
- **誰能做什麼**：所有成員看報表、下載 PDF 與 CSV；editor 與 admin 建立、編輯、開關、刪除排程、立即產生、重試失敗的報表。角色讀 session（`useReportRole`），權限由伺服器強制，畫面只是不放按鈕；viewer 打開表單網址時顯示 `WriteDenied`。
- **下載**：`useDownload` 取得檔案後交給 `FileSaver`（瀏覽器版是 object URL 加 `<a download>`，測試換成記錄的版本）；檔名由 `downloadName` 以報表類型與期間開始日組成，兩者都驗證過才用，不用伺服器給的標題或標頭。
- **進行中的報表**：列表在有報表處於「產生中」時每 5 秒重新讀一次，直到它們完成或失敗。
- **「問 AI」**：用共用的 `components/ask/AskAiButton`（見「情境 AI 按鈕與建議問題」），沒有自己的按鈕。問句範本在 `components/ask/questions.ts`（`reportQuestions`、`lastMonthUse`、`thisWeekUse`，文字是 `ask.report.*`），報表名稱與期間以 `context` 接在問句後面，`back` 讓新對話頂端回到這份報表。
- **設定清單的選項**（Laysi/trustiot-v2#1134）：`WeeklyReportStep` 放在設定清單第 4 步「選第一個用途」的卡片下面。報表只在 UI 預覽，而正式版不得含 `src/api/pending/`（`check:dist`），所以清單只認 `pages/home/first-use-slot.tsx` 的插槽（正式版是空的），UI 預覽的報表 provider（`src/test/fake-reports.ts`）把 `WeeklyReportStep` 放進去；報表 API 做好時，`SetupStepList` 直接放它、刪掉插槽（做法與 `HomeSlot.tsx`、`suggestions-slot.tsx` 相同）。表單存完後回到 `/`，路由 state 是 `onboarding.ts` 的 `FIRST_USE_DONE`；清單（`SetupChecklist`）讀到它就以 `weekly_report` 呼叫 `chooseUse` 完成第 4 步，並把 state 從這筆網址紀錄拿掉，重新整理不會再選一次。測試在 `src/pages/home/FirstUseReport.test.tsx`（有報表頁與沒有的建置、存完回首頁並打勾）。
- **測試**：`src/pages/reports/*.test.ts(x)` 以 `src/test/render-reports.tsx`（把頁面放在租戶的路由下，和預覽一樣）與 `FakeReportServer` 覆蓋列表、檢視、表單、角色、Starter、載入與失敗、下載與預覽的宣告。

## 輸入框的補全：@ 提及與問句範本

Laysi/trustiot-v2#155，`src/chat/completion/`；設計與和設計稿不同的地方在 [`docs/design/home-ai/design.md`](../docs/design/home-ai/design.md)。確定性補全，不經 LLM。

- **接在哪**：`chat/Composer.tsx` 呼叫 `useCompletion({ value, onChange })`，用 `CompletionField` 取代 `textarea`（`textarea` 照舊是原生的，文字透明，鏡像層畫提及與空格的底色），在工具列放 `CompletionTools`，鍵盤先交給 `completion.onKeyDown`，`onSubmit` 多帶 `{ mentions }`。文字由頁面持有；hook 只多持有開著的選單與每個提及對應的結構化資料（`mentions`），隨每次編輯移動，編輯到提及裡面它就變回一般文字。
- **資料**：設備、型號與資料點用設備頁的 API 與 query key（`useMentionSources`，只在選單開著時查，`staleTime` 30 秒），時間範圍在前端算（`time-ranges.ts`）。key 都以目前租戶的 ID 開頭，租戶切換時頁面重新掛載、租戶的 query 被清掉。篩選與排序的規則在 `groups.ts`（純函式）。
- **範本**：API 還沒有（`api/pending/question-templates.ts`），`chat/completion/templates.tsx` 的 `TemplateSourceProvider` 預設沒有提供者，所以正式版沒有「範本」鈕與 `/`；UI 預覽與測試掛 `src/test/fake-question-templates.ts`。
- **時間與時鐘**：`CompletionConfigContext` 給骨架列的延遲（300 ms）、結果數朗讀的延遲（400 ms）與時鐘；測試給短的延遲與固定的時鐘，等的是「發生了什麼」的順序，不是經過的時間。
- **測試**：`text.test.ts`（純函式與時間範圍）、`Completion.test.tsx`（鍵盤、狀態、範本，以 `Composer` 加假設備服務）、`TenantIsolation.test.tsx`（兩個租戶的假服務、開著選單時切換租戶、舊租戶遲到的回應）。

## 輸入框的灰字續寫

Laysi/trustiot-v2#156（前端的一半），`src/chat/completion/`；設計與和設計稿不同的地方在 [`docs/design/home-ai/design.md`](../docs/design/home-ai/design.md)。續寫的端點、`complete` 模型角色（Laysi/trustiot-v2#157）與帳本的不計額度標記（Laysi/trustiot-v2#140）都還沒有，所以**只在 UI 預覽出現**：

- **介面與內容提供者**：`src/api/pending/completion.ts`（`CompletionApi`，請求帶 `AbortSignal`；形狀是假設，列在 PR 說明）與 `CompletionApiProvider.tsx`。正式的 `Composer` 只讀 `src/chat/completion/context.tsx` 的 `useCompletionSettings()`，預設是 `null`，沒有提供者就沒有灰字、不送請求、`Tab` 照常移動焦點（`check:dist` 不允許正式的程式碼引用 `src/api/pending/`）。端點上線時，提供者換成產生的 client 上的模組並掛進 `AppProviders`。
- **假服務**：`src/test/fake-completion.ts`（`preview` 是 provider）。給示範用的續寫，開關有 `hold()`/`release()`（慢）、`failNext()`/`failing`（失敗）、`limit` 與 `limitFor()`（每人限流：429 `rate_limited` 加 `details.retry_after_secs`）、`honorAbort = false`（不理會取消的服務）。在預覽裡，文字含 `[slow]` 會 3 秒後才回、`[fail]` 會失敗、`[limit]` 會被限流 60 秒。
- **接到輸入框**：`useGhostCompletion`（邏輯）、`GhostTail`／`GhostToolbar`／`GhostHint`（灰字、接受鈕與限流說明、說明文字），`Composer.tsx` 只多幾行接線。灰字不另外疊鏡像層：`GhostTail` 交給提及的 `CompletionField` 當 `tail`，畫在同一個鏡像層的文字後面；`useGhostCompletion` 的 textarea 事件（點擊、選取、失焦、組字）由 `CompletionField` 接在提及自己的事件之後。提及選單開著或範本還有空格時，`Composer` 傳 `paused`：這時不出灰字、不送請求、`Tab` 不接受；按鍵先給 `completion.onKeyDown`，再給 `ghost.handleKeyDown`，最後才是 `Enter` 送出。鍵盤事件已被別人 `preventDefault` 的 `Tab` 也不會再被接受。
- **時間**：停頓預設 250 毫秒（`DEFAULT_COMPLETION_DELAY_MS`），提供者的 `delayMs` 與 `clock`（`setTimeout`、`clearTimeout`、`now`）可以換，測試用手動撥動的時鐘，不等真實時間。
- **測試**：`GhostCompletion.test.tsx`（和提及的 `Completion.test.tsx` 放在同一個目錄，檔名只差大小寫的話，不分大小寫的檔案系統會把兩個當成同一個檔）：停頓、下一個按鍵取消（signal 被中止、晚到的回應丟棄，服務不理會取消也一樣）、限流期間不送請求、`Tab` 只在有灰字時接受、`Esc`、提及選單開著時 `Tab` 選項目而不接受灰字、範本還有空格時暫停、選完提及後 `Tab` 接受灰字而提及保留、輸入法組字、手機的接受鈕、失敗不顯示。

## 情境 AI 按鈕與建議問題

Laysi/trustiot-v2#158，`src/components/ask/`（按鈕與問句範本）、`src/pages/home/SuggestedQuestions.tsx`（建議問題）；設計與和設計稿不同的地方在 [`docs/design/home-ai/design.md`](../docs/design/home-ai/design.md)「情境 AI 按鈕」與「和設計稿不同的地方（Laysi/trustiot-v2#158 的實作）」。按鈕用的端點（建立對話、提問）都在 `api/openapi.json`，所以按鈕直接在正式的頁面裡；建議問題的端點還沒有，只在 UI 預覽。

- **一個元件**：`AskAiButton`（星芒膠囊）給所有「問 AI」入口：`question` 是送出的文字，`label` 是按鈕上的字（沒給就是問句本身），`context` 是這個問句關於什麼（規則、設備、時間），以全形括號接在問句後面，`icon`、`size`、`back` 見元件的註解。按下就送出（設計決定 4）；送不出去時原因寫在按鈕下方，`role="alert"`、以 `aria-describedby` 連到按鈕（設計 12-D），文字是服務端的錯誤訊息 key（額度、權限、服務暫停各有自己的說明）。等待中按鈕保持可聚焦（`aria-disabled`），再按一次不會再送。
- **問句是固定範本**（`questions.ts`）：把設備名稱、時間範圍與元件標題填進範本，不經 LLM，所以每次相同、可以測；每個範本有按鈕上的 `label` 與送出的 `text`。API 的 `AskQuestion` 只有 `question`，所以設備與時間範圍寫在問句文字裡（結構化的設備 ID 與範圍要等 API 有欄位）。週從星期一開始，日期與時間用使用者偏好的時區並寫出時區名稱。
- **按下後去哪**由按鈕在哪裡決定（`useAskQuestion`）：不在對話裡（設備頁、警報與報表）就建立新對話、在裡面提問、打開它的頁面（路由 state 帶 `AskedState`，與首頁輸入框相同）；`back` 讓新對話的頂端出現「回到 <名稱>」（`BackToOrigin`，連結是按按鈕那一刻的網址，含時間範圍，且只接受本 app 的路徑）。在對話裡的按鈕（回答中的元件）用 `ConversationAskProvider`（`pages/conversations/ConversationAsk.tsx`）在同一個對話提問，`ConversationTurn` 像追蹤輸入框送出的問題一樣追蹤它；對話正在回答時按鈕停用，因為服務一次只回答一個問題。唯讀的分享對話沒有提供者，也就沒有按鈕。
- **放在哪**：設備頁頁首（`DeviceAsk`：資料停滯時「為什麼離線？」、從沒有讀數時「為什麼沒有資料？」、有讀數時「跟上週比」；全部開新對話），設備頁的圖放大後（`DeviceStretchAsk`）與回答中的時間序列元件放大後（`AskInAnswer`）有「分析這段」，回答中的每個元件卡底部有「問 AI」列（`ComponentAskRow`，依種類 2 到 3 個問題）。圖的選取就是圖原有的放大（拖曳、滾輪、雙指），沒有另外做一套選取。
- **給元件的擴充點**：`src/dashboard/extensions.tsx` 的 `ComponentExtrasProvider`（`footer`：元件卡底部；`stretch`：放大中的圖；`explainSource`：資料來源面板底部的「請 AI 解釋」；`deviceName`：面板的統計查詢摘要裡以名稱寫設備）。內層提供者沒寫的欄位沿用外層的。沒有提供者時，元件與儀表板頁畫的一樣；`DashboardComponent.tsx`、`SourcePanel.tsx`、`charts.tsx` 與 `HistoryChart.tsx` 只多讀它，`src/dashboard/` 不依賴頁面或 `components/ask/`。
- **資料來源面板的「請 AI 解釋」**（Laysi/trustiot-v2#1175）：只有儀表板頁的面板有（`components/ask/SourceAsk.tsx` 的 `DashboardSourceAsk` 包住頁面的元件清單）：按下以 `explainSource` 範本（帶入儀表板名稱與資料集、計算或常數的標題）開新對話，`back` 回到這個儀表板（網址含 `?version=`）。對話裡的面板（回答中的儀表板、草稿面板）沒有：回答的元件底下已有「問 AI」列，輸入框也在同一段對話裡，理由見 `docs/design/chat-dashboards/design.md`。
- **面板裡的設備名稱**：`pages/device/DeviceName.tsx` 的 `DeviceNames` 提供 `deviceName`，讀設備頁的設備列表（`GET /api/assets/v1/devices`，租戶 viewer 以上都能呼叫，所以每個能開儀表板或對話的成員都讀得到；同一個 query key 與 30 秒的 `staleTime`，一個面板的所有名稱只有一次請求，面板用到名稱時才讀）；讀取中、失敗或列表沒有那台設備時顯示設備 ID。儀表板頁（`DashboardSourceAsk`）與對話頁（`ConversationDashboards`）都提供。UI 預覽有統計與用量查詢可看：儀表板「Line 2 power statistics」，以及預覽對話「Line 2 energy this week」的回答寫的草稿（設備 ID 與預覽的設備列表固定對應，`src/test/preview-ids.ts` 與 `fake-devices.ts` 的 `PREVIEW_METER_ID`；預覽沒有「對話的草稿列表」端點，所以只看得到回答裡的那份）。
- **建議問題**：介面在 `src/api/pending/suggestions.ts`（`SuggestionsApi.listSuggestions(tenant)`：服務端把範本填上這個租戶的設備與時間，回傳問句文字與參數，`has_data` 為 false 的租戶只收到說明），假服務在 `src/test/fake-suggestions.ts`（`FakeSuggestionsServer` 依租戶保存設備，沒給過的租戶回 404）。首頁的 `HomePage` 只認 `pages/home/suggestions-slot.tsx` 的空位：正式版是空的，UI 預覽的 `provider` 把 `SuggestedQuestions` 放進去。桌機最多 6 個、手機 4 個；載入失敗是一行淡色說明加重試，不擋輸入框。
- **別的畫面要放「問 AI」**：用 `AskAiButton`，不要自己寫導向首頁的連結或另一套送出；問句範本與送出的規則在這裡，別處各寫一份會對不上。
- **測試**：`questions.test.ts`（範本、週與時區）、`AskAiButton.test.tsx`（送出、回到來源、四種送不出去的原因、等待中）、`ConversationAsk.test.tsx`（回答中的元件、同一個對話、放大後的「分析這段」、儀表板頁沒有按鈕）、`DeviceAsk.test.tsx`（設備頁每個狀態、問句文字、回到設備頁）、`SuggestedQuestions.test.tsx`（只用這個租戶的設備、切換租戶、空租戶、失敗、上限）。
- **API 上線時**：建議問題照「UI 預覽」的「後端完成時要改的地方」，換成產生的 client 上的模組、`HomePage` 直接放 `SuggestedQuestions`（刪掉 `suggestions-slot.tsx`）、假服務改成 `http`。

## 測試的寫法

- `bun run test` 經 `tools/run-tests.ts` 把全部測試分成 8 段（`bun test --shard=1/8` 到 `8/8`），一段一個行程、依序執行，最後印出加總：同一個行程跑完所有檔案時，每個檔案留下的記憶體到行程結束才釋放，2026-11-05 約 1390 個測試在一個行程裡的峰值約 14 GB，`--isolate` 也沒有降低。`FRONTEND_TEST_SHARDS` 改段數；帶參數（路徑、`-t <名稱>` 或其他 `bun test` 選項）時照原樣跑一次 `bun test`，不分段。
- `src/test/setup.ts` 註冊 happy-dom，網址是 `https://app.example.test/`，並把 `react-style-singleton` 換成與建置相同的替代（見「UI 元件與樣式」）。它也讓 DOM 節點在失敗的 `expect` 與 `console.log` 裡印成自己的標記，最多 2000 字元（`src/test/print-node.ts`）：Bun 的 `expect` 原本會走遍節點能連到的一切（document、window、React 的 fiber），2026-11-06 一次失敗的 `expect(節點).toBeNull()` 產生 1.3 億到 7.3 億字元的訊息、同步花 6 到 33 秒，包在 `waitFor` 裡時連 `waitFor` 自己的逾時都無法觸發（Laysi/trustiot-v2#1138）。
- 畫面測試用 `src/test/render.tsx` 的 `renderAt(路徑, …)`：app 的 query client（401 導向登入頁，不重試）、記憶體版本的偏好副本，以及 `src/test/fake-identity.ts` 的假身分服務。假服務有使用者、租戶、伺服器端的工作階段、偏好與驗證碼信箱，回應照身分服務的 README（401、403 `session.tenant_not_member`、422 的欄位錯誤）；`browser()` 各有自己的 cookie，可以測兩個瀏覽器（登出所有裝置、在另一個瀏覽器讀到同樣的偏好）。`failNext` 讓下一個請求失敗，`holdNext` 讓它等到測試放行，`holdNextAnswer` 讓它立刻生效、但回應等到測試放行（伺服器已改變狀態、回應還在路上）。沒有指定時，`renderAt` 用一個已登入、屬於一個租戶的使用者。
- 設備頁的畫面測試以 `renderAt(…, { devices: server.api() })` 注入 `src/test/fake-devices.ts` 的假服務（見「設備頁」）；沒有指定時是一個沒有設備的假服務。
- 儀表板元件的測試以 `src/test/fake-chart.ts` 的假引擎代替 ECharts，不需要工作階段（見「儀表板元件」）。
- `src/styles/design-tokens.test.ts` 比對 `global.css` 與設計系統的 `docs/design/system/tokens.css`（Laysi/trustiot-v2#1006）：解析兩邊的淺色（`:root`，設計系統另有 `[data-theme="light"]` 與放共用值的 `:root`）與深色（`:root[data-theme="dark"]`、`[data-theme="dark"]`）宣告，展開 `var()` 別名後，兩邊都定義的 token 在兩個主題的值必須相同，不同就失敗，訊息列出 token、主題與兩邊的值。只在一邊定義的 token 不比；`global.css` 的 `@theme` 區塊（`--radius-sm` 等 Tailwind 名稱）也不解析。前端用不同名稱的 token 要在同一個 PR 補進測試檔頂端的 `FRONTEND_TO_DESIGN`（目前只有 `--chart-N` 對 `chart-series-N`），否則它不會被比對；例如實作 `--warning` 時補 `warning: "status-warning"`。測試讀 `frontend/` 以外的檔案，所以 `docs/design/system/tokens.css` 在 `scripts/ci/changes.ts` 的前端規則裡，只改它的 PR 也會跑 `frontend` 工作。
- 不依賴固定的短時間等待：SSE 用手動時鐘，畫面用 Testing Library 的 `findBy*` 與 `waitFor`。
- fixture 裡的網址一律用 `*.example.test`，email 用 `@example.com`（例如 `alice@example.com`）。
- 假服務以 `src/test/fake-devices.ts` 的 `fakeUuid()` 產生 ID，形式是 `00000000-0000-4000-a000-…`；它的計數器由同一個 `bun test` 行程裡的所有測試檔共用，測試無法知道它走到哪裡。測試手寫的 ID（必須不存在的 ID、固定的使用者）用 `00000000-0000-4000-8000-…`，`fakeUuid()` 不會產生，所以不會撞到假服務的紀錄。2026-11-07 曾有一個手寫的「不存在」ID 在產生的範圍裡：同一段前面的檔案剛好用掉 244 到 246 個 ID 時，它就成了真的紀錄，測試只在某些分段組成下失敗（Laysi/trustiot-v2#1176）。

## 瀏覽器端對端測試

2026-11-01 建立（Laysi/trustiot-v2#945，自 Laysi/trustiot-v2#675 拆出）：把 `docs/test-reports/2026-10-25-124-login-settings-ui.md` 的手動流程（登入、租戶、個人設定、登出，以及每次開關 Radix 彈出元件後的 CSP 檢查）寫成 Playwright 測試，對**真的身分服務**與**真的 `services/web`**（正式的 CSP 標頭）執行。單元測試用的是假身分服務（`src/test/fake-identity.ts`）；這裡沒有任何假的後端，只有入口與收信器的替代（見下）。這次只跑 Chromium，Firefox、WebKit 與時區清單在 Laysi/trustiot-v2#946。

### 怎麼跑

在 repo 根目錄：

```sh
proto install
bun install --cwd scripts --frozen-lockfile
bun install --cwd frontend --frozen-lockfile
bun scripts/browser-e2e/run.ts [--out <目錄>] [--grep <測試名稱的一部分>]
```

- **前置**：podman（rootless 即可，要能連到 `mcr.microsoft.com`、docker.io 與 gcr.io 拉映像）、Rust 工具鏈與 Bun（都由 `.prototools` 釘選）。`up` 的 podman 流程目前只在 x86_64 Linux 驗證過，arm64 上還沒試。第一次要編譯 `identity`、`web` 與 `migrate`、拉開發環境的映像與約 1 GB（壓縮後） 的 Playwright 映像，記憶體少的機器設 `CARGO_BUILD_JOBS=2`。
- **一次執行做的事**（每次都是新的、結束時全部移除，不碰別人的開發環境）：
  1. 以獨立的實例名稱與隨機埠執行 `bun scripts/dev.ts up`（Postgres 含 TLS、migration Job、身分服務的簽章金鑰）；不需要 `seed`，測試自己建使用者。
  2. 寫入兩個租戶（Acme Factory、Beta Plant），`bun run --cwd frontend build` 建置 SPA，`cargo build` 出 `identity` 與 `web`。
  3. 在主機上啟動 `identity`、`web`、收信器、入口替代與控制介面，等兩個服務的 `/readyz` 回 200。
  4. 把測試與釘選版本的 Playwright 套件複製到暫存目錄，唯讀掛進 Playwright 容器（`--network host`，丟掉所有 capability），在裡面執行 `playwright test`。
  5. 結束或被中斷時：以自己啟動的行程的 handle 停掉服務（不用 `pkill`）、移除容器、`dev.ts purge` 該實例、刪暫存目錄。
- **輸出**：`--out`（預設是系統暫存目錄下的新目錄，必須在 repo 之外）有 `browser.log`（Playwright 的清單）、`playwright/results.json`、失敗時的 trace（`playwright/artifacts/`，`npx playwright show-trace` 開啟）、`identity.log`、`web.log`、`ingress-requests.log`（每個 `/api` 與 `/auth` 請求的狀態碼）與 `up.log`、`build.log`、`cargo.log`。結束碼是 Playwright 的結束碼。
- **單一測試**：`--grep "all devices"`。
- 時間上限都是等條件的寬鬆上限，不是固定的短等待：Playwright 的 `expect` 逾時 30 秒、單一測試 180 秒、等收信 60 秒，逾時的訊息寫出在等什麼（符合 `CLAUDE.md` 的測試時間規則）。

### 測試資料與替代品

- **使用者與 session**：每個測試用 `uniqueEmail()` 建自己的 `@example.com` 使用者（身分服務限制同一個地址 60 秒內一封、每小時 5 封，所以不共用地址）。使用者、成員資格與 session 沒有 API 可以建（`services/identity/README.md` 「測試工具怎麼建立 session」），由 `scripts/browser-e2e/control.ts` 以 `identity_owner` 的身分寫入；容器裡的測試經 `e2e/support/control.ts` 的 HTTP 介面呼叫它。介面只綁 `127.0.0.1`，每次執行有一個隨機的 bearer token，只接受 `example.com` 的地址、兩個租戶與三種角色，SQL 的值都以 psql 變數傳入。
- **登入**：登入流程的測試走真的 email 驗證碼：身分服務寄到 `scripts/browser-e2e/smtp.ts` 的收信器（implicit TLS，憑證由 `scripts/lib/tls.ts` 產生的拋棄式 CA 簽，CA 以 `TRUSTIOT_SMTP_CA_FILE` 交給身分服務），測試從控制介面讀最新那封信的 8 位數驗證碼。其他測試（租戶、設定、登出）以控制介面建立的 session cookie 開始，省掉重複的登入，也不吃寄送限制。
- **入口的替代**：`scripts/browser-e2e/ingress.ts` 依 `services/identity/README.md` 的 ext_authz 說明代替叢集的入口：`/auth/login` 原樣轉給身分服務；`/api/identity` 先以方法、Host、Cookie、Origin、`Sec-Fetch-Site` 呼叫 `/internal/ext-authz<路徑>`，非 200 原樣回給瀏覽器，200 則帶著回來的 `x-internal-identity` 轉送（用戶端自己帶的丟掉）；其他 `/api`、`/auth`、`/internal` 回 404；其餘轉給 `services/web`，標頭（含 CSP）原樣保留。它不是 Istio：經真實入口的行為在 Laysi/trustiot-v2#947。
- **身分服務的路由表**：`dev.ts env` 印出的 `TRUSTIOT_INGRESS_ROUTES` 只有 assets 一條路由；執行器改給 `/api/identity` 對 `identity`、其餘對 `assets` 兩條，否則 ext_authz 簽出的 token 受眾不是身分服務，每個 API 請求都 401。
- 瀏覽器以 `http://localhost:<埠>` 存取：Chromium 把 `localhost` 視為安全來源，所以 `__Host-` 開頭、帶 `Secure` 的 session cookie 在 http 下也保留。

### 涵蓋的流程

| 檔案 | 測試的事 |
| --- | --- |
| `e2e/login.e2e.ts` | 沒有 session 時 `sessions/current` 的 401 當成未登入；錯誤驗證碼的 401 顯示在驗證碼欄位、換語言後重新翻譯；正確驗證碼 200 並以帳號的語言取代登入頁選的語言；`?next` 導回原本的頁面（`redirect_to` 與回應）；登入在資料庫留下一個有效的 session |
| `e2e/tenants.e2e.ts` | 沒有目前租戶：多個租戶要選（含角色）、只有一個時自動切換、不屬於任何租戶時說明並可重新檢查（中途加入成員後自動切換）；頁首租戶選單切換、焦點回到觸發按鈕 |
| `e2e/settings.e2e.ts` | 逐欄位 `PATCH`（只送改的那一欄）、主題立刻套用到 `<html>`；時區不合法的 422 顯示在該欄位、欄位退回、換語言後錯誤訊息重新翻譯；另一個工作階段改值後重新載入顯示伺服器的值、瀏覽器的副本跟著改 |
| `e2e/sign-out.e2e.ts` | 帳號選單登出這個瀏覽器（另一個 session 不受影響）；登出所有裝置先確認（Escape、取消不改變任何東西）、確認後另一個 session 變成 401、資料庫沒有有效的 session |
| `e2e/csp.e2e.ts` | `services/web` 送出的是強制的 CSP 與 Trusted Types 的 report-only 標頭；登入、首頁與設定頁載入後沒有違規、`<style>` 與 console 錯誤 |
| `e2e/checks.e2e.ts` | 檢查本身會失敗：注入的 `<style>` 被回報為違規與 `<style>` 元素、`console.error` 被回報 |
| `e2e/custom-page.e2e.ts` | 儀表板自訂頁面（Laysi/trustiot-v2#997）：真的 `CustomPageFrame`、guard 與 host adapter（`e2e/custom-page/harness.tsx`，執行前由 `e2e/custom-page/build.ts` 打包）放在帶 SPA 真實標頭的頁面，沙箱檔案來自真的 `services/web`。頁面一律是對抗的：試網路、儲存空間、cookie、外層文件與子 frame 的 realm；偽造、重送與大量送出 window 訊息；找 port；比對後經 DOM、CSS object model、封閉的 shadow root 或動畫改值；經 HTML 與 SVG `<style>` 的 `disabled`、SVG DOM 的 `baseVal`、建構的樣式表、寫在 `style` 屬性 `!important` 裡、沒有 script 的 CSS 動畫與 `:hover` transition 改值；頁面自己的 `!important` 打不開動作，有 `:hover` transition 與動畫的正當頁面經過十次指標也一直顯示，`style` 屬性裡的 transition 每次 `:hover` 都藏起、超過 `MAX_REGRESSIONS` 就移除（sandbox/README.md「沒有動作」）；以亂數間隔反覆閃現錯的值；每個 frame 改值並以自己送出或引起的事件掩護；一直把值改回去；卡住執行緒；重新載入或導向。frame 只在 port 上乾淨的回報時顯示，其他情況藏起或移除；對抗的頁面在每個 animation frame 記下自己畫出的值，測試量出錯的值在外層顯示時被畫出的時間，並斷言 sandbox/README.md「顯示時比對的強制」寫的上限（量到的值記在結果的 annotation）。也測點標記開來源面板、重新整理資料與資料裡的 HTML 只當文字。這個檔案不登入，只用 `services/web`；以 `--grep` 可以只跑它 |

**Radix 彈出元件的四項檢查**（`e2e/support/page.ts` 的 `checkPopup`，用在租戶選單、帳號選單與登出所有裝置的確認對話框，每個都以選擇、Escape 與取消各關閉過）：打開時 `body` 有 `data-scroll-locked` 而且 `overflow` 是 `hidden`（鎖定來自 `global.css` 的規則，CSP 不允許注入的 `<style>`）、關閉後解除；兩個時間點的 `securitypolicyviolation`（強制的與 Trusted Types 的 report-only）都是空的、文件裡的 `<style>` 元素是 0 個、console 沒有錯誤。console 的錯誤不含瀏覽器自己為被拒絕的請求記的 `Failed to load resource: … status of 4xx`（測試改檢查那個回應的狀態碼）；`pageerror` 一律算。每個測試結束時，沒有被明確檢查過的 console 錯誤也會讓它失敗。

### 版本釘選與升級

- **Playwright 套件**：`@playwright/test` 是 `package.json` 的精確版本（`bunfig.toml` 的 `exact`），`bun.lock` 記下它、`playwright` 與 `playwright-core` 的完整性雜湊；套件沒有安裝腳本，`trustedDependencies` 仍是空的。
- **瀏覽器**：不在本機下載，用 Playwright 發行的容器映像，`e2e/image.ts` 以 `name:tag@digest` 釘選（tag 是同一版的 `v<版本>-noble`，digest 是它的多架構 manifest，linux/amd64 與 linux/arm64 都在裡面），所以 Chromium 的建置也固定。永遠不用 `latest`。
- **兩者要同一版**：`e2e/pins.test.ts`（`bun run --cwd frontend test` 的一部分，所以 CI 的 `frontend` job 也跑）檢查套件、`bun.lock` 與映像的 tag 是同一版，映像有 digest。`renovate.json5` 把這三個套件與映像放在同一組（`playwright`），一個 PR 一起更新；Renovate 的 PR 要在合併前手動跑一次這份測試（它不在 CI）。
- **手動升級**：`bun add --cwd frontend -d @playwright/test@<版本>`，再把 `e2e/image.ts` 換成該版 `mcr.microsoft.com/playwright:v<版本>-noble` 的 digest（`podman manifest inspect` 或 registry 的 `Docker-Content-Digest` 標頭取得多架構 manifest 的 digest），跑 `pins.test.ts` 與整套測試。

### 在哪裡執行（選項與目前的決定）

目前只有指令，沒有 CI 工作；理由是 repo 之後會轉回私有，一個要起 Postgres、ClickHouse、Mosquitto、編譯 Rust 服務並拉約 1 GB（壓縮後） 映像的 job 沒有快取就是常駐的重型成本。選項：

| 選項 | 成本 | 說明 |
| --- | --- | --- |
| 指令，在開發機或建置主機執行（目前） | 沒有 CI 成本 | 動到登入、設定、租戶、session 或 Radix 元件的 PR，作者在合併前跑一次並把結果寫進 PR。缺點是靠人記得跑 |
| nightly 或手動觸發的工作流程，加標籤才在 PR 上跑 | 每次一次完整的 cargo 建置與映像拉取；要先有 Rust 與映像的快取才可以接受 | 沿用 `nightly.yml` 的做法（只在 main 變動時跑、PR 要有標籤）；需要先決定 Rust 快取與 Playwright 映像的快取，才不會變成常駐的重型 job |
| 每個 PR 的 `ci.yml` job | 最高 | 不建議：每次 PR 都付 cargo 與映像成本，與路徑過濾（`scripts/ci/changes.ts`）也要新增規則 |
| 自建 runner（Laysi/trustiot-v2#893）上的 job | 沒有 GitHub 分鐘數，快取留在機器上 | 建議的下一步：#893 完成後，以 `frontend/`、`services/identity/`、`services/web/` 的變更或手動觸發執行；現在不實作，因為它會新增一個常駐的 CI job，而 runner 還沒有 |

建議：先維持指令；#893 的 runner 就緒後再接成路徑過濾的 job。接進 CI 時要同步更新 `scripts/ci/changes.ts` 的規則（`scripts/browser-e2e/` 與它讀的檔案）與 `docs/architecture/ci-release.md`。

### 限制

- 只有 Chromium（Playwright 1.64.1 附的版本），沒有 Chrome 桌面版；跨瀏覽器在 Laysi/trustiot-v2#946。
- 入口是替代，不是 Istio；身分服務停掉時的錯誤畫面在 Laysi/trustiot-v2#947。
- 窄螢幕（360 px）的版面不在這次的範圍。
- 組織停權頁已由 Laysi/trustiot-v2#780 的實測涵蓋，這裡沒有重複。

## 警報與通知畫面

`/alerts`、`/alerts/notifications/:id` 與 `/alerts/rules/:id`（`src/pages/alerts/`，Laysi/trustiot-v2#172；設計稿與不同之處在 [`docs/design/ms4-ms5/design.md`](../docs/design/ms4-ms5/design.md) 的第二節）。後端（Laysi/trustiot-v2#168 回測與護欄、Laysi/trustiot-v2#169 通知）還沒有 API，所以**只在 UI 預覽**（`pages/alerts/preview.ts` 的 `previewRoutes` 與 `previewMenu`），不在 `routes.tsx` 與 `menu.ts`。

- **介面與假服務**：`src/api/pending/alerts.ts` 是介面（`AlertsApi`：總覽、通知列表與已讀、規則列表與詳情、停用與再啟用、暫停與恢復全部 AI 規則），`AlertsApiProvider.tsx` 提供給頁面；`src/test/fake-alerts.ts` 是記憶體裡的實作（`FakeAlertsServer`，種子資料是設計稿的虛構工廠），`preview` 以 provider 形式宣告。`failNext(端點)` 讓下一個呼叫失敗，`hold`／`release` 讓呼叫等著（載入中的畫面）。
- **誰能做什麼**：所有成員看通知、規則、版本與回測，也都能「問 AI」；停用與再啟用規則、暫停與恢復全部 AI 規則是編輯者以上（`useAlertsRole`），檢視者沒有這些按鈕，不是顯示成停用（設計決定 Q2）。權限仍由伺服器強制。
- **網址**：分頁 `?tab=rules`、通知的 `?show=all&device=&rule=`、規則的 `?origin=&status=&q=`、規則頁的 `?version=<n>`（展開該版與前一版的差異）與 `?action=disable`（Email 連結：只開啟停用確認，按下按鈕才停用）。改選時以 `replace` 更新；不認得的版本退回預設。
- **暫停中**：規則的狀態不因暫停而改變；畫面把「啟用中的 AI 建立規則」在總覽的 `pause.paused` 為真時顯示成「暫停中（全部 AI 規則）」（`state.ts` 的 `ruleState`），成員建立與已停用的規則維持原狀。
- **回測圖**：14 根長條是 SVG（位置與大小在屬性、顏色在 class，沒有 `style`），`role="img"` 加一句摘要，另有觸發表格；沒有載入 ECharts（設計決定 Q8）。
- **「問 AI」**：用共用的 `components/ask/AskAiButton`（見「情境 AI 按鈕與建議問題」），沒有自己的按鈕。問句範本在 `components/ask/questions.ts`（`suggestRules`、`firstUnusual`、`explainRule`、`compareRuleLastWeek`、`whyNotified`、`analyzeTrigger`，文字是 `ask.alert.*`），規則、設備與時間以 `context` 接在問句後面，按下開新對話並送出，`back` 讓新對話頂端回到警報頁或這條規則。
- **後端完成時**：依「UI 預覽」的「後端完成時要改的地方」，把 `AlertsApi` 換成產生的 client 上的模組，頁面搬進 `routes.tsx` 與 `menu.ts`；還要加選單的未讀數與圓點（`docs/design/app-shell.md`「未讀提示」）。
- **測試**：`AlertsPage.test.tsx` 與 `RuleDetailPage.test.tsx` 以 `src/test/alerts-render.tsx` 的 `renderAlerts`（和預覽一樣把 `previewRoutes` 掛在租戶頁面底下）測每個設計稿的狀態、角色、停用與暫停的確認、深連結；`diff.test.ts` 與 `time.test.ts` 測版本差異與時間格式。

## 文件庫畫面

2026-11-05 建立（Laysi/trustiot-v2#199）：文件庫列表、上傳、處理狀態、版本、重試、刪除與頁面檢視（引用連結的目的地）。後端 knowledge 服務（Laysi/trustiot-v2#193、Laysi/trustiot-v2#198）還沒有，所以這些頁面**只在 UI 預覽**（`src/pages/documents/preview.ts`），不在 `routes.tsx` 與 `menu.ts`；版面與狀態照 `docs/design/ms4-ms5/documents.html`，和設計稿不同的地方在 `docs/design/ms4-ms5/design.md` 的「文件庫畫面和設計稿不同的地方」。

- **頁面**：`/documents`（`DocumentsPage.tsx`：型號、狀態與名稱篩選都寫進網址，容量條，表格在 640 px 以下變成每列一張卡片，行動選單、重試、上傳新版本、下載原檔與刪除）；`/documents/:documentId?version=&page=&box=`（`DocumentViewerPage.tsx`：版本、頁碼、放大、頁碼清單，引用的裁切範圍由 `PageImage.tsx` 畫成疊在頁面圖上的 SVG，`viewBox` 是頁面圖的像素，所以 `box=` 直接可用，不需要 `style` 屬性）。不認得的參數值退回預設（最新版本、第 1 頁、沒有框）；已刪除的文件只顯示「已刪除」，不載入任何頁面。
- **上傳**：`UploadDialog.tsx` 用 Radix 的 alert dialog 原件（沒有一般的 Dialog 元件；`role="dialog"`，「在背景繼續」就是它的 Cancel）。檔案在頁面層級的佇列（`useUploadQueue.ts`），關掉對話框上傳照常進行，列表上方顯示「在背景上傳」；離開文件庫頁面（或切換租戶，`TenantGate` 會重新掛載）會取消上傳。非 PDF 與超過剩餘容量的檔案在該列直接拒絕；同型號同名的檔案問「上傳為新版本」（預設）或「另存為新文件」。
- **待接的 API**：介面與型別在 `src/api/pending/documents.ts`（第一行寫著 Laysi/trustiot-v2#193），`DocumentApiProvider.tsx` 是它旁邊的 provider。`upload` 把預簽網址直傳（決定 Q14）藏在裡面，進度與取消經 `onProgress` 與 `AbortSignal`。方案沒有文件 RAG 時，每個呼叫都回 403 `quota_exceeded` 與訊息鍵 `entitlement.feature_not_in_plan`（`docs/product/plans.md`），頁面以它顯示升級牆；超過容量的上傳回 `entitlement.limit_reached`。型號名稱讀真正的設備 API（`listDeviceModels`）。
- **假服務**：`src/test/fake-documents.ts` 的 `FakeDocumentServer` 實作這個介面（沒有 HTTP 層可答，因為端點還不存在）：上傳停在 `uploads` 裡，由測試呼叫 `progress`、`finish`、`fail`；處理狀態只經 `advance()` 前進，`failAtPage` 讓某份文件在那一頁失敗；`failNext(方法, 錯誤)` 讓下一個呼叫失敗；刪除留下墓碑（決定 Q13）。它也是 UI 預覽的項目（`preview`，provider 形式）：預覽裡上傳會跑出進度、處理每 1.5 秒前進一步，頁面圖是即時畫出的 SVG，沒有任何東西離開瀏覽器。`src/test/documents-render.tsx` 把預覽的路由加進正式的路由（`withRoutes`）來測試。
- **後端完成時**：依「UI 預覽」的步驟：以產生的 client 寫 `src/api/documents.ts` 與 zod schema，刪掉 `src/api/pending/` 的檔案，假服務改成 HTTP 層，路由與選單項目搬進 `routes.tsx`、`menu.ts`（選單第 3 組）。頁面圖的 `image_url` 要是同源的路徑（CSP 的 `img-src` 只有 `'self'`）。

## TypeScript 停在 6

repo 其他地方（`scripts/`、`deploy/host`）用 TypeScript 7，`frontend/` 用 6.0.3。原因：`openapi-typescript` 7.13.0 以 TypeScript 的 JS API（`ts.factory`）產生型別，TypeScript 7.0.2 的套件沒有這套 API，`api:generate` 失敗（`TypeError: undefined is not an object (evaluating 'ts.factory.createKeywordTypeNode')`）。`renovate.json5` 讓 `frontend/` 的 TypeScript 停在 6.x；產生器支援 TypeScript 7 之後再刻意升級，升級時以 `api:check` 與 `typecheck` 確認。
