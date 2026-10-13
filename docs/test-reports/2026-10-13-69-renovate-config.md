# Renovate 設定修正：第一個 Dependency Dashboard 顯示的問題

對應 issue：Laysi/trustiot-v2#69 的第二條驗收條件「Renovate 開出的 PR 保持 SHA 與 digest 釘選」的前置工作。Renovate GitHub App 於 2026-10-13 建立 Dependency Dashboard（Laysi/trustiot-v2#389），列出的更新與警告顯示 `renovate.json5` 有五個問題，第一次排程（2026-10-19 週一 06:00 前，台北時間）前要修正；另外確認第一次「pin dependencies」PR 對 Rust workspace 相依加上的 `=` 釘選不會讓 workspace 建置失敗。

## 環境

- 工作站：Fedora，x86_64，podman 6.1.3，skopeo，`.prototools` 的 bun 1.4.2。
- Renovate：容器映像 `ghcr.io/renovatebot/renovate:44.155.1@sha256:f5796f26088c0f5debb07c16fa9e801145e4d3ba9717d9cc22ca8cd265e2a0eb`（2026-10-12 建立的 multi-arch index），以 podman 執行。proto 沒有 Renovate 的 plugin，所以用釘選 digest 的官方映像，這是組織規則允許的最窄替代做法。Mend 代管的 App 跑的版本不由我們決定，可能較新。
- 沒有 GitHub token：本機執行時 Renovate 對 GitHub 上的資料來源（GitHub Actions、`github-releases`、`github-tags`，例如 `go`、`fluxcd/flux2`、`kubernetes/kubernetes`）一律標 `github-token-required` 不查詢。這些依賴的規則改以 Renovate 本身的程式（package rule 比對、versioning 與版本篩選、auto-replace）在同一個映像裡驗證，見下方。Docker Hub 與 ghcr 的查詢不需要 token。
- Rust 的建置檢查：建置主機（x86_64，經 check queue `rcheck.sh`），Rust 1.99.0。
- 分支 `fix/69-renovate-config`，基於 main `45f29d7`。「之前」是 main 的 `renovate.json5`，「之後」是本分支的。
- 日期：2026-10-13（Asia/Taipei）。

## 指令

快照：「之前」是 `git archive origin/main` 解開的目錄，「之後」是工作目錄的 tracked 與 untracked 檔案。以下腳本都在 [data 目錄](data/2026-10-13-69-renovate-config/)，`<dir>` 是放快照與腳本的暫存目錄（`before/`、`after/`、`probe/` 在它底下）。

```sh
# 設定驗證（renovate-config-validator --strict，讀快照根目錄的 renovate.json5）
bash validate.sh <dir>/before
bash validate.sh <dir>/after

# 本機 dry run：抽取依賴並查詢更新，不寫任何東西（LOG_LEVEL=debug、LOG_FORMAT=json）
bash run-renovate.sh <dir>/before before.log renovate --platform=local --dry-run=lookup
bash run-renovate.sh <dir>/after after.log renovate --platform=local --dry-run=lookup
bun summarize.ts before.log > lookup-before.tsv
bun summarize.ts after.log > lookup-after.tsv

# 以 Renovate 本身的程式驗證查不到的部分（node-probe.sh 把 before/、after/ 與 probe/ 掛進映像）
bash node-probe.sh rules.mjs        # packageRules 套用到 go、fluxcd/flux2、ubuntu runner 等依賴的結果
bash node-probe.sh versioning.mjs   # k3s 與 kubectl 的 versioning 與版本篩選（filterVersions）
bun extract-deps.ts after.log probe/values-deps.json
bash node-probe.sh autoreplace.mjs  # chart values 的更新以 Renovate 的 auto-replace 實際改寫

# k3s 與 Kubernetes 的實際 tag（版本篩選的輸入）
skopeo list-tags docker://docker.io/rancher/k3s        # 取 v1.36、v1.37 的 tag 存成 probe/k3s-tags.txt
git ls-remote --tags --refs https://github.com/kubernetes/kubernetes 'v1.36*' 'v1.37*'   # 去掉 v 存成 probe/k8s-tags.txt

# Rust 的 = 釘選：把 dry run 規劃的釘選套到 Cargo.toml（只在工作目錄暫時修改，檢查後還原）
bun apply-pins.ts pins.txt Cargo.toml
bash rcheck.sh 69r 'cargo check --workspace --all-targets --locked ...; cargo update --dry-run'
git checkout -- Cargo.toml

# 規則測試
bun install --cwd scripts --frozen-lockfile
bun test scripts/nightly
bun run --cwd scripts typecheck
```

rcheck 的完整指令在 [cargo-check-with-pins.log](data/2026-10-13-69-renovate-config/cargo-check-with-pins.log) 與 [cargo-update-dry-run.log](data/2026-10-13-69-renovate-config/cargo-update-dry-run.log) 的對應段落之前已列出；第一次是 `grep -c "\"=" Cargo.toml; cargo check --workspace --all-targets --locked 2>&1 | tail -25; …; git status --porcelain -- Cargo.lock Cargo.toml; cargo update --dry-run 2>&1 | tail -40`，第二次是有釘選與去掉釘選各跑一次 `cargo update --dry-run --verbose`。

## 結果

### 設定驗證

| 設定 | 結果 |
| --- | --- |
| main 的 `renovate.json5` | `Config validated successfully`（[validator-before.txt](data/2026-10-13-69-renovate-config/validator-before.txt)） |
| 本分支的 `renovate.json5` | `Config validated successfully`（[validator-after.txt](data/2026-10-13-69-renovate-config/validator-after.txt)） |
| 本分支，故意把 k3s 的 versioning regex 改壞（少了 `)$`） | 仍然 `Config validated successfully`（[validator-mutant.txt](data/2026-10-13-69-renovate-config/validator-mutant.txt)） |

validator 不檢查 `regex:` versioning 的內容，所以 `bun test scripts/nightly` 會編譯那個 regex 並以實際的 tag 測試，見下方「規則測試」。

dry run 的依賴與更新逐行列在 [lookup-before.tsv](data/2026-10-13-69-renovate-config/lookup-before.tsv) 與 [lookup-after.tsv](data/2026-10-13-69-renovate-config/lookup-after.tsv)（欄位：manager、檔案、依賴、目前的值與 digest 前 12 碼、versioning、略過原因、更新）。兩次的 log 都有一則 WARN：GitHub token 不存在，GitHub 上的依賴不查詢。修正前另有兩個依賴層級的警告 `Could not determine new digest for update`（`deploy/chart/values.yaml` 的 clickhouse 與 otel，與 Dashboard 的警告相同，見下方第 3 項），修正後消失。

### 1. k3s 的 release candidate

原因：`rancher/k3s` 用 `versioning: "loose"`，它把 `v1.36.5-rc2-k3s1` 排在 `v1.36.5-k3s1` 之後，而且視為穩定版；`allowedVersions: "/^v?1\.36\./"` 也擋不掉。修正：k3s 改用 `regex:^v(?<major>\d+)\.(?<minor>\d+)\.(?<patch>\d+)-k3s(?<build>\d+)$`，只有正式版是版本，RC 與單一架構的 tag（`-amd64`）不符合就不會被提議；1.36 的限制不變。

| | 之前 | 之後 |
| --- | --- | --- |
| dry run（Docker Hub 實際查詢） | `rancher/k3s v1.36.5-k3s1@057cfd5a9d51` → `patch -> v1.36.5-rc2-k3s1@5d18c19cce99`，分支 `renovate/rancher-k3s-1.x` | `no updates` |
| 以 2026-10-13 的實際 tag 篩選（[versioning-probe.txt](data/2026-10-13-69-renovate-config/versioning-probe.txt)） | 可提議 `v1.36.5-rc1-k3s1`、`v1.36.5-rc2-k3s1` | 沒有；19 個 RC tag 都不是版本 |
| 下一個 patch（假設的 tag） | 可提議 `v1.36.5-k3s1-amd64`、`v1.36.5-k3s2`、`v1.36.6-rc1-k3s1`、`v1.36.6-k3s1` | 只有 `v1.36.5-k3s2`、`v1.36.6-k3s1` |

`kubernetes/kubernetes`（`.prototools` 的 kubectl）沒有改：它用 Renovate 預設的 `semver-coerced`，`-alpha.N`、`-beta.N`、`-rc.N` 都判為不穩定，目前的 pin 是穩定版時不會被提議。以 GitHub 上實際的 v1.36、v1.37 tag 篩選，沒有可提議的版本（1.36.5 是 1.36 最新的 patch，1.37 被擋）；加上假設的 `1.36.6-alpha.0` 到 `1.36.6-rc.1` 時，只提議 `1.36.6`。本機沒有 token，`github-tags` 的實際查詢沒有跑。

### 2. GitHub runner 映像

原因：github-actions manager 把每個 `runs-on: ubuntu-24.04-arm` 當成 `github-runners` 資料來源的依賴，提議升到 `26.04-arm`（Dashboard 的「update dependency ubuntu to v26」）。修正：`github-runners` 資料來源的 rule 設 `enabled: false`，description 寫明 runner 是 `docs/architecture/ci-release.md` 記錄的刻意選擇。

| | 之前 | 之後 |
| --- | --- | --- |
| dry run | 16 個 `ubuntu 24.04-arm` → `major -> 26.04-arm`，分支 `renovate/major-github-actions` | 16 個都是 `skip=disabled`，沒有 `renovate/major-github-actions` 分支 |
| rule 比對（[rules-probe.txt](data/2026-10-13-69-renovate-config/rules-probe.txt)） | `ubuntu github-runners enabled=true` | `enabled=false`；`actions/checkout` 仍是 `pinDigests=true` |

### 3. chart values 的映像

原因：`deploy/chart/values.yaml` 的映像寫成 `repository:`／`tag:`／`digest:` 三行，被兩個 manager 讀到。內建的 helm-values manager 讀得到 repository 與 tag、讀不到 digest，所以想再「pin」一次，並在 Dashboard 留下「Could not determine new digest」；自訂的 regex manager 把 tag 連引號一起讀成 `"26.8"`，查不到這個 tag，同樣的警告。修正：關閉 helm-values manager（`"helm-values": { enabled: false }`），只由自訂 manager 讀；它的 regex 把 repository、tag、digest 外面可能有的單引號或雙引號放在 capture group 外。

| | 之前 | 之後 |
| --- | --- | --- |
| helm-values manager | clickhouse `26.8`：`minor -> 26.9@387ebbcca253`、`pinDigest -> 26.8@dd81097e2c38`（pin-dependencies 群組）；otel `0.161.0`：`minor -> 0.162.0@39923a8e431b`、`pinDigest -> 0.161.0@fd328de25524` | 不再抽取 |
| 自訂 regex manager | clickhouse `"26.8"@110d3d1f8e1e`、otel `"0.161.0"@fd328de25524`：都是 `no updates`，警告 `Could not determine new digest for update` | clickhouse `26.8@110d3d1f8e1e`：`minor -> 26.9@387ebbcca253`、`digest -> 26.8@dd81097e2c38`；otel `0.161.0@fd328de25524`：`minor -> 0.162.0@39923a8e431b`；沒有警告 |

以 Renovate 的 auto-replace（分支實際改檔的程式，含改完後重新抽取確認）套用這三個更新（[autoreplace-probe.txt](data/2026-10-13-69-renovate-config/autoreplace-probe.txt)）：引號保留，tag 與 digest 一起換，例如 clickhouse 的 minor 更新是 `tag: "26.8"` → `tag: "26.9"` 加上 `digest: sha256:110d3d1f…` → `digest: sha256:387ebbcc…`；只換 digest 的更新只改 digest 那一行。

其他 values 檔：`deploy/chart/tests/values/smoke.yaml` 與 `deploy/chart/tests/e2e/values-e2e.yaml` 的 whoami 是不加引號的 `v1.12.0`，修正前後都讀得到；`values-saas.yaml`、`values-onprem.yaml` 與 `deploy/flux/` 的 values 沒有映像。`deploy/flux/clusters/saas/flux-system/kustomization.yaml` 的 Flux 控制器映像被 kustomize manager 以 `invalid-dependency-specification` 略過（`newTag` 加 `digest`），它們本來就由 `bun scripts/flux/components.ts` 重寫，不需要 Renovate。

ClickHouse 的浮動 tag：values 的 `"26.8"` 釘在 `110d3d1f…`，而 2026-10-13 的 `26.8` 已指向 `dd81097e…`（skopeo 查到的 index digest），所以修正後會多一個只換 digest 的 PR（分支 `renovate/docker.io-clickhouse-clickhouse-server-26.8`），帶進 26.8 的新 patch 版。每一種更新（digest、minor）都會重寫 digest，符合「浮動 minor tag 加 digest」可以接受的條件。另外 Renovate 會提議 `26.9`：ClickHouse 的 LTS 是每年的 `.3` 與 `.8`，26.9 不是 LTS，支援期較短；可行性驗證用的是 26.8 LTS。要不要讓 Renovate 只提議 LTS（例如 `allowedVersions` 只允許 `.3`、`.8`）是產品決定，本次沒有改，列為後續。

### 4. Dashboard 的標籤

修正：`dependencyDashboardLabels: ["area:ci", "priority:P2"]`，與 coordinator 在 Laysi/trustiot-v2#389 手動加的相同。本機 `--platform=local` 不建立 issue，這一項由 validator 與 `bun test scripts/nightly`（恰好一個 `priority:P0|P1|P2`，至少一個 `area:`）驗證；App 下一次執行時套用。

### 5. pin dependencies 群組裡的 `go` 與 `fluxcd/flux2`

原因：top-level `pinDigests: true`（為了容器映像）也套到 proto manager 與 flux manager。`github-tags` 與 `github-releases` 兩個資料來源都能查 digest（tag 指向的 commit），所以 Renovate 為沒有 digest 的 `go`（proto manager，`golang/go` 的 tag）與 `fluxcd/flux2`（flux manager，`gotk-components.yaml` 的 `# Flux Version: v2.9.6`）各提議一個 `pinDigest`，版本不變（`1.27.2 → 1.27.2`、`v2.9.6 → v2.9.6`）。那個 PR 實際上：

- `go`：`.prototools` 沒有寫 digest 的地方，proto manager 不改任何東西，是空的更新。
- `fluxcd/flux2`：flux manager 的 artifact 更新以同一版 `flux install --export --components …` 重寫 `gotk-components.yaml`，內容相同時 Renovate 回報「Flux contents are unchanged」，也是空的更新。真正的版本升級時，它只重寫 `gotk-components.yaml`，不重寫 `kustomization.yaml` 的控制器 digest，也不驗簽，與 `bun scripts/flux/components.ts` 重複又不完整。

自訂 regex manager 預設 `pinDigests: false`，所以 `.prototools` 裡其他寫 `# renovate:` 註解的工具沒有這個問題；`bun` 也是 proto manager 讀的 GitHub 依賴，Dashboard 上卻沒有它的 digest 釘選；推測是它的 release tag 為 `bun-v1.4.2`，Renovate 以 `1.4.2` 找不到 tag 的 commit 而放棄（沒有 token，本機無法確認）。修正後它同樣不會有 digest 釘選。

修正：proto manager 設 `pinDigests: false`；flux manager 的 `fluxcd/flux2` 設 `enabled: false`，Flux 只由 `.prototools` 的 `flux` pin 帶動（與 `deploy/flux/README.md`「升級 Flux」一致）。`.prototools` 的 pin 仍是精確版本。

| | 之前 | 之後 |
| --- | --- | --- |
| rule 比對 | `proto go github-tags pinDigests=true`；`flux fluxcd/flux2 github-releases enabled=true pinDigests=true` | `proto go pinDigests=false`；`flux fluxcd/flux2 enabled=false`；`.prototools` 的 `fluxcd/flux2`（regex）仍是 `enabled=true`；flux manager 讀到的 OCI 映像仍是 `pinDigests=true` |
| dry run 的 pin-dependencies 群組 | 42 個（40 個 Rust 釘選與 helm-values 的 2 個 digest 釘選；本機查不到 `go` 與 `fluxcd/flux2`，Dashboard 上有） | 40 個，全部是 Rust 的 `=` 釘選 |

`go` 與 `fluxcd/flux2` 的實際查詢需要 GitHub token，本機沒有跑；以 rule 比對確認之後的設定不會產生這兩個更新（lookup 的程式只在 `pinDigests` 為 true 時才加 `pinDigest`，被 `enabled: false` 的依賴不查詢）。App 下一次執行後，Dashboard 的 pin dependencies 應該只剩 Rust crate。

### 6. Rust 的 `=` 釘選

dry run 規劃的 40 個釘選（[pins.txt](data/2026-10-13-69-renovate-config/pins.txt)）都取 `Cargo.lock` 的版本，除了 `cc`（`1.5.1` → `=1.7.0`，lock 裡是 1.7.0）都與目前的需求相同。套到 `Cargo.toml` 後（[cargo-pins.diff](data/2026-10-13-69-renovate-config/cargo-pins.diff)）：

| 檢查 | 結果 |
| --- | --- |
| `cargo check --workspace --all-targets --locked`（建置主機） | 通過，`Finished`，exit 0；`Cargo.lock` 沒有變動（[cargo-check-with-pins.log](data/2026-10-13-69-renovate-config/cargo-check-with-pins.log)） |
| `cargo update --dry-run --verbose`，有釘選 | 只動 `crypto-common`（0.1.7 → 0.1.6）、`generic-array`、`syn 3.x`、`unicode-ident`；`cc`、`serde_json`、`sha2` 留在 lock 的版本（[cargo-update-dry-run.log](data/2026-10-13-69-renovate-config/cargo-update-dry-run.log)） |
| `cargo update --dry-run --verbose`，沒有釘選（main 的需求） | 同上，另外 `cc` 1.7.0 → 1.8.0、`serde_json` 1.0.151 → 1.0.154、`sha2` 0.11.0 → 0.11.1 |

沒有 resolver 衝突。`crypto-common` 的降版在兩種情況都出現，與釘選無關。

## 結論

| 項目 | 結果 |
| --- | --- |
| 1. k3s 不會被提議 RC | 通過：dry run 不再提議 `v1.36.5-rc2-k3s1`；實際 tag 與假設的下一版都只剩正式版；kubectl 預設就不會拿到預發布版（實際的 `github-tags` 查詢待測，需要 token） |
| 2. runner 不被升級 | 通過：16 個 `ubuntu 24.04-arm` 都 `skip=disabled` |
| 3. chart values 的映像只由一個 manager 讀取、保持 digest 釘選、引號都可以 | 通過：警告消失，三個更新都同時重寫 tag 與 digest，引號保留 |
| 4. Dashboard 的標籤 | 設定與測試通過；實際套用要等 App 下一次執行 |
| 5. 空的 pin（`go`、`fluxcd/flux2`） | 原因確認，設定已修正，rule 比對通過；實際查詢待測（需要 token），App 下一次執行後在 Dashboard 確認 |
| 6. Rust `=` 釘選後可建置 | 通過：`cargo check --workspace --all-targets --locked` 成功，lock 不變，沒有 resolver 衝突 |

風險與後續：

- `=` 釘選在 `serde`、`syn`、`proc-macro2`、`tokio` 這類很多 crate 共用的相依上：某個相依的新版要求更高的 patch（例如新的 `serde_json` 要求更新的 `serde`）時，必須和那個被釘選的 crate 在同一個 PR 一起升，否則 Renovate 的 lock 更新會失敗。目前 minor 與 patch 都合在每週的 `rust crates` PR，通常會一起升；若某個更新單獨開 PR（例如 major 或 `minimumReleaseAge` 等規則日後拆開），可能出現 artifact 錯誤，要手動合併處理。
- 釘選之後，每月的 lock file maintenance 只移動間接相依；直接相依的 patch（`cc` 1.8.0、`serde_json` 1.0.154、`sha2` 0.11.1）改由 `rust crates` PR 帶進，資安修補由 `vulnerabilityAlerts` 隨時開 PR。
- `renovate-config-validator` 不檢查 `regex:` versioning 的內容；由 `bun test scripts/nightly` 編譯並以實際 tag 測試。
- ClickHouse 是否只跟 LTS（見上方第 3 項）待決定。
- 第 4、5 項與 `github-tags` 的實際查詢，在 App 下一次執行後看 Dashboard 確認。
