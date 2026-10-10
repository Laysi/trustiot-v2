# 入口 gateway 的叢集測試

`ingress.e2e.ts` 把 chart 裝進現有叢集的一個 namespace，經真正的入口 gateway 驗證決策 27 的規則（Laysi/trustiot-v2#62）：

- 需要 cookie 的路徑：沒有 cookie 或 session 無效回 401；有效 cookie 時後端只看到 1 個 `x-internal-identity`，是身分服務給的值，偽造的值進不來，底線寫法（`x_internal_identity` 等三種）也被移除；`x-forwarded-proto` 為 `https`；`/internal` 沒有 cookie 回 401、有 cookie 回 403。
- `/internal` 的 11 種斜線寫法（`INTERNAL_SLASH_CASES`：`//internal`、`///internal`、`//./internal`、`/x/..//internal/…`、`/%2finternal`、`/%2Finternal` 等）：沒有 cookie 回 401、有效 cookie 回 403（Laysi/trustiot-v2#255）。這些案例只在 istiod 設了 `deploy/istio/istiod-values.yaml` 的 `meshConfig.pathNormalization`（`DECODE_AND_MERGE_SLASHES`）時通過；少了它，有效 cookie 的請求會到達全包路由的後端。
- 例外路徑（登入、健康檢查、憑證換發、MQTT）：不用 cookie，偽造的身分標頭與其底線寫法被剝除，後端看到 0 個；`Host` 帶埠號（`:80`、`:443`、`:8443`）仍是例外；在別的主機上（帶不帶埠號）不算例外。
- 路徑繞過：`BYPASS_CASES` 列出 28 種寫法（`..`、`%2e%2e`、`;`、`//`、大小寫、`?`、`%23` 等），沒有 cookie 時全部到不了需要 cookie 的後端。
- 身分服務延遲 3 秒時約 0.5 秒回 503；身分服務停機（scale 到 0）時需要 cookie 的路徑回 503、不依賴身分服務的例外路徑照常 200，恢復後回到 401／200。

不在 `bun test deploy/chart` 內（檔名沒有 `.test.`），要有叢集才能跑。叢集可以是 k3d、k3s 或其他 Kubernetes，只要 kubectl 連得到。

nightly workflow（Laysi/trustiot-v2#69）每晚在 k3d 叢集上以兩種型態各跑一次：裝好 Gateway API 與 Istio ambient、為每個型態的測試命名空間在 istiod 加一個 provider、建置身分服務替身並匯入節點，做法見 [CI 與發佈](../../../../docs/architecture/ci-release.md) 的 nightly.yml 一節與 `scripts/nightly/platform.ts`；那邊請求經 `kubectl port-forward`，不涵蓋入口的 NetworkPolicy。

## 替身

| 服務 | 替身 | 說明 |
| --- | --- | --- |
| `identity` | `identity-double/`（Bun，自己建映像） | 模擬身分服務的 ext_authz 端點：`sid=valid-…` 允許並回 `x-internal-identity`，`sid=slow-…` 延遲 3 秒，其他回 401；其餘路徑回 JSON，列出收到的標頭 |
| `assets` | traefik/whoami，以 digest 固定 | 需要 cookie 的後端（`hosts.app` 的 `/` 全包路由），回應帶 `Name: protected-backend` |
| `certs`、`mqtt-bridge` | 同上 | 例外路徑的後端 |

設定在 `values-e2e.yaml`。身分服務的真實版本是 Laysi/trustiot-v2#61，接上後的端到端驗證在 Laysi/trustiot-v2#77。

## 前置條件

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

## 通道斷線時的復原

測試經 `kubectl port-forward`（或 `E2E_SSH_JUMP` 的 `ssh -L`）連入口 gateway。這條通道偶爾會在請求進行中斷線：kubectl 印出 `error forwarding port … to pod` 與 `error: lost connection to pod`。2026-10-10 PR Laysi/trustiot-v2#258 的 CI 就發生過一次（run 38075563886 attempt 1，`identity service down`），當時 `raw()` 丟出 `no HTTP response for /api/whoami: ""`，測試失敗。

原因是競態：請求先因連線被關閉而失敗，kubectl 行程要稍晚才結束；舊的 `raw()` 在失敗的當下檢查行程是否已結束，看到還在跑就直接丟出錯誤。現在的做法（`tunnel-recovery.ts`）：

- 請求以連線層錯誤失敗（`raw-client.ts` 的 `ConnectionError`：連線被拒、被重設、在回應標頭之前被關閉、逾時）時，最多等 5 秒讓通道行程結束。
- 行程在這段時間內結束，就重開通道並把請求重送一次；重送只有一次，第二次再失敗就失敗。
- 行程沒有結束，代表是 gateway 那一端真的關掉連線，錯誤照樣丟出，不重試。
- 只要拿到 HTTP 回應（或已收到回應標頭）就不重試，狀態碼不符（例如該是 200 卻得到 503）照樣讓測試失敗；非 `ConnectionError` 的錯誤也直接丟出。

沒有查到可避免 port-forward 斷線的原因（斷線時間點只有一次紀錄，kubectl 沒有說明 pod 端為何關閉連線），所以靠上述復原處理；若之後斷線變得頻繁，再評估改用 k3d 的 LoadBalancer 埠對映取代 port-forward。

邏輯有不需要叢集的單元測試（`tunnel-recovery.test.ts`、`raw-client.test.ts`），由 `bun test deploy/chart` 執行。測試用真正的子行程（`fake-port-forward.ts`，一個 TCP proxy）扮演 kubectl：它在收到請求時切斷連線，300 毫秒後才結束行程，重現上述競態。
