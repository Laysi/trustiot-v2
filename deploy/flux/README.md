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
- 每個 SOPS 檔的 `sops` 區塊自己決定加密範圍，所以 CI 也檢查：`encrypted_regex` 剛好是 `^(data|stringData)$`、沒有其他改變範圍的選項、`data` 與 `stringData` 以外沒有加密的值；`metadata` 只有 `name`、`namespace` 與 `reconcile.fluxcd.io/watch: Enabled`，沒有其他 label、annotation、ownerReferences 或 finalizers。審查的人因此看得到值以外的一切，Secret 也不能帶 Helm 的歸屬標記被 release 收編（[密鑰與權限](../../docs/architecture/secrets.md) 的規則表）。
- 每個叢集能有哪些 SOPS Secret 列在 `scripts/flux/lib.ts` 的 `SOPS_SECRETS`（名稱與 type）：SaaS 是 `ghcr-pull`（`kubernetes.io/dockerconfigjson`）、`trustiot-qdrant`、`trustiot-sentry`；展示叢集是 `ghcr-pull`、`trustiot-smtp`、`trustiot-anthropic`、`trustiot-demo-values`（其餘都是 `Opaque`）。chart 在叢集內產生的 Secret（例如 `trustiot-identity-signing-keys` 與各服務的密碼）不在清單上，不能從 git 來：Helm 會收編同名的 Secret 並保留它的 `data`。產生 Secret 的腳本拒絕清單外的名稱、type 與路徑，`bun test deploy/flux` 檢查目錄裡的每個 SOPS 檔；新增 Secret 時在同一個 PR 更新清單、本 README 與清冊。
- 輪替與收件者異動的步驟在 [密鑰輪替 runbook](../../docs/runbooks/secrets.md)。

### GHCR 拉取 token

Flux 拉 chart（`chart.yaml` 的 `secretRef`）與 kubelet 拉映像（`values-instance.yaml` 的 `global.imagePullSecrets`）共用 namespace `trustiot` 的 Secret `ghcr-pull`（`kubernetes.io/dockerconfigjson`）。

GHCR 在 Actions 之外只接受 classic personal access token（[Working with the Container registry](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry#authenticating-to-the-container-registry)：「GitHub Packages only supports authentication using a personal access token (classic)」），GitHub App 的 installation token 不在支援之列。GitHub 組織以人頭計費，所以 2026-10-21 決定不另建機器帳號：這把 token 只給展示叢集用，由一位現有的組織成員建立。正式的 SaaS 叢集改用有唯讀機器帳號的 registry（Laysi/trustiot-v2#502），在那之前 SaaS 叢集不放個人的 token。

1. 由一位組織成員建立 classic token，只勾 `read:packages`，到期日一年。不可使用帶 `repo` 或任何寫入 scope 的 token：它會跟著 SOPS 檔案進叢集，叢集或維運人員的私鑰外洩時，等於這位成員看得到的 repo 都能被讀寫。在團隊行事曆記下到期前 14 天的提醒。`read:packages` 讀得到這位成員看得到的每一個 private 套件，不限於這個 repo，所以挑看得到的 repo 最少的成員；token 外洩時，這些套件都視為外洩（[密鑰輪替 runbook](../../docs/runbooks/secrets.md)「外洩處理」）。
2. 確認 token 只有 `read:packages` 一個 scope。token 只經 stdin 傳入，不放在指令列；GitHub API 回應標頭的 `X-OAuth-Scopes` 列出 token 的 scope：

   ```sh
   <輸出 token 的指令> | sed 's/^/header = "Authorization: token /; s/$/"/' | curl -sI -K - https://api.github.com/ | grep -i '^x-oauth-scopes:'
   ```

   輸出應該只有 `read:packages`。多出任何 scope（例如 `repo`）就到 GitHub 的 token 設定取消勾選，再檢查一次；沒有任何輸出代表 token 沒有送出或無效（例如剪貼簿已被別的內容覆蓋）。
3. 產生加密的 Secret。token 只經 stdin 傳入，明文只在記憶體與 sops 的 stdin 裡：

   ```sh
   <輸出 token 的指令> | bun scripts/flux/ghcr-pull-secret.ts --username <GitHub 帳號> --cluster demo
   ```

   寫出 `deploy/flux/clusters/demo/trustiot/ghcr-pull.sops.yaml`（以 `.sops.yaml` 中展示叢集的收件者加密）。`--cluster` 預設是 `saas`，但 SaaS 叢集的拉取憑證等 Laysi/trustiot-v2#502。
4. 把 `ghcr-pull.sops.yaml` 加進同目錄 `kustomization.yaml` 的 `resources`，開 PR 合併。
5. 確認：`sudo k3s kubectl -n trustiot get ocirepository trustiot`（READY 為 True），以及 runbook 的 kubelet 拉取檢查。

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
4. **拉取憑證** 與其他 SOPS Secret：SaaS 叢集拉 chart 與映像的唯讀帳號等 Laysi/trustiot-v2#502，不用個人的 GHCR token（見上方「GHCR 拉取 token」）；清冊中標為 SOPS 的每一項；開 PR 合併。
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
| 入口的 ClusterIP | 固定：`values-demo.yaml` 以 `ingress.tunnel.clusterIP` 設一個展示機 Service 網段（`10.43.0.0/16`）靜態區段裡沒被使用的位址，tunnel 的路由指向這個位址（Laysi/trustiot-v2#478）。設為 `10.43.0.80`（Laysi/trustiot-v2#480），與換到含這個欄位的 chart 版本在同一個 PR，而且在展示環境第一次部署（Laysi/trustiot-v2#467）之前；較舊的 chart 不認得這個欄位，schema 拒絕整份 values，HelmRelease 就失敗 | 入口 Service 重建後拿回同一個位址，路由不失效，舊位址也不會被別的 Service 拿走而經 tunnel 公開；ClusterIP 建立後不能改，所以要在 Service 第一次建立前設好。選法與改位址的步驟見 [chart 的 README](../chart/README.md)「固定的入口 ClusterIP」 |
| 服務 | `values-demo.yaml` 的 `services` 逐一打開切片的服務（目前 `identity`、`assets`、`certs`），`ingress.routes` 列出 app 主機的 cookie 路由（目前 `/api/identity`、`/api/certs`） | 只開部署中那一版 chart 有映像的服務；切片的 issue 進了 release 後，在這裡打開（certs、AI 閘道與 telemetry-query 要另設的值見下方「打開 certs」「打開 AI 閘道」「打開 telemetry-query」） |
| 外部端點 | Qdrant 的網址在 `.invalid`（RFC 6761，不會解析）；Sentry 關閉 | 切片不用知識庫，SaaS 型態的 schema 又要求外部網址；沒有展示用的 Sentry 專案，錯誤留在 ClickHouse 的日誌 |
| 備份與磁碟 | 沒有備份；Postgres 10Gi、ClickHouse 20Gi | 里程碑「展示切片」接受展示資料遺失；一台閘道器加幾具電表的量 |

SOPS 加密的 Secret（建立後列進 `clusters/demo/trustiot/kustomization.yaml`）：

| 檔案 | Secret 與 key | 內容與產生方式 |
| --- | --- | --- |
| `ghcr-pull.sops.yaml` | `ghcr-pull`，`.dockerconfigjson` | 展示環境自己的 GHCR 拉取 token，一位組織成員的 classic token，只有 `read:packages`（上方「GHCR 拉取 token」）：`<輸出 token 的指令> \| bun scripts/flux/ghcr-pull-secret.ts --cluster demo --username <GitHub 帳號>` |
| `trustiot-smtp.sops.yaml` | `trustiot-smtp`，`username`、`password` | Resend 的 API key，權限只能寄信並限定寄件網域（[密鑰與權限](../../docs/architecture/secrets.md)「外部服務」）：`<輸出 API key 的指令> \| bun scripts/flux/sops-secret.ts --cluster demo --name trustiot-smtp --key password --set username=resend` |
| `trustiot-anthropic.sops.yaml` | `trustiot-anthropic`，`api-key` | 展示專用的 Anthropic 工作區（設每月用量上限）的 API key；只有 AI 閘道讀它（`ai.providers.anthropic.apiKeySecret.name`，見下方「打開 AI 閘道」）：`... \| bun scripts/flux/sops-secret.ts --cluster demo --name trustiot-anthropic --key api-key` |
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
- **打開 certs**（Laysi/trustiot-v2#619）：展示叢集部署的 chart 要含 `deploy/images/certs`、`services.certs` 與 `certificateAuthority`（2026-10-22 的 Laysi/trustiot-v2#78 起），也要含 2026-10-24 的 Laysi/trustiot-v2#609（整版滾動的連線預算檢查，`max_connections` 從 100 調高）：`0.0.0-rc.10` 兩者都沒有，先照下方「換版」換到之後發的版本，這個變更要在換版之後才合併：`chart.yaml` 釘的版本不晚於 rc.10 時，`bun test deploy/flux` 失敗。rc.10 的 schema 接受任何名稱的服務，所以在 rc.10 上合併時渲染照樣通過，但那一版沒有發佈 certs 的映像：pod 拉不到映像，Helm 升級等到 `timeout` 後失敗並 rollback，重試 3 次。分兩次改 `values-demo.yaml`，中間是簽發 CA 的儀式（[展示環境 runbook](../../docs/runbooks/demo-environment.md)「簽發 CA 的儀式」）：
  1. **打開服務與路由**（已在 `values-demo.yaml`）：`services.certs.enabled: true`，以及 app 主機的兩條 cookie 路由：`/api/certs` 給 certs（平台管理員的工作站憑證 API 與出廠登錄，[服務的 README](../../services/certs/README.md)「工作站憑證」「出廠登錄」），`/api/identity` 給身分服務自己的 API（產線工作站工具的 `logout` 與平台管理員的確認都要它，[factory/workstation](../../factory/workstation/README.md)）。不用加 Secret：服務角色的密碼由 migration Job 在叢集裡產生，簽發 CA 的私鑰由 certs pod 的 init container 產生（`<release>-certs-ca-keys`），兩者都不經過人。這時還沒有根憑證：服務照常就緒，記 ERROR `cannot load the issuing CA`（根憑證是空的），之後每次檢查記 WARN `no issuing CA is loaded`，工作站憑證的 API 回 503 `workstation_certificate.ca_not_ready`，等第 2 步。連線預算：identity、assets、certs 各一個副本、一起滾動時要 62 + 28 + 18 條，加上保留的 18 條共 126 條，chart 預設的 `max_connections` 放得下（Laysi/trustiot-v2#609 起 160，Laysi/trustiot-v2#655 起 200），不用改（[chart 的 README](../chart/README.md)「連線預算」）。migration Job 重跑、每個服務滾動重啟一次，不要排在展示前；打開前在展示機上 `free -h` 確認記憶體放得下（服務的記憶體上限是 chart 預設的 256Mi）。
  2. **設定根憑證與簽發 CA 憑證**（儀式之後，另一個 PR）：兩者都是公開資料，各是 DER 的 base64、一行（runbook 的步驟印出）。kubelet 更新掛載的 ConfigMap 後服務自己重讀，pod 不重啟、Job 不重跑。

  ```yaml
  certificateAuthority:
    rootCertificate: <根憑證 DER 的 base64，一行>
    issuingCertificates:
      - <簽發 CA 第 1 代憑證 DER 的 base64，一行>
  ```

  `ingress.routes` 是清單，Helm 會整個取代：之後加別的路由（例如下方的 telemetry-query）要保留這兩條。

- **打開 AI 閘道**（Laysi/trustiot-v2#536）：展示叢集部署的 chart 含 `deploy/images/ai-gateway` 之後，`values-demo.yaml` 加上下面的值，`trustiot-anthropic.sops.yaml` 已在 main 上並列在 `kustomization.yaml`（上表）。`credentials: [ai]` 是 chart 的預設，仍寫出來：`scripts/demo/flux.ts` 只從展示叢集的 values 判斷要哪些 Secret，看不到 chart 的預設。展示環境只有 Anthropic 的金鑰，所以角色只能用 `anthropic/` 的模型；角色名稱與模型由呼叫端的 issue 決定（AI 助理是 Laysi/trustiot-v2#145），沒有角色時 chart 拒絕渲染。`baseUrl` 留空是 Anthropic 自己的 API，要經 Cloudflare AI Gateway 時才設。閘道沒有呼叫端時誰都連不到它（`mesh.calls` 由呼叫端的 issue 加上），打開後確認 pod 就緒；金鑰能不能用，以 chart README「AI 閘道」的 port-forward 送一次最小的呼叫確認。

  ```yaml
  services:
    ai-gateway:
      enabled: true
      credentials: [ai]
  ai:
    providers:
      anthropic:
        apiKeySecret:
          name: trustiot-anthropic
    roles:
      main:                      # 例子：角色與模型由呼叫端的 issue 決定
        models: [anthropic/claude-sonnet-5-5]
  ```

- **打開 telemetry-query**（Laysi/trustiot-v2#539）：展示叢集部署的 chart 含 `deploy/images/telemetry-query` 之後，`values-demo.yaml` 加上下面兩個值：打開服務，加一條 app 主機的 cookie 路由 `/api/telemetry`（設備圖表的歷史查詢 API `/api/telemetry/v1`，[服務的 README](../../services/telemetry-query/README.md)「API」）。不用加 Secret：Postgres 角色與 ClickHouse 服務帳號的密碼由 migration Job 在叢集裡產生（`<release>-telemetry-query-password`），每個租戶的 ClickHouse 唯讀帳號由服務自己建立。路由的 `host: app` 是 `hosts.app` 這個欄位的名稱，不是主機名稱；真的主機名稱仍只在加密的 `trustiot-demo-values` 裡。`ingress.routes` 是清單，Helm 會整個取代，所以加這條路由時要保留 `values-demo.yaml` 已有的路由，之後加別的路由也要保留這一條。服務第一次啟動時從頭讀身分服務的事件，打開之前就有的租戶也會有帳號。打開前確認展示機的記憶體還放得下（服務的記憶體上限是 chart 預設的 256Mi）；打開後確認 pod 就緒，再以一個租戶成員的 session 呼叫 `GET /api/telemetry/v1/devices/<設備 ID>/latest`：帳號建好之前回 404（`telemetry.account_not_ready`），之後回 200。nightly 每晚以同樣的服務與路由部署並這樣呼叫一次，也在 ClickHouse 的 `system.users` 確認新租戶的帳號（[CI 與發版](../../docs/architecture/ci-release.md)「nightly.yml」）。

  ```yaml
  services:
    telemetry-query:
      enabled: true
  ingress:
    routes:                      # 整個清單：連同 values-demo.yaml 已有的路由
      - host: app
        path: /api/identity
        match: PathPrefix
        service: identity
      - host: app
        path: /api/certs
        match: PathPrefix
        service: certs
      - host: app
        path: /api/telemetry
        match: PathPrefix
        service: telemetry-query
  ```

- **換版**：同 SaaS（上方「發版與回滾」），或以 `pin-chart.ts` 只換展示叢集。合併之前先照[展示環境 runbook](../../docs/runbooks/demo-environment.md)「換 chart 版本」比對新舊版本的 `deploy/istio/istiod-values.yaml`，有改就先對齊 istiod。

打開服務、換版、重設展示資料、金鑰輪替與共用叢集的完整步驟在[展示環境 runbook](../../docs/runbooks/demo-environment.md)。

### 在展示機安裝 Flux

`scripts/demo/flux.ts`（Laysi/trustiot-v2#484）在有 repo 的工作站執行，經 ssh 在展示機上產生 deploy key、安裝 Flux、看狀態與移除 Flux。它遵守 [scripts/README.md](../../scripts/README.md)「展示機（`demo/`）」的共同規則：不加 `--apply` 時只讀，印出檢查結果、計畫與確認碼；`--apply --confirm <確認碼>` 才動手，先取得 lab 鎖（擁有者 `Laysi/trustiot-v2#467 <UTC 時間> demo <步驟>`，Laysi/trustiot-v2#467 是在展示機安裝 Flux 的 issue；自己已經持有 #467 的鎖時加 `--lock-held`），在鎖內重新檢查與規劃，與確認過的計畫不同就停；維運用的 tunnel（`cloudflared.service`）只讀狀態、從不改它。紀錄寫在 `$XDG_STATE_HOME/trustiot-v2/demo/`（或 `--log-dir`）。

#### 順序

第一次部署前，main 上要有這些，`install` 逐項檢查，少一項就拒絕、什麼都不改：

1. **chart**：發一版含 Laysi/trustiot-v2#478 的 chart。那一版加上 `ingress.tunnel.clusterIP`，在 tunnel 模式下固定 Istio 產生的入口 Service 的 ClusterIP。tag 由使用者核准後才打。展示環境用 2026-10-21 發的 `0.0.0-rc.10`。
2. **pin PR**：一個 PR 同時做三件事：`bun scripts/flux/pin-chart.ts --cluster demo --version <版本> --digest <chart digest>` 把展示叢集移到那一版；`values-demo.yaml` 設 `ingress.tunnel.clusterIP`（叢集 Service 網段靜態區段裡沒有使用的位址，選法見 [deploy/chart/README.md](../chart/README.md)「固定的入口 ClusterIP」，Laysi/trustiot-v2#478）；`scripts/demo/lib.ts` 的 `DEMO_MIN_CHART_VERSION` 改成那一版。ClusterIP 在 Service 建立後就不能改，Laysi/trustiot-v2#469 的 tunnel 路由指向這個位址，所以第一次部署就要帶著它；`0.0.0-rc.8` 沒有這個值，chart 的 schema 會拒絕不認得的 key，所以第一次部署不用 rc.8。由 Laysi/trustiot-v2#480 完成：釘 `0.0.0-rc.10`，位址設為 `10.43.0.80`。
3. **GHCR 拉取 token**：`ghcr-pull.sops.yaml`（上表），列進 `clusters/demo/trustiot/kustomization.yaml`，開 PR 合併。Laysi/trustiot-v2#480 一併加入。
4. **私有 values**：`trustiot-demo-values.sops.yaml` 已經在 main 上（上表）。HelmRelease 讀它不是 optional，少了主機名稱時 chart 的 schema 拒絕整份 values，什麼都不裝。`trustiot-smtp` 與 `trustiot-anthropic` 不是第一次部署的前提：`values-demo.yaml` 打開的服務要求 `smtp` 或 `ai` 憑證時（例如 Laysi/trustiot-v2#468 的寄信），對應的 Secret 才要在 main 上，`install` 依打開的服務檢查。
5. **叢集的 age 金鑰**：上方「展示叢集的 age 金鑰」（2026-10-20 完成）。

chart 的最低版本是 `scripts/demo/lib.ts` 的 `DEMO_MIN_CHART_VERSION`（`0.0.0-rc.10`），prerelease 依數字比較（`rc.10` 在 `rc.8` 之後）。更早的 chart 沒有 Laysi/trustiot-v2#404（內部身分簽章金鑰在叢集內產生；舊版要人建立金鑰）、Laysi/trustiot-v2#400（建立 Postgres 時收回 `postgres` 與 `template1` 的 PUBLIC 權限，只在 Cluster 建立時發生）或 Laysi/trustiot-v2#478（入口 Service 的固定 ClusterIP，只在 Service 建立時套用）；`0.0.0-rc.9` 有這三項，但沒有 Laysi/trustiot-v2#487（rustls-webpki 的 RUSTSEC-2026-0382 修正）。

#### 步驟

工作目錄必須是 main 的乾淨 checkout：腳本先 `git fetch origin main`，`origin` 不是 GitHub 上的 `Laysi/trustiot-v2`、HEAD 不等於 `origin/main`，或 `deploy/flux`、`scripts`、`.sops.yaml`、`.prototools`、`.proto/plugins/` 有未 commit 的變更時拒絕；標了 assume-unchanged 或 skip-worktree 的檔案（`git status` 看不到它們的變更，`git ls-files -v` 以小寫字母或 `S` 標示）也拒絕。Flux 的 manifest 從這個 checkout 以 `kubectl kustomize deploy/flux/clusters/demo/flux-system` 建出（需要 `proto install kubectl`），只能有 Flux 的物件，剛好一個 GitRepository（`spec.ref` 剛好是 `{branch: main}`）與一個 Kustomization（根，同步 `./deploy/flux/clusters/demo`）；從 main 讀到、之後寫進 YAML 與指令的 Secret 名稱都要是 Kubernetes 的物件名稱（DNS-1123）。檢查 `sops-age` 用的 age-keygen 同上方「祕密」，以 `.prototools` 的 Go 建置成 linux/arm64（需要 `proto install go`）。

`DEMO_SSH_HOST` 要與 lab e2e 以同一個使用者登入展示機：lab 鎖在登入使用者的家目錄（`~/e2e.lock`），換一個使用者就看不到另一邊的鎖。

```sh
: "${DEMO_SSH_HOST:?set DEMO_SSH_HOST to the ssh host of the demo machine}"
git switch main && git pull --ff-only
bun scripts/demo/flux.ts deploy-key --ssh-host "$DEMO_SSH_HOST"                              # 計畫與確認碼，只讀
bun scripts/demo/flux.ts deploy-key --ssh-host "$DEMO_SSH_HOST" --apply --confirm <確認碼>     # 印出公鑰與指紋
# GitHub：Laysi/trustiot-v2 → Settings → Deploy keys → Add deploy key，貼上印出的公鑰，不勾 Allow write access；
# 加好後確認 GitHub 列出的 SHA256 指紋與腳本印出的相同
# 有 repo 管理權限的人以 GitHub API 確認這把 key 唯讀（GET /repos/Laysi/trustiot-v2/keys 裡它的 read_only 是 true）；
# 確認之前不跑 install --apply
bun scripts/demo/flux.ts install --ssh-host "$DEMO_SSH_HOST"                                 # 檢查、計畫與確認碼，只讀
bun scripts/demo/flux.ts install --ssh-host "$DEMO_SSH_HOST" --apply --confirm <確認碼>
bun scripts/demo/flux.ts status --ssh-host "$DEMO_SSH_HOST"                                  # 隨時可看，只讀，不取鎖
```

- **`deploy-key`**：在展示機上以 root 產生 ed25519 金鑰（`/run/trustiot-demo-flux` 底下只有 root 能讀的暫存目錄，結束時 shred），寫成 `flux-system` 的 Secret `flux-system`（`identity`、`identity.pub`、`known_hosts`；`known_hosts` 是本目錄的 `github-known-hosts`，送到展示機後核對雜湊），帶 label `trustiot.io/demo-flux=deploy-key`。私鑰只經過 root 的暫存檔與 kubectl 的 stdin，不出現在指令列或輸出；工作站只讀回 `identity.pub` 與 `known_hosts`。三個值先確認都不是空的才建立 Secret，建立後在展示機上讀回、與產生的檔案逐位元組比對；建立之後任何一步失敗，腳本刪掉剛建立的 Secret，不留下不完整的 deploy key。Secret 已經是它建立的就不重建，只再印一次公鑰與指紋；有空的值時（建立途中被強制終止）拒絕並寫明刪掉重來的指令；同名的 Secret 不是它建立的就拒絕。
- **deploy key 必須唯讀**：Flux 只需要讀 main，可以寫入的 key 從展示機外洩時就能直接推到 repo。key 在 GitHub 上的權限要有 repo 管理權限的 token 才讀得到，腳本在工作站上不檢查；由有管理權限的人在 `install --apply` 之前以 GitHub API 確認（上面的步驟），`install --apply` 等這個確認之後才跑。
- **root 腳本**（`deploy-key`、`sops-age` 的檢查、Flux 的套用與刪除）：開始時以 `flock` 鎖住展示機的 `/run/trustiot-demo-flux/.lock`，另一個 root 腳本還在跑時立刻停下，所以兩次執行（例如兩個 `--lock-held`）不會互相清掉對方的暫存目錄；接著以 shred 清掉之前被強制終止的執行留下的暫存目錄（印出 `STALE`），再建立自己的。
- **`install` 的檢查**：上面「順序」的每一項；lab 鎖空著或屬於 #467（以 `Laysi/trustiot-v2#467 ` 連同空白比對，`#4670` 不算）；`flux-system` 是上方 age 金鑰步驟留下的樣子（沒有其他 label 與 annotation，只有 `sops-age`、Kubernetes 的預設物件與 `deploy-key` 的 Secret），或是這支腳本之前裝的（label `trustiot.io/demo-flux=installed`），其他的一律拒絕，例如 lab e2e 的（label `e2e.trustiot.io/run`）；沒有不屬於這次安裝的 Flux CRD、ClusterRole 或 ClusterRoleBinding；`sops-age` 只有 `age.agekey`，在展示機上以 root 執行 `age-keygen -y` 從它算出的公鑰剛好是 `.sops.yaml` demo 規則的 `# cluster: demo` 收件者（私鑰只經過管線，不印出；age-keygen 只暫存在 root 的暫存目錄，結束時刪除）。這項檢查以 root 讀私鑰，所以只在 lab 鎖空著或屬於 #467、`flux-system` 是上述兩種樣子之一時才做，dry run 也一樣；`--apply` 在鎖內以重新讀到的鎖與 namespace 再判斷一次才做。deploy key 的 Secret 由 `deploy-key` 建立、三個 key 都在而且都不是空的、`known_hosts` 與 main 的相同；PoC 已清除（`scripts/demo/cleanup.ts`）；namespace `trustiot` 不存在或屬於 Flux；平台的 CRD（Istio、Gateway API、CloudNativePG）都在；根 Kustomization、GitRepository 與 HelmRelease `trustiot/trustiot` 都沒有暫停，根 Kustomization 的 `prune` 是 true（中斷的 `remove` 留下的樣子，見下方「中斷之後」）；GitRepository `flux-system` 跟著 main，或是之前中斷的 `install` 留下的 pin（`{branch: main, commit: <commit>}`），其他的 `spec.ref`，以及根 Kustomization 在而 GitRepository 不在，都拒絕。

  `ingress.tunnel.clusterIP` 釘的位址要在叢集唯一的 IPv4 Service 網段（ServiceCIDR `kubernetes`，讀不到就拒絕）的靜態區段裡，不是網路位址或廣播位址，也沒有其他 Service 佔用（之前裝過的 release `trustiot/trustiot` 自己的 Service 除外）。靜態區段是網段開頭的 `min(max(16, 網段大小 / 16), 256)` 個位址（k3s 預設的 `10.43.0.0/16` 是 `10.43.0.1` 到 `10.43.1.0`），計算用 nightly 的 smoke 部署也用的 `scripts/nightly/ingress-address.ts`，與 Laysi/trustiot-v2#483 切換 tunnel 路由前的檢查是同一條規則。動態區段的位址可能在第一次部署之前被別的 Service 拿走，那時 API server 拒絕建立 ingress Service，第一次部署失敗；理由見 [deploy/chart/README.md](../chart/README.md)「固定的入口 ClusterIP」。
- **`install --apply`**：
  1. 給 `flux-system` 加 label `trustiot.io/demo-flux=installed`（field manager `trustiot-demo-flux`，帶讀到的 `resourceVersion`，期間有人改過 namespace 就失敗）。之後的執行、`remove` 與人都靠它認出這次安裝。kustomize-controller 套用 Flux 自己的 Namespace 時只接管 kubectl 與它自己寫的欄位，這個 label 留著；腳本在第一次套用後與 Flux 第一次 reconcile 後各確認一次，不見了就停。
  2. 同上方「建立叢集」第 6 步，但第一次同步釘在檢查過的 commit：manifest 經 stdin 送到展示機，核對 SHA-256 後存在 root 的暫存目錄。送去的版本只改兩處：GitRepository `flux-system` 的 `spec.ref` 加上 `commit: <計畫讀到的 origin/main>`，根 Kustomization `flux-system` 拿出來最後才建立（計畫印出改過的文字的 SHA-256，確認碼涵蓋它）。server-side apply，等 8 個 CRD established，再套用一次；再以 field manager `trustiot-demo-flux` 套用只含這個 pin 的 GitRepository，最後才建立根 Kustomization。
     - 為什麼要釘：檢查是對 lock 內 fetch 到的 `origin/main` 做的。GitRepository 若只寫 `branch: main`，source-controller 第一次 clone 之前 main 可能已經前進到沒有被檢查過的 commit，kustomize-controller（綁 `cluster-admin`）就會套用它，包括只在第一次部署發生、事後改不了的部分（CloudNativePG 只在建立 Cluster 時收回 PUBLIC 權限、建立後不能改的入口 ClusterIP）。`commit` 與 `branch` 都有時，source-controller v1.9.6 只 clone 這個 commit（以 `github.com/fluxcd/pkg/git` v0.52.0 的 gogit client；commit 不在 main 上就失敗），revision 仍寫成 `main@sha1:<commit>`。根 Kustomization 在 pin 之後才建立，所以 kustomize-controller 第一次套用的一定是這個 commit，沒有空檔。
     - 為什麼另外以 `trustiot-demo-flux` 套用 pin：根 Kustomization 套用 main 的 `gotk-sync.yaml`（沒有 `commit`）時，kustomize-controller v1.9.6 會清掉 kubectl 與它自己寫過的欄位，其他 field manager 一起擁有的欄位留著；所以 pin 一直留到腳本自己拿掉。
  3. 依序等三個控制器、GitRepository `flux-system`（等不到時提示 GitHub 上要有那把 deploy key）、Kustomization `flux-system`、OCIRepository 與 HelmRelease `trustiot/trustiot`。每個 Flux 物件都要在目前的 generation `Ready=True`，而且停在計畫的 revision：GitRepository 的 artifact 與 Kustomization 的 `lastAppliedRevision` 是 `main@sha1:<計畫讀到的 origin/main>`，OCIRepository 的 artifact 是 `chart.yaml` 釘的 digest，HelmRelease 最新一筆 history 是 `deployed` 的 `<版本>+<digest 前 12 位>`。只看 `Ready=True` 不夠：中斷過的 run 或較舊的 main 留下的物件，在控制器追上之前一直是舊 revision 的 `Ready=True`。pin 還在的期間，每次輪詢也讀 GitRepository：`spec.ref` 不再是 `{branch: main, commit: <這次的 commit>}`，或目前 generation 的 artifact 是別的 commit 時，立刻停下（不等逾時），訊息寫明現在的 `spec.ref` 或 revision，之後什麼都不改。
  4. Kustomization 套用了這個 commit 之後，在 HelmRelease 設 annotation `reconcile.fluxcd.io/requestedAt`（同 `flux reconcile`，field manager `trustiot-demo-flux`），等 helm-controller 把它記進 `status.lastHandledReconcileAt`、release 也在上述 chart `Ready=True` 才算完成。chart 版本相同時看不出 values：跟著 main 改的 `values-demo.yaml` 或 SOPS 加密的 Secret 只經 watch 通知 helm-controller，這次要求的 reconcile 一定讀到這個 commit 的 values。
  5. 第一次部署在這個 commit 與釘選的 chart 確認之後，拿掉 pin，讓 Flux 再跟著 main：以 JSON patch（`test` 確認 `spec.ref.commit` 還是這個 commit，再 `remove`，field manager `trustiot-demo-flux`）拿掉，不靠 kustomize-controller 清欄位；ssh 斷線或 patch 失敗時讀回 GitRepository，已經是 `{branch: main}` 就當作完成。pin 在這之前已經不在（第 4 步確認之後才不見的）時不送 patch，印出它已經不在。接著等 GitRepository 跟著 main：`spec.ref` 剛好是 `{branch: main}`、目前 generation `Ready=True`、artifact 是 `main@sha1:<某個 commit>`。main 在安裝期間前進了，這時 Flux 會套用新的 main，腳本印出 `main moved on since this run checked <commit>: Flux now follows main at <revision>, which this run did not check`，新的部署以 `status` 看；它已經不是這次 `install` 檢查過的內容。

     第 3 到 5 步每 10 秒以短指令讀一次（ssh 會切斷約 3 分鐘沒有輸出的指令），各有上限：控制器、GitRepository（兩次）、Kustomization 各 10 分鐘，OCIRepository 15 分鐘，HelmRelease 50 分鐘（每次 10 分鐘、remediation 重試 3 次）；`Stalled` 時立刻停，逾時的訊息寫明在等什麼 revision 與最後的狀態。pin 讓第一次部署不受 main 前進影響；`flux-system` 已經跟著 main（之前的 `install` 已經完成）時，等的時候 main 又前進了，GitRepository 會停在新的 commit 而逾時：`git fetch` 之後從新的 main 再跑一次（先 dry run）。
  6. 再確認根 Kustomization、GitRepository 與 HelmRelease 都沒有暫停、根 Kustomization 的 `prune` 是 true，否則以錯誤結束（Flux 不再跟著 main 時不說成完成）；印出與 `status` 相同的內容，以及叢集層級物件的前後差異。
- **成功的樣子**：最後印出三個 Deployment `1/1 available`；GitRepository、Kustomization、OCIRepository、HelmRelease 都是 `Ready=True`，GitRepository 與 Kustomization 的 revision 是 `main@sha1:<main 的 commit>`，OCIRepository 的是 `sha256:<digest>`；`main's deploy/flux/clusters/demo/trustiot/chart.yaml: <版本>@sha256:<digest>` 之下 `pulled: the same` 與 `deployed: the same (<版本>+<digest 前 12 位> (deployed))`；namespace `trustiot` 的 pod；最後一行 `done; records in <紀錄目錄>`。拿掉 pin 之後還有一行 `GitRepository flux-system/flux-system following main: Ready=True ...; at main@sha1:<commit>`。失敗時最後印出 `ERROR <原因>` 與 `log: <日誌路徑>`。
- **遮蔽**：`flux.ts` 印出或存下的一切（訊息、錯誤、最後的 `ERROR` 行、計畫與紀錄檔，連 JSON 的 key 也算）都先經過 `redact()`：ssh、kubectl 與 kustomize 的錯誤可能帶著區網位址、ssh 的主機別名、存取用的主機名稱與工作站的路徑。規則是 fail-closed，先全部換掉再放回允許的：
  - 凡是看起來像主機名稱的字（以點分隔、最後一段是字母；前面是 `*`、`.`、`-` 或空的 label 都算）換成 `<host>`，只放回完全相同的公開名稱（`flux.ts` 的 `SHOWN_NAMES`）、叢集內的 DNS 名稱（`<name>.<namespace>.svc` 或 `.pod`，可以再接 `.cluster.local`）、Kubernetes 與 Flux、Istio、CloudNativePG 的 API group（`<resource>.<group>` 也算）、副檔名不是頂級網域的檔名；`trustiot.io` 與 `k8s.io` 結尾的名稱只在 label 或 annotation 的 key 的 `/` 前面保留（`trustiot.io/demo-flux`、`app.kubernetes.io/name`），其他的子網域一樣換掉。
  - k3s 的 pod 與 Service 網段（`10.42.`、`10.43.`）與 loopback 以外的 IPv4、loopback 以外的 IPv6 換成 `<address>`，後面接 `.` 或前面接 `.` 的位址也算。
  - 私鑰、JWT、token 與 API key（`gh[pousr]_`、`github_pat_`、`re_`、`sk-`、`AKIA` 開頭的）、HTTP 的 `Bearer` 與 `Basic` 值、`password=`、`token=` 這類設定的值、URL 裡的帳號密碼（`git@` 除外）、長的 base64，以及 20 字元以上大小寫字母與數字混雜的字串換成 `<redacted>`；email 在放回允許的名稱之前就換成 `<email>`（`someone@github.com` 也是，clone 用的 `git@github.com` 除外）。腳本要印的東西保留：`sha256:<hex>` digest、40 位 commit 與 `main@sha1:<commit>`、ssh 公鑰與它的 `SHA256:` 指紋、age 收件者。
  - 工作站的路徑：repo 的根目錄寫成 `<repo>`，家目錄（`$HOME`、其他 `/home/<名稱>`、`/Users/<名稱>`、`/root`）寫成 `~`。ssh 自己的錯誤裡的 ssh 主機寫成 `<ssh-host>`。
- **中斷之後**：Flux 在叢集裡跑，工作站中斷不影響它。lab 鎖留著時依 scripts/README.md「中斷之後」處理。ssh 在 root 腳本執行中斷線（exit 255）時，`sudo bash -c` 在展示機上可能還在跑，所以腳本不釋放 lab 鎖，印出 `the lab lock stays held for Laysi/trustiot-v2#467` 與要檢查的事：在展示機上 `sudo flock --nonblock /run/trustiot-demo-flux/.lock true` 成功時已經沒有這支腳本的 root 腳本在跑。之後再跑一次 `install` 的 dry run，計畫依叢集的樣子決定：
  - 根 Kustomization 還不存在：從套用重來（server-side apply 可以重複），pin 也一起重來。
  - 根 Kustomization 已經存在、GitRepository 還釘在這次檢查的 commit（上次在等第一次部署時中斷）：不再套用（Flux 已經管理自己，再以 kubectl 套用會和它搶欄位），以 `trustiot-demo-flux` 再套用一次 pin，等第一次部署，再拿掉 pin。
  - GitRepository 釘在較舊的 commit（中斷之後 main 前進了）：這次檢查的是新的 main，計畫先以 JSON patch（`test` 舊的 commit，`replace` 成新的）把 pin 移到新的 commit，之後同上；根 Kustomization 還不存在時移完再套用。舊的 commit 是之前的執行檢查過的，移動之前 Flux 只會套用它。
  - GitRepository 已經跟著 main（之前的 `install` 已經完成）：只等待與回報。
  - 根 Kustomization 暫停或 `prune: false`，或 GitRepository、HelmRelease 暫停：`install` 拒絕，不自動恢復。這是中斷的 `remove` 留下的樣子（或有人以 `flux suspend` 暫停了它們）。要繼續移除就再跑 `remove`（先 dry run），它從暫停的根 Kustomization 接著做完。要恢復 Flux，只在 GitRepository 與根 Kustomization 都還在時，持有 lab 鎖，把暫停的那幾個改回來，再跑 `install` 的 dry run 確認：

  ```sh
  sudo k3s kubectl -n flux-system patch kustomizations.kustomize.toolkit.fluxcd.io flux-system --type=merge -p '{"spec":{"suspend":false,"prune":true}}'
  sudo k3s kubectl -n flux-system patch gitrepositories.source.toolkit.fluxcd.io flux-system --type=merge -p '{"spec":{"suspend":false}}'
  sudo k3s kubectl -n trustiot patch helmreleases.helm.toolkit.fluxcd.io trustiot --type=merge -p '{"spec":{"suspend":false}}'
  ```

  `remove` 已經刪掉 HelmRelease 時，恢復的根 Kustomization 從 main 重新建立它，Helm 接管留下的 Postgres 與磁碟區（同下方「移除後再裝」，尚未在叢集上驗證）。`deploy-key` 只有一個 `kubectl create`，中斷後重跑會看到已經建立的 Secret 並再印一次公鑰；被強制終止而留下有空值的 Secret 時拒絕，並寫明刪掉重來的指令。所以這裡不用 systemd 的暫時 unit：每一步都是短的、可以重跑的指令，不像 connector 的安裝程式要長時間不中斷地跑（[展示環境 runbook](../../docs/runbooks/demo-environment.md)「安裝 connector」）。

`install` 改動的叢集層級物件：Flux 的 8 個 CRD（`*.toolkit.fluxcd.io` 的 buckets、externalartifacts、gitrepositories、helmcharts、helmrepositories、ocirepositories、helmreleases、kustomizations）、ClusterRole `crd-controller-flux-system`、`flux-edit-flux-system`、`flux-view-flux-system`、ClusterRoleBinding `crd-controller-flux-system` 與 `cluster-reconciler-flux-system`；後者把 kustomize-controller 與 helm-controller 綁到 `cluster-admin`，所以合併進 main 的 `clusters/demo` 能改展示機 k3s 上的任何東西，包括 lab e2e 共用的部分（SaaS 叢集也是如此）。namespace `flux-system` 加上 label，裡面多了三個控制器與它們的 ServiceAccount、Service、NetworkPolicy、ResourceQuota。之後 Flux 從 main 建立 namespace `trustiot` 與 release。

deploy key 換新：先在展示機上刪除 Secret `flux-system/flux-system`（持有 lab 鎖），再跑 `deploy-key`、在 GitHub 加新的公鑰、刪舊的。期間 Flux 讀不到 main、不套用新的變更，正在跑的服務不受影響。

#### 移除

```sh
bun scripts/demo/flux.ts remove --ssh-host "$DEMO_SSH_HOST"                                  # 計畫與確認碼，只讀
bun scripts/demo/flux.ts remove --ssh-host "$DEMO_SSH_HOST" --apply --confirm <確認碼>
```

只移除這支腳本裝的 Flux（`flux-system` 帶 `trustiot.io/demo-flux=installed`）。以下情況拒絕，什麼都不刪：`flux-system` 是別人的；`flux-system` 裡有安裝以外的物件（安裝只有 Kubernetes 的預設物件、`sops-age`、`deploy-key` 的 Secret、main 的 Flux manifest 裡的物件與控制器的 leader election Lease；被其他物件擁有的 Pod、ReplicaSet 等不算），刪除 namespace 會連帶刪掉它們，Flux 刪除之前還會再檢查一次；叢集裡有不是根 Kustomization `flux-system` 從 main 套用的 Flux 物件（依 kustomize-controller 的 label `kustomize.toolkit.fluxcd.io/name` 與 `namespace` 判斷；根的 GitRepository 與 Kustomization 本身除外），例如有人手動建立的 HelmRelease，否則它會被刪除、它的 Helm release 被解除安裝，刪除 Flux 的 CRD 也會連帶刪掉它；有 Flux 物件的 kind 不在 main 的 manifest 裡；有 main 的 manifest 沒有定義的 Flux CRD 或帶 `app.kubernetes.io/part-of=flux` 的 ClusterRole、ClusterRoleBinding（第 4 步只刪 main 定義的，會停在一半），Flux 刪除之前同樣再檢查一次；HelmRelease 處於暫停。順序依釘選版本的 Flux 的行為（kustomize-controller v1.9.6、helm-controller v1.6.5 的刪除流程）：

1. 把根 Kustomization `flux-system` 改成 `suspend: true`、`prune: false`。沒有暫停、`prune: true` 的 Kustomization 被刪除時，kustomize-controller 會刪掉它套用過的一切，包括 namespace `trustiot` 與裡面的資料；暫停時只拿掉 finalizer，不刪任何東西。暫停之後重新列出 Flux 物件，與計畫的不同時（暫停生效之前，Flux 從新的 main 建立或刪除了物件）停下、什麼都不刪，根 Kustomization 維持暫停：再跑一次 `remove`（先 dry run），或依上方「中斷之後」恢復 Flux。之前中斷的 `remove` 留下暫停的根 Kustomization 時，`remove` 照樣接著做完。
2. 刪除 HelmRelease `trustiot/trustiot`，等它消失（最多 20 分鐘）：沒有暫停的 HelmRelease 被刪除時，helm-controller 解除安裝 Helm release。暫停的 HelmRelease 被刪除時 release 會留著、沒有人管理，所以腳本先拒絕。
3. 刪除其他 Flux 物件（OCIRepository），再刪 GitRepository 與 Kustomization `flux-system`，各自等它們消失。
4. 以 `kubectl delete -f` 刪除 main 的 Flux manifest 的全部 27 個物件：CRD、ClusterRole、ClusterRoleBinding、控制器，以及 namespace `flux-system`，連同 `sops-age` 與 deploy key 的 Secret；等到它們都消失。
5. 印出 namespace `trustiot` 留下的 PersistentVolumeClaim、Secret 與 Postgres Cluster。

留下的資料：namespace `trustiot` 本身（Kustomization 已經不 prune）；帶 `helm.sh/resource-policy: keep` 的 CloudNativePG Cluster 與 ImageCatalog，Postgres 由 operator 繼續執行，連同它的磁碟區；ClickHouse 的 PersistentVolumeClaim（StatefulSet 的 `whenDeleted: Retain`）；migration Job 與 init container 寫入的密碼 Secret、內部身分簽章金鑰的 Secret 與公鑰 ConfigMap（都帶 `helm.sh/resource-policy: keep`）；Flux 從 git 解密套用的 Secret（`ghcr-pull`、`trustiot-demo-values`），不再需要時以 `sudo k3s kubectl -n trustiot delete secret <名稱>` 刪除。服務的 Deployment 與 Service 隨 release 移除，展示環境中斷。

移除後再裝：兩把金鑰已經跟著 `flux-system` 刪掉，所以照上方「展示叢集的 age 金鑰」產生新的 age 金鑰、開 PR 換掉 `.sops.yaml` 的 demo 收件者並 `sops updatekeys -y` 每個 demo 的 SOPS 檔；在 GitHub 刪除舊的 deploy key，以 `deploy-key` 產生新的；再 `install`。namespace `trustiot` 留著時 `install` 的計畫會註明：Helm 依物件上的 release 標註接管留下來的 Postgres、Secret 與磁碟區，這一點還沒在叢集上驗證過（上方「日常操作」）。

lab e2e：`tests/e2e/lab.e2e.ts` 在叢集已經有 Flux（namespace `flux-system` 或 Flux 的 CRD）時拒絕執行，也只移除自己建立的 Flux（下方「lab 測試」）；反過來，`flux.ts` 的子指令都不碰帶 lab e2e label（`e2e.trustiot.io/run`）的 `flux-system`。要跑 lab e2e，先以 `remove` 移除展示環境的 Flux，測完照上一段重新安裝。期間展示環境中斷，閘道器的資料會留下缺口，所以只在必要時（例如驗證 Flux 的升級）這樣做。

## 檢查

`bun test deploy/flux`（CI 的 `helm` job），每個叢集都檢查：

- `kubectl kustomize` 建得出整棵樹，內容只有 Flux、同步設定與 TrustIoT release；展示叢集的 Flux 與 SaaS 的逐一相同，只有同步路徑不同。
- 每個物件通過 kubeconform `-strict`；Flux 的物件以 `gotk-components.yaml` 裡同版本的 CRD 驗證，不需要網路。
- `gotk-components.yaml` 與釘選版本的 `flux install --export` 相同，控制器映像都以 digest 釘選。
- `chart.yaml` 是 `renderChartSource` 的輸出：digest、拉取 token、只接受這個 tag 的 release.yml 簽章；沒有其他可以略過驗簽的來源（HelmRepository、Bucket）。
- `values-saas.yaml` 來自部署中的版本；HelmRelease 依序讀兩份 values；目錄裡的每個 SOPS 檔都有套用。
- 每個 SOPS 檔都是該叢集 `SOPS_SECRETS` 裡的 Secret，type 相符，在 namespace `trustiot`，檔名是 `<名稱>.sops.yaml`，並符合上方「祕密」的 SOPS 規則；以假的 sops 輸出確認：chart 保留的名稱（`trustiot-identity-signing-keys`）、type 不對的 `ghcr-pull`、加密範圍加上 annotation、加密的 label 與 Helm 的歸屬 annotation 都被擋下。
- `values-instance.yaml` 留空的只有上表的欄位，填上測試值後 HEAD 的 chart 接受它，服務的 pod 以 `ghcr-pull` 拉映像。
- 展示叢集：HelmRelease 與 SaaS 的相同，只多了私有 values：每個讀 Secret 的項目都有 `targetPath`（四個值之一）、對應的 key 與 `literal: true`，沒有整份 values 的項目；產生器與 commit 的 Secret 剛好有那四個 key。以測試用的私有 values（`.test` 網域，寄件人含空白、`<…@…>` 與逗號）渲染時，值原樣出現在 ConfigMap，整份輸出與用 values 檔給同樣的值相同；同樣的值改用 `--set` 語法時，逗號後面會改到映像的 registry（證明 `literal` 擋下了什麼）。少了私有 values 時 chart 拒絕。
- `values-demo.yaml` 不含主機名稱、寄件人與 IP：每個像主機名稱的字（值、URL 與註解都算）都在 `.invalid` 或 `.test` 之下、是列出的公開服務（`smtp.resend.com`）、檔名，或 chart values 的路徑；沒有 `@`。
- `.sops.yaml` 每個叢集一條規則，規則不會比對到別的叢集的檔案；每個收件者一行並註明角色；每條規則是空的，或剛好是自己叢集的金鑰加上每一把維運人員的金鑰；`github-known-hosts` 與 GitHub 公布的指紋相同。

release 流程的部分在 `bun test scripts/release`（`flux.test.ts`、`deploy.test.ts`：PR 的內容、每個叢集的版本比較、只移動較舊的叢集、重跑（分支已經存在時，它改的檔案或內容與這次該寫的不同就停下）、缺少或被手改的 `chart.yaml`、GitHub 拒絕開 PR 時的訊息）；`scripts/flux/lib.test.ts` 檢查 Secret 的產生、輸入值的檢查、私有 values 的 key、收件者的角色與 `pin-chart.ts`；`scripts/flux/sops.test.ts` 檢查寫檔前對 sops 輸出的檢查與 stdin 必須是管線。展示機的安裝腳本 `scripts/demo/flux.ts` 在 `bun test scripts/demo`（`flux.test.ts`）：上方「在展示機安裝 Flux」的每個拒絕條件、計畫與確認、第一次同步釘在檢查過的 commit（安裝期間 main 前進、釘住時 GitRepository 回報別的 commit 或 pin 被改掉、中斷後重跑與移動 pin、拿掉 pin 後跟著 main）、暫停的 Flux 物件、移除的順序與暫停後的重新比對、ssh 斷線時保留 lab 鎖、遮蔽規則（ssh、kubectl 與 kustomize 的錯誤帶著的位址、主機名稱與路徑不出現在輸出、錯誤與紀錄檔），以及以 bash 對假的 `k3s` 執行送到展示機的 root 腳本：不把私鑰印出來、空的值不建立 Secret、失敗時刪掉不完整的 Secret、清掉殘留的暫存目錄、另一個 root 腳本在跑時停下。

## lab 測試

`tests/e2e/lab.e2e.ts` 在實驗機（k3s，唯一的 Kubernetes 環境）以 README 的方式裝 Flux，驗證整條路。它在整個叢集裝上再移除 Flux，所以叢集已經有 Flux（namespace `flux-system` 或 Flux 的 CRD）時拒絕執行；實驗機也是展示機，展示環境的 Flux 在時要先以 `scripts/demo/flux.ts remove` 移除它才能跑（見上方「在展示機安裝 Flux」）。它開始時與裝 Flux 之前各檢查一次：lab 鎖 `~/e2e.lock/owner` 是一行 `Laysi/trustiot-v2#<issue> <UTC 時間>`，issue 與 `E2E_NAMESPACE`（`e2e-<issue>`）相同（連同後面的空白比對，`#4570` 不會被當成 `#457`），叢集上沒有 Flux。測試 namespace 與 `flux-system` 都以這次執行的 ID 為 label（`e2e.trustiot.io/run`）建立，建立時已經存在就停下，什麼都不裝。`gotk-components.yaml` 也宣告 `flux-system`，Flux 也會套用自己的那棵樹，所以以 server-side apply 套用 Flux 的 manifest 之後立刻、Flux 第一次 reconcile 之後再一次重新讀 `flux-system` 的 label，不是這次的 ID 就停下，並說明要手動移除什麼（Flux 的 manifest 存在 `E2E_OUT`）。結束時重新讀兩個 label，只刪仍屬於這次執行的東西，刪什麼、照什麼順序由 `tests/e2e/preflight.ts` 的 `teardownSteps` 決定（有單元測試）：`flux-system` 不是這次的就不刪 Flux 的物件、manifest（CRD、ClusterRole、ClusterRoleBinding）與 namespace，測試 namespace 不是這次的就不刪它，兩種情形都讓測試失敗。驗證內容：合併 deploy commit 後在間隔內部署、revert 後回到前一版、別的金鑰簽的 chart 不會被安裝、SOPS 加密的拉取 Secret 被解密並用來拉 chart。測完移除 Flux 與測試 namespace，並比對叢集層級的物件與其他 namespace 的 workload 與測試前相同。

```sh
ssh trustiot-mac-m1 'mkdir ~/e2e.lock && echo "Laysi/trustiot-v2#70 $(date -u +%FT%TZ)" > ~/e2e.lock/owner'
E2E_SSH_HOST=trustiot-mac-m1 E2E_NAMESPACE=e2e-70 E2E_OUT=<輸出目錄> bun deploy/flux/tests/e2e/lab.e2e.ts
ssh trustiot-mac-m1 'grep -q "^Laysi/trustiot-v2#70 " ~/e2e.lock/owner && rm -rf ~/e2e.lock'
```

與正式環境不同的地方（都只在測試裡）：git 伺服器是測試 namespace 裡的 soft-serve（同樣以 SSH 與唯讀金鑰讀取），registry 是有 TLS 與帳密的一次性 registry，chart 是小的測試 chart；簽章用一次性的 cosign 金鑰對，`chart.yaml` 以 `secretRef`（測試公鑰）取代 `matchOIDCIdentity`，因為 keyless 簽章只能由 release.yml 在 `v*` tag 上產生。其餘欄位與 `renderChartSource` 的輸出相同。所有金鑰、密碼與憑證都在執行時產生，結束後刪除。結果見 [測試報告](../../docs/test-reports/2026-10-11-70-flux-gitops.md)。
