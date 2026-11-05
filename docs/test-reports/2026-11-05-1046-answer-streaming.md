# 回答的串流與續播的驗證（2026-11-05）

對應 issue：Laysi/trustiot-v2#1046（對話的逐字串流與斷線續播）。後端是 Laysi/trustiot-v2#147（回答的串流，2026-11-05 併入 main `8b90f33c`），前端的提問是 Laysi/trustiot-v2#1044。程式的說明在 `frontend/README.md` 的「回答的串流」，畫面規格在 `docs/design/chat-dashboards/design.md` 的「輸入框與提問」。

## 環境

- 開發者工作站（arm64）。程式是 `feat/1046-answer-streaming` 分支，基底是 main `337b9294`；Bun、Rust 與其他工具是 `.prototools` 釘選的版本。
- 容器引擎（linux/arm64）。這個容器引擎沒有 host network，所以瀏覽器容器不用 `--network host`：`e2e.mjs` 在容器裡開一個 TCP 轉送，`127.0.0.1:28911` 轉到 `host.docker.internal:28911`，頁面的來源仍是 `http://127.0.0.1:28911`（與 Laysi/trustiot-v2#1044 的做法相同的來源，安全情境不變）。
- 瀏覽器：`frontend/e2e/image.ts` 釘選的 Playwright 映像（`mcr.microsoft.com/playwright:v1.64.1-noble`，以 digest 釘選，linux/arm64）裡的 Chromium，丟掉所有 capability、`no-new-privileges`；語言 en-US、時區 UTC；桌機 1280 × 900 淺色主題，最後一段是手機 390 × 844 深色主題。
- API 是真的：被忽略的測試 `services/assistant/tests/spa_e2e_1046.rs`（不進 repo，副本是 `data/.../scripts/spa_e2e_1046.rs.txt`）在容器裡啟動 Postgres（TLS，migration Job 套用產品的 schema），以產品的方式啟動 assistant 執行檔兩次，用同一個資料庫：一個 `TRUSTIOT__STREAMS__ANSWER_TOOL=true`，一個 `false`；兩者都以預設的 heartbeat 間隔（20 秒）。**模型是替身，沒有任何真的模型呼叫**：替身 AI 閘道對有回答工具的對話以回答工具回答，工具的輸入以 server-sent events 慢慢送（每 24 位元組一塊，每寫完一段停一下：問題含 `[slow]` 時 1.2 秒，含 `[long]` 時 10 秒），對沒有回答工具的對話在 6 秒後回文字回答。回答有粗體、表格、清單、中文與一個 BMP 以外的字元（UTF-16 兩個 code unit），每段有自己的標記（`P01`…，中文的段落裡出現兩次）。
- 這台工作站上，`cargo test -p assistant` 的 dev-dependency（`tools/lab-dataset` 依賴 `edge/agent`）編不過：`edge/agent` 把 `u32` 的模式傳給 `libc::mode_t`，而這台工作站的 `mode_t` 不是 `u32`（與本 issue 無關）。為了編出測試，暫時改了那一行（`data/.../scripts/agent-mode-t-workaround.patch.txt`），跑完就還原，沒有進 repo；它不影響 assistant 或前端。
- 預覽頁（不進 repo，副本在 `data/.../scripts/`）：以 `frontend/vite.config.ts` 的設定建置 SPA 真正的 `App` 與 `ROUTES`，對話的 API 用真的 `conversationApi(createApiClient())`（回答的串流經真的 `openEventStream` 與 `fetch`）；身分服務以測試用的記憶體版本代替（工作階段不是這次要驗的）。一個 Bun 伺服器（`e2e-serve.ts`）提供建置結果，每個回應帶 `services/web/src/headers.rs` 的 `CSP`（強制）與 `CSP_REPORT_ONLY`（Trusted Types，只回報），並把 `/api/assistant/` 轉給 API、補上對話擁有者的內部身分 token（入口做的事）。
- 停住的代理：同一個 Bun 伺服器也是替身代理。`stall-next` 存在時，下一個回答串流轉送到兩個 `text` 事件之後，就不再轉送任何東西（連 heartbeat 也不轉），回應維持開著、狀態 200，就像停止轉送的代理；`idleTimeout: 0`，否則 Bun 在 10 秒沒有資料時自己關掉連線。它記下每個串流的 `Last-Event-ID`、轉送的 `text` 的位置、停住與關閉的時間，以及是誰關的。

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

# 端對端：把 data/.../scripts/ 的 spa_e2e_1046.rs.txt 放成 services/assistant/tests/spa_e2e_1046.rs，
# 其餘檔案（去掉 .txt）放進 frontend/.preview-1046/；`mode_t` 不是 `u32` 的平台另外套用 agent-mode-t-workaround.patch.txt。
# 然後在 repo 根目錄（腳本以 docker 指令啟動容器）
bash frontend/.preview-1046/run.sh
```

## 結果

原始資料：[e2e-result.json](data/2026-11-05-1046-answer-streaming/e2e-result.json)（每一步看到的內容、違規、主控台、其他來源的請求）、[streams.json](data/2026-11-05-1046-answer-streaming/streams.json)（代理記下的每個回答串流）、[final.json](data/2026-11-05-1046-answer-streaming/final.json)（瀏覽器流程後 API 的對話與替身閘道寫出的回答）、[proxied-api-requests.txt](data/2026-11-05-1046-answer-streaming/proxied-api-requests.txt)。截圖不附（repo 不放圖檔）。下表的長度是畫面上回答的文字長度（Markdown 畫出來之後），時間是毫秒。

### 端對端的流程

| 步驟 | 畫面 | 串流與 API |
| --- | --- | --- |
| 1. 首頁提問（`[slow]`，回答工具開） | 打開對話頁；第一段在 371 ms 出現，之後 9 個不同的長度 73、101、178、263、348、433、518、603、688，每一個都以前一個開頭；全都在 `latest_run` 還是 `running` 時看到；12.4 s 回答完成，串列裡的回答 773 字 | 一個串流，從開頭開啟，`end` 是 `answered`；存下的回答（832 個 code unit）與替身閘道寫的完全相同 |
| 2. 下一個問題寫到第二段時重新整理 | 重新整理前顯示 101；重新整理後第一個畫面就是 101（從目前的位置接著），之後 178…688 繼續長，回答完成 773 | 重新整理後的串流沒有 `Last-Event-ID`、第一個 `text` 從位置 0 開始（已寫好的部分），`end` 是 `answered`；每個標記的次數與存下的回答相同（沒有重複） |
| 3. 回答工具關閉的副本（新對話） | 19 次取樣是「Looking up readings…」、提問 `running`；提問中沒有任何回答文字；6.1 s 後回答一次出現 | 串流（另一個副本）在答完之後才送出整個回答（一個 `text`，0 到 832）與 `end`；替身閘道收到的請求沒有回答工具，回答是文字 |
| 4. 代理在兩段之後停止轉送（`[long]`，每段 10 秒） | 停住前顯示 73、101；停住的 75 秒內一直是 101；重新連線後一次補上停住期間寫好的段落（688），再 773、858、943、1028，回答完成 | 串流 5 轉送到位置 148（`lastDataAt` 44,755），瀏覽器在 119,788 關掉它（沒有資料 75,033 ms 之後），串流 6 在 120,803 開啟，`Last-Event-ID: 148`，第一個 `text` 從 148 開始，一直到 1176 與 `end: answered`；沒有資料到重新連線共 76,048 ms（75 秒的 heartbeat 逾時加上第一次的退避） |
| 5. 手機（390 px、深色）在第 1 段的對話裡再問一題 | 41 次取樣中 39 次有回答文字；寫的時候與寫完之後水平溢位都是 −15（沒有溢位；表格在自己的捲動框裡）；`data-theme` 是 `dark` | 存下的回答與替身閘道寫的相同 |

每一步都比對：畫面最後的回答以串流最後顯示的文字開頭、存下的回答與替身閘道寫出的回答逐字相同、每段的標記在畫面上出現的次數與存下的回答相同。

### CSP、Trusted Types 與主控台

五個步驟（含重新整理、停住的串流與手機深色）：`securitypolicyviolation`（強制的與 Trusted Types 的只回報）0 件，頁面例外 0 個，送往其他來源的請求 0 個，主控台的錯誤與警告 0 則。

### 端對端找到並修正的問題

第一次完整的執行（步驟 4）發現：服務在提問進行中寫出的部分回答是去掉結尾空白的段落（下一段的空行跟著下一段來），而頁面原本只顯示到最後一個空行為止，所以最新寫好的一段要等下一段來才出現；每段 10 秒時，停住的 75 秒裡表格一直沒出現。改成：最後收到的一段比服務切的大小（4,096 個 code unit）短時就是服務當時有的全部，整段顯示；只有剛好是切的大小時才只顯示到最後一個空行之前（`shownAnswer`）。單元測試與頁面測試改用與服務相同的段落形式，頁面測試在第三段寫出之前就要看到第二段的表格。上表是修正之後的執行；步驟 4 另外檢查停住期間看得到停住前送出的第二段。

### 前端與其他檢查

在開發工作站上，樹含本分支的全部變更（日期 2026-11-05），結果見下一節「結論」上方的表；單元測試的涵蓋：

- `src/api/answer-stream.test.ts`（手動時鐘，經真的 `openEventStream`）：分段顯示與 `answered` 的 `end`；heartbeat 逾時（75 秒少 1 ms 不重連、到 75 秒才重連）後帶 `Last-Event-ID` 重新連線，重疊的段落不重複；狀態 200 的串流在段落中間被切斷後接著收；開始位置超過已有內容（缺口）時立刻從自己的位置重開；解析不了的事件（警告不含值）重開、最多 `MAX_RESYNCS` 次；停止、失敗、中斷時丟掉已收到的段落、不再重連；重新開啟頁面從開頭收到已寫好的部分再接著收；400 從開頭重來一次、再次 400 與 404 停止；503、429 重試；解析不了的 `end`；`close()`；服務切成 4,096 的長文字只顯示到前一段。
- `src/api/sse.test.ts`：`lastEventId` 的第一次連線、`stopOn` 停在 `refused` 並釋放本文、不在 `stopOn` 的狀態照樣重試、處理程式關閉串流之後同一塊資料其餘的事件不再交給它。
- `src/pages/conversations/ConversationStream.test.tsx`（`FakeQuestionServer` 的串流）：問題下方一段一段出現、`aria-busy`、Markdown 規則，答完之後只在串列出現一次；回答工具關閉時一直是等待的那一行；圖片與外部連結是文字；狀態 200 被切斷後帶位置重連、沒有重複；開啟頁面時已寫好的部分；寫到一半時停止、失敗、中斷；另一個分頁較新的問題取代時舊的串流關閉。`ConversationTurn.test.tsx`（Laysi/trustiot-v2#1044 的測試）全部照舊通過。

## 結論

| 指令 | 結果 |
| --- | --- |
| `api:check`、`typecheck`、`lint`（Biome）、`build`、`check:dist` | 通過 |
| `test` | 見下方；本分支新增與改動的測試全部通過 |
| `cargo test --locked -p repo-check` | 通過 |

`bun run --cwd frontend test` 在這台工作站上有 2 個失敗，都與本分支無關：`src/dashboard/format.test.ts` 與 `DashboardComponent.test.tsx` 期待 `Nov 1, 2026, 1:06:07.250 AM`，這台工作站的 ICU 寫成 `Nov 1, 2026 at …`；在基底 `337b9294` 的乾淨副本上同樣失敗。

| 驗收條件 | 結果 |
| --- | --- |
| 端對端：提問、看到回答逐步出現、重新整理、看到它續播 | 通過：步驟 1（9 個逐步變長的畫面，全在提問進行中）與步驟 2（重新整理後從 101 接著長到完成，沒有重複）；真的 assistant 與 Postgres，模型是替身 |
| 連線被切斷但狀態仍是 200 的測試：心跳逾時後續播，文字沒有重複或缺少 | 通過：單元測試（手動時鐘）與端對端步驟 4（停止轉送的代理，76 秒後帶 `Last-Event-ID: 148` 重連，下一段從 148 開始，存下的回答與畫面一致） |
| `frontend` 的 `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist` 與 CI 通過 | 本機通過（`test` 除了上述兩個與本分支無關、基底也失敗的日期格式測試）；CI 在 PR 上 |

沒有涵蓋：真的模型與 AI 閘道（`eager_input_streaming` 經過供應商與 Cloudflare 的驗證屬於 Laysi/trustiot-v2#147 之後的部署驗證）、真的 Cloudflare（以停止轉送的代理代替）、多個副本之間的續播（服務端由 Laysi/trustiot-v2#147 的測試涵蓋；這裡兩個副本共用資料庫，但一段對話的串流都經同一個副本）、Chromium 以外的瀏覽器、zh-TW 介面的端對端（單元測試涵蓋兩種語言的字串）。
