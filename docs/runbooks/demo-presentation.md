# 展示 runbook：劇本四步

展示切片（[里程碑](../roadmap/milestones.md)「展示切片」，epic Laysi/trustiot-v2#456）的展示劇本怎麼上台：展示前檢查、每一步的操作與要說的話、彩排清單，以及出錯時怎麼辦（Laysi/trustiot-v2#459）。展示環境本身（Cloudflare、Flux、換版、重設資料）在[展示環境 runbook](demo-environment.md)，本文只引用它。

本文每一步寫的畫面與行為，都追到 main 上的程式與文件（文末「對照」）；沒有實測過的地方會寫「彩排時確認」。2026-11-07 的狀態是：展示叢集釘在 chart `0.0.0-rc.15`（main `74b26f3e`），閘道器當時是關機的，所以本文的畫面描述來自程式與文件，不是從螢幕上看到的；第一次照本文走完全程是彩排（見「彩排清單」）。

主機名稱、帳號、email 與位址不寫進本文（[密鑰與權限](../architecture/secrets.md)「公開 repo 的資訊規則」，`scripts/demo/public-text.test.ts` 檢查）。指令用變數，由執行的人在自己的終端機設定：

```sh
: "${DEMO_SSH_HOST:?set DEMO_SSH_HOST to the ssh host of the demo machine}"
: "${GATEWAY_SSH_HOST:?set GATEWAY_SSH_HOST to the ssh host of the lab gateway}"
: "${DEMO_APP_HOST:?set DEMO_APP_HOST to the app host name, in lower case}"
```

## 先決定：展示用哪一版

劇本四步在 rc.15 上都有程式（登入、設備頁、對話、串流、元件模式的儀表板、儲存、儀表板頁、資料來源面板）。但展示前必須換到比 rc.15 新的版本，因為下面這一項只在 main 上修：

| 必須在展示前部署 | 為什麼 |
| --- | --- |
| Laysi/trustiot-v2#1144（儀表板的時間序列元件在資料缺口處斷線並畫斜紋，與設備頁相同） | 沒有它，元件把缺口畫成一條直線：步驟 2 的斷網與關機的缺口在儀表板上看不出來，還會畫出沒有資料的假走勢。本文假設它已部署 |

換版的做法與風險在[換 chart 版本](demo-environment.md#換-chart-版本)：migration Job 重跑、每個服務滾動一次、叢集內 Postgres 以新映像重建（2026-11-01 換到 rc.13 時中斷約 3 分鐘），istiod 的 values 要先比對。所以換版排在彩排之前，**彩排與展示用同一版**；換版之後到展示之前不再換。

rc.15 之後 main 上的其他變更對四步沒有影響（`git log 74b26f3e..origin/main`，2026-11-07 共 14 個）：

| 變更 | 對展示 |
| --- | --- |
| Laysi/trustiot-v2#1121（展示叢集打開回答工具的值） | 已經在叢集上：Flux 讀 main，assistant 的環境有 `TRUSTIOT__STREAMS__ANSWER_TOOL=true`（2026-11-07 唯讀確認）。只有這個值打開之後**新開的對話**會一段一段串流 |
| Laysi/trustiot-v2#1083（重新產生回答的 API）與 Laysi/trustiot-v2#1115（重新產生、輸入補全、問 AI 的按鈕） | 輸入補全（`@` 提及與灰字續寫）只在開發用的 UI 預覽，正式的路由沒有。「重新產生」：PR Laysi/trustiot-v2#1145（Laysi/trustiot-v2#1047，審查中）合併並進了展示叢集的版本之後，展示者自己的對話的最後一則回答有「重新產生」按鈕；在那之前只在 UI 預覽。劇本不用它，彩排用一次（見「要用真模型確認的項目」）。設備頁與回答裡的「問 AI」入口是正式畫面，rc.15 沒有，劇本不用它們 |
| Laysi/trustiot-v2#1120（平台管理員、AI 用量、警報、報表、機台、文件庫畫面） | 只有模型角色的平台頁進正式路由，其他是 UI 預覽；劇本不用 |
| 測試、CI、重構、閘道器映像（Laysi/trustiot-v2#1084、#1124、#1128、#1129、#1139、#1140、#1125、#1113、#1110、#1118） | 沒有影響 |

儀表板檢查服務（自訂頁面，Laysi/trustiot-v2#881）2026-11-07 還沒打開（沒有 `trustiot-dashboard-check` pod，assistant 沒有 `TRUSTIOT__DASHBOARD_CHECK__URL`）。沒有它時 AI 只能寫元件模式的儀表板，含自訂頁面的寫入被拒絕（`dashboard.spec.custom_page_disabled`）：劇本要的圖表、數字、表格元件都畫得出，**展示照常**。打開之後 AI 仍先用元件，元件畫不出的圖（桑基圖、散佈圖等）才寫自訂頁面，劇本的要求句不會用到它。所以打不打開都不改變劇本，只改變「要一張元件畫不出的圖」時會發生什麼：沒打開時 AI 會說這裡不提供，打開後會寫出自訂頁面。

## 展示前檢查

分兩段：展示前一天做完所有項目；當天出門前再做標 ★ 的。任何一項不通過，先處理再繼續，不在展示前一小時內換版、重設資料、重啟 k3s 或做會讓服務滾動的變更。

### 展示環境（只讀，不取 lab 鎖）

| 項目 | 指令與預期 | 不通過時 |
| --- | --- | --- |
| ★ Flux 與服務 | `bun scripts/demo/flux.ts status --ssh-host "$DEMO_SSH_HOST"`：`lab lock: free`；HelmRelease 是 `deployed`、版本是要展示的那一版，`pulled` 與 `deployed` 都是 `the same`；每個 pod 都是 `Running ready=true`，沒有不斷增加的 restarts | [展示環境 runbook](demo-environment.md)「部署」與「換 chart 版本」；lab 鎖不是 free 時，鎖的擁有者在用叢集（lab 的 e2e），展示前請對方收掉 |
| 入口與 tunnel | `bun scripts/demo/tunnel.ts status --ssh-host "$DEMO_SSH_HOST"`（約 1 到 2 分鐘）：`cloudflared-demo` 的 ready connections 大於 0，最後 `OK the three ingress addresses agree`。再 `bun scripts/demo/tunnel.ts check --ssh-host "$DEMO_SSH_HOST" --stage 2`（需要三個主機名稱的環境變數，見[檢查對外的請求](demo-environment.md#檢查對外的請求)）全部 `PASS` | 同上「架構」的「路由」；Access 或 Cloudflare 擋下的判讀表在[檢查對外的請求](demo-environment.md#檢查對外的請求) |
| ★ 記憶體 | `ssh "$DEMO_SSH_HOST" 'free -m; sudo k3s kubectl top node'`：2026-11-07 唯讀看到 available 約 2.9 GiB、節點記憶體 42%。要展示的版本比這個數字低很多時（例如打開了儀表板檢查，它 limits 512Mi），先看[與 lab e2e 共用叢集](demo-environment.md#與-lab-e2e-共用叢集) | 不在展示前加服務 |
| 儀表板檢查有沒有打開 | `ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot get pod -o name' \| grep dashboard-check`：有 pod 就是打開了。照上一節決定要不要在展示前打開；**不要在展示前一天以內才打開**（會讓 assistant 滾動，且要先做隔離實測，Laysi/trustiot-v2#881） | 沒有 pod 是目前的預期狀態 |

### 資料：閘道器在線、資料是新的

閘道器不能整天通電時，維護者每天把它關機（`sudo systemctl poweroff`，等 ACT 燈熄滅再斷電），開機後 agent 的 systemd unit 自己啟動。2026-11-06 17:02（Asia/Taipei）是開始每天關機之後的第一次實際關機。展示當天要**提早開機**，先確認資料回來，再上台。

```sh
# ★ read-only, prints no tenant row: the newest received time of the telemetry table and the
# gateway's last presence change. The admin account cannot read tenant rows (row policies), so
# this reads the table's parts and the service logs instead.
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot exec -i trustiot-clickhouse-0 -c clickhouse -- sh -c "clickhouse-client --user \"\$CLICKHOUSE_USER\" --password \"\$(cat \"\$CLICKHOUSE_PASSWORD_FILE\")\" --multiquery"' <<'EOF'
SELECT
  max(max_time) AS newest_received_utc,
  dateDiff('second', max(max_time), now('UTC')) AS seconds_ago,
  sum(rows) AS rows
FROM system.parts
WHERE database = 'trustiot' AND table = 'telemetry' AND active
FORMAT Vertical;
SELECT
  argMax(LogAttributes['state'], Timestamp) AS gateway_state,
  max(Timestamp) AS since_utc
FROM trustiot_logs.otel_logs
WHERE ServiceName = 'gateway-ops' AND Body = 'gateway presence changed'
FORMAT Vertical;
EOF
```

預期：`gateway_state` 是 `online`；`seconds_ago` 在 180 以內（每分鐘一筆，兩個資料點同時寫入）。每個資料點每分鐘一列，所以 `rows` 每分鐘加 2。2026-11-07 的結果是 `newest_received_utc` 2026-11-06 09:01:26、`seconds_ago` 34093、`rows` 19310、`gateway_state` `offline`（`shutdown`）：閘道器關機中，這個檢查就是用來抓這種情況。時間是 UTC，台北時間加 8 小時。`system.parts` 的 `max_time` 是整個資料表最新的**接收**時間（`received_at`），不是讀值時間 `ts`；兩者在連線正常時差幾秒。

閘道器本身（只讀，閘道器的 ssh 設定由維護者提供，`GATEWAY_SSH_HOST`）：

```sh
ssh "$GATEWAY_SSH_HOST" 'systemctl is-active trustiot-agent; uptime -p; test -e /run/systemd/timesync/synchronized && echo ntp-synchronized || echo ntp-NOT-synchronized'
```

預期：`active`、開機後的時間、`ntp-synchronized`。閘道器的作業系統還沒有啟用 RTC（Laysi/trustiot-v2#995），所以開機之後到 NTP 校時之前，時鐘不可信。agent 的 `clock_synced` 在行程第一次拿雲端的確認來比對之前，看 systemd-timesyncd 說不說已校正；之後改看確認裡的 `server_time` 與本機時鐘的差（±2 秒）（[edge/agent](../../edge/agent/README.md)「時鐘」）。但 ingest 不論批次是不是 `clock_synced`，都把每筆讀值以閘道器自己的時間存下：它解碼這個旗標，卻不存、也不用（`services/ingest/src/telemetry/batch.rs`），資料表的 TTL 則依 `received_at`（`services/ingest/migrations/clickhouse/0002_telemetry.sql`）。所以校時前讀到的值會永遠帶著錯的時間，這就是 NTP 檢查要排在上台之前的原因：校時之前不要上台，也不要拿開機後前幾分鐘的資料當證據。（`/run/systemd/timesync/synchronized` 是 agent README 寫的判斷方式；這條指令 2026-11-07 沒有在閘道器上執行過，因為它當時關機，彩排時確認。）

資料新鮮之後，再看 7 天的連續性：登入後設備頁選「最近 7 天」，圖上的斜紋「N 小時沒有讀數」只應該出現在閘道器關機的夜裡。2026-10-30 16:08 UTC 起到 2026-11-06 09:01 UTC 的列數與每分鐘一筆的預期相符（19,310 列，約 9,655 個時間點），這段沒有缺口；之後每個關機的夜晚都會有一段。

### 帳號與瀏覽器

| 項目 | 做法與預期 | 不通過時 |
| --- | --- | --- |
| ★ 展示用的 Google 帳號在 Access 的受邀名單 | 以展示用的瀏覽器開 `https://$DEMO_APP_HOST/`：Google 登入之後到 TrustIoT 的登入頁。沒有權限時 Access 顯示沒有權限的頁面（不會到 app）。Access 的 session 24 小時，並綁在同一個瀏覽器（`CF_Binding`）：換瀏覽器、無痕視窗或換電腦都要重新登入 | 請有 Cloudflare 權限的人加進名單（[Access 名單](demo-environment.md#access-名單)），改完再試 |
| 展示用 email 收得到驗證碼 | 登入頁輸入展示用 email，「寄送驗證碼」，信裡是 8 位數字，10 分鐘內有效，同一個驗證碼最多試 5 次（第 5 次之後不論對錯都作廢，要重寄）；寄信走 Resend 的 SMTP，2026-10-30 的 `trustiot-workstation login` 用過同一條路。信沒到先看垃圾郵件，頁面有「重新寄送驗證碼」（有冷卻秒數） | 展示前一天就試；信一直不來時換另一個已在展示租戶裡的 email，或用備用 session（下一列） |
| 備用 session | 展示前在**另一個瀏覽器設定檔**先登入好（app 的 session 閒置 12 小時、最長 7 天，使用中會自動延長），出事時切過去。備用 session 與台上的登入互不影響 | 備用的也要在展示前一小時內開過一次，確認沒過期 |
| 展示用帳號是展示租戶的 admin（或 editor） | 登入後只有一個租戶時自動進入，首頁是「首頁 / 歡迎使用 TrustIoT。」加「問 TrustIoT」輸入框。要有「儲存到儀表板列表」按鈕（步驟 4）就必須是 editor 或 admin；viewer 看得到儀表板、沒有按鈕。成員資格與角色以 API 設定：[展示環境 runbook](demo-environment.md#重設展示資料)第 8 步 | 設定成員的做法同上 |
| 語言與時區 | 設定頁的偏好是繁體中文與台北時區；圖與時間都以使用者的時區顯示。展示用的電腦時區也要是台北，沒設偏好時瀏覽器的設定會被採用 | 設定頁改，重新整理 |
| 主題用淺色 | 在展示用的瀏覽器登入，設定頁的「主題」選「淺色」。偏好存在帳號上，瀏覽器也留一份，登入前的畫面就用這一份。預設是「跟隨系統」：展示用電腦的系統是深色時，app 也是深色。投影走查（Laysi/trustiot-v2#1179）在每個視窗都截圖看過淺色；深色只量了版面，沒看過顏色與對比 | 設定頁改 |
| 乾淨的對話與儀表板列表 | 首頁下方「最近的對話」與「對話紀錄」會列出彩排的對話；「已存的儀表板」會列出彩排存的儀表板，而 MS3 的儀表板只能封存、不能刪除。展示前把彩排的對話刪除（可還原，在「已刪除」分頁），彩排的儀表板在列表按「封存」（可取消封存） | 見下一列 |
| 對話與儀表板的上限 | 每位使用者 24 小時最多開始 100 段對話；每分鐘最多發布與還原 10 個版本。彩排不會碰到，但彩排與展示在同一天時留意 | 沒有辦法繞過，只能等 |
| 投影的視窗大小 | 用全螢幕（F11），CSS 視窗至少 1280×720，版面才不會擠。1920×1080 的投影機：放大 150 % 加全螢幕（等於 1280×720）；要保留分頁列時，改用 125 % 的一般視窗（等於 1536×768），不要用 150 % 的一般視窗。1280×720 的投影機：100 % 加全螢幕，不要放大，也不要用一般視窗。依據是 Laysi/trustiot-v2#1179 的投影走查。有草稿的對話頁在這個寬度以上是兩欄（步驟 3、4；Laysi/trustiot-v2#1195 的走查）：不要為了單欄把視窗調到 1280 px 以下，單欄時草稿面板接在整段對話之後，看最新的回答時看不到 | 彩排時在現場的投影機上照這裡設定，走一次劇本 |

用滑鼠滾輪或觸控板捲動頁面時，游標不要停在折線圖（時間序列）上。這是目前的設計，不是故障：游標在折線圖上時，滾輪縮放那張圖，頁面不捲。往下捲時圖已經是全範圍、不能再縮小，看起來像頁面卡住；往上捲時圖會放大，圖上方出現「回到全範圍」，按它就恢復。設備頁的歷史圖、回答與儀表板頁裡的折線圖都是這樣；長條圖與 KPI 不會。捲動時把游標放在圖旁邊的文字或空白處；有草稿的對話頁看最新的回答時，右半邊是空的，游標放在那裡捲最穩（2026-11-07 的量測，Laysi/trustiot-v2#1196：游標在右半邊時每一下都捲動頁面；游標在對話上時，捲到折線圖移到游標下就停住）。

下列問題修好並換版之前，簡報者自己避開；換版後刪掉對應的一行：

- 步驟 3 打開資料來源面板後，往下捲一屏：面板出現在視窗下方（Laysi/trustiot-v2#1184）。
- 步驟 4 送出追問後，用滑鼠滾輪或觸控板捲到底：新的問題與回答在輸入框後面，觀眾看不到（Laysi/trustiot-v2#1182）。不要按 End：游標還在輸入框裡，End 只會移動輸入框裡的游標。
- 結尾不按「停止」：停止後的提示一行只有一兩個字，把輸入框撐得很高（Laysi/trustiot-v2#1183）。

### AI

| 項目 | 做法與預期 | 不通過時 |
| --- | --- | --- |
| ★ 模型連得上 | 展示當天早上以展示用帳號開一段新對話，問「現在的溫度是多少？」：幾秒到十幾秒內有回答（2026-11-06 兩個問題的 `question finished` 是 14.6 秒與 5.2 秒）。問完把這段對話刪掉 | 看 assistant 與 ai-gateway 的日誌；AI 閘道的 `main` 角色是 Opus 5.5 為首選、Sonnet 5.5 為備援，備援時畫面看不出來（日誌的 `model` 欄位看得到） |
| Anthropic 用量 | 展示用的 Anthropic 工作區每月上限是 20 美元，沒有設通知門檻（[外寄郵件與 Anthropic 金鑰](demo-environment.md#外寄郵件與-anthropic-金鑰)）。展示前由有 Console 權限的人看本月已用多少；彩排與展示都會用 Opus。上限到了模型呼叫就失敗，AI 整個不能用 | 由有權限的人調整上限，這不是 agent 能做的 |
| 要在新的對話裡展示 | 對話的工具組、system prompt 與回答工具在**建立時**固定：`TRUSTIOT__STREAMS__ANSWER_TOOL` 打開之前建立的對話不會串流，部署儀表板元件之前建立的對話沒有元件。展示一律從首頁開新對話，不重用任何舊的對話 | — |

### 展示當天不要做的事

- 換 chart 版本、改 `values-demo.yaml`、重設資料、重啟 k3s 或 istiod、改 Cloudflare 設定。
- 在展示機上跑 lab 的 e2e 或任何會取 lab 鎖的工作。
- 重啟閘道器的 agent，或在步驟 2 的斷網期間斷電。
- 展示當天才打開儀表板檢查。

## 展示

**順序**：劇本的第 3 步原本包含 email 登入，但步驟 1 與 2 的畫面（設備頁）要登入之後才看得到，所以本文把登入放在最前面當開場。這與[里程碑](../roadmap/milestones.md)「展示劇本」的順序不同，維護者審查本文時決定；要照原順序時，步驟 1 與 2 只在閘道器旁做實體動作，登入之後在步驟 3 的儀表板與設備頁回頭證明。

每一步的格式：**操作**、**畫面上會看到**（含程式的依據）、**要說的話**、**注意**。

### 開場：登入

**操作**

1. 展示用的瀏覽器開 `https://$DEMO_APP_HOST/`。Access 的 Google 登入通過後是「登入 TrustIoT」。
2. 輸入 email，按「寄送驗證碼」；頁面換成「請查看你的 email」，說明驗證碼 8 位數、10 分鐘後失效。
3. 開信、輸入 8 位數字、按「登入」。

**畫面上會看到**：只有一個租戶時直接進入它；首頁有標題「首頁」、「歡迎使用 TrustIoT。」、輸入框「問 TrustIoT」與「最近的對話」。選單在頂端：對話紀錄、已存的儀表板、設備、設定等；平台管理員另有「平台後台」，展示不點它。

**要說的話**：「TrustIoT 不用密碼。輸入 email，系統寄一組 8 位數的驗證碼，輸入之後登入。外面還有一層 Cloudflare 的存取控制，只放行受邀的人。」

**注意**：驗證碼錯誤或過期時頁面說「驗證碼不正確或已失效」，可以重新寄送。展示的主要風險是信沒到（見「出錯時」）。

### 步驟 1：閘道器讀設備，資料進展示環境

**操作**：選單「設備」，打開 `lab-temp-humidity-1`（型號 `ebyte-eid041-temp-humidity`，資料點 `temperature` 與 `humidity`）。看「目前讀數」卡與標籤。要看到新的讀數就重新整理頁面：設備頁沒有自動更新，每次重新整理才會重新查詢（`frontend/src/pages/device/DevicePage.tsx`；查詢的 `refetchOnWindowFocus` 也關著）。

**畫面上會看到**：「目前讀數」列出溫度與濕度的最新值與時間；標籤「資料正常 · N 秒前」（最新讀數不超過 3 個取樣間隔、至少 2 分鐘，也就是 3 分鐘；超過就變「資料停滯」）；「歷史」卡可選時間範圍（最近 1 小時、6 小時、24 小時、7 天…）。展示的設備是 lab 的 RS485 轉換器下的溫濕度變送器，不是電表（Laysi/trustiot-v2#793 的[測試報告](../test-reports/2026-10-30-793-demo-bind-device.md)）。

**要說的話**：「這是 lab 裡的一台溫濕度變送器，接在 RS485 轉換器上。閘道器每 60 秒讀它一次，經 Cloudflare Tunnel 把資料送進這個環境，這一頁看到的就是雲端存下來的最新一筆。」（閘道器的指示燈每分鐘閃一次，維護者 2026-11-02 決定維持 60 秒。）要補充安全性時：「閘道器連雲端用自己的憑證，金鑰放在 TPM 裡，不經過剛才的登入。」

**注意**：不要說「即時」。讀數最多晚一個取樣間隔再加上傳的幾秒。讀數的值不要在稿裡寫死，以螢幕為準。

### 步驟 2：拔掉網路線再接回，圖表補齊、沒有重複

這一步要有人在閘道器旁。簡報者不在閘道器旁時，先約好口令（電話或對講機）。

**操作**

1. 設備頁選資料點 `temperature`、時間範圍「最近 1 小時」。確認圖沒有斜紋，圖下「讀數」那一格的註記是「無缺漏」。記下時間 T0。
2. 在閘道器旁拔掉網路線。
3. **保持拔掉約 6 分鐘，不少於 5 分鐘。** 理由（都來自程式）：取樣間隔 60 秒；圖只把「超過 3 個區間」的空洞當成缺口（`frontend/src/pages/device/chart.ts` 的 `gapsOf`），以 1 分鐘為區間時要 4 分鐘以上才畫出斜紋；設備頁的「資料停滯」要 3 分鐘；閘道器在斷線期間每 5 分鐘才關閉一次收集視窗（[edge/agent](../../edge/agent/README.md)「收集視窗與批次」），拔 5 分鐘以上才走到這條路。
4. 拔掉約 4 分鐘時重新整理設備頁：出現「N 分鐘 M 秒沒有新的讀數」的提示、標籤是「資料停滯 · 最後讀數 …」，圖的右側是斜紋「N 分鐘 M 秒沒有讀數」（圖例的「沒有讀數」），「讀數」那一格的註記也改成「N 分鐘沒有讀數」（取整到分鐘）。（缺口與斜紋要在雲端**沒有**資料時才看得到，所以要在接回之前看。）
5. 約 6 分鐘後在閘道器旁接回網路線，記下時間 T1。
6. 等 1 到 2 分鐘（agent 的重連退避最多 60 秒，連上後最舊的批次先送、一次一批，逐批等雲端確認），重新整理設備頁：標籤回到「資料正常」，斜紋消失，「讀數」那一格的註記回到「無缺漏」。
7. 要證明沒有重複：展開「以表格顯示」，每個區間的「讀數」是 1 筆（區間是 1 分鐘的時候；彩排時確認這個範圍的區間）。同一批若重送，雲端以（閘道器、串流、序號）判斷重複並回 `duplicate`，不會寫第二次（[protocol README](../../crates/protocol/README.md)「遙測與確認」）。

**畫面上會看到**：如上，斷網前「資料正常」、斷網中「資料停滯」加斜紋缺口、接回後缺口消失。儀表板的時間序列元件（Laysi/trustiot-v2#1144 部署後）也在缺口處斷線並畫斜紋，所以步驟 3 若在斷網期間或之前建立儀表板，重新整理後也看得到補齊。

**要說的話**：「現在我拔掉閘道器的網路線。閘道器沒有停：它照樣每分鐘讀設備，先存在自己的磁碟裡。雲端這邊沒有新資料，所以圖上出現缺口，頁面也說資料停滯。……接回來之後，閘道器依序把存著的資料補送，雲端靠序號確認每一批只收一次。所以缺口補上了，每一分鐘還是一筆，沒有重複。舊系統就是在這裡掉資料。」

**注意**

- 拔線期間**不要重啟 agent，也不要斷電**：還在記憶體裡、沒關閉的收集視窗（最長約 5 分鐘）會遺失，補不回來；寫進磁碟的批次不會遺失。
- 圖的缺口以**讀值時間**（`ts`）算，補送的資料帶原本的時間，所以補齊的是歷史位置，不是接回的那一刻。
- 閘道器如果在拔線前剛開機，時鐘可能還沒校正，讀值的時間不準；步驟 2 要在「展示前檢查」的 `ntp-synchronized` 之後做。
- 整夜關機造成的缺口不是這一步的缺口，見下面「電源缺口怎麼說」。

#### 電源缺口怎麼說

觀眾若問「圖上這段夜裡為什麼沒有資料」：「那段時間閘道器關機，沒有讀值可記，是沒有紀錄，不是資料在路上掉了。網路斷線不一樣：閘道器開著、讀值照樣存在它的磁碟，連回來會補送，所以步驟 2 沒有缺口。」兩者在圖上都是斜紋，差別在能不能補：關機的缺口永遠補不回來，斷網的會補。

### 步驟 3：用一句話要儀表板，點任一數字看資料來源

**操作**

1. 回首頁（頂端的 TrustIoT 標誌），在「問 TrustIoT」輸入框輸入要求句，按「送出」（或 Enter）。這會建立新的對話並打開對話頁。
2. 等待：問題下方是「正在查詢讀數…」，AI 在這段時間讀型號、查資料、寫儀表板。
3. 回答出現後，回答下方有儀表板卡片，標籤「草稿・第 1 版・未儲存」。滑過圖看 tooltip，點圖例切換系列，點圖上的資料點或 KPI 數字。
4. 點數字或資料點會打開「資料來源」面板。從左欄回答卡片裡的數字或資料點打開，不要從右欄的草稿面板打開（見「注意」）。

**要求句**（要求句裡給時區：對話的租戶脈絡目前是空的，AI 不知道使用者的時區，工具說明在不知道時用 UTC，「每天」就會以 UTC 的日界切）：

- 主要：「做一個儀表板：最近 7 天每天的平均溫度與最高溫度，用長條圖比較，另外用一個數字顯示現在的溫度和濕度。以台北時間分天。」
- 閘道器剛開機或最近有關機的夜晚、7 天的圖太破碎時：「做一個儀表板：最近 2 小時每分鐘的溫度與濕度走勢，另外用一個數字顯示現在的溫度和濕度。」2 小時以每分鐘畫，閘道器連續在線時沒有缺口，斷網補齊的段落也看得到。

**畫面上會看到**

- 儀表板由元件組成：時間序列（折線）、長條、KPI 數字（可帶迷你走勢）、表格、狀態；一張圖最多 6 個系列。元件不運算，只顯示資料集的值；運算（平均、最大、分桶）寫在 AI 的 SQL 裡。
- **兩欄與草稿面板**：回答寫出草稿後，頁面在 CSS 寬度 1280 px 以上變成兩欄：左邊是對話，右邊是草稿面板。
  - 草稿面板與回答下方的卡片是同一份草稿：同一個標題、「草稿・第 N 版・未儲存」與自己的「儲存到儀表板列表」，內容是這份草稿最新的一版。
  - 兩欄時對話欄 520 px，比沒有草稿時的單欄（768 px）窄，回答卡片裡的圖也較小。1280、1536、1920 的視窗，兩欄的寬度都一樣（2026-11-07 的兩欄走查，Laysi/trustiot-v2#1195）。
  - 新對話在回答寫完時才從單欄變成兩欄（見「注意」）；之後追問寫出的新版本只換面板與卡片的內容，版面不再改變。
- **資料來源面板**：標題「資料來源」，列出資料集的標題與說明、查詢本身、「查詢時間」，以及被點的那一列或整個結果。查詢本身有兩種寫法：AI 寫的 SQL（顯示「SQL」），或統計查詢的參數（顯示「遙測統計查詢」與一段 JSON）。**不一定是 SQL**，由 AI 選；要說「資料集的查詢」。「查詢時間」是這次查詢執行的時刻，不是花了多久（程式只存 `queried_at`），所以不要說「查詢花了幾毫秒」。
- 這個畫面的資料集是以**檢視者自己的身分**查的，租戶與角色由資料庫強制：換一個權限較少的成員看到的就較少。

**要說的話**：「我只講了一句話。AI 先看這個租戶有哪些設備和資料點，再自己寫查詢、做出這個儀表板。每個數字都是查詢的結果：我點這個數字，這裡就列出它來自哪個資料集、查詢怎麼寫、什麼時候查的。AI 不能改數字，圖表元件只負責把查詢的值畫出來。」

**注意**

- AI 的寫入有檢查：一次提問最多寫 6 次儀表板，不通過會自己修正；寫不出來時回答會說明，見「出錯時」。2026-11-07 的日誌裡，真模型在展示環境只回答過兩個資料題（共 5 次模型呼叫），沒有寫過儀表板，所以做儀表板要多久沒有量測值，彩排時記錄。
- 回答裡的圖在靠近視窗時才查資料；一個租戶同時最多 2 個資料集在跑，所以打開有很多圖的長對話會慢。
- 回答不會顯示 SQL 或程式碼：回答文字經過濾，SQL、HTML、程式碼被移除並標示。SQL 只在資料來源面板。
- **資料來源從左欄的回答卡片打開**：右欄草稿面板裡的 KPI 方塊較窄，從那裡打開的資料來源面板只有 204 px 寬、約 2400 px 高，值一個字一行（Laysi/trustiot-v2#1197）。回答卡片裡的面板是 358 px 寬，1920×1080 時整個放得進畫面。
- **第一份草稿出現的那一刻，版面會跳一次**（2026-11-07 在 UI 預覽的實測，Laysi/trustiot-v2#1198）：新對話送出問題時是置中的單欄。頁面要等回答寫完、存下來之後才讀到草稿，這時一次變成兩欄：整欄對話向左移約 176 px、從 768 px 縮到 520 px，文字多換行，問題與回答往下移約 50～130 px；右邊同時出現草稿面板，第一屏就看得到面板裡的數字與圖的上段。對話仍在頂端列與輸入框之間，頁面不會自己捲動，焦點不動。這是**一次**跳動，不是閃爍，不用處理。
  - 要說的話：跳動時說「回答寫完了，儀表板出現在右邊。」指向右邊的草稿面板，不要回頭找剛才讀到的那一行。
  - 兩欄之後，左欄回答下方的卡片跟著往下移：1280×720 時卡片的標題落在輸入框後面，要往下捲才看得到。第 4 點從左欄的卡片打開資料來源之前，先往下捲。
  - **不要**在回答還在寫的時候重新整理頁面：重新整理後頁面會再讀一次草稿列表，AI 已經寫出草稿時，兩欄會在回答串流的途中出現，對話欄在觀眾讀的時候縮窄（依程式推論，沒有實測）。
  - 預覽的假服務立刻回答，量到的是最快的情況。在展示環境，單欄、卡片顯示骨架屏的畫面會多停一次讀草稿列表的往返；變成兩欄之後，圖還要等資料集查完（卡片與面板暫時是「載入中」）。彩排時量這兩段的秒數，記在紀錄表；要等好幾秒時，對觀眾說「資料正在查」，不要重新整理。

### 步驟 4：在對話中修改儀表板、再問 AI、儲存、重新整理

**操作**

1. 在同一段對話底部的輸入框「繼續問 TrustIoT」輸入修改句，送出：「改成每小時，只看濕度。」（7 天的版本；2 小時的版本改說「改成每 5 分鐘」。）新的回答下方出現同一個草稿的新版本「草稿・第 2 版・未儲存」。第一則回答仍顯示它自己那一輪寫的第 1 版：每則回答畫它那一輪寫的草稿的最後一版。右欄的草稿面板在回答完成時換成第 2 版，不用重新整理。
2. 再問一題：「這 7 天哪一天最熱？濕度最高是什麼時候？請分三段說明。」回答**一段一段**出現：問題下方先是「正在查詢讀數…」，第一段到了之後是「正在寫回答…」。要有幾段才看得出串流，所以要求分段；只有一段的回答會一次出現。
3. 在**最後一個回答**的儀表板卡片按「儲存到儀表板列表」。成功後出現「已儲存為儀表板列表裡的第 1 版」與「開啟儀表板」。草稿面板也變成「已發布・第 1 版」，出現同樣的通知與連結，但它在頁面頂端，這時不在畫面上。
4. 按「開啟儀表板」：儀表板頁顯示標題、「已發布・第 1 版」、「資料時間 …」與「每次開啟都會重新查詢」。等一分鐘（讓新的讀數進來），按「更新資料」或重新整理（F5）：「資料時間」更新、圖與 KPI 以最新的資料重新查詢，每張卡片標題下的「查詢於 …」是新的。要打開資料來源，按圖的標題列右邊的「資料來源」，不要點 KPI：儀表板頁在 1280 px 以上一列四格，KPI 只佔一格，從它打開的資料來源面板只有約 226 px 寬（Laysi/trustiot-v2#1197）。

**要說的話**：「我可以在對話裡直接改它：改成每小時、只看濕度。AI 讀目前的版本，只改要改的部分，做出第 2 版。再問它一個問題，回答是一段寫完、一段送出，不用等整篇。我覺得可以了就儲存，它就進入組織的儀表板列表，有自己的網址。之後任何人打開它，都會重新查詢：不是存圖，是存查詢。」

**注意**

- **串流是一段一段，不是逐字**：回答的過濾要看到整段才知道要移除什麼，所以服務只送寫完的段落（[services/assistant](../../services/assistant/README.md)「回答的串流」）。不要說「逐字」。
- **提問進行中不要重新整理對話頁**：問題與回答在回答完成時才一起存，中途重新整理看不到問題文字（頁面會接上已寫好的部分）。
- **儲存之後不要重新整理對話頁再按一次儲存**：已儲存的狀態只在這次打開頁面期間顯示（Laysi/trustiot-v2#1096），重新整理後按鈕又出現，再按得到 409 `dashboard.saved_changed`。要「儲存後重新整理」就用「開啟儀表板」打開儀表板頁，在那一頁重新整理。
- 只在最後一個回答（第 2 版）的卡片儲存。右欄草稿面板的按鈕存的也是最新的第 2 版，但它在頁面頂端，看最新的回答時不在畫面上。每則回答的卡片都有儲存按鈕，第一則回答的是第 1 版；先在第一則回答儲存第 1 版，再儲存第 2 版會得到 409 `dashboard.saved_changed`：第 2 版寫在第一次儲存之前，不以列表裡的任何一版為基礎（[services/assistant](../../services/assistant/README.md)「發布的衝突」）。
- **草稿面板在頁面頂端，往下捲就離開畫面**：看最新的回答時，右欄的面板已在視窗上方，螢幕右半邊是空的（Laysi/trustiot-v2#1196）。台詞不要說「右邊這個」；要指草稿面板，先捲回頁面頂端。
- 按鈕只出現在「對話擁有者且是 editor 或 admin」時（伺服器的 `permissions.can_publish` 決定），與那一版是不是草稿的最新版無關。
- 儀表板與修改都計入 AI 額度與每月的 Anthropic 上限；打開與重新整理儀表板只重跑查詢，不呼叫模型。

### 結尾：停止鈕（選用）

回答進行中，輸入框的按鈕是「停止」；按下後畫面說「已停止，這次的問題沒有留下。問題放回輸入框，可以修改後再送出。」問題與回答都不存，部分回答隨提問結束丟掉。展示時不需要用，但彩排要做一次（Laysi/trustiot-v2#1044 的第 5 項）。

## 出錯時

先判斷是哪一層：**進不去**（Cloudflare、登入）、**資料不對**（閘道器、資料）、**AI 不動**（模型、assistant）、**畫面怪**（儀表板、串流）。處理時不在台上改叢集；要改的都等下台。每一項的第一個動作都是「用備援讓展示繼續」。

| 症狀 | 可能的原因 | 在台上 | 下台後 |
| --- | --- | --- | --- |
| Access 顯示沒有權限的頁面 | 帳號不在受邀名單，或 Access 的 session 過期、換了瀏覽器 | 切到備用 session 的瀏覽器設定檔 | 加名單（[Access 名單](demo-environment.md#access-名單)） |
| 頁面被 Cloudflare 擋（challenge 或錯誤碼頁） | Bot Fight Mode 或規則 | 備用 session；換網路 | `tunnel.ts check --stage 2` 與[檢查對外的請求](demo-environment.md#檢查對外的請求)的判讀表 |
| 登入頁說服務暫時無法使用（503） | 身分服務連不上 | 等約 1 分鐘重新整理；備用 session | `flux.ts status` 看 identity 與 postgres |
| 驗證碼信沒到 | Resend、垃圾郵件、收件端延遲 | 等 1 分鐘、看垃圾郵件、按「重新寄送驗證碼」（有冷卻）；超過 3 分鐘就切備用 session | 看 Resend 的寄送紀錄；Laysi/trustiot-v2#492（展示環境寄測試信）還沒做 |
| 驗證碼被拒 | 超過 10 分鐘，或試了 5 次 | 重新寄一組，用最新那封 | — |
| 設備頁「資料停滯」，但閘道器有電 | agent 沒連上、閘道器在退避、NTP 未校時、網路線 | 等 2 分鐘（退避最多 60 秒）；看實體網路指示燈；改講步驟 2 的「斷網補送」並跳過步驟 1 | 前面「資料」一節的兩個指令；`ssh "$GATEWAY_SSH_HOST" 'sudo journalctl -u trustiot-agent --since "-15 min"'` |
| 步驟 2 接回後超過 3 分鐘還是「資料停滯」 | 閘道器還在退避，或沒有取得 IP | 再等；**不要重啟 agent**；確認網路指示燈 | 同上 |
| 步驟 2 的圖沒有斜紋 | 拔得太短（缺口要超過 3 個區間）、沒有重新整理設備頁、範圍選了太長（區間變大） | 再拔久一點，並選「最近 1 小時」 | — |
| 「問題沒有送出：…」，文字還在輸入框 | 服務暫時不可用、同時提問上限、24 小時 100 段對話的上限 | 照訊息的原因；通常重送一次 | 日誌的 `question started` 與 `question finished` |
| 一直停在「正在查詢讀數…」超過 2 分鐘 | 模型慢、AI 在修儀表板（最多寫 6 次）、模型備援 | 按「停止」，改用更簡單的要求句：「只畫最近 2 小時溫度的折線圖」 | 日誌的 `model call completed`（`duration_ms`、`model`）與 `the dashboard tool saved nothing`（`kinds`） |
| 寫到一半的回答消失，輸入框上方出現「助理目前無法使用，請稍後再試。」、「助理回答花了太久時間，請再試一次。」或「助理無法回答這個問題。」，問題回到輸入框 | 模型呼叫失敗。第一句是供應商或 AI 閘道忙碌、過載、伺服器錯誤或連不上（HTTP 429、529、5xx），第二句是逾時，第三句是請求被拒（金鑰、權限、請求格式）或回應無法解讀。Anthropic 工作區的每月上限用完時出現哪一句，沒有實測過 | 再按一次送出（問題還在輸入框）；重試後仍然失敗，就不在台上等，改放彩排的錄影。Laysi/trustiot-v2#1183 部署前，這行字和「已停止」一樣會被擠成一字一行 | 日誌 `question finished` 的 `outcome`（`model_unavailable`、`model_timeout`、`model_failed`）；請有 Console 權限的人看本月用量（上面「AI」一節的「Anthropic 用量」） |
| 輸入框上方出現「這個問題需要的工作量超過預算，請把問題問得更具體。」或「這個問題花的時間超過助理處理一個問題的上限，請把問題問得更具體。」 | 這一題用完了助理處理一個問題的工作量預算或時間上限 | 改用更簡單的要求句（同上面「一直停在」那一列） | 日誌 `agent run finished` 的 `outcome`（`budget_exhausted`、`run_expired`）、`steps`、`budget.spent` 與 `budget.limit` |
| 輸入框上方出現「助理重新啟動，這個問題被中斷了。請再問一次。」 | assistant 在回答途中重新啟動（部署滾動或 pod 重啟） | 再送一次 | `flux.ts status` 看 assistant 的 restarts；`question finished` 的 `outcome` 是 `interrupted` |
| 回答說做不出儀表板，或說沒有資料 | 要求句超出元件、資料點名稱不對、查詢被拒（可查期間：原始讀數 7 天、分鐘與小時彙總 3 個月）、儀表板檢查沒打開而 AI 想寫自訂頁面 | 改簡單的要求句；只問溫度與濕度；不要要求圓餅圖、散佈圖等元件畫不出的圖 | 日誌的 `kinds` |
| 儀表板卡片是「無法載入資料」或某個元件說「無法顯示這個元件」 | 資料集查詢失敗或超時、`telemetry.account_not_ready`（telemetry-query 剛啟動，要等一下）、一個租戶 60 次/分鐘的查詢上限 | 等 30 秒按「更新資料」；其他元件不受影響 | `a dataset run's call to a service failed` |
| 時間序列的缺口是一條直線 | Laysi/trustiot-v2#1144 沒有部署 | 改用設備頁證明缺口 | 部署 Laysi/trustiot-v2#1144 |
| 回答一次全部出現，沒有串流 | 回答只有一段；或這段對話是回答工具打開之前建立的；或串流被切斷後客戶端重連（Cloudflare 125 秒沒資料會切） | 要求「分三段」重問；一律開新對話 | `TRUSTIOT__STREAMS__ANSWER_TOOL` 與 `question started` |
| 看不到「儲存到儀表板列表」 | 不是 editor 或 admin；不是這段對話的擁有者（例如從「分享給我的」打開的唯讀頁） | 確認打開的是自己的對話；不行就只展示儀表板列表裡已有的儀表板 | 設定成員角色 |
| 儲存後重新整理對話頁，按鈕又出現 | Laysi/trustiot-v2#1096 | 不要再按；用「開啟儀表板」或選單「已存的儀表板」打開 | — |
| 按儲存得到 409 | `dashboard.saved_changed`：這一版不是以列表裡最新版為基礎 | 在最後一個回答儲存；或請 AI「把最新版併進草稿」再儲存 | — |
| 整個頁面空白或「無法載入」 | 部署中（滾動重啟）或 chunk 已更新 | 重新整理一次（頁面遇到載入失敗會自己重新整理一次）；等 1 分鐘；備用 session | `flux.ts status` |
| 整個環境起不來 | 叢集或 Postgres 異常 | 改放彩排時錄的操作影片或螢幕截圖；說明環境在 lab、不是雲端服務 | [展示環境 runbook](demo-environment.md) |

備援的備援：每次彩排**錄影**（螢幕與聲音，不含 email 與主機名稱，錄完先看過再存），展示時整個環境掛掉就播放。

## 彩排清單

彩排用展示的那一版（見「先決定：展示用哪一版」），前提是 Laysi/trustiot-v2#149 與 Laysi/trustiot-v2#881 已關閉、Laysi/trustiot-v2#1144 已部署。彩排由維護者與展示者一起做；agent 不在展示環境操作瀏覽器，只讀證據。**結果存進 `docs/test-reports/YYYY-MM-DD-459-demo-rehearsal.md`**（格式見 [test-reports/README.md](../test-reports/README.md)），不含主機名稱、email、對話內容與位址；原始數字放 `docs/test-reports/data/` 下的同名目錄。

### 每一步要記的

| 項目 | 記什麼 |
| --- | --- |
| 開始前 | 日期與時間（Asia/Taipei）、chart 版本、`flux.ts status` 的摘要、「展示前檢查」每項的結果、`newest_received_utc` 與 `gateway_state` |
| 開場 | 驗證碼信從按下「寄送驗證碼」到收到的秒數；登入有沒有第二次 Access 的登入 |
| 步驟 1 | 設備頁的標籤與最新讀數的時間（以螢幕截圖時間為準）；「最近 7 天」圖的斜紋與原因（關機的夜晚） |
| 步驟 2 | T0（拔）、T1（接）；設備頁在拔了約 4 分鐘時的提示與斜紋；接回後多久（秒）標籤回到「資料正常」；補齊後有沒有斜紋、「以表格顯示」每列是不是 1 筆；儀表板的時間序列有沒有同樣斷線；**缺口與重複的資料證據**：照 [24 小時連續性報告](../test-reports/2026-10-31-794-demo-24h-continuity.md)的查詢（臨時只讀使用者、查完刪除），查 T0 到 T1 前後每個資料點的列數、不同的 `ts` 數與最大間隔 |
| 步驟 3 | 要求句原文；送出到儀表板出現的秒數；元件種類；資料來源面板是 SQL 還是統計查詢；有沒有看到自訂頁面（打開儀表板檢查之後仍應該沒有）；回答最後一段出現後，單欄的卡片骨架屏停了幾秒、右邊的圖幾秒後出現（Laysi/trustiot-v2#1198） |
| 步驟 4 | 修改句與時間；第一段回答出現的秒數、回答總共幾段；儲存後「已儲存為第 N 版」；儀表板頁重新整理前後的「資料時間」 |
| 全程 | 螢幕錄影；每一個和本文不同的地方（本文以彩排的結果修正） |

### 要用真模型確認的項目

下面是其他 issue 留給彩排的真模型確認（Laysi/trustiot-v2#554 已在 2026-11-07 以兩輪對話的日誌關閉，PR Laysi/trustiot-v2#1143，不再等彩排；最後一列的查詢仍可用來看彩排的快取命中），每一項都有只讀的證據，不需要額外呼叫模型。查詢都在展示機上對 ClickHouse 執行（做法同上一節的資料查詢，換成下面的 SQL）；日誌不含問題、回答、標題、SQL 與儀表板內容（[services/assistant](../../services/assistant/README.md)「日誌與指標」）。`<彩排開始 UTC>` 是彩排開始的時間，寫成 `'YYYY-MM-DD HH:MM:SS'`。

| issue 與條件 | 彩排做什麼 | 只讀的證據 |
| --- | --- | --- |
| Laysi/trustiot-v2#1044 第 5 項：以真模型實測一次提問與停止 | 首頁問一個簡單問題，等到回答；再問第二題，在回答進行中按「停止」 | `question finished` 的 `outcome` 一個是 `answered`、一個是 `cancelled`，加上 `duration_ms`：<br>`SELECT Timestamp, LogAttributes['run_id'] AS run, LogAttributes['outcome'] AS outcome, LogAttributes['duration_ms'] AS ms FROM trustiot_logs.otel_logs WHERE ServiceName = 'assistant' AND Body = 'question finished' AND Timestamp >= toDateTime('<彩排開始 UTC>') ORDER BY Timestamp` |
| Laysi/trustiot-v2#935 第 2 項：題庫的一題，答案是沒有自訂頁面的元件模式儀表板 | 從 `evals/dashboard/questions.json` 挑一題溫濕度的題目，照原文問（展示環境的設備只有 `temperature` 與 `humidity`，問功率與用電的題目只會得到沒有資料）。建議 `dashboard-temperature-humidity-trend`：「幫我做一個儀表板，看最近 12 小時的溫度和濕度走勢。」（題庫期望的元件種類是 `time_series`）；也可以用 `dashboard-daily-temperature`（`bar`）或 `dashboard-current-climate`（`kpi`），報告寫明問了哪一題 | 同一個 run 有 `the dashboard tool wrote a version`，沒有 `dashboard check finished`（沒有送檢查，也就是沒有頁面）；再以操作員的 session 讀那份草稿，確認沒有 `page`、元件種類是那一題期望的種類（`dashboard-temperature-humidity-trend` 是 `time_series`）：<br>`o=$(session_origin) && session_headers \| curl -s -K - "$o/api/assistant/v1/dashboards/<草稿 ID>" \| bun -e 'const v=JSON.parse(await Bun.stdin.text()).version; console.log("page:", v.page == null ? "none" : "present", "components:", (v.spec.components ?? []).map((c) => c.kind).join(","))'`（`session_origin` 與 `session_headers` 見[平台管理員的登入確認](demo-environment.md#平台管理員的登入確認)；草稿 ID 從對話頁的網址或 `GET /api/assistant/v1/conversations/<對話 ID>/dashboards` 取得，不貼進報告） |
| Laysi/trustiot-v2#1047：真模型重新產生最後一則回答 | 彩排的版本含 PR Laysi/trustiot-v2#1145 時做：在自己對話的最後一則回答按「重新產生」一次。不含時這一項跳過並在報告寫明，不用 API 手動打 | `question started` 與 `question finished` 的 `regenerated` 是 `true`，`discarded_messages` 大於 0，`outcome` 是 `answered` |
| 觀察用（Laysi/trustiot-v2#554 已關閉）：3 輪對話的快取命中 | 步驟 3、步驟 4 的修改句與問題剛好是同一段對話的 3 輪，不用另外做 | 每次模型呼叫的 token 與模型，第二次呼叫起 `cache_read` 大於 0：<br>`SELECT Timestamp, LogAttributes['run_id'] AS run, LogAttributes['step'] AS step, LogAttributes['model'] AS model, LogAttributes['usage.input'] AS input, LogAttributes['usage.cache_read'] AS cache_read, LogAttributes['usage.cache_write'] AS cache_write, LogAttributes['usage.output'] AS output FROM trustiot_logs.otel_logs WHERE ServiceName = 'assistant' AND Body = 'model call completed' AND Timestamp >= toDateTime('<彩排開始 UTC>') ORDER BY Timestamp FORMAT TSVWithNames`（欄位是 `usage.cache_write`，不是 `cache_creation`；`model` 欄看得出有沒有退到 Sonnet 備援）。2026-11-06 的兩輪已有 `cache_read` 0 → 13,615 → 14,331 → 15,336 → 15,953，彩排補上 3 輪以上 |

另外兩個看得到的指標，彩排時順便記：`the dashboard tool wrote a version`（`target` 是 `new`、`draft` 或 `fork`）與 `published a dashboard` 各有一筆；`the dashboard tool saved nothing` 的 `kinds` 若出現，記下是哪一類，它是展示時 AI 做不出儀表板的原因。

### 彩排通過的條件

- 四步都做完，每一步的結果與本文一致，或本文已依結果修正。
- 步驟 2：缺口出現、補齊、沒有重複，各有螢幕與資料的證據。
- 上表的項目，除了標明「等上線」的以外，證據都存進報告。
- 沒有任何一步需要在台上改叢集。

## 對照：每一步依據的程式與文件

| 本文的說法 | 依據 |
| --- | --- |
| 主題的預設「跟隨系統」、偏好在登入前用瀏覽器留的一份 | `frontend/src/preferences/preferences.ts`（`resolveTheme`、`STORAGE_KEY`）、`services/identity/src/preferences.rs` |
| 游標在折線圖上時滾輪縮放那張圖、頁面不捲；「回到全範圍」 | `frontend/src/dashboard/chart/options.ts` 的 `dataZoom`（`zoomOnMouseWheel`；設備頁的 `historyOption.ts` 以同一個 `timeSeriesOption` 為底）、`InteractiveChart.tsx`；ai-assistant.md「元件模式」的「互動」；`docs/test-reports/2026-11-01-895-dashboard-components.md`「觀察到、沒有處理的」；量測：Laysi/trustiot-v2#1196 |
| 登入頁、驗證碼 8 位、10 分鐘、5 次、重寄冷卻 | `frontend/src/pages/LoginPage.tsx`、`frontend/src/i18n/locales/zh-TW.json` 的 `page.login.*`；[services/identity](../../services/identity/README.md)「登入驗證碼」（`CODE_LIFETIME`、`MAX_ATTEMPTS`） |
| session 閒置 12 小時、最長 7 天 | services/identity/README.md「session」（`SESSION_IDLE_LIFETIME`、`SESSION_ABSOLUTE_LIFETIME`） |
| 單一租戶自動進入、沒有租戶時的選擇 | `frontend/src/session/TenantGate.tsx` |
| 首頁是對話框，送出建立對話並提問 | `frontend/src/pages/pages.tsx`、`frontend/src/pages/conversations/HomeComposer.tsx` |
| 設備頁的標籤、停滯門檻 3 個間隔且至少 2 分鐘、不自動更新 | `frontend/src/pages/device/health.ts`、`DevicePage.tsx`、`frontend/src/app/query-client.ts` |
| 缺口：超過 3 個區間才算、畫斜紋與「N 分鐘沒有讀數」 | `frontend/src/pages/device/chart.ts` 的 `gapsOf`、`historyOption.ts`、`zh-TW.json` 的 `device.history.*` |
| 閘道器斷線期間的收集視窗 5 分鐘、補送依序一次一批、以序號去重、關機與斷電的遺失範圍 | [edge/agent](../../edge/agent/README.md)「遙測上傳與離線緩衝」 |
| 讀值時間是閘道器的系統時鐘、`clock_synced` | edge/agent/README.md「時鐘」 |
| 資料表欄位 `ts` 與 `received_at`、去重的高水位 | `services/ingest/migrations/clickhouse/0002_telemetry.sql`、`0012_gateway_stream_ids.sql` |
| 儀表板由元件組成、元件不運算、最多 6 個系列、先用元件 | [docs/architecture/ai-assistant.md](../architecture/ai-assistant.md)「儀表板」「元件模式」；services/assistant/README.md「`dashboard` 工具」 |
| 資料來源面板：資料集、SQL 或統計查詢參數、查詢時間 | `frontend/src/dashboard/SourcePanel.tsx`（只存 `queried_at`）、ai-assistant.md「儀表板」第 6 點 |
| 資料集來源是 `sql`、`telemetry_statistics` 或 `telemetry_usage` | `services/assistant/src/tools/dashboard.rs` 的工具說明 |
| 沒有檢查服務時只能寫元件 | services/assistant/README.md「`dashboard` 工具」第 1 步（`custom_page_disabled`） |
| 租戶脈絡目前是空的、`tz` 預設 UTC | `services/assistant/src/api/conversations.rs` 的 `TENANT_CONTEXT`；services/assistant/README.md「`dashboard` 工具」 |
| 回答一段一段送出、不是逐字；進行中的問題不存 | services/assistant/README.md「回答的串流」「為什麼要回答工具」；`frontend/README.md`「回答的串流」「提問」 |
| 對話的工具與提示在建立時固定 | services/assistant/README.md「回答的串流」的開關、`Preamble` |
| 儲存、「開啟儀表板」、儲存狀態只在這次打開頁面期間 | `frontend/src/pages/conversations/AnswerDashboards.tsx`、`savedDrafts.tsx`；ai-assistant.md「儀表板」第 1 點（Laysi/trustiot-v2#1096） |
| 有草稿時兩欄（`xl`，1280 px 以上）、草稿面板畫最新的一版、回答寫出新版本時重讀 | `frontend/src/pages/conversations/ConversationPage.tsx`、`DraftPanel.tsx`、`drafts.ts` 的 `useConversationDrafts`；欄寬與面板位置的實測：Laysi/trustiot-v2#1195 |
| 新對話的第一份草稿在回答存下來之後才讀到，頁面一次從單欄變兩欄；回答進行中重新整理會提早變成兩欄 | `services/assistant/src/store/answer_drafts.rs`（回答存下來之後才列出 `draft_versions`）、`services/assistant/src/api/answer_stream.rs`（串流沒有草稿的資訊）、`frontend/src/pages/conversations/drafts.ts` 的 `useConversationDrafts`（依 `written` 重讀）；轉換的實測：Laysi/trustiot-v2#1198 |
| 儀表板頁每次開啟重新查詢、「更新資料」、「資料時間」 | `frontend/src/pages/dashboards/DashboardPage.tsx`；`zh-TW.json` 的 `dashboard_page.*` |
| 卡片標題下的「查詢於 …」、標題列的「資料來源」按鈕；KPI 只佔一格，資料來源面板畫在卡片裡 | `frontend/src/dashboard/DashboardComponent.tsx` 的 `Card`、`uses.ts` 的 `componentQueriedAt`；`frontend/src/pages/dashboards/VersionItems.tsx` 的格線；面板寬度：Laysi/trustiot-v2#1197 |
| 停止鈕與文案 | `frontend/src/chat/Composer.tsx`；`zh-TW.json` 的 `chat.composer.*` |
| 模型失敗、預算用完與中斷時的訊息；問題回到輸入框，寫到一半的回答丟掉 | `services/assistant/src/model/anthropic.rs`（`error_kind`、`stream_error`）、`services/assistant/src/agent.rs`（`AgentError::message_key`）、`frontend/src/pages/conversations/ConversationTurn.tsx`；`zh-TW.json` 的 `question.*`；[services/assistant](../../services/assistant/README.md)「日誌與指標」（`question finished`、`agent run finished`） |
| 重新產生的按鈕（PR Laysi/trustiot-v2#1145 之前只在 UI 預覽）；補全只在 UI 預覽 | `frontend/src/pages/conversations/regenerate.tsx`、PR Laysi/trustiot-v2#1145；補全：`frontend/src/chat/completion/context.tsx` 的說明 |
| 校時前讀到的值帶著錯的時間：ingest 不存 `clock_synced`；TTL 依 `received_at` | `services/ingest/src/telemetry/batch.rs`、`services/ingest/migrations/clickhouse/0002_telemetry.sql` |
| 日誌事件與欄位 | services/assistant/README.md「日誌與指標」 |
