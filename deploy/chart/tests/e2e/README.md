# 叢集測試（入口 gateway、服務間授權、日誌管線、叢集內的 Postgres、Postgres 的複寫與備份、ClickHouse 管理帳號密碼、內部身分簽章金鑰）

兩個測試共用 `cluster.ts`（環境變數、`.prototools` 鎖定的 helm 與 kubectl、不碰既有安裝的 `helm install`）：

| 測試 | 驗證 | issue |
| --- | --- | --- |
| `ingress.e2e.ts` | 入口 gateway：cookie 驗證、例外路徑、標頭、路徑繞過、身分服務停機 | Laysi/trustiot-v2#62 |
| `mesh.e2e.ts` | 服務間授權：清單內的呼叫放行、清單外的被拒（帶著使用者的 token 也一樣），AuthorizationPolicy 與 NetworkPolicy 各自生效，預設拒絕下健康檢查通過；代表使用者呼叫的服務連得到身分服務的 token 交換 | Laysi/trustiot-v2#63、Laysi/trustiot-v2#106 |

`otel.e2e.ts`（日誌管線，Laysi/trustiot-v2#73）、`postgres.e2e.ts`（叢集內的 Postgres，Laysi/trustiot-v2#58）、`postgres-replication.e2e.ts`（叢集內 Postgres 的複寫、備份與還原，Laysi/trustiot-v2#400）、`clickhouse-admin.e2e.ts`（叢集內 ClickHouse 的管理帳號密碼，Laysi/trustiot-v2#403）、`identity-keys.e2e.ts`（內部身分簽章金鑰，Laysi/trustiot-v2#404）與 `key-rotation.e2e.ts`（依金鑰年齡自動輪替，Laysi/trustiot-v2#429）不經入口 gateway，有自己的環境變數，不用 `cluster.ts`，見最後六節。

身分服務（或它的替身）啟用時，chart 在它的 pod 加上 init container `signing-keys`，以 migrate 映像在叢集內產生內部身分簽章金鑰（Laysi/trustiot-v2#404），所以每個啟用 `identity` 的測試都要 migrate 映像（`E2E_MIGRATE_IMAGE`，建法見「叢集內的 Postgres」）。它填的 Secret `<release>-identity-signing-keys` 與 ConfigMap `<release>-internal-identity-keys` 在 uninstall 時保留（`helm.sh/resource-policy: keep`）；`cluster.ts` 與 `postgres.e2e.ts` 移除這次執行建立的那兩個物件，安裝前已經存在的不動。chart 同時渲染依金鑰年齡輪替的 CronJob（Laysi/trustiot-v2#429，同一個 migrate 映像，預設每天 03:17 UTC、金鑰 90 天才到期），測試期間不會換金鑰。

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
2. istiod 有一個 ext_authz provider 指向測試 namespace 的身分服務替身 `trustiot-identity.<namespace>.svc.cluster.local:8080`，設定與 `deploy/istio/istiod-values.yaml` 相同（`pathPrefix: /internal/ext-authz`、`failOpen: false`、`timeout: 0.5s`、`statusOnError: "503"`、`includeRequestHeadersInCheck: [cookie, origin, sec-fetch-site]`），mesh 的 `pathNormalization` 也與該檔相同。k3s 以 `HelmChartConfig istiod` 疊加時，清單會整份取代，所以叢集原有的 provider 要原樣重複寫入；範例見 [lab-istiod-helmchartconfig.yaml](../../../../docs/test-reports/data/2026-10-11-255-path-normalization/lab-istiod-helmchartconfig.yaml)。這是整個叢集共用的設定：先備份 `istio-system` 的 ConfigMap `istio`，測完刪掉 `HelmChartConfig`，再確認 `mesh` 與備份逐位元組相同。

   lab 的 Mac mini M1 同時跑展示環境（Laysi/trustiot-v2#457）。它的 HelmChart `kube-system/istiod` 本身已經有 `deploy/istio/istiod-values.yaml` 的 provider `trustiot-identity`，指向展示環境的身分服務 `trustiot-identity.trustiot.svc.cluster.local`，mesh 也已經開啟路徑正規化（由 `scripts/demo/platform.ts` 寫入，見 [scripts/README.md](../../../../scripts/README.md) 的「展示機」）。所以在 lab 跑入口測試時：

   - provider 名稱不能用預設的 `trustiot-identity`，改用這次測試自己的名稱（例如 `trustiot-e2e-<issue>`），`E2E_AUTHZ_PROVIDER` 設成同一個名稱。用同名的 provider 時，HelmChartConfig 裡那份會取代展示環境的，展示環境的入口在測試期間就把 cookie 檢查交給測試的替身。
   - `HelmChartConfig istiod` 的 `extensionProviders` 會整份取代 HelmChart 的清單，所以要把 `trustiot-identity` 從 `deploy/istio/istiod-values.yaml` 原樣複製進去，再加上測試自己的 provider；上面的範例重複的是當時 PoC 的 `identity`，現在要重複的是 `trustiot-identity`。漏掉時展示環境的入口在測試期間對需要 cookie 的請求一律回 403（不會放行）。`pathNormalization` 是 map，Helm 會與 HelmChart 的值合併，不必重寫。
   - 加 HelmChartConfig 之前先取得 lab lock（[deploy/flux/README.md](../../../flux/README.md) 的「lab 測試」），測完刪掉並比對 `mesh` 之後才釋放。HelmChartConfig 存在時 `scripts/demo/platform.ts --apply` 會拒絕執行。
   - 測試的 namespace 用 `e2e-<issue>`，不能用展示環境的 `trustiot`。
3. 測試用的 namespace 已建立，建議設 Pod Security `restricted`。測試的身分要能在這個 namespace 管理 chart 的物件，包含 `gateway.networking.k8s.io` 的 Gateway、HTTPRoute 與 `security.istio.io` 的 AuthorizationPolicy（內建的 `admin` ClusterRole 不含這兩個 API group）。
4. 身分服務替身的映像（arm64 或叢集節點的架構）與 migrate 映像要在叢集拉得到。例如在節點上建好再匯入 containerd：

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
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:62 \
E2E_IMAGE_PULL_POLICY=Never \
E2E_AUTHZ_PROVIDER=<provider 名稱> \
bun test --timeout 600000 ./deploy/chart/tests/e2e/ingress.e2e.ts
```

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `E2E_NAMESPACE` | 必填 | 測試用的 namespace，必須已存在 |
| `E2E_IDENTITY_IMAGE` | 必填 | 身分服務替身的映像，`repository:tag` 或加上 `@sha256:…` |
| `E2E_MIGRATE_IMAGE` | 必填 | migrate 映像：替身 pod 的 init container 以它產生內部身分簽章金鑰 |
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
| assets → certs | 清單內（`values-mesh-e2e.yaml` 的 `mesh.calls`），放行，certs 收到呼叫帶的使用者 token |
| assets → identity 的 token 交換（Service `<release>-identity-exchange` 的 `token-exchange` 埠 8082，`POST /internal/token-exchange`） | assets 代表使用者呼叫 certs，chart 因此讓它連得到身分服務的 `token-exchange` 埠（Laysi/trustiot-v2#106），放行，身分服務收到 assets 自己的 token。身分服務替身也在 8082 回應，所以第 3 階段（兩層都拿掉）清單外的 token 交換也會通過 |
| assets → identity 的 API 埠（8080，ext_authz 所在的埠） | 只有 AuthorizationPolicy 擋：assets 只能到 `token-exchange`，NetworkPolicy 看不到 HBONE 裡的埠，為了 token 交換放行 assets 的 pod。第 1、4、5 階段被拒，第 2 階段（只剩 NetworkPolicy）通過 |
| 入口 gateway → identity、certs、mqtt-bridge（例外路徑，經 `kubectl port-forward` 進入口 pod） | 放行 |
| certs → assets、assets → alerts、mqtt-bridge 與 certs → identity 的 token 交換、stranger → certs、outsider → certs 的 Service 與 pod IP | 清單外，任一層在就被拒，即使帶著被呼叫端會接受的使用者 token |
| imposter → certs | NetworkPolicy 依 label 放行它，只有 AuthorizationPolicy 擋得住；用來確認 AuthorizationPolicy 是否生效 |
| outsider → 入口 gateway 的 pod | 只有入口的 NetworkPolicy 擋；用來確認 NetworkPolicy 是否生效 |

代表使用者的呼叫（Laysi/trustiot-v2#106）：從 pod 發出的每個呼叫都帶一張使用者的內部身分 token（`x-internal-identity`），直接呼叫時受眾是被呼叫端，向身分服務換 token 時是呼叫端自己。token 由測試以這次安裝的簽章金鑰簽出（init container 填入的 Secret `<release>-identity-signing-keys`，`internal-token.ts`），用前先以服務掛載的公鑰 ConfigMap 驗過，所以是服務會接受的 token。私鑰只在測試的行程裡，token 經 `kubectl exec -i` 的 stdin 交給 pod，不出現在指令列、輸出或結果檔；呼叫的腳本（`mesh-call.ts`）只回報被呼叫端是否收到它。服務都是身分服務替身、不驗證 token，所以這裡看到的拒絕都是 mesh 在 L4 做的，驗證的是 mesh 放行什麼、token 隨清單內的呼叫到達，以及身分服務拿到的呼叫清單（安裝後檢查 `TRUSTIOT_ON_BEHALF_CALLS` 是 `{"assets":["certs"]}`）；token 交換與服務自己的檢查（受眾、過期、清單外的組合、換來的 token 不能再換）由 `services/identity` 的 Rust 測試涵蓋。身分服務的授權依埠分開：身分服務自己的 AuthorizationPolicy 只讓入口 gateway 到 `http`，token 交換自己的 AuthorizationPolicy（`<release>-identity-exchange`）只讓 assets 到 `token-exchange`；上表「assets → identity 的 API 埠」在叢集裡驗證這件事，chart 測試另外比對渲染結果。`mesh-call.ts` 與 `internal-token.ts` 有不需要叢集的單元測試，隨 `bun test deploy/chart` 執行。

測試依序執行五個階段，每個階段從前一個階段留下的狀態開始：

1. 兩層都在：清單內的呼叫與入口放行，清單外的與 imposter 全部被拒；所有 pod 就緒、沒有重啟（預設拒絕下 kubelet 的健康檢查照常通過）。
2. 刪掉服務的 AuthorizationPolicy（入口的 cookie 驗證與 `/internal` 拒絕保留）：imposter 通過（確認 AuthorizationPolicy 已失效），清單外的呼叫仍全部被 NetworkPolicy 拒絕，清單內的與入口照常放行。
3. 再刪掉 release 的所有 NetworkPolicy（依金鑰年齡輪替的 CronJob 的 NetworkPolicy 與 AuthorizationPolicy 除外：沒有人呼叫它的 pod，四個階段都不動這兩個，安裝後只確認它們在）：outsider 連得到入口 pod，清單外的呼叫全部成功，證明前後階段被拒是政策造成的，不是呼叫本身有問題。
4. 以 `helm get manifest` 的內容重建 AuthorizationPolicy、NetworkPolicy 仍刪除：清單外的呼叫與 imposter 全部被 AuthorizationPolicy 拒絕，清單內的與入口照常放行。
5. 重建 NetworkPolicy，回到安裝時的狀態：再檢查一次呼叫與 pod 健康狀態。

只有 NetworkPolicy 的階段排在任何清單外的呼叫成功之前：NetworkPolicy 不會切斷已經建立的連線，而 ztunnel 會保留 HBONE 連線重複使用，所以第 3 階段之後才重建 NetworkPolicy 時，走既有連線的呼叫照樣成功（2026-10-10 在實驗機實測，見 docs/test-reports/2026-10-10-63-mesh-authz.md）。AuthorizationPolicy 由 ztunnel 對每條代理的連線檢查，第 4 階段不受順序影響。

被 ztunnel 拒絕時呼叫端看到連線重置（`ECONNRESET`）；被 NetworkPolicy 擋時，依 NetworkPolicy 控制器的做法是立即拒絕（k3s 是 `ConnectionRefused`，mesh 內的呼叫端經 ztunnel 轉成重置）或逾時。都算被拒，結果檔記錄每次呼叫的種類與毫秒數。

### 前置條件與執行

前置條件同入口測試的 1、3、4、5 項：需要 Istio ambient 與 Gateway API CRD、已建立的 namespace（建議 `restricted`）、叢集拉得到的身分服務替身映像；不需要 ext_authz provider，入口只走例外路徑。測試的身分還要能在 namespace 裡建立 pod、`pods/exec` 與 `pods/portforward`，讀取簽章金鑰的 Secret 與公鑰的 ConfigMap，並刪除與建立 NetworkPolicy 和 AuthorizationPolicy。身分服務替身的映像同時用在五個服務與三個測試 pod，因為它是唯一有執行環境（Bun）、能從 pod 內發出呼叫的測試映像。

在 repo 根目錄：

```sh
E2E_NAMESPACE=e2e-63 \
E2E_IDENTITY_IMAGE=localhost/trustiot-e2e/identity-double:63@sha256:<digest> \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:63 \
E2E_IMAGE_PULL_POLICY=Never \
bun test --timeout 600000 ./deploy/chart/tests/e2e/mesh.e2e.ts
```

變數同入口測試的 `E2E_NAMESPACE`、`E2E_IDENTITY_IMAGE`、`E2E_MIGRATE_IMAGE`、`E2E_IMAGE_PULL_POLICY`、`E2E_KUBECONTEXT`、`E2E_RELEASE`、`E2E_KEEP`，另有：

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

扮演 identity 的 example service 的 pod 也有 init container `signing-keys`（Laysi/trustiot-v2#404），它產生的內部身分金鑰 Secret 與公鑰 ConfigMap 和密碼 Secret 一樣：開始前已經存在就停止，結束時刪除。

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

`postgres-replication.e2e.ts` 驗證收回 `postgres` 與 `template1` 的 `PUBLIC` 權限之後（Laysi/trustiot-v2#400，`files/postgres/system-databases.sql`），CloudNativePG 自己的連線照常運作。它以地端型態 `helm template --show-only templates/postgres.yaml` 渲染 chart 的 ImageCatalog、Cluster 與 NetworkPolicy，**改過 Cluster 再套用**：`instances: 2`、以 Barman Cloud plugin 當 WAL archiver（`spec.plugins`）、較小的資源（requests `50m`／`192Mi`、limit `512Mi`）與 1Gi 的磁碟區；其他欄位（bootstrap 與 `postInitSQL`、pg_hba、image catalog、不開 superuser）都是 chart 的，報告的 `clusterChanges` 列出改了什麼、沒改什麼。chart 本身維持 `instances: 1`、沒有備份設定（備份是 Laysi/trustiot-v2#134）。chart 的 NetworkPolicy 只放行服務與 migration Job，所以測試另外加一個 NetworkPolicy，讓這個 Cluster 的 pod 連它的 instance 的 5432：instance 之間，以及從 primary 複製新 replica 的 join Job（它的 pod 有 `cnpg.io/cluster`，沒有 `cnpg.io/podRole=instance`）。不裝其他元件，也不跑 migration Job（Job 在 `postgres.e2e.ts` 驗證）。依序檢查：

- 新 bootstrap 的 Cluster：`postgres` 與 `template1` 的 ACL 只剩 superuser 與 operator 的兩個角色；superuser 與 `pg_` 開頭的角色以外，每個角色（owner、以 owner 的 CREATEROLE 照 Job 的方式建立的 `e2e_app`、operator 的角色）在兩個資料庫都沒有 `CONNECT` 與 `TEMPORARY`，只有 `streaming_replica` 與 `cnpg_metrics_exporter` 有 `postgres` 的 `CONNECT`；`trustiot` 仍讓 `PUBLIC` 連線；operator 接手 `postInitSQL` 先建好的兩個角色後，屬性與它自己建立的相同（`cnpg_metrics_exporter` 是 `pg_monitor` 的成員）。
- 複寫：primary 的 `pg_stat_replication` 有 `streaming_replica` 的 `streaming`，replica 的 `pg_stat_wal_receiver` 是 `streaming`，primary 寫入的列出現在 replica。
- switchover：把 Cluster 的 `status.targetPrimary` 設成 replica（`kubectl cnpg promote` 與 CloudNativePG 自己的 e2e 測試的做法）；新 primary 生效、Cluster 回到兩個 instance 健康，前 primary 以 replica 身分從新 primary 串流，新 primary 寫入的列出現在它上面。前 primary 的日誌有 `Starting up pg_rewind`，以及 `Done!` 或 `no rewind required`（`pg_rewind` 以 `streaming_replica` 連新 primary 的 `postgres`），沒有權限錯誤；報告的 `switchover.rewind` 列出這些日誌。
- 備份：Cluster 的 `ContinuousArchiving` 條件是 `True`，`method: plugin` 的 Backup 達到 `completed`。
- 還原：新的 Cluster `<release>-postgres-restored`（一個 instance）經 plugin 從這份備份 bootstrap（`bootstrap.recovery` 與 `externalClusters[].plugin`），有原本的資料，兩個資料庫的 ACL 與角色權限和原 Cluster 相同，instance manager 的 metrics 沒有錯誤；檢查完就刪掉它，把記憶體還給節點。
- instance manager：最後每個 instance 都 Ready，metrics（在 instance 的 postgres 容器內以 bash 的 `/dev/tcp` 讀 9187：chart 的 NetworkPolicy 不放行 9187，API server 的 proxy 連不到）的 `cnpg_collector_up` 是 1、`cnpg_collector_last_collection_error` 是 0，每個 instance 的日誌沒有 `permission denied for database`、`CONNECT privilege` 或 `permission denied to create temporary`。

物件儲存是同一個 namespace 裡的 RustFS（`docker.io/rustfs/rustfs:1.0.0-glibc@sha256:bffcab0c9d647aab0055d1c69d340b202d0909966b385932d4ead1aeb7602858`，可用 `E2E_STORE_IMAGE` 換掉）。選它的理由：Barman Cloud plugin v0.15.1 自己的 e2e 測試就是對這個映像與 digest 跑的（plugin repo 的 `test/e2e/internal/objectstore/s3.go`），barman-cloud 的 S3 用戶端與它的相容性有上游驗證；它持續維護、Apache-2.0、有 arm64 映像。MinIO 社群版自 2025 年起不再發佈映像，Garage 與 versitygw 不在 plugin 的測試範圍內。資料放在 emptyDir，隨 pod 刪除。存取金鑰是這個行程產生的隨機值，只寫進測試建立的 Secret，不印出、不寫進報告。決策 20 的產品物件儲存不用 RustFS，這裡只是測試用的伺服器。

開始前同名的 Cluster、ImageCatalog、chart 的 NetworkPolicy `<release>-postgres` 或 Cluster 的 PVC 已經存在時直接失敗，不碰它們。結束時刪除還原的 Cluster、Backup、Cluster、ImageCatalog、chart 的 NetworkPolicy、PVC、測試的 NetworkPolicy、ObjectStore 與物件儲存。

前置條件：

1. 平台元件（叢集層級，由平台維運安裝，不由測試安裝）：CloudNativePG 1.30.1（`deploy/cloudnative-pg/README.md`）、cert-manager v1.21.2（plugin 的 manifest 需要它的 Issuer 與 Certificate），以及 Barman Cloud plugin v0.15.1（官方 release 的 `manifest.yaml`：Deployment `barman-cloud` 在 `cnpg-system`、CRD `objectstores.barmancloud.cnpg.io`，映像 `ghcr.io/cloudnative-pg/plugin-barman-cloud:v0.15.1`）。我們的 Postgres 映像以 CloudNativePG 的 `minimal` 映像為基底，沒有 barman-cloud，所以備份與還原一律經 plugin 的 sidecar。lab 安裝時的版本與 manifest 的 SHA-256 記在測試報告。
2. namespace 已建立，設 Pod Security `restricted`。測試的身分要能在這個 namespace 管理 Cluster、ImageCatalog、Backup（`postgresql.cnpg.io`）、ObjectStore（`barmancloud.cnpg.io`）、Deployment、Service、Secret、NetworkPolicy 與 PVC，能 patch Cluster 的 `status`（switchover），能 `exec` 進 pod、讀 pod 的 log。
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

## 內部身分簽章金鑰（`identity-keys.e2e.ts`）

`identity-keys.e2e.ts` 驗證內部身分簽章金鑰在叢集內產生、公開、輪替與撤銷（Laysi/trustiot-v2#404，設計在 [docs/architecture/secrets.md](../../../../docs/architecture/secrets.md) 的「內部身分簽章金鑰」）。以 `E2E_SHAPE` 的型態加 `values-identity-keys-e2e.yaml` 一次安裝叢集內的 Postgres、ClickHouse、migration Job、從這個 commit 建出的身分服務，以及在 assets 位置的 `crates/service-kit` example service（`GET /v1/whoami` 驗證給 assets 的內部身分 token，不用資料庫）；沒有任何東西事先建立金鑰，`internalIdentity.keyPropagationSeconds` 是 schema 的下限 120 秒。`internalIdentity.maxKeyAgeSeconds` 設成 0，不渲染依金鑰年齡輪替的 CronJob（它由下一節的 `key-rotation.e2e.ts` 測）：這個測試檢查的是 init container，而每天 03:17 UTC 的排程如果剛好落在「移除舊金鑰」的等待與重啟之間，會在重啟之前就移除舊金鑰。依序檢查：

- **全新安裝**：身分服務 pod 的 init container 產生一把代 1 的金鑰，Secret `<release>-identity-signing-keys` 只有 `signing-keys.json`、綁定自己的 uid、帶 `keep` 與 `trustiot.io/filled-by: migrate`；ConfigMap `<release>-internal-identity-keys` 只有 `jwks.json`，列出同一個 kid（RFC 7638 thumbprint），每把只有公開的成員；第一把立即簽章（`signs_from` 等於 `published_at`）。以 `kubectl exec` 在 primary 建立一個平台管理者與它的 session（同 nightly 的 API 檢查），經 `kubectl port-forward` 向身分服務的 ext_authz 要一個給 assets 路由的 token，example service 接受它（200，使用者相符）。記錄每個 pod 在金鑰公開之後多久啟動、載入金鑰（日誌）與就緒。
- **權限**：namespace 的每個 ServiceAccount 以 `kubectl auth can-i --as` 問 Secret 與 ConfigMap 的 `get`、`patch`、`update`、`delete`，以及兩種物件的 `create`、`list`、`watch`：只有身分服務的 ServiceAccount 對這兩個物件的 `get` 與 `patch` 是 yes。
- **輪替**（`keyGeneration=2`）：Secret 有代 1 與代 2 兩把，代 2 的 `signs_from` 比 `published_at` 晚 120 秒，ConfigMap 列出兩把；`signs_from` 之前身分服務仍以舊金鑰簽、example service 接受；example service 的 pod 沒有重啟，從日誌量出它與身分服務多久之後載入兩把公鑰、身分服務多久之後載入兩把私鑰；身分服務在 `signs_from` 之後改用新金鑰，新 token 被接受。
- **rollback**：`helm rollback` 到第 1 版（`keyGeneration=1`）與再升級回 2，Secret 與 ConfigMap 都不變，token 仍是代 2 的 kid。
- **移除舊金鑰**：等到代 2 的 `signs_from` 之後再過 120 + 305 秒，`kubectl rollout restart` 身分服務：只剩代 2，example service 從日誌看得到它換成只有代 2 的一組。
- **撤銷**（`keyGeneration=3`、`revokeBeforeGeneration=3`）：只剩代 3，立即簽章；記錄新的身分服務 pod 多久之後就緒、example service 多久之後換掉被撤銷的公鑰，代 3 的 token 被接受。
- **沒有私鑰外流**：測試讀 Secret 只為了知道每把的代與時間，並確認每一把私鑰都不在 ConfigMap、事件、身分服務（含 init container）與 example service 的日誌裡；比較只有是或否，私鑰不印、不寫進報告。
- **uninstall**：Secret 與 ConfigMap 保留，Secret 仍是代 3 那一把。

開始前同名的 release、Cluster、ImageCatalog、PVC、ClickHouse 的 PVC、保留的 Secret 或 ConfigMap 已存在時直接失敗；結束時移除 release 與這些物件。整個測試約 20 分鐘，大多在等新金鑰的 `signs_from` 與舊金鑰的移除時間。

前置條件：

1. 叢集已依 `deploy/cloudnative-pg/README.md` 裝好 CloudNativePG operator，並有 Istio ambient 與 Gateway API CRD（chart 一定會產生入口的 Gateway；測試不經入口）。
2. namespace 已建立，設 Pod Security `restricted`。測試的身分要能在這個 namespace 管理 chart 的物件（同「叢集內的 Postgres」），讀 pod 的 log 與 Secret、`exec` 進 pod、`port-forward`，並能以 `--as` 模擬這個 namespace 的 ServiceAccount。
3. Postgres、身分服務、example service 與 migrate 的映像要在叢集拉得到，建法見「叢集內的 Postgres」與「日誌管線」；身分服務的映像以 `deploy/images/rust-binary.Containerfile`（`@BIN@` 換成 `identity`）建出。身分服務與 migrate 的映像 tag 要相同（身分服務用 Postgres）。
4. 本機有 `proto install` 裝好的 helm 與 kubectl。

執行（在 repo 根目錄）：

```sh
E2E_NAMESPACE=e2e-404 \
E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:404 \
E2E_IDENTITY_IMAGE=localhost/trustiot-e2e/identity:404 \
E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:404 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:404 \
E2E_IMAGE_PULL_POLICY=Never \
bun test --timeout 3600000 ./deploy/chart/tests/e2e/identity-keys.e2e.ts
```

| 變數 | 預設 | 說明 |
| --- | --- | --- |
| `E2E_NAMESPACE` | 必填 | 測試用的 namespace，必須已存在 |
| `E2E_POSTGRES_IMAGE` | 必填 | 含 AGE 的 Postgres 映像 |
| `E2E_IDENTITY_IMAGE` | 必填 | 身分服務的映像，tag 要與 `E2E_MIGRATE_IMAGE` 相同 |
| `E2E_SERVICE_IMAGE` | 必填 | example service 的映像，在 assets 的位置驗證 token |
| `E2E_MIGRATE_IMAGE` | 必填 | migrate 映像：migration Job 與身分服務 pod 的 init container |
| `E2E_SHAPE` | `saas` | `saas` 或 `onprem`，決定前兩個 values 檔 |
| `E2E_IMAGE_PULL_POLICY` | `IfNotPresent` | 四個映像都用這個值；匯入節點的映像用 `Never` |
| `E2E_KUBECONTEXT` | 目前的 context | kubeconfig 裡的 context |
| `E2E_RELEASE` | `trustiot` | helm release 名稱 |
| `E2E_REPORT_DIR` | 空 | 設定時把每一步的 kid、代、時間與 token 的結果（沒有任何金鑰或 token）寫成 `identity-keys-e2e-<shape>.json` |
| `E2E_KEEP` | 空 | `1` 時測完不移除任何東西，方便查看 |

## 依金鑰年齡自動輪替（`key-rotation.e2e.ts`）

`key-rotation.e2e.ts` 驗證依金鑰年齡自動輪替（Laysi/trustiot-v2#429，設計在 [docs/architecture/secrets.md](../../../../docs/architecture/secrets.md) 的「依金鑰年齡自動輪替」）。安裝與 `identity-keys.e2e.ts` 相同（同一份 `values-identity-keys-e2e.yaml`、同樣四個映像），另外設 `internalIdentity.maxKeyAgeSeconds=900`（比 chart 允許的下限 545 秒多 355 秒，讓 CronJob 移除舊金鑰、example service 換上新的一組都在新金鑰到期之前完成，之後的手動輪替不會碰上進行中的自動輪替）與 `keyRotationSchedule="* * * * *"`（每分鐘一次）。依序檢查：

- **CronJob 的形狀與權限**：CronJob 以身分服務的 ServiceAccount 執行，不掛 Secret、不從 Secret 讀環境變數；namespace 的每個 ServiceAccount 以 `kubectl auth can-i --as` 問 Secret 與 ConfigMap 的 `get`、`patch`、`update`、`delete` 與兩種物件的 `create`、`list`、`watch`，仍然只有身分服務的 ServiceAccount 的 `get`、`patch` 是 yes。金鑰公開之後的第一次執行不改任何東西，日誌的 `rotation_due_at` 是 `published_at + 900`。
- **自動輪替**：測試持續（每 2 秒）向身分服務要 token、交給 example service 驗證，經兩個一直連著同一個 pod 的 port-forward。金鑰滿 900 秒之後的一次執行加一把同一代（代 1）的新金鑰，`signs_from` 比 `published_at` 晚 120 秒，ConfigMap 列出兩把，那次執行的日誌寫出 `to replace key <舊 kid>`；在 `signs_from` 前 20 秒取得的舊 kid token，在身分服務改用新金鑰之後仍被接受；新 token 是新 kid、被接受；兩個 pod 都沒有重啟；持續的請求沒有一次失敗，kid 只從舊的換到新的一次。
- **CronJob 移除舊金鑰**：在新金鑰的 `signs_from + 120 + 305` 秒之後，CronJob 的一次執行（不是重啟）從 Secret 與 ConfigMap 移除舊金鑰，example service 不重啟就換成只有新的一把。
- **自動輪替開著時的手動輪替**（`keyGeneration=2`）：產生一把代 2 的金鑰，`signs_from` 晚 120 秒，之後的 token 是它的 kid、被接受（升級的 init container 與 CronJob 可能同時執行，兩種先後都只會多出一把代 2）。
- **自動輪替開著時的撤銷**（`keyGeneration=3`、`revokeBeforeGeneration=3`）：自動產生的代 1 金鑰與代 2 都被移除，只剩代 3、立即簽章；example service 換上新的一組，token 被接受。
- **`maxKeyAgeSeconds: 0`**：升級後 CronJob 不存在，身分服務的 `TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS` 是 0；等到金鑰超過 900 秒再多 90 秒，金鑰沒有換；再重啟身分服務（init container 不依年齡輪替），仍然沒有換。
- **沒有私鑰外流**：每一把私鑰都不在 ConfigMap、事件、身分服務（含 init container）與 example service 的日誌，以及測試期間讀到的每一次 CronJob 執行的日誌；每一次執行都成功。

開始前同名的 release 或保留的物件已存在時直接失敗；結束時移除 release 與這些物件（同 `identity-keys.e2e.ts`）。整個測試約 55 分鐘，大多在等金鑰到期與舊金鑰的移除時間。只讀的 kubectl 指令在連不上 API server 時每 5 秒重試、最多 5 分鐘（經 SSH 轉送連 lab 時網路可能中斷一陣子），改變叢集的指令不重試。前置條件與 `identity-keys.e2e.ts` 相同（上一節）。

執行（在 repo 根目錄）：

```sh
E2E_NAMESPACE=e2e-429 \
E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:429 \
E2E_IDENTITY_IMAGE=localhost/trustiot-e2e/identity:429 \
E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:429 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:429 \
E2E_IMAGE_PULL_POLICY=Never \
bun test --timeout 3600000 ./deploy/chart/tests/e2e/key-rotation.e2e.ts
```

環境變數與 `identity-keys.e2e.ts` 相同（上一節的表），只有 `E2E_REPORT_DIR` 寫出的檔名是 `key-rotation-e2e-<shape>.json`（kid、代、時間、每次 CronJob 執行的變更與 token 的結果，沒有任何金鑰或 token）。
