# frontend

Vite + React 的單頁應用程式（SPA，決策 2），頁面全在登入後，與 API 走同一個網域：入口把 `/api`、`/auth` 送到服務，其餘路徑由 `services/web` 提供這裡建置出來的檔案。以 Bun 安裝相依套件、執行腳本與測試，Bun 版本鎖在根目錄的 `.prototools`。不是 Cargo workspace 成員。2026-10-21 建立骨架（Laysi/trustiot-v2#123）：路由與深層連結、zh-TW 與 en、API client、錯誤頁、偏好設定、租戶切換、SSE 的連線處理；登入頁是 Laysi/trustiot-v2#124 的範圍，這裡只有預留的頁面。

## 技術組合

| 用途 | 套件 |
| --- | --- |
| 建置與開發伺服器 | Vite 8（Rolldown）、`@vitejs/plugin-react` |
| UI | React 19 |
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

- Radix 的 modal 元件（`Dialog`、`DropdownMenu` 等）經 `react-remove-scroll` 注入 `<style>` 來鎖住背景捲動。CSP 會擋下，元件本身照常運作，只是背景沒有鎖住。要鎖捲動，就把規則寫進靜態 CSS（例如針對 `body[data-scroll-locked]`），不要放寬 `style-src`。
- micromark（`react-markdown` 用的解析器）依賴的 `decode-named-character-reference`，瀏覽器版以 `innerHTML` 解碼 HTML 實體。在 `vite.config.ts` 把它 alias 到不碰 DOM 的 `index.js`。
- Radix 的 tooltip 與 popper 以 CSSOM 設定定位樣式，CSP 允許，不必處理。

部署新版後，舊頁面要載入的 chunk 可能已不存在：`vite:preloadError` 時重新整理一次頁面，60 秒內不重複（`src/app/preload-error.ts`），避免無限重新整理。

## 路由與錯誤頁

路由定義在 `src/app/routes.tsx`：`/login` 不套版面，其餘套 `Layout`（頁首、主選單、租戶切換、偏好設定）。每一層都有 `errorElement`，所以任何畫面出錯都留在版面內顯示錯誤頁，不會整頁空白。錯誤頁依錯誤分類（`src/api/errors.ts` 的 `classifyError`）：

| 情況 | 畫面 |
| --- | --- |
| 401 或 `unauthenticated` | 「工作階段已結束」，連到 `/login?next=<目前路徑>` |
| 403 或 `forbidden` | 沒有權限 |
| `tenant_suspended` | 組織已停用 |
| 404 或 `not_found` | 找不到頁面 |
| 503、504、網路錯誤、`unavailable`、`timeout` | 服務暫時無法使用，附重新整理按鈕 |
| 其他 | 一般錯誤；只有 API 的錯誤會顯示翻譯後的訊息，程式錯誤的內容不顯示給使用者 |

查詢或寫入收到 401 時直接導向登入頁（`src/app/query-client.ts`）。登入後要回到的 `next` 來自網址列，只接受本 SPA 的路徑（同源、不在保留路徑、不是 `/login`；`//`、`/\`、控制字元與其他網域一律回首頁），避免成為開放重新導向（`safeNextPath`）。

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
- **邊界的寬鬆行為**：錯誤本文與儲存的偏好都不用 `z.strictObject`，未知欄位忽略（M1 規定新欄位只以選填的方式加入）；不合格的 `details` 項目與 `field_errors` 項目丟掉，不讓整個本文失敗；偏好的每個欄位以 `.catch()` 退回自己的預設值，版本欄位 `v` 不是 1 時回傳預設值。
- **SSE**：`sse.ts` 目前只交出 `data` 字串；串流畫面（Laysi/trustiot-v2#149 決定是否在 `sse.ts` 提供共用介面）自己以 schema 解析，解析失敗的事件丟掉並以 `console.warn` 記下事件類型與 issue 路徑（不記值），串流照常繼續。

## SSE

即時圖表與 AI 回答的串流用 `src/api/sse.ts` 的 `openEventStream`（以 fetch 讀 `text/event-stream`，不用 `EventSource`，因為它看不到註解行與狀態碼）。與伺服器的約定：

- Cloudflare 在 125 秒沒有資料時切斷回應，狀態碼仍是 200，所以伺服器至少每 30 秒送一次 heartbeat（註解行即可）。
- 前端 75 秒沒收到任何資料就當作斷線，與串流結束一樣處理：帶 `Last-Event-ID` 重新連線，讓伺服器接續。
- 重新連線的等待從 1 秒倍增到 30 秒，加上隨機的抖動；伺服器的 `retry:` 是下限。連線成功並收到資料後重新從 1 秒算。
- 401、403 停止重連並回報狀態；其他狀態碼或不是 `text/event-stream` 的回應照樣重試。

計時器經由可注入的 `Clock`，測試以手動時鐘推進，不等待真實時間。

## 偏好設定、租戶與工作階段

- 偏好設定（語言、時區、主題）存在 `localStorage` 的 `trustiot.preferences`，讀寫失敗（隱私模式、配額）時用預設值而不出錯。預設語言取瀏覽器語言清單中第一個 zh 或 en，都沒有時用 en；時區預設跟隨瀏覽器；主題預設跟隨系統。改了立刻套用，不需重新整理。
- 工作階段（使用者、目前租戶、可切換的租戶）由 `SessionSource` 提供（`src/session/session.ts`）。身分服務的端點在 Laysi/trustiot-v2#497 加入：`GET /api/identity/v1/sessions/current`（使用者、`active_tenant`、`tenants`，見[身分服務](../services/identity/README.md#目前的-session-與切換租戶)）與 `PUT /api/identity/v1/sessions/current/tenant`（`{"tenant_id"}`，回應同前者）；偏好的端點是 `GET`／`PATCH /api/identity/v1/preferences`。`main.tsx` 目前仍接 `noSessionSource`（沒有工作階段，不顯示租戶切換），把它換成呼叫這些端點的實作、偏好改存到伺服器是 Laysi/trustiot-v2#124 的範圍，元件與測試不用改。
- 切換租戶時先取消進行中的請求，再把其他查詢全部重設：還開著的頁面會為新租戶重新查詢，不會短暫顯示前一個租戶的資料。不用 `queryClient.clear()`：它移除查詢後，還在畫面上的元件仍拿著被移除的查詢，畫面停在舊租戶（測試抓到的問題）。分頁標題是「<租戶名稱> · TrustIoT」。

## 開發伺服器

`bun run --cwd frontend dev` 只提供前端。要接後端時，先依 `scripts/README.md` 在本機啟動服務，再以 `TRUSTIOT_DEV_API_ORIGIN=<服務的 origin> bun run --cwd frontend dev` 啟動：`/api` 與 `/auth` 轉送到該 origin，保留原本的 Host，與正式環境一樣同源。

## 測試的寫法

- `src/test/setup.ts` 註冊 happy-dom，網址是 `https://app.example.test/`；畫面測試用 `src/test/render.tsx` 的 `renderAt(路徑, …)`，偏好設定與工作階段以記憶體版本注入。
- 不依賴固定的短時間等待：SSE 用手動時鐘，畫面用 Testing Library 的 `findBy*` 與 `waitFor`。
- fixture 裡的網域一律用 `*.example.test`。

## TypeScript 停在 6

repo 其他地方（`scripts/`、`deploy/host`）用 TypeScript 7，`frontend/` 用 6.0.3。原因：`openapi-typescript` 7.13.0 以 TypeScript 的 JS API（`ts.factory`）產生型別，TypeScript 7.0.2 的套件沒有這套 API，`api:generate` 失敗（`TypeError: undefined is not an object (evaluating 'ts.factory.createKeywordTypeNode')`）。`renovate.json5` 讓 `frontend/` 的 TypeScript 停在 6.x；產生器支援 TypeScript 7 之後再刻意升級，升級時以 `api:check` 與 `typecheck` 確認。
