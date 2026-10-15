# 叢集測試（入口 gateway、服務間授權、日誌管線、叢集內的 Postgres、Postgres 的複寫與備份、ClickHouse 管理帳號密碼）

兩個測試共用 `cluster.ts`（環境變數、`.prototools` 鎖定的 helm 與 kubectl、不碰既有安裝的 `helm install`）：

| 測試 | 驗證 | issue |
| --- | --- | --- |
| `ingress.e2e.ts` | 入口 gateway：cookie 驗證、例外路徑、標頭、路徑繞過、身分服務停機 | Laysi/trustiot-v2#62 |
| `mesh.e2e.ts` | 服務間授權：清單內的呼叫放行、清單外的被拒，AuthorizationPolicy 與 NetworkPolicy 各自生效，預設拒絕下健康檢查通過 | Laysi/trustiot-v2#63 |

`otel.e2e.ts`（日誌管線，Laysi/trustiot-v2#73）、`postgres.e2e.ts`（叢集內的 Postgres，Laysi/trustiot-v2#58）、`postgres-replication.e2e.ts`（叢集內 Postgres 的複寫、備份與還原，Laysi/trustiot-v2#400）與 `clickhouse-admin.e2e.ts`（叢集內 ClickHouse 的管理帳號密碼，Laysi/trustiot-v2#403）不經入口 gateway，有自己的環境變數，不用 `cluster.ts`，見最後四節。

## 入口 gateway（`ingress.e2e.ts`）

`ingress.e2e.ts` 把 chart 裝進現有叢集的一個 namespace，經真正的入口 gateway 驗證決策 27 的規則（Laysi/trustiot-v2#62）：

- 需要 cookie 的路徑：沒有 cookie 或 session 無效回 401；有效 cookie 時後端只看到 1 個 `x-internal-identity`，是身分服務給的值，偽造的值進不來，底線寫法（`x_internal_identity` 等三種）也被移除；`x-forwarded-proto` 為 `https`；`/internal` 沒有 cookie 回 401、有 cookie 回 403。
- cookie 檢查拿到什麼（Laysi/trustiot-v2#77）：`Host` 是客戶端送的原樣，不帶埠號、帶 listener 的埠與帶 8443 都一樣；路徑是 mesh 正規化之後的（`//api/whoami`、`/api/x/../whoami`、`/api%2fwhoami` 等七種寫法，cookie 檢查與後端都看到 `/api/whoami`）。替身把 ext_authz 請求的 `Host` 與路徑放進它回的身分標頭，whoami 原樣列出，測試從那裡讀。
- `/internal` 的 11 種斜線寫法（`INTERNAL_SLASH_CASES`：`//internal`、`///internal`、`//./internal`、`/x/..//internal/…`、`/%2finternal`、`/%2Finternal` 等）：沒有 cookie 回 401、有效 cookie 回 403（Laysi/trustiot-v2#255）。這些案例只在 istiod 設了 `deploy/istio/istiod-values.yaml` 的 `meshConfig.pathNormalization`（`DECODE_AND_MERGE_SLASHES`）時通過；少了它，有效 cookie 的請求會到達全包路由的後端。
- 例外路徑（登入、健康檢查、憑證換發、MQTT）：不用 cookie，偽造的身分標頭與其底線寫法被剝除，後端看到 0 個；`Host` 帶埠號（`:80`、`:443`、`:8443`）仍是例外；在別的主機上（帶不帶埠號）不算例外。
- 路徑繞過：`BYPASS_CASES` 列出 28 種寫法（`..`、`%2e%2e`、`;`、`//`、大小寫、`?`、`%23` 等），沒有 cookie 時全部到不了需要 cookie 的後端。
- 身分服務延遲 3 秒時約 0.5 秒回 503；身分服務停機（scale 到 0）時需要 cookie 的路徑回 503、不依賴身分服務的例外路徑照常 200，恢復後回到 401／200。

不在 `bun test deploy/chart` 內（檔名沒有 `.test.`），要有叢集才能跑。叢集可以是 k3d、k3s 或其他 Kubernetes，只要 kubectl 連得到。

nightly workflow（Laysi/trustiot-v2#69）每晚在 k3d 叢集上以兩種型態各跑一次（`mesh.e2e.ts` 自 2026-10-15 起也每晚以 SaaS 型態跑一次，Laysi/trustiot-v2#77）：裝好 Gateway API 與 Istio ambient、為每個型態的測試命名空間在 istiod 加一個 provider、建置身分服務替身並匯入節點，做法見 [CI 與發佈](../../../../docs/architecture/ci-release.md) 的 nightly.yml 一節與 `scripts/nightly/platform.ts`；那邊請求經 `kubectl port-forward`，不涵蓋入口的 NetworkPolicy。

### 安裝與等待（Laysi/trustiot-v2#399）

2026-10-14 nightly 的 saas ingress e2e 在 `helm install --rollback-on-failure --wait --timeout 5m` 開始約 3 秒後以 `resource Deployment/…/trustiot-identity not ready. status: InProgress, message: Available: 0/1` 失敗並回滾，5 分鐘的上限還差得遠；同樣的程式碼重跑就過。

- **等待方式**：helm 4.3.0 的 `--wait`（`watcher`）在 CLI 預設開 8 個並行算狀態的 worker（`pkg/cmd/flags.go` 的 `cliDefaultStatusComputeWorkers`），為舊版物件算出的狀態可能比新版的晚送達；watcher 在所有資源第一次都是 `Current` 時就停止，結束時回報它最後收到的狀態，所以事後才到的 `InProgress` 會讓等待在幾秒內以失敗結束。改用 `--wait=legacy`（flag 要寫成 `=` 形式，單寫 `--wait` 是 `watcher`）：每 2 秒輪詢，以 Deployment 的就緒條件判斷，沒有事件順序的問題。取捨：legacy 不等 Gateway、HTTPRoute 這類 helm 不認得的種類，也不等 Job（要加 `--wait-for-jobs`）；這兩個測試的 release 沒有 Job（`migrate.enabled: false`），Gateway 與入口 gateway 自己的 Deployment 由測試接著以 `kubectl wait --for=condition=Programmed` 與 `kubectl rollout status` 等。原因的推導與沒有證實的部分見 [CI 與發佈](../../../../docs/architecture/ci-release.md) 的 nightly.yml 一節。
- **失敗時**：不加 `--rollback-on-failure`，因為回滾會刪掉物件，也就刪掉為什麼沒有就緒的證據。安裝失敗時 `install.ts` 先收集 namespace 的事件，以及 Deployment、ReplicaSet、Pod 的狀態（`install-evidence.ts`，另有 `helm history`），再移除這個 release；證據寫在 `E2E_REPORT_DIR/install-failure-<release>.txt`，沒設 `E2E_REPORT_DIR` 時放在錯誤訊息裡。release 名稱已被佔用（helm 回 `cannot reuse a name that is still in use`）時不是這次執行建立的，不收集也不移除。nightly 把這個檔案併進該型態的 diagnostics。
- **證據裡沒有什麼**：repo 是公開的，所以證據只從白名單欄位組成（名稱、副本數、conditions、容器狀態、事件）；Pod 的 spec（環境變數的值、volume）、Secret 與 ConfigMap 不讀，容器的 log 也不收（應用程式可能把憑證寫進 log，無法與可公開的行區分），每個自由文字欄位截成一行、最多 300 字元。剩下的自由文字是 Kubernetes 控制器與 kubelet 寫的（conditions、事件訊息、waiting 的原因）。

### 替身

| 服務 | 替身 | 說明 |
| --- | --- | --- |
| `identity` | `identity-double/`（Bun，自己建映像） | 模擬身分服務的 ext_authz 端點：`sid=valid-…` 允許並回 `x-internal-identity`（`double.` 加上 base64url 的 JSON，內含 ext_authz 請求的 `Host` 與原始路徑），`sid=slow-…` 延遲 3 秒，其他回 401；其餘路徑回 JSON，列出收到的標頭 |
| `assets` | traefik/whoami，以 digest 固定 | 需要 cookie 的後端（`hosts.app` 的 `/` 全包路由），回應帶 `Name: protected-backend` |
| `certs`、`mqtt-bridge` | 同上 | 例外路徑的後端 |

設定在 `values-e2e.yaml`。替身都不用資料庫，所以 overlay 把 `mesh.calls` 的 `identity` 與 `assets` 設成空清單：`values.yaml` 為這兩個服務列了 `postgres`，留著的話 chart 會讓替身讀密碼 Secret 的 key，而這個 overlay 關掉了 migration Job，沒有人填那個 Secret，pod 起不來，`helm install --wait` 就逾時（PR Laysi/trustiot-v2#411 的 smoke，2026-10-14）。`values-otel-e2e.yaml` 對 `assets` 也一樣。`bun test deploy/chart` 的「e2e overlays」以每個叢集測試的安裝方式渲染，檢查沒有 Job 的安裝裡沒有 pod 讀到沒人填的密碼 Secret。真實的身分服務在 `services/identity`（Laysi/trustiot-v2#61），回應表與替身相同（允許時回 200 並帶 `x-internal-identity`、無效回 401、逾時由入口回 503），但 cookie 是 `__Host-trustiot-session`，還會依 `ingress.routes` 決定 token 的受眾、對不到路由回 403，也需要 Postgres 與簽章私鑰；換成真實版本的端到端驗證在 Laysi/trustiot-v2#77。

### 前置條件

1. 叢集已依 `deploy/istio/README.md` 裝好 Istio ambient 與 Gateway API CRD。
2. istiod 有一個 ext_authz provider 指向測試 namespace 的身分服務替身 `trustiot-identity.<namespace>.svc.cluster.local:8080`，設定與 `deploy/istio/istiod-values.yaml` 相同（`pathPrefix: /internal/ext-authz`、`failOpen: false`、`timeout: 0.5s`、`statusOnError: "503"`），mesh 的 `pathNormalization` 也與該檔相同。k3s 以 `HelmChartConfig istiod` 疊加時，清單會整份取代，所以叢集原有的 provider 要原樣重複寫入；範例見 [lab-istiod-helmchartconfig.yaml](../../../../docs/test-reports/data/2026-10-11-255-path-normalization/lab-istiod-helmchartconfig.yaml)。這是整個叢集共用的設定：先備份 `istio-system` 的 ConfigMap `istio`，測完刪掉 `HelmChartConfig`，再確認 `mesh` 與備份逐位元組相同。
3. 測試用的 namespace 已建立，建議設 Pod Security `restricted`。測試的身分要能在這個 namespace 管理 chart 的物件，包含 `gateway.networking.k8s.io` 的 Gateway、HTTPRoute 與 `security.istio.io` 的 AuthorizationPolicy（內建的 `admin` ClusterRole 不含這兩個 API group）。
4. 身分服務替身的映像（arm64 或叢集節點的架構）要在叢集拉得到。例如在節點上建好再匯入 containerd：

   ```sh
   cd deploy/chart/tests/e2e/identity-double
   podman build --platform linux/arm64 -t localhost/trustiot-e2e/identity-double:62 .
   podman save --format oci-archive -o identity-double.tar localhost/trustiot-e2e/identity-double:62
   sudo k3s ctr -n k8s.io images import identity-double.tar   # k3d 用 k3d image import
   # 以 digest 引用時，另外為 digest 加一個名稱：
   sudo k3s ctr -n k8s.io images tag localhost/trustiot-e2e/identity-double:62 localhost/trustiot-e2e/identity-double@sha256:<digest>
   ```

5. 本機有 `proto install` 裝好的 helm 與 kubectl；地端型態還需要 `openssl`（產生一次性的自簽憑證）。

### 執行

在 repo 根目錄：

```sh
E2E_NAMESPACE=e2e-62 \
E2E_IDENTITY_IMAGE=localhost/trustiot-e2e/identity-double:62@sha256:<digest> \
E2E_IMAGE_PULL_POLICY=Never \
E2E_AUTHZ_PROVIDER=<provider 名稱> \
bun test --timeout 600000 ./deploy/chart/tests/e2e/ingress.e2e.ts
```

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `E2E_NAMESPACE` | 必填 | 測試用的 namespace，必須已存在 |
| `E2E_IDENTITY_IMAGE` | 必填 | 身分服務替身的映像，`repository:tag` 或加上 `@sha256:…` |
| `E2E_IMAGE_PULL_POLICY` | `IfNotPresent` | 匯入節點的映像用 `Never` |
| `E2E_AUTHZ_PROVIDER` | `trustiot-identity` | istiod 裡指向替身的 provider 名稱 |
| `E2E_AUTHZ_TIMEOUT_MS` | `500` | 該 provider 的 `timeout`，用來檢查 503 在逾時內回來 |
| `E2E_SHAPE` | `saas` | `saas`（HTTP 80，`tunnel`）或 `onprem`（HTTPS 443，`direct`）；地端型態會建立一次性的 TLS Secret（`trustiot-e2e-tls-<隨機>`），入口 Service 改用 ClusterIP，不佔用節點的 443 |
| `E2E_KUBECONTEXT` | 目前的 context | kubeconfig 裡的 context；kubeconfig 本身用 `KUBECONFIG` |
| `E2E_RELEASE` | `trustiot` | helm release 名稱；provider 的 `service` 要跟著改 |
| `E2E_SSH_JUMP` | 空 | 設定時經 `ssh -L` 由這台主機連入口的 ClusterIP，請求來源與 cloudflared 相同，會經過入口的 NetworkPolicy；空白時用 `kubectl port-forward`，直接進入口 pod，不經過 NetworkPolicy |
| `E2E_REPORT_DIR` | 空 | 設定時把每個請求的結果寫成 `ingress-e2e-<shape>.json`；安裝失敗時另外寫 `install-failure-<release>.txt`（見「安裝與等待」） |
| `E2E_KEEP` | 空 | `1` 時測完不移除 release，方便查看 |

測試以 `helm install --wait=legacy --timeout 5m` 安裝（`install.ts`）；release 已經存在時直接失敗，不碰既有的安裝。為什麼是 `legacy`、為什麼不加 `--rollback-on-failure`，以及失敗時留下什麼，見「安裝與等待」。結束時只移除這次建立的 release 與 TLS Secret（名稱帶隨機字尾）。身分服務停機的測試會暫時把替身 scale 到 0，結束時恢復。請求一律以自寫的 HTTP/1.1 客戶端送出原始的請求行，`fetch` 會先把 `..` 與編碼正規化，測不到繞過。

## 服務間授權（`mesh.e2e.ts`）

`mesh.e2e.ts` 把 chart 裝進現有叢集的一個 namespace，以真實的連線驗證決策 27 的服務間授權（Laysi/trustiot-v2#63，規則見 chart README 的「服務間授權（mesh）」）。呼叫從服務自己的 pod 內發出（`kubectl exec` 在服務的容器裡跑 Bun），所以 ztunnel 看到的是該服務的 ServiceAccount；另外建三個測試 pod 當呼叫端：stranger 在 mesh 內、用 namespace 的 `default` ServiceAccount；outsider 不在 mesh（`istio.io/dataplane-mode: none`）；imposter 在 mesh 內、帶 assets 服務的 selector label 但用 `default` ServiceAccount（readiness 永遠不過，所以不會變成 assets Service 的端點）。

| 呼叫 | 預期 |
| --- | --- |
| assets → certs | 清單內（`values-mesh-e2e.yaml` 的 `mesh.calls`），放行 |
| 入口 gateway → identity、certs、mqtt-bridge（例外路徑，經 `kubectl port-forward` 進入口 pod） | 放行 |
| certs → assets、assets → alerts、mqtt-bridge → identity、stranger → certs、outsider → certs 的 Service 與 pod IP | 清單外，任一層在就被拒 |
| imposter → certs | NetworkPolicy 依 label 放行它，只有 AuthorizationPolicy 擋得住；用來確認 AuthorizationPolicy 是否生效 |
| outsider → 入口 gateway 的 pod | 只有入口的 NetworkPolicy 擋；用來確認 NetworkPolicy 是否生效 |

測試依序執行五個階段，每個階段從前一個階段留下的狀態開始：

1. 兩層都在：清單內的呼叫與入口放行，清單外的與 imposter 全部被拒；所有 pod 就緒、沒有重啟（預設拒絕下 kubelet 的健康檢查照常通過）。
2. 刪掉服務的 AuthorizationPolicy（入口的 cookie 驗證與 `/internal` 拒絕保留）：imposter 通過（確認 AuthorizationPolicy 已失效），清單外的呼叫仍全部被 NetworkPolicy 拒絕，清單內的與入口照常放行。
3. 再刪掉 release 的所有 NetworkPolicy：outsider 連得到入口 pod，清單外的呼叫全部成功，證明前後階段被拒是政策造成的，不是呼叫本身有問題。
4. 以 `helm get manifest` 的內容重建 AuthorizationPolicy、NetworkPolicy 仍刪除：清單外的呼叫與 imposter 全部被 AuthorizationPolicy 拒絕，清單內的與入口照常放行。
5. 重建 NetworkPolicy，回到安裝時的狀態：再檢查一次呼叫與 pod 健康狀態。

只有 NetworkPolicy 的階段排在任何清單外的呼叫成功之前：NetworkPolicy 不會切斷已經建立的連線，而 ztunnel 會保留 HBONE 連線重複使用，所以第 3 階段之後才重建 NetworkPolicy 時，走既有連線的呼叫照樣成功（2026-10-10 在實驗機實測，見 docs/test-reports/2026-10-10-63-mesh-authz.md）。AuthorizationPolicy 由 ztunnel 對每條代理的連線檢查，第 4 階段不受順序影響。

被 ztunnel 拒絕時呼叫端看到連線重置（`ECONNRESET`）；被 NetworkPolicy 擋時，依 NetworkPolicy 控制器的做法是立即拒絕（k3s 是 `ConnectionRefused`，mesh 內的呼叫端經 ztunnel 轉成重置）或逾時。都算被拒，結果檔記錄每次呼叫的種類與毫秒數。

### 前置條件與執行

前置條件同入口測試的 1、3、4、5 項：需要 Istio ambient 與 Gateway API CRD、已建立的 namespace（建議 `restricted`）、叢集拉得到的身分服務替身映像；不需要 ext_authz provider，入口只走例外路徑。測試的身分還要能在 namespace 裡建立 pod、`pods/exec` 與 `pods/portforward`，並刪除與建立 NetworkPolicy 和 AuthorizationPolicy。身分服務替身的映像同時用在五個服務與三個測試 pod，因為它是唯一有執行環境（Bun）、能從 pod 內發出呼叫的測試映像。

在 repo 根目錄：

```sh
E2E_NAMESPACE=e2e-63 \
E2E_IDENTITY_IMAGE=localhost/trustiot-e2e/identity-double:63@sha256:<digest> \
E2E_IMAGE_PULL_POLICY=Never \
bun test --timeout 600000 ./deploy/chart/tests/e2e/mesh.e2e.ts
```

變數同入口測試的 `E2E_NAMESPACE`、`E2E_IDENTITY_IMAGE`、`E2E_IMAGE_PULL_POLICY`、`E2E_KUBECONTEXT`、`E2E_RELEASE`、`E2E_KEEP`，另有：

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `E2E_CALL_TIMEOUT_MS` | `5000` | 一次呼叫多久沒有回應算被拒（NetworkPolicy 丟棄封包時呼叫端只會等到逾時） |
| `E2E_SETTLE_MS` | `90000` | 刪除或重建政策後，等 ztunnel 與 NetworkPolicy 控制器套用的上限 |
| `E2E_REPORT_DIR` | 空 | 設定時把每次呼叫的結果寫成 `mesh-e2e.json` |

結束時只移除這次建立的 release 與三個測試 pod（名稱帶隨機字尾）。中途失敗時，刪掉的政策隨 release 一起移除；`E2E_KEEP=1` 時 release 保留在最後的狀態。

## 通道斷線時的復原（`ingress.e2e.ts` 與 `mesh.e2e.ts`）

測試經 `kubectl port-forward`（或 `E2E_SSH_JUMP` 的 `ssh -L`）連入口 gateway，`mesh.e2e.ts` 的入口呼叫也經 `kubectl port-forward`，兩者共用同一套復原邏輯。這條通道偶爾會在請求進行中斷線：kubectl 印出 `error forwarding port … to pod` 與 `error: lost connection to pod`。2026-10-10 PR Laysi/trustiot-v2#258 的 CI 就發生過一次（run 38075563886 attempt 1，`identity service down`），當時 `raw()` 丟出 `no HTTP response for /api/whoami: ""`，測試失敗。

原因是競態：請求先因連線被關閉而失敗，kubectl 行程要稍晚才結束；舊的 `raw()` 在失敗的當下檢查行程是否已結束，看到還在跑就直接丟出錯誤。現在的做法（`tunnel-recovery.ts`）：

- 請求以連線層錯誤失敗（`raw-client.ts` 的 `ConnectionError`：連線被拒、被重設、在回應標頭之前被關閉、逾時）時，最多等 5 秒讓通道行程結束。
- 行程在這段時間內結束，就重開通道並把請求重送一次；重送只有一次，第二次再失敗就失敗。
- 行程沒有結束，代表是 gateway 那一端真的關掉連線，錯誤照樣丟出，不重試。
- 只要拿到 HTTP 回應（或已收到回應標頭）就不重試，狀態碼不符（例如該是 200 卻得到 503）照樣讓測試失敗；非 `ConnectionError` 的錯誤也直接丟出。

沒有查到可避免 port-forward 斷線的原因（斷線時間點只有一次紀錄，kubectl 沒有說明 pod 端為何關閉連線），所以靠上述復原處理；若之後斷線變得頻繁，再評估改用 k3d 的 LoadBalancer 埠對映取代 port-forward。

邏輯有不需要叢集的單元測試（`tunnel-recovery.test.ts`、`raw-client.test.ts`），由 `bun test deploy/chart` 執行。測試用真正的子行程（`fake-port-forward.ts`，一個 TCP proxy）扮演 kubectl：它在收到請求時切斷連線，300 毫秒後才結束行程，重現上述競態。

給通道行程用的本機埠一律由 `ports.ts` 的 `freePort()` 取得，不要自己 `Bun.listen` 再 `stop`：在 WSL2 上 `stop(true)` 回傳時監聽 socket 還沒關，要等事件迴圈再跑一輪才會關。剛啟動通道就探測該埠的程式會連到這個將死的監聽，誤以為通道已就緒，再把請求送進即將關閉的 socket（`no HTTP response … ""`）。`freePort()` 等到連線被拒絕才回傳（Laysi/trustiot-v2#288，`docs/test-reports/2026-10-11-288-tunnel-recovery-wsl.md`）。

## 日誌管線（`otel.e2e.ts`）

`otel.e2e.ts` 驗證決策 22 的日誌管線（Laysi/trustiot-v2#73）：以地端型態加 `values-otel-e2e.yaml` 安裝 chart（叢集內 ClickHouse、OpenTelemetry collector，`assets` 換成 `crates/service-kit` 的 example-service），另外部署一個替身 SIEM（同一個 collector 映像，只收帶 Bearer token 的 OTLP/HTTP，把收到的資料印到 stdout），然後檢查：

- example-service 處理一個帶 `traceparent` 的請求後，ClickHouse 的 `trustiot_logs.otel_logs` 出現帶同一個 trace id 的結構化紀錄（服務名稱、等級、method、路由、狀態碼），`otel_traces` 有這個請求的 span；請求 body 裡的 email 與密碼不在任何一筆紀錄裡。
- 兩張日誌表都是 MergeTree，從伺服器讀回 `TTL ReceivedAt + toIntervalDay(30)`。
- 替身 SIEM 收到同一個 trace id 的紀錄；不帶標頭送給它時回 401，所以收到的資料是 collector 以 Secret 裡的標頭送來的。
- 直接以 OTLP 送給 collector 含 email、Bearer token、JWT、密碼、電話與 request body 的日誌與 span 後，ClickHouse 與 SIEM 裡的紀錄都已遮蔽（`****`），任何一個原始值都找不到。
- collector 的 pod 在 restricted 的 namespace 裡執行，安全設定符合 restricted Pod Security Standard。

安裝分兩步：先只裝 ClickHouse，以 bootstrap 管理帳號在 pod 內套用 `services/ingest/migrations/clickhouse/` 並建立 collector 的帳號（正式安裝由 migration Job 做這一步，也由它產生 collector 的密碼；這個測試以 `migrate.enabled: false` 關閉 Job，自己做；但 ClickHouse 的 init container 以 migrate 映像產生管理帳號密碼，所以仍需要 migrate 映像），再 `helm upgrade` 開啟 collector、轉送與 example-service。chart 宣告的 collector 密碼 Secret（`<release>-otel-collector-password`，空的）出現後，測試把同一個密碼寫進去，代替 Job 的那一步；Job 本身產生密碼的流程由 `postgres.e2e.ts` 驗證。collector 帳號與 SIEM token 每次執行重新產生，測試自己建立的 Secret 名稱帶隨機字尾，密碼只寫在本機暫存目錄的檔案（權限 0600）與 Secret 裡，不出現在指令列；ClickHouse 管理帳號的密碼由 init container 在叢集內產生（Laysi/trustiot-v2#403），SQL 經 stdin 送進 pod，pod 內的 client 讀 init container 寫的檔案，管理帳號密碼不離開 pod。collector 與 ClickHouse 管理帳號的密碼 Secret 在 uninstall 時保留（`helm.sh/resource-policy: keep`），開始前若已存在就停止。結束時移除 release、ClickHouse 的 PVC、替身 SIEM、這次建立的 Secret 與兩個保留的密碼 Secret。

前置條件：

1. namespace 已建立，建議設 Pod Security `restricted`；不需要 Istio 的 ext_authz provider（測試不經入口 gateway），但 chart 一定會產生入口的 Gateway，叢集要有 Gateway API CRD 與 Istio。測試的身分要能在這個 namespace 管理 chart 的物件，並能 `exec` 進 pod 與 `port-forward`。
2. example-service 的映像（叢集節點的架構，實驗機是 arm64）要在叢集拉得到。aarch64 的執行檔在本機交叉編譯（musl、靜態連結，只用 Rust 工具鏈內附的 `rust-lld`），再在節點上建映像匯入 containerd：

   ```sh
   rustup target add aarch64-unknown-linux-musl
   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld \
   CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes" \
     cargo build --locked --release --target aarch64-unknown-linux-musl -p service-kit --bin example-service
   # 把 target/aarch64-unknown-linux-musl/release/example-service 與 example-service/Containerfile 放進同一個目錄，在節點上：
   podman build --platform linux/arm64 -t localhost/trustiot-e2e/example-service:73 .
   podman save --format oci-archive -o example-service.tar localhost/trustiot-e2e/example-service:73
   sudo k3s ctr -n k8s.io images import example-service.tar
   ```

3. migrate 映像（`deploy/images/migrate`）要在叢集拉得到，建法見「叢集內的 Postgres」；tag 不必和 example-service 相同（Job 關閉時沒有這個限制）。
4. collector 與 ClickHouse 的映像以 `values.yaml` 的 digest 從 Docker Hub 拉取。
5. 本機有 `proto install` 裝好的 helm 與 kubectl。

執行（在 repo 根目錄）：

```sh
E2E_NAMESPACE=e2e-73 \
E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:73 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:73 \
E2E_IMAGE_PULL_POLICY=Never \
bun test --timeout 900000 ./deploy/chart/tests/e2e/otel.e2e.ts
```

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `E2E_NAMESPACE` | 必填 | 測試用的 namespace，必須已存在 |
| `E2E_SERVICE_IMAGE` | 必填 | example-service 的映像，`repository:tag` 或加上 `@sha256:…` |
| `E2E_MIGRATE_IMAGE` | 必填 | migrate 映像，ClickHouse 的 init container 用它產生管理帳號密碼 |
| `E2E_IMAGE_PULL_POLICY` | `IfNotPresent` | 匯入節點的映像用 `Never` |
| `E2E_KUBECONTEXT` | 目前的 context | kubeconfig 裡的 context |
| `E2E_RELEASE` | `trustiot` | helm release 名稱；已經存在時直接失敗 |
| `E2E_REPORT_DIR` | 空 | 設定時把查到的紀錄、TTL 與 SIEM 的摘錄寫成 `otel-e2e.json` |
| `E2E_KEEP` | 空 | `1` 時測完不移除任何東西，方便查看 |

## 叢集內的 Postgres（`postgres.e2e.ts`）

`postgres.e2e.ts` 驗證 `postgres.mode: bundled`（Laysi/trustiot-v2#58，決策 21、29，清單 M3、M8）與 migration Job 產生的密碼（Laysi/trustiot-v2#60）：以 `E2E_SHAPE` 的型態加 `values-postgres-e2e.yaml`，一次 `helm install` 裝好 CloudNativePG 的 Cluster、叢集內的 ClickHouse、collector（不轉送）、migration Job 與 `identity`（由 `crates/service-kit` 的 example-service 扮演；identity 在 `mesh.calls` 列了 `postgres`，所以 chart 給它角色 `identity_app` 與 Job 產生的密碼），關掉 Sentry，然後檢查：

- Cluster 達到 `Cluster in healthy state`，一個 instance，pod 帶 `istio.io/dataplane-mode: none`，每個容器（含 init container）符合 restricted Pod Security Standard 的要求。
- 伺服器：`ssl` 開啟、`shared_preload_libraries` 含 `age`、PostgreSQL 18、`max_connections` 與 `shared_buffers` 是 values 的值；`trustiot` 資料庫的 AGE 版本等於 `deploy/images/postgres/Containerfile` 的 `AGE_VERSION`，locale 是內建的 `C.UTF-8`，owner 是 `trustiot_owner`；`postgres` 沒有密碼、`<cluster>-superuser` Secret 不存在；owner 與服務的角色都不是 superuser，也沒有 CREATEDB、BYPASSRLS；服務的角色沒有 CREATEROLE，owner 有（bootstrap 的 `postInitApplicationSQL`，migration Job 以它建立服務的角色，Laysi/trustiot-v2#60）。
- cypher() 在這個資料庫建立、查詢並刪除一張圖。
- 從 pod 內以 TCP、`sslmode=disable` 連線被伺服器的 pg_hba 拒絕（`no encryption`）。
- 叢集的其他資料庫（Laysi/trustiot-v2#400）：`postgres` 與 `template1` 的 ACL 只剩 superuser，`postgres` 另外只有 `streaming_replica` 與 `cnpg_metrics_exporter` 的 `CONNECT`；superuser 與 `pg_` 開頭的角色以外，每個角色（owner、Job 建立的角色、operator 的角色）在兩個資料庫都沒有 `CONNECT` 與 `TEMPORARY`，只有 operator 的兩個角色有 `postgres` 的 `CONNECT`。`identity_app` 以 Job 產生的密碼（經 stdin 送進 primary pod，不在指令列上）從 pod 內以 TLS 連這兩個資料庫被拒（`permission denied for database`、`User does not have CONNECT privilege.`），連 `trustiot` 成功；Job 的日誌沒有 `may connect to it` 的警告。
- ClickHouse 的 init container 把管理帳號密碼產生進 `<release>-clickhouse-admin-password`（key 是帳號名稱、annotation `trustiot.io/password-generation` 是 `1`、`keep`，值是 64 個十六進位字元），Job 經 API 讀到它並以它連上 ClickHouse。
- Job 把密碼產生進 chart 宣告的空 Secret（`<release>-identity-password`、`<release>-otel-collector-password`）：每個 Secret 只有該有的 key、annotation `trustiot.io/password-generation` 是 `1`、帶 `helm.sh/resource-policy: keep`，`data` 只由 field manager `trustiot-migrate` 寫入；值是 64 個十六進位字元；Job 的日誌寫出產生了哪個角色的密碼，不含任何值。collector Ready。
- example-service 以 Job 建立的角色 `identity_app`、chart 掛載的 CA 與 `<cluster>-rw.<namespace>.svc` 連線：pod Ready 代表 readiness 的 Postgres 檢查以 verify-full 成功，伺服器端看得到這個角色的 TLS 連線。
- 同一個 pod template 的獨立 pod：不改的對照組 Ready；CA 換成叢集的根 CA（`kube-root-ca.crt`）時失敗（`UnknownIssuer`）；主機改成 primary 的 pod IP 時失敗（`not valid for name`）；換掉 `app.kubernetes.io/component` label、不在 `mesh.calls` 的 pod 連不上（NetworkPolicy 擋下，連線到不了 Postgres，ambient 下由 pod 的 ztunnel 重置，沒交換任何憑證或密碼）。
- 同樣的 values 再 `helm upgrade`：Job 名稱不變、不再執行，密碼 Secret 的內容不變；`helm rollback` 回第一版後也不變。
- `migrate.passwordGeneration=2`：新的 Job 換掉每個密碼（annotation 變成 `2`），日誌寫出設定了角色與 collector 帳號的新密碼，identity 與 collector 以新密碼滾動更新後 Ready。
- 刪掉 Secret 裡的 key 後以 `migrate.revision=2` 重跑 Job：key 重新產生，服務以新密碼 Ready。
- `helm uninstall` 後 Cluster、ImageCatalog、PVC 與兩個密碼 Secret 都還在，資料也在；Database 物件隨 release 刪除，資料庫保留。

密碼的比較只在測試的行程裡做（`data` 的 SHA-256），輸出與報告只有 `unchanged` 或 `changed`，不印任何值或 digest。

測試不建立任何 Secret：ClickHouse 的管理帳號密碼由它的 init container 產生（Laysi/trustiot-v2#403），服務與 collector 的密碼全由 Job 產生。測試查資料庫的 SQL 經 stdin 送進 primary，以 `postgres` 角色走 pod 內的 local socket 執行，沒有任何密碼。開始前，同名的 Cluster、ImageCatalog、它們的 PVC、ClickHouse 的 PVC 或三個密碼 Secret 已經存在（之前 uninstall 保留下來的）時直接失敗，不碰它們。結束時刪除測試的 pod、NetworkPolicy、release，再明確刪除保留下來的密碼 Secret、ClickHouse 的 PVC、Cluster、ImageCatalog 與 PVC。

前置條件：

1. 叢集已依 `deploy/cloudnative-pg/README.md` 裝好 CloudNativePG operator，並有 Istio ambient 與 Gateway API CRD（chart 一定會產生入口的 Gateway；測試不經入口）。
2. namespace 已建立，設 Pod Security `restricted`。測試的身分要能在這個 namespace 管理 chart 的物件（含 `postgresql.cnpg.io` 的 Cluster、ImageCatalog、Database，`gateway.networking.k8s.io` 的 Gateway、HTTPRoute 與 `security.istio.io` 的 AuthorizationPolicy，內建的 `admin` ClusterRole 都不含；還有 migration Job 與 ClickHouse 的 Role 與 RoleBinding，它們只給 Job 讀寫兩個密碼 Secret、讀 ClickHouse 的管理帳號 Secret，給 ClickHouse 讀寫它自己的那一個），並能 `exec` 進 pod、讀 pod 與 Job 的 log、讀 Secret。
3. Postgres 映像（`deploy/images/postgres`）、example-service 的映像（建法見「日誌管線」）與 migrate 映像（`deploy/images/migrate`）要在叢集拉得到；example-service 與 migrate 的映像 tag 要相同（chart 要求用 Postgres 的服務與 Job 同一個版本）。Postgres 映像沒有 `RUN` 在 arm64 階段，x86 主機可以不靠模擬器建出 arm64 映像，再匯入節點：

   ```sh
   docker buildx build --platform linux/arm64 --load -t localhost/trustiot-e2e/postgres:58 -f deploy/images/postgres/Containerfile deploy/images/postgres
   docker save localhost/trustiot-e2e/postgres:58 -o postgres.tar
   sudo k3s ctr -n k8s.io images import postgres.tar   # 在節點上；k3d 用 k3d image import
   ```

   example-service 依賴的 `ring` 要編譯 C 程式碼，主機沒有 aarch64 musl 的 C 編譯器時，上面「日誌管線」的 `rust-lld` 做法會失敗；改用 `.prototools` 鎖定的 zig 與 cargo-zigbuild（`proto install zig cargo-zigbuild`，兩者的執行檔目錄要在 `PATH`），映像同樣以 `docker buildx build --platform linux/arm64` 建出：

   ```sh
   cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p service-kit --bin example-service
   ```

   migrate 映像同樣做法：`cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p migrate --bin migrate`，再以 `deploy/images/rust-binary.Containerfile`（把 `@BIN@` 換成 `migrate`）建出 arm64 映像、匯入節點。

4. 本機有 `proto install` 裝好的 helm 與 kubectl。

執行（在 repo 根目錄）：

```sh
E2E_NAMESPACE=e2e-60 \
E2E_SHAPE=onprem \
E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:60 \
E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:60 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:60 \
E2E_IMAGE_PULL_POLICY=Never \
bun test --timeout 900000 ./deploy/chart/tests/e2e/postgres.e2e.ts
```

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `E2E_NAMESPACE` | 必填 | 測試用的 namespace，必須已存在 |
| `E2E_POSTGRES_IMAGE` | 必填 | 含 AGE 的 Postgres 映像，`repository:tag` 或加上 `@sha256:…` |
| `E2E_SERVICE_IMAGE` | 必填 | example-service 的映像 |
| `E2E_MIGRATE_IMAGE` | 必填 | migrate 映像，tag 要與 `E2E_SERVICE_IMAGE` 相同 |
| `E2E_SHAPE` | `onprem` | `onprem` 或 `saas`，決定前兩個 values 檔 |
| `E2E_IMAGE_PULL_POLICY` | `IfNotPresent` | 三個映像都用這個值；匯入節點的映像用 `Never` |
| `E2E_KUBECONTEXT` | 目前的 context | kubeconfig 裡的 context |
| `E2E_RELEASE` | `trustiot` | helm release 名稱；release、或同名的 Cluster、ImageCatalog、PVC 與密碼 Secret（之前 uninstall 保留下來的）已經存在時直接失敗，不碰它們 |
| `E2E_REPORT_DIR` | 空 | 設定時把伺服器設定、角色、密碼 Secret 的形狀（名稱、key、世代，沒有值）、拒絕訊息、`unchanged`/`changed` 與保留的物件寫成 `postgres-e2e-<shape>.json` |
| `E2E_KEEP` | 空 | `1` 時測完不移除任何東西，方便查看 |

## 叢集內 Postgres 的複寫、備份與還原（`postgres-replication.e2e.ts`）

`postgres-replication.e2e.ts` 驗證收回 `postgres` 與 `template1` 的 `PUBLIC` 權限之後（Laysi/trustiot-v2#400，`files/postgres/system-databases.sql`），CloudNativePG 自己的連線照常運作。它以地端型態 `helm template --show-only templates/postgres.yaml` 渲染 chart 的 ImageCatalog、Cluster 與 NetworkPolicy，**改過 Cluster 再套用**：`instances: 2`、以 Barman Cloud plugin 當 WAL archiver（`spec.plugins`）、較小的資源（requests `50m`／`192Mi`、limit `512Mi`）與 1Gi 的磁碟區；其他欄位（bootstrap 與 `postInitSQL`、pg_hba、image catalog、不開 superuser）都是 chart 的，報告的 `clusterChanges` 列出改了什麼、沒改什麼。chart 本身維持 `instances: 1`、沒有備份設定（備份是 Laysi/trustiot-v2#134）。chart 的 NetworkPolicy 只放行服務與 migration Job，所以測試另外加一個只讓這個 Cluster 的 instance 互連 5432 的 NetworkPolicy。不裝其他元件，也不跑 migration Job（Job 在 `postgres.e2e.ts` 驗證）。依序檢查：

- 新 bootstrap 的 Cluster：`postgres` 與 `template1` 的 ACL 只剩 superuser 與 operator 的兩個角色；superuser 與 `pg_` 開頭的角色以外，每個角色（owner、以 owner 的 CREATEROLE 照 Job 的方式建立的 `e2e_app`、operator 的角色）在兩個資料庫都沒有 `CONNECT` 與 `TEMPORARY`，只有 `streaming_replica` 與 `cnpg_metrics_exporter` 有 `postgres` 的 `CONNECT`；`trustiot` 仍讓 `PUBLIC` 連線；operator 接手 `postInitSQL` 先建好的兩個角色後，屬性與它自己建立的相同（`cnpg_metrics_exporter` 是 `pg_monitor` 的成員）。
- 複寫：primary 的 `pg_stat_replication` 有 `streaming_replica` 的 `streaming`，replica 的 `pg_stat_wal_receiver` 是 `streaming`，primary 寫入的列出現在 replica。
- switchover：把 Cluster 的 `status.targetPrimary` 設成 replica（`kubectl cnpg promote` 與 CloudNativePG 自己的 e2e 測試的做法）；新 primary 生效、Cluster 回到兩個 instance 健康，前 primary 以 replica 身分從新 primary 串流，新 primary 寫入的列出現在它上面。前 primary 的日誌有 `Starting up pg_rewind`，以及 `Done!` 或 `no rewind required`（`pg_rewind` 以 `streaming_replica` 連新 primary 的 `postgres`），沒有權限錯誤；報告的 `switchover.rewind` 列出這些日誌。
- 備份：Cluster 的 `ContinuousArchiving` 條件是 `True`，`method: plugin` 的 Backup 達到 `completed`。
- 還原：新的 Cluster `<release>-postgres-restored`（一個 instance）經 plugin 從這份備份 bootstrap（`bootstrap.recovery` 與 `externalClusters[].plugin`），有原本的資料，兩個資料庫的 ACL 與角色權限和原 Cluster 相同，instance manager 的 metrics 沒有錯誤；檢查完就刪掉它，把記憶體還給節點。
- instance manager：最後每個 instance 都 Ready，metrics（經 API server 對 pod 9187 的 proxy）的 `cnpg_collector_up` 是 1、`cnpg_collector_last_collection_error` 是 0，每個 instance 的日誌沒有 `permission denied for database`、`CONNECT privilege` 或 `permission denied to create temporary`。

物件儲存是同一個 namespace 裡的 RustFS（`docker.io/rustfs/rustfs:1.0.0-glibc@sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`，可用 `E2E_STORE_IMAGE` 換掉）。選它的理由：Barman Cloud plugin v0.15.1 自己的 e2e 測試就是對這個映像與 digest 跑的（plugin repo 的 `test/e2e/internal/objectstore/s3.go`），barman-cloud 的 S3 用戶端與它的相容性有上游驗證；它持續維護、Apache-2.0、有 arm64 映像。MinIO 社群版自 2025 年起不再發佈映像，Garage 與 versitygw 不在 plugin 的測試範圍內。資料放在 emptyDir，隨 pod 刪除。存取金鑰是這個行程產生的隨機值，只寫進測試建立的 Secret，不印出、不寫進報告。決策 20 的產品物件儲存不用 RustFS，這裡只是測試用的伺服器。

開始前同名的 Cluster、ImageCatalog、chart 的 NetworkPolicy `<release>-postgres` 或 Cluster 的 PVC 已經存在時直接失敗，不碰它們。結束時刪除還原的 Cluster、Backup、Cluster、ImageCatalog、chart 的 NetworkPolicy、PVC、測試的 NetworkPolicy、ObjectStore 與物件儲存。

前置條件：

1. 平台元件（叢集層級，由平台維運安裝，不由測試安裝）：CloudNativePG 1.30.1（`deploy/cloudnative-pg/README.md`）、cert-manager v1.21.2（plugin 的 manifest 需要它的 Issuer 與 Certificate），以及 Barman Cloud plugin v0.15.1（官方 release 的 `manifest.yaml`：Deployment `barman-cloud` 在 `cnpg-system`、CRD `objectstores.barmancloud.cnpg.io`，映像 `ghcr.io/cloudnative-pg/plugin-barman-cloud:v0.15.1`）。我們的 Postgres 映像以 CloudNativePG 的 `minimal` 映像為基底，沒有 barman-cloud，所以備份與還原一律經 plugin 的 sidecar。lab 安裝時的版本與 manifest 的 SHA-256 記在測試報告。
2. namespace 已建立，設 Pod Security `restricted`。測試的身分要能在這個 namespace 管理 Cluster、ImageCatalog、Backup（`postgresql.cnpg.io`）、ObjectStore（`barmancloud.cnpg.io`）、Deployment、Service、Secret、NetworkPolicy 與 PVC，能 patch Cluster 的 `status`（switchover），能 `exec` 進 pod、讀 pod 的 log，並能經 API server 的 proxy 讀 pod 的 9187（`pods/proxy`）。
3. Postgres 映像（建法見「叢集內的 Postgres」）要在叢集拉得到，或以 `latest` 以外的 tag 匯入節點。Cluster 的 `imagePullPolicy` 也套用在 CloudNativePG 加進每個 instance 的容器（operator 的 bootstrap-controller、plugin 的 sidecar `ghcr.io/cloudnative-pg/plugin-barman-cloud-sidecar:v0.15.1`），所以這個測試固定用 `IfNotPresent`，不讀 `E2E_IMAGE_PULL_POLICY`；sidecar 與 RustFS 的映像由節點自己拉。
4. 本機有 `proto install` 裝好的 helm 與 kubectl。

執行（在 repo 根目錄）：

```sh
E2E_NAMESPACE=e2e-400 \
E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:400 \
bun test --timeout 2400000 ./deploy/chart/tests/e2e/postgres-replication.e2e.ts
```

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `E2E_NAMESPACE` | 必填 | 測試用的 namespace，必須已存在 |
| `E2E_POSTGRES_IMAGE` | 必填 | 含 AGE 的 Postgres 映像，`repository:tag` 或加上 `@sha256:…` |
| `E2E_STORE_IMAGE` | 上面的 RustFS | S3 相容伺服器的映像，要接受 RustFS 的環境變數 |
| `E2E_KUBECONTEXT` | 目前的 context | kubeconfig 裡的 context |
| `E2E_RELEASE` | `trustiot` | 渲染 chart 的 release 名稱，決定 Cluster 的名稱 |
| `E2E_REPORT_DIR` | 空 | 設定時把每一步的結果（ACL、權限、複寫狀態、`pg_rewind` 的日誌、Backup 的狀態、還原後的資料與權限、metrics、時間線；沒有任何金鑰）寫成 `postgres-replication-e2e.json` |
| `E2E_KEEP` | 空 | `1` 時測完不移除任何東西，方便查看 |

## 叢集內 ClickHouse 的管理帳號密碼（`clickhouse-admin.e2e.ts`）

`clickhouse-admin.e2e.ts` 驗證叢集內 ClickHouse 的 bootstrap 管理帳號密碼在叢集內產生（Laysi/trustiot-v2#403）：以地端型態加 `values-otel-e2e.yaml` 只安裝 ClickHouse（collector、服務與 migration Job 都關掉，不需要 Postgres 或 CloudNativePG），然後依序檢查：

- 舊的安裝方式：`clickhouse.bundled.adminSecret.name` 指向測試建立的 Secret 時，ClickHouse 接受那個密碼，沒有 init container，也沒有產生的 Secret。
- 搬移：拿掉 `adminSecret` 再 `helm upgrade`：ClickHouse 重啟，init container `admin-password` 以 0 結束，Secret `<release>-clickhouse-admin-password` 只有帳號名稱一個 key、值是 64 個十六進位字元、代是 `1`、綁定（`trustiot.io/password-secret-uid` 等於它的 uid）、帶 `keep` 與 `trustiot.io/filled-by: migrate`；init container 的日誌寫出產生了密碼、不含密碼；新密碼被接受，舊密碼被拒（`Code 516`）。刪掉舊 Secret 再重啟 pod，密碼不變、日誌沒有再產生。
- 輪替：`adminPasswordGeneration=2` 升級後 Secret 換成代 `2` 的新密碼，新密碼被接受、前一個被拒。
- rollback 回輪替前的版本：密碼與代都不變；再升級回代 `2` 也不變。
- 權限：只有 ClickHouse 的 ServiceAccount 能 `get`、`patch` 這個 Secret；chart 的其他 ServiceAccount 與 namespace 的 `default` 都不能，所有人都不能 `list`、`create` Secret（`kubectl auth can-i --as`）。

密碼只在測試的行程裡比較（SHA-256），送進 pod 時經 stdin；輸出與報告只有 `unchanged`、`changed` 與 ClickHouse 的錯誤碼。開始前同名的 release、產生的 Secret 或 ClickHouse 的 PVC 已存在時直接失敗；結束時移除 release、PVC、產生的 Secret 與測試建立的舊 Secret。

前置條件：

1. namespace 已建立，設 Pod Security `restricted`；叢集要有 Gateway API CRD 與 Istio（chart 一定會產生入口的 Gateway）。測試的身分要能在這個 namespace 管理 chart 的物件（含 Gateway、HTTPRoute 與 AuthorizationPolicy）、Role 與 RoleBinding，讀 pod 的 log 與 Secret、`exec` 進 pod，並能以 `--as` 模擬這個 namespace 的 ServiceAccount（內建的 `admin` ClusterRole 都有）。
2. migrate 映像要在叢集拉得到，建法見「叢集內的 Postgres」。
3. 本機有 `proto install` 裝好的 helm 與 kubectl。

執行（在 repo 根目錄）：

```sh
E2E_NAMESPACE=e2e-403 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:403 \
E2E_IMAGE_PULL_POLICY=Never \
bun test --timeout 900000 ./deploy/chart/tests/e2e/clickhouse-admin.e2e.ts
```

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `E2E_NAMESPACE` | 必填 | 測試用的 namespace，必須已存在 |
| `E2E_MIGRATE_IMAGE` | 必填 | migrate 映像，`repository:tag` 或加上 `@sha256:…` |
| `E2E_IMAGE_PULL_POLICY` | `IfNotPresent` | 匯入節點的映像用 `Never` |
| `E2E_KUBECONTEXT` | 目前的 context | kubeconfig 裡的 context |
| `E2E_RELEASE` | `trustiot` | helm release 名稱 |
| `E2E_REPORT_DIR` | 空 | 設定時把每一步的結果（沒有任何值或 digest）寫成 `clickhouse-admin-e2e.json` |
| `E2E_KEEP` | 空 | `1` 時測完不移除任何東西，方便查看 |
