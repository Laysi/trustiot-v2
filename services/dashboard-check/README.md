# dashboard-check

儀表板的發布前檢查（Laysi/trustiot-v2#150）：assistant 把 AI 寫的頁面與資料集的結果交給它，它在全新的無頭 Chromium（Debian 的 `chromium-headless-shell`）執行頁面，從頁面碰不到的 isolated world 比對每個標記的值與資料，回傳判定。決定與理由見 [AI 助理與 AI 閘道](../../docs/architecture/ai-assistant.md)「儀表板」第 4 點。

- API 的型別、上限與判定格式：`crates/contract/src/api/dashboard/check.rs`。
- 沙箱檔案的路徑與 response header：`crates/contract/src/api/dashboard/sandbox.rs`。
- runtime、比對程式、檢查規則與流程的契約：[frontend/sandbox](../../frontend/sandbox/README.md)。
- 映像、chart 與 nightly 的隔離證明隨 Laysi/trustiot-v2#150 的其餘部分加入；下面「威脅模型」第 1、2 點是部署必須做到的事。

| 檔案 | 內容 |
| --- | --- |
| `src/api.rs` | 端點：一次一個檢查、等候佇列、請求的大小與格式檢查、日誌 |
| `src/check.rs` | 一次檢查的流程與判定的合併 |
| `src/observer.rs` | 瀏覽器回報的事件：frame、執行環境、host 頁的回報、例外、導向，以及請求攔截 |
| `src/cdp.rs` | DevTools Protocol 的 pipe 傳輸（NUL 結尾的 JSON，fd 3 與 4） |
| `src/browser.rs` | 啟動瀏覽器、它的旗標與環境，結束時整個 process group 一起殺掉 |
| `src/cleanup.rs` | 檢查之間的清理，以及保護服務自己的記憶體 |
| `src/site.rs` | 啟動時讀進來的沙箱檔案與它們的 header |
| `src/config.rs` | 設定 |

## 威脅模型

頁面的作者是模型，而模型可以被不可信的文字引導，所以頁面的 script 一律當成攻擊者的程式；Chromium 又沒有自己的沙箱（見「瀏覽器旗標」的 `--no-sandbox`），renderer 被攻破就等於攻擊者以服務的使用者（UID 65532）在 pod 裡執行任意程式。這個服務的設計目標是把損害限制在「這一次檢查自己的資料，而且出不了 pod」：

1. **出不去**：部署要讓 pod 沒有任何 egress（DNS 也沒有，NetworkPolicy `egress: []`）、沒有 ServiceAccount token、沒有 Secret，環境變數裡沒有任何憑證；服務自己也不送任何東西出去：沒有 OpenTelemetry 匯出（`telemetry.otlp_endpoint` 有設定就拒絕啟動），沒有 Sentry，沒有資料庫，日誌只寫 stdout。瀏覽器的旗標再讓它自己不連任何地方（名稱一律解析失敗、所有請求都走一個解析不到的 proxy），所有請求都先經過攔截，只回映像裡的檔案。
2. **只有 assistant 進得來**：部署要以 mesh 的授權只讓 assistant 呼叫 API 埠；請求不帶使用者身分，服務也不驗證身分。
3. **檢查之間不留東西**：每次檢查用新的瀏覽器行程與新的目錄，結束時（不論正常、失敗、逾時或請求被中斷）整個 process group 用 SIGKILL 殺掉。服務是容器的 PID 1 時，還會殺掉並回收服務使用者的其他所有行程（離開 process group 的行程也跑不掉），清空工作目錄、`/dev/shm`、`/dev/mqueue`，清空 `/dev/termination-log`，刪掉所有 System V 的共享記憶體、訊息佇列與 semaphore。pod 的 volume 與 IPC namespace 在容器重啟後還在，所以啟動時也做一次。這一點防的是跨租戶：一個頁面若能留下行程或檔案，就能讀到之後別的租戶的檢查資料，再從自己下一次檢查的判定（例如 script 錯誤訊息）帶出去。
4. **服務本身碰不到**：服務啟動時設 `PR_SET_DUMPABLE` 為 0，同一個使用者的行程（瀏覽器）不能 ptrace 它，也讀不到它的 `/proc/<pid>/mem`、`fd`、`environ`。瀏覽器只拿到 fd 0–2（`/dev/null`）與 3、4 兩條 pipe，環境變數只有指向它自己目錄的 `HOME`、`TMPDIR`、`XDG_CONFIG_HOME`、`XDG_CACHE_HOME`；它的 OOM 分數調到 1000，記憶體不夠時核心先殺瀏覽器。
5. **比對不被頁面騙**：比對程式在沙箱 frame 的 isolated world 執行，頁面改寫 DOM 原型或全域物件影響不到它；畫完的條件也由服務在 isolated world 自己確認，不相信 runtime 的 `rendered`。比對的輸入（預期的值）只送進 isolated world，頁面看不到。
6. **判定只帶得出有限的東西**：判定裡來自頁面的只有 script 錯誤訊息（第一行、移除控制與不可見字元、最多 200 字元）與字面數字（只有數字與分隔符號），其餘都來自請求自己的資料；服務回傳前以 `CheckVerdict::validate` 確認，assistant 收到後再確認一次。

剩下的風險：

- 頁面的程式可以連到同一個 pod 裡服務自己的 API（loopback），送自己的檢查請求。它只拿得到自己送的頁面的判定，最多佔滿等候佇列，讓這段時間的其他檢查收到 503（每次最多 15 秒）。
- 頁面的程式可以對 PID 1 送 SIGTERM，讓服務關閉並重啟。這只影響可用性。
- 清理失敗時服務會結束（見「清理失敗」）；刻意做出刪不掉的檔案（例如超過路徑長度上限的深層目錄）會讓它重啟後拒絕啟動，直到刪掉 pod 換新的 volume。這同樣只影響可用性，不會讓資料留到下一次檢查。
- 使用者瀏覽器裡沙箱的外送缺口不屬於這個服務，見 Laysi/trustiot-v2#699。

## 流程

`POST /internal/dashboard-check/v1/check`，本文是 `CheckRequest`（`frontend/sandbox/README.md`「檢查服務的流程」的實作）：

1. 取得位置：一個在跑、最多 `queue.max_waiting` 個在等，再多就立刻回 503。
2. 讀本文（上限 `MAX_CHECK_REQUEST_BYTES`，17 MiB）、解析成 `CheckRequest`（內部 API，不認得的欄位忽略）、`CheckRequest::validate`；不符回 400。runtime 版本不是映像裡有的，解析就失敗，也回 400。這些都在啟動瀏覽器之前。
3. 等輪到自己，最多 `queue.max_wait`，逾時回 503。
4. 啟動瀏覽器，接管它開的 `about:blank` 頁，開啟 `Fetch` 攔截（所有請求）、`Page`、`Runtime`、自動附加子 target（`waitForDebuggerOnStart`，附加上來的一律不放行），加上 binding `tqCheckReport`，導向 `https://check.invalid/sandbox/check.html`。
5. 等 host 頁載入後呼叫 `tqCheckHost.start(request)`。host 頁建立沙箱 iframe、送 `init` 與 `data`，再以 `tqCheckReport(JSON.stringify(report))` 回報（見下面「host 頁的介面」）。
6. 等 `ready`（5 秒內；host 頁收到 `ready` 就送出資料），在沙箱 frame 建立 isolated world 並開始記錄 DOM 變動；再等 `rendered` 或 `error`，最晚到 `ready` 後 5 秒（`RENDER_TIME_LIMIT`）。收到 `rendered` 後在 isolated world 確認 200 ms（`QUIET_PERIOD`）沒有變動，否則判 `timeout`（`phase: render`）；時限到了還沒有 `rendered`：有 `returned`（`render` 回傳了，頁面卻一直在變）判 `timeout`（`phase: render`），沒有（`render` 沒有回傳，含忙到不回應）判 `not_rendered`。`returned` 與 `rendered` 頁面偽造得出來，只用來分類失敗；畫完一律以 isolated world 的確認為準。
7. 在 isolated world 載入該版本的 `compare.js`，以請求去掉 `page` 的內容（`runtime`、`spec`、`datasets`、`locale`、`tz`）呼叫 `tqCompare`；判定以 Rust 型別解析，含不認得的問題類別就當成服務的失敗。之後以 `DOMSnapshot.captureSnapshot` 找頁面的 shadow root（open 或 closed）。
8. 合併問題：先是瀏覽器看到的 `navigated`、`requested`、`script_error`（最多 5 個），再來是流程與比對的問題，去掉重複，最多 20 個；沒有任何問題才是通過。以 `CheckVerdict::validate` 確認後回 200。
9. 不論結果如何都關掉瀏覽器並清理（見「威脅模型」第 3 點），然後才放出位置給下一個檢查。

整個檢查（從啟動瀏覽器到判定）超過 15 秒（`CHECK_TIME_LIMIT`）就停下，判 `timeout`（`phase: check`），加上之前看到的問題。

請求攔截的規則：

| 請求 | 處理 |
| --- | --- |
| 主 frame 的第一個文件請求 `/sandbox/check.html`、第一個 `/sandbox/check.js` | 回檔案，header 用 `CHECK_HOST_HEADERS` |
| 沙箱 frame（主 frame 的第一個子 frame）的第一個文件請求，而且是請求版本的 `sandbox.html` | 回檔案，header 用 `SANDBOX_DOCUMENT_HEADERS` |
| 沙箱 frame 與它的子 frame 對請求版本的 `runtime.js`、`d3.js`、`sandbox.css`、`compare.js` 的請求 | 回檔案，header 用 `SANDBOX_ASSET_HEADERS` |
| 其他文件請求（沙箱 frame 的第二個文件、其他網址） | 失敗，判 `navigated` |
| 頁面其他的請求（其他路徑、其他版本、其他來源、非 GET、帶 query） | 失敗，判 `requested`；判定不放網址 |
| host 頁其他的請求 | 失敗，當成服務的失敗（500） |

每個檔案另加 `Content-Type` 與 `Cache-Control: no-store`。頁面的請求超過 1,000 個之後不再回應（已經判 `requested`）。

導向的判定：沙箱 frame 除了第一次載入之外的任何導向請求、導向、同文件導向（`navigatedWithinDocument`），沙箱裡任何子 frame 的導向，主 frame 在 host 頁之後的導向，以及新的 page target（開出的視窗），都是 `navigated`；看到就停下檢查。沙箱沒有 `allow-popups` 與 `allow-forms`，`window.open()` 與送出表單什麼都不會發生，所以不算失敗；Chromium 對每次 `window.open()` 都送 `Page.windowOpen`（包括被沙箱擋下的），服務不以它判定。附加上來的其他子 target（worker、另一個行程的 frame）判 `requested`。

例外：沙箱 frame 與它的子 frame 的 `Runtime.exceptionThrown`（含未處理的 rejection；之後被 `exceptionRevoked` 撤銷的不算）判 `script_error`；runtime 自己接住的錯誤由 host 頁的 `error` 回報。host 頁自己的例外、或 host 頁送了不合格的回報，是服務的失敗（500）。

### host 頁的介面

`check.html` 與 `check.js` 由 `frontend/sandbox` 建置（Laysi/trustiot-v2#150 的 runtime）。服務呼叫與接收的介面：

- `globalThis.tqCheckHost.start(request)`：`request` 是整個 `CheckRequest`（含 `page`），只呼叫一次，不回傳值。host 頁以 `<iframe sandbox="allow-scripts" src="/sandbox/v<runtime>/sandbox.html">` 建立沙箱，收到 `ready` 後送 `init`（`mode: "check"`）與 `generation` 為 1 的 `data`（每個結果都是 `{status: "ok", result}`）。
- 回報：`globalThis.tqCheckReport(JSON.stringify(report))`，`report` 是下面其中之一，每一種最多一次，最多 4 KiB：`{"type": "ready"}`（收到沙箱的 `ready` 之後）、`{"type": "returned"}`（收到 `generation` 1 的 `returned`，`render` 回傳）、`{"type": "rendered"}`（收到 `generation` 1 的 `rendered`）、`{"type": "error", "message": "…"}`（收到 `error`，`message` 最多 200 字元，可省略）。其他欄位忽略。`returned`、`rendered` 在 `ready` 之前，或同一種回報第二次，是 host 頁的失敗（500）。binding 在沙箱的 world 也看得到，服務只採信 host 頁主 world 的呼叫。

## 瀏覽器旗標

| 旗標 | 原因 |
| --- | --- |
| `--no-sandbox` | Chromium 自己的沙箱需要 user namespace 或 setuid 輔助程式，RuntimeDefault 的 seccomp 擋掉它們（沒有這個旗標時 Chromium 以 `No usable sandbox!` 拒絕啟動）。隔離改由 pod 負責（「威脅模型」） |
| `--remote-debugging-pipe` | DevTools 只走 fd 3 與 4 的 pipe，不開任何 TCP 埠，pod 裡沒有別的東西連得上 |
| `--no-zygote` | renderer 由瀏覽器行程直接啟動，所有行程都留在瀏覽器的 process group 裡，結束時一次殺光；也少一層行程（只能與 `--no-sandbox` 一起用） |
| `--disable-gpu`、`--in-process-gpu` | 沒有 GPU；軟體繪製放在瀏覽器行程裡，不另開 GPU 行程（省記憶體與行程） |
| `--disable-dev-shm-usage` | 共享記憶體檔放在 `TMPDIR`（這次檢查的目錄），不放 `/dev/shm`（容器預設只有 64 MiB） |
| `--host-resolver-rules=MAP * ~NOTFOUND` | 任何名稱在 Chromium 裡都解析失敗，不會送出 DNS 查詢 |
| `--proxy-server=http://proxy.invalid:9`、`--proxy-bypass-list=<-loopback>` | 所有連線（含 IP 位址與 loopback）都要走一個解析不到的 proxy，就算攔截漏了也連不出去 |
| `--force-webrtc-ip-handling-policy`、`--webrtc-ip-handling-policy=disable_non_proxied_udp` | WebRTC 只能走上面的 proxy，不直接送 UDP |
| `--dns-prefetch-disable` | 不預先解析名稱 |
| `--disable-background-networking`、`--disable-component-update`、`--disable-domain-reliability`、`--disable-client-side-phishing-detection`、`--no-pings` | 關掉 Chromium 自己的背景連線：更新、元件下載、錯誤回報、Safe Browsing、超連結稽核 |
| `--disable-breakpad` | 不產生、不送出 crash 報告 |
| `--disable-extensions`、`--no-first-run` | 沒有擴充功能，不做首次執行的工作 |
| `--deny-permission-prompts` | 任何權限請求一律拒絕 |
| `--block-new-web-contents` | `window.open` 與彈出視窗一律失敗（沙箱本來就不允許） |
| `--mute-audio` | 不出聲 |
| `--disable-background-timer-throttling`、`--disable-renderer-backgrounding` | 無頭頁面的計時器與 animation frame 照常執行，「畫完」的條件量到的是頁面，不是節流 |
| `--window-size=1280,1024` | 固定的版面大小，幾何比對的結果不隨環境改變 |
| `--lang=en-US` | 固定的瀏覽器語言；頁面的格式化依請求的 `locale`，由 runtime 決定 |
| `--user-data-dir=<這次檢查的目錄>/profile` | 每次全新的 profile，結束後刪掉 |
| 最後的 `about:blank` | 啟動時開的頁面，檢查接管它 |

完整清單在 `src/browser.rs` 的 `FLAGS`，改旗標要同時改這張表。

## 上限

| 上限 | 值 | 來源 |
| --- | --- | --- |
| 同時執行的檢查 | 1 | 決定 |
| 等候的檢查 | `queue.max_waiting`，預設 2，最多 8 | 設定 |
| 等候時間 | `queue.max_wait`，預設 10 秒，最多 60 秒 | 設定 |
| 請求本文 | 17 MiB | `MAX_CHECK_REQUEST_BYTES` |
| 整個檢查 | 15 秒 | `CHECK_TIME_LIMIT` |
| 送出資料到畫完 | 5 秒；靜止 200 ms | `RENDER_TIME_LIMIT`、`QUIET_PERIOD` |
| 判定的問題 | 20 個，其中 script 錯誤最多 5 個 | `MAX_PROBLEMS` |
| 從瀏覽器讀到的一則訊息 | 32 MiB；超過就判 `too_large` | `cdp::MAX_MESSAGE_BYTES` |
| 映像裡的一個沙箱檔案 | 8 MiB | `site::MAX_FILE_BYTES` |
| 清理後殘留行程的等待 | 3 秒 | `cleanup::PROCESS_DEADLINE` |

`http.request_timeout`（service-kit，預設 30 秒）要涵蓋最長的請求：`queue.max_wait` 加 15 秒加清理的 5 秒，不夠就拒絕啟動。assistant 呼叫它的時間上限也要涵蓋這個長度。

## 回應

| 情況 | 狀態 | 本文 |
| --- | --- | --- |
| 檢查完成 | 200 | `CheckVerdict` |
| 佇列已滿，或等候超過 `queue.max_wait` | 503 | `unavailable` |
| 本文不是 `CheckRequest`、超過大小、`validate` 不通過、runtime 版本不存在 | 400 | `invalid_request` |
| 瀏覽器無法啟動、host 頁或比對程式失敗、判定違反契約 | 500 | `internal` |

錯誤本文是 M1 的 `ErrorBody`，不帶任何細節。所有回應帶 `cache-control: no-store`。

## 清理失敗

清理（「威脅模型」第 3 點）留下任何東西時，服務記一行錯誤後以結束碼 70 結束：容器重啟時整個 PID namespace 消失，啟動時再清一次 volume 與 IPC，清不乾淨就拒絕啟動。持續重啟或啟動失敗時，刪掉 pod 換一個新的 volume。

## 日誌

每次檢查一行 `check finished`：判定（`passed`、`failed`）、問題的類別（逗號分隔的固定名稱）與各階段的毫秒數（`launch_ms`、`ready_ms`、`rendered_ms`、`compared_ms`、`total_ms`，沒走到的是 -1）。拒絕的請求記 `outcome`（`busy`、`invalid_request`）與固定的原因。頁面、資料、參照、值、script 錯誤訊息一律不寫進日誌，瀏覽器的 stdout 與 stderr 直接丟掉。啟動時記一行 `dashboard check ready`，帶瀏覽器的版本與是否做容器範圍的清理。

## 設定

前綴 `TRUSTIOT__`（service-kit 的規則）：

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `TRUSTIOT__CHROMIUM` | `/usr/lib/chromium/chromium-headless-shell` | 瀏覽器的執行檔本身（不是 `/usr/bin` 下的 shell 包裝） |
| `TRUSTIOT__SITE_DIR` | `/srv/www` | 前端建置的目錄，裡面有 `sandbox/` |
| `TRUSTIOT__WORK_DIR` | `/tmp` | 瀏覽器唯一能寫的目錄；chart 掛記憶體的 `emptyDir` |
| `TRUSTIOT__QUEUE__MAX_WAITING` | `2` | 見「上限」 |
| `TRUSTIOT__QUEUE__MAX_WAIT` | `10s` | 見「上限」 |

service-kit 的 `http.*`、`shutdown.*`、`telemetry.log_filter` 照常；`telemetry.otlp_endpoint` 不可設定。

## 測試

- `cargo test -p dashboard-check`：設定、沙箱檔案與 header、CDP 的 pipe 傳輸（拆開的訊息、過大的訊息、關閉）、請求攔截與導向的判定、host 頁回報只採信主 world、例外的歸屬、判定的合併與訊息的清理、端點（一次一個、佇列、逾時、請求中斷取消檢查、錯誤不帶細節）、瀏覽器行程（以假的瀏覽器確認旗標、環境、pipe 與整個 process group 被殺掉）、清理（深層與鎖住的目錄、不跟隨 symbolic link）。不需要 Chromium。
