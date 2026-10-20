# 展示機：清除可行性測試的殘留，平台元件對齊 repo 的釘選

對應 issue：Laysi/trustiot-v2#465（父 issue Laysi/trustiot-v2#457）。

測試日期為 2026-10-21（展示機與工作站的當地日期）。下方 log 的時間戳記一律是 UTC，所以顯示為 2026-10-20T15:40Z 到 17:10Z。

## 環境

- **展示機**：lab 的 Mac mini M1。
  - 系統：Fedora Linux Asahi Remix 44（Server Edition），aarch64。
  - Kubernetes：k3s `v1.36.5+k3s1`。腳本與下方的檢查都以 `sudo k3s kubectl` 操作。
  - 下文以 `<demo-ssh-host>` 代表它的 ssh 主機。
- **執行腳本的地方**：工作站。
  - 指令都從固定在 commit `38b67353b4185c6056252fd56952fc5e05958bf0`（分支 `feat/465-demo-lab-cleanup`）的 detached checkout 執行，經 ssh 操作展示機。
  - 會改叢集的兩次 `--apply` 由人執行（2026-10-20 的決定），都在 lab lock 內。
- **紀錄**：留在執行者工作站的 `$XDG_STATE_HOME/trustiot-v2/demo/`，不進 git。
  - 內容：每次執行的 log、計畫、變更前後的叢集狀態，以及 istiod HelmChart 的備份。
  - 本報告摘錄其中的相關行。省略的有：
    - 確認碼（只對當時的計畫有效）；
    - PersistentVolume 名稱與各物件的 uid（只記數量）。
- **repo 的釘選**：`.github/workflows/nightly.yml` 的 `jobs.smoke.env`：
  - `ISTIO_VERSION: 1.30.5`
  - `GATEWAY_API_VERSION: v1.6.1`
  - `CNPG_VERSION: 1.30.1`
  - istiod 的 values 是 `deploy/istio/values-k3s.yaml` 加上 `deploy/istio/istiod-values.yaml`。

## 指令

用法與每一步會動到什麼，見 `scripts/README.md` 的「展示機（`demo/`）」。依序執行：

```sh
bun scripts/demo/cleanup.ts  --ssh-host <demo-ssh-host>                                   # 1. 計畫，只讀
bun scripts/demo/cleanup.ts  --ssh-host <demo-ssh-host> --apply --confirm <cleanup 的確認碼>   # 2. 刪除 PoC
bun scripts/demo/platform.ts --ssh-host <demo-ssh-host>                                   # 3. cleanup 之後：比對與計畫，只讀
bun scripts/demo/platform.ts --ssh-host <demo-ssh-host> --apply --confirm <platform 的確認碼>  # 4. 換掉 istiod 的 provider
bun scripts/demo/cleanup.ts  --ssh-host <demo-ssh-host>                                   # 5. 確認，只讀
bun scripts/demo/platform.ts --ssh-host <demo-ssh-host>                                   # 6. 確認，只讀
```

腳本之外，另以下列唯讀指令直接查看叢集（2026-10-21，套用之後）：

```sh
ssh <demo-ssh-host> 'sudo k3s kubectl get namespace trustiot poc istio-ingress'
ssh <demo-ssh-host> 'sudo k3s kubectl get gateways.gateway.networking.k8s.io -A'
ssh <demo-ssh-host> 'sudo k3s kubectl get gateways.networking.istio.io -A'
ssh <demo-ssh-host> 'sudo k3s kubectl get persistentvolumes'
ssh <demo-ssh-host> 'sudo k3s kubectl -n istio-system get configmap istio -o jsonpath="{.data.mesh}"' > mesh.yaml
ssh <demo-ssh-host> 'sudo k3s kubectl -n kube-system get helmchartconfigs.helm.cattle.io'
ssh <demo-ssh-host> 'sudo k3s kubectl -n kube-system get helmcharts.helm.cattle.io -o custom-columns=NAME:.metadata.name,CHART:.spec.chart,VERSION:.spec.version'
ssh <demo-ssh-host> 'sudo k3s kubectl -n istio-system get deployment/istiod daemonset/ztunnel daemonset/istio-cni-node -o custom-columns=NAME:.metadata.name,IMAGE:.spec.template.spec.containers[0].image'
ssh <demo-ssh-host> "sudo k3s kubectl get crd gateways.gateway.networking.k8s.io httproutes.gateway.networking.k8s.io -o jsonpath='{range .items[*]}{.metadata.name}{\"\t\"}{.metadata.annotations.gateway\.networking\.k8s\.io/bundle-version}{\"\t\"}{.metadata.annotations.gateway\.networking\.k8s\.io/channel}{\"\n\"}{end}'"
ssh <demo-ssh-host> "sudo k3s kubectl get gatewayclass istio -o jsonpath='{.status.conditions[?(@.type==\"Accepted\")].status}'"
ssh <demo-ssh-host> 'sudo k3s kubectl -n cnpg-system get deployment cnpg-controller-manager -o custom-columns=NAME:.metadata.name,IMAGE:.spec.template.spec.containers[0].image,AVAILABLE:.status.availableReplicas'
```

讀回的 mesh config 與 repo 的 `istiod-values.yaml` 比對時，鍵的順序不計（`stableJson` 會排序鍵）。在 repo 根目錄執行：

```sh
bun -e '
import { stableJson } from "./scripts/demo/lib.ts";
const repo = Bun.YAML.parse(await Bun.file("deploy/istio/istiod-values.yaml").text()).meshConfig;
const mesh = Bun.YAML.parse(await Bun.file("mesh.yaml").text());
console.log("extensionProviders equal:", stableJson(repo.extensionProviders) === stableJson(mesh.extensionProviders));
console.log("pathNormalization equal:", stableJson(repo.pathNormalization) === stableJson(mesh.pathNormalization));'
```

## 結果

### 1. cleanup dry run（15:40Z）

計畫刪除三個 namespace：

| namespace | 物件數（不含被其他物件擁有的） | PersistentVolumeClaim |
| --- | --- | --- |
| `istio-ingress` | 8（含 Gateway `gateway.gateway.networking.k8s.io/edge`） | 無 |
| `trustiot`（雲端 PoC） | 31 | `data-clickhouse-0`、`data-postgres-0`、`data-qdrant-0`、`mosq-data` |
| `poc`（閘道器端 PoC，含 probe-a、probe-b、probe-c） | 39 | `mosquitto-poc-data` |

- `deleteObjects`、`deleteBindings`、`keep`、`refused` 都是空的：沒有 ClusterRoleBinding、RoleBinding、webhook、APIService 或 CRD conversion 指向這三個 namespace。
- 15:37Z 的另一次 dry run，以及 16:33Z `--apply` 在 lock 內重新規劃的結果，都與這份計畫逐位元組相同。

### 2. cleanup `--apply`（16:33Z 到 16:35Z）

```text
16:33:27Z took the lab lock: Laysi/trustiot-v2#465 2026-10-20T16:33:27Z demo cleanup
16:33:47Z deleted gateway.gateway.networking.k8s.io/edge in istio-ingress
16:33:53Z deleting namespace istio-ingress (8 objects; volume claims: none)
16:34:01Z deleted namespace istio-ingress
16:34:08Z deleting namespace trustiot (31 objects; volume claims: data-clickhouse-0, data-postgres-0, data-qdrant-0, mosq-data)
16:34:27Z deleted namespace trustiot
16:34:34Z deleting namespace poc (39 objects; volume claims: mosquitto-poc-data)
16:35:27Z deleted namespace poc
16:35:28Z the persistent volumes of the 5 removed claims are deleted
16:35:37Z cluster-scoped changes: {"namespaces":{"added":[],"removed":["istio-ingress","poc","trustiot"],"changed":true},"persistentvolumes":{"added":[],"removed":[<5 個 PV>],"changed":true}}
16:35:37Z workloads and services removed: 29 (outside the plan: 0); added: 0
16:35:45Z released the lab lock
16:35:45Z cloudflared.service kept running (same process)
```

叢集層級的變化只有這 3 個 namespace 與 5 個 PV。其他 namespace 的 workload 與 Service 沒有任何移除或新增。

### 3. platform dry run（16:58Z）

10 項 `ok`，沒有 `DIFFERENCE` 或 `BLOCKED`，叢集裡沒有 Gateway 或 waypoint（`Gateways and waypoints in the cluster ...: none`）。唯一的變更是 HelmChart `kube-system/istiod` 的 values：

| | ext_authz provider | 指向 | `pathPrefix` | `statusOnError` | `pathNormalization` |
| --- | --- | --- | --- | --- | --- |
| 改前（PoC） | `identity` | `identity.poc.svc.cluster.local:8080` | `/check` | 未設（預設 403） | 未設 |
| 改後（`istiod-values.yaml`） | `trustiot-identity` | `trustiot-identity.trustiot.svc.cluster.local:8080` | `/internal/ext-authz` | `"503"` | `DECODE_AND_MERGE_SLASHES` |

其餘欄位兩者相同：`failOpen: false`、`timeout: 0.5s`、`includeRequestHeadersInCheck: [cookie]`、`headersToUpstreamOnAllow: [x-internal-identity]`。values 的其他部分（`profile: ambient`、`global.platform: k3s`、pilot 的 resources）也不變。

### 4. platform `--apply`（16:59Z 到 17:00Z）

```text
16:59:37Z took the lab lock: Laysi/trustiot-v2#465 2026-10-20T16:59:36Z demo platform
17:00:04Z saved the HelmChart as it was: $XDG_STATE_HOME/trustiot-v2/demo/platform-2026-10-20T165918Z-helmchart-istiod-before.json (roll back with --rollback <that file>)
17:00:04Z patched HelmChart kube-system/istiod; waiting for k3s's helm-controller to upgrade istiod
17:00:11Z istiod serves the new mesh config
17:00:19Z cluster-scoped changes: {"helmcharts.helm.cattle.io":{"added":[],"removed":[],"changed":true}}
17:00:35Z istiod's mesh config carries deploy/istio/istiod-values.yaml's providers exactly
17:00:35Z released the lab lock
17:00:36Z cloudflared.service kept running (same process)
```

（log 印出的備份路徑是家目錄底下的 `~/...`，這裡改寫成 `$XDG_STATE_HOME` 的形式。）

### 5. 套用後的確認（17:09Z 與 17:10Z，只讀，不取 lock）

- **cleanup**：`keep` 是 `namespace trustiot: already gone`、`namespace poc: already gone`、`namespace istio-ingress: already gone`，結尾為 `nothing to remove: the PoC's leftovers are gone`。
- **platform**：12 項 `ok`，沒有 `DIFFERENCE` 或 `BLOCKED`，結尾為 `istiod's values match the repository; nothing to change`。比步驟 3 多出兩項：
  - `istiod: values are deploy/istio/values-k3s.yaml plus istiod-values.yaml`
  - `istiod's mesh config: provider trustiot-identity, path normalization DECODE_AND_MERGE_SLASHES`

每次執行（共 7 次）結尾都是 `cloudflared.service kept running (same process)`：展示機上維運用的 tunnel 沒有重啟。

### 6. 直接查看叢集（2026-10-21，套用之後，只讀）

**PoC 已移除：**

```text
$ kubectl get namespace trustiot poc istio-ingress
Error from server (NotFound): namespaces "trustiot" not found
Error from server (NotFound): namespaces "poc" not found
Error from server (NotFound): namespaces "istio-ingress" not found
$ kubectl get gateways.gateway.networking.k8s.io -A
No resources found
$ kubectl get gateways.networking.istio.io -A
No resources found
$ kubectl get persistentvolumes
No resources found
$ kubectl -n kube-system get helmchartconfigs.helm.cattle.io
No resources found in kube-system namespace.
```

**istiod 的 mesh config（ConfigMap `istio-system/istio` 的 `mesh`，摘錄）：**

```yaml
extensionProviders:
- envoyExtAuthzHttp:
    failOpen: false
    headersToUpstreamOnAllow:
    - x-internal-identity
    includeRequestHeadersInCheck:
    - cookie
    pathPrefix: /internal/ext-authz
    port: 8080
    service: trustiot-identity.trustiot.svc.cluster.local
    statusOnError: "503"
    timeout: 0.5s
  name: trustiot-identity
pathNormalization:
  normalization: DECODE_AND_MERGE_SLASHES
```

與 `deploy/istio/istiod-values.yaml` 比對（上方的 `bun -e`）：

```text
extensionProviders equal: true
pathNormalization equal: true
```

只有一個 provider，PoC 的 `identity` 已不在。HelmChart `istiod` 的註記 `trustiot.io/demo-platform-values-sha256` 等於目前 `spec.valuesContent` 的 SHA-256：values 是 `--apply` 寫入的那份，之後沒有被改過。

**版本與 repo 的釘選：**

| 元件 | 展示機 | repo 的釘選 | 一致 |
| --- | --- | --- | --- |
| HelmChart `istio-base`、`istio-cni`、`istiod`、`ztunnel` | chart 版本皆 1.30.5 | `ISTIO_VERSION` 1.30.5 | 是 |
| `deployment/istiod` 映像 | `registry.istio.io/release/pilot:1.30.5-distroless` | 1.30.5 | 是 |
| `daemonset/ztunnel` 映像 | `registry.istio.io/release/ztunnel:1.30.5-distroless` | 1.30.5 | 是 |
| `daemonset/istio-cni-node` 映像 | `registry.istio.io/release/install-cni:1.30.5-distroless` | 1.30.5 | 是 |
| istio-base、istio-cni、ztunnel 的 values | 與 `deploy/istio` 相同（platform.ts 的 `ok` 行） | `deploy/istio/values-k3s.yaml` | 是 |
| istiod 的 values | `values-k3s.yaml` 加 `istiod-values.yaml`（platform.ts 的 `ok` 行） | 同左 | 是（本次修改後） |
| CRD `gateways`、`httproutes.gateway.networking.k8s.io` | bundle-version `v1.6.1`，channel `standard` | `GATEWAY_API_VERSION` v1.6.1，standard channel | 是 |
| GatewayClass `istio` | `istio.io/gateway-controller`，Accepted `True` | — | 是 |
| CloudNativePG operator（`cnpg-system/cnpg-controller-manager`） | `ghcr.io/cloudnative-pg/cloudnative-pg:1.30.1`，available 1 | `CNPG_VERSION` 1.30.1 | 是 |

套用前（步驟 3 的 dry run）這些版本就已經一致。腳本沒有安裝或升級任何叢集層級的元件。

### 回復 istiod 的 provider

變更前的 HelmChart 原樣存在執行者工作站的 `$XDG_STATE_HOME/trustiot-v2/demo/platform-2026-10-20T165918Z-helmchart-istiod-before.json`，不進 git。要回復時，從同一個 commit 的 checkout 執行：

```sh
bun scripts/demo/platform.ts --ssh-host <demo-ssh-host> --rollback "$XDG_STATE_HOME/trustiot-v2/demo/platform-2026-10-20T165918Z-helmchart-istiod-before.json"
bun scripts/demo/platform.ts --ssh-host <demo-ssh-host> --rollback "$XDG_STATE_HOME/trustiot-v2/demo/platform-2026-10-20T165918Z-helmchart-istiod-before.json" --apply --confirm <第一行印出的確認碼>
```

- 第一行只讀，印出目前與要放回的 values，以及回復用的確認碼。
- 第二行在 lab lock 內以 compare-and-swap 把存下的 `valuesContent` 逐位元組放回，再等 istiod 的 mesh config 顯示回復的 provider。

以下情況腳本會拒絕回復：

- istiod 的 values 已不是上次 `--apply` 寫入的；
- 有 CUSTOM AuthorizationPolicy 還在用會被拿掉的 provider；
- 叢集裡有 Gateway 或 waypoint：回復會拿掉 `DECODE_AND_MERGE_SLASHES`，讓 `//internal/...` 與 `/%2finternal/...` 重新繞過 `/internal*` 的 DENY（Laysi/trustiot-v2#255）。

注意：回復後的 provider 指向 `identity.poc`，而那個 namespace 已經刪除。所以回復只用於排查，不是可用的設定。

## 結論

對照 Laysi/trustiot-v2#465 的驗收條件：

1. **腳本的單元測試涵蓋盤點、確認前不刪除與重複執行，不需要叢集；`bun test scripts` 通過：通過。**
   - 測試在 `scripts/demo/*.test.ts`，以記憶體裡的假叢集與假的 ssh 執行。
   - 盤點：`names objects the way kubectl deletes them, and marks owned and managed ones`、`the inventory lists only the filtered kinds, and stops when kubectl fails`。
   - 確認前不刪除：`--apply without the code, or with another plan's code, deletes nothing and takes no lock`，以及 platform 的 `--apply without the code, or with another one, changes nothing`。
   - 重複執行：`runs again: after the cleanup there is nothing to remove, and nothing changes`、`runs again: once istiod has the values there is nothing to change`。
   - 實際的重複執行也符合：步驟 5 兩支腳本都回報沒有要改的。
   - `bun test scripts` 的結果見 PR。
2. **展示機上沒有 namespace `poc`、PoC 的 namespace `trustiot` 與 `edge` Gateway，istiod 的 extension providers 與 `deploy/istio/istiod-values.yaml` 一致：通過。** 證據見結果 2、5、6：
   - 三個 namespace 都是 NotFound；
   - 叢集沒有任何 Gateway；
   - mesh config 的 `extensionProviders` 與 `pathNormalization` 和 `istiod-values.yaml` 相同。
3. **Istio、Gateway API CRD 與 CloudNativePG operator 的版本與 repo 的釘選一致：通過。** 見結果 6 的版本表，沒有差異需要處理。
4. **腳本的用法與會動到的叢集層級元件寫在 `scripts/README.md`：通過。** 見「展示機（`demo/`）」一節，其中：
   - 「清除 PoC 與平台對齊的順序」寫用法；
   - cleanup 的「叢集層級的影響」寫 namespace、PersistentVolume、ClusterRoleBinding 與其他 namespace 的 RoleBinding；
   - platform 的段落寫唯一會改的 HelmChart `kube-system/istiod`，並說明不安裝也不升級叢集層級元件。
