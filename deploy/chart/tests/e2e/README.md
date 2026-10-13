# 叢集測試（入口 gateway、服務間授權、日誌管線、叢集內的 Postgres）

兩個測試共用 `cluster.ts`（環境變數、`.prototools` 鎖定的 helm 與 kubectl、不碰既有安裝的 `helm install`）：

| 測試 | 驗證 | issue |
| --- | --- | --- |
| `ingress.e2e.ts` | 入口 gateway：cookie 驗證、例外路徑、標頭、路徑繞過、身分服務停機 | Laysi/trustiot-v2#62 |
| `mesh.e2e.ts` | 服務間授權：清單內的呼叫放行、清單外的被拒，AuthorizationPolicy 與 NetworkPolicy 各自生效，預設拒絕下健康檢查通過 | Laysi/trustiot-v2#63 |

`otel.e2e.ts`（日誌管線，Laysi/trustiot-v2#73）與 `postgres.e2e.ts`（叢集內的 Postgres，Laysi/trustiot-v2#58）不經入口 gateway，有自己的環境變數，不用 `cluster.ts`，見最後兩節。

## 入口 gateway（`ingress.e2e.ts`）

`ingress.e2e.ts` 把 chart 裝進現有叢集的一個 namespace，經真正的入口 gateway 驗證決策 27 的規則（Laysi/trustiot-v2#62）：

- 需要 cookie 的路徑：沒有 cookie 或 session 無效回 401；有效 cookie 時後端只看到 1 個 `x-internal-identity`，是身分服務給的值，偽造的值進不來，底線寫法（`x_internal_identity` 等三種）也被移除；`x-forwarded-proto` 為 `https`；`/internal` 沒有 cookie 回 401、有 cookie 回 403。
- `/internal` 的 11 種斜線寫法（`INTERNAL_SLASH_CASES`：`//internal`、`///internal`、`//./internal`、`/x/..//internal/…`、`/%2finternal`、`/%2Finternal` 等）：沒有 cookie 回 401、有效 cookie 回 403（Laysi/trustiot-v2#255）。這些案例只在 istiod 設了 `deploy/istio/istiod-values.yaml` 的 `meshConfig.pathNormalization`（`DECODE_AND_MERGE_SLASHES`）時通過；少了它，有效 cookie 的請求會到達全包路由的後端。
- 例外路徑（登入、健康檢查、憑證換發、MQTT）：不用 cookie，偽造的身分標頭與其底線寫法被剝除，後端看到 0 個；`Host` 帶埠號（`:80`、`:443`、`:8443`）仍是例外；在別的主機上（帶不帶埠號）不算例外。
- 路徑繞過：`BYPASS_CASES` 列出 28 種寫法（`..`、`%2e%2e`、`;`、`//`、大小寫、`?`、`%23` 等），沒有 cookie 時全部到不了需要 cookie 的後端。
- 身分服務延遲 3 秒時約 0.5 秒回 503；身分服務停機（scale 到 0）時需要 cookie 的路徑回 503、不依賴身分服務的例外路徑照常 200，恢復後回到 401／200。

不在 `bun test deploy/chart` 內（檔名沒有 `.test.`），要有叢集才能跑。叢集可以是 k3d、k3s 或其他 Kubernetes，只要 kubectl 連得到。

nightly workflow（Laysi/trustiot-v2#69）每晚在 k3d 叢集上以兩種型態各跑一次：裝好 Gateway API 與 Istio ambient、為每個型態的測試命名空間在 istiod 加一個 provider、建置身分服務替身並匯入節點，做法見 [CI 與發佈](../../../../docs/architecture/ci-release.md) 的 nightly.yml 一節與 `scripts/nightly/platform.ts`；那邊請求經 `kubectl port-forward`，不涵蓋入口的 NetworkPolicy。

### 替身

| 服務 | 替身 | 說明 |
| --- | --- | --- |
| `identity` | `identity-double/`（Bun，自己建映像） | 模擬身分服務的 ext_authz 端點：`sid=valid-…` 允許並回 `x-internal-identity`，`sid=slow-…` 延遲 3 秒，其他回 401；其餘路徑回 JSON，列出收到的標頭 |
| `assets` | traefik/whoami，以 digest 固定 | 需要 cookie 的後端（`hosts.app` 的 `/` 全包路由），回應帶 `Name: protected-backend` |
| `certs`、`mqtt-bridge` | 同上 | 例外路徑的後端 |

設定在 `values-e2e.yaml`。真實的身分服務在 `services/identity`（Laysi/trustiot-v2#61），回應表與替身相同（允許時回 200 並帶 `x-internal-identity`、無效回 401、逾時由入口回 503），但 cookie 是 `__Host-trustiot-session`，還會依 `ingress.routes` 決定 token 的受眾、對不到路由回 403，也需要 Postgres 與簽章私鑰；換成真實版本的端到端驗證在 Laysi/trustiot-v2#77。

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
| `E2E_REPORT_DIR` | 空 | 設定時把每個請求的結果寫成 `ingress-e2e-<shape>.json` |
| `E2E_KEEP` | 空 | `1` 時測完不移除 release，方便查看 |

測試以 `helm install --wait` 安裝；release 已經存在時直接失敗，不碰既有的安裝。結束時只移除這次建立的 release 與 TLS Secret（名稱帶隨機字尾）。身分服務停機的測試會暫時把替身 scale 到 0，結束時恢復。請求一律以自寫的 HTTP/1.1 客戶端送出原始的請求行，`fetch` 會先把 `..` 與編碼正規化，測不到繞過。

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

安裝分兩步：先只裝 ClickHouse，以 bootstrap 管理帳號在 pod 內套用 `services/ingest/migrations/clickhouse/` 並建立 collector 的帳號（正式安裝由 migration Job 做這一步，也由它產生 collector 的密碼；這個測試以 `migrate.enabled: false` 關閉 Job，自己做，所以不需要 migrate 映像），再 `helm upgrade` 開啟 collector、轉送與 example-service。chart 宣告的 collector 密碼 Secret（`<release>-otel-collector-password`，空的）出現後，測試把同一個密碼寫進去，代替 Job 的那一步；Job 本身產生密碼的流程由 `postgres.e2e.ts` 驗證。ClickHouse 管理帳號、collector 帳號與 SIEM token 每次執行重新產生，測試自己建立的 Secret 名稱帶隨機字尾，密碼只寫在本機暫存目錄的檔案（權限 0600）與 Secret 裡，不出現在指令列；SQL 經 stdin 送進 pod，管理帳號密碼不離開 pod。collector 的密碼 Secret 在 uninstall 時保留（`helm.sh/resource-policy: keep`），開始前若已存在就停止。結束時移除 release、ClickHouse 的 PVC、替身 SIEM、這次建立的 Secret 與 collector 的密碼 Secret。

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

3. collector 與 ClickHouse 的映像以 `values.yaml` 的 digest 從 Docker Hub 拉取。
4. 本機有 `proto install` 裝好的 helm 與 kubectl。

執行（在 repo 根目錄）：

```sh
E2E_NAMESPACE=e2e-73 \
E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:73 \
E2E_IMAGE_PULL_POLICY=Never \
bun test --timeout 900000 ./deploy/chart/tests/e2e/otel.e2e.ts
```

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `E2E_NAMESPACE` | 必填 | 測試用的 namespace，必須已存在 |
| `E2E_SERVICE_IMAGE` | 必填 | example-service 的映像，`repository:tag` 或加上 `@sha256:…` |
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
- Job 把密碼產生進 chart 宣告的空 Secret（`<release>-identity-password`、`<release>-otel-collector-password`）：每個 Secret 只有該有的 key、annotation `trustiot.io/password-generation` 是 `1`、帶 `helm.sh/resource-policy: keep`，`data` 只由 field manager `trustiot-migrate` 寫入；值是 64 個十六進位字元；Job 的日誌寫出產生了哪個角色的密碼，不含任何值。collector Ready。
- example-service 以 Job 建立的角色 `identity_app`、chart 掛載的 CA 與 `<cluster>-rw.<namespace>.svc` 連線：pod Ready 代表 readiness 的 Postgres 檢查以 verify-full 成功，伺服器端看得到這個角色的 TLS 連線。
- 同一個 pod template 的獨立 pod：不改的對照組 Ready；CA 換成叢集的根 CA（`kube-root-ca.crt`）時失敗（`UnknownIssuer`）；主機改成 primary 的 pod IP 時失敗（`not valid for name`）；換掉 `app.kubernetes.io/component` label、不在 `mesh.calls` 的 pod 連不上（NetworkPolicy 擋下，連線到不了 Postgres，ambient 下由 pod 的 ztunnel 重置，沒交換任何憑證或密碼）。
- 同樣的 values 再 `helm upgrade`：Job 名稱不變、不再執行，密碼 Secret 的內容不變；`helm rollback` 回第一版後也不變。
- `migrate.passwordGeneration=2`：新的 Job 換掉每個密碼（annotation 變成 `2`），日誌寫出設定了角色與 collector 帳號的新密碼，identity 與 collector 以新密碼滾動更新後 Ready。
- 刪掉 Secret 裡的 key 後以 `migrate.revision=2` 重跑 Job：key 重新產生，服務以新密碼 Ready。
- `helm uninstall` 後 Cluster、ImageCatalog、PVC 與兩個密碼 Secret 都還在，資料也在；Database 物件隨 release 刪除，資料庫保留。

密碼的比較只在測試的行程裡做（`data` 的 SHA-256），輸出與報告只有 `unchanged` 或 `changed`，不印任何值或 digest。

測試只建立一個 Secret：ClickHouse 的 bootstrap 管理帳號（名稱帶隨機字尾，密碼每次隨機產生，只寫在本機暫存目錄的檔案與 Secret 裡）。服務與 collector 的密碼全由 Job 產生。測試查資料庫的 SQL 經 stdin 送進 primary，以 `postgres` 角色走 pod 內的 local socket 執行，沒有任何密碼。開始前，同名的 Cluster、ImageCatalog、它們的 PVC、ClickHouse 的 PVC 或兩個密碼 Secret 已經存在（之前 uninstall 保留下來的）時直接失敗，不碰它們。結束時刪除測試的 pod、NetworkPolicy、release，再明確刪除保留下來的密碼 Secret、ClickHouse 的 PVC、Cluster、ImageCatalog 與 PVC，最後刪除管理帳號的 Secret。

前置條件：

1. 叢集已依 `deploy/cloudnative-pg/README.md` 裝好 CloudNativePG operator，並有 Istio ambient 與 Gateway API CRD（chart 一定會產生入口的 Gateway；測試不經入口）。
2. namespace 已建立，設 Pod Security `restricted`。測試的身分要能在這個 namespace 管理 chart 的物件（含 `postgresql.cnpg.io` 的 Cluster、ImageCatalog、Database，`gateway.networking.k8s.io` 的 Gateway、HTTPRoute 與 `security.istio.io` 的 AuthorizationPolicy，內建的 `admin` ClusterRole 都不含；還有 migration Job 的 Role 與 RoleBinding，它們只給 Job 讀寫兩個密碼 Secret），並能 `exec` 進 pod、讀 pod 與 Job 的 log、讀 Secret。
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
