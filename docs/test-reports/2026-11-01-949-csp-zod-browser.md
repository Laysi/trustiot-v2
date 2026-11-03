# SPA 在真瀏覽器與正式 CSP 下的實測：深連結、語言切換、錯誤頁與 zod 路徑（2026-11-01）

Laysi/trustiot-v2#949（來自 Laysi/trustiot-v2#498 的第 7、8 項；來源 Laysi/trustiot-v2#123 與 Laysi/trustiot-v2#568）。這份報告是以真的 Chromium，經 `services/web`（正式的 CSP 標頭）載入建置後的 SPA，檢查主控台沒有 CSP 與 Trusted Types 的 violation，並讓請求走過 zod 的每一條路徑。

## 環境

- **程式碼**：`origin/main` 的 `d2e47c2f`，產品程式碼沒有變更；這次只新增報告與 `data/` 下的實測程式。
- **機器**：建置主機（x86_64 Linux），rootless podman。開發環境（Postgres 含 TLS、migration Job、身分服務的簽章金鑰）由 `bun scripts/dev.ts up` 與 `seed` 備好；身分服務與 `services/web` 在主機上以 `cargo run` 啟動；SPA 是 `bun run --cwd frontend build` 的輸出，由 `services/web` 提供，標頭與正式環境相同（`CSP`：`default-src 'none'; script-src 'self'; style-src 'self'; ...`，沒有 `'unsafe-eval'`；`Content-Security-Policy-Report-Only: require-trusted-types-for 'script'; trusted-types 'none'`）。
- **入口的替代**：沒有入口 gateway，所以用一個只為實測寫的小程式（[`proxy.ts`](data/2026-11-01-949-csp-zod-browser/proxy.ts)，不屬於產品）：`/auth/login` 原樣轉給身分服務；`/api/identity` 先以 `/internal/ext-authz` 檢查，通過才帶 `x-internal-identity` 轉送；其他 `/api`、`/auth`、`/internal` 回純文字 404；其餘轉給 `services/web`。它另有一個控制埠，讓驅動程式指定「某個請求改回什麼」，用來走到健康的後端不會走的路徑：
  - `respond`：由替代程式直接回狀態、內容類型與本文（錯誤本文、不符 schema 的 2xx、助手服務與遙測服務的假資料）；
  - `patch`：取真實的 2xx JSON 回應，改掉或刪掉最上層的成員（例如把 `language` 改成 `5`）；
  - 另可切換「頁面從哪個 web 服務來」，只用在下面的反向對照。
- **瀏覽器**：Playwright 1.49.1 的 headless Chromium（官方映像，與主機共用網路），英文介面（`en-US`），淺色配色，以 `http://localhost` 存取。工作階段 cookie 是 `seed` 印出的開發 session，由 Playwright 以 `__Host-trustiot-session` 放進每個瀏覽器環境（context）。
- **監聽**：每個 context 在第一支頁面 script 之前就以 init script 掛上 `securitypolicyviolation` 的監聽（捕捉階段，記錄 `disposition`、`effectiveDirective`、`blockedURI`、`sample`、來源檔與行號，經 `exposeFunction` 送回驅動程式，換頁也不會掉）；同時記錄所有主控台訊息、`pageerror`、`requestfailed`、API 回應（方法、路徑、狀態、內容類型）與每個文件回應的 CSP 標頭。一個步驟要通過，必須 UI 結果符合預期，而且沒有任何 violation 事件，主控台也沒有提到 CSP 或 Trusted Types 的訊息。
- **日期**：2026-11-01。

## 指令

不碰產品程式碼，所以沒有跑 frontend 的 `api:check`、`typecheck`、`lint`、`test`、`check:dist`（`build` 在 `run.sh` 裡跑過）；另在同一台建置主機跑 `cargo test --locked -p repo-check`：18 + 10 個測試通過，0 失敗。

瀏覽器流程：在 repo 根目錄，於有 podman、bun 與 Rust 工具鏈的 x86_64 Linux 主機上執行（每次都自帶 `up`、實測、`purge`，只清掉自己這個實例）：

```sh
STANDIN=docs/test-reports/data/2026-11-01-949-csp-zod-browser \
  bash docs/test-reports/data/2026-11-01-949-csp-zod-browser/run.sh
```

[`run.sh`](data/2026-11-01-949-csp-zod-browser/run.sh) 依序：`dev.ts up` 與 `seed`（獨立的實例名稱與埠）、建置 SPA、檢視建置輸出的 JavaScript、啟動身分服務（28180）、`services/web`（28280）、反向對照用的第二個 `services/web`（28290）與入口替代（28300，控制埠 28301）、在容器裡以 Playwright 執行 [`drive.mjs`](data/2026-11-01-949-csp-zod-browser/drive.mjs)，最後 `purge`。完整輸出（去掉執行佇列的行與暫存目錄名）在 [`output.txt`](data/2026-11-01-949-csp-zod-browser/output.txt)。

## 結果

整次共 20 個步驟（4 個對照、16 個實測），0 個失敗。對照之外的步驟共 `securitypolicyviolation` 事件 0 個，主控台提到 CSP 或 Trusted Types 的訊息 0 則。

### 建置輸出裡的 zod

建置出的 JavaScript 仍含 zod 的 JIT 程式碼：`static/B1Kiyeui.js`（SPA 的主 bundle）有探測 `try{return Function(``),!0}catch{return!1}`，也有 `o.write(...);o.compile()` 的編譯器；同一個 bundle 裡 `To({jitless:!0})` 在 schema 建立前設定 `jitless`。`sandbox/` 下的三個檔案（儀表板沙箱，Laysi/trustiot-v2#462 還沒接進 SPA）同樣各有一份 zod 與 `jitless` 設定，這次沒有載入它們，不在範圍。

### 對照（controls）：證明監聽與 CSP 看得到 zod 探測會引發的事

| 步驟 | 結果 |
| --- | --- |
| 0a 頁面載入同源的 script，其中呼叫 `new Function("")` | 丟出 `EvalError`；事件 `script-src`（enforce）、`blockedURI: eval`，來源是該 script 的第 1 行 |
| 0d 同一份建置，只把主 bundle 的 `({jitless:!0})` 改成 `({jitless:!1})`，由第二個 `services/web` 提供，開登入頁 | 載入時就出現 `script-src`（enforce）、`blockedURI: eval`，來源是 `static/B1Kiyeui.js:9`，就是 zod 的探測。這證明下面沒有 violation 是因為 `jitless` 生效，而不是因為頁面沒有走到探測 |
| 0b 以 DOM 加一張跨來源圖片 | `img-src`（enforce）事件，主控台「Refused to load the image」 |
| 0c 把 HTML 字串寫進 `innerHTML` | Trusted Types 的事件（`disposition: report`，`require-trusted-types-for`），主控台「[Report Only] This document requires 'TrustedHTML' assignment」 |

0a 要靠頁面自己載入的 script：`page.evaluate` 在頁面的 CSP 之外執行，第一次嘗試時 `new Function("")` 在 `evaluate` 裡沒有被擋（「allowed」），所以改成由替代程式在 `/static/control-eval.js` 回一段 script，再以 DOM 載入它。

### 第 7 項：深連結、語言切換、錯誤頁

| 步驟 | UI 結果 | violation |
| --- | --- | --- |
| A1 未登入，直接開 `/settings`，再重新整理 | 轉到 `/login?next=%2Fsettings`，標題「Sign in to TrustIoT」，重新整理後相同；兩個文件回應都帶正式 CSP（`script-src 'self'`、沒有 `'unsafe-eval'`、Trusted Types 只回報） | 0 |
| A2 已登入，直接開 `/settings`、`/`、`/devices/<id>`、`/no/such/page`，各自重新整理 | 依序顯示「Settings」「Home」「Device not found」（助手 API 回純文字 404）「Page not found」；8 個文件回應都帶正式 CSP | 0 |
| A3 未登入，登入頁的語言選單切成繁體中文，重新整理，再切回 English | 標題「登入 TrustIoT」，`<html lang>` 為 `zh-TW`，重新整理後仍是中文，切回後 `lang` 為 `en` | 0 |
| A4 已登入，設定頁把語言存成繁體中文、重新整理、再把主題存成深色，最後都存回 | 兩個 `PATCH /api/identity/v1/preferences` 都 200（真的身分服務）；標題「個人設定」，重新整理後仍是中文；`data-theme` 為 `dark` | 0 |
| A5 錯誤顯示：`sessions/current` 回 503 純文字（重試三次後），再回 401 | 「Service temporarily unavailable」；401 後轉到 `/login` | 0 |

### 第 8 項：走過 zod 的路徑

| 步驟 | 觸發的 zod 路徑 | UI 結果 | violation |
| --- | --- | --- | --- |
| B1 | `unwrap` 的成功：`sessions/current`、`preferences` 是真的回應；裝置、型號、最新值、時間序列四個 body 是替代程式給的合法假資料（含 discriminated union 與巢狀物件） | 裝置頁顯示「Boiler 1」與最新值 21.5，序列請求 200 | 0 |
| B2a | `unwrap` 的失敗，403 且本文為 `tenant_suspended` 的 M1 錯誤本文 | 「Organization suspended」 | 0 |
| B2b | 錯誤本文的解析：500 的 M1 本文（`details` 含巢狀物件、布林與無限大，`field_errors` 含無效的項目）、500 帶不認得的 `code`、409 `conflict`、500 的 JSON 陣列、500 純文字、500 的 JSON `null` | 每一種都在裝置頁顯示「The device couldn't be loaded: …」；409 顯示「This was changed by someone else…」，證明本文被解析；其他的訊息與預設的 500 相同 | 0 |
| B2c | 錯誤本文的 `field_errors` 與 `details`：設定頁存主題時回 422（`field_errors` 有一個有效的項目夾著無效的）、409、500 純文字 | 主題欄位顯示「Choose one of the listed themes.」；409 顯示「Not saved: This was changed by someone else…」；500 顯示「Not saved: …」 | 0 |
| B3a | `ResponseShapeError`：`preferences` 回 200，`language` 改成 `5` | 設定頁說偏好設定無法載入，變更被停用 | 0 |
| B3b | `ResponseShapeError`：`sessions/current` 回 200，缺 `tenants` | 版面顯示「Service temporarily unavailable」 | 0 |
| B3c | `ResponseShapeError`：裝置 200 但 `device_id` 是數字；之後裝置與型號合法，最新值的讀數類型是 `weird`、時間序列的 `points` 是字串 | 裝置頁顯示載入失敗；重新整理後「The latest values couldn't be loaded」與「The history couldn't be loaded」兩張卡片各自顯示失敗 | 0 |
| B3e | 登入的 union schema：`send_code` 回 429 的 M1 本文；200 但本文是 `{"ok":true}`；200 的合法 `CodeSent`（開出驗證碼步驟）；`verify_code` 回 401 的 M1 本文；200 但 `redirect_to` 是數字 | 依序顯示翻譯後的「Too many requests…」、仍在 email 步驟並顯示錯誤、驗證碼步驟、「That code is wrong or has expired…」、仍在驗證碼步驟並顯示錯誤 | 0 |
| B4a | 偏好設定的讀取，`localStorage` 的 `trustiot.preferences` 在第一個畫面就被讀：合法的 v2、舊的 v1、v2 但語言／主題／時區不合法、v2 只有一個欄位合法（兩組）、含 `__proto__` 與多餘成員、非 JSON 文字、JSON `null`／陣列／字串／數字、空字串（共 12 組） | 每組的 `<html lang>` 與 `data-theme` 都等於預期：合法的保留（`zh-TW` 與 `dark`）、欄位不合法的退回預設、其餘整份退回預設（`en` 與 `light`） | 0 |
| B4b | 同上，但已登入：`localStorage` 為不合法內容 | 等到伺服器的 `GET preferences` 答覆後，`localStorage` 的內容與伺服器的值逐字相同（v2 的格式），`lang` 為 `en`、`data-theme` 為 `light` | 0 |
| B3d | `z.undefined()` 的路徑：把 `DELETE .../sessions/current` 改成 200 加本文，再讓它回真的 204 | 前者顯示「Could not sign out」（`ResponseShapeError`）；後者 204，回到登入頁 | 0 |

每一步的 UI 結果、請求清單、替代程式的命中紀錄與 violation 事件在 [`output.txt`](data/2026-11-01-949-csp-zod-browser/output.txt)。

### 主控台裡出現的其他訊息

沒有被列為 violation 的訊息只有兩類：瀏覽器為 4xx、5xx 的 API 回應印的「Failed to load resource」（步驟刻意製造的），以及換頁或重新整理時被取消的請求（`ERR_ABORTED`）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 瀏覽器實測紀錄存進 `docs/test-reports/`，涵蓋深連結、語言切換、錯誤頁，且主控台沒有 violation | 通過（A1 至 A5；錯誤頁的涵蓋範圍見下面的限制） |
| 觸發第 8 項列出的 zod 路徑時，主控台沒有任何 `securitypolicyviolation`（特別是 `script-src` 的 `unsafe-eval`），紀錄存進 `docs/test-reports/` | 通過（B1 至 B4b、B3d；反向對照 0d 證明同一份建置在沒有 `jitless` 時會報 `script-src` 的 `eval`） |

限制：

- 路由的 `errorElement`（`ErrorPage`，內有 `SignedOutPage`、`ForbiddenPage`、`UnavailablePage`、`GenericErrorPage`）只有畫面元件在渲染時丟出例外才會顯示；目前沒有頁面走得到它，所以這次沒有在瀏覽器出現過，只有 `routes.test.tsx` 的單元測試（一個故意丟錯的路由）涵蓋。實測涵蓋的「錯誤頁」是：找不到頁面、找不到裝置、組織停權、服務暫時無法使用（工作階段讀取失敗）、登入失效後的轉向，以及各頁的內嵌錯誤訊息。
- 單一瀏覽器（Chromium，headless），沒有 Firefox 與 WebKit（Laysi/trustiot-v2#946）；入口是上述替代，不是 Istio（Laysi/trustiot-v2#943）；登入走 `seed` 的開發 session，不是 email 驗證碼，登入畫面的流程以替代程式的假回應走過。
- 裝置、型號、最新值與時間序列是替代程式給的假資料，因為這個環境沒有助手服務與遙測查詢服務；zod 的解析與瀏覽器的行為是真的，資料內容不是。
- Trusted Types 目前只回報；這次沒有任何回報事件（對照 0c 證明回報看得到），所以改成強制之前，這些畫面不需要改。沒有涵蓋 AI 對話畫面（Laysi/trustiot-v2#149）。
- 自動化與跨瀏覽器另見 Laysi/trustiot-v2#945、Laysi/trustiot-v2#946。
