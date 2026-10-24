# 登入與個人設定畫面（2026-10-25）

Laysi/trustiot-v2#124（rebuild checklist A1、L2）：SPA 的第一批正式畫面與 UI 基礎。email 驗證碼登入、登出與登出所有裝置、從身分服務讀取的工作階段與租戶切換、沒有目前租戶時的畫面、存在帳號裡的個人設定、組織停權頁，以及 shadcn + Radix + Tailwind（編譯成靜態 CSS）的元件基礎。規則在 [frontend/README.md](../../frontend/README.md) 的「UI 元件與樣式」「登入與登出」「偏好設定、租戶與工作階段」，決定在 [決策 2](../architecture/decisions.md)。這份報告是六條驗收條件的證據。

## 環境

- **程式碼**：分支 `feat/124-login-settings-ui`，以 `248fae6`（Laysi/trustiot-v2#497 的分支，PR Laysi/trustiot-v2#669）為基底。
  - `361c60c` UI 基礎（Tailwind、Radix、`src/components/ui/`、捲動鎖定的替代）
  - `b1e07af` 畫面（登入、工作階段與租戶、個人設定、登出、停權）
  - `915edc2` 文件（決策 2、`frontend/README.md`、`services/identity/README.md`）
  - 這份報告在下一個 commit 加入。
  - `cdfb842` 安全審查後的修正（見「安全審查後的修正」），那一節在下一個 commit 加入。
- **檢查環境**：開發者工作站。工具版本是 `.prototools` 釘選的版本，前端相依以 `frontend/bun.lock` 鎖定。
- **實際流程**：同一台開發者工作站上的本機開發環境（`scripts/dev.ts`），真的身分服務與 `services/web`，桌面版 Chrome 與 headless Chrome；細節在「實際流程」。
- **日期**：2026-10-25。

## 指令

### 每個 commit 前的檢查

在 repo 根目錄依序執行，三個 commit 各跑一次：

```sh
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
```

文件與 CI 規則相關的檢查（最後一個 commit 前）：

```sh
bun install --cwd scripts --frozen-lockfile
bun test scripts/ci/changes.test.ts
cargo test -p repo-check
```

### 實際流程

本機沒有 Kubernetes 與 Istio，所以用兩個不進版控的小程式代替叢集裡的元件，其餘都是 repo 裡的程式：

- **SMTP 收信器**：以 `scripts/lib/tls.ts` 的 `generateDevCertificates` 產生拋棄式 CA，用 implicit TLS 接受所有信，把解碼後的內文寫成檔案。驗證碼從這裡讀。
- **入口的替代**：照 [身分服務 README](../../services/identity/README.md) 的 ext_authz 說明，`/auth/login` 原樣轉給身分服務；其他 `/api/identity` 請求先以 method、Host、Cookie、Origin、`Sec-Fetch-Site` 呼叫身分服務的 `/internal/ext-authz<path>`，帶著回來的 `x-internal-identity` 轉送（用戶端自己帶的一律丟掉）；其他 `/api`、`/auth`、`/internal` 回 404；其餘路徑轉給 `services/web`，回應標頭（含 CSP）原樣保留。

```sh
# 開發環境：Postgres（TLS）、migration Job、身分服務的簽章金鑰，以及 seed
bun scripts/dev.ts up
bun scripts/dev.ts seed
set -a; eval "$(bun scripts/dev.ts env)"; set +a

# 測試資料（以 identity_owner 寫入；只用 example.com 的地址）
psql <<'SQL'
SET ROLE identity_owner;
INSERT INTO identity.tenants (tenant_id, name) VALUES
  ('00000000-0000-4000-8000-0000000124a1', 'Acme Factory'),
  ('00000000-0000-4000-8000-0000000124a2', 'Beta Plant');
INSERT INTO identity.users (id, email, platform_admin) VALUES
  ('00000000-0000-4000-8000-0000000124c1', 'alice@example.com', false),
  ('00000000-0000-4000-8000-0000000124c2', 'bob@example.com', false),
  ('00000000-0000-4000-8000-0000000124c3', 'carol@example.com', false);
INSERT INTO identity.memberships (tenant_id, user_id, tenant_role) VALUES
  ('00000000-0000-4000-8000-0000000124a1', '00000000-0000-4000-8000-0000000124c1', 'admin'),
  ('00000000-0000-4000-8000-0000000124a2', '00000000-0000-4000-8000-0000000124c1', 'viewer'),
  ('00000000-0000-4000-8000-0000000124a1', '00000000-0000-4000-8000-0000000124c2', 'editor');
SQL

# 身分服務：寄信到 SMTP 收信器（埠 28465，CA 是收信器產生的那一份）
TRUSTIOT_INGRESS_ROUTES='[{"host":"localhost","path":"/api/identity","match":"PathPrefix","service":"identity"},{"host":"localhost","path":"/","match":"PathPrefix","service":"assets"}]' \
TRUSTIOT__DATABASE__USERNAME="$TRUSTIOT_DEV_IDENTITY_DATABASE_USERNAME" \
TRUSTIOT__DATABASE__PASSWORD="$TRUSTIOT_DEV_IDENTITY_DATABASE_PASSWORD" \
TRUSTIOT__DATABASE__PLATFORM_USERNAME="$TRUSTIOT_DEV_IDENTITY_PLATFORM_DATABASE_USERNAME" \
TRUSTIOT__DATABASE__PLATFORM_PASSWORD="$TRUSTIOT_DEV_IDENTITY_PLATFORM_DATABASE_PASSWORD" \
TRUSTIOT__HTTP__LISTEN_ADDR=127.0.0.1:28180 TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR=127.0.0.1:28181 \
TRUSTIOT__EXCHANGE__LISTEN_ADDR=127.0.0.1:28182 \
TRUSTIOT_SMTP_HOST=localhost TRUSTIOT_SMTP_PORT=28465 TRUSTIOT_SMTP_SECURITY=tls \
TRUSTIOT_SMTP_FROM='TrustIoT <noreply@example.com>' TRUSTIOT_SMTP_CA_FILE=<收信器的 CA 檔> \
cargo run -p identity

# 建置的 SPA 由 services/web 提供（真的 CSP 標頭）
bun run --cwd frontend build
TRUSTIOT__ROOT_DIR=<frontend/dist 的副本> TRUSTIOT__HTTP__LISTEN_ADDR=127.0.0.1:28280 \
TRUSTIOT__HTTP__HEALTH_LISTEN_ADDR=127.0.0.1:28281 cargo run -p web

# 入口的替代在 http://localhost:28300 接收瀏覽器的請求，轉給 28180（身分服務）與 28280（web）
```

瀏覽器打開 `http://localhost:28300/login`。Chrome 把 `localhost` 視為安全來源，所以 `__Host-` 開頭、帶 `Secure` 的 session cookie 在 http 下也會保留。每個頁面載入後在 console 掛上 `securitypolicyviolation` 的收集器，每一步之後讀它、`document.querySelectorAll("style").length` 與 `body[data-scroll-locked]`。另一個裝置以同一個入口、不帶 cookie 的請求登入（`send_code`、讀收信器、`verify_code`），用它的 cookie 讀寫 API。窄螢幕以 headless Chrome 經 DevTools Protocol 模擬 360 px 寬的手機視窗，放進另一個裝置的 cookie 後截圖，並收集 Log 與 console。

`up` 的 example-service 這次沒有啟動成功，本流程只用到 Postgres、migration 與身分服務，不受影響。

## 結果

### 檢查

| 指令 | 結果 |
| --- | --- |
| `bun install --cwd frontend --frozen-lockfile` | 通過（lockfile 不變） |
| `bun run --cwd frontend api:check` | 通過 |
| `bun run --cwd frontend typecheck` | 通過 |
| `bun run --cwd frontend lint` | 通過（Biome，80 個檔案） |
| `bun run --cwd frontend test` | `915edc2`：187 通過、0 失敗（17 個檔案）；`361c60c`：135 通過 |
| `bun run --cwd frontend build` | 通過；Vite 提醒 JS 超過 500 kB（只是提醒，見下） |
| `bun run --cwd frontend check:dist` | 通過（沒有會被 CSP 擋下的 inline 內容與 `data:`，版面與檔案類型符合 `services/web` 的約定） |
| `bun test scripts/ci/changes.test.ts` | 282 通過、0 失敗 |
| `cargo test -p repo-check` | 17 + 9 通過 |

建置結果的大小（`b1e07af` 對基底 `248fae6`）：

| 檔案 | 基底 | 本分支 |
| --- | --- | --- |
| JS | 477.83 kB（gzip 145.64 kB） | 657.96 kB（gzip 202.78 kB） |
| CSS | 1.88 kB | 28.33 kB（gzip 6.44 kB） |

### 單元測試對照

畫面測試以 `src/test/fake-identity.ts` 的假身分服務執行（使用者、租戶、伺服器端的工作階段、偏好、驗證碼信箱；回應照身分服務的 README），等待一律用 `findBy*` 與 `waitFor`，沒有固定的短時間等待。

| 驗收條件 | 測試 |
| --- | --- |
| 驗證碼登入 | `src/pages/LoginPage.test.tsx`：成功登入並回到 `next`、已登入時再登入、空白地址、email 的 422、寄信 503、驗證碼錯誤 401、短的驗證碼與 `/code` 的 422、`/redirect_to` 的 422 與驗證時 503、重新寄送後舊碼失效、改用其他地址、沒有帳號的地址回應相同、瀏覽器沒有保留 cookie；`src/api/identity.test.ts` 的兩個步驟與回應形狀 |
| 登出所有裝置 | `src/pages/SettingsPage.test.tsx` 的 `signing out`：先確認、結束每個工作階段（另一個瀏覽器隨即 401）並在登入頁說明、取消後焦點回到按鈕、失敗時在對話框說明、這個工作階段已結束時在登入頁與登入後的設定頁說明其他裝置還沒登出（見「安全審查後的修正」）；帳號選單只登出這個瀏覽器 |
| 切換語言後介面與錯誤訊息改變 | `LoginPage.test.tsx` 的 `switching the language changes the page and the error already shown…`；`SettingsPage.test.tsx` 的 422 錯誤換語言後重新翻譯、`changing the interface language…`；`TenantGate.test.tsx` 的 `the no-tenant page is translated like every other` |
| 不在任何租戶時 | `src/session/TenantGate.test.tsx`：多個租戶時選擇、一個租戶時自動切換、自動切換失敗可重試、沒有租戶時說明並可重新檢查、個人設定不需要租戶 |
| 偏好經 API 讀寫 | `SettingsPage.test.tsx`：顯示帳號的值而不是瀏覽器副本、伺服器回應前不能修改、逐欄位 `PATCH`、422 退回並顯示在該欄位、其他錯誤顯示在該欄位、兩個瀏覽器讀到同樣的值、讀不到時可重試；`src/app/routes.test.tsx` 的帳號語言與主題套用、登出後用瀏覽器副本 |
| Radix 彈出元件不注入 `<style>` | `src/lib/no-style-singleton.test.tsx`：alias 只換 `react-style-singleton`、export 與原套件相同、打開選單與對話框時鎖住捲動且沒有 `<style>`；`src/session/TenantSwitcher.test.tsx` 的 Escape 關閉並把焦點還給觸發按鈕 |

其他：停權頁（`TenantGate.test.tsx` 的 `a suspended tenant`）、別處結束的工作階段（導向登入、清掉快取）、切換租戶取消並丟掉舊租戶的資料、保留使用者自己的查詢、分頁標題。

### 實際流程

以真的身分服務、`services/web` 與桌面版 Chrome 依序操作：

| 步驟 | 結果 |
| --- | --- |
| 打開 `/login`（未登入） | `GET /api/identity/v1/sessions/current` 回 401，當成未登入而不是錯誤，顯示登入頁 |
| alice 送出 email | `POST /auth/login` 202；收信器收到中英雙語的 8 位數驗證碼 |
| 輸入錯的驗證碼 | 401，錯誤顯示在驗證碼欄位；登入頁切到 English，同一個錯誤變成英文 |
| 輸入正確的驗證碼 | 200，`redirect_to` 是 `/`；帳號的語言（`auto`，瀏覽器是繁體中文）取代登入頁選的 English |
| alice 沒有目前租戶、屬於兩個 | 「選擇組織」列出 Acme Factory（管理員）與 Beta Plant（檢視者）；選 Acme 後 `PUT .../sessions/current/tenant` 200，分頁標題「Acme Factory · TrustIoT」 |
| 租戶選單切到 Beta Plant | `PUT` 200，標題「Beta Plant · TrustIoT」，選單關閉後焦點回到觸發按鈕 |
| 個人設定改主題、時區、AI 語言 | 三次 `PATCH /api/identity/v1/preferences` 200，立刻套用（淺色）；資料庫 `identity.preferences` 那一列是新值 |
| 時區改成伺服器沒有的名稱 | 在 devtools 加一個 `Mars/Olympus` 選項後選它（瀏覽器與伺服器的時區清單可能不同）：`PATCH` 422，欄位顯示「請選擇有效的時區。」並退回 Europe/Berlin；介面語言改成 English 後錯誤變成「Choose a valid time zone.」 |
| 另一個裝置改偏好 | 另一個裝置的 `PATCH` 把語言改成 zh-TW、主題改成 dark；瀏覽器重新載入後顯示伺服器的值，`localStorage` 的副本跟著改 |
| 登出所有裝置 | 確認對話框（Escape 關閉、焦點回到按鈕）後確認：`DELETE /api/identity/v1/sessions` 204，回到登入頁並顯示「你已登出所有裝置。」；另一個裝置的 `GET .../sessions/current` 變成 401；資料庫裡 alice 沒有有效的工作階段 |
| bob 登入（只屬於一個租戶） | 登入後隨即 `PUT .../sessions/current/tenant` 200，直接進入 Acme Factory；頁首只顯示租戶名稱，沒有選單 |
| bob 從帳號選單登出 | `DELETE /api/identity/v1/sessions/current` 204，登入頁顯示「你已登出。」 |
| carol 登入（不屬於任何租戶） | 「還沒有組織」的說明、重新檢查與登出；把 carol 加進 Beta Plant 後按重新檢查，自動切換進 Beta Plant |
| carol 登出後打開 `/settings` 再登入 | 導向 `/login?next=%2Fsettings`，登入後回到 `/settings` |

入口記錄的請求（含另一個裝置的請求）：

| 請求 | 狀態 | 次數 |
| --- | --- | --- |
| `POST /auth/login` | 202 / 200 / 401 | 7 / 6 / 1 |
| `GET /api/identity/v1/sessions/current` | 200 / 401 | 13 / 8 |
| `PUT /api/identity/v1/sessions/current/tenant` | 200 | 4 |
| `GET /api/identity/v1/preferences` | 200 | 13 |
| `PATCH /api/identity/v1/preferences` | 200 / 422 | 5 / 1 |
| `DELETE /api/identity/v1/sessions/current` | 204 | 2 |
| `DELETE /api/identity/v1/sessions` | 204 | 1 |

### CSP

`services/web` 送出的標頭（`curl -sI http://localhost:28300/settings`）：

```text
Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; font-src 'self'; connect-src 'self'; manifest-src 'self'; worker-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'; object-src 'none'
Content-Security-Policy-Report-Only: require-trusted-types-for 'script'; trusted-types 'none'
```

在這個 CSP 下打開並關閉每個 Radix 彈出元件：租戶選單（`DropdownMenu`，選擇一項關閉）、帳號選單（`DropdownMenu`，Escape 與選擇一項關閉）、登出所有裝置的確認對話框（`AlertDialog`，Escape、取消與確認關閉）。每次打開時 `body[data-scroll-locked]` 是 `1`、`body` 的 `overflow` 是 `hidden`（靜態 CSS 的規則），關閉後屬性移除、`overflow` 回到 `visible`。

- `securitypolicyviolation` 收集器在每一步之後都是空的，`enforce` 與 `report`（Trusted Types）都沒有。
- 文件裡的 `<style>` 元素始終是 0 個；樣式只有 `static/<hash>.css` 一個 stylesheet。
- console 沒有本站程式的訊息。唯一的一則是瀏覽器擴充功能自己的例外，來源是擴充功能的 script，不是本站。
- headless Chrome 打開 `/settings`、`/` 與 `/login` 時，Log 與 console 都沒有任何項目。

### 窄螢幕

headless Chrome 以 360 px 寬打開 `/settings`、`/` 與 `/login`：`document.documentElement.scrollWidth` 都等於 360（沒有橫向捲動），個人設定的四個欄位變成一欄，頁首在一列內。桌面版 Chrome 的視窗一度只有 392 px 寬，登入頁、首頁、沒有組織的頁面與帳號選單在那個寬度也沒有橫向捲動，選單在視窗之內。

### 實際操作中發現並修正的問題

都在 `b1e07af` 之前修正，修正後重新建置再操作：

- 登入頁的語言選單寬度固定，「跟隨瀏覽器（繁體中文）」被截斷：改成依最長的選項決定寬度。
- 選擇組織的列表文字置中（`<button>` 預設置中）：改成靠左。
- 個人設定兩欄並排時，有說明文字的欄位讓同一列另一欄的標籤與選單往下撐開：表單欄位改成 `content-start`，同一列的選單對齊。
- 頁首在 360 px 寬換成兩列：窄螢幕縮小間距。

自動化瀏覽器的分頁不在前景（`document.visibilityState` 是 `hidden`），CSS 動畫只在畫面繪製時前進；Radix 在離場動畫結束前保留關閉中的元件，所以關閉後要先截圖讓畫面繪製一次，才會看到元件移除、捲動解鎖。前景分頁沒有這個情況。

### 實際操作之後的修正

commit 前的程式審查又修正兩處，瀏覽器流程沒有重跑，以下列證據取代：

- `--font-sans` 原本宣告在 `@theme inline` 裡。建置出的 CSS 原本就有這個變數，改成一般的 `@theme` 區塊後意圖更明確；建置結果的 `@layer theme` 有 `--font-sans`，preflight 的 `--default-font-family` 是 `var(--font-sans)`。
- 切換租戶原本取消所有進行中的請求，包括偏好的第一次讀取，讀取被取消後會一直停在載入中。改成不取消偏好；`TenantSwitcher.test.tsx` 的 `a switch while the preferences are still loading lets them finish loading` 修正前失敗、修正後通過。

### 安全審查後的修正

2026-10-25 對這個分支做了獨立的安全審查，沒有 Critical、High、Medium，找到三項 Low，都在前端，在 `cdfb842` 修正。另一項 Info（伺服器端的 `redirect_to` 接受 `.`、`..` 這類點段）在 Laysi/trustiot-v2#677 處理，這裡不改。

每項的回歸測試都以突變確認抓得到：拿掉修正後測試失敗，還原後通過。

#### Low-1：`safeNextPath` 經路徑正規化後會回傳 `//host`

- **問題**：前置檢查只看原始字串的開頭，解析後的路徑可能是 `//host`（`/.//evil`、`/..//evil`、`/%2e%2e//evil`、`/./\evil`、`/x/../\evil`）。目前靠伺服器的 422 與 React Router 拒絕外部導向擋下，但路由拒絕時登入頁會一直停在「驗證中」。
- **修正**：解析後的路徑再做一次同樣的檢查（`//`、`/\`、控制字元、保留路徑、登入頁），不通過就回 `/`。登入頁的比對與路由一樣不分大小寫、不管結尾的 `/`。
- **回歸測試**：
  - `src/app/paths.test.ts` 的 `rejects %p, which parses into another host or a path it may not return to`（19 個寫法，含 `%2E`、大寫、帶 `?` 與 `#`、`/LOGIN`、`/login/`）與 `normalizes dot segments that stay inside the SPA`。
  - `src/pages/LoginPage.test.tsx` 的 ``a `next` of %p that parses into another host or the sign-in page asks for and goes to the home page``（6 個寫法：送出的 `redirect_to` 是 `/`，登入後到首頁）與 ``a `redirect_to` from the server that parses into another host goes to the home page``。
- **突變與結果**：
  - 拿掉解析後的 `//`、`/\` 檢查：20 個失敗（paths 14 個、LoginPage 6 個，後者停在登入頁）。
  - 登入頁改回只比對 `/login`：4 個失敗（`/LOGIN`、`/Login?next=%2F`、`/login/`，以及 LoginPage 的 `/x/../LOGIN`；後者停在登入頁，證實路由不分大小寫）。
  - 還原後兩個檔案 63 個測試全部通過。

#### Low-2：同一個工作階段可以同時送出兩個切換租戶的請求

- **問題**：頁首的租戶選單與租戶清單各自管停用狀態，切換的 mutation 沒有 scope。先點清單的 T1、馬上從頁首選 T2，伺服器最後在 T2，但 T1 的回應晚到時快取被寫成 T1。
- **修正**：
  - `SessionProvider` 提供共用的 `switching`，切換中每個入口都停用，handler 裡也再檢查一次。
  - 切換的 mutation 加上 `scope`，一次只送一個、依發出的順序。
  - 回應改在 mutation 的 `onSuccess` 寫進快取。TanStack Query 等 `onSuccess` 完成才放行同一個 scope 的下一個切換，所以快取的寫入順序就是伺服器的處理順序。
  - 沒有另外加世代編號，理由見下。
- **回歸測試**：`src/session/TenantSwitcher.test.tsx` 的 `switching from more than one place`。假身分服務加了 `holdNextAnswer`：請求立刻生效，回應等到測試放行。
  - `a switch from the tenant list disables the top bar and the rest of the list until it lands`
  - `a switch from the top bar disables the tenant list until it lands`
  - `two switches started together run one after the other, and the cache ends where the server does`：清單與頁首在同一輪各觸發一次，第一個回應晚到。
  - `the next switch waits until the earlier answer is cached, however long caching it takes`
- **突變與結果**：
  - 拿掉 `scope`：`two switches…` 失敗，第二個請求在第一個回應前就送出。只留最後狀態的斷言時，快取是 T1、伺服器是 T2，重現審查描述的情況。
  - 頁首改回自己的停用狀態（修正前的 `TenantSwitcher.tsx`）：`a switch from the tenant list…` 失敗。
  - 清單改回自己的停用狀態（修正前的 `TenantList.tsx`）：`a switch from the top bar…` 失敗。
  - 保留 `scope`，但把快取的寫入移回 `mutateAsync` 之後：`the next switch waits…` 失敗。
  - 還原後全部通過。

沒有加世代編號的理由：切換在同一個 scope 裡依序執行，而 TanStack Query（`@tanstack/query-core` 5.104.5 的 `Mutation.execute`）在 mutation 自己的 `onSuccess` 完成、狀態變成 success 之後，才在 `finally` 繼續同一個 scope 的下一個 mutation。回應在 `onSuccess` 裡寫進快取，所以下一個請求送出前，前一個回應已經寫好；伺服器也依送出的順序處理，因為下一個請求在前一個回應之後才送出。快取最後寫入的就是伺服器最後處理的那一個，世代編號在這個順序下不會擋下任何回應，只會是測不到的程式碼。後送的切換失敗時，先前成功的回應照樣寫入，與伺服器一致；若改成「只套用最後發出的切換」，反而會在這個情況丟掉正確的回應。仍然留下的情況：請求在伺服器處理後才因網路中斷而失敗時，快取停在前一個租戶，直到工作階段的查詢過期（60 秒）重新讀取。這與並行無關，世代編號也處理不了。

#### Low-3：「登出所有裝置」遇到 401 時，畫面沒有說其他裝置還沒登出

- **問題**：登出所有裝置遇到 401（自己的工作階段剛好過期）時走通用的導向，登入頁沒有任何提示。重新登入回到設定頁後，使用者以為已經成功，其他裝置的工作階段仍然有效。
- **修正**：
  - 這個 mutation 標上 `HANDLES_UNAUTHENTICATED`，自己處理 401：導向 `/login?next=%2Fsettings`，history state 帶 `signOutEverywhereUnfinished`，經 zod 解析，做法與 `signedOut` 相同。
  - 登入頁在兩個步驟都提示其他裝置還沒登出、登入後請再按一次。登入後把同樣的 state 帶到 `/settings`，「已登入的裝置」再提示一次。
  - 文字有 zh-TW 與 en。
- **回歸測試**：`src/pages/SettingsPage.test.tsx` 的 `of all devices when this session had already ended says the others are still signed in, before and after signing in again`。工作階段在伺服器端結束後按登出所有裝置：登入頁顯示提示、不顯示「你已登出所有裝置」、每一次到登入頁都帶著提示的 state、另一個裝置仍登入著；登入後回到 `/settings`，「已登入的裝置」顯示提示。
- **突變與結果**：
  - 拿掉 `onError` 與 meta（修正前的行為）：失敗，登入頁沒有提示。
  - 拿掉 meta、保留 `onError`：失敗，通用的處理先導向一次沒有提示的登入頁（正式環境是兩個導向互相競爭）。
  - 登入頁不把 state 帶到下一頁：失敗，設定頁沒有提示。
  - 還原後通過。

#### 修正後的檢查

`cdfb842`，指令同「每個 commit 前的檢查」：`install`（lockfile 不變）、`api:check`、`typecheck`、`lint`（80 個檔案）、`test`（218 通過、0 失敗，17 個檔案）、`build`、`check:dist` 全部通過。瀏覽器的實際流程沒有重跑。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 端到端測試：以驗證碼登入、登出所有裝置 | **通過**（實際流程，真的身分服務與瀏覽器）。自動化的瀏覽器 e2e（Playwright）依驗證分級延後為進階驗證，見下 |
| 切換語言後介面與錯誤訊息改變（測試） | **通過**：單元測試與實際流程（登入頁的 401、個人設定的 422） |
| 不在任何租戶時引導選租戶、只屬於一個時自動切換、不屬於任何租戶時說明（測試） | **通過**：`TenantGate.test.tsx` 與實際流程（alice 選擇、bob 自動切換、carol 說明後重新檢查） |
| 偏好經 API 讀寫，另一個 session 讀到相同設定，422 顯示在對應欄位（測試） | **通過**：`SettingsPage.test.tsx` 與實際流程（另一個裝置的修改、時區的 422） |
| 決策 2 與 `frontend/README.md` 寫明 UI 元件與樣式的規則與 assistant-ui | **通過**：`915edc2` |
| 瀏覽器實際操作登入與個人設定，開關 Radix 彈出元件，沒有 CSP violation | **通過**：見「CSP」 |

2026-10-25 的獨立安全審查沒有 Critical、High、Medium；三項 Low 已在 `cdfb842` 修正，各有回歸測試（見「安全審查後的修正」），Info 由 Laysi/trustiot-v2#677 處理。

範圍內但沒有在實際流程測的：

- 停權頁：**待測**。目前沒有服務會回 `tenant_suspended`（後端是 Laysi/trustiot-v2#107），只有單元測試。
- 身分服務 503 時的畫面：**待測**於實際流程，入口的替代無法重現真的入口在服務停止時的回應；單元測試涵蓋登入兩個步驟的 503 與偏好儲存失敗。

延後的進階驗證（依組織的驗證分級，另開最低優先級的 issue）：

- 以 Playwright 自動化本報告的瀏覽器流程（登入、登出所有裝置、個人設定、切換租戶、開關每個 Radix 彈出元件並檢查 console 與 `securitypolicyviolation`）。
- JS 增加約 180 kB（gzip 約 57 kB），超過 Vite 的 500 kB 提醒：量測載入時間，決定是否以路由切分程式碼。
