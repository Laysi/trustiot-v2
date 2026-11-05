# 分享對話給整個租戶的驗證（2026-11-04）

對應 issue：Laysi/trustiot-v2#1048（分享對話給整個租戶：切換、「分享給我的」、唯讀檢視與複製）。後端是 Laysi/trustiot-v2#650（PR Laysi/trustiot-v2#1066，已合併）。畫面的規格在 `docs/design/chat-dashboards/design.md` 的「分享給整個租戶」一節，程式的說明在 `frontend/README.md` 的「分享的對話」。

## 環境

- 建置主機（arm64 Linux）。程式是 `feat/1048-share-conversation` 分支，合併了 main `9a7dde53`（含 Laysi/trustiot-v2#1045 的對話儀表板草稿面板與 Laysi/trustiot-v2#1065 的 UI 預覽）；Bun、Rust 與其他工具是 `.prototools` 釘選的版本。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像（`mcr.microsoft.com/playwright:v1.64.1-noble`，以 digest 釘選，linux/arm64）裡的 Chromium，`--network host`、丟掉所有 capability；語言 en-US、時區 UTC；桌機 1280 × 900 的淺色主題，最後一段是手機 390 × 844 的深色主題。
- API 是真的：`scripts/spa_e2e_1048.rs.txt`（被忽略的測試，不進 repo，副本在原始資料的 `scripts/`）在容器裡啟動 Postgres（TLS，migration Job 套用產品的 schema），以 chart 的方式啟動 assistant 執行檔。同一個租戶有兩位成員：A 與 B，都是 editor（B 是 editor 才看得出「發布」被服務端以 `dashboard.owner_required` 拒絕，而不是角色的 `permission_denied`）。A 有兩個對話，回答由真正的 agent 對著替代的模型產生後存進資料庫：「Floor 3 energy」的回答是惡意的 Markdown（外部圖片、外部連結、`javascript:`、`//` 網址、表格與一個 SPA 連結），瀏覽器流程要分享它；「Private notes」不分享。「Floor 3 energy」另有一份儀表板草稿（以服務自己的 `create_draft` 寫進資料庫，一個「Daily energy」元件）。
- 預覽頁（不進 repo，副本在 `scripts/`）：以 `frontend/vite.config.ts` 的設定（含 `STYLE_SINGLETON_ALIAS` 與 `CHARACTER_REFERENCE_ALIAS`）建置 SPA 真正的 `App` 與 `ROUTES`（分享的路由與頁面就是產品的），對話與儀表板的 API 用真的 `conversationApi(createApiClient())` 與 `dashboardApi`；身分服務以測試用的記憶體版本代替（工作階段不是這次檢查的對象）。靜態檔以產品的標頭（`services/web/src/headers.rs` 的 CSP 與只回報的 Trusted Types）提供；`/api/assistant/` 經一個只聽 loopback 的代理轉給真的 API，代理依瀏覽器的 `e2e_user` cookie（`a` 或 `b`）加上那位成員的內部身分 token，就像入口閘道所做的。

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

# 端對端：把 scripts/ 的 spa_e2e_1048.rs.txt 放成 services/assistant/tests/spa_e2e_1048.rs，
# 其餘檔案（去掉 .txt）放進 frontend/.preview-1048/，然後在 repo 根目錄
bash frontend/.preview-1048/run.sh
```

`run.sh` 依序：建置預覽頁；背景執行 `E2E_DIR=<目錄> cargo test --locked -p assistant --test spa_e2e_1048 -- --ignored --nocapture`，等它寫出 API 的位址與兩位成員的 token；啟動 `e2e-serve.ts`；在 Playwright 容器裡執行 `e2e.mjs`；最後寫出 `done`，測試再經同一個 API 讀出服務端最後的狀態。

## 結果

原始資料：[e2e-result.json](data/2026-11-04-1048-share-conversation/e2e-result.json)（每一步看到的內容、違規、主控台、其他來源的請求、API 請求）、[final-state.json](data/2026-11-04-1048-share-conversation/final-state.json)（瀏覽器流程後兩位成員各自看到的列表與兩個對話）、[proxied-requests.txt](data/2026-11-04-1048-share-conversation/proxied-requests.txt)（經代理的每個 API 請求與狀態，標出是 A 還是 B）。

### 端對端的流程（`e2e-result.json`）

| 步驟 | 畫面 | API |
| --- | --- | --- |
| A 按「Share」 | 確認視窗：「Share with everyone in East demo plant?」、每位成員都會在「Shared with me」看到、回答「may contain data other members can't normally see」、「Stopping sharing or deleting the conversation doesn't take back copies that members already made.」；按下確認之前 `shared_at` 仍是 `null` | |
| A 確認 | 標題列下方的提示「Shared with every member of East demo plant. …」，按鈕變成「Shared」 | `PUT …/share` 200；`shared_at` 是服務的時間 `2026-11-04T14:50:07Z` |
| B 打開「Shared with me」 | 只有「Floor 3 energy」一列（分享者「Another member」、分享時間、最後活動），沒有列的動作；頁面上沒有「Private notes」；B 自己的對話是 0 個 | `GET …/shared-conversations`：`owner_user_id` 是 A 的使用者 ID |
| B 打開唯讀頁 | `/shared-conversations/<ID>`：標題、「Read only」標籤、說明（只帶提問與回答的文字、取消分享或刪除原本的對話都不會收回副本）；整頁的按鈕只有「Menu」、「Account」、「Copy to my conversations」與回答旁的「Copy answer」；沒有輸入框、停止、重新產生、改名、封存、刪除、分享、儲存或發布 | `GET …/shared-conversations/<ID>` 200 |
| 回答的 Markdown | `<img>` 0 個；粗體「412 kWh」與表格照常畫出；唯一的連結是 `/dashboards`（同源）；外部圖片是「[image not shown: floor plan]」，外部連結、autolink、`javascript:` 與 `//evil.example.test/x` 都是文字，後面以括號寫出網址；沒有送往其他來源的請求，Resource Timing 沒有 `leak`、`evil`、`frame` | |
| A 的草稿面板 | 對話頁右欄的草稿面板有「Save」按鈕（擁有者看到的動作） | |
| B 的草稿面板 | 唯讀頁分成兩欄，右欄是同一個草稿面板的唯讀模式：版本預覽（1 個）、說明「This draft is in a conversation another member shared. You can only look at it: it can't be saved or changed here, and its data is queried with your own permissions, so you may see less than its owner does.」；沒有「Save」、「Publish」、「Edit」、「Discard」、「Restore」；只剩「Run again」與「See the source」（以讀者自己的權限查資料與看來源） | `GET …/conversations/<ID>/dashboards` 列出 1 個草稿，`permissions` 的 `can_publish`、`can_restore`、`can_archive` 全是 `false`；B 呼叫發布得 403 `dashboard.owner_required`，之後草稿的 `saved` 仍是 `null`；取消分享後同一個列表 404 |
| B 碰關著的門 | 擁有者的 `GET /conversations/<A 的 ID>`、`POST …/questions`、沒分享的 `GET /shared-conversations/<private ID>`、不存在的 ID：都是 404；沒分享的與不存在的回應本文逐位元組相同 | 404 × 4 |
| B 複製 | 打開新對話 `/conversations/<新 ID>`，標題「Floor 3 energy」，有擁有者的動作（分享、改名、選單）；新對話屬於 B、`shared_at` 與 `archived_at` 都是 `null`、`title_source` 是 `user`，兩則訊息與來源逐字相同；B 的「使用中」列出這一列 | `POST …/copy` 201 |
| A 按「Shared」 | 取消分享的確認：「Stop sharing this conversation?」、「Stopping sharing doesn't take back copies that members already made …」、「Deleting this conversation doesn't take them back either」 | |
| A 確認取消 | 「Stopped sharing. Copies that members already made stay.」，按鈕回到「Share」 | `DELETE …/share` 200；`shared_at` 是 `null` |
| B 之後 | 「Shared with me」是「Nothing shared with you yet」；舊的唯讀連結是「Shared conversation not found」；B 的副本仍可打開，兩則訊息都在 | `GET …/shared-conversations/<ID>` 404；`GET …/conversations/<副本>` 200 |
| 深色、手機 | 「Shared with me」、唯讀頁與取消分享的確認視窗都沒有水平溢位（`scrollWidth` 不大於 `clientWidth`） | |

服務端最後的狀態（`final-state.json`）：A 重新分享過一次（為了手機的檢查），所以 A 的「使用中」有「Private notes」（`shared_at` 是 `null`）與「Floor 3 energy」（已分享）；B 的「使用中」只有自己的副本（`shared_at` 是 `null`），B 的「分享給我的」只有「Floor 3 energy」，A 的「分享給我的」是空的（自己的不在自己的清單裡）。兩位成員對兩個對話的兩種讀取：A 對自己的兩個對話走擁有者的端點 200、走唯讀端點 404；B 對 A 分享的走擁有者的端點 404、唯讀端點 200；B 對 A 沒分享的兩個端點都是 404。

### CSP、Trusted Types 與主控台

全部頁面（A 分享與草稿面板、B 的列表、唯讀頁與唯讀的草稿面板、複製後的對話、A 取消分享、取消分享後 B 的頁面、手機深色的三個畫面）：`securitypolicyviolation`（強制的與 Trusted Types 的只回報）0 件，頁面例外 0 個，送往其他來源的請求 0 個；每頁的 `<style>` 與沒有 `src` 的 `<script>` 都是 0 個。主控台有 24 則 error，全是瀏覽器對非 2xx 回應的「Failed to load resource」：17 個是 503（儀表板元件的 `dataset-runs`：檢查環境沒有接遙測服務，元件畫出自己的錯誤狀態與「Run again」，A 與 B 的頁面各重複載入多次），6 個是 404（流程刻意探測的擁有者端點、提問、沒分享的與不存在的唯讀端點，與取消分享後打開舊的唯讀連結與草稿列表），1 個是刻意的 403（B 的發布被拒絕）；沒有其他錯誤或警告。

### 前端與其他檢查

在建置主機上（合併 `9a7dde53` 後的分支）：

| 指令 | 結果 |
| --- | --- |
| `api:check`、`typecheck`、`lint`（Biome，263 個檔案）、`build`、`check:dist` | 通過 |
| `test` | 1123 個通過、0 個失敗（63 個檔案） |
| `bun test scripts` | 2024 個通過、0 個失敗 |
| `cargo test --locked -p repo-check` | 26 + 11 個通過 |

單元測試（`frontend/src/pages/conversations/sharing/*.test.tsx`，用 `src/test/fake-conversations.ts`，它依服務的規則處理分享、「分享給我的」、唯讀檢視與複製）涵蓋：分享與取消分享的確認內容與流程、取消、錯誤留在視窗裡、送出中不能關閉也不能送兩次、剛被刪除的對話；「分享給我的」的列、深連結、自己的不在清單、空狀態、搜尋（讀完其餘的頁）、錯誤與重試、載入；唯讀頁的內容與沒有任何擁有者動作、viewer 也能閱讀與複製、複製（打開新對話、標題與訊息、不分享）、複製失敗與不能送兩次、找不到（沒分享、自己的、已刪除、不存在、ID 格式不對不發請求）、讀取失敗、載入、回答裡的連結不帶來源對話；唯讀頁的草稿（viewer、editor、admin 都只能看：沒有儲存、發布、編輯、捨棄、還原；草稿列表讀取失敗時有錯誤提示與重試）；UI 預覽（`src/ui-preview/preview.test.tsx`：「Shared with me」分頁與唯讀頁在預覽裡可用，複製的對話出現在自己的列表）；以及兩位成員之間的流程（A 分享、B 看到、讀、複製，A 取消分享後 B 的清單是空的、副本仍在，刪除會取消分享、還原不會再分享）。

## 結論

- 擁有者分享後，另一位成員在「分享給我的」看到它、只能唯讀，複製後得到自己的新對話；取消分享後清單裡不再有它、副本仍在：**通過**（端對端與單元測試）。複製後「在新對話繼續提問」：**通過**（單元測試，合併 Laysi/trustiot-v2#1044 的輸入框之後）：複製打開讀者自己的新對話（有擁有者的動作與輸入框），在那裡提問、回答出現在複製裡，分享的原件沒有收到問題；唯讀頁沒有輸入框。這一項沒有跑真 API 的瀏覽器流程（端對端的檢查在合併輸入框之前做，服務端的複製後提問由 Laysi/trustiot-v2#650 的測試涵蓋）。
- 分享切換與複製的畫面文字寫明「取消分享或刪除不會收回副本」（zh-TW 與 en）：**通過**（確認視窗與唯讀頁的說明；en 的文字由測試與端對端逐字檢查，兩種語言的鍵與預留位置相同由 `i18n.test.ts` 保證）。
- 分享的讀者看得到對話的儀表板，但不能儲存或修改：**通過**。唯讀頁用 Laysi/trustiot-v2#1045 的草稿面板的唯讀模式（同一個版本預覽元件），端對端中 B 看到草稿與版本預覽、沒有任何儲存、發布、編輯、捨棄或還原的動作，服務端回的 `permissions` 全是 `false`、發布被 403 拒絕且草稿不變。元件資料以讀者自己的權限查詢（`dataset-runs` 經 Laysi/trustiot-v2#650 的規則）：本檢查沒有接遙測服務，元件只畫出錯誤狀態，所以「讀者看到的數字」由單元測試（fake）與 Laysi/trustiot-v2#650 的服務端測試涵蓋，不是本次瀏覽器檢查的證據。
- 以真的 assistant API 實測分享、唯讀檢視與複製：**通過**（本報告）。
- `frontend` 的 `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist`：**通過**（建置主機）；CI 待 PR。
