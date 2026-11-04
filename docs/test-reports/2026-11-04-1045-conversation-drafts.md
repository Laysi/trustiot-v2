# 對話裡的儀表板草稿與發布的驗證（2026-11-04）

對應 issue：Laysi/trustiot-v2#1045（對話頁列出這段對話的儀表板草稿，editor 與 admin 可以把草稿儲存到儀表板列表）。畫面的規格在 `docs/design/chat-dashboards/design.md` 的「對話裡的草稿」，程式的說明在 `frontend/README.md` 的「對話畫面」。

## 環境

- 建置主機（Linux）。程式是 `feat/1045-draft-panel` 分支；Bun、Rust 與其他工具是 `.prototools` 釘選的版本。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像（`mcr.microsoft.com/playwright:v1.64.1-noble`，以 digest 釘選）裡的 Chromium，`--network host`、丟掉所有 capability；語言 en-US、時區 UTC；桌機 1280 × 1000 的淺色主題。
- API 是真的：`services/assistant/tests/spa_e2e_1045.rs` 的測試（被忽略的測試，不進 repo，副本是 `scripts/spa_e2e_1045.rs.txt`）在容器裡啟動 Postgres（TLS，產品的 schema）與 ClickHouse（產品的 migration，寫入兩個設備各 24 筆讀數），telemetry-query 的路由與 assistant 的公開 API 在同一個行程。草稿由測試經 store 直接寫進資料庫（`create_draft`、`fork`、`write_draft`、`publish`，與 `dashboard` 工具寫入的路徑相同），沒有呼叫任何模型。
- 預覽頁（不進 repo，副本在 `scripts/`）：以 `frontend/vite.config.ts` 的設定建置 SPA 真正的 `App` 與 `ROUTES`，對話與儀表板的 API 都用真的 `conversationApi(createApiClient())`、`dashboardApi(createApiClient())`；身分服務以測試用的記憶體版本代替（工作階段不是這次要驗的），它給的租戶角色（editor 或 viewer）與代理補上的內部身分 token 的角色相同。一個 Bun 伺服器提供建置結果，每個回應帶 `services/web/src/headers.rs` 的 `CSP`（強制）與 `CSP_REPORT_ONLY`（Trusted Types，只回報），字串逐字相同，並把 `/api/assistant/` 轉給 API、補上內部身分 token（入口會補的那個標頭）。頁面載入前註冊 `securitypolicyviolation` 的收集器，也收主控台的錯誤與警告、頁面例外與所有送往其他來源的請求。

## 指令

```sh
# 前端的檢查，repo 根目錄
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
bun install --cwd scripts --frozen-lockfile && bun test scripts
cargo test --locked -p repo-check

# 端對端：把 scripts/ 的 spa_e2e_1045.rs.txt 放成 services/assistant/tests/spa_e2e_1045.rs，
# 其餘檔案（去掉 .txt）放進 frontend/.preview-1045/，然後在 repo 根目錄
bash frontend/.preview-1045/run.sh
```

`run.sh` 依序：建置預覽頁；背景執行 `cargo test --locked -p assistant --test spa_e2e_1045 -- --ignored --nocapture`，等它寫出 API 的位址、token 與 ID；啟動 `e2e-serve.ts`（只聽 `127.0.0.1`）；在 Playwright 容器裡執行 `e2e.mjs`；最後寫出 `done`。

## 結果

原始資料：[e2e-result.json](data/2026-11-04-1045-conversation-drafts/e2e-result.json)（每一步看到的內容、違規、主控台、其他來源的請求、瀏覽器發出的 API 請求）、[proxied-api-requests.txt](data/2026-11-04-1045-conversation-drafts/proxied-api-requests.txt)（代理轉給 API 的每個請求與狀態碼）。截圖不附（repo 不放圖檔）。

### 端對端的流程（`e2e-result.json`）

| 步驟 | 畫面 | API |
| --- | --- | --- |
| editor 打開有一份草稿的對話 | 右邊的草稿面板：標題「Meter power」、標籤「Draft · version 1 · not saved」、按鈕「Save to dashboard list」；兩欄（左 520 px、右 576 px）；預覽的「Peak power」是 64.954 kW、「Readings」是 48，「Meters」表格每個設備的筆數、最高與總和，都等於直接查 ClickHouse 的值（64.954、48） | 讀對話的草稿列表與草稿那一版；兩個資料集各對草稿的 ID 執行一次（`POST …/dashboards/<草稿>/dataset-runs`），沒有 publish |
| editor 按「Save to dashboard list」 | 「Saved as version 1 in the dashboard list」，標籤變「Published · version 1」，按鈕停用，焦點在「Open the dashboard」 | `POST …/dashboards/<草稿>/publish`，body 是 `{"version":1}`，201；之後 `GET /dashboards` 有「Meter power」（`kind` 是 `saved`、`latest_version` 是 1），連結是 `/dashboards/<那個 ID>` |
| 按「Open the dashboard」 | 儀表板頁「Meter power」，Peak power 64.954、Readings 48、表格與 ClickHouse 相同 | 對儲存的儀表板執行兩個資料集 |
| 儀表板列表 | 有「Meter power」與「Shared dashboard」 | `GET /dashboards` |
| editor 打開以「Shared dashboard」第 1 版為基礎的草稿，而同事已經發布了第 2 版，按儲存 | destructive 的提示「The draft wasn't saved. This draft version is not based on the saved dashboard's latest version (version 2). Ask the assistant to merge that version into the draft, then publish the updated draft.」；按鈕仍可再按；沒有顯示伺服器的原文或訊息鍵 | publish 回 409（`dashboard.saved_changed`，`details.latest` 是 2）；儲存的儀表板的最新版在前後都是 2，沒有被覆蓋 |
| viewer 打開同一段對話 | 草稿與預覽照樣顯示（數字相同），卡片裡是「Only editors and admins can save drafts to the dashboard list. You can look at this one here.」，沒有「Save to dashboard list」 | 直接以 viewer 的身分 `POST …/publish`：403 `permission_denied`（伺服器也不讓 viewer 發布） |

### CSP、Trusted Types 與主控台

對話頁（editor 與 viewer）、儲存之後的儀表板頁與列表、衝突的畫面：`securitypolicyviolation`（強制的與 Trusted Types 的只回報）0 件，頁面例外 0 個，送往其他來源的請求 0 個。主控台只有兩則錯誤，都是瀏覽器對預期的拒絕所記的資源載入失敗：editor 的 409（衝突）與 viewer 的 403（直接呼叫）。

### 前端與其他檢查

在建置主機上，最後的樹：

| 指令 | 結果 |
| --- | --- |
| `api:check`、`typecheck`、`lint`（Biome，241 個檔案）、`build`、`check:dist` | 通過 |
| `bun run --cwd frontend test` | 1063 個通過、0 個失敗（含這張 issue 的 `DraftPanel.test.tsx` 12 個，已合併 main 的 Laysi/trustiot-v2#1073 自訂頁面的接線） |
| `bun test scripts` | 2024 個通過、9 個略過、0 個失敗 |
| `cargo test --locked -p repo-check` | 通過（26 個與 11 個） |

單元測試（`frontend/src/pages/conversations/DraftPanel.test.tsx`，以真的 client 對 `FakeDashboardServer`）涵蓋：草稿與元件畫出的數字等於資料集的值（對草稿的 ID 執行資料集）、沒有草稿時的對話頁、editor 儲存後儀表板在列表裡且連結打開它的頁面、admin 儲存、草稿取自已儲存的儀表板時成為下一個版本、viewer 沒有儲存、草稿依據的版本不是最新時顯示衝突的說明且不覆蓋、權限與封存的拒絕與不認得的訊息鍵都是翻譯過的訊息、讀取草稿與草稿列表失敗時的重試、多份草稿的選擇與已儲存的版本在換草稿再換回來之後仍是已儲存。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 對話頁顯示這段對話的草稿，元件畫出的數字等於資料集的值（測試） | 通過：`DraftPanel.test.tsx`；端對端（真的 ClickHouse，上表） |
| editor 與 admin 發布後，儀表板出現在列表，連結打開它的頁面（測試） | 通過：`DraftPanel.test.tsx`（editor、admin）；端對端（上表） |
| viewer 看不到「儲存到儀表板列表」（測試） | 通過：`DraftPanel.test.tsx`；端對端（上表，API 也回 403） |
| 草稿依據的版本已經不是最新時，畫面顯示衝突的說明，不覆蓋別人的版本（測試） | 通過：`DraftPanel.test.tsx`；端對端（真的 409，儲存的儀表板仍是第 2 版） |
| 以真的 Postgres、ClickHouse 與 assistant API 實測一次顯示與發布 | 通過：本報告 |
| `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist` | 通過：上表；CI 待推送後確認 |

沒有涵蓋：手機寬度與深色主題的草稿面板（版面與其他頁面共用的元件，沒有另做端對端）、zh-TW 介面的端對端（單元測試的字串兩種語言都有對應）、重新整理頁面後，已儲存過的草稿版本仍會顯示「儲存」（API 的草稿列表沒有「這一版已發布」的欄位；再按一次，依 `services/assistant/README.md`「發布的衝突」的規則由伺服器以 409 拒絕，不會重複發布（這個情況沒有另外實測））。
