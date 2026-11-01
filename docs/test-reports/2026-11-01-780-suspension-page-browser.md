# SPA 停權說明頁接上真實後端的瀏覽器實測（2026-11-01）

Laysi/trustiot-v2#780（清單 B1；Laysi/trustiot-v2#107 的最後一條驗收條件）：SPA 除了在請求回 403 `tenant_suspended` 時顯示停權頁，也依 `GET /api/identity/v1/sessions/current` 裡目前租戶的 `state` 顯示它。這份報告是以真的身分服務與 `services/web`、在真的瀏覽器裡停權與恢復租戶的證據。

## 環境

- **程式碼**：以 `origin/main` 的 `0627c59a` 為基底加上這個 issue 的變更（`frontend/src/session/` 的停權判斷、`api/identity.ts` 讀出租戶的 `state`、測試用的假身分服務與單元測試、`frontend/README.md`）。
- **機器**：建置主機（arm64 Linux VM），rootless podman。開發環境（Postgres 含 TLS、migration Job、身分服務的簽章金鑰）由 `bun scripts/dev.ts up`、`seed` 備好；身分服務與 `services/web` 在主機上以 `cargo run` 啟動；SPA 是 `bun run --cwd frontend build` 的輸出，由 `services/web` 提供（真的 CSP 標頭）。
- **入口的替代**：本機沒有入口 gateway，所以用一個只為這次實測寫的小程式（`data/.../proxy.ts`，不屬於產品）代替：`/auth/login` 原樣轉給身分服務；`/api/identity` 先以原方法、Host、Cookie、Origin、`Sec-Fetch-Site` 呼叫 `/internal/ext-authz<路徑>`，非 200 原樣回給瀏覽器，200 則帶著回來的 `x-internal-identity` 轉送（用戶端自己帶的丟掉）；其他 `/api`、`/auth`、`/internal` 回 404；其餘轉給 `services/web`。
- **瀏覽器**：Playwright 1.49.1 的 headless Chromium（官方映像，與主機共用網路），英文介面（`en-US`），以 `http://localhost` 存取。工作階段 cookie 用 `seed` 印出的開發 session，由 Playwright 以 `__Host-trustiot-session` 放進瀏覽器。
- **測試資料**（以 `identity_owner` 寫入）：租戶 Acme Factory（開發租戶，使用者是管理員）與 Beta Plant（檢視者），開發使用者設為平台管理者（只為了在同一個瀏覽器以真的 API 停權與恢復；後台按鈕是 Laysi/trustiot-v2#132，不在範圍）。
- **日期**：2026-11-01。

## 指令

單元與檢查（repo 根目錄）：

```sh
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
cargo test -p repo-check
```

瀏覽器流程：把 [`proxy.ts`](data/2026-11-01-780-suspension-page-browser/proxy.ts) 與 [`drive.mjs`](data/2026-11-01-780-suspension-page-browser/drive.mjs) 複製到 repo 外的目錄，在 repo 根目錄執行 [`run.sh`](data/2026-11-01-780-suspension-page-browser/run.sh)（`STANDIN=<那個目錄> bash run.sh`）。它依序：`dev.ts up` 與 `seed`（用獨立的實例名稱與埠，最後只清掉這一份）、寫入測試資料、建置 SPA、啟動身分服務（監聽 28180）、`services/web`（28280）與入口替代（28300）、在容器裡以 Playwright 操作瀏覽器、印出身分服務對狀態改變的日誌，最後 `purge` 掉那個實例。完整輸出在 [`output.txt`](data/2026-11-01-780-suspension-page-browser/output.txt)。

## 結果

### 檢查

| 指令 | 結果 |
| --- | --- |
| `bun run --cwd frontend api:check` | 通過 |
| `bun run --cwd frontend typecheck` | 通過 |
| `bun run --cwd frontend lint` | 通過（Biome，140 個檔案） |
| `bun run --cwd frontend test` | 683 通過、0 失敗（35 個檔案） |
| `bun run --cwd frontend build` | 通過（Vite 的 500 kB 提醒只是提醒） |
| `bun run --cwd frontend check:dist` | 通過 |
| `cargo test -p repo-check` | 18 + 10 通過 |

新增的單元測試（`frontend/src/session/TenantGate.test.tsx`，`a suspended tenant`）：工作階段回報目前租戶停權時顯示停權頁、而且沒有任何請求被拒絕，並可切到其他租戶；切換到已停權的租戶顯示停權頁而不是錯誤；租戶恢復後重新載入顯示正常畫面。`api/identity.test.ts` 檢查 `state` 讀進 SPA 的租戶資料。

### 瀏覽器流程

| 步驟 | 結果 |
| --- | --- |
| 1 以 Acme Factory 為目前租戶開首頁 | `sessions/current` 的兩個租戶都是 `state: active`，顯示「Home」，沒有停權頁 |
| 2 同一個瀏覽器 `PUT /api/identity/v1/platform/tenants/{Acme}/state` 停權（`security_or_abuse`） | 200，版本 2；身分服務記下 `changed a tenant's state` |
| 3 重新整理 | `sessions/current` 的目前租戶 `state` 是 `suspended`；顯示「Organization suspended」，說明文字帶出「Acme Factory has been suspended…」，沒有「Home」；通往這一頁的三個請求（`sessions/current`、`preferences`、`sessions/current`）都是 200，沒有 403；只列出 Beta Plant，並有 Sign out |
| 4 在停權頁選 Beta Plant | `PUT .../sessions/current/tenant` 200，顯示 Beta Plant 的首頁，分頁標題「Beta Plant · TrustIoT」 |
| 5 從頁首的租戶選單切回 Acme Factory（已停權） | 200，顯示停權頁，沒有「Could not switch organization」的錯誤 |
| 6 恢復（`PUT ... {"state":"active"}`）後重新整理 | 恢復 200，版本 3；重新整理後回到正常的「Home」 |
| 7 再停權（`non_payment`，版本 4）、重新整理、在停權頁按 Sign out | `DELETE .../sessions/current` 204，回到 `/login`；同一個 cookie 再讀 `sessions/current` 是 401 |

每一步的 PASS 與請求清單在 `output.txt`。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 停權後，已登入的使用者在瀏覽器看到停權說明頁，可切到自己的其他租戶或登出；恢復後重新整理回到正常畫面（真的身分服務與 `services/web`，紀錄存進 docs/test-reports/） | 通過（步驟 3 至 7） |
| `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist` 通過 | 通過 |

限制：這次用 Chromium 而不是 Chrome 桌面版；入口是上述替代，不是 Istio；登入走 `seed` 的開發 session，不是 email 驗證碼（登入畫面已由 Laysi/trustiot-v2#124 的報告實測）。
