# 入口 gateway 的叢集測試

`ingress.e2e.ts` 把 chart 裝進現有叢集的一個 namespace，經真正的入口 gateway 驗證決策 27 的規則（Laysi/trustiot-v2#62）：

- 需要 cookie 的路徑：沒有 cookie 或 session 無效回 401；有效 cookie 時後端只看到 1 個 `x-internal-identity`，是身分服務給的值，偽造的值進不來；`x-forwarded-proto` 為 `https`；`/internal` 沒有 cookie 回 401、有 cookie 回 403。
- 例外路徑（登入、健康檢查、憑證換發、MQTT）：不用 cookie，偽造的身分標頭被剝除，後端看到 0 個；在別的主機上不算例外。
- 路徑繞過：`BYPASS_CASES` 列出 28 種寫法（`..`、`%2e%2e`、`;`、`//`、大小寫、`?`、`%23` 等），沒有 cookie 時全部到不了需要 cookie 的後端。
- 身分服務延遲 3 秒時約 0.5 秒回 503；身分服務停機（scale 到 0）時需要 cookie 的路徑回 503、不依賴身分服務的例外路徑照常 200，恢復後回到 401／200。

不在 `bun test deploy/chart` 內（檔名沒有 `.test.`），要有叢集才能跑。叢集可以是 k3d、k3s 或其他 Kubernetes，只要 kubectl 連得到。

## 替身

| 服務 | 替身 | 說明 |
| --- | --- | --- |
| `identity` | `identity-double/`（Bun，自己建映像） | 模擬身分服務的 ext_authz 端點：`sid=valid-…` 允許並回 `x-internal-identity`，`sid=slow-…` 延遲 3 秒，其他回 401；其餘路徑回 JSON，列出收到的標頭 |
| `assets` | traefik/whoami，以 digest 固定 | 需要 cookie 的後端（`hosts.app` 的 `/` 全包路由），回應帶 `Name: protected-backend` |
| `certs`、`mqtt-bridge` | 同上 | 例外路徑的後端 |

設定在 `values-e2e.yaml`。身分服務的真實版本是 Laysi/trustiot-v2#61，接上後的端到端驗證在 Laysi/trustiot-v2#77。

## 前置條件

1. 叢集已依 `deploy/istio/README.md` 裝好 Istio ambient 與 Gateway API CRD。
2. istiod 有一個 ext_authz provider 指向測試 namespace 的身分服務替身 `trustiot-identity.<namespace>.svc.cluster.local:8080`，設定與 `deploy/istio/istiod-values.yaml` 相同（`pathPrefix: /internal/ext-authz`、`failOpen: false`、`timeout: 0.5s`、`statusOnError: "503"`）。
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

## 執行

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

## 日誌管線的叢集測試（`otel.e2e.ts`）

`otel.e2e.ts` 驗證決策 22 的日誌管線（Laysi/trustiot-v2#73）：以地端型態加 `values-otel-e2e.yaml` 安裝 chart（叢集內 ClickHouse、OpenTelemetry collector，`assets` 換成 `crates/service-kit` 的 example-service），另外部署一個替身 SIEM（同一個 collector 映像，只收帶 Bearer token 的 OTLP/HTTP，把收到的資料印到 stdout），然後檢查：

- example-service 處理一個帶 `traceparent` 的請求後，ClickHouse 的 `trustiot_logs.otel_logs` 出現帶同一個 trace id 的結構化紀錄（服務名稱、等級、method、路由、狀態碼），`otel_traces` 有這個請求的 span；請求 body 裡的 email 與密碼不在任何一筆紀錄裡。
- 兩張日誌表都是 MergeTree，從伺服器讀回 `TTL ReceivedAt + toIntervalDay(30)`。
- 替身 SIEM 收到同一個 trace id 的紀錄；不帶標頭送給它時回 401，所以收到的資料是 collector 以 Secret 裡的標頭送來的。
- 直接以 OTLP 送給 collector 含 email、Bearer token、JWT、密碼、電話與 request body 的日誌與 span 後，ClickHouse 與 SIEM 裡的紀錄都已遮蔽（`****`），任何一個原始值都找不到。
- collector 的 pod 在 restricted 的 namespace 裡執行，安全設定符合 restricted Pod Security Standard。

安裝分兩步：先只裝 ClickHouse，以 bootstrap 管理帳號在 pod 內套用 `services/ingest/migrations/clickhouse/` 並建立 collector 的帳號（migration Job，Laysi/trustiot-v2#60，之後會做這一步），再 `helm upgrade` 開啟 collector、轉送與 example-service。所有 Secret（ClickHouse 管理帳號、collector 帳號、SIEM token）每次執行重新產生，名稱帶隨機字尾，密碼只寫在本機暫存目錄的檔案與 Secret 裡，不出現在指令列；SQL 經 stdin 送進 pod，管理帳號密碼不離開 pod。結束時移除 release、ClickHouse 的 PVC、替身 SIEM 與這次建立的 Secret。

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
