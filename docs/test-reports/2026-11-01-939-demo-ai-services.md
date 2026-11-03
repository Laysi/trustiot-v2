# 展示環境打開 AI 閘道、telemetry-query 與 assistant：打開前的記憶體盤點、換到 rc.13 與打開後的確認（2026-11-01、2026-11-03）

對應 issue：Laysi/trustiot-v2#939（父 issue Laysi/trustiot-v2#456）。這份報告記錄打開三個服務之前的唯讀盤點與 chart 的投影、換到 `0.0.0-rc.13` 之後的確認，以及 2026-11-03 打開三個服務（PR Laysi/trustiot-v2#941）之後的確認。

結果先講：

- 展示機放得下三個服務：打開後節點的記憶體 requests 從 2612Mi（34%）到 2804Mi（37%），limits 從 9970Mi（132%）到 11250Mi（149%）；`free -h` 的 available 是 2.7Gi。
- 只換版（rc.12 到 main 上的 chart）不改任何長駐 workload 的 requests 與 limits，叢集內 Postgres 的 `max_connections` 也不變（兩版都是 240）。但 Postgres 的映像 tag 跟著版本走（`postgres.bundled.image.tag` 預設 `Chart.appVersion`），所以換版時 CloudNativePG 仍以新映像重建 instance：換到 rc.13 時 Postgres 從 10:49:59 到 10:53:07 UTC 停了約 3 分鐘（「換到 rc.13」）。
- rc.12 到 main 之間 `deploy/istio/istiod-values.yaml` 沒有變，換版前不必對齊 istiod。
- 2026-11-01 換到 rc.13（PR Laysi/trustiot-v2#940）：HelmRelease 升級成功，所有長駐 pod 就緒，閘道器重新連上 broker，遙測資料繼續寫進 ClickHouse，`tunnel.ts check --stage 2` 的 20 項都通過。release 的 `verify` 只有「映像可以匿名拉取」一項失敗（repo 公開期間 GHCR 的套件也是公開的，rc.10 到 rc.12 都一樣），其他檢查都通過。
- 2026-11-03 合併 main 之後重新投影（「合併 main 之後的重新投影」）：main 在 2026-11-02 打開了 SPA 的 `web`（Laysi/trustiot-v2#961，32Mi／256Mi），三個服務的增加量不變（requests +192Mi、limits +1280Mi），節點投影變成 requests 約 2836Mi（37%）、limits 約 11506Mi（152%）。`web` 不用 Postgres，連線預算仍是 236／240。合併後的 values 以 rc.13 的 chart 與 HEAD 的 chart 渲染，輸出逐位元組相同。
- 2026-11-03 打開三個服務（PR Laysi/trustiot-v2#941，main `d649d4bb`；「打開三個服務之後」）：
  - HelmRelease 升級成功，三個服務都就緒，重啟 0 次。
  - 以展示租戶成員的 session，經入口呼叫 `GET /api/assistant/v1/conversations` 與 `/api/telemetry/v1/devices/<設備 ID>/latest`，都回 200。
  - 實際呼叫一次 AI 閘道的 `main` 角色，回 200，由首選的 Opus 5.5 回答。
  - 節點的 requests 是 2836Mi（37%），limits 是 11506Mi（152%），與投影相同。
  - `tunnel.ts check --stage 2` 的 20 項都通過。

## 環境

| 項目 | 內容 |
| --- | --- |
| 展示機 | lab 的 Mac mini M1，k3s `v1.36.5+k3s1`，arm64，8 核、節點 allocatable 記憶體 7724464Ki（約 7543Mi）。展示環境以 Flux 部署（Laysi/trustiot-v2#467） |
| 部署中的 chart | 盤點時 `0.0.0-rc.12+e9b58baf0632`（HelmRelease 最新一筆 `deployed`）；10:54 UTC 起 `0.0.0-rc.13+1028ecbb8632`。2026-11-03 打開三個服務是同一版 chart 的 release v11 |
| 打開的服務 | 盤點時：identity、assets、certs、mqtt-bridge、ingest、gateway-ops 與 MQTT broker，沒有 ai-gateway、telemetry-query、assistant。2026-11-02 加上 `web`（Laysi/trustiot-v2#961），2026-11-03 加上 ai-gateway、telemetry-query 與 assistant |
| lab 鎖 | 盤點時沒有人持有。這次的盤點與兩次確認都只讀，沒有取鎖 |
| chart 的投影 | 工作站以 `.prototools` 釘的 helm `4.3.0` 渲染：main `0627c59a` 的 chart，以及 tag `v0.0.0-rc.12` 的 chart |
| 時段 | 盤點 2026-11-01 10:02–10:12 UTC；換到 rc.13 之後的確認 10:55–11:02 UTC；打開三個服務之後的確認 2026-11-03 05:53–06:08 UTC |

主機名稱、ssh 的 Host 與位址都不寫進報告；節點名稱是主機名稱，在原始資料裡遮成 `<demo-node>`。

## 指令

`DEMO_SSH_HOST` 是工作站 `~/.ssh/config` 裡展示機的 Host。

```sh
# 1. 展示機（只讀）
ssh "$DEMO_SSH_HOST" 'free -h; nproc'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl top node'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl describe node | sed -n "/Allocated resources/,/Events/p"'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl get pods -A --field-selector=status.phase=Running -o jsonpath="{range .items[*]}{.metadata.namespace} {.metadata.name} {range .spec.containers[*]}{.resources.requests.memory}/{.resources.limits.memory};{end}{\"\n\"}{end}"'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl top pods -A --sort-by=memory'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot get helmrelease trustiot -o jsonpath="{.status.history[0].chartVersion} {.status.history[0].status}"'

# 2. chart 的投影（工作站，不連叢集）：打開前（main 的 values-demo.yaml）與打開後（這次的 values-demo.yaml），
#    私有 values 以 .test 的名稱代替。rc.12 的 chart 取自 tag，搭配展示叢集目前的 values-saas.yaml；
#    main 的 chart 搭配 deploy/chart/values-saas.yaml（換版時 deploy PR 會把它複製過去）。
git fetch origin --tags
mkdir -p <dir>/rc12 && git archive v0.0.0-rc.12 deploy/chart | tar -x -C <dir>/rc12
SET='--set-literal hosts.app=app.demo.trustiot.test --set-literal hosts.gateway=gw.demo.trustiot.test --set-literal hosts.ota=ota.demo.trustiot.test --set-literal smtp.from=no-reply@mail.demo.trustiot.test'
git show origin/main:deploy/flux/clusters/demo/trustiot/values-demo.yaml > <dir>/values-demo-before.yaml
helm template trustiot deploy/chart -n trustiot -f deploy/chart/values-saas.yaml -f deploy/flux/clusters/demo/trustiot/values-demo.yaml $SET > <dir>/head-on.yaml
helm template trustiot deploy/chart -n trustiot -f deploy/chart/values-saas.yaml -f <dir>/values-demo-before.yaml $SET > <dir>/head-off.yaml
helm template trustiot <dir>/rc12/deploy/chart -n trustiot -f deploy/flux/clusters/demo/trustiot/values-saas.yaml -f deploy/flux/clusters/demo/trustiot/values-demo.yaml $SET > <dir>/rc12-on.yaml
helm template trustiot <dir>/rc12/deploy/chart -n trustiot -f deploy/flux/clusters/demo/trustiot/values-saas.yaml -f <dir>/values-demo-before.yaml $SET > <dir>/rc12-off.yaml
# 每個 Deployment、StatefulSet 的 containers 的 memory requests、limits 乘上 replicas 相加，比較 on 與 off

# 3. 換版前的 istiod 前提（docs/runbooks/demo-environment.md「換 chart 版本」第 1 步）
git diff v0.0.0-rc.12 origin/main -- deploy/istio/istiod-values.yaml   # 沒有輸出

# 4. 換到 rc.13 之後（只讀；主機名稱照 runbook 從私有的 Cloudflare 變更清單取得）
bun scripts/demo/flux.ts status --ssh-host "$DEMO_SSH_HOST"
DEMO_APP_HOST=<app 主機名稱> DEMO_GATEWAY_HOST=<gateway 主機名稱> DEMO_OTA_HOST=<ota 主機名稱> \
  bun scripts/demo/tunnel.ts check --ssh-host "$DEMO_SSH_HOST" --stage 2
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot get pods -o custom-columns=NAME:.metadata.name,PHASE:.status.phase,START:.status.startTime --sort-by=.status.startTime'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot get events --field-selector involvedObject.name=trustiot-postgres-1 --sort-by=.lastTimestamp'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot logs deploy/trustiot-ingest --since=30m' | grep mqtt
ssh "$DEMO_SSH_HOST" 'free -m'
# lab 的閘道器上
sudo journalctl -u trustiot-agent --since "2026-11-01 10:40:00 UTC"
# ClickHouse 每個表最新的 active part 與列數（parts.sql 的內容在原始輸出裡；system.parts 不受 row policy 影響），
# 隔一分鐘讀兩次。密碼在 pod 裡從檔案讀出，不印出、不離開 pod
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot exec -i trustiot-clickhouse-0 -c clickhouse -- sh -c "clickhouse-client --user \"\$CLICKHOUSE_USER\" --password \"\$(cat \"\$CLICKHOUSE_PASSWORD_FILE\")\" --multiquery"' < parts.sql

# 5. 打開三個服務之後（2026-11-03，只讀；runbook「打開 AI 閘道、telemetry-query 與 assistant」第 2 步）
bun scripts/demo/flux.ts status --ssh-host "$DEMO_SSH_HOST"
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot get deploy trustiot-ai-gateway trustiot-telemetry-query trustiot-assistant; sudo k3s kubectl -n trustiot get jobs'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot get pods -o custom-columns=NAME:.metadata.name,PHASE:.status.phase,READY:.status.containerStatuses[*].ready,RESTARTS:.status.containerStatuses[*].restartCount,START:.status.startTime --sort-by=.status.startTime'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot get configmap trustiot-ai-gateway -o jsonpath="{.data}"'
ssh "$DEMO_SSH_HOST" 'sudo k3s kubectl -n trustiot get httproute trustiot-ingress-app -o jsonpath="{range .spec.rules[*]}{range .matches[*]}{.path.type}:{.path.value} {end}-> {range .backendRefs[*]}{.name}:{.port}{end}{\"\n\"}{end}"'
# 不帶 session：從展示機以 app 主機的 Host 標頭直接送入口的 Service
ssh "$DEMO_SSH_HOST" 'h=$(sudo k3s kubectl -n trustiot get httproute trustiot-ingress-app -o jsonpath="{.spec.hostnames[0]}"); ip=$(sudo k3s kubectl -n trustiot get svc trustiot-ingress-istio -o jsonpath="{.spec.clusterIP}"); curl -sS -o /dev/null -w "%{http_code}\n" -H "Host: $h" "http://$ip/api/assistant/v1/conversations"'
# 模型：runbook「輪替 Resend 與 Anthropic 金鑰」第 3 步的 port-forward，送 services/ai-gateway/README.md「範例：一次最小的呼叫」，角色 main，只留 HTTP 狀態列、ai-gateway-model 與 ai-gateway-outcome
ssh "$DEMO_SSH_HOST" 'free -m; sudo k3s kubectl top node; sudo k3s kubectl top pod -n trustiot; sudo k3s kubectl describe node | sed -n "/Allocated resources/,/Events/p"'
DEMO_APP_HOST=<app 主機名稱> DEMO_GATEWAY_HOST=<gateway 主機名稱> DEMO_OTA_HOST=<ota 主機名稱> \
  bun scripts/demo/tunnel.ts check --ssh-host "$DEMO_SSH_HOST" --stage 2
```

帶 session 經入口的兩個請求，由維護者執行：在瀏覽器以展示租戶的成員登入 app，在同一個頁面打開開發者工具的 Console，執行下面這段只讀的程式。它是同源請求，瀏覽器會帶上 session cookie。程式只印狀態碼、目前租戶的角色、租戶數，以及 M1 錯誤的 code 與 message key，不印 ID 與網址：

```js
(async () => {
  const get = async (p) => {
    const r = await fetch(p, { credentials: "same-origin", cache: "no-store" });
    let b = {};
    try { b = await r.json(); } catch {}
    return [r.status, b];
  };
  const [s, sess] = await get("/api/identity/v1/sessions/current");
  console.log("session", s, "active tenant role:", sess.active_tenant?.role ?? "none", "tenants:", sess.tenants?.length ?? 0);
  const [c, conv] = await get("/api/assistant/v1/conversations");
  console.log("assistant", c, conv.code ?? "", conv.message_key ?? "");
  const [d, dev] = await get("/api/assets/v1/devices");
  const id = dev.devices?.[0]?.device_id;
  if (!id) { console.log("devices", d, "no device"); return; }
  const [t, lat] = await get(`/api/telemetry/v1/devices/${id}/latest`);
  console.log("telemetry", t, lat.code ?? "", lat.message_key ?? "", "points", lat.points?.length ?? 0);
})();
```

runbook 的做法是在工作站上，以 `trustiot-workstation` 的 session 呼叫同樣兩個端點。這次工作站沒有裝這個工具，所以改用瀏覽器。兩種做法都是帶展示租戶成員的 session，經 Access 與入口送出。

## 結果

原始輸出：[inventory.txt](data/2026-11-01-939-demo-ai-services/inventory.txt)。

### 展示機現況

| 項目 | 數值 |
| --- | --- |
| `free -h` | total 7.4Gi、used 4.7Gi、buff/cache 3.1Gi、available 2.7Gi；swap 8.0Gi 用了 887Mi |
| `kubectl top node` | 記憶體 3533Mi（46%）、CPU 630m（7%） |
| 節點的 requests／limits | 記憶體 2612Mi（34%）／9970Mi（132%）；CPU 1640m（20%）／20100m（251%） |
| 用得最多的 pod | ClickHouse 519Mi、Postgres 236Mi、MQTT broker 100Mi、istiod 97Mi；TrustIoT 的 Rust 服務各 5–35Mi |

各 pod 的 requests 與 limits 相加剛好是節點列出的 2612Mi 與 9970Mi，沒有漏算的 pod。

### 打開三個服務的投影

| chart | 新的 Deployment（containers 的 requests／limits） | 長駐 workload 的增加 |
| --- | --- | --- |
| main `0627c59a`（rc.13 會從 main 發） | ai-gateway 32Mi／256Mi、telemetry-query 32Mi／256Mi、assistant 128Mi／768Mi | requests +192Mi、limits +1280Mi |
| `0.0.0-rc.12` | 三個都是 32Mi／256Mi | requests +96Mi、limits +768Mi |

- assistant 在 main 上的 128Mi／768Mi 是 Laysi/trustiot-v2#884 依儀表板的記憶體上限設的（`deploy/chart/values.yaml` 的 `services.assistant.resources`）；rc.12 還是服務的共同預設。
- Postgres 的連線預算：打開後所有服務一起滾動時要 218 條，加上保留的 18 條共 236 條，在 `max_connections` 240 以內，剩 4 條。identity 多一個副本就要 269 條，chart 會拒絕渲染（`bun test deploy/flux` 以這一點當對照）。AI 閘道佔其中 4 × 2 條，assistant 10 × 2 條，telemetry-query 12 × 2 條。
- 三個 Deployment 各有 init container（最多 32Mi／128Mi），只在啟動時執行。migration Job（32Mi／128Mi）每次變更跑一次就結束。
- 只換版（兩邊都是打開前的 values）：長駐 workload 的 requests 與 limits 都 +0Mi。

### 打開後的節點

| 項目 | 打開前 | 打開後（rc.13） | 之後再加 dashboard-check（Laysi/trustiot-v2#881，128Mi／512Mi） |
| --- | --- | --- | --- |
| requests | 2612Mi（34%） | 2804Mi（37%） | 2932Mi（39%） |
| limits | 9970Mi（132%） | 11250Mi（149%） | 11762Mi（156%） |

### 換到 rc.13

原始輸出：[rc13-rollout.txt](data/2026-11-01-939-demo-ai-services/rc13-rollout.txt)（主機名稱、節點名稱、閘道器的主機名稱、deploy key 的指紋、MQTT 的 session ID 與 Cloudflare 的 ray ID 已遮蔽）。

| 項目 | 結果 |
| --- | --- |
| release | tag `v0.0.0-rc.13`（main `0627c59a`）。release.yml 的 plan、build、publish 成功；`verify` 只有一項失敗：13 個映像可以匿名拉取（repo 公開期間 GHCR 的套件也是公開的，rc.10 到 rc.12 相同），其他檢查（簽章、SBOM attestation 等）都通過。`deploy-pr` 因此略過，pin 改由 PR Laysi/trustiot-v2#940 以 `pin-chart.ts --cluster demo` 寫入（main `d2e47c2f`） |
| HelmRelease | 10:54:07 UTC `Ready=True UpgradeSucceeded`（release v9）；`flux.ts status` 的 pulled 與 deployed 都等於 main 的 pin `0.0.0-rc.13@sha256:1028ecbb…` |
| pod | 12 個長駐 pod 都 Running、ready、restarts 0；migration Job `Succeeded` |
| 中斷 | 10:49:59 UTC 服務、broker 與 ClickHouse 的 pod 重建，Postgres 同時停下；Postgres 10:53:07 UTC 以 rc.13 的映像重新啟動，之後 `Cluster in healthy state`。ingest 等 migration 完成，10:53:23 UTC 啟動 |
| 閘道器 | agent 10:49:59 UTC 斷線，重試 3 次（inner mTLS 握手 EOF）後 10:50:10 UTC `connected to the broker`；10:53:29 UTC 再斷一次，10:53:30 UTC 連上，之後沒有錯誤 |
| 資料路徑 | ingest 10:53:23 UTC 重新連上 broker、訂閱 telemetry。ClickHouse 的 `trustiot.telemetry` 在 11:00:47 與 11:01:34 UTC 兩次讀取之間從 5148 列增加到 5150 列，最新的 part 是 11:01:26 UTC |
| 入口 | `tunnel.ts check --stage 2`：工作站與展示機各 10 項，20 項都 PASS；入口的三個位址一致 |
| 記憶體 | `free -m` 的 available 3279Mi，swap 用了 237Mi |

### 合併 main 之後的重新投影（2026-11-03）

PR Laysi/trustiot-v2#941 在 2026-11-03 合併了 main `944073c2`。main 在這之前打開了 SPA 的靜態檔 `web`（Laysi/trustiot-v2#961，2026-11-02 部署在 rc.13 上，[它的測試報告](2026-11-02-961-demo-web.md)），所以以合併後的 values 重算一次。只在工作站渲染（helm `4.3.0`），沒有連展示叢集；節點的數字是 2026-11-01 的盤點加上 chart 的增加量，打開之後以 `kubectl describe node` 的實際數字補進本報告。

渲染（`$SET` 同上方第 2 步，私有 values 以 `.test` 的名稱代替；`values-saas.yaml` 用展示叢集目錄裡的那份，與 HelmRelease 相同）：

```sh
mkdir -p <dir>/rc13 && git archive -o <dir>/rc13.tar 0627c59a deploy/chart && tar -xf <dir>/rc13.tar -C <dir>/rc13
git show origin/main:deploy/flux/clusters/demo/trustiot/values-demo.yaml > <dir>/values-demo-main.yaml
for chart in <dir>/rc13/deploy/chart deploy/chart; do
  for values in <dir>/values-demo-main.yaml deploy/flux/clusters/demo/trustiot/values-demo.yaml; do
    helm template trustiot "$chart" -n trustiot -f deploy/flux/clusters/demo/trustiot/values-saas.yaml -f "$values" $SET
    # 連線預算：max_connections 設得很低，chart 的錯誤訊息寫出需要的連線數
    helm template trustiot "$chart" -n trustiot -f deploy/flux/clusters/demo/trustiot/values-saas.yaml -f "$values" $SET \
      --set postgres.bundled.parameters.max_connections=21
  done
done
# 每個 Deployment、StatefulSet 的 containers 的 memory requests、limits 乘上 replicas 相加
```

| values | chart | Deployment 與 StatefulSet 的記憶體 requests／limits | 連線預算 |
| --- | --- | --- | --- |
| main（`web` 開、三個服務關） | rc.13（main `0627c59a`）與 HEAD | 960Mi／4736Mi | 184／240 |
| 合併後（`web` 與三個服務都開） | rc.13 與 HEAD | 1152Mi／6016Mi | 236／240 |

- 以 rc.13 與 HEAD 的 chart 渲染合併後的 values，輸出逐位元組相同（SHA-256 一致）：rc.13 之後 chart 的變更（入口的登入與健康檢查路徑、限流等）不改這份 values 渲染出的物件。
- 三個服務的增加量與 2026-11-01 相同：requests +192Mi、limits +1280Mi（assistant 128Mi／768Mi，AI 閘道與 telemetry-query 各 32Mi／256Mi）。叢集內 Postgres 的 Cluster 仍是 256Mi／1Gi，不在上表。
- `web` 沒有資料庫角色，所以連線預算與 2026-11-01 相同：所有服務一起滾動時 236 條，在 `max_connections` 240 以內，剩 4 條。
- AI 閘道的 `AI_GATEWAY__ROLES__MAIN__MODELS` 渲染成 `anthropic/claude-opus-5-5,anthropic/claude-sonnet-5-5`：Opus 5.5 為首選、Sonnet 5.5 為備援（維護者 2026-11-03 決定）。
- 以 PR Laysi/trustiot-v2#1013 的 chart（assistant 開啟時要求 AI 閘道也開，而且 `mesh.calls.assistant` 列出 `ai-gateway`）渲染，輸出也與上面相同；同一份 values 把 `services.ai-gateway.enabled` 改成 `false` 時，那個 chart 拒絕渲染。

| 項目 | 2026-11-01 盤點 | 加上 `web`（Laysi/trustiot-v2#961） | 再打開三個服務 | 之後再加 dashboard-check（Laysi/trustiot-v2#881，128Mi／512Mi） |
| --- | --- | --- | --- | --- |
| requests | 2612Mi（34%） | 2644Mi（35%） | 2836Mi（37%） | 2964Mi（39%） |
| limits | 9970Mi（132%） | 10226Mi（135%） | 11506Mi（152%） | 12018Mi（159%） |

百分比是對 allocatable 約 7543Mi。

### 打開三個服務之後（2026-11-03）

原始輸出：[open-2026-11-03.txt](data/2026-11-01-939-demo-ai-services/open-2026-11-03.txt)。已遮蔽 deploy key 的指紋、節點名稱與 Cloudflare 的 ray ID；主機名稱由 `tunnel.ts` 自己遮蔽，或者根本不印。

| 項目 | 結果 |
| --- | --- |
| 合併與套用 | PR Laysi/trustiot-v2#941 以 squash 合併為 main `d649d4bb`，CI 與 smoke 通過。Flux 在 05:53:09 UTC 讀到這個 commit。HelmRelease 05:53:10 開始升級，05:53:36 UTC `Ready=True UpgradeSucceeded`（release v11，chart 仍是 `0.0.0-rc.13+1028ecbb8632`） |
| 三個服務 | `trustiot-ai-gateway`、`trustiot-telemetry-query`、`trustiot-assistant` 都是 1/1 available，pod Running、ready、restarts 0。migration Job `trustiot-migrate-e01767df` 是 Complete（5 秒）。assistant 的 readiness 要兩個資料庫角色都登得進 Postgres，所以它就緒，也表示 Job 建好了角色 |
| 其他服務 | migration Job 的 spec 改變，identity、assets、certs、ingest、gateway-ops、mqtt-bridge、otel collector 與 web 都在 05:53:14 UTC 換了新的 pod，之後全部 Running、ready、restarts 0。Postgres、ClickHouse、MQTT broker 與入口沒有重建；Postgres 的 pod 從 2026-11-01T10:53:01Z 起沒有重啟 |
| AI 閘道 | ConfigMap 的 `AI_GATEWAY__ROLES__MAIN__MODELS` 是 `anthropic/claude-opus-5-5,anthropic/claude-sonnet-5-5`，日誌有 `model role ready`。一次最小的呼叫（角色 `main`，`max_tokens` 16）回 `HTTP/1.1 200`，`ai-gateway-model: anthropic/claude-opus-5-5`，`ai-gateway-outcome: completed`。日誌沒有 `falling back`、WARN 或 ERROR，也沒有金鑰 |
| assistant | 日誌有 `assistant ready`（tools 5、delegation true）。`no dashboard tool: dashboard_check.url is not set` 是預期的，儀表板檢查由 Laysi/trustiot-v2#881 打開 |
| 入口的路由 | app 主機的 HTTPRoute 有 `PathPrefix:/api/telemetry -> trustiot-telemetry-query:8080` 與 `PathPrefix:/api/assistant -> trustiot-assistant:8080`，Accepted、ResolvedRefs、ResolvedWaypoints 都是 True |
| 不帶 session | 從展示機送入口的 Service，兩條路由都回 401 `unauthenticated`。app 主機上沒有路由的路徑也回同樣的 401，因為 cookie 檢查在路由之前。所以這只證明兩條路由在 cookie 檢查後面 |
| 帶 session | 維護者在瀏覽器登入 app，身分是展示租戶的 `admin`，session 只有這一個租戶。以 Console 執行只讀的檢查：`GET /api/assistant/v1/conversations` 回 200；`GET /api/telemetry/v1/devices/<設備 ID>/latest` 回 200，有 2 個資料點 |
| 記憶體 | 06:07 UTC：節點的 requests 是 2836Mi（37%），limits 是 11506Mi（152%），與投影相同。`kubectl top node` 是 3271Mi（43%）。`free -m` 的 available 是 3150Mi，swap 用了 626Mi。三個新 pod 閒置時各用 3–4Mi |
| tunnel | `tunnel.ts check --stage 2`（05:57 UTC）：三個入口位址一致；工作站與展示機各 10 項，20 項都 PASS；`cloudflared.service` 是同一個行程 |

runbook 的前提要在打開前再以 `free -m` 看一次，這次合併前沒有做。打開後的 available 是 3150Mi，比 2026-11-01 盤點時的 2.7Gi 多。

## 結論

**放得下，可以打開**：

- requests 打開後是 allocatable 的 37%，排程沒有問題。
- 實際用量方面，現有的 Rust 服務閒置時各用 5–35Mi。三個新服務打開後閒置時各用 3–4Mi（2026-11-03 的 `kubectl top pod`），打開後 available 還有 3150Mi。
- limits 的超額從 132% 升到 149%（2026-11-03 加上 `web` 後的投影是 152%）。只有很多 pod 同時接近上限時才會用完記憶體，最大的是 ClickHouse 的 2Gi 與 assistant 的 768Mi（儀表板的每個上限同時用滿時）。展示的量（一台閘道器、少數使用者）碰不到。
- 展示機也跑 lab e2e：跑 e2e 之前照 runbook「與 lab e2e 共用叢集」的記憶體一項再看一次 `free -h`。

逐條對照 Laysi/trustiot-v2#939 的驗收條件：

| 驗收條件 | 狀態 |
| --- | --- |
| `v0.0.0-rc.13` 已發佈，release 的 `verify` 通過 | 已發佈；`verify` 除了「映像可以匿名拉取」（repo 公開期間的已知例外）都通過：「換到 rc.13」 |
| 展示叢集的 HelmRelease 部署 rc.13 成功，原有服務與閘道器的資料路徑仍正常 | 通過：「換到 rc.13」 |
| 打開前的記憶體盤點寫進測試報告 | 通過：本報告「結果」 |
| 三個服務就緒、`GET /api/assistant/v1/conversations` 與 `/api/telemetry/v1` 經入口回 200、AI 閘道的角色實際呼叫得到模型 | 通過：「打開三個服務之後」 |
| `bun test deploy/flux` 通過；runbook 與測試報告已更新 | 通過。`bun test deploy/flux`：PR Laysi/trustiot-v2#941 的 CI 通過，合併後的樹也在本機跑過。runbook：步驟隨那個 PR 更新，現況在補上本節的同一個變更裡改成打開後的狀態。測試報告：本節 |
