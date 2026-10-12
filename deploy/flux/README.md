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

## 檔案

| 路徑 | 內容 | 誰改 |
| --- | --- | --- |
| `clusters/saas/kustomization.yaml` | 叢集套用的全部內容：Flux 本身與 TrustIoT | 人 |
| `clusters/saas/flux-system/gotk-components.yaml` | Flux 的 CRD 與控制器（`flux install --export`，只有 source、kustomize、helm 三個控制器） | `scripts/flux/components.ts` |
| `clusters/saas/flux-system/kustomization.yaml` | 控制器映像以 digest 釘選；digest 取自 cosign 驗證過的 fluxcd 官方 keyless 簽章 | `scripts/flux/components.ts` |
| `clusters/saas/flux-system/gotk-sync.yaml` | GitRepository（main，只取 `deploy/flux/`）與根 Kustomization（`prune`、SOPS 解密） | 人 |
| `clusters/saas/trustiot/chart.yaml` | chart 的 OCIRepository：版本、digest、拉取 token、簽章驗證 | release.yml 的 deploy PR |
| `clusters/saas/trustiot/values-saas.yaml` | 部署中那一版的 `deploy/chart/values-saas.yaml` | release.yml 的 deploy PR |
| `clusters/saas/trustiot/values-instance.yaml` | SaaS 安裝實例的覆寫值：主機名稱、外部端點、Secret 名稱 | 人 |
| `clusters/saas/trustiot/helm-release.yaml`、`namespace.yaml` | HelmRelease 與 namespace（Pod Security `restricted`） | 人 |
| `clusters/saas/trustiot/*.sops.yaml` | 以 SOPS 加密的 Secret，例如 `ghcr-pull.sops.yaml`；建立後要列進同目錄的 `kustomization.yaml` | 人（`sops edit`） |
| `github-known-hosts` | GitHub 公布的 SSH 主機金鑰，給 GitRepository 的 Secret 用 | 人（GitHub 換金鑰時） |
| `tests/flux.test.ts` | `bun test deploy/flux`，見下方「檢查」 | |
| `tests/e2e/lab.e2e.ts` | 在實驗機上驗證 Flux 的整條路，見下方「lab 測試」 | |

## 發版與回滾

1. 推 `v*` tag，release.yml 發佈並驗證映像與 chart（[CI 與發佈](../../docs/architecture/ci-release.md) 的「release.yml」）。
2. verify 通過後，`deploy-pr` job（`scripts/release/deploy-pr.ts`）以 GitHub API 在 main 上做一個 commit，推到分支 `deploy/trustiot-<版本>`，改兩個檔：`chart.yaml` 換成新版本與 publish 推送的 chart digest，`values-saas.yaml` 換成該 tag 的 `deploy/chart/values-saas.yaml`；接著開 PR。候選版（`-rc.N`）開成 draft，內文提醒它不一定在 main 上。版本不比 main 上部署中的新時（例如舊版線的修補版）不開 PR。重跑時已有的分支與 PR（包含已關閉的）不重開。
3. 審查後合併。Flux 在 1 分鐘內（GitRepository 的 `interval`）取得新的 commit，套用後 helm-controller 立刻升級；整個 Helm 升級的上限是 HelmRelease 的 `timeout`（10 分鐘），要等所有 workload 與 Job 完成。升級失敗時自動 rollback 到上一個成功的版本再重試，最多 3 次。
4. 回到前一版：revert deploy PR 並合併，Flux 以同樣的方式把舊版本裝回去。

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

- 在目前的 GitHub Free 方案，分支保護與 ruleset 都不能用，核准不是合併的條件，所以 workflow 能核准 PR 並不會讓它繞過任何強制關卡；合併前的確認由流程把關。
- 能取得 `pull-requests: write` 的只有 `deploy-pr` 這個 job，它不執行任何建置程式碼；fork PR 的 token 一律唯讀。
- 升級到可用分支保護的方案時，要重新檢視：屆時以 required reviewers 加上「workflow 不能核准」的設定，或改由 GitHub App 開 PR。

## 簽章驗證

`chart.yaml` 由 `scripts/release/flux.ts` 的 `renderChartSource` 產生：

- 以 tag 與 digest 拉 chart，digest 優先：GHCR 的 tag 可以被有寫入權限的人移動，digest 不行。
- 只接受 release.yml 在**這個 tag** 上的 keyless 簽章（issuer `https://token.actions.githubusercontent.com`，subject `https://github.com/Laysi/trustiot-v2/.github/workflows/release.yml@refs/tags/v<版本>`，皆為完全比對；決策 30）。別的 tag、別的 workflow 或別的 repo 簽的都不接受。驗證失敗時 OCIRepository 的 Ready 為 False，HelmRelease 不升級，正在跑的版本不受影響。
- 簽章證明的是「release.yml 在這個 tag 上建置並推送」，不是「來自 main」。正式版的 tag 必須在 main 上（release.yml 的 plan 檢查），所以執行的 release.yml 就是 main 上審查過的版本；候選版的 tag 可以在任何分支上，跑的是該分支的 release.yml，由推 tag 的人決定。所以候選版的 deploy PR 是 draft，合併前的審查才是把關。要收緊時，以 tag ruleset 限制誰能建立 `v*` tag（repo 設定，由人決定）。
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

- 收件者在 repo 根目錄的 `.sops.yaml`：叢集的 age 公鑰，加上每位維運人員的公鑰。收件者還沒填時 sops 拒絕加密（`empty key group provided`），不會產生沒有人解得開的檔案。
- 編輯：`SOPS_AGE_KEY_FILE=<自己的私鑰檔> sops edit deploy/flux/clusters/saas/trustiot/<名稱>.sops.yaml`。新增的 Secret 檔要列進 `clusters/saas/trustiot/kustomization.yaml` 的 `resources`（測試檢查兩邊一致）。
- 只加密 `data` 與 `stringData` 的值；CI 的 `check-secret-handling.ts` 擋下任何沒加密的 Secret。
- 輪替與收件者異動的步驟在 [密鑰輪替 runbook](../../docs/runbooks/secrets.md)。

### GHCR 拉取 token

Flux 拉 chart（`chart.yaml` 的 `secretRef`）與 kubelet 拉映像（`values-instance.yaml` 的 `global.imagePullSecrets`）共用 namespace `trustiot` 的 Secret `ghcr-pull`（`kubernetes.io/dockerconfigjson`）。GHCR 只接受 classic personal access token：

1. 以專用的機器帳號（不是任何人的帳號）建立 classic token，只勾 `read:packages`，設定到期日（最長一年），在團隊行事曆記下到期前 14 天的提醒。機器帳號要能讀 `Laysi/trustiot-v2` 的套件，而且只給這些套件的讀取權限：classic token 的 `read:packages` 能讀帳號看得到的每一個套件，不限於這個 repo。
2. 產生加密的 Secret。token 只經 stdin 傳入，明文只在記憶體與 sops 的 stdin 裡：

   ```sh
   <輸出 token 的指令> | bun scripts/flux/ghcr-pull-secret.ts --username <機器帳號>
   ```

   寫出 `deploy/flux/clusters/saas/trustiot/ghcr-pull.sops.yaml`（以 `.sops.yaml` 的收件者加密）。
3. 把 `ghcr-pull.sops.yaml` 加進 `clusters/saas/trustiot/kustomization.yaml` 的 `resources`，開 PR 合併。
4. 確認：`sudo k3s kubectl -n trustiot get ocirepository trustiot`（READY 為 True），以及 runbook 的 kubelet 拉取檢查。

## 上線前要填的值

`values-instance.yaml` 裡留空的欄位是建立 SaaS 叢集的人要做的決定；沒填之前 chart 的 schema 拒絕這份 values，HelmRelease 停在 not ready 並寫出原因，不會裝到一半。`bun test deploy/flux` 檢查留空的只有下列欄位：

| 欄位 | 內容 |
| --- | --- |
| `hosts.app`、`hosts.gateway`、`hosts.ota` | SaaS 的網域（Cloudflare Tunnel 的路由，`deploy/host/README.md`） |
| `clickhouse.bundled.storage.size` | 叢集內 ClickHouse 的磁碟大小，依主機磁碟與負載決定 |
| `qdrant.external.url` | Qdrant Cloud 的叢集網址（Laysi/trustiot-v2#194） |

values 引用的 Secret（`trustiot-clickhouse-admin`、`trustiot-qdrant`、`trustiot-sentry`，以及之後各服務的）都以 SOPS 檔案放在 `clusters/saas/trustiot/`，名稱與 key 見 [密鑰與權限](../../docs/architecture/secrets.md) 的清冊。

## 建立叢集（一次性）

前提：主機已依 [deploy/host](../host/README.md) 安裝（k3s、cloudflared），Istio ambient 已依 [deploy/istio](../istio/README.md) 安裝。以下 `kubectl` 指令都在主機上以 `sudo k3s kubectl` 執行（經 Cloudflare Access 的維運 SSH）；`kubectl kustomize` 在有 repo 的工作站以 `.prototools` 的 kubectl 執行。

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

## 升級 Flux

Renovate 會開 PR 更新 `.prototools` 的 `flux`。那個 PR 的 `bun test deploy/flux` 會失敗（`gotk-components.yaml` 與新版 CLI 的輸出不同），在同一個 PR 執行：

```sh
proto install flux
bun scripts/flux/components.ts    # 重寫 gotk-components.yaml 與控制器映像的 digest（先驗簽）
```

合併後 Flux 自己升級自己。看 Flux 的 release notes 是否有 API 版本的變更。

## 檢查

`bun test deploy/flux`（CI 的 `helm` job）：

- `kubectl kustomize` 建得出整棵樹，內容只有 Flux、同步設定與 TrustIoT release。
- 每個物件通過 kubeconform `-strict`；Flux 的物件以 `gotk-components.yaml` 裡同版本的 CRD 驗證，不需要網路。
- `gotk-components.yaml` 與釘選版本的 `flux install --export` 相同，控制器映像都以 digest 釘選。
- `chart.yaml` 是 `renderChartSource` 的輸出：digest、拉取 token、只接受這個 tag 的 release.yml 簽章；沒有其他可以略過驗簽的來源（HelmRepository、Bucket）。
- `values-saas.yaml` 來自部署中的版本；HelmRelease 依序讀兩份 values；目錄裡的每個 SOPS 檔都有套用。
- `values-instance.yaml` 留空的只有上表的欄位，填上測試值後 HEAD 的 chart 接受它，服務的 pod 以 `ghcr-pull` 拉映像。
- `.sops.yaml` 的規則與收件者格式；`github-known-hosts` 與 GitHub 公布的指紋相同。

release 流程的部分在 `bun test scripts/release`（`flux.test.ts`、`deploy.test.ts`：PR 的內容、版本比較、重跑、被手改的 `chart.yaml`、GitHub 拒絕開 PR 時的訊息）。

## lab 測試

`tests/e2e/lab.e2e.ts` 在實驗機（k3s，唯一的 Kubernetes 環境）以 README 的方式裝 Flux，驗證整條路：合併 deploy commit 後在間隔內部署、revert 後回到前一版、別的金鑰簽的 chart 不會被安裝、SOPS 加密的拉取 Secret 被解密並用來拉 chart。測完移除 Flux 與測試 namespace，並比對叢集層級的物件與其他 namespace 的 workload 與測試前相同。

```sh
ssh trustiot-mac-m1 'mkdir ~/e2e.lock && echo "<issue> $(date -u +%FT%TZ)" > ~/e2e.lock/owner'
E2E_SSH_HOST=trustiot-mac-m1 E2E_NAMESPACE=e2e-70 E2E_OUT=<輸出目錄> bun deploy/flux/tests/e2e/lab.e2e.ts
ssh trustiot-mac-m1 'rm -rf ~/e2e.lock'
```

與正式環境不同的地方（都只在測試裡）：git 伺服器是測試 namespace 裡的 soft-serve（同樣以 SSH 與唯讀金鑰讀取），registry 是有 TLS 與帳密的一次性 registry，chart 是小的測試 chart；簽章用一次性的 cosign 金鑰對，`chart.yaml` 以 `secretRef`（測試公鑰）取代 `matchOIDCIdentity`，因為 keyless 簽章只能由 release.yml 在 `v*` tag 上產生。其餘欄位與 `renderChartSource` 的輸出相同。所有金鑰、密碼與憑證都在執行時產生，結束後刪除。結果見 [測試報告](../../docs/test-reports/2026-10-11-70-flux-gitops.md)。
