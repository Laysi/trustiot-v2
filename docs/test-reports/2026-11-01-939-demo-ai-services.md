# 展示環境打開 AI 閘道、telemetry-query 與 assistant：打開前的記憶體盤點（2026-11-01）

對應 issue：Laysi/trustiot-v2#939（父 issue Laysi/trustiot-v2#456）。這份報告先記錄打開三個服務之前的唯讀盤點與 chart 的投影；換到 `0.0.0-rc.13` 與打開之後的確認，在兩個 PR 合併後補進同一份報告。

結果先講：

- 展示機放得下三個服務：打開後節點的記憶體 requests 從 2612Mi（34%）到 2804Mi（37%），limits 從 9970Mi（132%）到 11250Mi（149%）；`free -h` 的 available 是 2.7Gi。
- 只換版（rc.12 到 main 上的 chart）不改任何長駐 workload 的 requests 與 limits，叢集內 Postgres 的 `max_connections` 也不變（兩版都是 240），不會因此重啟。
- rc.12 到 main 之間 `deploy/istio/istiod-values.yaml` 沒有變，換版前不必對齊 istiod。
- `v0.0.0-rc.13` 在盤點時還沒有發佈，所以換版與打開後的確認都是「待測」。

## 環境

| 項目 | 內容 |
| --- | --- |
| 展示機 | lab 的 Mac mini M1，k3s `v1.36.5+k3s1`，arm64，8 核、節點 allocatable 記憶體 7724464Ki（約 7543Mi）。展示環境以 Flux 部署（Laysi/trustiot-v2#467） |
| 部署中的 chart | `0.0.0-rc.12+e9b58baf0632`（HelmRelease 最新一筆 `deployed`） |
| 打開的服務 | identity、assets、certs、mqtt-bridge、ingest、gateway-ops 與 MQTT broker；沒有 ai-gateway、telemetry-query、assistant |
| lab 鎖 | 盤點時沒有人持有；這次只讀，沒有取鎖 |
| chart 的投影 | 工作站以 `.prototools` 釘的 helm `4.3.0` 渲染：main `0627c59a` 的 chart，以及 tag `v0.0.0-rc.12` 的 chart |
| 時段 | 2026-11-01 10:02–10:12 UTC |

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
```

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

## 結論

**放得下，可以打開**：

- requests 打開後是 allocatable 的 37%，排程沒有問題。
- 實際用量方面，現有的 Rust 服務閒置時各用 5–35Mi，預期三個新服務閒置時也在這個量級（打開後以 `kubectl top pods` 確認，補進本報告）；available 2.7Gi 還有很多空間。
- limits 的超額從 132% 升到 149%。只有很多 pod 同時接近上限時才會用完記憶體，最大的是 ClickHouse 的 2Gi 與 assistant 的 768Mi（儀表板的每個上限同時用滿時）。展示的量（一台閘道器、少數使用者）碰不到。
- 展示機也跑 lab e2e：跑 e2e 之前照 runbook「與 lab e2e 共用叢集」的記憶體一項再看一次 `free -h`。

逐條對照 Laysi/trustiot-v2#939 的驗收條件：

| 驗收條件 | 狀態 |
| --- | --- |
| `v0.0.0-rc.13` 已發佈，release 的 `verify` 通過 | 待測：盤點時（2026-11-01 10:02 UTC）tag 還沒推，由維護者發佈 |
| 展示叢集的 HelmRelease 部署 rc.13 成功，原有服務與閘道器的資料路徑仍正常 | 待測：要先有 rc.13 與它的換版 PR |
| 打開前的記憶體盤點寫進測試報告 | 通過：本報告「結果」 |
| 三個服務就緒、`GET /api/assistant/v1/conversations` 與 `/api/telemetry/v1` 經入口回 200、AI 閘道的角色實際呼叫得到模型 | 待測：要在換版之後合併打開的 PR |
| `bun test deploy/flux` 通過；runbook 與測試報告已更新 | 打開的變更在 `bun test deploy/flux` 通過，runbook 已更新；換版與打開後的確認補進本報告 |
