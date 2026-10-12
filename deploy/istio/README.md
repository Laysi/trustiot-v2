# Istio ambient（平台元件）

TrustIoT 的入口 gateway 與服務間 mTLS 用 Istio ambient（決策 27）。Istio 是整個叢集共用的平台元件（CRD、istiod、istio-cni、ztunnel 都是叢集層級），所以不放進 `deploy/chart`，每個叢集在裝 chart 之前先依本頁裝一次。chart 只產生 Gateway、HTTPRoute、AuthorizationPolicy 等 namespace 內的物件，見 [chart 的「入口 gateway」](../chart/README.md#入口-gateway)。

| 檔案 | 內容 |
| --- | --- |
| `values-k3s.yaml` | k3s（SaaS、實驗機、地端 k3s）：`profile: ambient`、`global.platform: k3s` |
| `values-openshift.yaml` | OpenShift：`profile: ambient`、`global.platform: openshift` |
| `istiod-values.yaml` | mesh 的路徑正規化（`DECODE_AND_MERGE_SLASHES`）、istiod 的 ext_authz provider `trustiot-identity`（呼叫身分服務），與 istiod 的資源需求 |

## 版本與前置條件

- Istio 1.30.5（實驗機與 SaaS），四個官方 Helm chart：`base`、`istiod`、`cni`、`ztunnel`，repo 為 `https://istio-release.storage.googleapis.com/charts`。Istio 約每 3 個月一版，升級時四個 chart 一起升（docs/roadmap/risks.md）。
- Gateway API 的 standard channel CRD。k3s 內建的 `traefik-crd` 已經附帶（實驗機是 v1.6.1）；其他叢集沒有時，先套用 Gateway API 官方 release 的 `standard-install.yaml`，版本與 Istio 1.30 的支援範圍相符。
- k3s 安裝時關閉內建的 Traefik（決策 27；主機安裝腳本見 Laysi/trustiot-v2#64）。

## 安裝（k3s）

在可以連到叢集的機器上，helm 用 repo 鎖定的版本（`proto install`）：

```sh
helm repo add istio https://istio-release.storage.googleapis.com/charts
helm install istio-base istio/base -n istio-system --create-namespace --version 1.30.5 -f deploy/istio/values-k3s.yaml --wait
helm install istiod istio/istiod -n istio-system --version 1.30.5 -f deploy/istio/values-k3s.yaml -f deploy/istio/istiod-values.yaml --wait
helm install istio-cni istio/cni -n istio-system --version 1.30.5 -f deploy/istio/values-k3s.yaml --wait
helm install ztunnel istio/ztunnel -n istio-system --version 1.30.5 -f deploy/istio/values-k3s.yaml --wait
```

k3s 也可以把同樣的 values 放進 k3s 的 `HelmChart` 資源（`valuesContent`），由 k3s 內建的 helm-controller 安裝，實驗機就是這樣裝的（`~/stack/platform/istio.yaml`）。之後要改 istiod 的設定時，用同名的 `HelmChartConfig` 疊在上面，不必改原檔；刪掉 `HelmChartConfig` 就回到原本的設定（範例見 [docs/test-reports/data/2026-10-10-62-ingress-e2e/lab-istiod-helmchartconfig.yaml](../../docs/test-reports/data/2026-10-10-62-ingress-e2e/lab-istiod-helmchartconfig.yaml)）。

## 路徑正規化

`istiod-values.yaml` 設 `meshConfig.pathNormalization.normalization: DECODE_AND_MERGE_SLASHES`。mesh 裡每個 Envoy（入口 gateway、waypoint）在路由與 AuthorizationPolicy 比對之前，先把路徑的 `%2F`、`%5C`（大小寫不拘）解碼成 `/`、`\`，依 RFC 3986 處理 `.` 與 `..`，再把連續的 `/` 合併成一個，之後轉給後端的也是正規化後的路徑。所以 Envoy 比對的路徑與後端看到的路徑相同。

為什麼需要（Laysi/trustiot-v2#255）：Istio 預設（`BASE`）只處理 `.` 與 `..`，不合併斜線、不解碼 `%2F`。chart 以 DENY `/internal*` 擋住服務之間的內部 API（決策 27），但 `//internal/tenants`、`/x/..//internal/tenants`、`/%2finternal/tenants` 這些寫法比對不到 `/internal*`，會落到 `/` 這類全包路由；後端若把它們整理成 `/internal/...`，帶有效 cookie 的使用者就呼叫得到內部 API（2026-10-10 獨立驗證的 F1，[報告](../../docs/test-reports/2026-10-10-62-e2e-verify.md)）。`MERGE_SLASHES` 擋得住雙斜線，擋不住 `%2f`；`DECODE_AND_MERGE_SLASHES` 全部擋下（2026-10-11 實測，[報告](../../docs/test-reports/2026-10-11-255-path-normalization.md)）。

影響：

- 這是 mesh 層級的設定，同一個叢集的所有 gateway 與 waypoint 都套用，包括其他系統的 gateway。ztunnel 只處理 L4，不受影響。
- 路徑裡的 `%2F` 一定會變成 `/`，路徑段落裡無法夾帶編碼的斜線。產品的路徑不受影響：路徑參數只有 ID，ID 是 UUID（`crates/contract/src/id.rs`，序列化成小寫、帶 `-` 的字串，不含 `/`），其餘路徑都是固定的英文小寫字；名稱、檔名、搜尋字串這類可能含 `/` 的值一律放在查詢字串或本文，不放在路徑。查詢字串不做正規化。新增 API 時也要遵守這條規則。
- 路徑比對仍區分大小寫，不處理過長 UTF-8（例如 `%c0%ae`）：`/INTERNAL` 不是 `/internal`。後端不能自行做大小寫或這類解碼的正規化（axum 的路由不會）。
- 改這個設定時 istiod 即時套用到所有 Envoy，不必重啟；回復方法與 provider 相同（見下節）。

## ext_authz provider

chart 的 AuthorizationPolicy（CUSTOM）以名稱 `ingress.authzProvider`（預設 `trustiot-identity`）引用 istiod 的 provider，兩邊名稱要一致。`istiod-values.yaml` 的 provider 指向 release `trustiot`、namespace `trustiot` 的身分服務 `trustiot-identity.trustiot.svc.cluster.local:8080`；release 名稱或 namespace 不同時改 `service`。同一個叢集裝多套 TrustIoT 時，每套各一個 provider（名稱不同），在各自的 values 設 `ingress.authzProvider`。

| 設定 | 值 | 為什麼 |
| --- | --- | --- |
| `failOpen` | `false` | 身分服務無法連線或出錯時拒絕請求，不放行 |
| `timeout` | `0.5s` | Istio 預設 600 秒；身分服務卡住時，請求在 0.5 秒內被拒絕 |
| `statusOnError` | `"503"` | 預設 403 與「沒有權限」相同；503 讓前端分得出「服務暫時不可用」與「該重新登入」（401）。Envoy 把身分服務回的 5xx 也當成出錯，所以身分服務查不到資料庫時回 503，入口一樣回這個值（經入口的實測在 Laysi/trustiot-v2#77） |
| `pathPrefix` | `/internal/ext-authz` | 身分服務的檢查端點，入口在後面接上原始路徑；入口不路由 `/internal`，外面呼叫不到 |
| `includeRequestHeadersInCheck` | `[cookie]` | 只把 cookie 交給身分服務（Host、方法、路徑與 Authorization 一律附帶） |
| `headersToUpstreamOnAllow` | `[x-internal-identity]` | 允許時以身分服務回的值覆寫用戶端帶來的同名標頭，所以身分服務允許時一定要回這個標頭（決策 27） |

2026-10-10 在實驗機實測：身分服務停機時需要 cookie 的路徑回 503，身分服務延遲 3 秒時約 0.5 秒回 503，例外路徑照常（[測試報告](../../docs/test-reports/2026-10-10-62-ingress-e2e.md)）。

provider 與路徑正規化都屬於 istiod 的 mesh 設定，改動時 istiod 會即時套用，不必重啟；已在用的入口不受影響。回復時把 provider 從 values 移除再 `helm upgrade istiod`（k3s 的 `HelmChartConfig` 則直接刪除）；chart 的 AuthorizationPolicy 引用不存在的 provider 時，入口對所有需要 cookie 的請求回 403，例外路徑照常，不會放行（2026-10-10 實測）。

## OpenShift

- 用 `values-openshift.yaml` 取代 `values-k3s.yaml`，其他步驟相同。`global.platform: openshift` 讓 istio-cni 用 OpenShift 的 CNI 路徑，並套用 istio-cni 與 ztunnel 需要的 SCC 設定；istio-cni 與 ztunnel 需要特權，由叢集管理員安裝。
- 客戶已用 OpenShift Service Mesh 3（Sail operator）時，不另裝 Istio：在 `Istio` 資源設 `spec.profile: ambient`，並把 `istiod-values.yaml` 的 `meshConfig.pathNormalization` 與 `meshConfig.extensionProviders` 放進 `spec.values.meshConfig`，`IstioCNI` 與 `ZTunnel` 資源同樣用 ambient。
- 入口 gateway 的 pod 由 Istio 產生；OpenShift 的 restricted-v2 SCC 會指派 UID。chart 本身的 pod 不固定 UID（見 chart README）。
- 目前沒有 OpenShift 環境，以上設定未實測。
