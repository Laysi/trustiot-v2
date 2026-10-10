# CI 與發佈

GitHub Actions，三個 workflow：

1. `ci.yml`：每個 PR（不論目標分支，整合分支也一樣）與推到 main 都跑，全綠才能合併（目前由流程把關，見下方「必要檢查與強制方式」）。Rust 跑 fmt、clippy、測試與 sqlx 離線檢查，並以 cargo-deny 檢查授權與漏洞公告（地端產品不能混入 BSL 等不能轉售的授權）；整合測試以 service container 起含 AGE 的 Postgres、ClickHouse 與 Mosquitto，跑完全部 migration 後跑跨租戶負向測試（RLS、row policy、AGE 角色）；閘道器程式交叉編譯成 aarch64，agent 用 testkit 的假 Modbus 端點跑故障注入測試；前端跑 Bun 安裝、型別檢查、lint、單元測試與建置；重新產生 `api/openapi.json` 與前端 client，有差異就失敗；helm lint，兩份 values 各自 helm template 後以 kubeconform 驗證（`bun test deploy/chart` 一次跑完，也檢查 chart 自己的安全規則，見 `deploy/chart/README.md`）；gitleaks 掃描祕密。各項由哪個 job 負責、哪些還沒加入，見下方「ci.yml 的 job」。Codex 審查設為 PR 的必要檢查，是伺服器端的關卡；本機 pre-commit 只是提早攔下。
2. `release.yml`：打 tag 時跑。容器映像只出 arm64：Rust 先交叉編譯再放進映像，推到 GHCR，以 cosign 簽章並附 SBOM；SaaS 主機與展示機（Mac M1）都要是 arm64，地端客戶若用 x86 主機，到時再加 amd64。Helm chart 打包推到 GHCR。閘道器 agent 與 updater 的 aarch64 執行檔附建置來源證明，由 CI 以 OTA 簽章金鑰簽章：金鑰存在受保護的 GitHub environment，只有 main 上的 tag 能用，每次簽章都要人工核准。另外產出離線安裝包（MS6）；release notes 沿用 `Laysi/release-notes-action`。
3. `nightly.yml`：每晚用 k3d 部署整套 chart（兩份 values 各一次），裝好 Istio ambient 後跑冒煙測試與入口 e2e 測試，失敗時開 issue 通知；依賴更新交給 Renovate。做什麼、怎麼通知與失敗時怎麼辦見下方「nightly.yml」與「Renovate」。

SaaS 以 GitOps 部署：叢集內的 Flux 從 GHCR 拉映像與 chart，設定放在 `deploy/flux/`；發版就是合併一個改 chart 版本號的 PR（由 release 流程自動開）。GitHub 不持有任何部署憑證，主機也不必對外開放，叢集只存一把唯讀的 GHCR 拉取 token；Flux 控制器的記憶體算進主機規格（MS3 重測）。地端不裝 Flux，由客戶以 Helm 安裝或用離線安裝包。

通用規則（[#158](https://github.com/Laysi/trustiot/issues/158)、[#120](https://github.com/Laysi/trustiot/issues/120)、[#122](https://github.com/Laysi/trustiot/issues/122)）：每個 workflow 明列最小權限，每個 job 都寫出自己的 `permissions`，workflow 層級設 `permissions: {}`；第三方 action（含 GitHub 官方的 `actions/*` 與 reusable workflow）一律釘 40 字元的 commit SHA，行尾註解寫版本；容器映像（`docker://` action、job 與 service container）釘 digest；需要雲端憑證時用 OIDC，不放長期金鑰；發佈走 GitHub environment 加人工核准；fork 來的 PR 拿不到任何 secret，所以不使用 `pull_request_target` 與 `workflow_run` 觸發。權限、釘選與觸發條件三項由 CI 的 `workflow-policy` job（`scripts/ci/check-workflows.ts`）檢查每個 workflow 與 `.github/actions/` 下的 action。本地 action（`uses: ./...`）不必釘 SHA，因為它隨使用它的變更一起審查，但只能放在 `.github/actions/` 下，本地 reusable workflow 只能直接放在 `.github/workflows/`，才在檢查範圍內；symbolic link 依實際指向的檔案檢查。

## ci.yml 的 job

2026-10-10 上線（Laysi/trustiot-v2#67）。同一個 PR 有新的推送時取消舊的執行；推到 main 的每個 commit 各自一個 concurrency group，一律跑完，每個 main commit 都有結果（共用一個 group 時，GitHub 會以較新的執行取代還在排隊的執行，即使沒有設 `cancel-in-progress`）。本機重跑的指令列在 `CLAUDE.md` 的「Build, lint, and test」。

| job（檢查名稱） | 內容 |
| --- | --- |
| `rust` | `cargo fmt --all --check`；`cargo clippy --workspace --all-targets --locked -- -D warnings`；`cargo test --workspace --locked`（含 `tools/repo-check` 的 repo 規則、`crates/service-kit` 以 `cargo tree` 檢查關閉 `contract` feature 時不依賴 contract，以及之後各 crate 的測試，例如 agent 以 testkit 假 Modbus 端點跑的故障注入測試）。`--workspace` 只建置預設 feature，所以 service-kit 關閉預設 feature 另跑一次 clippy 與測試（`--package service-kit --no-default-features`）。service-kit 的容器測試（以 PID 1 執行的範例服務、advisory lock 用的 Postgres）用 runner 上的 podman 或 docker 起容器；GitHub 設定 `CI=true`，找不到可用的容器執行環境時這些測試失敗而不是略過，所以測試步驟通過就表示它們真的跑過。全程設 `SQLX_OFFLINE=true`：sqlx 的查詢巨集只讀進版控的 `.sqlx/`，查詢資料沒更新就編譯失敗。接著執行 `cargo run --locked --package openapi` 重新產生 `api/openapi.json`（`tools/openapi`，各服務由程式碼產生的 OpenAPI 規格加上 M1 錯誤 schema 合併而成），有任何差異就印出差異並失敗，所以改了 API 型別卻沒重新產生規格的 PR 過不了；同一項比對也在 `cargo test --workspace` 裡（`tools/openapi/tests/openapi.rs`）。最後把 `crates/protocol` 與 `edge/` 下的每個成員以 `aarch64-unknown-linux-gnu` 建置；還沒有這些 crate 時只留下說明，protocol crate（Laysi/trustiot-v2#50）併入後自動涵蓋。函式庫建置不需要交叉連結器，第一個閘道器執行檔加入時一併加上。前一項失敗時後面幾項照跑，一次回報全部結果 |
| `cargo-deny` | `cargo deny --locked check`，規則在根目錄的 `deny.toml`：授權白名單（見下方「授權白名單」）；RustSec 的漏洞、unmaintained 與 unsound 公告一律失敗，要忽略某則公告必須在 `deny.toml` 寫明理由與到期日；Cargo.lock 裡有 yanked 版本就失敗；相依只能來自 crates.io |
| `helm` | `bun test deploy/chart`：`helm lint --strict`；`values-saas.yaml` 與 `values-onprem.yaml` 各自 helm template 後以 `kubeconform -strict` 驗證（Kubernetes schema 由 kubeconform 下載）；再加上 chart 自己的規則與 README 欄位檢查，見 `deploy/chart/README.md` |
| `gitleaks` | 掃描 HEAD 能走到的每個 commit（完整歷史）；merge commit 也以對第一個 parent 的差異掃描，只在合併時加入的內容也查得到。只掃 HEAD，不掃 checkout 時一起抓下來的其他分支。發現時在該檔案與行號標註，附 fingerprint；報告遮蔽祕密值，不會印進紀錄 |
| `workflow-policy` | `scripts/ci/check-workflows.ts` 檢查 `.github/workflows/` 的 workflow 與 `.github/actions/` 的 action，規則即上方通用規則中的權限、釘選（含本地 action 的位置）與觸發條件三項；再跑 `scripts/` 全部 TypeScript 的型別檢查與測試（含本地開發環境 Laysi/trustiot-v2#71 的單元測試；它的端對端測試要以 podman 起整套環境並編譯服務，不在 CI 執行） |

之後由其他 issue 加進 ci.yml 的檢查（加入時同步更新本表與必要檢查清單）：

| 檢查 | 加入的 issue |
| --- | --- |
| 整合測試：service container 起含 AGE 的 Postgres、ClickHouse、Mosquitto，在全新資料庫跑完全部 migration 後跑跨租戶負向測試（RLS、row policy、AGE 角色）；在同一個資料庫上跑 `cargo sqlx prepare --workspace --check`，確認 `.sqlx/` 與 schema 一致 | Laysi/trustiot-v2#60（負向測試框架為 Laysi/trustiot-v2#56） |
| 擋下把外部輸入拼進 SQL 字串的靜態檢查 | Laysi/trustiot-v2#53 |
| 前端：Bun 安裝、型別檢查、lint、單元測試、建置；重新產生 API client，有差異就失敗 | Laysi/trustiot-v2#123 |

### 必要檢查與強制方式

必要檢查清單（ci.yml 的 job 名稱）：`rust`、`cargo-deny`、`helm`、`gitleaks`、`workflow-policy`。

2026-10-10 確認：本 repo 是私有庫，組織在 GitHub Free 方案，分支保護與 ruleset 都不能用（API 回 HTTP 403「Upgrade to GitHub Pro」），GitHub 不會擋下沒通過檢查的合併。方案升級前由流程把關：合併任何 PR 之前，確認上列檢查在該 PR 最新的 commit 上全部通過。方案升級後，在 main 的 ruleset 把上列檢查設為必要檢查。

### 省 Actions 分鐘數

私有庫在 Free 方案的 Actions 分鐘數有上限，所以：同一個 PR 的新推送會取消舊的執行；每個 job 設 `timeout-minutes`；Rust 的編譯快取（`Swatinem/rust-cache`）只在 main 儲存，PR 只讀取；aarch64 一律交叉編譯，不用模擬器；同一項檢查只跑一次。唯一的 arm64 代管 runner（`ubuntu-24.04-arm`）給 nightly 的 `smoke` job，因為 chart 預設的 nodeSelector 是 `kubernetes.io/arch: arm64`（決策 5），叢集架構要與實際出貨的一致；其他 job 都在 x64。

### 工具安裝

CI 以 `.github/actions/setup-proto` 安裝 proto：版本與兩種架構（x64、arm64）各自壓縮檔的 SHA-256 寫死在該 action，不執行會變動的線上安裝腳本；再以 proto 安裝 `.prototools` 釘選的工具。沒有內建 proto 外掛的工具（cargo-deny、gitleaks、helm、k3d、kubeconform、kubectl）在 `.proto/plugins/` 設定，下載官方 release 並以 release 附的 checksum 驗證。Rust 是例外：proto 的 Rust 外掛本身只是呼叫 rustup，而 `proto install rust` 在 GitHub 代管 runner 上無法完成（2026-10-10 以暫時 PR 實測：job 超過 10 分鐘的上限仍未結束，同一個 action 裝 Bun、gitleaks 都正常；沒有 Actions 紀錄的讀取權限，原因待查），所以 setup-proto 在 CI 直接用 runner 映像內建的 rustup 執行不帶參數的 `rustup toolchain install`，依 `rust-toolchain.toml` 安裝釘選的版本、元件與 profile。本機照常由 proto 經 rustup 安裝。兩邊的 cargo 都由 rustup 讀 `rust-toolchain.toml` 決定版本，`tools/repo-check` 保證它與 `.prototools` 的 `rust` 相同，所以 CI 與本機用的是同一組版本。

### 授權白名單

`deny.toml` 只允許寬鬆授權：0BSD、Apache-2.0（含 Apache-2.0 WITH LLVM-exception）、BSD-2-Clause、BSD-3-Clause、CC0-1.0、CDLA-Permissive-2.0、ISC、MIT、MIT-0、Unicode-3.0、Zlib。其他授權一律失敗，包括 Business Source License（SPDX `BUSL-1.1`）、SSPL、Elastic License、Commons Clause 與 GPL、LGPL、AGPL。MPL-2.0 等檔案層級的 copyleft 授權還沒決定，遇到時再評估。要加入新授權，先確認它允許商業轉售，再把授權與理由寫進本節與 `deny.toml`。SPDX 的 `BSL-1.0` 是 Boost 授權，不是 Business Source License。

### fork 來的 PR

ci.yml 不使用任何 secret，每個 job 的 GITHUB_TOKEN 只有 `contents: read`，checkout 也不把 token 留在 `.git/config`（`persist-credentials: false`）。fork 來的 PR 以 `pull_request` 事件執行，GitHub 不給 secret、token 唯讀；`workflow-policy` 擋下 `pull_request_target` 與 `workflow_run`。私有庫 Actions 設定中的「Send write tokens to workflows from fork pull requests」與「Send secrets to workflows from fork pull requests」保持關閉（GitHub 預設值）。

## nightly.yml

每晚 18:17 UTC（台北 02:17）執行，也可在 main 上手動執行（`workflow_dispatch`，選項 `simulate_failure` 會讓每份 values 在安裝後故意失敗，用來驗證通知）。動到 `deploy/chart/`、`deploy/istio/`、`scripts/nightly/`、`.prototools`、`.proto/plugins/`、`setup-proto` 或 `nightly.yml` 本身的 PR 也會跑，chart 的改動在合併前就先真的部署一次；這些 PR 的執行不寫任何留言（PR 的 workflow 檔由 PR 自己決定，不給它寫入權限），結果看 check 狀態、job summary 與 `nightly-report` artifact。

### 做什麼

`smoke` job 在一台 arm64 的 ubuntu runner（`ubuntu-24.04-arm`）上起一個 k3d 叢集，節點是 arm64，與出貨的架構相同，不覆寫 chart 的 nodeSelector，裝好平台元件，兩份 values（`values-saas.yaml`、`values-onprem.yaml`，各加上 `tests/values/` 的實例覆寫檔與 `smoke.yaml`）依序測試，各用自己的命名空間。k3s 映像是 `rancher/k3s:v1.36.5-k3s1`（SaaS 主機與 lab 的版本），以 multi-arch index 的 digest 釘選，不開 Traefik、ServiceLB 與 metrics-server。

平台元件（`scripts/nightly/platform.ts`，照 `deploy/istio/README.md` 的順序，與 SaaS 主機在裝 chart 前做的事相同）：

1. Gateway API 的 standard CRD：從 release 下載 `standard-install.yaml`，SHA-256 與 workflow 裡釘的值相符才套用（`GATEWAY_API_VERSION`、`GATEWAY_CRDS_SHA256`，要一起改）。
2. Istio ambient：官方 repo 的 `base`、`istiod`、`cni`、`ztunnel` 四個 chart，版本 `ISTIO_VERSION`，用 `deploy/istio/values-k3s.yaml`。istiod 另加一份由 `deploy/istio/istiod-values.yaml` 產生的覆寫：同一個 ext_authz provider（fail closed、0.5 秒逾時、503）為每個 e2e 命名空間各複製一份，名稱 `trustiot-e2e-<shape>`，所以 provider 的設定永遠是 repo 裡那份。
3. 建出 e2e 的身分服務替身映像（`deploy/chart/tests/e2e/identity-double`），匯入 k3d 節點。

每份 values 依序做：

1. 命名空間以 Pod Security `restricted`（enforce、warn、audit）建立：chart 要能在最嚴格的等級下安裝，也是 OpenShift 的前提。
2. 為 chart 渲染出來的 pod 讀取、但 chart 自己不建立的 Secret 建立假值，並為 Gateway listener 引用的 TLS Secret 建立自簽憑證（Secret 怎麼進叢集由 Laysi/trustiot-v2#72 決定；這裡驗證的是接線，不是帳密）。
3. `helm upgrade --install --wait --wait-for-jobs --timeout 5m`。
4. 讀 `helm get manifest`，每個 Deployment、StatefulSet、DaemonSet 都要 rollout 完成，每個 Job 都要完成。release 一個 workload 都沒有就失敗（`smoke.yaml` 一定會加一個，沒有代表覆寫檔壞了）。
5. 每個 Gateway 的 `Programmed` 條件為真（Istio 在 helm 結束後才產生入口 pod）。
6. 沒有 pod 處於 CrashLoopBackOff、ImagePullBackOff、設定錯誤或未就緒；重啟次數記在報告裡。
7. 每個 workload 的 HTTP 探針（startup、liveness、readiness），凡是有 Service 對外開那個埠的，都經 API server 的 Service proxy 呼叫一次（`/healthz`、`/readyz` 等），不需要 port-forward。
8. `helm uninstall` 之後，release 的每個物件都不存在。
9. 入口 e2e 測試（`deploy/chart/tests/e2e/ingress.e2e.ts`，Laysi/trustiot-v2#62）：在 `nightly-e2e-<shape>` 命名空間以對應的型態跑一次，驗證 cookie 驗證、例外路徑、路徑繞過與身分服務停機時的行為；這也是 #62 要求的 k3d 測試，每晚執行。請求經 `kubectl port-forward` 直接進入口 pod，所以不涵蓋入口的 NetworkPolicy（那一項只在 lab 以 `E2E_SSH_JUMP` 驗證）。

每份 values 任何一項失敗就停止該份並收集診斷（物件、事件、`describe`、pod 日誌；不含 Secret 內容，輸出可能含 Secret 的指令不寫進日誌）。

檢查 3 到 8 全部從 release 自己的 manifest 推導（`scripts/nightly/manifests.ts`），不寫死元件；之後加入的元件（Postgres、ClickHouse、broker 等）會自動被納入 workload 就緒與探針檢查。元件需要的前置（例如 operator）由加入該元件的 issue 在 `scripts/nightly/platform.ts` 補上；各里程碑完成條件的測試也由各自的 issue 加進來。

在 TrustIoT 服務映像出來以前，`smoke.yaml` 的替身服務（`traefik/whoami`，以 digest 釘選）讓通用服務樣板在真叢集上被驗證。

### 本機執行

nightly 只在 CI 跑：本 repo 規定工作站不跑 Kubernetes。要在有 docker 與 openssl 的機器上重現，`proto install` 之後，把 workflow 裡的 `K3S_IMAGE`、`GATEWAY_API_VERSION`、`GATEWAY_CRDS_SHA256`、`ISTIO_VERSION` 設成環境變數：

```sh
NIGHTLY_OUT=/tmp/nightly-out bun scripts/nightly/run.ts
```

單元測試（不需要叢集，只需要 helm）：`bun test scripts/nightly`。

### 通知

通知只用 workflow 自己的 `GITHUB_TOKEN`，沒有外部服務與額外 secret。`notify` job 只在排程執行與 main 上的手動執行跑（執行的是 main 上審過的程式），權限是 `issues: write` 與 `actions: read`：

- smoke job 失敗、逾時或被取消：開一張標籤為 `nightly-failure`、`area:ci`、`priority:P1`、類型 Bug 的 issue；已經有開著的就在上面留言，所以連續失敗一週是一張 issue 加幾則留言。內文是報告與診斷；沒有報告時（例如叢集建不起來）寫明哪個步驟失敗。
- 隔天通過：留言並關閉那張 issue。
- GitHub 的 `GITHUB_TOKEN` 不能設定專案看板的欄位，所以這張 issue 不會自動進看板：看到 `nightly-failure` 標籤的 issue 時，由人或 agent 依流程加入看板並分流。

### 失敗時怎麼辦

先看 issue 內文的報告：它列出哪份 values 的哪一項失敗與診斷。要重現就到 Actions 頁手動執行 nightly（選 `simulate_failure` 只用來驗證通知，不用來除錯）。常見原因：Docker Hub 對匿名拉取的流量限制（`traefik/whoami`、k3s 映像與 e2e 替身的基底映像都在 Docker Hub，建立叢集與建置映像已重試）、chart 的改動讓某份 values 裝不起來或讓入口的行為改變、Istio、Gateway API、k3s 或 k3d 版本更新。GitHub 在 repo 60 天沒有活動時會停掉排程 workflow，這個 repo 有活動所以不會發生，但若看到排程停了，到 Actions 頁重新啟用。

### 成本

一次執行一個 job（加上約 1 分鐘的 `notify`），兩份 values 共用一個叢集。實測的分鐘數見 `docs/test-reports/2026-10-10-69-nightly-workflow.md`。

## Renovate

設定在 `renovate.json5`：

| 範圍 | 做法 |
| --- | --- |
| Rust crate | Renovate 的 cargo manager 更新 `Cargo.toml` 與 `Cargo.lock`；版本固定為精確值（`rangeStrategy: pin`），minor 與 patch 每週合成一個 PR |
| Bun 套件 | `scripts/` 的 `package.json` 與 `bun.lock`，每週一個 PR |
| GitHub Actions | `helpers:pinGitHubActionDigestsToSemver`：每個 `uses:` 維持 40 字元 commit SHA，行尾註解維持版本 |
| Istio 與其他寫在 workflow env 的版本 | env 上方寫 `# renovate:` 註解（資料來源與套件名稱）即由自訂規則更新；Istio 停在 1.30 minor。Gateway API 的版本與雜湊要一起改，不由 Renovate 更新 |
| 容器映像 | `pinDigests: true`；原始碼、測試、workflow 與 values 裡寫成 `名稱:tag@sha256:…` 的映像由自訂規則讀取，更新時 tag 與 digest 一起換；chart 的 `repository`／`tag`／`digest` 分三行寫法也有規則；`rancher/k3s` 與 kubectl 停在 1.36 minor，換 minor 是刻意的升級 |
| `.prototools` | Renovate 的 proto manager 認得 `bun` 與 `rust`；有 schema 外掛的工具（cargo-deny、gitleaks、helm、k3d、kubeconform、kubectl）在 pin 上方寫 `# renovate:` 註解，由自訂規則更新。新增工具時照樣寫註解，`bun test scripts/nightly` 會檢查 |
| Rust 工具鏈 | `.prototools` 的 `rust` 與 `rust-toolchain.toml` 的 `channel` 合成一個 PR，兩處同時改（`repo-check` 要求兩者相同） |

PR 標題是 Angular 格式（`chore(deps): …`），標籤 `area:ci` 與 `dependencies`；同時開著的 PR 最多 5 個、每小時最多 2 個，週一 06:00 前（台北時間）才開，資安修補不受排程限制。

不由 Renovate 更新的項目（要手動改）：

- nightly 的 `GATEWAY_API_VERSION` 與 `GATEWAY_CRDS_SHA256`：同上，版本與雜湊要一起換。
- `.github/actions/setup-proto/action.yml` 的 `PROTO_VERSION` 與 `PROTO_SHA256_X64`、`PROTO_SHA256_ARM64`：三個值必須一起換，Renovate 只能改版本號。
- `scripts/lib/stack.ts` 的開發環境映像：只有 digest，tag 寫在註解，Renovate 讀不到。把 tag 寫進字串（`名稱:tag@sha256:…`）就會被納入。

### 需要人在 GitHub 組織裝 Renovate

設定檔本身不會執行，要有 Renovate 跑它：組織管理員安裝 Mend 的 Renovate GitHub App，授權給 `Laysi/trustiot-v2`。App 會先開一個 onboarding PR 或直接讀 `renovate.json5`；之後 Dependency Dashboard issue 列出所有待辦。這是管理員動作，agent 不做。App 的代價是第三方存取原始碼，私有庫要確認組織同意；替代做法是用 `renovatebot/github-action` 在我們自己的 workflow 跑，但那需要一個有寫入權限的 token（GitHub App 或 PAT）存成 secret，目前原則是 GitHub 不持有這類憑證，所以優先用 App。

設定的驗證：`renovate-config-validator` 通過；以 `renovate --platform=local --dry-run=extract` 對這個 repo 抽取依賴（結果在測試報告）。
