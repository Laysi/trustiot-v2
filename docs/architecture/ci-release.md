# CI 與發佈

GitHub Actions，三個 workflow：

1. `ci.yml`：每個 PR（不論目標分支，整合分支也一樣）與推到 main 都跑，release.yml 開的 deploy PR 則由 release.yml 以 `workflow_dispatch` 觸發（見下方「SaaS 以 GitOps 部署」），全綠才能合併（目前由流程把關，見下方「必要檢查與強制方式」）。Rust 跑 fmt、clippy、測試與 sqlx 離線檢查，並以 cargo-deny 檢查授權與漏洞公告（地端產品不能混入 BSL 等不能轉售的授權）；整合測試以 service container 起含 AGE 的 Postgres、ClickHouse 與 Mosquitto，跑完全部 migration 後跑跨租戶負向測試（RLS、row policy、AGE 角色）；閘道器程式交叉編譯成 aarch64，agent 用 testkit 的假 Modbus 端點跑故障注入測試；前端跑 Bun 安裝、型別檢查、lint、單元測試與建置；重新產生 `api/openapi.json` 與前端 client，有差異就失敗；helm lint，兩份 values 各自 helm template 後以 kubeconform 驗證（`bun test deploy/chart` 一次跑完，也檢查 chart 自己的安全規則，見 `deploy/chart/README.md`）；SaaS 主機安裝程式的型別檢查、測試與建置（`deploy/host`）；gitleaks 掃描祕密。各項由哪個 job 負責、哪些還沒加入，見下方「ci.yml 的 job」。Codex 審查設為 PR 的必要檢查，是伺服器端的關卡；本機 pre-commit 只是提早攔下。2026-10-11 起（Laysi/trustiot-v2#309）每個 job 都在 arm64 runner 上執行，`rust`、`cargo-deny`、`helm`、`host` 依變更的路徑決定是否執行，見下方「依變更路徑決定 job」。
2. `release.yml`：打 tag 時跑。容器映像只出 arm64：Rust 先交叉編譯再放進映像，推到 GHCR，以 cosign 簽章並附 SBOM；SaaS 主機與展示機（Mac M1）都要是 arm64，地端客戶若用 x86 主機，到時再加 amd64。Helm chart 打包推到 GHCR。閘道器 agent 與 updater 的 aarch64 執行檔附建置來源證明，由 CI 以 OTA 簽章金鑰簽章：金鑰存在受保護的 GitHub environment，只有 main 上的 tag 能用，每次簽章都要人工核准。另外產出離線安裝包（MS6）；release notes 沿用 `Laysi/release-notes-action`。
3. `nightly.yml`：每晚用 k3d 部署整套 chart（兩份 values 各一次），裝好 Istio ambient 後跑冒煙測試與入口 e2e 測試，失敗時開 issue 通知；PR 只在帶 `smoke` label 時才部署；依賴更新交給 Renovate。做什麼、怎麼通知與失敗時怎麼辦見下方「nightly.yml」與「Renovate」。

SaaS 以 GitOps 部署：叢集內的 Flux 從 GHCR 拉映像與 chart，設定放在 `deploy/flux/`；發版就是合併一個改 chart 版本號的 PR（由 release 流程自動開）。GitHub 不持有任何部署憑證，主機也不必對外開放，叢集只存一把唯讀的 GHCR 拉取 token；Flux 控制器的記憶體算進主機規格（MS3 重測）。地端不裝 Flux，由客戶以 Helm 安裝或用離線安裝包。

2026-10-11 實作（Laysi/trustiot-v2#70，操作與建立叢集的步驟見 [deploy/flux/README.md](../../deploy/flux/README.md)）：

- Flux 只裝 source、kustomize、helm 三個控制器（`.prototools` 釘 Flux 2.9.6，控制器映像以 cosign 驗過 fluxcd 官方簽章的 digest 釘選）。叢集以一把唯讀的 deploy key 經 SSH 每分鐘讀一次 main，只取 `deploy/flux/`；叢集沒有能寫 GitHub 的憑證，GitHub 也沒有叢集的憑證。
- chart 以 OCIRepository 拉取，tag 與 digest 都寫在 `deploy/flux/clusters/saas/trustiot/chart.yaml`（digest 優先），只接受 release.yml 在**該版本的 tag** 上的 keyless 簽章（issuer 與 subject 都是完全比對，比下方「簽章與驗證」的 `v.*` 更窄）。這個檔案由 `scripts/release/flux.ts` 產生，`bun test deploy/flux` 要求它與產生的內容逐字相同，所以驗簽不會被手改拿掉或放寬。
- 發版：release.yml 的 `deploy-pr` job 在 verify 通過後，以 GitHub API 在 main 上做一個 commit（改 `chart.yaml` 與部署中的 `values-saas.yaml` 副本），推到 `deploy/trustiot-<版本>` 並開 PR；候選版開成 draft，版本不比 main 上的新時不開。合併後 Flux 在 1 分鐘內開始升級，Helm 升級失敗時自動 rollback；revert 該 PR 即回到前一版。2026-10-11 在實驗機以一次性的 git 伺服器、registry 與簽章金鑰實測（[測試報告](../test-reports/2026-10-11-70-flux-gitops.md)）。
- deploy PR 上的 CI：以 `GITHUB_TOKEN` 開的 PR 不觸發 workflow，`workflow_dispatch` 是唯一的例外，所以 `deploy-pr` job 以 `actions: write` 在該分支執行 ci.yml（ci.yml 因此加了 `workflow_dispatch` 觸發），結果掛在分支的 head commit，PR 看得到；dispatch 失敗時由人關閉再重開 PR。沒有改用 GitHub App 或 PAT 開 PR：那要在 GitHub 存一把能寫 repo 的長期憑證。
- 前提：repo 設定「Allow GitHub Actions to create and approve pull requests」要打開，否則 `deploy-pr` 推好分支後失敗並附上 compare 連結，由人開 PR。這與下方「GitHub 的 Actions 設定」目前的 `false` 相反，由人決定（見該節）。

### SaaS 的祕密怎麼進叢集

2026-10-10 決定（Laysi/trustiot-v2#72）：**SOPS 加 age，由 Flux 的 kustomize-controller 解密**。密鑰的規則與清冊見 [密鑰與權限](secrets.md)，輪替步驟見 [密鑰輪替 runbook](../runbooks/secrets.md)。

做法：

- 每個 Secret 寫成一般的 Kubernetes Secret YAML，放在 `deploy/flux/`，檔名以 `.sops.yaml` 結尾。只加密 `data` 與 `stringData` 的值（`encrypted_regex: ^(data|stringData)$`），Secret 名稱與 key 保持明文，PR 看得出改了哪個 Secret 的哪個 key，看不到值。
- repo 根目錄的 `.sops.yaml` 列出收件者：SaaS 叢集的 age 公鑰，加上每位負責維運的人的 age 公鑰。
- Flux 的 Kustomization 設 `decryption.provider: sops`，引用 `flux-system` namespace 的 Secret `sops-age`（叢集的 age 私鑰），在記憶體中解密後套用。CI 不解密，GitHub 上只有密文。
- CI 的 `check-secret-handling.ts` 擋下 repo 內任何沒有加密的 Secret（見下方 `workflow-policy`）。

理由：

1. **不多跑元件**：解密是 Flux 本來就在跑的 kustomize-controller 的功能。sealed-secrets 與 External Secrets Operator 都要多一個常駐控制器，單節點主機（決策 5）的記憶體每一份都要算。
2. **從零重建可以重現**：版控裡有完整的期望狀態（含加密的 Secret），新主機放回叢集的 age 私鑰後，Flux 就能把整套裝回來（重建演練，Laysi/trustiot-v2#187）。sealed-secrets 的密文綁定叢集控制器自己的金鑰，重建時要先還原那把金鑰，否則每個 Secret 都要重新封裝；它也預設每 30 天換新封裝金鑰，備份要跟著更新。
3. **人能檢查與輪替，不必碰叢集**：維運人員是收件者，以自己的 age 私鑰 `sops edit` 就能確認內容與換值，走一般的 PR 審查。sealed-secrets 的密文只有叢集解得開，人無法確認裡面是什麼。
4. **不依賴雲端帳號**：age 不需要 KMS（決策 1 去 AWS）。External Secrets Operator 需要外部的祕密庫：Vault 要自己維運，1Password、Infisical 等是新的供應商與費用；叢集還是要存一把讀祕密庫的憑證，問題只是往外移一層。
5. **GitHub 仍不持有部署憑證**：CI 從頭到尾不需要解密金鑰。

代價與對策：

- 叢集的 age 私鑰成為新的信任根：只存在叢集的 Secret 與離線備份，不進版控；離線備份與根 CA 私鑰一起保管（密鑰清冊）。
- 收件者能解密全部 SOPS 檔案：收件者只限負責維運的人；有人離開時，除了移除收件者，還要輪替他讀得到的每一個值（runbook）。
- 密文永久留在版控歷史：叢集私鑰或任一收件者的私鑰外洩，歷史上的每個值都解得開，所以私鑰外洩時要輪替所有值，不只換金鑰。
- gitleaks 若把 `ENC[...]` 誤判為祕密，以 gitleaks 設定排除 SOPS 密文的格式，不排除整個檔案。

需要人提供的東西（Laysi/trustiot-v2#70 建立 `deploy/flux/` 之前）：

1. 決定哪幾位維運人員是收件者，建議至少兩位，避免只有一個人解得開。每位以 `age-keygen` 在自己的裝置產生金鑰對，只交出公鑰。
2. 建立 SaaS 叢集時，由維運人員產生叢集的 age 金鑰對、直接建立 `sops-age` Secret（不經版控），並把私鑰的離線備份交給兩位保管人。
3. GHCR 拉取 token 用的專用機器帳號（GHCR 只接受 classic personal access token，見密鑰清冊）。

Laysi/trustiot-v2#70 建立了 `.sops.yaml`（規則：`deploy/flux/clusters/saas/` 下的 `*.sops.yaml`，只加密 `data` 與 `stringData`），收件者還是空的：上列第 1、2 項的公鑰由人提供後填入，在那之前 sops 拒絕加密，不會產生沒有人解得開的檔案。sops 鎖在 `.prototools`（schema 外掛驗 release 的 checksum）。age 的 release 只附 Sigsum 證明、沒有 checksum 檔，proto 的 schema 外掛驗證不了下載，所以改由 `.prototools` 釘選的 Go 從原始碼建置：`bun scripts/flux/age.ts age-keygen ...` 以 `go install filippo.io/age/cmd/...@<版本>` 取得 module，由 Go checksum database 驗證（版本在 `scripts/flux/age.ts` 的 `AGE_VERSION`）。地端不裝 Flux，Secret 由客戶以自己的方式建立，chart 只認 Secret 名稱與 key。

通用規則（[#158](https://github.com/Laysi/trustiot/issues/158)、[#120](https://github.com/Laysi/trustiot/issues/120)、[#122](https://github.com/Laysi/trustiot/issues/122)）：每個 workflow 明列最小權限，每個 job 都寫出自己的 `permissions`，workflow 層級設 `permissions: {}`；第三方 action（含 GitHub 官方的 `actions/*` 與 reusable workflow）一律釘 40 字元的 commit SHA，行尾註解寫版本；容器映像（`docker://` action、job 與 service container）釘 digest；需要雲端憑證時用 OIDC，不放長期金鑰；發佈走 GitHub environment 加人工核准（私有 repo 在目前的 GitHub 方案設不了核准，2026-10-10 決定暫時接受，改以補償措施把關，見下方「發佈核准」）；fork 來的 PR 拿不到任何 secret，所以不使用 `pull_request_target` 與 `workflow_run` 觸發。權限、釘選與觸發條件三項由 CI 的 `workflow-policy` job（`scripts/ci/check-workflows.ts`）檢查每個 workflow 與 `.github/actions/` 下的 action；workflow 本身是否為 GitHub 能解析的語法，由同一個 job 以 actionlint 檢查（見下方「Workflow 語法檢查」）。本地 action（`uses: ./...`）不必釘 SHA，因為它隨使用它的變更一起審查，但只能放在 `.github/actions/` 下，本地 reusable workflow 只能直接放在 `.github/workflows/`，才在檢查範圍內；symbolic link 依實際指向的檔案檢查。

## ci.yml 的 job

2026-10-10 上線（Laysi/trustiot-v2#67）。同一個 PR 有新的推送時取消舊的執行；推到 main 的每個 commit 各自一個 concurrency group，一律跑完，每個 main commit 都有結果（共用一個 group 時，GitHub 會以較新的執行取代還在排隊的執行，即使沒有設 `cancel-in-progress`）。本機重跑的指令列在 `CLAUDE.md` 的「Build, lint, and test」。

2026-10-11 起（Laysi/trustiot-v2#309）每個 job 都在 2-core 的 arm64 代管 runner（`ubuntu-24.04-arm`）上執行，理由與例外見下方「省 Actions 分鐘數」。`changes` job 先依變更的路徑決定 `rust`、`cargo-deny`、`helm`、`host` 要不要執行（見下方「依變更路徑決定 job」）；`gitleaks` 與 `workflow-policy` 每次都執行，不等 `changes`。

| job（檢查名稱） | 內容 |
| --- | --- |
| `changes` | `bun scripts/ci/changes.ts`：比較這次要測的 commit 與一個已測過的 commit，輸出 `rust`、`cargo-deny`、`helm`、`host` 各自是否執行（`true` 或 `false`），並在 job summary 列出理由、比較的 commit 與變更的檔案。推到 main 時另以 `actions: read` 讀 main 上近期的 ci 結果；PR 執行的是 PR 自己的腳本，不給它 token。規則見下方「依變更路徑決定 job」 |
| `rust` | `cargo fmt --all --check`；`cargo clippy --workspace --all-targets --locked -- -D warnings`；`cargo test --workspace --locked`（含 `tools/repo-check` 的 repo 規則、`crates/service-kit` 以 `cargo tree` 檢查關閉 `contract` feature 時不依賴 contract，以及之後各 crate 的測試，例如 agent 以 testkit 假 Modbus 端點跑的故障注入測試）。`--workspace` 只建置預設 feature，所以 service-kit 關閉預設 feature 另跑一次 clippy 與測試（`--package service-kit --no-default-features`）。service-kit 的容器測試（以 PID 1 執行的範例服務、advisory lock 用的 Postgres）與 testkit 跨租戶負向測試框架的自我測試（帶 TLS 的 Postgres）用 runner 上的 podman 或 docker 起容器；GitHub 設定 `CI=true`，找不到可用的容器執行環境時這些測試失敗而不是略過，所以測試步驟通過就表示它們真的跑過。全程設 `SQLX_OFFLINE=true`：sqlx 的查詢巨集只讀進版控的 `.sqlx/`，查詢資料沒更新就編譯失敗。接著執行 `cargo run --locked --package openapi` 重新產生 `api/openapi.json`（`tools/openapi`，各服務由程式碼產生的 OpenAPI 規格加上 M1 錯誤 schema 合併而成），有任何差異就印出差異並失敗，所以改了 API 型別卻沒重新產生規格的 PR 過不了；同一項比對也在 `cargo test --workspace` 裡（`tools/openapi/tests/openapi.rs`）。最後把 `crates/protocol` 與 `edge/` 下的每個成員以 `aarch64-unknown-linux-gnu` 建置；還沒有這些 crate 時只留下說明，protocol crate（Laysi/trustiot-v2#50）併入後自動涵蓋。runner 是 arm64 時主機本身就是這個 target，直接原生建置（沿用前面步驟的編譯快取）；在 x64 主機上才交叉編譯，函式庫建置不需要交叉連結器，第一個閘道器執行檔加入時一併加上。前一項失敗時後面幾項照跑，一次回報全部結果 |
| `cargo-deny` | `cargo deny --locked check`，規則在根目錄的 `deny.toml`：授權白名單（見下方「授權白名單」）；RustSec 的漏洞、unmaintained 與 unsound 公告一律失敗，要忽略某則公告必須在 `deny.toml` 寫明理由與到期日；Cargo.lock 裡有 yanked 版本就失敗；相依只能來自 crates.io |
| `helm` | `bun test deploy/chart`：`helm lint --strict`；`values-saas.yaml` 與 `values-onprem.yaml` 各自 helm template 後以 `kubeconform -strict` 驗證（Kubernetes schema 由 kubeconform 下載）；再加上 chart 自己的規則與 README 欄位檢查，見 `deploy/chart/README.md`；入口 e2e 的通道復原邏輯（`deploy/chart/tests/e2e/tunnel-recovery.ts`）有不需要叢集的單元測試，也在這一步跑。接著 `bun test deploy/flux`（Laysi/trustiot-v2#70）：SaaS 的 GitOps 樹以 `kubectl kustomize` 建置、以 kubeconform 對釘選版本的 Flux CRD 驗證，Flux 的 manifest 與釘選的 flux CLI 輸出相同，chart 來源的驗簽設定、實例 values 對 chart schema、`.sops.yaml`，見 `deploy/flux/README.md` 的「檢查」 |
| `host` | SaaS 主機安裝程式（`deploy/host`，Laysi/trustiot-v2#64）：型別檢查；`bun test` 以模擬主機跑每個安裝步驟（乾淨主機一次到位、重跑不改任何東西、拒絕時主機不變、token 不出現在指令列與輸出），並以 runner 的 `systemd-analyze security --offline` 確認 cloudflared unit 的評分不高於 2.0、以 gpg 確認釘選的 Cloudflare 套件簽章金鑰指紋；`CI=true` 時缺 systemd-analyze 或 gpg 會失敗而不是略過；最後編譯 x86_64 與 aarch64 兩個執行檔 |
| `gitleaks` | 掃描 HEAD 能走到的每個 commit（完整歷史）；merge commit 也以對第一個 parent 的差異掃描，只在合併時加入的內容也查得到。只掃 HEAD，不掃 checkout 時一起抓下來的其他分支。發現時在該檔案與行號標註，附 fingerprint；報告遮蔽祕密值，不會印進紀錄 |
| `workflow-policy` | `scripts/ci/check-workflows.ts` 檢查 `.github/workflows/` 的 workflow 與 `.github/actions/` 的 action，規則即上方通用規則中的權限、釘選（含本地 action 的位置）與觸發條件三項；`scripts/ci/lint-workflows.ts` 以 actionlint 檢查每個 workflow 的語法、expression 與各 key 可用的 context（2026-10-10，Laysi/trustiot-v2#251；本機 `bun scripts/ci/lint-workflows.ts`）；`scripts/ci/check-secret-handling.ts` 檢查密鑰的處理：repo 內每個 Dockerfile 與 Containerfile 不 COPY 金鑰類檔案、`ARG` 與 `ENV` 不放密鑰、只有旁邊的 ignore 檔以 `*` 開頭時才能複製整個 build context，`--build-arg` 與 `build-args` 不傳密鑰，映像建置一律經 `containerBuildArgs` 檢查實際的 context，`${{ secrets.* }}` 只出現在 step 的 `env` 與 `with`（不寫進 `run`，也不放在 workflow 與 job 層級的 `env`），repo 內的 Kubernetes Secret 一律以 SOPS 加密（規則見 [密鑰與權限](secrets.md)）；再跑 `scripts/` 與 `deploy/chart/tests/` 全部 TypeScript 的型別檢查（2026-10-11，Laysi/trustiot-v2#261）與 `scripts/` 的測試（含本地開發環境 Laysi/trustiot-v2#71 的單元測試；它的端對端測試要以 podman 起整套環境並編譯服務，不在 CI 執行；也含 `changes` 的路徑規則，以及 ci.yml 與 nightly.yml 決定執行與否的條件式，見下方「依變更路徑決定 job」） |

之後由其他 issue 加進 ci.yml 的檢查（加入時同步更新本表與必要檢查清單）：

| 檢查 | 加入的 issue |
| --- | --- |
| 整合測試：service container 起含 AGE 的 Postgres、ClickHouse、Mosquitto，在全新資料庫跑完全部 migration 後跑跨租戶負向測試（RLS、row policy、AGE 角色）；在同一個資料庫上跑 `cargo sqlx prepare --workspace --check`，確認 `.sqlx/` 與 schema 一致 | Laysi/trustiot-v2#60（負向測試框架為 Laysi/trustiot-v2#56） |
| 擋下把外部輸入拼進 SQL 字串的靜態檢查 | Laysi/trustiot-v2#53 |
| 前端：Bun 安裝、型別檢查、lint、單元測試、建置；重新產生 API client，有差異就失敗 | Laysi/trustiot-v2#123 |

加入的 job 若只跟部分路徑有關，就在 `scripts/ci/changes.ts` 加它的規則與測試，並由 `changes` 輸出；否則每次都執行。

### 依變更路徑決定 job

2026-10-11 決定（Laysi/trustiot-v2#309）。起因：2026-10-10～11 兩天全組織用掉超過 1,000 個 Actions 分鐘（Free 方案每月 2,000 分鐘，組織內所有 repo 共用），其中多數是只改文件、chart 或腳本的 PR 也照跑約 7 分鐘的 `rust` job。

**比較哪兩個 commit**（`scripts/ci/changes.ts`）：

- PR：GitHub checkout 的 merge commit 對它的第一個 parent（合併當下 base 分支的最新 commit），所以列出的正是合併後會改變的檔案；base 分支之後的變動不算進來。
- 推到 main：推送的 commit 對「main 上最近一個 ci 通過的 commit」。沿著 first-parent 往回找（最多 50 個），每個 commit 看它最新一次 ci.yml 的 push 執行：還在跑、失敗、被取消或沒有執行（一次推多個 commit 時中間的 commit）的都跳過，它們的變動因此也算進這次的比較。所以 main 上「略過」一定代表「與一個整次 ci 都通過的 commit 相比，這個 job 讀的檔案沒變」；連續合併時上一個 commit 的 ci 還在跑也不必全部重跑；紅燈的 main 在修好以前一直是紅燈（造成紅燈的變動會一直在比較範圍內）。決策 30 的「每個 main commit 都有 CI 結果」仍然成立：略過的 job 狀態是 skipped，整次執行的結論仍是 success，release 的 plan job 照常以它判斷（見「發佈核准」）。讀不到執行紀錄（`changes` 以 `actions: read` 呼叫 GitHub API）或 50 個內都沒有通過的 commit 時，全部 job 都執行。
- 其他事件、git 比較失敗：全部 job 都執行。
- 比較時加 `--no-renames`，搬移檔案時新舊兩個路徑都算，從 `crates/` 搬出去的檔案也會讓 `rust` 執行。

**規則**（寧可多跑，不可漏跑）：

| job | 何時執行 |
| --- | --- |
| 全部 | `.github/workflows/ci.yml`、`.github/actions/` 下的任何檔案、`scripts/ci/changes.ts` 本身與它直接或間接 import 的 repo 內檔案（目前是 `scripts/release/lib.ts`，推到 main 時由它挑出比較的基準 commit）、`.prototools`、`.proto/` 下的任何檔案（`rust`、`cargo-deny`、`bun`、`helm`、`kubeconform` 的版本都從這裡來）有變動 |
| `rust` | 預設執行，只有在每個變動的檔案都屬於下列「`rust` 不讀的路徑」時才略過：`docs/`、`deploy/`（`deploy/chart/values.yaml` 除外，`crates/service-kit/tests/chart_contract.rs` 讀它）、`scripts/`、`frontend/`、`.github/`（上一列的 ci.yml 與本地 action 除外）、`.githooks/`，以及根目錄的 `README.md`、`renovate.json5`、`.gitleaksignore`、`.dockerignore`、`deny.toml`。不論在哪個目錄，`.rs`、`.sql` 與任何大小寫的 `AGENTS.md` 都會讓它執行（`tools/sql-check` 掃描整個 repo 的 Rust 檔與它們 include 的 SQL，`tools/repo-check` 不允許任何 AGENTS.md）。沒有列出的路徑（`crates/`、`services/`、`edge/`、`factory/`、`tools/`、`api/`、`Cargo.toml`、`Cargo.lock`、`rust-toolchain.toml`、`clippy.toml`、`.cargo/`、`CLAUDE.md`、`.gitignore`、`datasets/`、`evals/`，以及之後新增的任何目錄）一律執行 |
| `cargo-deny` | 任何 `Cargo.toml`、`Cargo.lock`、`deny.toml`、`rust-toolchain.toml`、`.cargo/` 下的檔案有變動 |
| `helm` | `deploy/chart/`、`deploy/istio/`、`deploy/flux/`、`scripts/flux/` 下的檔案，或 chart 測試讀的 `scripts/lib/stack.ts`、`services/ingest/tests/common/mod.rs`、`docs/architecture/deployment.md`，或 `bun test deploy/flux` 讀的 `.sops.yaml`、`scripts/release/flux.ts` 有變動（Laysi/trustiot-v2#70） |
| `host` | `deploy/host/` 下的檔案有變動（安裝程式的原始碼、測試、相依與建置都在這個目錄） |
| `gitleaks`、`workflow-policy` | 每次都執行 |

**怎麼確保規則跟得上**：`scripts/ci/changes.test.ts` 逐類列出路徑與預期結果，另有兩項守門測試：掃描 repo 內每個 `.rs` 檔的字串常值與 `include_str!`/`include_bytes!`，凡是指向 repo 內某個路徑的，都必須會讓 `rust` 執行；掃描 `deploy/chart/tests/` 與 `deploy/host/` 的 TypeScript，凡是讀到 repo 內其他檔案的，都必須會讓 `helm` 或 `host` 執行。所以新測試開始讀某份文件時，若沒有同時更新規則，`workflow-policy` 會失敗。另一項守門測試沿著 `changes.ts` 的相對 import 一路追下去，每個被 import 的檔案都必須列在 `DECISION_CODE`，也就是會讓全部 job 執行。`scripts/ci/workflow-conditions.test.ts` 以 GitHub 自己的 expression 引擎（[`@actions/expressions`](https://www.npmjs.com/package/@actions/expressions)，GitHub 的 actions/languageservices，MIT）計算 ci.yml 與 nightly.yml 的 `if` 與 concurrency：`changes` 回答 `false` 才略過，回答 `true`、空值或 `changes` 本身失敗時都執行，整次執行被取消時不執行。

**失敗時往執行的方向走**：每個依路徑的 job 寫成 `if: !cancelled() && needs.changes.outputs.<job> != 'false'`，`changes` 失敗、逾時或沒寫出結果時照樣執行；`changes.ts` 遇到任何問題也都回答「全部執行」，並在 job summary 寫明原因。

**為何自己寫腳本、不用第三方 action**（例如 `dorny/paths-filter`）：(1) `rust` 的規則是「全部檔案都在忽略清單裡才略過」，用 glob 設定要靠反向比對與量詞設定，不如程式清楚；(2) 規則與守門測試放在一起，由同一個 CI 測試，規則改錯會在 PR 上直接失敗；(3) 推到 main 時要讀 main 上近期的 ci 結果、找出最近一個通過的 commit，本來就要寫程式；(4) 不多一個要追 SHA、會拿到 token 的第三方 action。代價是 `changes` job 本身約 1 個計費分鐘（checkout 完整歷史、裝 Bun）。

**已知的取捨**：`cargo-deny` 的 RustSec 公告是隨時間增加的，略過時新公告要等下一次 Cargo 相關的變動才會被發現；Renovate 的 `vulnerabilityAlerts` 會為有公告的相依開 PR（前提是組織已裝好 Renovate，見下方「Renovate」）。是否加每日的 `cargo deny check advisories` 另案評估。

### 必要檢查與強制方式

必要檢查清單（ci.yml 的 job 名稱）：`changes`、`rust`、`cargo-deny`、`helm`、`host`、`gitleaks`、`workflow-policy`。依路徑略過的 job 狀態是 skipped，GitHub 把它當作通過（job 層的 `if` 略過時會回報狀態；workflow 層的 `paths` 篩選則根本不會建立 check，必要檢查會一直等待，所以 ci.yml 不用 workflow 層的 `paths`）。

2026-10-10 確認：本 repo 是私有庫，組織在 GitHub Free 方案，分支保護與 ruleset 都不能用（API 回 HTTP 403「Upgrade to GitHub Pro」），GitHub 不會擋下沒通過檢查的合併。方案升級前由流程把關：合併任何 PR 之前，確認上列檢查在該 PR 最新的 commit 上全部通過或略過（略過只能來自 `changes` 的判斷）。方案升級後，在 main 的 ruleset 把上列檢查設為必要檢查。

### 省 Actions 分鐘數

私有庫在 Free 方案的 Actions 分鐘數有上限，所以：同一個 PR 的新推送會取消舊的執行；每個 job 設 `timeout-minutes`；Rust 的編譯快取（`Swatinem/rust-cache`）只在 main 儲存，PR 只讀取；不用模擬器；同一項檢查只跑一次；`rust`、`cargo-deny`、`helm`、`host` 只在相關路徑變動時執行（見上方「依變更路徑決定 job」）；nightly 的 k3d 部署在 PR 上只在帶 `smoke` label 時執行（見下方「nightly.yml」）。

Runner：2026-10-11 依使用者指示（Laysi/trustiot-v2#309），ci.yml 與 nightly.yml 的每個 job 都用 2-core 的 arm64 代管 runner（`ubuntu-24.04-arm`），它每分鐘的價格比 x64 低；不夠用時再議。nightly 的 `smoke` job 原本就必須是 arm64：chart 預設的 nodeSelector 是 `kubernetes.io/arch: arm64`（決策 5），叢集架構要與實際出貨的一致。改用 arm64 前逐項確認過（2026-10-11，見 [測試報告](../test-reports/2026-10-11-309-ci-cost.md)）：setup-proto 的 proto 有 arm64 的 SHA-256；ci.yml 用到的 schema plugin（gitleaks、actionlint、cargo-deny、helm、kubeconform）都解析到存在的 linux arm64 release 資產，checksum 相符，執行檔能在 arm64 Linux 執行；Rust 測試啟動的容器映像（Postgres、ClickHouse、Mosquitto、distroless）都以 multi-arch index 的 digest 釘選，index 內有 linux/arm64；Rust 工具鏈由 runner 映像內建的 rustup 依 `rust-toolchain.toml` 安裝。arm64 runner 上 aarch64 是原生建置，不必交叉編譯。`release.yml` 仍在 x64，是否改用 arm64 另案評估（發佈次數少，且牽涉交叉編譯與可重現建置的設計）。目前沒有因為不能在 arm64 執行而留在 x64 的 ci.yml 或 nightly.yml job。

### 工具安裝

CI 以 `.github/actions/setup-proto` 安裝 proto：版本與兩種架構（x64、arm64）各自壓縮檔的 SHA-256 寫死在該 action，不執行會變動的線上安裝腳本；再以 proto 安裝 `.prototools` 釘選的工具。沒有內建 proto 外掛的工具（actionlint、cargo-deny、gitleaks、helm、k3d、kubeconform、kubectl、flux、sops，以及 release.yml 用的 zig、cargo-zigbuild、cargo-auditable、syft、cosign）在 `.proto/plugins/` 設定，下載官方 release 並以 release 附的 checksum 驗證（zig 以官方的 minisign 簽章驗證）。Go 用 proto 內建的外掛，只用來從原始碼建置 age（age 的 release 沒有 checksum 檔，見上方「SaaS 的祕密怎麼進叢集」）。Rust 是例外：proto 的 Rust 外掛本身只是呼叫 rustup，而 `proto install rust` 在 GitHub 代管 runner 上無法完成（2026-10-10 以暫時 PR 實測：job 超過 10 分鐘的上限仍未結束，同一個 action 裝 Bun、gitleaks 都正常；沒有 Actions 紀錄的讀取權限，原因待查），所以 setup-proto 在 CI 直接用 runner 映像內建的 rustup 執行不帶參數的 `rustup toolchain install`，依 `rust-toolchain.toml` 安裝釘選的版本、元件與 profile；裝完確認 rustup 選到的工具鏈名稱正好是 `<rust-toolchain.toml 的 channel>-<host triple>`，不符就在這一步失敗，不留到後面的 cargo 指令。本機照常由 proto 經 rustup 安裝。兩邊的 cargo 都由 rustup 讀 `rust-toolchain.toml` 決定版本，`tools/repo-check` 保證它與 `.prototools` 的 `rust` 相同，所以 CI 與本機用的是同一組版本。

### Workflow 語法檢查

2026-10-10（Laysi/trustiot-v2#251）加入。起因：Laysi/trustiot-v2#247 的 `nightly.yml` 在 `jobs.<id>.env` 用了 `${{ runner.temp }}`。`runner` context 不能用在 job 層的 `env`，GitHub 因此判定整個檔案無效，每次觸發都在解析階段失敗，畫面只有 "This run likely failed because of a workflow file issue"；排程 workflow 要到排程時間才會發現。`check-workflows.ts` 只把 YAML 當資料讀，看不出這類錯誤，所以另外以 [actionlint](https://github.com/rhysd/actionlint) 檢查，它知道每個 key 可用哪些 context 與函式。

- 本機與 CI 同一個指令：`bun scripts/ci/lint-workflows.ts [repository-root]`（先 `proto install actionlint`）。它找出 `.github/workflows/` 下的 `.yml` 與 `.yaml`，以 `proto bin actionlint` 取得 `.prototools` 釘選版本的執行檔，檢查並回報；退出碼 0 無錯誤、1 有錯誤、2 無法執行。測試在 `scripts/ci/lint-workflows.test.ts`：以暫時目錄裡的 workflow 證明 job 層 `env` 的 `runner.temp` 會失敗、同一個值放在 step 層則通過，並檢查 repo 現有的每個 workflow 都通過。
- 為何是獨立的腳本、不併進 `check-workflows.ts`：後者是純 YAML 規則檢查，單元測試不需任何外部執行檔；actionlint 是另一個執行檔，缺少時要有明確的錯誤訊息，並且在 CI 是獨立的步驟，失敗時一眼看出是哪一類問題。兩個步驟都在 `workflow-policy` job，前一步失敗時後一步照跑。
- 版本與來源：`.prototools` 釘 actionlint 1.7.12（2026-03-30 發佈），`.proto/plugins/actionlint.toml` 下載官方 release 的 `actionlint_<version>_<os>_<arch>.tar.gz`，以同一個 release 的 `actionlint_<version>_checksums.txt` 驗證 SHA-256；支援 linux 與 macOS 的 x64、arm64。
- composite action 的涵蓋範圍：actionlint 不把 `action.yml` 當獨立檔案檢查（拿它當 workflow 讀會得到一堆結構錯誤）。它檢查的是 workflow 呼叫本地 action 時給的 `with` 是否定義過、必填的是否漏給、讀的 output 是否存在。action 自己的 steps 只由 `check-workflows.ts` 的規則檢查。要讓這項檢查生效，actionlint 必須找到專案根目錄（`.git`），所以腳本在沒有 `.git` 的目錄會直接失敗，而不是無聲地略過。
- shellcheck 與 pyflakes：不啟用，腳本固定加 `-shellcheck= -pyflakes=`。決定與理由（2026-10-10）：(1) actionlint 在 PATH 上找得到 shellcheck 才會用它，GitHub 代管 runner 映像內建 shellcheck 而本機通常沒有，不明確關掉的話同一個 commit 在 CI 與本機會得到不同結果；(2) shellcheck 要啟用就得一樣經 proto 釘版本，但 shellcheck 的 release 只附壓縮檔、沒有 checksum 檔（`shellcheck-v0.11.0.sha256sum` 回 404），做不出本 repo 要求的「下載時驗證 checksum」的外掛；(3) 目前 workflow 的 `run` 區塊很短，且都以 `set -euo pipefail` 執行。代價是 `run` 區塊裡的 shell 問題（例如變數沒加引號）不會被檢查。重新評估的時機：shellcheck 開始附 checksum，或 `run` 區塊變多變長；屆時在 `.prototools` 釘 shellcheck、加外掛，並把 `ACTIONLINT_FLAGS` 的 `-shellcheck=` 拿掉。
- 偵測到問題時修 workflow 本身；不用 `-ignore` 或設定檔壓掉規則，除非是 actionlint 的誤判，並在該處寫明理由。

### 授權白名單

`deny.toml` 只允許寬鬆授權：0BSD、Apache-2.0（含 Apache-2.0 WITH LLVM-exception）、BSD-2-Clause、BSD-3-Clause、CC0-1.0、CDLA-Permissive-2.0、ISC、MIT、MIT-0、Unicode-3.0、Zlib。其他授權一律失敗，包括 Business Source License（SPDX `BUSL-1.1`）、SSPL、Elastic License、Commons Clause 與 GPL、LGPL、AGPL。MPL-2.0 等檔案層級的 copyleft 授權還沒決定，遇到時再評估。要加入新授權，先確認它允許商業轉售，再把授權與理由寫進本節與 `deny.toml`。SPDX 的 `BSL-1.0` 是 Boost 授權，不是 Business Source License。

### fork 來的 PR

ci.yml 不使用任何 secret，每個 job 的 GITHUB_TOKEN 只有 `contents: read`，checkout 也不把 token 留在 `.git/config`（`persist-credentials: false`）。fork 來的 PR 以 `pull_request` 事件執行，GitHub 不給 secret、token 唯讀；`workflow-policy` 擋下 `pull_request_target` 與 `workflow_run`。repo 層級的 Actions 設定另外保證這幾點，見下一節。

### GitHub 的 Actions 設定

2026-10-10 經使用者同意，以 GitHub API 設定並讀回確認（記錄於 Laysi/trustiot-v2#72）：

| 設定 | 值 | 效果 |
| --- | --- | --- |
| fork PR 的 workflow 送出 secret 與變數（`send_secrets_and_variables`） | `false` | 私有庫的 fork PR 拿不到任何 secret 與 Actions 變數 |
| fork PR 的 workflow 拿到寫入權限的 token（`send_write_tokens_to_workflows`） | `false` | fork PR 的 `GITHUB_TOKEN` 一律唯讀 |
| fork PR 的 workflow 要先核准（`require_approval_for_fork_pr_workflows`） | `true` | 有寫入權限的人核准後才會執行 |
| 預設的 `GITHUB_TOKEN` 權限（`default_workflow_permissions`） | `read` | 沒寫 `permissions` 的 job 也只有讀取權限；我們的 workflow 另外每個 job 都明列 |
| Actions 能否開立與核准 PR（`can_approve_pull_request_reviews`） | `false`（待決定，見下） | workflow 不能以 `GITHUB_TOKEN` 開 PR，也不能核准 PR、繞過人工審查 |
| action 必須釘完整的 commit SHA（`sha_pinning_required`） | `true` | GitHub 拒絕執行沒有釘 SHA 的 action，與 `workflow-policy` 的檢查互為兩層 |

重新確認的指令（需要 repo 的管理權限；每年與人員異動時各做一次，[密鑰輪替 runbook](../runbooks/secrets.md) 也列了這一項）：

```sh
# sha_pinning_required: true
gh api repos/Laysi/trustiot-v2/actions/permissions
# default_workflow_permissions: "read"、can_approve_pull_request_reviews: false
gh api repos/Laysi/trustiot-v2/actions/permissions/workflow
# send_write_tokens_to_workflows: false、send_secrets_and_variables: false、require_approval_for_fork_pr_workflows: true
gh api repos/Laysi/trustiot-v2/actions/permissions/fork-pr-workflows-private-repos
```

任何一項與上表不同，就表示設定被改過：先改回，再查組織的稽核紀錄找出原因。

`can_approve_pull_request_reviews` 同時控制「開 PR」與「核准 PR」。release.yml 的 `deploy-pr` job 要以 `GITHUB_TOKEN` 開 deploy PR（Laysi/trustiot-v2#70），設定為 `false` 時它只推分支、由人開 PR。改為 `true` 由人決定並記錄在 Laysi/trustiot-v2#70：在目前的 Free 方案，核准不是合併的條件（沒有分支保護），所以多出的「核准」能力不繞過任何強制關卡；有 `pull-requests: write` 的 job 只有 `deploy-pr`，它不執行建置程式碼。改了之後更新上表，升級到能用分支保護的方案時重新檢視（[deploy/flux/README.md](../../deploy/flux/README.md)「需要的 repo 設定」）。

## release.yml

2026-10-10 上線（Laysi/trustiot-v2#68）。推送 `vMAJOR.MINOR.PATCH`（正式版）或 `vMAJOR.MINOR.PATCH-rc.N`（候選版）tag 時執行，其他 tag 不觸發。流程由 `scripts/release/` 的 Bun 腳本執行，本機測試方式見 [scripts/README.md](../../scripts/README.md)。

### 發佈內容

- 映像：`deploy/images/` 下每個子目錄是一個映像，目錄名就是映像名，也是 chart 裡的服務名；發佈為 `ghcr.io/laysi/trustiot-v2/<名稱>:<版本>`，只有 linux/arm64 一個平台。新增映像只要加目錄，不必改 workflow。
  - Rust 映像：目錄只放 `image.toml`，以 `[rust]` 的 `package` 指定 Cargo 套件，套件有多個執行檔時加 `bin`。執行檔交叉編譯成 `aarch64-unknown-linux-musl` 的靜態執行檔，放進共用的 `deploy/images/rust-binary.Containerfile`：distroless static 基底（以 digest 釘選），以 UID 65532 執行。
  - 自帶 Containerfile 的映像（例如 Laysi/trustiot-v2#57 含 AGE 的 Postgres）：目錄放 `Containerfile`，目錄本身是 build context。release.yml 在 x86 的代管 runner 上建置，而且不用模擬器，所以 arm64 那一層不能有 `RUN`：要編譯的東西放在 `FROM --platform=$BUILDPLATFORM` 的建置階段交叉編譯，或取用上游已有的 arm64 映像。
- Helm chart：`ghcr.io/laysi/trustiot-v2/charts/trustiot:<版本>`。打包時 version 與 appVersion 都設成 tag 的版本，repo 裡的 `Chart.yaml` 不改；chart 預設的映像 tag 是 appVersion，所以預設拉同一版的映像（所有服務一起發版，決策 4）。
- GitHub Release：發版說明（`Laysi/release-notes-action`，從上一個版本的 tag 算起：正式版從上一個正式版，候選版從上一個任何 tag）、每個映像與 chart 的 digest、驗證指令、每個映像的 SBOM（`<名稱>-<版本>.spdx.json`），最後附上 verify job 的驗證結果。候選版標為 prerelease，不會成為 Latest。發版說明要用模型整理時，設定 repo 變數 `AI_MODEL`（與 `AI_BASE_URL`）及 secret `AI_API_KEY`；沒設定或呼叫失敗時，內容是上一版以來的 commit 主旨。
- `release-selftest`（`tools/release-selftest`）是流程自測用的映像，不是服務：一個連結 C 物件的小程式，執行時印出自己的架構，用來驗證交叉編譯（含 C 編譯器與連結器）、簽章、SBOM 與驗證整條路。它不在 chart 裡，也不部署。第一個服務映像加入後，連同 crate 一起刪除。

### tag 規則

- 正式版 tag 指向的 commit 必須在 main 上，而且 ci.yml 在 main 上為這個 commit 跑的最新一次 push 執行必須成功；plan job 以 `actions: read` 從 GitHub API 讀取，任一項不通過就停止。所以正式版要打在 main 的 ci 跑過的 commit（合併產生的那個 commit），ci 還在跑時，等它通過後重跑這次 release（Re-run all jobs）。候選版可以打在任何分支，不檢查 ci。
- 版本不可變：GHCR 上已有同版本而內容不同時，publish 失敗，要重發就打新的版本號。同一個 tag 重跑失敗的 publish job 時沿用同一次 build 的產物，digest 不變：已推送的不再推送，簽章與 SBOM attestation 重新附上。
- plan、build、publish 都確認 checkout 的 commit 就是 tag 指向的 commit；排隊期間 tag 被移動時，該次執行失敗。

### job 與權限

| job | 權限 | 內容 |
| --- | --- | --- |
| `plan` | `contents: read`、`actions: read` | 檢查 tag（正式版要在 main 上，且該 commit 在 main 的 ci 通過；`actions: read` 只用來讀 ci 的執行結果）、列出要發佈的映像與 chart、`helm lint --strict`（兩種部署型態），產生發版說明。`AI_API_KEY` 只交給產生發版說明的步驟 |
| `build` | `contents: read` | 交叉編譯並建置映像、產生 SBOM。它執行第三方的建置程式碼（Cargo build script、proc macro），所以沒有任何 secret 與寫入權限，交出去的只有資料：映像的 OCI 壓縮檔與 SBOM，以 artifact 交給 publish，保留 1 天 |
| `publish` | `contents: write`、`packages: write`、`id-token: write`；`release` environment | 原封不動推送映像（digest 不變）、簽章、附 SBOM attestation；從 repo 打包、推送並簽章 chart；建立 GitHub Release。不執行建置程式碼 |
| `verify` | `contents: read`、`packages: read` | 以消費端的方式唯讀檢查發佈結果，見下方「簽章與驗證」 |
| `deploy-pr` | `contents: write`、`pull-requests: write`、`actions: write` | verify 通過後提議部署到 SaaS（Laysi/trustiot-v2#70）：以 GitHub API 在 main 上做一個 commit（`deploy/flux/` 的 chart 版本、digest 與 `values-saas.yaml` 副本），推到 `deploy/trustiot-<版本>`、開 PR（候選版為 draft），再以 `workflow_dispatch` 在該分支跑 ci.yml。commit 由 GitHub 以 `github-actions[bot]` 建立並簽章，job 不需要 git 憑證。版本不比 main 上部署中的新時不開 PR；重跑時已有的分支與 PR 不重開。見 [deploy/flux/README.md](../../deploy/flux/README.md) |
| `report` | `contents: write`、`statuses: write` | 不論前面成敗都執行：在 tag 指向的 commit 上設 commit status `release`（失敗時寫出第一個失敗的 job 與原因，含 `deploy-pr`），並把驗證結果附到 GitHub Release |

沒有長期雲端金鑰，也沒有簽章金鑰：GHCR 以各 job 自己的 `GITHUB_TOKEN` 登入；cosign 以 keyless 方式簽章，憑證由 Sigstore Fulcio 依 publish job 的 GitHub OIDC token 簽發，效期約 10 分鐘。

### 交叉編譯與可重現建置

- `cargo auditable zigbuild --locked --release --target aarch64-unknown-linux-musl`：Zig 當 C 編譯器與連結器（cargo-zigbuild），cargo-auditable 把相依清單嵌進執行檔，syft 才列得出 crate。zig、cargo-zigbuild、cargo-auditable、syft、cosign 都鎖在 `.prototools`，Zig 以官方 minisign 簽章驗證，其餘以 release 附的 checksum 驗證。Rust 在 CI 由 runner 的 rustup 依 `rust-toolchain.toml` 安裝，再加上 musl target。
- 映像建置不執行任何 arm64 程式，x86 的代管 runner 不需要模擬器。
- 時間戳一律取 tag 所指 commit 的時間（`SOURCE_DATE_EPOCH`、`podman build --timestamp`、`helm package`），chart 推送時的建立時間也固定，執行檔裡的原始碼路徑換成固定的前綴（`--remap-path-prefix`），所以同一個 commit 以同一組工具重建，會得到相同的映像與 chart digest（2026-10-10 本機驗證，見 [測試報告](../test-reports/2026-10-10-68-release-workflow.md)）。runner 上的 podman 版本不由本 repo 釘選，不同版本打包出的映像層可能不同，所以不保證本機與 CI 建出的 digest 相同。

### 簽章與驗證

簽章身分是 release.yml 在該 tag 上的執行：

```
issuer:   https://token.actions.githubusercontent.com
identity: https://github.com/Laysi/trustiot-v2/.github/workflows/release.yml@refs/tags/<tag>
```

GHCR 套件是私有的，驗證前先以有讀取權限的帳號 `cosign login ghcr.io`：

```sh
cosign verify ghcr.io/laysi/trustiot-v2/<名稱>:<版本> \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity "https://github.com/Laysi/trustiot-v2/.github/workflows/release.yml@refs/tags/v<版本>"
cosign verify-attestation --type spdxjson ghcr.io/laysi/trustiot-v2/<名稱>:<版本> \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity "https://github.com/Laysi/trustiot-v2/.github/workflows/release.yml@refs/tags/v<版本>"
```

chart 以同樣的身分驗證 `ghcr.io/laysi/trustiot-v2/charts/trustiot:<版本>`。每次發佈的 GitHub Release 都列出該版本完整的驗證指令與 digest。SBOM 是 syft 產生的 SPDX 2.3 JSON，以 attestation 附在映像上，也是 GitHub Release 的附件。

verify job 對每個映像檢查：tag 指向 publish 推送的 digest；只有一個平台且是 linux/arm64；簽章與 SBOM attestation 以上述身分（再加上 commit SHA 與觸發事件）驗證通過；匿名拉取被拒，即套件是私有的。chart 檢查 tag、簽章與 version、appVersion。另外以不屬於這次發佈的身分驗證一次，必須失敗，證明上述檢查不是空檢查。

Flux（Laysi/trustiot-v2#70）驗證 chart 時，要用支援 cosign v3 bundle 格式的 Flux（2.8 以上；`.prototools` 釘 2.9.6），`matchOIDCIdentity` 的 issuer 寫 `^https://token\.actions\.githubusercontent\.com$`，subject 寫該版本的完整身分，例如 `^https://github\.com/Laysi/trustiot-v2/\.github/workflows/release\.yml@refs/tags/v1\.2\.0$`（`deploy/flux/clusters/saas/trustiot/chart.yaml`，由 release.yml 的 `deploy-pr` job 寫入）。Flux 只驗證它自己拉的 chart；服務映像由 kubelet 拉取，SaaS 上的映像驗簽要另外的 admission controller（Laysi/trustiot-v2#70 的後續提案）。

### 公開的透明紀錄（2026-10-10 決定接受）

keyless 簽章會寫進 Sigstore 公開服務的透明紀錄：Fulcio 簽發的憑證進入公開的憑證透明度日誌，簽章與 attestation 的紀錄進入 Rekor。任何人都能以映像或 chart 的 digest 查到這些紀錄，紀錄中的憑證寫著 repo 名稱 `Laysi/trustiot-v2` 與數字 ID、組織、workflow 路徑、tag、commit SHA、Actions 執行的網址，以及簽章時 repo 是私有的。Rekor 只存簽章與 attestation 內容的雜湊，不存內容本身，所以 SBOM 列出的相依套件與版本不會公開，原始碼與映像內容也查不到（2026-10-10 以 v0.0.0-rc.1 實查，見 [測試報告](../test-reports/2026-10-10-68-release-workflow.md)）。紀錄無法刪除。

2026-10-10 決定接受（Laysi/trustiot-v2#68 審查，決策 30），理由：

- 公開的只有上述中繼資料，不含程式碼、映像內容或 SBOM。
- 替代方案都更差：自管簽章金鑰（即使放在 KMS）會引入長期簽章金鑰，多了外洩與輪替的風險，違反「沒有長期金鑰」的原則；自建 Sigstore 的維運成本與風險都更高。

日後 repo 名稱或發版節奏變成敏感資訊時，另開 issue 改用 KMS 金鑰或自建 Sigstore。

### 發佈核准（2026-10-10 決定接受的現況）

publish job 在 `release` environment 執行，原本的設計是在 environment 設必要審查者（required reviewers）與只允許 `v*` tag 的部署規則，讓每次發佈都經人工核准。本 repo 是私有庫、組織在 GitHub Free 方案：依 GitHub 文件，Free 方案只能為公開 repo 設定 environment；Pro 與 Team 可以為私有 repo 設定 environment、部署分支與 tag 規則及 environment secret，但必要審查者與等待時間在 Free、Pro、Team 方案都只適用於公開 repo（私有 repo 要 GitHub Enterprise）。所以 environment 擋不下任何發佈：v0.0.0-rc.1 的 publish job 沒有等待核准就執行了。

2026-10-10 決定（Laysi/trustiot-v2#68 審查，決策 30）：暫不升級 GitHub 方案，repo 維持私有，接受發佈沒有人工核准，以下列補償措施把關：

- 只有對 repo 有寫入權限的人能推 tag。
- 正式版 tag 必須在 main 上，而且 main 的 ci 為這個 commit 跑的最新一次執行必須成功（見「tag 規則」）。
- 版本不可變，已發佈的版本不覆蓋。
- 簽章身分綁定 release.yml 與 tag，消費端只接受這個身分。
- 執行第三方程式碼的 build job 沒有寫入權限，也拿不到任何 secret。
- 每次發佈都留下 commit status `release` 與 GitHub Release，可以稽核。

這些措施擋不住有寫入權限的人（或以其憑證操作的 agent）把 main 上 ci 已通過的 commit 打成正式版發佈。workflow 照樣宣告 environment：升級到可為私有 repo 設定必要審查者的方案（目前是 GitHub Enterprise），或 repo 改為公開時，重新檢視本決定，並在 repo 設定的 `release` environment 加上必要審查者（開啟 prevent self-review，推 tag 的人不能自己核准）與 tag 規則 `v*`，不必改 workflow。只升級到 Team 或 Pro 時，可以先加上 tag 規則。

### 結果怎麼看、失敗怎麼處理

- 每次執行都在 tag 指向的 commit 上留下 commit status `release`：成功是「`<tag>` published and verified」，失敗寫出第一個失敗的 job 與原因，連結到該次執行。成功時 GitHub Release 的最後一節「驗證結果」列出 verify job 檢查過的每一項。
- plan 或 build 失敗時還沒有發佈任何東西：候選版修正後改打下一個 rc 號；正式版可以刪掉 tag，修正後重打。正式版只因 main 的 ci 還沒跑完而停在 plan 時，等 ci 通過後重跑這次 release 即可。publish 或 verify 失敗時先看原因：外部服務暫時失敗就只重跑失敗的 job（Re-run failed jobs）：publish 沿用同一次 build 的產物，digest 不變，重跑是安全的；內容有問題就打新的版本號，不要移動已經發佈過的 tag。
- 已發佈的版本不刪除也不覆蓋；有問題的版本以新版本取代，必要時在 GitHub Release 註明。

### Actions 分鐘數與儲存空間

- 一次發佈有 5 個 job，每個 job 的分鐘數各自無條件進位。build 不做 Rust 編譯快取：發佈次數少，快取空間留給 ci.yml。
- build 交給 publish 的 artifact 只保留 1 天。依 GitHub 文件，Free 方案組織的 Packages 與 Actions artifact 共用 500 MB 儲存空間；GHCR 的容器映像目前不計費（GitHub 承諾改變前至少提前一個月通知）。映像變多變大時，留意 artifact 的大小。

## nightly.yml

每晚 18:17 UTC（台北 02:17）執行，也可在 main 上手動執行（`workflow_dispatch`，選項 `simulate_failure` 會讓每份 values 在安裝後故意失敗，用來驗證通知）。動到 `deploy/chart/`、`deploy/istio/`、`scripts/nightly/`、`.prototools`、`.proto/plugins/`、`setup-proto` 或 `nightly.yml` 本身的 PR，帶有 `smoke` label 時也會跑，chart 的改動在合併前就先真的部署一次；這些 PR 的執行不寫任何留言（PR 的 workflow 檔由 PR 自己決定，不給它寫入權限），結果看 check 狀態、job summary 與 `nightly-report` artifact。

### PR 的部署要加 `smoke` label

2026-10-11 決定（Laysi/trustiot-v2#309）：每次部署約 7 個 arm64 計費分鐘，原本動到上列路徑的 PR 每次推送都部署一次。現在：

- PR 沒有 `smoke` label 時不部署：事件照樣觸發 workflow，但 `smoke` job 被略過（`smoke` check 顯示 skipped），不計費。
- 協調者在決定合併前替 PR 加上 `smoke` label，加上的當下（`labeled` 事件）部署一次；之後每次推送都重新部署，同一個 PR 較新的推送會取消還在跑的部署。部署通過才合併，與以前相同。
- 為 PR 加上其他 label 不會部署，也不會取消進行中的部署：這類事件有自己的 concurrency group（`nightly-no-smoke-<run_id>`）。
- label 名稱比對不分大小寫，與 GitHub 的字串比較相同。
- 排程與手動執行不受影響。
- label 在 2026-10-11 建立（顏色 `0e8a16`），說明寫著用途；改名會讓條件失效，要改名就同時改 `nightly.yml` 與本節。
- 條件寫在 `nightly.yml` 的 `concurrency.group` 與 `smoke.if` 兩處，`scripts/ci/workflow-conditions.test.ts` 以 GitHub 的 expression 引擎逐一驗證上列情境。

### 做什麼

`smoke` job 在一台 arm64 的 ubuntu runner（`ubuntu-24.04-arm`）上起一個 k3d 叢集，節點是 arm64，與出貨的架構相同，不覆寫 chart 的 nodeSelector，裝好平台元件，兩份 values（`values-saas.yaml`、`values-onprem.yaml`，各加上 `tests/values/` 的實例覆寫檔與 `smoke.yaml`）依序測試，各用自己的命名空間。k3s 映像是 `rancher/k3s:v1.36.5-k3s1`（SaaS 主機與 lab 的版本），以 multi-arch index 的 digest 釘選，k3s 的設定與正式安裝相同，只關閉 Traefik（決策 27）；ServiceLB 保留，地端型態的入口是 LoadBalancer Service，位址由它指派。

平台元件（`scripts/nightly/platform.ts`，照 `deploy/istio/README.md` 的順序，與 SaaS 主機在裝 chart 前做的事相同）：

1. Gateway API 的 standard CRD：從 release 下載 `standard-install.yaml`，SHA-256 與 workflow 裡釘的值相符才套用（`GATEWAY_API_VERSION`、`GATEWAY_CRDS_SHA256`，要一起改）。
2. Istio ambient：官方 repo 的 `base`、`istiod`、`cni`、`ztunnel` 四個 chart，版本 `ISTIO_VERSION`，用 `deploy/istio/values-k3s.yaml`。istiod 另加一份由 `deploy/istio/istiod-values.yaml` 產生的覆寫：同一個 ext_authz provider（fail closed、0.5 秒逾時、503）為每個 e2e 命名空間各複製一份，名稱 `trustiot-e2e-<shape>`，所以 provider 的設定永遠是 repo 裡那份。
3. 建出 e2e 的身分服務替身映像（`deploy/chart/tests/e2e/identity-double`），匯入 k3d 節點。

每份 values 依序做：

1. 命名空間以 Pod Security `restricted`（enforce、warn、audit）建立：chart 要能在最嚴格的等級下安裝，也是 OpenShift 的前提。
2. 為 chart 渲染出來的 pod 讀取、但 chart 自己不建立的 Secret 建立假值，並為 Gateway listener 引用的 TLS Secret 建立自簽憑證（SaaS 的 Secret 怎麼進叢集見上方「SaaS 的祕密怎麼進叢集」；這裡驗證的是接線，不是帳密）。內建的 ClickHouse（`clickhouse.mode: bundled`，Laysi/trustiot-v2#59）照 `tests/values/<shape>-instance.yaml` 的 `storage.size` 安裝，磁碟區由 k3s 內建的 local-path 提供，管理帳號的 Secret `trustiot-clickhouse-admin` 也是假值；它的 `/ping` 探針與其他 workload 一起被檢查。
3. `helm upgrade --install --wait --wait-for-jobs --timeout 5m`。
4. 讀 `helm get manifest`，每個 Deployment、StatefulSet、DaemonSet 都要 rollout 完成，每個 Job 都要完成。release 一個 workload 都沒有就失敗（`smoke.yaml` 一定會加一個，沒有代表覆寫檔壞了）。
5. 每個 Gateway 的 `Programmed` 條件為真（Istio 在 helm 結束後才產生入口 pod）。
6. 沒有 pod 處於 CrashLoopBackOff、ImagePullBackOff、設定錯誤或未就緒；重啟次數記在報告裡。
7. 每個 Gateway 的 NetworkPolicy 放行的埠，與 Istio 為它產生的 Deployment（探針）和 Service（target port）實際用的埠完全相同：少放行一個，入口就收不到流量或通不過探針；多放行一個，就多開了用不到的埠（例如 Envoy 的指標埠）。chart 的埠是從自己的 Gateway 與 Istio 固定的狀態埠 15021 推得的，這一項以實際安裝的 Istio 版本驗證（Laysi/trustiot-v2#287，見 `deploy/chart/README.md` 的「入口 pod 的埠」）。
8. 每個 workload 的 HTTP 探針（startup、liveness、readiness），凡是有 Service 對外開那個埠的，那個 Service 的 EndpointSlice 在該埠至少有一個就緒的端點。不經網路呼叫：chart 預設拒絕所有連入（決策 27，Laysi/trustiot-v2#63），API server 的 Service proxy 從節點連 pod，會被 NetworkPolicy 與 AuthorizationPolicy 擋下；探針本身已由 kubelet 經 ambient 的探測位址呼叫過，第 6 項的就緒就是結果。服務的探針連健康埠（`health`，Laysi/trustiot-v2#286），Service 不開這個埠，所以服務只由第 6 項涵蓋；這一項檢查的是 smoke 的替身服務（探針連 `http`）與 ClickHouse 這類由 Service 開放探針埠的工作負載。
9. `helm uninstall` 之後，release 的每個物件都不存在。
10. 入口 e2e 測試（`deploy/chart/tests/e2e/ingress.e2e.ts`，Laysi/trustiot-v2#62）：在 `nightly-e2e-<shape>` 命名空間以對應的型態跑一次，驗證 cookie 驗證、例外路徑、路徑繞過與身分服務停機時的行為；這也是 #62 要求的 k3d 測試，每晚執行。請求經 `kubectl port-forward` 直接進入口 pod，所以不涵蓋入口的 NetworkPolicy（那一項只在 lab 以 `E2E_SSH_JUMP` 驗證）。通道在請求進行中斷線時，測試會重開通道並重送一次（Laysi/trustiot-v2#261，細節見該目錄的 README）。

每份 values 任何一項失敗就停止該份並收集診斷（物件、事件、`describe`、pod 日誌；不含 Secret 內容，輸出可能含 Secret 的指令不寫進日誌）。

檢查 3 到 9 全部從 release 自己的 manifest 推導（`scripts/nightly/manifests.ts`），不寫死元件；之後加入的元件（Postgres、ClickHouse、broker 等）會自動被納入 workload 就緒與探針檢查。元件需要的前置（例如 operator）由加入該元件的 issue 在 `scripts/nightly/platform.ts` 補上；各里程碑完成條件的測試也由各自的 issue 加進來。

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

一次執行一個 job（加上約 1 分鐘的 `notify`），兩份 values 共用一個叢集。實測的分鐘數見 `docs/test-reports/2026-10-10-69-nightly-workflow.md`。兩個 job 都在 arm64 runner 上（`notify` 自 2026-10-11 起，Laysi/trustiot-v2#309）。PR 只在帶 `smoke` label 時部署，見上方「PR 的部署要加 `smoke` label」。

## Renovate

設定在 `renovate.json5`：

| 範圍 | 做法 |
| --- | --- |
| Rust crate | Renovate 的 cargo manager 更新 `Cargo.toml` 與 `Cargo.lock`；版本固定為精確值（`rangeStrategy: pin`），minor 與 patch 每週合成一個 PR |
| Bun 套件 | `scripts/` 的 `package.json` 與 `bun.lock`，每週一個 PR |
| GitHub Actions | `helpers:pinGitHubActionDigestsToSemver`：每個 `uses:` 維持 40 字元 commit SHA，行尾註解維持版本 |
| Istio 與其他寫在 workflow env 的版本 | env 上方寫 `# renovate:` 註解（資料來源與套件名稱）即由自訂規則更新；Istio 停在 1.30 minor。Gateway API 的版本與雜湊要一起改，不由 Renovate 更新 |
| 容器映像 | `pinDigests: true`；原始碼、測試、workflow 與 values 裡寫成 `名稱:tag@sha256:…` 的映像由自訂規則讀取，更新時 tag 與 digest 一起換；chart 的 `repository`／`tag`／`digest` 分三行寫法也有規則；`deploy/images/` 的 Containerfile（release.yml 的映像基底）由 Renovate 內建的 dockerfile manager 讀取；`rancher/k3s` 與 kubectl 停在 1.36 minor，換 minor 是刻意的升級 |
| `.prototools` | Renovate 的 proto manager 認得 `bun`、`go` 與 `rust`；有 schema 外掛的工具（`.proto/plugins/` 下的每一個，含 release.yml 用的 zig、cargo-zigbuild、cargo-auditable、syft、cosign，以及 flux、sops）在 pin 上方寫 `# renovate:` 註解，由自訂規則更新。`flux` 的更新 PR 要在同一個 PR 執行 `bun scripts/flux/components.ts` 重寫 Flux 的 manifest 與控制器 digest，否則 `bun test deploy/flux` 失敗（[deploy/flux/README.md](../../deploy/flux/README.md)「升級 Flux」）。上游在 GitHub 的工具以 `owner/repo` 命名；zig 的上游在 Codeberg（GitHub 上的 repo 在 0.15.2 之後沒有 tag），以 `git-tags` 讀 Codeberg repo 的 tag。新增工具時照樣寫註解，`bun test scripts/nightly` 會檢查註解存在、且指向外掛 `git-url` 的上游 |
| Rust 工具鏈 | `.prototools` 的 `rust` 與 `rust-toolchain.toml` 的 `channel` 合成一個 PR，兩處同時改（`repo-check` 要求兩者相同） |

PR 標題是 Angular 格式（`chore(deps): …`），標籤 `area:ci` 與 `dependencies`；同時開著的 PR 最多 5 個、每小時最多 2 個，週一 06:00 前（台北時間）才開，資安修補不受排程限制。

不由 Renovate 更新的項目（要手動改）：

- nightly 的 `GATEWAY_API_VERSION` 與 `GATEWAY_CRDS_SHA256`：同上，版本與雜湊要一起換。
- `.github/actions/setup-proto/action.yml` 的 `PROTO_VERSION` 與 `PROTO_SHA256_X64`、`PROTO_SHA256_ARM64`：三個值必須一起換，Renovate 只能改版本號。
- `scripts/lib/stack.ts` 的開發環境映像：只有 digest，tag 寫在註解，Renovate 讀不到。把 tag 寫進字串（`名稱:tag@sha256:…`）就會被納入。
- `scripts/flux/age.ts` 的 `AGE_VERSION`（age 從原始碼建置的版本）。
- Flux 控制器映像的 digest（`deploy/flux/clusters/saas/flux-system/kustomization.yaml`）：隨 `flux` 的 pin 由 `bun scripts/flux/components.ts` 重寫，不單獨更新。

### 需要人在 GitHub 組織裝 Renovate

設定檔本身不會執行，要有 Renovate 跑它：組織管理員安裝 Mend 的 Renovate GitHub App，授權給 `Laysi/trustiot-v2`。App 會先開一個 onboarding PR 或直接讀 `renovate.json5`；之後 Dependency Dashboard issue 列出所有待辦。這是管理員動作，agent 不做。App 的代價是第三方存取原始碼，私有庫要確認組織同意；替代做法是用 `renovatebot/github-action` 在我們自己的 workflow 跑，但那需要一個有寫入權限的 token（GitHub App 或 PAT）存成 secret，目前原則是 GitHub 不持有這類憑證，所以優先用 App。

設定的驗證：`renovate-config-validator` 通過；以 `renovate --platform=local --dry-run=extract` 對這個 repo 抽取依賴（結果在測試報告）。
