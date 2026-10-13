# Renovate 設定修正：第一個 Dependency Dashboard 顯示的問題

對應 issue：Laysi/trustiot-v2#69 的第二條驗收條件「Renovate 開出的 PR 保持 SHA 與 digest 釘選」。Renovate GitHub App 於 2026-10-13 建立 Dependency Dashboard（Laysi/trustiot-v2#389），列出的更新與警告顯示 `renovate.json5` 有五個問題，第一次排程（2026-10-19 週一 06:00 前，台北時間）前要修正；另外確認第一次「pin dependencies」PR 對 Rust workspace 相依加上的 `=` 釘選不會讓 workspace 建置失敗。第二輪（同一天）處理同一個 pin 寫在好幾個檔案的情況，讓 ClickHouse 只跟 LTS（見「同一個 pin 的多份副本」）。最後一節是從 Dashboard 提前要求的三個 Renovate PR 的證據。

## 環境

- 工作站：Fedora，x86_64，podman 6.1.3，skopeo，`.prototools` 的 bun 1.4.2。
- Renovate：容器映像 `ghcr.io/renovatebot/renovate:44.155.1@sha256:f5796f26088c0f5debb07c16fa9e801145e4d3ba9717d9cc22ca8cd265e2a0eb`（2026-10-12 建立的 multi-arch index），以 podman 執行。proto 沒有 Renovate 的 plugin，所以用釘選 digest 的官方映像，這是組織規則允許的最窄替代做法。Mend 代管的 App 跑的版本不由我們決定，可能較新。
- 沒有 GitHub token：本機執行時 Renovate 對 GitHub 上的資料來源（GitHub Actions、`github-releases`、`github-tags`，例如 `go`、`fluxcd/flux2`、`kubernetes/kubernetes`）一律標 `github-token-required` 不查詢。這些依賴的規則改以 Renovate 本身的程式（package rule 比對、versioning 與版本篩選、auto-replace）在同一個映像裡驗證，見下方。Docker Hub 與 ghcr 的查詢不需要 token。
- Rust 的建置檢查：建置主機（x86_64，經 check queue `rcheck.sh`），Rust 1.99.0。
- 分支 `fix/69-renovate-config`。第一輪基於 main `45f29d7`：「之前」是 main 的 `renovate.json5`，「之後」是第一輪的設定。第二輪基於 main `659788c`（已含 #391–#393），「最終」是第二輪的設定與檔案。
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

ClickHouse 的浮動 tag：values 的 `"26.8"` 釘在 `110d3d1f…`，而 2026-10-13 的 `26.8` 已指向 `dd81097e…`（skopeo 查到的 index digest），所以修正後會多一個只換 digest 的 PR（分支 `renovate/docker.io-clickhouse-clickhouse-server-26.8`），帶進 26.8 的新 patch 版。每一種更新（digest、minor）都會重寫 digest，符合「浮動 minor tag 加 digest」可以接受的條件。另外 Renovate 會提議 `26.9`：ClickHouse 的 LTS 是每年的 `.3` 與 `.8`，26.9 不是 LTS，支援期較短；values 與 `deploy/chart/README.md` 記的是 26.8 LTS。第二輪讓 ClickHouse 只跟 LTS，並發現這個只換 digest 的 PR 只改 values.yaml、會讓 CI 失敗（另外兩份副本沒被讀到），見「同一個 pin 的多份副本」。

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
- 第 4、5 項與 `github-tags` 的實際查詢，在 App 下一次執行後看 Dashboard 確認。
- 刻意擋下的四種 PR（ClickHouse 換 LTS minor、collector 換版本、k3s、Istio）會開出來但 CI 失敗，要有人完成另一半，見下一節。
- `docker.io/library/registry` 在 `deploy/flux/tests/e2e/lab.e2e.ts`（`3.1.2`）與 `scripts/README.md`（`3`）是兩個 tag，今天剛好同一個 digest；它們是各自獨立的 pin，會分開更新。

## 同一個 pin 的多份副本（第二輪）

### 問題

Renovate 的 PR 只改它讀得到的副本。第一輪修正後的 dry run 有一個只換 digest 的 ClickHouse PR（`26.8@110d3d1f… → 26.8@dd81097e…`），但它只改 `deploy/chart/values.yaml`：`scripts/lib/stack.ts` 與 `services/ingest/tests/common/mod.rs` 寫成 `docker.io/clickhouse/clickhouse-server@sha256:…`，沒有 tag，Renovate 讀不到；chart 測試要求三份相同，這個 PR 的 CI 會失敗（下方模擬 C）。另外 chart 測試寫死 collector 的 digest，連同一版本的新 digest 也會讓 CI 失敗。

### 變更

- 只寫 digest 的映像都補上 tag（`名稱:tag@sha256:…`）：`stack.ts` 的四個映像（AGE、ClickHouse、Mosquitto、服務映像的 distroless 基底）、`mod.rs` 的 ClickHouse、`crates/protocol/tests/broker.rs` 的 Mosquitto。digest 不變；podman 與 docker 都接受這個寫法（podman 6.1.3 實際 `run` 與 `rmi` 過）。
- `scripts/README.md` 的指令也由映像的自訂 manager 讀取。
- ClickHouse 只跟 LTS：`allowedVersions: "/^\\d+\\.(3|8)$/"`。
- chart 測試：ClickHouse 比對三份完整的 `名稱:tag@digest`，minor 由 `CLICKHOUSE_MINOR` 固定；collector 不再寫死 digest，版本由 `EXPORTER_VERSION` 固定。collector 換版本的 PR 帶 `prBodyNotes` 說明。
- `scripts/nightly/renovate.test.ts` 新增：Renovate 讀映像的檔案裡每個 digest 都屬於讀得到的 pin；同一個映像與 tag 的每份 digest 相同；同一個映像只有一種寫法；ClickHouse 的 LTS 規則；collector 的說明；nightly 的 k3s、Istio 與主機的版本相同。原本的「Istio 版本有被讀到」測試寫死 1.30.5，每個 Istio PR 都會因此失敗，改成讀 workflow 裡的值。
- 文件：`docs/architecture/ci-release.md`「同一個 pin 的多份副本」、`deploy/chart/README.md` 與 `values.yaml` 的說明。

### 每個 pin

「完整」表示一個 Renovate PR 改完所有副本，CI 會過；「刻意擋下」表示 PR 會開，但 CI 失敗，直到有人完成 Renovate 做不到的部分。

| pin | 副本與讀取的 manager | PR |
| --- | --- | --- |
| `docker.io/clickhouse/clickhouse-server:26.8` | `deploy/chart/values.yaml`（chart values 自訂 manager）、`scripts/lib/stack.ts`、`services/ingest/tests/common/mod.rs`（`名稱:tag@digest` 自訂 manager，第二輪補 tag） | digest：完整（三個檔案一個分支，模擬 B 通過）。換 LTS minor：刻意擋下（`CLICKHOUSE_MINOR`；決策 14 的租戶隔離分析與 `services/ingest` 的內省測試是在 26.8 上做的） |
| `docker.io/otel/opentelemetry-collector-contrib:0.161.0` | `deploy/chart/values.yaml`（chart values）；e2e 從 values 讀，chart 測試不再寫 digest | digest：完整（模擬 E 通過）。換版本：刻意擋下（`EXPORTER_VERSION`；`0006_logs.sql` 照 v0.161.0 exporter 的表結構） |
| `gcr.io/distroless/cc-debian13:nonroot` | `crates/service-kit/tests/container.rs`、`verify_container.rs`、`scripts/lib/stack.ts`（`名稱:tag@digest`，`stack.ts` 第二輪補 tag）、`deploy/chart/tests/e2e/example-service/Containerfile`（dockerfile manager） | 完整：四個檔案、兩個 manager 同一個分支 |
| `docker.io/library/eclipse-mosquitto:2.1.2-alpine` | `crates/protocol/tests/broker.rs`、`scripts/lib/stack.ts`（第二輪兩份都補 tag；之前都讀不到，不會更新） | 完整：兩個檔案一個分支 |
| `docker.io/traefik/whoami:v1.12.0` | `deploy/chart/tests/chart.test.ts`、`deploy/chart/tests/e2e/values-e2e.yaml`、`deploy/chart/tests/values/smoke.yaml`（`名稱:tag@digest`） | 完整：三個檔案一個分支 |
| `docker.io/apache/age:release_PG18_1.8.0` | `scripts/lib/stack.ts`（第二輪補 tag；tag 不是版本號，只會有 digest 更新） | 完整（一份） |
| `docker.io/library/registry:3.1.2` 與 `:3` | `deploy/flux/tests/e2e/lab.e2e.ts`；`scripts/README.md`（第二輪起讀取） | 完整：兩個 tag 各自一個分支 |
| `docker.io/library/postgres:18.6-alpine` | `crates/service-kit/tests/support/start.rs` | 完整（一份） |
| `ghcr.io/cloudnative-pg/postgresql:18.6-minimal-trixie` | `deploy/images/postgres/Containerfile` 的兩個 `FROM`（dockerfile manager） | digest：完整（#391 改了兩行）。換 minor：`postgres-image` job 檢查伺服器標頭（既有，見該目錄的 README） |
| `docker.io/oven/bun:*-distroless`、`gcr.io/distroless/static-debian13:nonroot`、`docker.io/charmcli/soft-serve`、`docker.io/alpine/git` | 各一份（Containerfile 或 `lab.e2e.ts`） | 完整 |
| `rancher/k3s` | `.github/workflows/nightly.yml`（`名稱:tag@digest`）；主機的 `K3S_VERSION`、安裝程式網址與 SHA-256（`deploy/host/src/desired.ts`）Renovate 讀不到，SHA-256 也算不出來 | 刻意擋下（新測試；模擬 G）。之前不會失敗，nightly 會悄悄和主機不同版 |
| Istio（`ISTIO_VERSION`） | `nightly.yml`（`# renovate:` 註解）；`deploy/istio/README.md` 的四個 `helm install --version` 讀不到 | 刻意擋下（新測試；模擬 H）。lab 與 SaaS 主機要手動升級 |
| Flux | `.prototools` 的 `flux`；`gotk-components.yaml` 與 `kustomization.yaml` 的控制器 digest 由 `components.ts` 重寫 | 刻意擋下（既有，`bun test deploy/flux`） |
| Rust 工具鏈 | `.prototools` 的 `rust` 與 `rust-toolchain.toml` | 完整（同一個群組；`repo-check` 檢查兩者相同） |
| GitHub Actions | 三個 workflow 與本地 action 的每個 `uses:` | 完整（同一個群組；#393） |
| `@types/bun` | `scripts/` 與 `deploy/host/` 的 `package.json` | 完整（同一個群組） |
| Rust crate | 根目錄 `Cargo.toml` 的 `[workspace.dependencies]` 與 `Cargo.lock` | 完整（一份） |
| `.prototools` 的其他工具（bun、go、kubectl、helm、zig 等） | 各一份；測試裡出現的版本號是假路徑的範例，不是副本 | 完整 |
| 不由 Renovate 更新 | `GATEWAY_API_VERSION`、`CNPG_VERSION`、`PROTO_VERSION`、`AGE_VERSION`（`docs/architecture/ci-release.md` 的清單） | 手動 |

### Dry run（最終設定）

`renovate-config-validator --strict` 通過（[validator-final.txt](data/2026-10-13-69-renovate-config/validator-final.txt)）。`--dry-run=lookup` 的結果在 [lookup-final.tsv](data/2026-10-13-69-renovate-config/lookup-final.tsv)，依分支整理在 [branches-final.txt](data/2026-10-13-69-renovate-config/branches-final.txt)：

- ClickHouse：只有 `renovate/docker.io-clickhouse-clickhouse-server-26.8` 一個分支，3 個更新、3 個檔案（`deploy/chart/values.yaml`、`scripts/lib/stack.ts`、`services/ingest/tests/common/mod.rs`），都是 `26.8@110d3d1f8e1e → 26.8@dd81097e2c38`。第一輪的 `26.9@387ebbcca253` 不再出現。
- 以 Renovate 的 auto-replace 依序套用這個分支的三個更新（[autoreplace-clickhouse.txt](data/2026-10-13-69-renovate-config/autoreplace-clickhouse.txt)）：三個檔案各改一行 digest，沒有失敗。
- 沒有警告。

其他映像今天沒有新 digest，所以另外做一份「tag 已經移動」的快照（[make-stale.ts](data/2026-10-13-69-renovate-config/make-stale.ts)：distroless、Mosquitto、whoami、collector、registry、AGE 的 digest 改成假的舊值，所有副本一起改，改了哪些檔案見 [stale-files.txt](data/2026-10-13-69-renovate-config/stale-files.txt)），再跑一次 dry run（[lookup-stale.tsv](data/2026-10-13-69-renovate-config/lookup-stale.tsv)、[branches-stale.txt](data/2026-10-13-69-renovate-config/branches-stale.txt)）：

| 分支 | 更新 | 檔案 |
| --- | --- | --- |
| `renovate/gcr.io-distroless-cc-debian13-nonroot` | 4 | `example-service/Containerfile`（dockerfile）、`container.rs`、`verify_container.rs`、`stack.ts` |
| `renovate/docker.io-library-eclipse-mosquitto-2.1.2-alpine` | 2 | `broker.rs`、`stack.ts` |
| `renovate/docker.io-traefik-whoami-v1.12.0` | 3 | `chart.test.ts`、`values-e2e.yaml`、`smoke.yaml` |
| `renovate/docker.io-otel-opentelemetry-collector-contrib-0.161.0` | 1 | `values.yaml`（只換 digest；換版本的 `0.162.0` 是另一個分支） |
| `renovate/docker.io-library-registry-3.1.2`、`…-registry-3` | 1、1 | `lab.e2e.ts`；`scripts/README.md` |
| `renovate/docker.io-apache-age-release_PG18_1.8.0` | 1 | `stack.ts` |

這些分支以 auto-replace 套用的結果在 [autoreplace-stale.txt](data/2026-10-13-69-renovate-config/autoreplace-stale.txt)：10 個更新、9 個檔案，沒有失敗。

### 模擬每一種 PR 的 CI

[pr-simulation.ts](data/2026-10-13-69-renovate-config/pr-simulation.ts) 在建置主機的分支副本上依序套用每一種 PR 會做的改動，跑那個 PR 在 CI 會跑的 chart 測試（`bun test deploy/chart -t "bundled ClickHouse|OpenTelemetry collector"`）與 `bun test scripts/nightly/renovate.test.ts`，再還原。指令：`bash rcheck.sh 69r 'bun pr-simulation.tmp.ts'`（腳本暫時放在工作目錄，跑完刪除）；輸出：[pr-simulation.log](data/2026-10-13-69-renovate-config/pr-simulation.log)。

| 情境 | chart 測試 | renovate.test.ts | 結論 |
| --- | --- | --- | --- |
| A：本分支不改 | 26 pass | 24 pass | |
| B：ClickHouse 26.8 的 digest PR（三份） | 26 pass | 24 pass | 完整 |
| C：同一個更新只改 values.yaml（第一輪設定會開的 PR） | ClickHouse 測試失敗 | 「同一個映像與 tag 的 digest 相同」失敗 | 第二輪修正的問題 |
| D：換到 LTS 27.3（假 digest） | `CLICKHOUSE_MINOR` 失敗 | 24 pass | 刻意擋下 |
| E：collector 同一版本的新 digest | 26 pass | 24 pass | 完整 |
| F：collector 換到 0.162.0 | `EXPORTER_VERSION` 的兩個測試失敗 | collector 說明測試失敗 | 刻意擋下 |
| G：k3s 換到 v1.36.6-k3s1（只改 nightly.yml） | 26 pass | nightly 與主機版本測試失敗 | 刻意擋下 |
| H：Istio 換到 1.30.6（只改 nightly.yml） | 26 pass | nightly 與主機版本測試失敗 | 刻意擋下 |

H 第一次跑時，原本寫死 1.30.5 的「Istio 版本有被讀到」測試也失敗；改成讀 workflow 的值之後重跑，只剩刻意的那一個。

### 其他檢查

- `bun test scripts/nightly` 112 pass、0 fail；`bun run --cwd scripts typecheck` 通過。
- 把 `stack.ts` 的 ClickHouse 改回只寫 digest，新測試有兩個失敗（讀不到的 digest、副本不一致），還原後通過。
- 建置主機（rcheck，`CI=1`，容器測試以 docker 執行、不會略過）：`cargo fmt --all --check` 通過；`cargo clippy --locked -p protocol -p ingest -p service-kit --all-targets -- -D warnings` 通過；`cargo test --locked -p protocol --test broker` 2 passed（以 `eclipse-mosquitto:2.1.2-alpine@sha256:…` 啟動 broker）；`cargo test --locked -p ingest` 全部通過（以 `clickhouse-server:26.8@sha256:…` 啟動 ClickHouse）；`cargo test --locked -p repo-check` 通過；`bun test scripts` 660 pass、2 skip、0 fail；`bun run --cwd scripts typecheck` 通過；`bun test deploy/chart` 288 pass、0 fail；`bun scripts/ci/check-secret-handling.ts` 沒有違規。

## 第二條驗收條件：Renovate 開出的 PR 保持 SHA 與 digest 釘選

2026-10-13 從 Dashboard 提前要求的三個 PR，各代表一種更新，都已 squash 合併。證據：[renovate-prs.txt](data/2026-10-13-69-renovate-config/renovate-prs.txt)，比對上游的腳本 [renovate-prs-evidence.sh](data/2026-10-13-69-renovate-config/renovate-prs-evidence.sh)（列出每個合併 commit 改動的釘選行，再查上游 tag 當時解析到的 commit 或 digest）。

| PR | 更新種類 | 釘選 | 上游比對 | CI |
| --- | --- | --- | --- | --- |
| Laysi/trustiot-v2#393 | GitHub Actions | `actions/upload-artifact` 與 `actions/download-artifact` 各換成 40 字元 commit SHA，行尾版本註解跟著改（`# v7.0.2`、`# v8.0.2`） | 兩個 SHA 都與 tag 解析到的 commit 相同（MATCH） | ci run 38332732278 成功；沒有觸發 nightly（release.yml 不在它的路徑內） |
| Laysi/trustiot-v2#391 | 只換 digest | `deploy/images/postgres/Containerfile` 的兩個 `FROM` 都只換 digest（`37ade18d… → c9be7f22…`），tag `18.6-minimal-trixie` 不變 | digest 與 tag 當時解析到的相同（MATCH） | 在當時 main 的合併 commit 上 ci run 38333163021 成功（含 `postgres-image` job 建置並測試映像）；PR 的 nightly smoke run 38333162837 成功，兩份 values 都 61 passed、0 failed |
| Laysi/trustiot-v2#392 | tag 與 digest | identity double 的 `FROM docker.io/oven/bun:1.4.2-distroless@… → 1.4.3-distroless@sha256:1aaa5150…`，tag 與 digest 一起換 | digest 與 tag 當時解析到的相同（MATCH） | 在當時 main 的合併 commit 上 ci run 38334542624 成功；PR 的 nightly smoke run 38334542704 成功（以新基底建置 identity double），兩份 values 都 61 passed、0 failed |

這份證據涵蓋從 Dashboard 提前要求的三種更新。本分支的設定修正，處理的是 Dashboard 在第一次排程（2026-10-19）前顯示的問題，以及上面多份副本的情況；排程開出的 PR 要在之後各自檢查。
