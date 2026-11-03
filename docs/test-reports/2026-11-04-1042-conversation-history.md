# 對話紀錄與對話頁的驗證（2026-11-04）

對應 issue：Laysi/trustiot-v2#1042（對話紀錄與對話頁：列表、改名、封存、刪除與還原，回答的安全 Markdown）。畫面的規格在 `docs/design/chat-dashboards/design.md` 的「對話紀錄與對話頁」一節與 `docs/design/app-shell.md`，程式的說明在 `frontend/README.md` 的「對話畫面」。

## 環境

- 建置主機（arm64 Linux VM），rootless podman。程式是 `feat/1042-conversation-history` 分支，樹與 `fcc8ce63` 相同；Bun、Rust 與其他工具是 `.prototools` 釘選的版本。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像（`mcr.microsoft.com/playwright:v1.64.1-noble`，以 digest 釘選，linux/arm64）裡的 Chromium，`--network host`、丟掉所有 capability；語言 en-US、時區 UTC；桌機 1280 × 900 的淺色主題，最後一段是手機 390 × 844 的深色主題。
- API 是真的：`services/assistant/tests/spa_e2e_1042.rs` 的測試（被忽略的測試，不進 repo，副本是 `scripts/spa_e2e_1042.rs.txt`）在容器裡啟動 Postgres（TLS，migration Job 套用產品的 schema），以 chart 的方式啟動 assistant 執行檔。兩個對話由真正的 agent 對著替代的模型回答後存進資料庫：一個的回答是惡意的 Markdown（外部圖片、`data:` 圖片、同源圖片、外部連結、autolink、`javascript:`、`//host`、原始 HTML 的 `img`／`iframe`／`a`、表格，以及兩個本站的路徑 `/settings` 與 `/dashboards`），一個是普通的回答。
- 預覽頁（不進 repo，副本在 `scripts/`）：以 `frontend/vite.config.ts` 的設定（含 `STYLE_SINGLETON_ALIAS` 與 `CHARACTER_REFERENCE_ALIAS`）建置 SPA 真正的 `App` 與 `ROUTES`，對話的 API 用真的 `conversationApi(createApiClient())`；身分服務以測試用的記憶體版本代替（工作階段不是這次要驗的）。一個 Bun 伺服器提供建置結果，每個回應帶 `services/web/src/headers.rs` 的 `CSP`（強制）與 `CSP_REPORT_ONLY`（Trusted Types，只回報），字串逐字相同，並把 `/api/assistant/` 轉給 API、補上對話擁有者的內部身分 token（入口會補的那個標頭）。頁面載入前註冊 `securitypolicyviolation` 的收集器（強制與只回報的都收），也收主控台的錯誤與警告、頁面例外與所有送往其他來源的請求。

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

# 端對端：把 scripts/ 的 spa_e2e_1042.rs.txt 放成 services/assistant/tests/spa_e2e_1042.rs，
# 其餘檔案（去掉 .txt）放進 frontend/.preview-1042/，然後在 repo 根目錄
bash frontend/.preview-1042/run.sh
```

`run.sh` 依序：建置預覽頁；背景執行 `E2E_DIR=<目錄> cargo test --locked -p assistant --test spa_e2e_1042 -- --ignored --nocapture`，等它寫出 API 的位址與 token；啟動 `e2e-serve.ts`（只聽 `127.0.0.1`）；在 Playwright 容器裡執行 `e2e.mjs`；最後寫出 `done`，測試再經同一個 API 讀回全部的狀態（`final-state.json`）。

## 結果

原始資料：[e2e-result.json](data/2026-11-04-1042-conversation-history/e2e-result.json)（每一步看到的內容、違規、主控台、其他來源的請求、經過代理的 API 請求）、[final-state.json](data/2026-11-04-1042-conversation-history/final-state.json)（瀏覽器流程後 API 的列表與兩個對話）。截圖不附（repo 不放圖檔）。

### 端對端的流程（`e2e-result.json`）

| 步驟 | 畫面 | API |
| --- | --- | --- |
| 首頁 | 「最近的對話」兩張卡片，較新的在前，最後是「All conversations」（`/conversations`）；兩個對話沒有標題，顯示「Conversation started …」 | `GET …/conversations?limit=5` |
| 對話紀錄：改名 | 「Renamed to “Floor 3 energy”.」，列的連結文字換成新標題 | `PATCH`；之後 `title` 是 `Floor 3 energy`、`title_source` 是 `user` |
| 封存、看「Archived」、取消封存 | 封存的列離開「In use」；`?tab=archived` 有那一列與「Archived …」；取消封存後回到「In use」 | 兩次 `PATCH`；最後 `archived_at` 是 `null` |
| 打開惡意的對話 | 見下一節 | `GET …/conversations/<id>` |
| 複製回答 | 「Answer copied」；剪貼簿的內容與 API 給的回答逐字相同（466 字元） | |
| 匯出 Markdown | 下載 `Floor 3 energy.md`；內容與 `# 標題` 加 API 的每則問題與回答逐字相同（548 字元）；「Exported a Markdown file.」 | |
| 回答裡的「the settings」連結 | 到 `/settings`；頂列的「Back to the conversation」指向這個對話，按下回到對話頁 | |
| 從對話頁刪除 | 回到對話紀錄，狀態列寫「Deleted “Floor 3 energy”. You can restore it under “Deleted” for 30 days.」與「Show deleted conversations」 | `DELETE` 204 |
| 看已刪除 | `?tab=deleted`，那一列的「Can be restored until」是 `2026-12-03T17:41:45.021820Z`，等於 API 的 `restorable_until`，正好是 `deleted_at` 加 30 天 | `GET …/deleted-conversations?limit=100` |
| 還原 | 「Restored “Floor 3 energy”. It is back in use.」，回到「In use」 | `POST …/restore`；`final-state.json` 的已刪除是空的、兩個對話都在使用中 |

### 惡意回答在瀏覽器裡（`e2e-result.json` 的 `conversation: hostile answer as shown`）

- 回答裡的元素：`<img>` 0 個、`<iframe>` 0 個；粗體與表格照常畫出。
- 連結只有兩個：`/settings` 與 `/dashboards`，解析後都是同源。外部連結、autolink、`javascript:` 與 `//evil.example.test/x` 都是文字，後面以括號寫出網址（例如「the report (https://evil.example.test/report?d=tenant-data)」）。
- 三張圖片都是文字：「[image not shown: floor plan]」、「[image not shown: chart]」、「[image not shown: same origin]」；同源的 `/static/leak.png` 是 CSP 的 `img-src 'self'` 允許的，所以它不是被 CSP 擋下，而是沒有元素。
- 整頁的 `<img>` 只有 app 自己的 `/favicon.svg`（頂列與回答旁的 TrustIoT 標誌）；送往其他來源的請求 0 個；Resource Timing 裡沒有任何 `leak`、`evil`、`raw.png` 的網址。
- 原始 HTML：服務端的 answer guard（`services/assistant/src/answer_guard.rs`）在存下回答前已移除 HTML 標籤、留下文字，所以 API 給的回答裡只剩「raw link」這個字，瀏覽器這一段看不到原始 HTML；前端把 HTML 當文字的規則由 `src/chat/safe-markdown.test.tsx` 的原始 HTML 案例驗證。

### CSP、Trusted Types 與主控台

首頁、對話紀錄（改名、封存、刪除、還原）、惡意的對話頁、普通的對話頁，以及手機寬度深色主題的對話頁、對話紀錄與首頁：`securitypolicyviolation`（強制的與 Trusted Types 的只回報）0 件，主控台的錯誤與警告 0 則，頁面例外 0 個；每頁的 `<style>` 與沒有 `src` 的 `<script>` 都是 0 個。手機寬度的三頁都沒有水平溢位。

### 前端與其他檢查

在建置主機上，樹與 `fcc8ce63` 相同：

| 指令 | 結果 |
| --- | --- |
| `api:check`、`typecheck`、`lint`（Biome，224 個檔案）、`build`、`check:dist` | 通過 |
| `test` | 945 個通過、0 個失敗 |
| `bun test scripts` | 2024 個通過、0 個失敗 |
| `cargo test --locked -p repo-check` | 37 個通過 |

對話頁的程式在自己的延遲載入 chunk 裡（`routes.tsx` 的 `lazyPage`），assistant-ui、react-markdown 與 remark-gfm 都在其中：524.8 kB，gzip 後 158.8 kB，超過 Vite 的 500 kB 提醒。這個 chunk 裡沒有 `innerHTML`（`CHARACTER_REFERENCE_ALIAS` 生效）；建置結果裡另有三個 chunk 含 `innerHTML` 字樣，都不是對話頁的 chunk（端對端在每一頁都沒有 Trusted Types 的回報）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 選單與三個分頁，分頁讀到最後一頁 | 通過：`ConversationsPage.test.tsx`、`AppHeader.test.tsx`；端對端的分頁切換 |
| 搜尋涵蓋還沒載入的頁 | 通過：`ConversationsPage.test.tsx` |
| 改名、封存、取消封存（列表與對話頁）；刪除後在「已刪除」顯示還原期限；還原後回到「使用中」 | 通過：`ConversationsPage.test.tsx`、`ConversationPage.test.tsx`；端對端（上表，真的 API） |
| 首頁顯示最近 5 個、點進對話頁、沒有對話時不顯示 | 通過：`RecentConversations.test.tsx`；端對端的首頁 |
| 外部圖片、`data:` 圖片、外部連結與原始 HTML：沒有圖片元素、沒有對外請求、外部網址是文字、同源路徑可點 | 通過：`safe-markdown.test.tsx`；端對端（上一節） |
| 複製與匯出等於 API 的原文 | 通過：`ConversationPage.test.tsx`；端對端（真的剪貼簿與下載） |
| 回到對話 | 通過：`AppHeader.test.tsx`；端對端（回答的連結到設定頁再回來） |
| 真的 API（容器裡的 Postgres）跑一次，兩種頁面都做 CSP 與 Trusted Types 的瀏覽器檢查 | 通過：本報告 |

沒有涵蓋：Chromium 以外的瀏覽器、真的手機觸控（以 390 px 的視窗代替）、zh-TW 介面的端對端（單元測試涵蓋兩種語言的字串）、經真正入口（Istio）的路徑正規化對百分比編碼路徑的處理。
