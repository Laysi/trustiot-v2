# 展示機：以 Flux 部署展示環境

對應 issue：Laysi/trustiot-v2#467（父 issue Laysi/trustiot-v2#457）。

測試日期為 2026-10-22。下方 log 的時間戳記一律是 UTC。

## 環境

- **展示機**：lab 的 Mac mini M1。
  - 系統：Fedora Linux Asahi Remix 44（Server Edition），aarch64。
  - Kubernetes：k3s `v1.36.5+k3s1`。
  - 下文以 `<demo-ssh-host>` 代表它的 ssh 主機。
  - 安裝前的狀態：Laysi/trustiot-v2#465 清掉可行性測試的殘留、平台元件對齊 repo 的釘選（`docs/test-reports/2026-10-21-465-demo-lab-cleanup.md`）；namespace `flux-system` 只有 deploy/flux/README.md「展示叢集的 age 金鑰」那一步建立的 Secret `sops-age`，叢集裡沒有 Flux 的 CRD。
- **執行腳本的地方**：工作站。
  - 指令都在 main 的乾淨 checkout 執行，經 ssh 操作展示機；`flux.ts` 在 checkout 不乾淨或不等於 `origin/main` 時拒絕。
  - `deploy-key` 與 `install` 的 `--apply` 由人執行，都在 lab lock 內；`install` 時 main 是 `1bda1d98864eb2b3658c87909235613125431e49`。
  - 最後一次 `status` 時 main 是 `2625a67ea22ec669c05dc7d3954f731ca7dfb090`。這之間合併的六個 PR 都沒有改 `deploy/flux/`（`git diff --stat 1bda1d98 2625a67e -- deploy/flux/` 沒有輸出）。
- **紀錄**：留在執行者工作站的 `$XDG_STATE_HOME/trustiot-v2/demo/`，不進 git。本報告摘錄其中的相關行；省略的有確認碼（只對當時的計畫有效）、deploy key 的指紋（寫成 `SHA256:<指紋>`；各次執行印出的都相同，也與 GitHub 上那把 deploy key 的相同，見 Laysi/trustiot-v2#467 的留言）、PersistentVolume 名稱與 Pod 名稱的亂數字尾。
- **版本**：
  - Flux：`.prototools` 的 `flux = "2.9.6"`，manifest 是 repo 裡的 `deploy/flux/clusters/demo/flux-system`。
  - chart：main 上 `deploy/flux/clusters/demo/trustiot/chart.yaml` 釘選的 `0.0.0-rc.10`，OCI digest `sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68`。這一版從 main 的 `d77ff65` 發佈，包含 Laysi/trustiot-v2#478（入口 Service 的固定 ClusterIP）。
  - 入口的 ClusterIP：`deploy/flux/clusters/demo/trustiot/values-demo.yaml` 的 `ingress.tunnel.clusterIP: 10.43.0.80`。

## 指令

用法與每一步會動到什麼，見 deploy/flux/README.md「在展示機安裝 Flux」與 `scripts/README.md` 的「`demo/flux.ts`：在展示機安裝 Flux」。依序執行：

```sh
bun scripts/demo/flux.ts deploy-key --ssh-host <demo-ssh-host>                                # 1. 計畫，只讀
bun scripts/demo/flux.ts deploy-key --ssh-host <demo-ssh-host> --apply --confirm <確認碼>      # 2. 在展示機產生 deploy key
bun scripts/demo/flux.ts install    --ssh-host <demo-ssh-host>                                # 4. 計畫，只讀
bun scripts/demo/flux.ts install    --ssh-host <demo-ssh-host> --apply --confirm <確認碼>      # 5. 安裝 Flux 與展示環境
bun scripts/demo/flux.ts status     --ssh-host <demo-ssh-host>                                # 6. 確認，只讀
bun scripts/demo/tunnel.ts route-origin --ssh-host <demo-ssh-host>                            # 7. 入口位址的比對，只讀
```

第 3 步在 GitHub 操作，由有 repo 管理權限的人執行：第 2 步印出的公鑰以 `POST /repos/Laysi/trustiot-v2/keys`（`read_only: true`）加成 deploy key `trustiot-demo-flux`，再以 `GET /repos/Laysi/trustiot-v2/keys` 讀回，確認 `read_only` 是 `true`，以 GitHub 存的公鑰算出的指紋與腳本印出的相同。

## 結果

### deploy key（第 1 到 3 步，2026-10-22）

- `deploy-key --apply` 在展示機建立 Secret `flux-system/flux-system`（鍵 `identity`、`identity.pub`、`known_hosts`，label `trustiot.io/demo-flux=deploy-key`），只印出公鑰與指紋 `SHA256:<指紋>`。私鑰在展示機上以 root 產生，只經過管線、root 才能讀的暫存檔（用完以 shred 刪除）與 Kubernetes Secret。
- GitHub：repo 只有這一把 deploy key，`read_only` 是 `true`，指紋相同。

### 安裝（第 4、5 步）

dry run 的計畫（只讀，結束碼 0）：

```text
plan, from main 1bda1d98864e: chart 0.0.0-rc.10 (sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68), ingress ClusterIP 10.43.0.80, Flux's manifests sha256 17d86823beb6
  1. label namespace flux-system trustiot.io/demo-flux=installed (field manager trustiot-demo-flux), so later runs, remove and lab e2e can tell this install
  2. server-side apply 26 objects of Flux with GitRepository flux-system/flux-system pinned to 1bda1d98864eb2b3658c87909235613125431e49 (spec.ref.commit; bootstrap sha256 789a34813874), wait for its 8 CRDs, apply again; apply the pin as trustiot-demo-flux too, so Flux keeps it; then create the root Kustomization. Cluster-wide: <8 個 Flux CRD、3 個 ClusterRole、2 個 ClusterRoleBinding>
  3. wait for the controllers (helm-controller, kustomize-controller, source-controller); GitRepository flux-system/flux-system and Kustomization flux-system at main@sha1:1bda1d98864eb2b3658c87909235613125431e49 (the deploy key SHA256:<指紋> must be on GitHub, read-only), stopping at once if the pin changes or Flux reports another commit; OCIRepository trustiot/trustiot at sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68
  4. request a reconcile of HelmRelease trustiot/trustiot (annotation reconcile.fluxcd.io/requestedAt, field manager trustiot-demo-flux) and wait until it is handled and the release is deployed at chart 0.0.0-rc.10+f18f3fe0cabc
  5. remove the pin (a JSON patch that tests it), so Flux follows main again, and wait until GitRepository flux-system/flux-system is Ready at a commit of main
  6. check that nothing of Flux is suspended; report everything
```

`--apply`（結束碼 0）的過程，省略逐一物件的 `serverside-applied` 行：

| 時間（UTC） | 事件 |
| --- | --- |
| 07:03:45 | 取得 lab lock，擁有者 `Laysi/trustiot-v2#467 2026-10-22T07:03:43Z demo install` |
| 07:04:42 | namespace `flux-system` 加上 label `trustiot.io/demo-flux=installed` |
| 07:04:46 | Flux 的 26 個物件套用兩次（第一次在 CRD 生效前，預期不完整），GitRepository 釘在 `1bda1d98`，建立 root Kustomization |
| 07:05:05–07:05:09 | 三個 controller 都是 1/1 可用 |
| 07:05:09 | GitRepository `flux-system/flux-system` 在 `main@sha1:1bda1d98…` `Ready=True Succeeded` |
| 07:05:10 | Kustomization `flux-system/flux-system` 在同一個 commit `Ready=True ReconciliationSucceeded` |
| 07:05:20 | OCIRepository `trustiot/trustiot` 在 `sha256:f18f3fe0…` `Ready=True Succeeded` |
| 07:05:21 | 對 HelmRelease `trustiot/trustiot` 要求 reconcile |
| 07:06:12 | HelmRelease `Ready=True InstallSucceeded`：`Helm install succeeded for release trustiot/trustiot.v1 with chart trustiot@0.0.0-rc.10+f18f3fe0cabc` |
| 07:06:13 | 拿掉釘選：`Flux follows main again` |
| 07:06:30 | GitRepository 跟著 main，仍在 `main@sha1:1bda1d98…`（main 沒有前進）`Ready=True` |
| 07:06:31–07:06:36 | 最後的報告（見下） |
| 07:06:53 | 叢集層級的變更（見下） |
| 07:06:54 | 釋放 lab lock；維運用的 tunnel 服務沒有變動（同一個行程） |

最後的報告：

```text
Deployment flux-system/helm-controller: 1/1 available, 1 updated, ghcr.io/fluxcd/helm-controller:v1.6.5@sha256:0d52fff5c4d476277b8fcb6beb9041e269adb5db943fe69f5a806ea0c92b1511
Deployment flux-system/kustomize-controller: 1/1 available, 1 updated, ghcr.io/fluxcd/kustomize-controller:v1.9.6@sha256:2ebeaa341da77d52b6abbbba5efcee0450d47f8b42f0e6f33b08f9020262d606
Deployment flux-system/source-controller: 1/1 available, 1 updated, ghcr.io/fluxcd/source-controller:v1.9.6@sha256:6a6693172589f8ff26123a231d5fa6ceb194a6efb4dc647cdf057c959f76a2e3
GitRepository flux-system/flux-system: Ready=True Succeeded: stored artifact for revision 'main@sha1:1bda1d98864eb2b3658c87909235613125431e49', revision main@sha1:1bda1d98864eb2b3658c87909235613125431e49
Kustomization flux-system/flux-system: Ready=True ReconciliationSucceeded: Applied revision: main@sha1:1bda1d98864eb2b3658c87909235613125431e49, revision main@sha1:1bda1d98864eb2b3658c87909235613125431e49, chart  ()
OCIRepository trustiot/trustiot: Ready=True Succeeded: stored artifact for digest 'sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68', revision sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68
HelmRelease trustiot/trustiot: Ready=True InstallSucceeded: Helm install succeeded for release trustiot/trustiot.v1 with chart trustiot@0.0.0-rc.10+f18f3fe0cabc, chart 0.0.0-rc.10+f18f3fe0cabc (deployed)
main's deploy/flux/clusters/demo/trustiot/chart.yaml: 0.0.0-rc.10@sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68
  pulled:   the same
  deployed: the same (0.0.0-rc.10+f18f3fe0cabc (deployed))
pods in trustiot: 8
```

namespace `trustiot` 的 8 個 Pod（名稱省略亂數字尾）：

| Pod | 狀態 |
| --- | --- |
| `trustiot-assets` | Running，ready，重啟 0 次 |
| `trustiot-clickhouse-0` | Running，ready，重啟 0 次 |
| `trustiot-identity` | Running，ready，重啟 0 次 |
| `trustiot-ingress-istio`（2 個） | Running，ready，重啟 0 次 |
| `trustiot-migrate`（Job） | Succeeded |
| `trustiot-otel-collector` | Running，ready，重啟 0 次 |
| `trustiot-postgres-1` | Running，ready，重啟 0 次 |

叢集層級的變更（安裝前後比對），與計畫相同：

| 種類 | 新增 | 移除 |
| --- | --- | --- |
| Namespace | `trustiot` | 無 |
| PersistentVolume | 2 個（ClickHouse 與 Postgres 的 PVC） | 無 |
| CustomResourceDefinition | Flux 的 8 個：`buckets`、`externalartifacts`、`gitrepositories`、`helmcharts`、`helmrepositories`、`ocirepositories`（`source.toolkit.fluxcd.io`），`helmreleases.helm.toolkit.fluxcd.io`，`kustomizations.kustomize.toolkit.fluxcd.io` | 無 |
| ClusterRole | `crd-controller-flux-system`、`flux-edit-flux-system`、`flux-view-flux-system` | 無 |
| ClusterRoleBinding | `cluster-reconciler-flux-system`、`crd-controller-flux-system` | 無 |

### 確認（第 6、7 步）

`status`（只讀，07:11:44，main `1bda1d98`，結束碼 0）：

```text
lab lock: free
namespace flux-system: installed by this script (trustiot.io/demo-flux=installed)
  Secret flux-system: type Opaque, keys identity, identity.pub, known_hosts, trustiot.io/demo-flux=deploy-key
  Secret sops-age: type Opaque, keys age.agekey
  deploy key fingerprint SHA256:<指紋>
GitRepository flux-system/flux-system: Ready=True Succeeded: stored artifact for revision 'main@sha1:1bda1d98864eb2b3658c87909235613125431e49', revision main@sha1:1bda1d98864eb2b3658c87909235613125431e49
Kustomization flux-system/flux-system: Ready=True ReconciliationSucceeded: Applied revision: main@sha1:1bda1d98864eb2b3658c87909235613125431e49, revision main@sha1:1bda1d98864eb2b3658c87909235613125431e49, chart  ()
OCIRepository trustiot/trustiot: Ready=True Succeeded: stored artifact for digest 'sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68', revision sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68
HelmRelease trustiot/trustiot: Ready=True InstallSucceeded: Helm install succeeded for release trustiot/trustiot.v1 with chart trustiot@0.0.0-rc.10+f18f3fe0cabc, chart 0.0.0-rc.10+f18f3fe0cabc (deployed)
main's deploy/flux/clusters/demo/trustiot/chart.yaml: 0.0.0-rc.10@sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68
  pulled:   the same
  deployed: the same (0.0.0-rc.10+f18f3fe0cabc (deployed))
pods in trustiot: 8
cloudflared.service kept running (same process)
```

三個 controller 與 Pod 的行和安裝時的最後報告相同，上面省略。Secret 只以名稱、型別、鍵名與 label 讀取，不讀內容。

`route-origin`（只讀，07:12:10，結束碼 0）：

```text
ingress address in deploy/flux/clusters/demo/trustiot/values-demo.yaml at 1bda1d98864e (ingress.tunnel.clusterIP): 10.43.0.80
ingress address in ConfigMap trustiot-ingress-options (service overlay): 10.43.0.80
ingress address of the Service with gateway.networking.k8s.io/gateway-name=trustiot-ingress: 10.43.0.80
static band of the cluster's Service range (ServiceCIDR kubernetes): 10.43.0.1-10.43.1.0 of 10.43.0.0/16
OK the three ingress addresses agree and are in the static band
cloudflared.service kept running (same process)
the stage-2 routes' origin: http://10.43.0.80:80
```

### main 前進之後（07:25:12）

安裝之後 main 合併了六個 PR（`1bda1d98` 到 `2625a67e`），都沒有改 `deploy/flux/`。再跑一次 `status`（只讀，結束碼 0）：

```text
GitRepository flux-system/flux-system: Ready=True Succeeded: stored artifact for revision 'main@sha1:2625a67ea22ec669c05dc7d3954f731ca7dfb090', revision main@sha1:2625a67ea22ec669c05dc7d3954f731ca7dfb090
Kustomization flux-system/flux-system: Ready=True ReconciliationSucceeded: Applied revision: main@sha1:2625a67ea22ec669c05dc7d3954f731ca7dfb090, revision main@sha1:2625a67ea22ec669c05dc7d3954f731ca7dfb090, chart  ()
OCIRepository trustiot/trustiot: Ready=True Succeeded: stored artifact for digest 'sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68', revision sha256:f18f3fe0cabc24b928168c3d553b73632b7cb23855572689aba1d2aaa8a89e68
HelmRelease trustiot/trustiot: Ready=True InstallSucceeded: Helm install succeeded for release trustiot/trustiot.v1 with chart trustiot@0.0.0-rc.10+f18f3fe0cabc, chart 0.0.0-rc.10+f18f3fe0cabc (deployed)
```

Flux 跟著 main 前進；chart 的釘選沒有變，release 仍是同一個版本，沒有重新部署（`trustiot.v1`）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 展示機由 Flux 部署指定版本（含 Laysi/trustiot-v2#478 的版本）的 chart：以 main 上的 `scripts/demo/flux.ts` 安裝，`flux.ts status` 顯示每個 Flux 物件在 main 的 commit 與釘選的 chart `Ready`，入口 Service 的 ClusterIP 等於 values-demo.yaml 設定的位址 | 通過：`install --apply` 結束碼 0；`status` 顯示 GitRepository、Kustomization 在 main 的 commit、OCIRepository 在釘選的 digest、HelmRelease 部署 `0.0.0-rc.10` 都是 `Ready=True`；`route-origin` 確認三處的入口位址都是 `10.43.0.80` |
| 改版與 revert 的實測 | 待測：2026-10-21 依驗證分級移到 Laysi/trustiot-v2#474（P2）；lab 叢集的 Flux e2e 已測過部署、revert 與簽章拒絕 |
| 密鑰不進 repo（deploy key 的私鑰與 `sops-age` 不離開展示機） | 通過：私鑰只在展示機上產生與保存，log 只有公鑰指紋與 Secret 的鍵名；本報告的 PR 通過 `bun scripts/ci/check-secret-handling.ts`，gitleaks 掃 branch 與 tree 都沒有發現（結果記在 PR） |
