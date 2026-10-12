# release.yml 改用 arm64 runner：逐 job 評估、x64 與 arm64 建置結果比對

對應 issue：Laysi/trustiot-v2#310。設計與規則見 [CI 與發佈](../architecture/ci-release.md) 的「release.yml」，尤其是「交叉編譯與可重現建置」。原始資料在 [data/2026-10-12-310-release-arm-runner/](data/2026-10-12-310-release-arm-runner/)。

## 環境

- **commit**：分支 `ci/310-release-arm-runner`，base 為 main 的 `ebbe88e`。建置用的是 build host 上這個分支當時的工作樹快照，加上 PR Laysi/trustiot-v2#307 head `e89bcf9` 的 `deploy/images/postgres/Containerfile`（blob `ed65e88`，逐位元組比對過），以固定時間 2026-10-12T00:00:00Z 提交成 `003e627`、打上本地 tag `v0.0.0-rc.1`。所以 `SOURCE_DATE_EPOCH` 是 1791763200，每次建置都從這同一個 commit 開始。
- **build host**：x86_64、WSL2（kernel 6.18）、20 核心、15 GiB，Docker 29.8.2（containerd image store）。所有建置都經 check queue 執行，每個容器限 11 GiB 記憶體、一次只跑一個。
- **arm64 執行**：build host 已有 aarch64 的 qemu-user binfmt handler（interpreter `/usr/bin/qemu-aarch64`，flags `POCF`）。它在本次工作之前就已註冊，不是本次安裝；本次沒有在 build host 安裝或變更任何東西。arm64 的部分都在 linux/arm64 容器內，以 user-mode 模擬執行。
- **模擬 runner 的容器**：`ubuntu:24.04`，index `sha256:534baea6…`，以各平台 manifest 的 digest 指定：amd64 `sha256:f610ab94…`，arm64 `sha256:08571ca1…`。容器內安裝的版本與 GitHub 兩種 runner 映像列出的相同：
  - Ubuntu 套件庫的 podman 4.9.3 與 skopeo 1.13.3；
  - rustup 1.29.1（官方 rustup-init，以 SHA-256 驗證）；
  - proto 0.60.2（`.github/actions/setup-proto` 的壓縮檔與 SHA-256）；
  - `.prototools` 釘選的 bun 1.4.2、helm 4.3.0、zig 0.16.0、cargo-zigbuild 0.23.4、cargo-auditable 0.7.6、syft 1.52.0、cosign 3.1.3。

  容器內實際的版本與每個執行檔的架構見 [versions-x64.txt](data/2026-10-12-310-release-arm-runner/versions-x64.txt) 與 [versions-arm64.txt](data/2026-10-12-310-release-arm-runner/versions-arm64.txt)。
- **runner 映像**：依 `actions/runner-images` 的軟體清單（2026-10-12 讀取），`ubuntu-24.04`（20261004.327.1）與 `ubuntu-24.04-arm`（20261004.142.1）都是 Ubuntu 24.04.5，podman、skopeo、buildah、jq、OpenSSL、GitHub CLI、rustup、Rust 的版本完全相同（[runner-images.txt](data/2026-10-12-310-release-arm-runner/runner-images.txt)）。arm64 映像原本由 Arm 維護（`actions/partner-runner-images`），2026 年起改由 GitHub 在 `actions/runner-images` 維護。
- **本機 registry**：`registry:3`（以 digest 釘選），只聽 `127.0.0.1:5168`，一次只開一個。簽章用本機模式當場產生的 cosign 金鑰，用完即刪。沒有推 tag、沒有建立 release、沒有觸發任何 workflow。

## 指令

評估腳本存在 data 目錄：[arm-eval.sh](data/2026-10-12-310-release-arm-runner/arm-eval.sh) 是主流程，[inner.sh](data/2026-10-12-310-release-arm-runner/inner.sh) 在容器內跑，另有 [rebuild.ts](data/2026-10-12-310-release-arm-runner/rebuild.ts)、[compare.ts](data/2026-10-12-310-release-arm-runner/compare.ts)、[elfdiff.ts](data/2026-10-12-310-release-arm-runner/elfdiff.ts)。腳本放在 build host 的 `~/tv2/agents/310-eval/`，在 worktree 根目錄執行：

```sh
# 第 1 輪：x64 與 arm64 容器各自從頭跑完整流程
# （plan.ts --dry-run、release-notes-action 的 notes.sh、build.ts、publish.ts 兩次、verify.ts、bun test scripts/release）
rcheck.sh 310 'bash ~/tv2/agents/310-eval/arm-eval.sh both'
# 第 2 輪：x64 在新 clone 只重跑 Cargo；arm64 沿用第 1 輪的 clone 與 target 再跑完整流程（RUN 改用 crun）
rcheck.sh 310 'bash ~/tv2/agents/310-eval/arm-eval.sh round2'
# 第 3 輪：arm64 在新 clone 只重跑 Cargo，然後比對
rcheck.sh 310 'bash ~/tv2/agents/310-eval/arm-eval.sh round3'
# 工具的 linux arm64 資產與 checksum 檔（本機）
bun docs/test-reports/data/2026-10-12-310-release-arm-runner/tool-assets.ts
# repo 的檢查（本機與 build host）
bun test scripts/release && bun test scripts/ci
bun scripts/ci/check-workflows.ts && bun scripts/ci/lint-workflows.ts && bun scripts/ci/check-secret-handling.ts
bun run --cwd scripts typecheck
```

每次執行的重點紀錄見 [runs.txt](data/2026-10-12-310-release-arm-runner/runs.txt)：階段時間、release 腳本的輸出、錯誤與測試結果。

## 結果

### 工具

`.proto/plugins/` 的 schema plugin 都不必改。每個 plugin 的檔名樣式解析出的 linux aarch64 資產都存在，release 也附有 checksum 檔（[tool-assets.txt](data/2026-10-12-310-release-arm-runner/tool-assets.txt)）：

- zig：`zig-aarch64-linux-0.16.0.tar.xz` 與 `.minisig`；
- cargo-zigbuild、cargo-auditable：`*-aarch64-unknown-linux-gnu.tar.xz` 與 `.sha256`；
- syft、cosign：`*_linux_arm64*`，各自在 checksums 檔中有對應的行；
- helm：`helm-v4.3.0-linux-arm64.tar.gz` 與 `.sha256sum`。

arm64 容器內 proto 對每個工具都印出 `Verifying checksum against …`，zig 是以 minisig 驗證，`file` 確認每個執行檔都是 ARM aarch64。

### 第 1 輪：x64 交叉編譯與 arm64 原生建置

| 項目 | x64 容器（現在的 runner） | arm64 容器（`ubuntu-24.04-arm`，模擬） |
| --- | --- | --- |
| plan（`plan.ts --dry-run`，含 `helm lint --strict` 兩種部署型態） | 通過 | 通過；`plan.json` 與 x64 逐位元組相同 |
| 發版說明（`notes.sh` 沒有設定模型，輸出 commit 清單） | 通過 | 通過；`notes.md` 相同 |
| Cargo（`identity`、`release-selftest`），從頭建置 | 43.6 秒 | 21 分 44 秒（模擬，非 runner 的速度） |
| `identity` 映像 | `sha256:8080c5f5…`，SBOM 213 個套件 | `sha256:a2fe0c51…`，SBOM 213 個套件 |
| `release-selftest` 映像 | `sha256:43f2cb6a…`，13 個套件 | 第 1 輪停在 postgres，第 2 輪：`sha256:43f2cb6a…`，**相同** |
| `postgres` 映像（PR Laysi/trustiot-v2#307 的 Containerfile） | `sha256:040a7fad…`，142 個套件 | 第一個 `RUN` 失敗：`Error during reexec(...): No such file or directory`，是模擬環境的限制，見下方 |
| publish（本機測試模式），再跑一次 | 推送、簽章、SBOM attestation、chart；重跑不重推，`published.json` 相同 | 第 2 輪：同左 |
| verify（本機測試模式） | 3 個映像與 chart 全部通過 | 第 2 輪：2 個映像與 chart 全部通過 |
| chart digest | `sha256:536de43f…` | `sha256:536de43f…`，**相同** |
| `bun test scripts/release` | 128 pass、0 fail | 128 pass、0 fail |

建置與驗證的輸出見 [build-x64.json](data/2026-10-12-310-release-arm-runner/build-x64.json)、[build-arm64.json](data/2026-10-12-310-release-arm-runner/build-arm64.json)、[published-x64.json](data/2026-10-12-310-release-arm-runner/published-x64.json)、[published-arm64.json](data/2026-10-12-310-release-arm-runner/published-arm64.json)、[verify-report-x64.md](data/2026-10-12-310-release-arm-runner/verify-report-x64.md)、[verify-report-arm64.md](data/2026-10-12-310-release-arm-runner/verify-report-arm64.md)，以及比對結果 [compare-x64-arm64.txt](data/2026-10-12-310-release-arm-runner/compare-x64-arm64.txt)。

### 模擬環境做不到的事

user-mode qemu 下，podman 無法執行 arm64 階段的 `RUN`，也無法啟動容器：

- chroot isolation 要重新執行 `/proc/self/exe`，但在 qemu-user 下那是 host 的 qemu，不在容器內，錯誤是 `Error during reexec(...)`；
- crun 要經由 memory file descriptor 重新執行自己，錯誤是 `crun: Failed to re-execute libcrun via memory file descriptor`。

在真正的 arm64 機器上沒有這兩個問題，而且這兩點與 runner 無關，只是驗證方式的限制。postgres 映像在 arm64 上原生建置的證據改取兩處：

- PR Laysi/trustiot-v2#307 的 CI 中，`postgres-image` job 在 `ubuntu-24.04-arm` runner 上以 podman 原生建置同一份 Containerfile 並啟動測試，結果 success（run 38155302001，job 114516981615，2026-10-11T16:24:24Z 至 16:25:15Z，以 GitHub MCP 的 check runs 讀取）；
- 同一 PR 在 lab arm64 機器上的原生建置與測試（見該 PR 的測試報告）。

`release-selftest` 在 arm64 上執行並印出 aarch64，是 2026-10-10 在 lab 驗證的（[Laysi/trustiot-v2#68 的測試報告](2026-10-10-68-release-workflow.md)）。這次想用 build host 的 Docker 載入 OCI 壓縮檔再執行，但 `docker load` 不接受 podman 推出的 oci-archive（`does not contain a manifest.json`），所以沒有再跑一次。

### 重建：同架構相同，跨架構只有服務執行檔不同

同一個 commit，每次都是新的 clone、新的 target 目錄，執行的是 build.ts 的 Cargo 指令（`rust-build.ts` 的 `rustCrossCompileCommand`），結果見 [binaries.txt](data/2026-10-12-310-release-arm-runner/binaries.txt)：

| 執行檔 | x64 第 1 次 | x64 第 2 次 | arm64 第 1 次 | arm64 第 2 次 |
| --- | --- | --- | --- | --- |
| `identity` | `eb73f00d…` | `eb73f00d…` | `fd166279…` | `fd166279…` |
| `release-selftest` | `752dea3c…` | `752dea3c…` | `752dea3c…` | `752dea3c…` |
| selftest 的 C 物件（zig cc 編譯） | `8b4bfebd…` | — | `8b4bfebd…` | — |

- **同一種架構重建，結果逐位元組相同**：x64 與 x64 相同，arm64 與 arm64 也相同。在此之前，`identity` 從頭重建是否相同沒有驗證過：Laysi/trustiot-v2#68 的重建共用 target 目錄，Laysi/trustiot-v2#308 的重建用的是 cargo 快取。
- **跨架構**：下列項目相同：
  - `release-selftest` 的執行檔、映像 digest，以及 SBOM（去掉每次隨機的 `documentNamespace` 與 `created` 後完全相同）；
  - zig 編譯的 C 物件，表示 zig 沒有依主機 CPU 選擇指令集；
  - chart digest；
  - `identity` 的 cargo-auditable 相依清單（`.dep-v0` section）與 SBOM 的套件清單（213 個，名稱與版本相同；不同的只有映像本身那一項，它的 checksum 就是映像 digest；[sbom-compare.txt](data/2026-10-12-310-release-arm-runner/sbom-compare.txt)）。

  `identity` 的執行檔則不同：x64 版 6,376,544 bytes，arm64 版 6,378,240 bytes。`.text`、`.rodata`、`.eh_frame` 等 section 都不同，`.rodata` 的大小相同，但字串的排列順序不同。執行檔裡沒有任何主機相關的字串，例如 `x86_64`、主機 triple、`/root/`、`/work/`（[elfdiff-identity.txt](data/2026-10-12-310-release-arm-runner/elfdiff-identity.txt)）。
- **原因**（[rlib-metadata.txt](data/2026-10-12-310-release-arm-runner/rlib-metadata.txt)）：兩次建置各有 186 個 rlib，其中 91 個的檔名不同。檔名裡的 hash 就是 Cargo 給 rustc 的 `-C metadata`。不同的都是本身或其相依有 build script 或 proc macro 的 crate，例如 serde、tokio、axum、ring、libc；Cargo 依主機編譯 build script 與 proc macro。沒有這類相依的 crate，例如 itoa、ryu、bytes，檔名相同，內容也大多相同。`-C metadata` 不同時，crate 的識別碼跟著不同，進而改變符號與程式碼的排列。小程式 `release-selftest` 剛好不受影響。這是依觀察推得的機制，沒有追進 Cargo 的原始碼。

### 逐 job 評估

| job | 在 arm64 上需要的東西 | 結論與證據 |
| --- | --- | --- |
| `plan` | checkout、setup-proto（bun、helm）、git、`plan.ts`（helm lint）、`Laysi/release-notes-action`（composite action，bash、git、jq、curl）、openssl | **可行，已改**：arm64 容器內 `plan.ts` 與 `notes.sh` 通過，產出與 x64 相同；jq、openssl 在 arm64 runner 映像內（[runner-images.txt](data/2026-10-12-310-release-arm-runner/runner-images.txt)） |
| `build` | setup-proto（bun、zig、cargo-zigbuild、cargo-auditable、syft）、runner 的 rustup、podman、skopeo | **可行，已改**：arm64 容器內以 build.ts 建出 `identity` 與 `release-selftest`，通過 aarch64 ELF 與靜態連結檢查，SBOM 列出 crate。有 `RUN` 的 Containerfile 映像在 arm64 runner 上原生建置，證據是 PR Laysi/trustiot-v2#307 的 `postgres-image` job（見上方）；模擬環境下無法執行 `RUN`。產出與 x64 的差異見上方「重建」 |
| `publish` | setup-proto（bun、cosign、helm）、skopeo、gh；keyless cosign 用 GitHub OIDC 與 Fulcio | **可行，已改**：arm64 容器內本機測試模式的推送、簽章、SBOM attestation、chart 推送與簽章、重跑都通過。keyless 簽章要 Actions 的 OIDC token，本機無法驗證；cosign 的 keyless 流程與 CPU 架構無關，Fulcio 憑證記錄的是 runner 類型（GitHub 代管），不記錄架構，所以簽章身分（`release.yml@refs/tags/<tag>`）不變。arm64 runner 上的實際發佈已由 `v0.0.0-rc.5` 證明，見下方「arm64 runner 上的 rc」 |
| `verify` | setup-proto（bun、cosign、helm）、skopeo | **可行，已改**：arm64 容器內本機測試模式的 verify 通過。`v0.0.0-rc.5` 的 verify 在 arm64 runner 上執行，停在「套件必須是私有」，與架構無關，見下方 |
| `deploy-pr` | setup-proto（bun）、GitHub API | **可行，已改**：純 TypeScript，沒有與架構相關的程式；`bun test scripts/release`（含 `deploy.test.ts`、`flux.test.ts`）在 arm64 容器內通過 |
| `report` | bash、jq、gh | **可行，已改**：兩種 runner 映像都有 jq 1.7 與 GitHub CLI 2.102.0 |
| `actions/checkout`、`upload-artifact`、`download-artifact` | JavaScript action | 與 runner 架構無關；ci.yml 與 nightly.yml 已在 arm64 上使用 checkout，從 Laysi/trustiot-v2#309 起 |

workflow 的變更：

- release.yml 每個 job 的 `runs-on` 都改為 `ubuntu-24.04-arm`；
- `scripts/ci/workflow-conditions.test.ts` 加上「release.yml 每個 job 都在 arm64 runner」的檢查；
- 「`RUN` 只在 `$BUILDPLATFORM` 階段」原本靠 x64 runner 沒有模擬器而自然擋下，改在 arm64 runner 後會失去這層保護，所以改由 plan 檢查（`scripts/release/containerfile.ts`，PR Laysi/trustiot-v2#307 的 Containerfile 通過）；
- `lib.test.ts` 裡「拒絕執行測試的 Bun 執行檔」的測試原本只在 x64 上有作用；ci.yml 從 Laysi/trustiot-v2#309 起在 arm64 上執行，這項測試等於沒有跑。現在 arm64 上改為確認 aarch64 的 Bun 被接受。

### arm64 runner 上的 rc：`v0.0.0-rc.5`

2026-10-12 19:07 UTC，coordinator 在本分支的 `716740a`（已合併 main `28862e1`）打上簽章 tag `v0.0.0-rc.5`，觸發 release.yml run 38295508885。GitHub 用 tag 指向的 commit 裡的 release.yml，所以 6 個 job 都指定 `ubuntu-24.04-arm`。實際執行的 plan、build、publish、verify、report 都在這個 runner（映像 20261004.142.1）上；deploy-pr 只在 verify 通過後執行，兩次都略過。每個 job 的結果、runner 映像與 `[release:*]` 輸出見 [rc5-run.txt](data/2026-10-12-310-release-arm-runner/rc5-run.txt)。

- **第 1 次執行**：
  - plan、build 成功。build 在 arm64 上原生建出三個映像，包括有 `RUN` 的 `postgres`。
  - publish 推送了 `identity`、`postgres`、`release-selftest`，三個都完成 keyless 簽章與 SBOM attestation，也推送了 chart。
  - 簽 chart 時，cosign 向 Actions 取 OIDC token，得到的回應不是 JSON（`fetching ambient OIDC credentials: invalid character 'u' looking for beginning of value`），publish 失敗。
  - 同一個 job 稍早已成功取得 6 次 token，所以判斷是 token 服務的暫時錯誤，與 runner 架構無關。
- **第 2 次執行**：依 [CI 與發佈](../architecture/ci-release.md) 的處理方式，只重跑失敗的 job（Re-run failed jobs）。
  - publish 沿用第 1 次 build 的產物，三個映像與 chart 的 digest 都與第 1 次相同，所以沒有重新推送，只再簽一次。
  - chart 完成簽章，建立了 GitHub 預發佈版本，附上三份 SBOM。
- **verify 失敗在第一項檢查**：`identity` 可以匿名拉取，而套件必須是私有。
  - 原因與在 x64 上發佈的 rc.4 相同：repo 公開之後新建的 GHCR 套件是公開的，與 runner 架構無關。
  - 使用者在 2026-10-13 決定 repo 公開期間不處理，轉回私有之後再檢查（Laysi/trustiot-v2#1）。
  - verify 停在這一項，沒有檢查簽章，所以改由下面的獨立驗證。

獨立驗證在 coordinator 的工作站上執行，用 `.prototools` 釘選的 cosign 3.1.3。憑證身分限定 `release.yml@refs/tags/v0.0.0-rc.5`，issuer 是 `https://token.actions.githubusercontent.com`。結果見 [rc5-cosign-verify.txt](data/2026-10-12-310-release-arm-runner/rc5-cosign-verify.txt)：

| 產出 | digest | `cosign verify` | `cosign verify-attestation --type spdxjson` |
| --- | --- | --- | --- |
| `identity` | `sha256:7a042972…` | 通過，2 個簽章 | 通過，2 個；SBOM 213 個套件 |
| `postgres` | `sha256:9b8b6b97…` | 通過，2 個簽章 | 通過，2 個；SBOM 142 個套件 |
| `release-selftest` | `sha256:bc2b33e0…` | 通過，2 個簽章 | 通過，2 個；SBOM 13 個套件 |
| chart `trustiot` | `sha256:1b96b299…` | 通過，1 個簽章 | chart 沒有 SBOM |

- 每個映像有兩個簽章與兩個 attestation，兩次執行各一個。chart 只有一個簽章，因為第 1 次在簽 chart 時就失敗了。
- SBOM 的套件數與本報告前面的本機建置相同（213、142、13）。
- `identity` 的 digest 不能和 rc.4 比較，因為兩者不是同一個 commit：rc.4 打在 main 的 `da6b4c8`。在模擬容器重建 `716740a` 再與 rc.5 比較，由 Laysi/trustiot-v2#372 追蹤。
- GHCR 套件狀態（2026-10-13，不帶認證向 `ghcr.io/token` 要 pull token）：`identity`、`postgres` 回 200，是公開的；`release-selftest`、`charts/trustiot` 回 401，是私有的。

### repo 的檢查

| 指令 | 結果 |
| --- | --- |
| `bun test scripts/release` | 本機 x86_64：通過（含新的 `containerfile.test.ts` 與 `plan.test.ts` 的新測試）；arm64 容器內（第 1、2 輪的快照）128 pass |
| `bun test scripts/ci` | 通過（含 `workflow-conditions.test.ts` 的 release.yml 測試） |
| `bun scripts/ci/check-workflows.ts`、`bun scripts/ci/lint-workflows.ts`、`bun scripts/ci/check-secret-handling.ts` | 通過 |
| `bun run --cwd scripts typecheck` | 通過 |

合併 main 之後的最終結果，寫在 PR 內文。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 評估結果逐 job 記錄能否改用 ARM 與理由 | **通過**，見上方「逐 job 評估」：6 個 job 都可行 |
| 可行的 job 已改用 `ubuntu-24.04-arm`，`bun test scripts/release`、`check-workflows.ts`、`lint-workflows.ts` 通過 | **通過**：全部 6 個 job 都已改，檢查通過 |
| 以 release 的本機流程證明，產出的映像、SBOM 與簽章流程與改動前相容 | **通過，有一點保留**：<br>• arm64 上的本機流程（plan、build、publish 兩次、verify）通過，映像只有 linux/arm64，SBOM 格式與套件清單相同，簽章流程相同，chart 與 `release-selftest` 的產出逐位元組相同。<br>• 保留：服務映像（`identity`）的 digest 與 x64 不同；同架構重建仍然逐位元組相同，所以「可重現」改為「在 arm64 上重建」，已寫進 [CI 與發佈](../architecture/ci-release.md)。<br>• arm64 runner 上的實際發佈（`v0.0.0-rc.5`）：build 原生建出三個映像，包括有 `RUN` 的 `postgres`；publish 完成 GHCR 推送、keyless 簽章、SBOM attestation、chart 簽章與 GitHub Release，簽章以 cosign 獨立驗證通過。verify 因套件公開而停在第一項檢查，與架構無關。見上方「arm64 runner 上的 rc」 |

後續建議：

- 以本報告的方式（arm64 模擬容器）重建 rc.5 的 commit `716740a`，與 rc.5 的 `identity` digest 比較。這可以證明模擬環境與真正 runner 的結果相同，同時檢查兩邊 podman 版本是否一致。由 Laysi/trustiot-v2#372 追蹤。
- 若要讓 x86 機器也能重現服務映像，得找出讓 Cargo 的 metadata hash 不受主機影響的方法，目前沒有穩定的做法，不在本次範圍。
