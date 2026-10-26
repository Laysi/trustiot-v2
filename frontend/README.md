# frontend

Vite + React 的單頁應用程式（SPA，決策 2），頁面全在登入後，與 API 走同一個網域：入口把 `/api`、`/auth` 送到服務，其餘路徑由 `services/web` 提供這裡建置出來的檔案。以 Bun 安裝相依套件、執行腳本與測試，Bun 版本鎖在根目錄的 `.prototools`。不是 Cargo workspace 成員。2026-10-21 建立骨架（Laysi/trustiot-v2#123）：路由與深層連結、zh-TW 與 en、API client、錯誤頁、偏好設定、租戶切換、SSE 的連線處理。2026-10-25 加上第一批真正的畫面與 UI 基礎（Laysi/trustiot-v2#124）：email 驗證碼登入、登出與登出所有裝置、從身分服務讀取的工作階段與租戶切換、沒有目前租戶時的選擇、存在帳號裡的個人設定、組織停權頁。

## 技術組合

| 用途 | 套件 |
| --- | --- |
| 建置與開發伺服器 | Vite 8（Rolldown）、`@vitejs/plugin-react` |
| UI | React 19；Radix UI 的 primitives 加上複製進 repo 的 shadcn 風格元件（`src/components/ui/`），圖示用 `lucide-react`；見「UI 元件與樣式」 |
| 樣式 | Tailwind CSS 4（`@tailwindcss/vite`，建置時編譯成靜態 CSS）與 `tw-animate-css`；`class-variance-authority`、`clsx`、`tailwind-merge` 組合 class |
| 路由 | React Router 8（data mode：`createBrowserRouter`，測試用 `createMemoryRouter`） |
| 伺服器狀態 | TanStack Query 5 |
| API client | `openapi-fetch`，型別由 `openapi-typescript` 從 `api/openapi.json` 產生 |
| 外部資料的執行時檢查 | zod 4，一律經 `src/lib/zod.ts`（`jitless`，見「CSP 的限制」與「外部資料一律經 zod 解析」） |
| 型別檢查 | TypeScript 6（原因見下方「TypeScript 停在 6」） |
| lint 與格式 | Biome |
| 測試 | `bun test`，DOM 由 happy-dom 提供，元件以 Testing Library 操作 |

相依一律是精確版本（`bunfig.toml` 的 `exact = true`），`bun.lock` 進版控，`trustedDependencies` 是空的：任何相依的安裝腳本都不執行。新增相依只用 `bun add --cwd frontend <套件>@<版本>`，不用 npm、pnpm。

## 指令

都在 repo 根目錄執行：

| 指令 | 做什麼 |
| --- | --- |
| `bun install --cwd frontend --frozen-lockfile` | 依 `bun.lock` 安裝 |
| `bun run --cwd frontend dev` | 開發伺服器（熱更新）；後端見下方「開發伺服器」 |
| `bun run --cwd frontend api:generate` | 由 `api/openapi.json` 重新產生 `src/api/schema.gen.ts` |
| `bun run --cwd frontend api:check` | 進版控的 `schema.gen.ts` 與規格產生的結果不同就失敗 |
| `bun run --cwd frontend typecheck` | `tsc --noEmit`，含 `src/`、`tools/` 與 `vite.config.ts` |
| `bun run --cwd frontend lint` | `biome ci .`；`bun run --cwd frontend format` 修正格式與 import 順序 |
| `bun run --cwd frontend test` | 全部單元測試；單一測試：`bun run --cwd frontend test -t "<測試名稱的一部分>"` |
| `bun run --cwd frontend build` | 建置到 `frontend/dist/`（不進版控） |
| `bun run --cwd frontend check:dist` | 檢查 `dist/` 符合下方的版面與 CSP 規則 |

CI 的 `frontend` job 依序跑 install、`api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist`（`docs/architecture/ci-release.md` 的「ci.yml 的 job」）。

產生 API client 與檢查建置結果的 Bun 腳本放在 `tools/`，不叫 `scripts/`：在 repo 根目錄執行的 `bun test scripts` 以路徑子字串篩選測試檔，目錄叫 `scripts/` 會連前端的測試一起跑，而那時前端的相依不一定已安裝。

## UI 元件與樣式

決策 2（2026-10-25，Laysi/trustiot-v2#124）：shadcn 風格的元件加 Radix 的 primitives 與 Tailwind CSS 4，取代骨架時「只用一般 CSS 或 CSS Modules」的做法。

- **Tailwind 在建置時編譯成靜態 CSS**：`vite.config.ts` 的 `@tailwindcss/vite` 掃描原始碼用到的 class，和 `src/styles/global.css` 一起輸出成 `static/<hash>.css`。樣式只有這一個 stylesheet；不用 CSS-in-JS、不用 `style` 屬性，執行時不產生也不注入任何樣式（見「CSP 的限制」）。動畫用 `tw-animate-css`（純 CSS），`prefers-reduced-motion` 時全部縮成一瞬間。
- **主題是 CSS 變數**：顏色以用途命名（`--background`、`--primary`、`--destructive`……，shadcn 的名稱，複製來的元件直接可用），定義在 `:root` 與 `:root[data-theme="dark"]`，經 `@theme inline` 變成 Tailwind 的顏色；`dark:` 變體跟著 `<html data-theme>`，不跟作業系統。主題由 `PreferencesProvider` 設定，`main.tsx` 在第一次繪製前先套用瀏覽器裡的副本，所以不會閃一下淺色。每組文字與背景、輸入框框線與焦點框都符合 WCAG 2.2 AA（文字 4.5:1，框線 3:1）；改顏色時要重新確認。
- **元件在 `src/components/ui/`**：從 shadcn 複製進來再依需要修改（原始碼是我們的，不是套件），只放畫面用到的；新畫面需要新元件時才加。Radix 的 primitives 是相依套件（`@radix-ui/react-*`，各自鎖定版本）。`src/lib/utils.ts` 的 `cn()` 合併 class，變體用 `class-variance-authority`。和 shadcn 原版不同的地方：焦點以 `outline` 顯示（`button.tsx` 的 `focusRing`，強制色彩模式下仍看得到）、`Button` 預設 `type="button"`、`Label` 一定要有 `htmlFor`、表單欄位以 `FormField` 串起標籤、說明與錯誤（`aria-describedby`、`aria-invalid`）。
- **游標**：可以點的元素（按鈕、下拉選單、選單項目、`role="button"` 等）顯示手形，停用時維持箭頭（2026-10-25 專案擁有者決定）。停用指的是 `disabled` 屬性、`aria-disabled="true"` 或 Radix 的 `data-disabled`，自己做的可點元素停用時要設其中一個。Tailwind 4 沿用瀏覽器的預設，只有連結顯示手形，所以這條規則寫在 `global.css` 的 base layer，新元件自動套用；元件不另外寫 `cursor-*`：從 shadcn 複製來的元件若自己設了游標（例如選單項目的箭頭、輸入框停用時的禁止符號），要拿掉那個 class，否則 utility 會蓋過這條規則，停用時的游標也會和其他控制項不一致。Tailwind 也會掃描這份 README，所以這裡不寫出完整的 class 名稱，免得建置出沒用到的 utility。
- **不用 Radix 的 Select**：它的 `SelectViewport` 以 `dangerouslySetInnerHTML` 寫入 inline `<style>` 隱藏捲軸，CSP 擋下，Trusted Types 也會回報。下拉選單一律用 `NativeSelect`（樣式化的原生 `<select>`），鍵盤、螢幕報讀與手機的選擇器都由瀏覽器處理。
- **Radix 的 modal 元件鎖捲動不注入 `<style>`**：`DropdownMenu`、`AlertDialog` 經 `react-remove-scroll` 鎖住背景捲動，它原本以 `react-style-singleton` 注入 `<style>`。`vite.config.ts` 把 `react-style-singleton` alias 到 `src/lib/no-style-singleton.ts`（同樣的 export、不碰 DOM），鎖捲動的規則改寫在 `global.css` 的 `body[data-scroll-locked]`（屬性仍由 `react-remove-scroll-bar` 設定），`html` 的 `scrollbar-gutter: stable` 讓鎖住時版面不跳動。`src/test/setup.ts` 以 `mock.module` 做同樣的替換，所以測試跑的與建置相同；`no-style-singleton.test.tsx` 確認打開選單與對話框時文件裡沒有 `<style>`。
- Radix 以 CSSOM（`element.style`）設定彈出位置與 focus guard，CSP 的 `style-src` 不管這種寫法，不必處理。
- **AI 對話畫面**（Laysi/trustiot-v2#149）用 assistant-ui 的官方元件，它們建在同一套 shadcn + Radix + Tailwind 上；由 Laysi/trustiot-v2#149 安裝，也由它處理下面「CSP 的限制」提到的 micromark `decode-named-character-reference` alias。這裡還沒有安裝 assistant-ui。

## 建置結果的約定（與 `services/web`）

`services/web` 原樣提供 `dist/`，所以版面是兩邊的約定，`tools/check-dist.ts` 檢查：

- 根目錄只有 `index.html` 與 `public/` 的檔案（`robots.txt`、`favicon.svg`）。
- 其餘都在 `static/` 下，檔名只有內容的 hash（`static/<hash>.js`、`static/<hash>.css`），原始碼的模組名稱與路徑不會出現在映像裡；內容改變時檔名一定改變，所以這些檔案可以長期快取。
- 沒有 source map、沒有 `index.html` 以外的 HTML，副檔名只限 html、js、css、svg、png、ico、woff2、txt、webmanifest。
- 資源以絕對路徑引用（`base: "/"`），所以 `/devices/42` 這種深層連結載入的資源與 `/` 相同。

保留給其他用途的路徑，前端路由不能使用（`src/app/paths.ts`，`routes.test.tsx` 檢查每個路由）：

- 前綴：`/api`、`/auth`（入口送到服務）、`/internal`（入口永遠不轉送）、`/static`（建置的資源）。
- 路徑：`/healthz`、`/readyz`、`/index.html`、`/robots.txt`、`/favicon.svg`。

其他路徑都由 `services/web` 回 `index.html`，再由前端路由決定畫面；不存在的路徑顯示前端的「找不到頁面」。

## CSP 的限制

頁面的 CSP 只允許同源的 script 與 style，所以：

- 不能有 inline script 或 inline style：不用 `style` 屬性、`<style>`、`on*=` 屬性與 `dangerouslySetInnerHTML`（Biome 擋下後者）。樣式全部寫在 CSS 檔，主題以 `<html data-theme>` 切換 CSS 變數。
- 不能有 `data:` URI：`assetsInlineLimit: 0`，小檔案也輸出成 `static/` 下的檔案。
- 不用 Vite 的 modulepreload polyfill 與 React Router 的 `<ScrollRestoration>`，兩者都會產生 inline script。
- 不用 `eval`、`new Function`。zod 4 建立 `z.object` schema 時會以 `new Function("")` 探測能不能編譯解析器，沒有 `'unsafe-eval'` 的 CSP 會擋下它（zod 吞掉例外），但瀏覽器仍發出 `securitypolicyviolation`，`check:dist` 又不看 JavaScript。所以 zod 一律以 `jitless` 執行：`src/lib/zod.ts` 先 `z.config({ jitless: true })` 再匯出 `z`，其他模組只從它匯入（ES module 的求值順序保證設定早於任何 schema 的建立）；Biome 的 `noRestrictedImports` 禁止在它以外匯入 `zod`，也禁止 `zod/compile`（它的 AOT 編譯同樣用 `new Function`）。
- 不把字串寫進會解析成 HTML 或程式的 DOM 介面（`innerHTML`、`outerHTML`、`insertAdjacentHTML`、`document.write`、`srcdoc`）。頁面已經送出 Trusted Types（`require-trusted-types-for 'script'`），目前只回報，之後會改成強制，強制後這類寫入會直接丟出例外。套件這樣做時，改用它不碰 DOM 的版本（`vite.config.ts` 的 `resolve.alias`）或換掉它。

`check:dist` 只檢查建置輸出的 HTML、SVG 與 CSS，不看 JavaScript。套件在執行時才注入的 `<style>` 或寫進 `innerHTML` 的字串，只會在瀏覽器裡出現：被 CSP 擋下或被 Trusted Types 回報時，主控台會出現 violation。新增 UI 或 Markdown 相關的套件時，要在瀏覽器實際操作用到它的畫面，確認主控台沒有 violation。

2026-10-21 以 assistant-ui 實測時碰到的情況：

- Radix 的 modal 元件（`Dialog`、`DropdownMenu` 等）經 `react-remove-scroll` 注入 `<style>` 來鎖住背景捲動。CSP 會擋下，元件本身照常運作，只是背景沒有鎖住。2026-10-25 起以 alias 與靜態 CSS 處理（見「UI 元件與樣式」），沒有放寬 `style-src`。
- micromark（`react-markdown` 用的解析器）依賴的 `decode-named-character-reference`，瀏覽器版以 `innerHTML` 解碼 HTML 實體。在 `vite.config.ts` 把它 alias 到不碰 DOM 的 `index.js`。
- Radix 的 tooltip 與 popper 以 CSSOM 設定定位樣式，CSP 允許，不必處理。

部署新版後，舊頁面要載入的 chunk 可能已不存在：`vite:preloadError` 時重新整理一次頁面，60 秒內不重複（`src/app/preload-error.ts`），避免無限重新整理。

## 路由與錯誤頁

路由定義在 `src/app/routes.tsx`：`/login` 不套版面，其餘套 `Layout`（頁首、主選單、租戶切換、帳號選單）。沒有工作階段的訪客由 `Layout` 導向 `/login?next=<目前路徑>`。`/settings` 與「找不到頁面」不需要目前租戶；處理租戶資料的頁面都放在 `TenantGate` 底下，它先讓工作階段進入一個租戶（見「偏好設定、租戶與工作階段」），租戶停權時以停權頁取代這些頁面。每一層都有 `errorElement`，所以任何畫面出錯都留在版面內顯示錯誤頁，不會整頁空白。錯誤頁依錯誤分類（`src/api/errors.ts` 的 `classifyError`）：

| 情況 | 畫面 |
| --- | --- |
| 401 或 `unauthenticated` | 「工作階段已結束」，連到 `/login?next=<目前路徑>` |
| 403 或 `forbidden` | 沒有權限 |
| `tenant_suspended` | 組織已停權 |
| 404 或 `not_found` | 找不到頁面 |
| 503、504、網路錯誤、`unavailable`、`timeout` | 服務暫時無法使用，附重新整理按鈕 |
| 其他 | 一般錯誤；只有 API 的錯誤會顯示翻譯後的訊息，程式錯誤的內容不顯示給使用者 |

查詢或寫入收到 401 時直接導向登入頁（`src/app/query-client.ts`），`SessionProvider` 同時忘掉這個使用者快取的資料，把工作階段標成已登出：工作階段在別的裝置被登出或過期時，畫面不會留著前一個工作階段的資料。自己處理 401 的請求（登入表單：驗證碼錯誤就是 401；登出所有裝置，見「登入與登出」）在 TanStack Query 的 `meta` 標上 `handlesUnauthenticated`，不觸發這兩件事。登入後要回到的 `next` 來自網址列，只接受本 SPA 的路徑（同源、不在保留路徑、不是登入頁；`//`、`/\`、控制字元與其他網域一律回首頁），避免成為開放重新導向（`safeNextPath`）。原始字串與解析後的路徑各檢查一次：解析會處理 `.`、`..`、`%2e` 這些點段，也會把 `\` 換成 `/`，所以 `/.//other.example`、`/x/../\other.example` 解析後是 `//other.example`，同樣回首頁。登入頁的比對與路由一樣不分大小寫、不管結尾的 `/`（`/LOGIN`、`/login/` 也算登入頁）。伺服器對 `redirect_to` 的檢查另見 Laysi/trustiot-v2#677。

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
- `unwrap()` 把錯誤轉成 `ApiError`：狀態碼、解析後的 M1 錯誤內容，網路錯誤的狀態碼是 0；`retryable` 只對 `rate_limited`、`unavailable`、`timeout`（沒有內容時 429、503、504）與網路錯誤成立。查詢最多重試 3 次，寫入不重試。

### 外部資料一律經 zod 解析

API 回應、錯誤本文、SSE 事件資料與瀏覽器儲存（`localStorage`）的內容都是外部資料：型別只在編譯期成立，執行時什麼都可能進來（滾動更新時前後端版本不一致、舊頁面還開著、入口回了不是 M1 格式的內容）。所以它們一律以 zod schema 解析後才使用，不用 `as` 轉型（`JSON.parse(...) as X` 也不行）。

- **schema 與 `schema.gen.ts` 雙向綁定**：手寫的 schema 旁邊用 `src/lib/type-binding.ts` 的 `Assert<Equal<z.output<typeof S>, components["schemas"]["S"]>>` 綁定產生的型別，規格一變 `typecheck` 就失敗。只用 `satisfies z.ZodType<T>` 抓不到規格刪掉的欄位。`src/api/errors.ts` 是範例；`ErrorBody` 的 `details` 刻意比規格窄（只留字串與有限數字），所以該欄位只檢查可指派，其餘欄位仍用 `Equal`。`tools/api-client.ts` 傳 `emptyObjectsUnknown: true`：沒有 properties 的 object（`details`）產生成 `Record<string, unknown>`，不是「不能有任何鍵」的 `Record<string, never>`，再由 schema 收窄。
- **邊界的寬鬆行為**：錯誤本文與儲存的偏好都不用 `z.strictObject`，未知欄位忽略（M1 規定新欄位只以選填的方式加入）；不合格的 `details` 項目與 `field_errors` 項目丟掉，不讓整個本文失敗；儲存的偏好每個欄位以 `.catch()` 退回自己的預設值，版本欄位 `v` 不是 2 時回傳預設值。身分服務的回應（工作階段、偏好、登入）則嚴格解析：例如不認得的角色是契約的改變，以 `ResponseShapeError` 呈現，不默默當成別的角色。
- **SSE**：`sse.ts` 目前只交出 `data` 字串；串流畫面（Laysi/trustiot-v2#149 決定是否在 `sse.ts` 提供共用介面）自己以 schema 解析，解析失敗的事件丟掉並以 `console.warn` 記下事件類型與 issue 路徑（不記值），串流照常繼續。

## SSE

即時圖表與 AI 回答的串流用 `src/api/sse.ts` 的 `openEventStream`（以 fetch 讀 `text/event-stream`，不用 `EventSource`，因為它看不到註解行與狀態碼）。與伺服器的約定：

- Cloudflare 在 125 秒沒有資料時切斷回應，狀態碼仍是 200，所以伺服器至少每 30 秒送一次 heartbeat（註解行即可）。
- 前端 75 秒沒收到任何資料就當作斷線，與串流結束一樣處理：帶 `Last-Event-ID` 重新連線，讓伺服器接續。
- 重新連線的等待從 1 秒倍增到 30 秒，加上隨機的抖動；伺服器的 `retry:` 是下限。連線成功並收到資料後重新從 1 秒算。
- 401、403 停止重連並回報狀態；其他狀態碼或不是 `text/event-stream` 的回應照樣重試。

計時器經由可注入的 `Clock`，測試以手動時鐘推進，不等待真實時間。

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
- **停權**：任何請求收到 `tenant_suspended`（403）時，`SessionProvider` 把目前的租戶記為停權，`TenantGate` 以停權頁取代它的頁面，列出使用者其他的租戶可以切換，或登出；切換租戶本身被拒絕時是那次切換的錯誤，不算停權。取代舊系統每 60 秒檢查一次的做法。
- **偏好設定**（介面語言、AI 回答的語言、時區、主題）存在帳號裡，`GET`／`PATCH /api/identity/v1/preferences`，以伺服器的值為準，每個登入的瀏覽器都一樣。前端的欄位與 API 的值一對一（`auto`、`zh-TW`、`en`；`auto` 或 IANA 名稱；`system`、`light`、`dark`），只在套用時解析 `auto` 與 `system`（`src/preferences/preferences.ts`），所以顯示的與儲存的不會不一致。
  - 個人設定頁每改一個欄位就送出只含那個欄位的 `PATCH`，立刻套用；伺服器的回應就是之後的值。一次只送一個，依修改的順序；還在儲存的修改顯示在較早的回應之上。伺服器拒絕時退回儲存的值，422 的欄位錯誤顯示在對應的欄位，其他錯誤也顯示在那個欄位；錯誤保留的是錯誤本身，換語言時重新翻譯。讀不到伺服器的偏好時不能修改，可以重試。
  - 瀏覽器的 `localStorage`（`trustiot.preferences`，格式版本 2）只是暫時的副本：登入前與伺服器回應前的畫面用它，伺服器每次回應都寫回去，登出後的登入頁也從它開始。讀寫失敗（隱私模式、配額）時用預設值而不出錯。預設值與伺服器相同（語言、AI 語言、時區都是 `auto`，主題 `system`）；`auto` 語言取瀏覽器語言清單中第一個 zh 或 en，都沒有時用 en。

## 開發伺服器

`bun run --cwd frontend dev` 只提供前端。要接後端時，先依 `scripts/README.md` 在本機啟動服務，再以 `TRUSTIOT_DEV_API_ORIGIN=<服務的 origin> bun run --cwd frontend dev` 啟動：`/api` 與 `/auth` 轉送到該 origin，保留原本的 Host，與正式環境一樣同源。

## 測試的寫法

- `src/test/setup.ts` 註冊 happy-dom，網址是 `https://app.example.test/`，並把 `react-style-singleton` 換成與建置相同的替代（見「UI 元件與樣式」）。
- 畫面測試用 `src/test/render.tsx` 的 `renderAt(路徑, …)`：app 的 query client（401 導向登入頁，不重試）、記憶體版本的偏好副本，以及 `src/test/fake-identity.ts` 的假身分服務。假服務有使用者、租戶、伺服器端的工作階段、偏好與驗證碼信箱，回應照身分服務的 README（401、403 `session.tenant_not_member`、422 的欄位錯誤）；`browser()` 各有自己的 cookie，可以測兩個瀏覽器（登出所有裝置、在另一個瀏覽器讀到同樣的偏好）。`failNext` 讓下一個請求失敗，`holdNext` 讓它等到測試放行，`holdNextAnswer` 讓它立刻生效、但回應等到測試放行（伺服器已改變狀態、回應還在路上）。沒有指定時，`renderAt` 用一個已登入、屬於一個租戶的使用者。
- 不依賴固定的短時間等待：SSE 用手動時鐘，畫面用 Testing Library 的 `findBy*` 與 `waitFor`。
- fixture 裡的網址一律用 `*.example.test`，email 用 `@example.com`（例如 `alice@example.com`）。

## TypeScript 停在 6

repo 其他地方（`scripts/`、`deploy/host`）用 TypeScript 7，`frontend/` 用 6.0.3。原因：`openapi-typescript` 7.13.0 以 TypeScript 的 JS API（`ts.factory`）產生型別，TypeScript 7.0.2 的套件沒有這套 API，`api:generate` 失敗（`TypeError: undefined is not an object (evaluating 'ts.factory.createKeywordTypeNode')`）。`renovate.json5` 讓 `frontend/` 的 TypeScript 停在 6.x；產生器支援 TypeScript 7 之後再刻意升級，升級時以 `api:check` 與 `typecheck` 確認。
