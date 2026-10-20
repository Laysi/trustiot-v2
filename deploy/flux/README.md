# SaaS 的 GitOps 部署（Flux）

SaaS 叢集由叢集內的 Flux 拉取部署（決策 5，Laysi/trustiot-v2#70）：Flux 每分鐘讀一次本 repo 的 main，套用本目錄，再從 GHCR 拉 chart 與映像。發版就是合併 release.yml 自動開的 deploy PR，回到前一版就是 revert 那個 PR。地端不裝 Flux，不用本目錄。

```text
GitHub main ──(唯讀 deploy key，SSH，每分鐘)──> source-controller ──> kustomize-controller ──> 本目錄的物件
                                                                    （以叢集的 age 私鑰解密 SOPS）
GHCR chart ──(唯讀拉取 token，驗 keyless cosign 簽章)──> source-controller ──> helm-controller ──> TrustIoT release
GHCR 映像 ──(同一個拉取 token)──> kubelet
```

- 叢集只讀：deploy key 與 GHCR token 都是唯讀的，叢集沒有任何能寫 GitHub 的憑證。
- GitHub 不持有叢集憑證：GitHub 的 secrets 與 environments 裡沒有 kubeconfig、叢集 token 或解密金鑰；release.yml 只用自己的 `GITHUB_TOKEN` 開 PR，不連叢集。主機也不必對外開放任何埠。
- 祕密以 SOPS 加 age 加密後放在本目錄（Laysi/trustiot-v2#72，見 [CI 與發佈](../../docs/architecture/ci-release.md) 的「SaaS 的祕密怎麼進叢集」）。

本目錄有兩個叢集：`clusters/saas` 是 SaaS 叢集（MS4），`clusters/demo` 是展示切片的展示環境（Laysi/trustiot-v2#457），以同樣的方式部署，差別見下方「展示叢集」。以下沒有特別說明的，兩個叢集都適用。

## 檔案

| 路徑 | 內容 | 誰改 |
| --- | --- | --- |
| `clusters/saas/kustomization.yaml` | 叢集套用的全部內容：Flux 本身與 TrustIoT | 人 |
| `clusters/saas/flux-system/gotk-components.yaml` | Flux 的 CRD 與控制器（`flux install --export`，只有 source、kustomize、helm 三個控制器） | `scripts/flux/components.ts` |
| `clusters/saas/flux-system/kustomization.yaml` | 控制器映像以 digest 釘選；digest 取自 cosign 驗證過的 fluxcd 官方 keyless 簽章 | `scripts/flux/components.ts` |
| `clusters/saas/flux-system/gotk-sync.yaml` | GitRepository（main，只取 `deploy/flux/`）與根 Kustomization（`prune`、SOPS 解密） | 人 |
| `clusters/saas/trustiot/chart.yaml` | chart 的 OCIRepository：版本、digest、拉取 token、簽章驗證 | release.yml 的 deploy PR |
| `clusters/saas/trustiot/values-saas.yaml` | 部署中那一版的 `deploy/chart/values-saas.yaml` | release.yml 的 deploy PR |
| `clusters/saas/trustiot/values-instance.yaml` | SaaS 安裝實例的覆寫值：主機名稱、外部端點、Secret 名稱、叢集內資料庫的磁碟區大小 | 人 |
| `clusters/saas/trustiot/helm-release.yaml`、`namespace.yaml` | HelmRelease 與 namespace（Pod Security `restricted`） | 人 |
| `clusters/<叢集>/trustiot/*.sops.yaml` | 以 SOPS 加密的 Secret，例如 `ghcr-pull.sops.yaml`；建立後要列進同目錄的 `kustomization.yaml` | 人（`scripts/flux/ghcr-pull-secret.ts`、`scripts/flux/sops-secret.ts`、展示叢集的私有 values 用 `scripts/flux/demo-values.ts`、`sops edit`） |
| `clusters/demo/` | 展示叢集：`flux-system` 引用 SaaS 的 Flux、只改同步路徑；`trustiot/` 是與 SaaS 相同的 HelmRelease、自己的 `chart.yaml` 與 `values-saas.yaml`，加上 `values-demo.yaml`（見下方「展示叢集」） | 人；`chart.yaml` 與 `values-saas.yaml` 同 SaaS（deploy PR），或 `scripts/flux/pin-chart.ts` |
| `scripts/flux/pin-chart.ts` | 把一個叢集的 `chart.yaml` 與 `values-saas.yaml` 寫成某一版，與 deploy PR 寫的逐字相同 | |
| `github-known-hosts` | GitHub 公布的 SSH 主機金鑰，給 GitRepository 的 Secret 用 | 人（GitHub 換金鑰時） |
| `tests/flux.test.ts` | `bun test deploy/flux`，見下方「檢查」 | |
| `tests/e2e/lab.e2e.ts` | 在實驗機上驗證 Flux 的整條路，見下方「lab 測試」 | |

## 發版與回滾

1. 推 `v*` tag，release.yml 發佈並驗證映像與 chart（[CI 與發佈](../../docs/architecture/ci-release.md) 的「release.yml」）。
2. verify 通過後，`deploy-pr` job（`scripts/release/deploy-pr.ts`）以 GitHub API 在 main 上做一個 commit，推到分支 `deploy/trustiot-<版本>`。commit 只動部署中的版本比新版舊的叢集（`scripts/release/flux.ts` 的 `DEPLOY_CLUSTERS`：`saas` 與 `demo`），每個叢集改兩個檔：`chart.yaml` 換成新版本與 publish 推送的 chart digest，`values-saas.yaml` 換成該 tag 的 `deploy/chart/values-saas.yaml`；接著開 PR，內文以表格列出每個叢集從哪一版換到哪一版，以及不變的叢集。候選版（`-rc.N`）開成 draft，內文提醒它不一定在 main 上。新版不比任何一個叢集部署中的版本新時（例如舊版線的修補版）不開 PR。main 上缺少某個叢集的 `chart.yaml`，或它被手改過時，`deploy-pr` 停下，不會略過那個叢集。重跑時已有的分支與 PR（包含已關閉的）不重開。分支在、PR 不在時，PR 內文依 main 現在的版本寫，所以分支改的檔案要剛好是 main 現在該移動的那幾個叢集的兩個檔，內容也要與這次會寫的逐字相同（`chart.yaml` 是這一版與這個 digest，`values-saas.yaml` 是這一版的 `deploy/chart/values-saas.yaml`）；不是的話（例如之後有人以 `pin-chart.ts` 移動了其中一個叢集）`deploy-pr` 停下，刪掉分支再重跑即可。
3. 審查後合併。Flux 在 1 分鐘內（GitRepository 的 `interval`）取得新的 commit，套用後 helm-controller 立刻升級；整個 Helm 升級的上限是 HelmRelease 的 `timeout`（10 分鐘），要等所有 workload 與 Job 完成。升級失敗時自動 rollback 到上一個成功的版本再重試，最多 3 次。
4. 回到前一版：revert deploy PR 並合併，Flux 以同樣的方式把舊版本裝回去；PR 動了幾個叢集，revert 就把那幾個叢集一起換回去。

只想移動其中一個叢集時（例如展示環境先換版，或只把一個叢集換回較舊的版本），不用 deploy PR，自己開 PR：

```sh
git fetch origin --tags
bun scripts/flux/pin-chart.ts --cluster <saas|demo> --version <版本> --digest <該版 GitHub Release 上的 chart digest>
```

它寫出與 deploy PR 逐字相同的兩個檔。digest 錯了只會讓 OCIRepository 驗證失敗，不會裝上別的 chart：Flux 仍只接受 release.yml 在該 tag 上的簽章。

### deploy PR 上的 CI

以 `GITHUB_TOKEN` 開的 PR 與推送不會觸發任何 workflow（GitHub 防止 workflow 互相觸發）。唯一的例外是 `workflow_dispatch`，所以 `deploy-pr` job 開完 PR 後以 `actions: write` 在該分支執行 ci.yml（`ci.yml` 因此多了 `workflow_dispatch` 觸發）。結果掛在分支的 head commit 上，PR 的 checks 看得到。合併前照常確認必要檢查全部通過（[CI 與發佈](../../docs/architecture/ci-release.md)「必要檢查與強制方式」）。

- 若沒有看到 ci 的結果（例如 dispatch 失敗，job 會失敗並寫出原因），由人關閉再重新開啟 PR：人觸發的 `reopened` 事件會跑 ci。
- 合併前確認 PR 只有一個 `github-actions[bot]` 的 commit，而且只改 `chart.yaml` 與 `values-saas.yaml`：有寫入權限的人都能再推 commit 到 `deploy/trustiot-<版本>` 分支（PR 內文也寫了這一條）。
- 沒有選的做法：GitHub App 或 PAT 的 token 開 PR 會觸發 workflow，但要在 GitHub 存一把能寫 repo 的長期憑證，違反「GitHub 不持有這類憑證」的原則（見 ci-release.md 的「Renovate」）；只推分支、由人開 PR，則不是「自動開 PR」。

### 需要的 repo 設定

GitHub 預設不讓 Actions 開 PR。repo 設定 **Settings → Actions → General → Workflow permissions → Allow GitHub Actions to create and approve pull requests**（API 欄位 `can_approve_pull_request_reviews`）打開時，`deploy-pr` 自己開 PR。

設定維持關閉（目前的狀態）時，GitHub 以「not permitted to create or approve pull requests」拒絕開 PR。這不算發版失敗，映像與 chart 已經發佈並驗證，所以 `deploy-pr` 仍然成功：

- 分支 `deploy/trustiot-<版本>` 照樣推好，不在分支上執行 ci.yml。
- job summary 寫出 compare 連結，以及 PR 的標題與內文，可以直接貼上；run 上有一則 warning annotation，同樣附上連結。
- 由人從 compare 連結開 PR（候選版開成 draft）。人開的 PR 由 `pull_request` 事件照常執行 ci.yml。
- 重跑時分支保留，結果相同。

其他錯誤（例如 API 失敗、token 權限不足、main 上的 `chart.yaml` 被手改）仍然讓 `deploy-pr` 失敗，release 的 commit status 也是失敗。

這個設定同時允許 workflow 以 `GITHUB_TOKEN` 核准 PR；[CI 與發佈](../../docs/architecture/ci-release.md)「GitHub 的 Actions 設定」原本把它設為 `false`。打開與否是人的決定（記錄於 Laysi/trustiot-v2#70），考量：

- main 目前沒有啟用分支保護或 ruleset：repo 2026-10-12 公開後可以啟用，但還沒有啟用，啟用與否由使用者決定（[CI 與發佈](../../docs/architecture/ci-release.md)「必要檢查與強制方式」）。所以核准不是合併的條件，workflow 能核准 PR 並不會讓它繞過任何強制關卡；合併前的確認由流程把關。
- 能取得 `pull-requests: write` 的只有 `deploy-pr` 這個 job，它不執行任何建置程式碼；fork PR 的 token 一律唯讀。
- main 啟用分支保護或 ruleset、並要求核准時，要重新檢視：那時 workflow 的核准就能滿足合併條件，所以要讓這個設定維持關閉（`deploy-pr` 只推分支），或改由 GitHub App 開 PR。

## 簽章驗證

`chart.yaml` 由 `scripts/release/flux.ts` 的 `renderChartSource` 產生：

- 以 tag 與 digest 拉 chart，digest 優先：GHCR 的 tag 可以被有寫入權限的人移動，digest 不行。
- 只接受 release.yml 在**這個 tag** 上的 keyless 簽章（issuer `https://token.actions.githubusercontent.com`，subject `https://github.com/Laysi/trustiot-v2/.github/workflows/release.yml@refs/tags/v<版本>`，皆為完全比對；決策 30）。別的 tag、別的 workflow 或別的 repo 簽的都不接受。驗證失敗時 OCIRepository 的 Ready 為 False，HelmRelease 不升級，正在跑的版本不受影響。
- 簽章證明的是「release.yml 在這個 tag 上建置並推送」，不是「來自 main」。正式版的 tag 必須在 main 上（release.yml 的 plan 檢查），所以執行的 release.yml 就是 main 上審查過的版本；候選版的 tag 可以在任何分支上，跑的是該分支的 release.yml，由推 tag 的人決定。所以候選版的 deploy PR 是 draft，合併前的審查才是把關。要收緊時，以 tag ruleset 限制誰能建立 `v*` tag：repo 設定，由使用者決定；ruleset 在 Free 方案的公開 repo 能不能用尚未驗證（[CI 與發佈](../../docs/architecture/ci-release.md)「必要檢查與強制方式」）。
- 需要 Flux 2.8 以上才讀得懂 cosign v3 的 bundle 格式；`.prototools` 釘的是 2.9.6。
- `bun test deploy/flux` 要求 `chart.yaml` 與 `renderChartSource` 對它自己的版本的輸出逐字相同，所以手改（拿掉 `verify`、放寬 subject、拿掉 digest）都會讓 CI 失敗；deploy-pr 遇到被手改過的 `chart.yaml` 也會停下，不會覆寫。
- Flux 的控制器映像以 digest 釘選；`scripts/flux/components.ts` 寫入 digest 前，以 cosign 驗證 fluxcd 官方 release workflow（`fluxcd/gha-workflows` 的 controller-release，由 `fluxcd/<控制器>` 的 tag 觸發）的 keyless 簽章。

Flux 只驗證它自己拉的 OCI 產物（chart）。服務映像由 kubelet 拉取，不經 Flux；映像的簽章驗證要另外的 admission controller，見 Laysi/trustiot-v2#70 的後續提案。

## 祕密

工具：`sops`（`.prototools` 釘選，`proto install sops`）與 age。age 的 release 只附 Sigsum 證明、沒有 checksum 檔，proto 的 schema 外掛無法驗證下載，所以改由 `.prototools` 釘選的 Go 從原始碼建置，下載的 module 由 Go checksum database（公開的透明紀錄）驗證：

```sh
proto install go sops
bun scripts/flux/age.ts age-keygen -o <私鑰檔>     # 產生金鑰對，印出公鑰
bun scripts/flux/age.ts age-keygen -y <私鑰檔>     # 從私鑰算出公鑰
bun scripts/flux/age.ts age -d -i <私鑰檔> <檔案>  # age 本身
```

第一次執行會建置 age（版本是 `scripts/flux/age.ts` 的 `AGE_VERSION`），快取在 `$XDG_CACHE_HOME/trustiot-v2/age/<版本>/`。

- 收件者在 repo 根目錄的 `.sops.yaml`，每個叢集一條規則：該叢集的 age 公鑰，加上每位維運人員的公鑰。一個叢集的金鑰解不開另一個叢集的檔案。收件者還沒填時 sops 拒絕加密（`empty key group provided`），不會產生沒有人解得開的檔案。每個收件者一行，註解只寫角色：`- age1… # cluster: demo` 或 `- age1… # operator`，不寫人名或機器名稱。`bun test deploy/flux` 檢查每條規則是空的，或剛好是自己叢集的金鑰加上每一把維運人員的金鑰，沒有別的叢集的金鑰。
- 編輯：`SOPS_AGE_KEY_FILE=<自己的私鑰檔> sops edit deploy/flux/clusters/<叢集>/trustiot/<名稱>.sops.yaml`。新增的 Secret 檔要列進同目錄 `kustomization.yaml` 的 `resources`（測試檢查兩邊一致）。
- 新增一般的 Secret（API key、帳密）：值只經 stdin，明文只在記憶體與 sops 的 stdin 裡，寫出 `clusters/<叢集>/trustiot/<Secret 名稱>.sops.yaml`：

  ```sh
  <輸出值的指令> | bun scripts/flux/sops-secret.ts --cluster <saas|demo> --name <Secret 名稱> --key <key> [--format token|text] [--set <key>=<不是祕密的值>] [--watch]
  ```

  `--format token`（預設）收一個字，例如 API key；`--format text` 收一行文字（可以有空白與 `<>@`；結尾的一個換行會去掉，其他的換行、控制字元與空值都拒絕）。`--set` 加一個不是祕密的 key（例如帳號，同樣加密）。`--watch` 給 Secret 加上 `reconcile.fluxcd.io/watch: Enabled`，HelmRelease 從它讀 values 時，值一改就升級。錯誤訊息不引用輸入的值。展示叢集的私有 values 用自己的 `demo-values.ts`（下方「展示叢集」）。
- 寫檔之前先檢查 sops 的輸出確實加密了（與 CI 同一套檢查：每個值都是 SOPS 的密文、`sops` 區塊是 sops 為 age 寫的樣子、metadata 與 key 和明文的 Secret 相同），不是的話什麼都不寫。
- 只加密 `data` 與 `stringData` 的值；CI 的 `check-secret-handling.ts` 擋下任何沒加密的 Secret。它只看得出值有沒有加密，看不到值本身。
- 輪替與收件者異動的步驟在 [密鑰輪替 runbook](../../docs/runbooks/secrets.md)。

### GHCR 拉取 token

Flux 拉 chart（`chart.yaml` 的 `secretRef`）與 kubelet 拉映像（`values-instance.yaml` 的 `global.imagePullSecrets`）共用 namespace `trustiot` 的 Secret `ghcr-pull`（`kubernetes.io/dockerconfigjson`）。GHCR 只接受 classic personal access token：

1. 以專用的機器帳號（不是任何人的帳號）建立 classic token，只勾 `read:packages`，設定到期日（最長一年），在團隊行事曆記下到期前 14 天的提醒。機器帳號要能讀 `Laysi/trustiot-v2` 的套件，而且只給這些套件的讀取權限：classic token 的 `read:packages` 能讀帳號看得到的每一個套件，不限於這個 repo。
2. 產生加密的 Secret。token 只經 stdin 傳入，明文只在記憶體與 sops 的 stdin 裡：

   ```sh
   <輸出 token 的指令> | bun scripts/flux/ghcr-pull-secret.ts --username <機器帳號> [--cluster demo]
   ```

   寫出 `deploy/flux/clusters/<叢集>/trustiot/ghcr-pull.sops.yaml`（`--cluster` 預設 `saas`；以 `.sops.yaml` 中該叢集的收件者加密）。展示叢集用自己的 token，與 SaaS 的分開輪替。
3. 把 `ghcr-pull.sops.yaml` 加進同目錄 `kustomization.yaml` 的 `resources`，開 PR 合併。
4. 確認：`sudo k3s kubectl -n trustiot get ocirepository trustiot`（READY 為 True），以及 runbook 的 kubelet 拉取檢查。

## 上線前要填的值

`values-instance.yaml` 裡留空的欄位是建立 SaaS 叢集的人要做的決定；沒填之前 chart 的 schema 拒絕這份 values，HelmRelease 停在 not ready 並寫出原因，不會裝到一半。`bun test deploy/flux` 檢查留空的只有下列欄位：

| 欄位 | 內容 |
| --- | --- |
| `hosts.app`、`hosts.gateway`、`hosts.ota` | SaaS 的網域（Cloudflare Tunnel 的路由，`deploy/host/README.md`） |
| `postgres.bundled.storage.size` | 叢集內 Postgres（CloudNativePG，資料與 WAL 共用）的磁碟大小，依主機磁碟與業務資料決定（[chart 的「叢集內的 Postgres」](../chart/README.md#叢集內的-postgres)） |
| `clickhouse.bundled.storage.size` | 叢集內 ClickHouse 的磁碟大小，依主機磁碟與負載決定 |
| `qdrant.external.url` | Qdrant Cloud 的叢集網址（Laysi/trustiot-v2#194） |

values 引用的 Secret（`trustiot-qdrant`、`trustiot-sentry`，以及之後各服務的）都以 SOPS 檔案放在 `clusters/saas/trustiot/`，名稱與 key 見 [密鑰與權限](../../docs/architecture/secrets.md) 的清冊。服務角色與 collector 的密碼不在這裡：migration Job 在叢集內產生它們，寫進 chart 宣告的空 Secret（`trustiot-<服務>-password`、`trustiot-otel-collector-password`），輪替是調大 `values-instance.yaml` 的 `migrate.passwordGeneration`。叢集內 ClickHouse 的管理帳號密碼也不在這裡（Laysi/trustiot-v2#403）：ClickHouse 的 init container 產生它，寫進 `trustiot-clickhouse-admin-password`，輪替是調大 `values-instance.yaml` 的 `clickhouse.bundled.adminPasswordGeneration`（[docs/runbooks/secrets.md](../../docs/runbooks/secrets.md)）。內部身分簽章金鑰也不在這裡（Laysi/trustiot-v2#404）：身分服務啟用後，它的 init container 產生金鑰寫進 `trustiot-identity-signing-keys`，公鑰寫進 ConfigMap `trustiot-internal-identity-keys`，輪替與撤銷是調大 `values-instance.yaml` 的 `internalIdentity.keyGeneration` 與 `revokeBeforeGeneration`，values 不放任何金鑰或公鑰。這些 Secret 的 `data` 不屬於 Helm：HelmRelease 沒有開 drift detection，升級與 rollback remediation 都不動它；將來開 drift detection 時，先在實驗機確認 helm-controller 不把 Job 與 ClickHouse 的 init container 寫的 `data` 當成漂移（渲染結果沒有 `data`，按理不算，但還沒驗證），必要時以 `driftDetection.ignore` 排除這些 Secret 的 `/data`（以及內部身分公鑰 ConfigMap 的 `/data`，它同樣由 init container 寫入）。

## 建立叢集（一次性）

前提：主機已依 [deploy/host](../host/README.md) 安裝（k3s、cloudflared），Istio ambient 已依 [deploy/istio](../istio/README.md) 安裝，CloudNativePG operator 已依 [deploy/cloudnative-pg](../cloudnative-pg/README.md) 安裝（chart 的 Postgres 是 CloudNativePG 的 Cluster；沒有 operator 時 HelmRelease 因為不認得 `postgresql.cnpg.io/v1` 而停在 not ready）。這兩個是整個叢集共用的平台元件，由人在裝 Flux 之前各裝一次，Flux 不管理它們（決策 5 的 2026-10-12 補充）。以下 `kubectl` 指令都在主機上以 `sudo k3s kubectl` 執行（經 Cloudflare Access 的維運 SSH）；`kubectl kustomize` 在有 repo 的工作站以 `.prototools` 的 kubectl 執行。

1. **收件者**：每位維運人員以 `bun scripts/flux/age.ts age-keygen -o <私鑰檔>` 在自己的裝置產生金鑰對，只交出公鑰；建議至少兩位。
2. **叢集的 age 金鑰**：由維運人員在可信的機器上產生（同上），公鑰與維運人員的公鑰一起寫進 `.sops.yaml` 的 `age:`，開 PR 合併。私鑰直接建立成叢集的 Secret，不進 git，經 stdin 傳入：

   ```sh
   sudo k3s kubectl create namespace flux-system
   sudo k3s kubectl -n flux-system create secret generic sops-age --from-file=age.agekey=/dev/stdin < <私鑰檔>
   ```

   私鑰的離線備份與根 CA 私鑰一起交給兩位保管人（[密鑰與權限](../../docs/architecture/secrets.md)），然後銷毀工作用的檔案。
3. **deploy key**：產生一把只給這個叢集的 SSH 金鑰，公鑰在 GitHub 的 repo 設定加成 **read-only** deploy key（Settings → Deploy keys，不勾 Allow write access），私鑰只放進叢集：

   ```sh
   ssh-keygen -t ed25519 -N '' -C trustiot-saas-flux -f flux-deploy-key
   # 在 GitHub 加入 flux-deploy-key.pub（read-only）後，把三個檔案複製到主機：
   sudo k3s kubectl -n flux-system create secret generic flux-system \
     --from-file=identity=flux-deploy-key --from-file=identity.pub=flux-deploy-key.pub \
     --from-file=known_hosts=github-known-hosts
   shred -u flux-deploy-key
   ```

   `known_hosts` 用本目錄的 `github-known-hosts`（測試核對過 GitHub 公布的指紋）。
4. **GHCR 拉取 token** 與其他 SOPS Secret：見上方「GHCR 拉取 token」，以及清冊中標為 SOPS 的每一項；開 PR 合併。
5. **實例的值**：填好「上線前要填的值」，開 PR 合併。
6. **裝 Flux**：從工作站把 Flux 本身送上主機。第一次套用時 CRD 還沒生效，GitRepository 與 Kustomization 會被拒絕，等 CRD 生效後再套用一次：

   ```sh
   kubectl kustomize deploy/flux/clusters/saas/flux-system > flux-system.yaml
   ssh <主機> sudo k3s kubectl apply --server-side -f - < flux-system.yaml           # 第一次：CRD 與控制器
   ssh <主機> sudo k3s kubectl wait --for=condition=established crd --all --timeout=120s
   ssh <主機> sudo k3s kubectl apply --server-side -f - < flux-system.yaml           # 第二次：開始同步 main
   ```

   之後 Flux 從 main 管理自己與其他所有東西，不再需要手動套用。
7. **確認**：

   ```sh
   sudo k3s kubectl -n flux-system get gitrepositories,kustomizations   # READY True，REVISION 是 main 的 commit
   sudo k3s kubectl -n trustiot get ocirepositories,helmreleases        # READY True
   ```

## 日常操作

主機上沒有 flux CLI，以 kubectl 操作：

| 要做的事 | 指令 |
| --- | --- |
| 看狀態 | `sudo k3s kubectl get gitrepositories,kustomizations,ocirepositories,helmreleases -A` |
| 看失敗原因 | `sudo k3s kubectl -n trustiot describe helmrelease trustiot`、`... describe ocirepository trustiot` |
| 不等間隔，立刻同步 | `sudo k3s kubectl -n flux-system annotate --overwrite gitrepository/flux-system reconcile.fluxcd.io/requestedAt="$(date +%s)"`（OCIRepository、HelmRelease 同樣以這個 annotation 觸發） |
| 暫停部署（例如維護期間） | `sudo k3s kubectl -n flux-system patch kustomization flux-system --type=merge -p '{"spec":{"suspend":true}}'`；恢復時改回 `false`。暫停期間合併的 PR 在恢復後才套用 |

不要在叢集上直接改 Flux 管理的物件：下一次同步會改回 git 裡的狀態。要改就改 git。

叢集內 Postgres 的資料不隨 release 刪除：CloudNativePG 的 Cluster 與 ImageCatalog 帶 `helm.sh/resource-policy: keep`，HelmRelease 從 git 移除、或安裝失敗後 Flux 依 remediation 解除安裝時，資料庫與它的磁碟區都會留下。確定不要資料時才明確刪除 Cluster（chart README 的「叢集內的 Postgres」）。重新安裝同名的 release 時，Helm 依物件上的 release 標註接管留下來的 Cluster；這一點還沒在叢集上驗證過。

## 升級 Flux

Renovate 會開 PR 更新 `.prototools` 的 `flux`。那個 PR 的 `bun test deploy/flux` 會失敗（`gotk-components.yaml` 與新版 CLI 的輸出不同），在同一個 PR 執行：

```sh
proto install flux
bun scripts/flux/components.ts    # 重寫 gotk-components.yaml 與控制器映像的 digest（先驗簽）
```

合併後 Flux 自己升級自己。看 Flux 的 release notes 是否有 API 版本的變更。

## 展示叢集

展示切片的展示環境（Laysi/trustiot-v2#457，[里程碑](../../docs/roadmap/milestones.md)「展示切片」）在展示機上：lab 的 Mac mini M1，與 lab e2e 共用同一個 k3s。它以 `clusters/demo` 部署，release、namespace 與 istiod 的 provider 都和 SaaS 相同（`trustiot`、`trustiot`、`trustiot-identity`），所以 [deploy/istio](../istio/README.md) 的設定原樣適用。

| 項目 | 展示叢集 | 理由與取捨 |
| --- | --- | --- |
| Flux | `clusters/demo/flux-system` 以 kustomize 引用 `clusters/saas/flux-system`，只以 patch 把根 Kustomization 的 `path` 換成 `./deploy/flux/clusters/demo`（patch 用 `replace`，SaaS 那邊的路徑不見時建置失敗，不會讓展示叢集套用 SaaS 的樹） | 升級 Flux 只改一處，兩個叢集一起跟上；代價是兩個叢集不能停在不同版的 Flux |
| chart 版本 | 自己的 `chart.yaml` 與 `values-saas.yaml`；deploy PR 把它與 SaaS 一起移到新版，只動它時用 `pin-chart.ts` | 換版與 SaaS 一樣是「合併即部署、revert 即回滾」，簽章驗證也相同；代價是一個 deploy PR 可能同時動兩個叢集 |
| values | HelmRelease 依序讀 `values-saas.yaml`、`values-demo.yaml`，最後是 SOPS 加密的 Secret `trustiot-demo-values` 的四個 key，各自以 `targetPath` 與 `literal: true` 寫進一個值：`app-host` → `hosts.app`、`gateway-host` → `hosts.gateway`、`ota-host` → `hosts.ota`、`smtp-from` → `smtp.from` | 主機名稱與寄件人不公開（[密鑰與權限](../../docs/architecture/secrets.md)「公開 repo 的資訊規則」），只在加密的 Secret 裡。加密的內容在 PR 裡看不到，所以它只能設這四個值：整份 values 的寫法能改任何設定（映像、ingress、環境變數），沒有 `literal` 時 Flux 以 Helm 的 `--set` 語法讀值，逗號就能接上另一個 `路徑=值`。Secret 帶 `reconcile.fluxcd.io/watch: Enabled`，改了就升級 |
| 服務 | `values-demo.yaml` 的 `services` 逐一打開切片的服務（目前 `identity`、`assets`） | 只開部署中那一版 chart 有映像的服務；切片的 issue 進了 release 後，在這裡打開 |
| 外部端點 | Qdrant 的網址在 `.invalid`（RFC 6761，不會解析）；Sentry 關閉 | 切片不用知識庫，SaaS 型態的 schema 又要求外部網址；沒有展示用的 Sentry 專案，錯誤留在 ClickHouse 的日誌 |
| 備份與磁碟 | 沒有備份；Postgres 10Gi、ClickHouse 20Gi | 里程碑「展示切片」接受展示資料遺失；一台閘道器加幾具電表的量 |

SOPS 加密的 Secret（建立後列進 `clusters/demo/trustiot/kustomization.yaml`）：

| 檔案 | Secret 與 key | 內容與產生方式 |
| --- | --- | --- |
| `ghcr-pull.sops.yaml` | `ghcr-pull`，`.dockerconfigjson` | 展示環境自己的 GHCR 拉取 token：`<輸出 token 的指令> \| bun scripts/flux/ghcr-pull-secret.ts --cluster demo --username <機器帳號>` |
| `trustiot-smtp.sops.yaml` | `trustiot-smtp`，`username`、`password` | Resend 的 API key，權限只能寄信並限定寄件網域（[密鑰與權限](../../docs/architecture/secrets.md)「外部服務」）：`<輸出 API key 的指令> \| bun scripts/flux/sops-secret.ts --cluster demo --name trustiot-smtp --key password --set username=resend` |
| `trustiot-anthropic.sops.yaml` | `trustiot-anthropic`，`api-key` | 展示專用的 Anthropic 工作區（設每月用量上限）的 API key；AI 閘道（Laysi/trustiot-v2#139）加入前沒有服務使用：`... \| bun scripts/flux/sops-secret.ts --cluster demo --name trustiot-anthropic --key api-key` |
| `trustiot-demo-values.sops.yaml` | `trustiot-demo-values`，`app-host`、`gateway-host`、`ota-host`、`smtp-from` | 三個主機名稱與寄件人（例如 `TrustIoT Demo <no-reply@寄件網域>`）；不是密鑰，只是不公開。stdin 每行一個 `<key>=<值>`：`<輸出四行的指令> \| bun scripts/flux/demo-values.ts`。剛好這四個 key，每個值一行，主機名稱要是小寫的 DNS 名稱；錯誤訊息不引用值 |

加密的值 CI 看不到：`check-secret-handling.ts` 只確認它們加密了，`bun test deploy/flux` 只看得到 key 的名稱。所以改到 `trustiot-demo-values.sops.yaml`（或其他 demo 的 SOPS 檔）的 PR，由持有維運人員私鑰的人審查：以 `SOPS_AGE_KEY_FILE=<私鑰檔> sops -d <檔案>` 解開，確認內容與 PR 說的相同；PR 內文寫明改了哪些 key（只寫 key，不寫值）。

### 展示叢集的 age 金鑰

`.sops.yaml` 的 demo 規則列出展示叢集的 age 公鑰（註解 `# cluster: demo`）與維運人員的公鑰（註解 `# operator`；維運人員的金鑰同上方「建立叢集」第 1 步）。展示叢集的私鑰在展示機上產生，經管線直接寫進 `flux-system` 的 Secret `sops-age`：不落地、不離開展示機，也不做離線備份。遺失時照同樣的步驟產生新的，把 demo 規則的舊公鑰換成新的，再由一位維運人員以自己的私鑰對每個 demo 的 SOPS 檔 `sops updatekeys -y`；換好之前 Flux 解不開 Secret、不套用新的變更，正在跑的服務不受影響。展示資料可以接受遺失，備份的保管成本不划算，所以和 SaaS（離線備份與根 CA 私鑰放在一起）不同。

由有展示機 SSH 權限的維運人員執行，期間持有 lab 鎖（下方「lab 測試」）。age 在工作站以 `.prototools` 的 Go 從原始碼建置成 linux/arm64，同樣經 Go checksum database 驗證；展示機上只放在 ssh 使用者的 runtime 目錄（tmpfs），用完刪除。下面的 `<issue>` 換成這次工作的 issue 編號，`<雜湊>` 換成工作站上 `sha256sum` 印出的值。每段遠端指令都以 `set -eu` 開頭，先確認 runtime 目錄存在、lab 鎖是這個 issue 的，任何一步失敗就停下，不往下做：

```sh
# 工作站：建置，印出 age 與 age-keygen 的路徑；算出 age-keygen 的雜湊
bun scripts/flux/age.ts --target linux/arm64
sha256sum <age-keygen 的路徑>

# 取得 lab 鎖；已經有人持有時 mkdir 失敗，印出持有者後停下
ssh <展示機> 'mkdir ~/e2e.lock && echo "Laysi/trustiot-v2#<issue> $(date -u +%FT%TZ)" > ~/e2e.lock/owner && echo ACQUIRED || cat ~/e2e.lock/owner'

# 把 age-keygen 送到展示機
ssh <展示機> 'set -eu
  [ -d "${XDG_RUNTIME_DIR:?}" ]
  grep -q "^Laysi/trustiot-v2#<issue> " ~/e2e.lock/owner || { echo "the lab lock is not held for this issue" >&2; exit 1; }
  umask 077
  cat > "$XDG_RUNTIME_DIR/age-keygen"
  chmod 0700 "$XDG_RUNTIME_DIR/age-keygen"' < <age-keygen 的路徑>

# 展示機：比對雜湊，產生私鑰並直接寫進 Secret，從 Secret 再算一次公鑰。namespace 已經存在時
# create 失敗而停下，不碰別人的 flux-system；建立 namespace 之後任何一步失敗，就刪掉這一步建立的
# namespace（連同 sops-age），不留下空的或壞掉的 Secret。最後印出的 Public key 寫進 .sops.yaml。
ssh <展示機> 'set -eu
  set -o pipefail
  [ -d "${XDG_RUNTIME_DIR:?}" ]
  grep -q "^Laysi/trustiot-v2#<issue> " ~/e2e.lock/owner || { echo "the lab lock is not held for this issue" >&2; exit 1; }
  K="$XDG_RUNTIME_DIR/age-keygen"
  P="$XDG_RUNTIME_DIR/age.pub"
  ours=no
  ok=no
  cleanup() {
    rm -f "$K" "$P"
    if [ "$ours" = yes ] && [ "$ok" != yes ]; then sudo k3s kubectl delete namespace flux-system --wait; fi
  }
  trap cleanup EXIT
  echo "<雜湊>  $K" | sha256sum -c --quiet -
  sudo k3s kubectl create namespace flux-system
  ours=yes
  "$K" 2>"$P" | sudo k3s kubectl -n flux-system create secret generic sops-age --from-file=age.agekey=/dev/stdin
  printed=$(sed -n "s/^Public key: //p" "$P")
  derived=$(sudo k3s kubectl -n flux-system get secret sops-age -o "jsonpath={.data.age\.agekey}" | base64 -d | "$K" -y)
  [ -n "$derived" ]
  [ "$derived" = "$printed" ]
  ok=yes
  echo "Public key: $derived"'

# 釋放 lab 鎖，只在它還是這個 issue 的時候
ssh <展示機> 'grep -q "^Laysi/trustiot-v2#<issue> " ~/e2e.lock/owner && rm -rf ~/e2e.lock'
```

age-keygen 的輸出不是終端機時，私鑰寫到管線、`Public key: …` 寫到 stderr，所以 `$P` 裡只有公鑰。遠端指令由展示機的登入 shell 執行，需要支援 `pipefail`（bash，或新版的 dash）；整段包在單引號裡，從 bash 或 fish 貼上都相同。

### 日常變更

- **私有 values**（主機名稱、寄件人）：收件者以自己的私鑰 `SOPS_AGE_KEY_FILE=<私鑰檔> sops edit deploy/flux/clusters/demo/trustiot/trustiot-demo-values.sops.yaml` 改其中一個 key 的值（不加 key；HelmRelease 只讀那四個），或以上表的 `demo-values.ts` 整份重新產生；開 PR，內文寫明改了哪些 key，由另一位維運人員解開審查後合併。Secret 帶 watch label，Flux 套用後 helm-controller 立刻升級。改主機名稱時，Cloudflare 的路由、DNS 與 Access 要一起改。
- **打開切片的服務**：服務的 issue 進了某一版 chart、展示叢集也部署了那一版之後，在 `values-demo.yaml` 的 `services` 加上 `<服務>: {enabled: true}`；它用到的 Secret 以 SOPS 檔一起加進來並列進 `kustomization.yaml`；開 PR 合併。`bun test deploy/flux` 以 HEAD 的 chart 渲染展示叢集的 values，打開的服務若少了 chart 要求的設定，測試會失敗。打開前確認展示機的記憶體還放得下。
- **換版**：同 SaaS（上方「發版與回滾」），或以 `pin-chart.ts` 只換展示叢集。

在展示機裝 Flux 之前：

- `chart.yaml` 至少要是 `0.0.0-rc.8`。更早的 chart 沒有 Laysi/trustiot-v2#404（內部身分簽章金鑰在叢集內產生；舊版要人建立金鑰）與 Laysi/trustiot-v2#400（建立 Postgres 時收回 `postgres` 與 `template1` 的 PUBLIC 權限，只在 Cluster 建立時發生）。目前釘的 `0.0.0-rc.7` 只是佔位：展示機還沒有 Flux，它不部署任何東西。
- `ghcr-pull`、`trustiot-smtp`、`trustiot-demo-values` 三個 SOPS 檔要在 main 上：HelmRelease 的私有 values 不是 optional，少了主機名稱時 chart 的 schema 拒絕整份 values，什麼都不裝。
- 其餘同上方「建立叢集」，差別是叢集的 age 金鑰（上方「展示叢集的 age 金鑰」）與 deploy key 都在展示機上產生（deploy key 加到 GitHub 時同樣不勾 Allow write access）。在展示機安裝 Flux 的步驟由 Laysi/trustiot-v2#467 補上。

lab e2e：`tests/e2e/lab.e2e.ts` 在展示環境的 Flux 存在時拒絕執行，也只移除自己建立的 Flux（下方「lab 測試」）。要跑它，先移除展示環境的 release 與 Flux（`flux-system` 連同兩把金鑰；資料可以保留），測完重新安裝，金鑰重新產生並更新 `.sops.yaml` 與 deploy key。期間展示環境中斷，閘道器的資料會留下缺口，所以只在必要時（例如驗證 Flux 的升級）這樣做。

## 檢查

`bun test deploy/flux`（CI 的 `helm` job），每個叢集都檢查：

- `kubectl kustomize` 建得出整棵樹，內容只有 Flux、同步設定與 TrustIoT release；展示叢集的 Flux 與 SaaS 的逐一相同，只有同步路徑不同。
- 每個物件通過 kubeconform `-strict`；Flux 的物件以 `gotk-components.yaml` 裡同版本的 CRD 驗證，不需要網路。
- `gotk-components.yaml` 與釘選版本的 `flux install --export` 相同，控制器映像都以 digest 釘選。
- `chart.yaml` 是 `renderChartSource` 的輸出：digest、拉取 token、只接受這個 tag 的 release.yml 簽章；沒有其他可以略過驗簽的來源（HelmRepository、Bucket）。
- `values-saas.yaml` 來自部署中的版本；HelmRelease 依序讀兩份 values；目錄裡的每個 SOPS 檔都有套用。
- `values-instance.yaml` 留空的只有上表的欄位，填上測試值後 HEAD 的 chart 接受它，服務的 pod 以 `ghcr-pull` 拉映像。
- 展示叢集：HelmRelease 與 SaaS 的相同，只多了私有 values：每個讀 Secret 的項目都有 `targetPath`（四個值之一）、對應的 key 與 `literal: true`，沒有整份 values 的項目；產生器與 commit 的 Secret 剛好有那四個 key。以測試用的私有 values（`.test` 網域，寄件人含空白、`<…@…>` 與逗號）渲染時，值原樣出現在 ConfigMap，整份輸出與用 values 檔給同樣的值相同；同樣的值改用 `--set` 語法時，逗號後面會改到映像的 registry（證明 `literal` 擋下了什麼）。少了私有 values 時 chart 拒絕。
- `values-demo.yaml` 不含主機名稱、寄件人與 IP：每個像主機名稱的字（值、URL 與註解都算）都在 `.invalid` 或 `.test` 之下、是列出的公開服務（`smtp.resend.com`）、檔名，或 chart values 的路徑；沒有 `@`。
- `.sops.yaml` 每個叢集一條規則，規則不會比對到別的叢集的檔案；每個收件者一行並註明角色；每條規則是空的，或剛好是自己叢集的金鑰加上每一把維運人員的金鑰；`github-known-hosts` 與 GitHub 公布的指紋相同。

release 流程的部分在 `bun test scripts/release`（`flux.test.ts`、`deploy.test.ts`：PR 的內容、每個叢集的版本比較、只移動較舊的叢集、重跑（分支已經存在時，它改的檔案或內容與這次該寫的不同就停下）、缺少或被手改的 `chart.yaml`、GitHub 拒絕開 PR 時的訊息）；`scripts/flux/lib.test.ts` 檢查 Secret 的產生、輸入值的檢查、私有 values 的 key、收件者的角色與 `pin-chart.ts`；`scripts/flux/sops.test.ts` 檢查寫檔前對 sops 輸出的檢查與 stdin 必須是管線。

## lab 測試

`tests/e2e/lab.e2e.ts` 在實驗機（k3s，唯一的 Kubernetes 環境）以 README 的方式裝 Flux，驗證整條路。它在整個叢集裝上再移除 Flux，所以叢集已經有 Flux（namespace `flux-system` 或 Flux 的 CRD）時拒絕執行；實驗機也是展示機，展示環境的 Flux 在時要先移除它才能跑（見上方「展示叢集」）。它開始時與裝 Flux 之前各檢查一次：lab 鎖 `~/e2e.lock/owner` 是一行 `Laysi/trustiot-v2#<issue> <UTC 時間>`，issue 與 `E2E_NAMESPACE`（`e2e-<issue>`）相同（連同後面的空白比對，`#4570` 不會被當成 `#457`），叢集上沒有 Flux。`flux-system` 以這次執行的 ID 為 label 建立；建立時已經存在就停下，什麼都不裝；結束時重新讀 label，仍是這次的 ID 才刪 Flux 的物件、manifest（CRD、ClusterRole、ClusterRoleBinding）與 namespace，否則一律不刪並讓測試失敗。驗證內容：合併 deploy commit 後在間隔內部署、revert 後回到前一版、別的金鑰簽的 chart 不會被安裝、SOPS 加密的拉取 Secret 被解密並用來拉 chart。測完移除 Flux 與測試 namespace，並比對叢集層級的物件與其他 namespace 的 workload 與測試前相同。

```sh
ssh trustiot-mac-m1 'mkdir ~/e2e.lock && echo "Laysi/trustiot-v2#70 $(date -u +%FT%TZ)" > ~/e2e.lock/owner'
E2E_SSH_HOST=trustiot-mac-m1 E2E_NAMESPACE=e2e-70 E2E_OUT=<輸出目錄> bun deploy/flux/tests/e2e/lab.e2e.ts
ssh trustiot-mac-m1 'grep -q "^Laysi/trustiot-v2#70 " ~/e2e.lock/owner && rm -rf ~/e2e.lock'
```

與正式環境不同的地方（都只在測試裡）：git 伺服器是測試 namespace 裡的 soft-serve（同樣以 SSH 與唯讀金鑰讀取），registry 是有 TLS 與帳密的一次性 registry，chart 是小的測試 chart；簽章用一次性的 cosign 金鑰對，`chart.yaml` 以 `secretRef`（測試公鑰）取代 `matchOIDCIdentity`，因為 keyless 簽章只能由 release.yml 在 `v*` tag 上產生。其餘欄位與 `renderChartSource` 的輸出相同。所有金鑰、密碼與憑證都在執行時產生，結束後刪除。結果見 [測試報告](../../docs/test-reports/2026-10-11-70-flux-gitops.md)。
