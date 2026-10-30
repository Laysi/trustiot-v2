# 展示環境打開閘道器的資料路徑：MQTT broker、mqtt-bridge、ingest 與 gateway-ops（2026-10-30）

對應 issue：Laysi/trustiot-v2#795（父 issue Laysi/trustiot-v2#456）。這份報告接續 [Laysi/trustiot-v2#792 的報告](2026-10-30-792-demo-gateway-agent.md)：那時展示用閘道器的 agent 已經裝好、登錄過，但每次連線都在 WebSocket 升級時拿到 404，因為展示環境還沒有 MQTT broker。

結果先講：

- 展示叢集先換到 chart `0.0.0-rc.12`（PR Laysi/trustiot-v2#799），再由 PR Laysi/trustiot-v2#800 打開 broker、mqtt-bridge、ingest 與 gateway-ops。
- 展示用閘道器經 Cloudflare 與 `/mqtt` 連上 broker，完成內層 mTLS，得到 CONNACK 0。閘道器還沒綁定，所以 `online` 被拒（0x87），這是預期的。
- 未登錄的憑證連不上 broker。
- 展示機的記憶體放得下。
- runbook 的 broker 檢查腳本有一個缺陷，在這次修正並重跑過（見「未登錄的憑證連不上」）。

## 環境

| 項目 | 內容 |
| --- | --- |
| 展示機 | lab 的 Mac mini M1，k3s `v1.36.5+k3s1`，arm64，OpenSSL 3.5.9（broker 檢查用的 `openssl`）。展示環境以 Flux 部署（Laysi/trustiot-v2#467） |
| chart | `0.0.0-rc.12`，從 main `34d863c6` 發佈。部署中的版本是 `0.0.0-rc.12+e9b58baf0632` |
| 打開的 PR | Laysi/trustiot-v2#800，合併為 `d4bf9bf1`。HelmRelease 的 release v7 |
| broker | `eclipse-mosquitto:2.1.2-alpine`（以 digest 釘選，chart 的預設） |
| 閘道器 | lab 的閘道器（展示用閘道器），agent 與設定同 Laysi/trustiot-v2#792 的報告，還沒綁定租戶 |
| Cloudflare | gateway 主機名稱所在 zone 的 `http_ratelimit` 入口 ruleset |
| 時段 | 2026-10-30，從 rc.12 的前提（11:31 UTC 前記錄）到 broker 檢查重跑（12:24 UTC） |

時間都是 UTC。主機名稱、zone、Cloudflare 的 ID、帳號與 ssh 的 Host 都不寫進報告；主機名稱寫成「gateway 主機名稱」。

## 指令

`DEMO_SSH_HOST` 是工作站 `~/.ssh/config` 裡展示機的 Host，`<gateway 主機名稱>` 是展示環境的 gateway 主機名稱。

```sh
# 1. rc.12 之前的兩個前提（docs/runbooks/demo-environment.md「換發端點上線」）
#    Cloudflare：把 /mqtt 的速率限制規則延伸到 /certs/renew，讀回 ruleset。
#    istiod：rc.11 到 main 之間 istiod 的 values 沒有變。
git diff v0.0.0-rc.11 origin/main -- deploy/istio/istiod-values.yaml
# 速率限制的實測：60 個並行請求，數狀態碼（與當時做法等效的指令）
seq 60 | xargs -P 60 -I{} curl -s -o /dev/null -w '%{http_code}\n' -X POST "https://<gateway 主機名稱>/certs/renew" | sort | uniq -c
seq 60 | xargs -P 60 -I{} curl -s -o /dev/null -w '%{http_code}\n' "https://<gateway 主機名稱>/mqtt" | sort | uniq -c

# 2. 換版：推 v0.0.0-rc.12 tag，release.yml 執行；展示叢集的換版 PR 以 pin-chart.ts 開
bun scripts/flux/pin-chart.ts --cluster demo --version 0.0.0-rc.12 --digest <chart digest>

# 3. 每次合併後（換版 PR 與打開的 PR）
bun scripts/demo/flux.ts status --ssh-host "$DEMO_SSH_HOST"
bun scripts/demo/tunnel.ts route-origin --ssh-host "$DEMO_SSH_HOST"
bun scripts/demo/tunnel.ts check --ssh-host "$DEMO_SSH_HOST" --stage 2

# 4. 打開之後（runbook「打開閘道器的資料路徑」第 2 步，只讀）
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot get pods,jobs'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot logs job/<CronJob 建的 Job>'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot logs trustiot-mqtt-broker-0 -c mosquitto; sudo k3s kubectl -n trustiot logs trustiot-mqtt-broker-0 -c trust'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot logs deploy/trustiot-mqtt-bridge | grep -c "relaying a gateway connection"'
# 閘道器上（唯讀）
sudo journalctl -u trustiot-agent --since "<合併時間>"
# 未登錄的憑證：runbook 裡的整段 ssh "$DEMO_SSH_HOST" '...'，照抄執行
# 記憶體
ssh "$DEMO_SSH_HOST" 'free -m; sudo k3s kubectl top pod -n trustiot'
```

## 結果

### rc.12 的前提與換版

- **Cloudflare 速率限制**：規則讀回的設定如下。
  - expression `(http.request.uri.path in {"/mqtt" "/certs/renew"})`
  - characteristics `cf.colo.id`、`ip.src`
  - period 10 秒，每個 period 40 個請求，mitigation_timeout 10 秒，action `block`
  - 描述以 `mqtt-bridge and certs renewal` 開頭
- **速率限制的實測**：從工作站對 gateway 主機名稱發出。
  - 60 個並行的空 body `POST /certs/renew`：51 個 404（那時原點還沒有換發端點），9 個 429，回應標頭 `server: cloudflare`。12 秒後再送一次回 404，表示封鎖已解除。
  - `GET /mqtt` 兩輪各 60 個並行：14 個 404（那時原點還沒有 broker），106 個 429。
  - 更早還送過一輪 60 個 `/mqtt` 的 WebSocket upgrade，全部回 404。原因可能是 10 秒的計數視窗剛好重設，沒有再查證；上面兩輪已證明規則對 `/mqtt` 有效。
- **換發端點對外開放的審查**：
  - #795 分支的獨立安全審查沒有 Critical、High 或 Medium。
  - 屬於 certs 服務本身的 Low 另開 Laysi/trustiot-v2#801（P2）：知道硬體 ID 的人可以用光該閘道器的換發限流，未經驗證的流量也能寫入 `certs.gateway_renewal_refused` 事件。
  - 在它修好之前，上面的 Cloudflare 規則是上限：每個 IP 每 10 秒 40 次。
- **istiod**：`git diff v0.0.0-rc.11 origin/main -- deploy/istio/istiod-values.yaml` 是空的（main `34d863c6`）。
- **發佈**：`v0.0.0-rc.12` 在 main `34d863c6` 上發佈。
  - release.yml 執行 41048658329 的 verify 只有「套件必須私有」一項沒過，與 rc.8–rc.11 相同：repo 公開期間這項不會過。
  - 所以展示叢集的換版 PR 是以 `pin-chart.ts` 開的，即 Laysi/trustiot-v2#799，合併為 `3d080f36`。它只改展示叢集的 chart pin 與隨 chart 複製的 `values-saas.yaml`。
- **換版後**：
  - HelmRelease `UpgradeSucceeded`，chart `0.0.0-rc.12+e9b58baf0632 (deployed)`。
  - pod 都 Ready，migration Job `Succeeded`。
  - `tunnel.ts route-origin` 結束碼 0，`tunnel.ts check --stage 2` 20 項 PASS、0 FAIL。

### 打開資料路徑後的 Flux 與 pod

PR Laysi/trustiot-v2#800 合併為 `d4bf9bf1` 之後：

- `flux.ts status` 顯示 HelmRelease `UpgradeSucceeded`（release v7），chart `0.0.0-rc.12+e9b58baf0632 (deployed)`。
- namespace `trustiot` 的 14 個 pod 都正常：`trustiot-mqtt-broker-0`、`trustiot-mqtt-bridge-*`、`trustiot-ingest-*`、`trustiot-gateway-ops-*` 與重新滾動的 `trustiot-certs-*` 都是 Running、ready，restarts 0。
- migration Job `Succeeded`。

### broker 的伺服器憑證與 CRL

- 合併後第一次排程的憑證 CronJob（12:15 UTC）是 `Complete`，Job 的日誌有 `wrote a new server certificate and key for the broker`。
- mosquitto 隨後開始接受 8883 的連線。
- trust sidecar 在那之後沒有錯誤，也沒有 CRL 的錯誤。合併前它唯一的 WARN 是預期的「還沒有伺服器憑證」。

### 閘道器連上 broker

閘道器的 journal（唯讀）依序出現：

1. `connected to the broker`：穿過 Cloudflare 與 mqtt-bridge，完成內層 mTLS，得到 CONNACK 0。
2. `the broker refused the online status: 0x87 (not authorized)`：閘道器還沒綁定，所以這是預期的（services/certs/README.md「broker 權限」）。

同時 mqtt-bridge 的日誌有 23 次 `relaying a gateway connection`。打開之前，同一個 agent 每次都在 WebSocket 升級時拿到 HTTP 404（Laysi/trustiot-v2#792 的報告）。

### 未登錄的憑證連不上

在展示機上以 port-forward 直接連 broker 的 8883，試兩種握手：

| 握手 | broker 的回應 |
| --- | --- |
| 當場產生的自簽用戶端憑證 | `CONNECTED` 之後收到 `tlsv1 alert unknown ca`（SSL alert 48） |
| 不帶用戶端憑證 | `CONNECTED` 之後收到 `tlsv13 alert certificate required`（SSL alert 116） |

**runbook 腳本的缺陷**：

- 第一次照 runbook 原樣執行時，第一次握手得到預期的 alert，第二次握手卻得到 `Connection refused`，腳本印 `FAIL`。
- 原因：broker 拒絕握手後重設連線，`kubectl port-forward` 收到 `connection reset by peer`，接著以 `error: lost connection to pod` 結束。第二次握手根本沒有連到 broker。
- 當時的做法：分兩次執行，第一次照原樣（自簽在前），第二次對調順序（不帶憑證在前），兩次的第一次握手各得到上面的 alert。兩次結束後 18883 都沒有 listener，port-forward 程序也不在。

**修正**：這次把 runbook 的腳本改成每次握手各開一個 port-forward。

- 每次握手的流程：
  1. 確認 18883 沒有 listener。
  2. 開 port-forward，等到 `Forwarding from`。
  3. 握手。
  4. 以 `sudo -n kill` 停掉 port-forward，再確認 18883 沒有 listener。
- 保留 `timeout 120` 與最後的 PASS/FAIL 判斷。
- 先在建置主機（x86_64 Linux）上以替身驗證：一個只接受一條連線就結束的 TLS 伺服器代替 port-forward，`sudo` 與 `k3s` 也是替身。結果兩個 alert 都出現，印 `PASS`，沒有留下 listener。
- 再對展示機照抄執行一次（12:24:27–12:24:34 UTC）。結果兩次都被 broker 拒絕，最後一行是 `PASS: both refused; the port-forward is closed`，結束碼 0。之後展示機上 18883 沒有 listener，也沒有 port-forward 程序。
- 輸出：[broker-refusal.txt](data/2026-10-30-795-demo-data-path/broker-refusal.txt)。

### 記憶體

| 量測（打開之後） | 數值 |
| --- | --- |
| 展示機 `free -m` | 總共 7543 MiB，available 2867 MiB，swap 已用 808 MiB |
| clickhouse | 461Mi |
| postgres | 293Mi |
| 入口（ingress-istio，兩個 pod） | 50Mi、39Mi |
| otel-collector | 45Mi |
| broker、mqtt-bridge、ingest、gateway-ops | 各 3–5Mi |

打開之前（2026-10-30，同一天）available 是 3104 MiB，節點的 memory requests 合計 2420Mi。這次新增的 requests 約 192Mi：broker 64Mi 加上 trust sidecar，其他三個服務各 32Mi。

### tunnel

- `tunnel.ts check --stage 2`：20 項 PASS、0 FAIL。
- `/mqtt` 的 WebSocket 升級，從工作站與從展示機都由 mqtt-bridge 回 101（`sec-websocket-protocol: mqtt`）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 展示叢集部署含 broker、ingest、gateway-ops 的 chart 版本，Flux 套用成功、所有 pod Ready | 通過：chart `0.0.0-rc.12`，HelmRelease release v7 `UpgradeSucceeded`，14 個 pod Running 且 ready，migration Job `Succeeded` |
| gateway 主機名稱的 `/mqtt` 經 Cloudflare 到 broker，agent 日誌顯示內層 mTLS 完成、CONNACK 0（`online` 被拒 0x87 為預期） | 通過：`connected to the broker`，接著 `the broker refused the online status: 0x87 (not authorized)`；mqtt-bridge 有 23 次 `relaying a gateway connection` |
| broker 的伺服器憑證由 certs 簽發、CRL 已載入；未登錄的憑證連不上 | 通過：CronJob 寫入 certs 簽發的伺服器憑證，trust sidecar 沒有 CRL 錯誤；自簽憑證收到 alert 48，不帶憑證收到 alert 116，修正後的 runbook 腳本印 `PASS` |
| 展示機啟用後的記憶體量測與預算更新 | 量測通過：結果在上面的「記憶體」，放得下。預算：Laysi/trustiot-v2#457 沒有留下記憶體預算文件，預算文件由 Laysi/trustiot-v2#493 負責，數字以這份報告為準 |
| 公開內容沒有主機名稱、內網位址或帳號；`bun test deploy/flux`、`bun test deploy/chart` 通過 | 通過：<ul><li>PR Laysi/trustiot-v2#800 的 CI：執行的 job（changes、gitleaks、workflow-policy、helm）都通過，其他依變更的路徑略過。</li><li>`bun test deploy/flux` 78 pass、0 fail（併入含 rc.12 pin 的 main 之後）。</li><li>`bun test deploy/chart` 711 pass、0 fail（建置主機（x86_64 Linux），合併前）。</li><li>這份報告與 runbook 也沒有主機名稱、Cloudflare 的 ID 或帳號。</li></ul> |

後續：

- 綁定閘道器、建立設備與 gateway-ops 看得到在線：Laysi/trustiot-v2#793。
- 換發端點的限流與拒絕事件：Laysi/trustiot-v2#801。
