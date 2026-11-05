# 回答裡的儀表板與從回答儲存的驗證（2026-11-05）

對應 issue：Laysi/trustiot-v2#1049（對話頁的每則回答畫出它寫的儀表板草稿，擁有者若是 editor 或 admin 可以從回答把它儲存到儀表板列表）。畫面規格與和設計稿不同的地方在 `docs/design/chat-dashboards/design.md` 的「回答裡的儀表板」，程式的說明在 `frontend/README.md` 的「對話畫面」。

## 環境

- 開發者工作站（arm64），容器引擎（linux/arm64），沒有 podman、沒有 Kubernetes。程式是 `feat/1049-answer-dashboards` 分支，基底是 main `337b9294`；Bun、Rust 與其他工具是 `.prototools` 釘選的版本。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像（`mcr.microsoft.com/playwright:v1.64.1-noble`，以 digest 釘選，linux/arm64）裡的 Chromium，丟掉所有 capability；語言 en-US、時區 UTC；桌機 1280 × 1000，手機 390 × 844（淺色與深色）。
- API 是真的：`services/assistant/tests/spa_e2e_1049.rs` 的測試（被忽略的測試，不進 repo，副本是 `data/2026-11-05-1049-answer-dashboards/scripts/spa_e2e_1049.rs.txt`）在容器裡啟動 Postgres（TLS，產品的 schema）與 ClickHouse（產品的 migration，兩個設備各 24 筆讀數），telemetry-query 的真路由在同一個行程，assistant 是產品的執行檔。**模型是替身，沒有真的模型呼叫**：替身 AI 閘道依腳本回應，問題含 `[dashboard]` 時呼叫 assistant 的 `dashboard` 工具寫一份新草稿（SQL 資料集，工具以提問者的身分在 telemetry-query 執行），再用文字回答；替身身分服務核發與交換提問的 run 委派，也交換公開 API 的資料集執行所需的 token。
- 預覽頁（不進 repo，副本在 `scripts/`）：以 `frontend/vite.config.ts` 的設定建置 SPA 真正的 `App` 與 `ROUTES`，對話與儀表板的 API 用真的 client；身分服務以測試用的記憶體版本代替。一個 Bun 伺服器提供建置結果，每個回應帶 `services/web/src/headers.rs` 的 `CSP`（強制）與 `CSP_REPORT_ONLY`（Trusted Types，只回報），並把 `/api/assistant/` 轉給 API、補上內部身分 token。頁面載入前註冊 `securitypolicyviolation` 的收集器，也收主控台、頁面例外與所有送往其他來源的請求。
- **和 podman 的做法不同的地方**：這個容器引擎沒有 host network，容器沒有主機的 loopback，`--network host` 到不了主機的連接埠。瀏覽器容器改用 `--add-host host.docker.internal:host-gateway`，`e2e.mjs` 在容器內開一個 TCP 轉送器，把容器自己的 `127.0.0.1:28911` 接到主機的預覽伺服器，所以頁面的來源仍是 `http://127.0.0.1:28911`（安全環境、與先前的報告相同）。預覽伺服器仍只聽主機的 `127.0.0.1`。
- `edge/agent` 有一個與這張 issue 無關的編譯錯誤（這台工作站的 `mode_t` 不是 `edge/agent` 假設的 `u32`），而 assistant 的 dev-dependency 會編到它；為了編譯這個測試，暫時在本機改了一行轉型，沒有進 commit。

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
cargo test --locked -p repo-check

# 端對端：把 data/.../scripts/ 的 spa_e2e_1049.rs.txt 放成 services/assistant/tests/spa_e2e_1049.rs，
# 其餘檔案（去掉 .txt）放進 frontend/.preview-1049/，然後在 repo 根目錄
bash frontend/.preview-1049/run.sh
```

`run.sh` 依序：建置預覽頁；背景執行 `cargo test --locked -p assistant --test spa_e2e_1049 -- --ignored --nocapture`，等它寫出 API 的位址與 token；啟動 `e2e-serve.ts`；在 Playwright 容器裡執行 `e2e.mjs`；最後寫出 `done`，harness 再把 API 的最後狀態寫進 `final.json`。

## 結果

原始資料：[e2e-result.json](data/2026-11-05-1049-answer-dashboards/e2e-result.json)（每一步看到的內容、違規、主控台、其他來源的請求、瀏覽器發出的 API 請求）、[expected-clickhouse.json](data/2026-11-05-1049-answer-dashboards/expected-clickhouse.json)（直接查 ClickHouse 的值）、[final-state.json](data/2026-11-05-1049-answer-dashboards/final-state.json)（API 最後的對話與草稿）、[proxied-api-requests.txt](data/2026-11-05-1049-answer-dashboards/proxied-api-requests.txt)。截圖不附（repo 不放圖檔）。

### 端對端的流程

直接查 ClickHouse 的值：最高功率 64.954 kW、讀數 48 筆；兩個設備各 24 筆，最高 64.954 與 49.954，總和 1,349.1 與 989.1。

| 步驟 | 畫面 | API |
| --- | --- | --- |
| editor 在首頁提問「[dashboard] Show me the power of my two meters over the last day」 | 打開對話頁；回答的文字是替身模型的說明，下面是「Meter power」的內嵌儀表板：標籤「Draft · version 1 · not saved」、版本說明、按鈕「Save to dashboard list」；「Peak power」是 64.954 kW、「Readings」是 48，「Meters」表格每個設備的筆數、最高與總和，都等於直接查 ClickHouse 的值 | 回答的 `draft_versions` 恰好一項（那份草稿的第 1 版）；讀那一版（`GET …/dashboards/<草稿>?version=1`）；兩個資料集各對草稿的 ID 執行一次 |
| 重新整理頁面 | 右邊多了草稿面板，與回答裡是同一版、同樣的數字；回答與面板各有一顆「Save to dashboard list」 | 兩處合計每個資料集只執行一次（`power` 1、`hourly` 1） |
| 在回答裡按「Save to dashboard list」 | 回答裡出現「Saved as version 1 in the dashboard list」，標籤變「Published · version 1」，按鈕停用，焦點在回答裡的「Open the dashboard」；草稿面板同時顯示相同的提示、標籤與停用的按鈕 | `POST …/publish`，body 是 `{"version":1}`，只一次；之後 `GET /dashboards` 有「Meter power」（`kind` 是 `saved`、`latest_version` 是 1），連結是 `/dashboards/<那個 ID>` |
| 按「Open the dashboard」 | 儀表板頁「Meter power」，數字與 ClickHouse 相同；儀表板列表有「Meter power」 | 對儲存的儀表板執行資料集 |
| 重新整理後再按一次同一版 | 回答裡的提示「The draft wasn't saved. This draft version is not based on the saved dashboard's latest version (version 1). … If you just saved this version, version 1 of the saved dashboard is it.」 | publish 回 409（`dashboard.saved_changed`，`details.latest` 是 1）；儀表板仍是第 1 版 |
| viewer 在自己的對話提問同一句 | 回答畫出同一份儀表板（數字相同），整頁沒有「Save to dashboard list」 | 那一版的 `permissions.can_publish` 是 `false`；直接以 viewer 的身分 `POST …/publish` 得 403 `permission_denied` |
| 手機 390 × 844，淺色與深色，打開 editor 已儲存的那段對話 | 頁面與內嵌儀表板都沒有橫向溢位（頁面寬 375 px，儀表板在 60 至 359 px 之間）；兩個主題的數字都與 ClickHouse 相同 | |

第一次執行時，資料集執行回 503：harness 的替身身分服務只有 run 委派的兩個端點，缺公開 API 的資料集執行用的 `/internal/token-exchange`；補上之後重跑。內嵌儀表板在窄的回答欄裡 KPI 兩個一排會把「Readings」折成兩行，因此改用 container query（回答自己的寬度夠才兩個一排），再重跑一次，上表是最後一次的結果。

### CSP、Trusted Types 與主控台

editor 的回答、回答與面板、儲存後的儀表板頁與列表、被拒的畫面，viewer 的回答，以及手機的淺色與深色：`securitypolicyviolation`（強制的與 Trusted Types 的只回報）0 件，頁面例外 0 個，送往其他來源的請求 0 個。主控台只有兩則錯誤，都是瀏覽器對預期的拒絕所記的資源載入失敗：editor 的 409（重複儲存）與 viewer 的 403（直接呼叫）。

### 前端與其他檢查

最後的樹（日期 2026-11-05）：

| 指令 | 結果 |
| --- | --- |
| `api:check`、`typecheck`、`lint`（Biome，271 個檔案）、`build`、`check:dist` | 通過 |
| `bun run --cwd frontend test` | 1134 個通過、2 個失敗：`dashboardFormat > a time shows the precision it has, milliseconds included` 與 `table > every cell is the row's value formatted by its column's type`，兩個都是本機的 ICU 把「Nov 1, 2026, 1:06 AM」寫成「Nov 1, 2026 at 1:06 AM」，不含本分支變更的 main 也一樣失敗（單獨在 `src/dashboard/format.test.ts` 確認），與這張 issue 無關 |
| `cargo test --locked -p repo-check` | 通過（26 與 11 個測試） |

這張 issue 的新測試：`AnswerDashboards.test.tsx` 12 個、`use-near-viewport.test.tsx` 2 個（共 14 個），涵蓋：

- 回答畫出儀表板，數字等於資料集的值，讀的是回答寫的那一版；
- 只畫每個草稿的最後一版、依第一次寫入的順序，沒有寫入或舊的回答沒有儀表板；
- 資料集在靠近視窗之前不執行，整頁（回答與草稿面板）同時最多 2 個；
- 在回答裡儲存，回答與草稿面板都顯示已儲存，反過來也是，焦點只在按下的那一處；
- 取自已儲存的儀表板的草稿存回那個儀表板；
- viewer 與不是擁有者的 editor 看不到按鈕（`permissions.can_publish`）；
- 一個資料集失敗只影響那個元件，一個讀不到的版本在原處顯示一行說明、其餘照常；
- 409 `dashboard.saved_changed` 在回答與面板都多一句「如果你剛儲存過這一版…」，別的拒絕沒有。

`FakeDashboardServer` 的 `can_publish` 改成與伺服器相同（editor 或 admin，而且是草稿的擁有者），非擁有者發布得 403 `dashboard.owner_required`。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 回答畫出它寫的儀表板，元件的數字等於資料集的值（測試） | 通過：`AnswerDashboards.test.tsx`；端對端（真的 ClickHouse，上表） |
| 從回答儲存，儀表板在列表裡且能打開；回答與草稿面板都顯示已儲存（測試） | 通過：`AnswerDashboards.test.tsx`；端對端（上表） |
| viewer 看不到釘選（測試） | 通過：`AnswerDashboards.test.tsx`（viewer 與非擁有者）；端對端（上表，API 也回 403） |
| 只畫每個草稿的最後一版；資料集靠近視窗才執行；讀不到的版本不影響其餘（測試） | 通過：`AnswerDashboards.test.tsx` |
| 以真的 Postgres、ClickHouse 與 assistant API 實測一次（AC 1） | 通過：本報告 |
| 手機寬度沒有橫向溢位、淺色與深色；沒有 CSP 與 Trusted Types 違規、沒有頁面例外 | 通過：上表 |
| `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist` | 通過，`test` 有 2 個與本分支無關的既有失敗（見上）；CI 待推送後確認 |

沒有涵蓋：重新整理後顯示為已儲存（API 沒有這個欄位，Laysi/trustiot-v2#1096）、串流中的回答（Laysi/trustiot-v2#1046）、分享的讀者頁（維護者的 PR 尚未合併）、zh-TW 介面的端對端（單元測試的字串兩種語言都有對應）、回答裡有自訂頁面（`custom_page`）的儀表板（沿用儀表板頁的沙箱 frame，沒有另做端對端）。
