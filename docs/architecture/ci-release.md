# CI 與發佈

GitHub Actions，三個 workflow：

1. `ci.yml`：每個 PR（不論目標分支，整合分支也一樣）與推到 main 都跑，全綠才能合併（目前由流程把關，見下方「必要檢查與強制方式」）。Rust 跑 fmt、clippy、測試與 sqlx 離線檢查，並以 cargo-deny 檢查授權與漏洞公告（地端產品不能混入 BSL 等不能轉售的授權）；整合測試以 service container 起含 AGE 的 Postgres、ClickHouse 與 Mosquitto，跑完全部 migration 後跑跨租戶負向測試（RLS、row policy、AGE 角色）；閘道器程式交叉編譯成 aarch64，agent 用 testkit 的假 Modbus 端點跑故障注入測試；前端跑 Bun 安裝、型別檢查、lint、單元測試與建置；重新產生 `api/openapi.json` 與前端 client，有差異就失敗；helm lint，兩份 values 各自 helm template 後以 kubeconform 驗證（`bun test deploy/chart` 一次跑完，也檢查 chart 自己的安全規則，見 `deploy/chart/README.md`）；gitleaks 掃描祕密。各項由哪個 job 負責、哪些還沒加入，見下方「ci.yml 的 job」。Codex 審查設為 PR 的必要檢查，是伺服器端的關卡；本機 pre-commit 只是提早攔下。
2. `release.yml`：打 tag 時跑。容器映像只出 arm64：Rust 先交叉編譯再放進映像，推到 GHCR，以 cosign 簽章並附 SBOM；SaaS 主機與展示機（Mac M1）都要是 arm64，地端客戶若用 x86 主機，到時再加 amd64。Helm chart 打包推到 GHCR。閘道器 agent 與 updater 的 aarch64 執行檔附建置來源證明，由 CI 以 OTA 簽章金鑰簽章：金鑰存在受保護的 GitHub environment，只有 main 上的 tag 能用，每次簽章都要人工核准。另外產出離線安裝包（MS6）；release notes 沿用 `Laysi/release-notes-action`。
3. `nightly.yml`：每晚用 k3d 部署整套 chart（兩份 values 各一次），跑端到端冒煙測試；依賴更新交給 Renovate。

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

私有庫在 Free 方案的 Actions 分鐘數有上限，也沒有 arm64 的 GitHub 代管 runner，所以：同一個 PR 的新推送會取消舊的執行；每個 job 設 `timeout-minutes`；Rust 的編譯快取（`Swatinem/rust-cache`）只在 main 儲存，PR 只讀取；aarch64 一律交叉編譯，不用模擬器；同一項檢查只跑一次。

### 工具安裝

CI 以 `.github/actions/setup-proto` 安裝 proto：版本與壓縮檔的 SHA-256 寫死在該 action，不執行會變動的線上安裝腳本；再以 proto 安裝 `.prototools` 釘選的工具。沒有內建 proto 外掛的工具（cargo-deny、gitleaks、helm、kubeconform）在 `.proto/plugins/` 設定，下載官方 release 並以 release 附的 checksum 驗證。Rust 是例外：proto 的 Rust 外掛本身只是呼叫 rustup，而 `proto install rust` 在 GitHub 代管 runner 上無法完成（2026-10-10 以暫時 PR 實測：job 超過 10 分鐘的上限仍未結束，同一個 action 裝 Bun、gitleaks 都正常；沒有 Actions 紀錄的讀取權限，原因待查），所以 setup-proto 在 CI 直接用 runner 映像內建的 rustup 執行不帶參數的 `rustup toolchain install`，依 `rust-toolchain.toml` 安裝釘選的版本、元件與 profile。本機照常由 proto 經 rustup 安裝。兩邊的 cargo 都由 rustup 讀 `rust-toolchain.toml` 決定版本，`tools/repo-check` 保證它與 `.prototools` 的 `rust` 相同，所以 CI 與本機用的是同一組版本。

### 授權白名單

`deny.toml` 只允許寬鬆授權：0BSD、Apache-2.0（含 Apache-2.0 WITH LLVM-exception）、BSD-2-Clause、BSD-3-Clause、CC0-1.0、CDLA-Permissive-2.0、ISC、MIT、MIT-0、Unicode-3.0、Zlib。其他授權一律失敗，包括 Business Source License（SPDX `BUSL-1.1`）、SSPL、Elastic License、Commons Clause 與 GPL、LGPL、AGPL。MPL-2.0 等檔案層級的 copyleft 授權還沒決定，遇到時再評估。要加入新授權，先確認它允許商業轉售，再把授權與理由寫進本節與 `deny.toml`。SPDX 的 `BSL-1.0` 是 Boost 授權，不是 Business Source License。

### fork 來的 PR

ci.yml 不使用任何 secret，每個 job 的 GITHUB_TOKEN 只有 `contents: read`，checkout 也不把 token 留在 `.git/config`（`persist-credentials: false`）。fork 來的 PR 以 `pull_request` 事件執行，GitHub 不給 secret、token 唯讀；`workflow-policy` 擋下 `pull_request_target` 與 `workflow_run`。私有庫 Actions 設定中的「Send write tokens to workflows from fork pull requests」與「Send secrets to workflows from fork pull requests」保持關閉（GitHub 預設值）。
