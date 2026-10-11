# release workflow：arm64 映像、keyless 簽章、SBOM 與 chart 發佈（2026-10-10）

對應 issue：Laysi/trustiot-v2#68。原始資料在 [data/2026-10-10-68-release-workflow/](data/2026-10-10-68-release-workflow/)。規則與設計見 [docs/architecture/ci-release.md](../architecture/ci-release.md) 的「release.yml」一節。

## 環境

- **commit**：`ee8c438`（分支 `ci/68-release-workflow`，base 為 main 的 `71be25d`）。本機的發佈測試、負向檢查與可重現建置都在這個 commit 上跑；rebase 之前跑過的交叉編譯檢查另外註明。之後分支九次合併 main（最後一次合併進 main 的 `a4bfe2b`），並加上「正式版要 main 的 ci 通過」的規則；第二次合併起，每次合併後都在合併的 commit 上重跑檢查，見「合併 main 之後」。
- **本機**：x86_64 的 Fedora 45（kernel 7.2.8）。podman 6.1.3、skopeo 1.24.1 來自系統；bun 1.4.2、helm 4.3.0、zig 0.16.0、cargo-zigbuild 0.23.4、cargo-auditable 0.7.6、syft 1.52.0、cosign 3.1.3 由 `.prototools` 鎖定版本、以 proto 安裝；Rust 1.99.0（`rust-toolchain.toml`）加上 `aarch64-unknown-linux-musl` target。詳見 [environment.txt](data/2026-10-10-68-release-workflow/environment.txt)。
- **本機 registry**：`docker.io/library/registry:3`（以 digest 釘選），只聽 `127.0.0.1:5068`。本機測試以當場產生的 cosign 金鑰對簽章（`--local-test-key`），不寫入 Sigstore：keyless 簽章需要 GitHub Actions 的 OIDC token，而且本機測試不該在公開紀錄留下任何東西。金鑰在測試的暫存目錄，測完刪除。
- **GitHub**：簽章過的 annotated tag `v0.0.0-rc.1` 指向 `ee8c438`，2026-10-10T12:39:36Z 推送，觸發 release.yml 的執行 [38052766358](https://github.com/Laysi/trustiot-v2/actions/runs/38052766358)，各 job 在 GitHub 代管的 `ubuntu-24.04`（x86_64）runner 上執行。repo 為私有，組織在 GitHub Free 方案。repo 沒有設定 `AI_MODEL` 變數，所以發版說明是 `Laysi/release-notes-action` 的 commit 主旨清單。合併 main 之後，以指向 `82ff556` 的 `v0.0.0-rc.2` 再跑一次（2026-10-10T14:28:17Z 推送，執行 [38059729095](https://github.com/Laysi/trustiot-v2/actions/runs/38059729095)）。第七、八次合併 main 之後，以指向 `c8b60a9` 的 `v0.0.0-rc.3` 再跑一次（2026-10-10T19:49:57Z 推送，執行 [38081354317](https://github.com/Laysi/trustiot-v2/actions/runs/38081354317)）。
- **結果的讀取方式**：本 repo 的 GitHub 讀寫走 GitHub MCP server，它的 token 讀不到 check run 與 Actions 日誌，所以 CI 的結果以 report job 設的 commit status `release`（MCP `pull_request_read` 的 `get_status`）與 GitHub Release 的內文（MCP `get_release_by_tag`）為準。
- **Sigstore 公開紀錄**：以 Rekor 的公開 API（`rekor.sigstore.dev`，v1）不帶任何憑證查詢。
- **lab 的 Mac mini M1**（`trustiot-mac-m1`）：Fedora Linux Asahi Remix 44、aarch64、podman 5.8.7，依 e2e 鎖的規則使用。

## 指令

本機發佈測試（[local-release.txt](data/2026-10-10-68-release-workflow/local-release.txt)），在 repo 根目錄以外的暫存目錄 `<work>` 執行：

```sh
proto install
rustup target add aarch64-unknown-linux-musl
podman run -d --rm --name release68-registry -p 127.0.0.1:5068:5000 \
  docker.io/library/registry:3@sha256:ddf754342cfc8acc51a56d5d0ab6af06826461864460636d8bd5c546dab2a7b8
cd <work>
export DOCKER_CONFIG=<work>/docker HELM_REGISTRY_CONFIG=<work>/helm-registry.json COSIGN_PASSWORD=
cosign generate-key-pair --output-key-prefix <work>/test
bun <repo>/scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --registry 127.0.0.1:5068/laysi/trustiot-v2 --output plan.json
CARGO_BUILD_JOBS=2 bun <repo>/scripts/release/build.ts --plan plan.json --out out
bun <repo>/scripts/release/publish.ts --plan plan.json --artifacts out --notes notes.md --output published.json --local-test-key <work>/test
# 同一個指令再跑一次（輸出到 published2.json），模擬重跑失敗的 publish job，兩份結果要相同
bun <repo>/scripts/release/verify.ts --plan plan.json --published published.json --output report.md --local-test-key <work>/test
```

可重現建置（[reproducibility.txt](data/2026-10-10-68-release-workflow/reproducibility.txt)）：以同一份 `plan.json` 再跑兩次 `build.ts`（`--out rebuild1`、`--out rebuild2`），比對三次的映像 digest，並檢查執行檔裡的原始碼路徑。

負向檢查（[local-negative.txt](data/2026-10-10-68-release-workflow/local-negative.txt)）：在上面的 registry 與工作目錄上，各自製造一種錯誤，確認對應的腳本拒絕，最後把被改動的 tag 放回去再驗證一次。

```sh
# 1. plan 的 sourceDateEpoch 加 60 秒後重建，得到內容不同的映像，推同一個版本 → publish.ts 要拒絕
# 2. 以 skopeo 把版本 tag 改指到 distroless 基底映像 → verify.ts 要拒絕
# 3. verify.ts 改用另一把金鑰驗證 → cosign 要拒絕
# 4. 設 GITHUB_ACTIONS=true 後使用 --local-test-key 或 --registry；不設時使用 keyless → 都要拒絕
# 5. GITHUB_ACTIONS=true 時，plan 指向 GHCR 以外的 registry → publish.ts 在推送前拒絕
# 5b. 本機測試模式下，plan 指向 GHCR → publish.ts 拒絕（本機測試不會占用正式版本的 tag）
# 6. plan 的 commit 不是目前 checkout 的 commit → build.ts 拒絕
# 7. published.json 的 chart 不是 plan 的 chart → verify.ts 拒絕
# 8. 放回正確的 tag 後重跑 verify.ts → 通過
```

repo 的檢查（[workspace-checks.txt](data/2026-10-10-68-release-workflow/workspace-checks.txt)、[unit-tests.txt](data/2026-10-10-68-release-workflow/unit-tests.txt)、[workflow-policy.txt](data/2026-10-10-68-release-workflow/workflow-policy.txt)、[typecheck.txt](data/2026-10-10-68-release-workflow/typecheck.txt)）：

```sh
CARGO_BUILD_JOBS=2 cargo fmt --all --check
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=2 cargo test --workspace --locked
bun test deploy/chart
bun test scripts/release
# Laysi/trustiot-v2#67 的 workflow 規則檢查與 TypeScript 型別檢查（該 PR 尚未合併，暫時複製過來執行）
bun scripts/ci/check-workflows.ts .
bun run --cwd scripts typecheck
```

合併 main 之後的檢查（[post-merge-checks.txt](data/2026-10-10-68-release-workflow/post-merge-checks.txt)）：在 `f10215a`（main 的 `a25efe9` 合併進來之後）跑 main 的 ci.yml 的每一項檢查，指令與 `CLAUDE.md` 的「Build, lint, and test」相同，再加上：

```sh
bun test scripts/release --reporter=junit --reporter-outfile=<work>/release-tests.junit.xml
bun scripts/release/plan.ts --tag v0.0.0-rc.99 --dry-run --output <work>/plan.json   # 只在本機規劃，不建 tag、不建置也不推送
helm package deploy/chart --version 0.0.0-rc.99 --app-version 0.0.0-rc.99 -d <work>
```

交叉編譯的檢查（[cross-compile-checks.txt](data/2026-10-10-68-release-workflow/cross-compile-checks.txt)）：

```sh
cargo auditable zigbuild --locked --release --target aarch64-unknown-linux-musl --package release-selftest
cargo build --locked --target aarch64-unknown-linux-musl --package release-selftest   # 沒有 C 編譯器，要停下並說明
CARGO_PROFILE_RELEASE_STRIP=none cargo zigbuild ...                                   # 再以 debuginfo 跑一次，readelf -S 看 .symtab
cargo auditable zigbuild --locked --release --target aarch64-unknown-linux-musl --package service-kit --bin example-service
```

workflow 的 shell 片段（[ci-glue-checks.txt](data/2026-10-10-68-release-workflow/ci-glue-checks.txt)）：從 `release.yml` 取出 report job 的 `run` 腳本，以記錄呼叫內容的假 `gh` 執行，模擬成功與各 job 失敗；`setup-proto` 的 Rust 工具鏈檢查，對釘選的版本與另一個版本各跑一次。

GitHub 上的發佈測試：

```sh
git tag -s v0.0.0-rc.1 -m "v0.0.0-rc.1: release pipeline test for Laysi/trustiot-v2#68 (branch ci/68-release-workflow)" ee8c438
git push origin v0.0.0-rc.1
# 結束後以 GitHub MCP 讀取：pull_request_read get_status（PR Laysi/trustiot-v2#243 的 head 就是 ee8c438）、get_release_by_tag v0.0.0-rc.1
# 合併 main 之後，對當時的 head 再跑一次，讀取方式相同
git tag -s v0.0.0-rc.2 -m "v0.0.0-rc.2: release pipeline test for Laysi/trustiot-v2#68 (branch ci/68-release-workflow, after merging main and adding the ci-on-main rule)" 82ff556
git push origin v0.0.0-rc.2
# 第七、八次合併 main（nightly、Renovate、arm64 的 setup-proto 等）之後，對當時的 head 再跑一次
git tag -s v0.0.0-rc.3 -m "v0.0.0-rc.3: release pipeline test for Laysi/trustiot-v2#68 (branch ci/68-release-workflow, after merging main's nightly and arm64 setup-proto)" c8b60a9
git push origin v0.0.0-rc.3
```

公開紀錄的查詢（[rekor-public-entries.txt](data/2026-10-10-68-release-workflow/rekor-public-entries.txt)）：

```sh
curl -s -X POST https://rekor.sigstore.dev/api/v1/index/retrieve -H 'Content-Type: application/json' \
  -d '{"hash":"sha256:7589584b37823a2ee9226f78e18c337d5a4dd93b18df348888def53fc252b126"}'
curl -s https://rekor.sigstore.dev/api/v1/log/entries/<uuid>
# body 以 base64 解開後看 spec 的欄位；憑證以 openssl x509 -text 或任何 X.509 解析器看 Fulcio 的擴充欄位
```

## 結果

### 本機

| 項目 | 結果 |
| --- | --- |
| `bun test scripts/release` | 82 項通過，0 項失敗 |
| `cargo fmt`、`cargo clippy -D warnings` | 通過 |
| `cargo test --workspace` | 23 組，194 項通過，0 項失敗（含 `release-selftest` 的 4 項） |
| `bun test deploy/chart` | 91 項通過，0 項失敗 |
| workflow 規則（Laysi/trustiot-v2#67 的檢查） | 2 個檔案（`release.yml`、`setup-proto`），沒有違規 |
| TypeScript 型別檢查（Laysi/trustiot-v2#67 的設定） | `scripts/release/` 沒有錯誤；`scripts/lib/mqtt.ts:121`、`scripts/lib/pgwire.ts:55` 各有一個錯誤，來自已在 main 的 Laysi/trustiot-v2#71，Laysi/trustiot-v2#67 的分支已修正 |
| plan | 兩份 values 的 `helm lint --strict` 通過；版本 `0.0.0-rc.1`（prerelease）；1 個映像與 chart |
| build | `release-selftest` → `sha256:182467660b…`，SBOM 為 SPDX-2.3，13 個套件（[清單](data/2026-10-10-68-release-workflow/local-sbom-packages.txt)：distroless 的 Debian 套件、Rust crate 與映像本身） |
| publish | 映像推送、簽章、附 SBOM attestation；chart 推送（`sha256:802286856c…`）並簽章 |
| 重跑 publish | 已推送的映像與 chart 不再推送，digest 相同，簽章與 attestation 重新附上 |
| verify | 映像：tag 指向推送的 digest、linux/arm64、簽章與 SBOM attestation 通過；chart：簽章通過，拉下的檔案就是發佈的檔案，version 與 appVersion 正確 |
| 負向檢查 1–8（含 5b） | 全部依預期拒絕，錯誤訊息見原始資料；放回 tag 後再驗證通過 |
| 可重現建置 | 三次建置的映像 digest 都是 `sha256:182467660b…`；執行檔的原始碼路徑只有 `/cargo/...`，沒有 `/home` |

交叉編譯的檢查：

- 以 release 的工具鏈建置 `release-selftest` 成功，是 aarch64 的靜態執行檔；build script 的檢查通過，表示 cargo-zigbuild 設定了目標的 C 編譯器（`CC_aarch64_unknown_linux_musl`）。不經 cargo-zigbuild、沒有目標 C 編譯器的建置在 build script 停下，訊息說明要用 `cargo zigbuild` 或設定哪個變數（exit 101），不會用主機的 C 編譯器編出錯誤架構的物件。
- `service-kit` 的 `example-service`（axum、tokio、sqlx、opentelemetry-otlp 等 186 個 crate）以同一套工具鏈交叉編譯成功，360 秒，產出 3,292,856 bytes 的 aarch64 靜態執行檔。這是 rebase 前跑的；之後的服務映像會走同一條路。
- 發現的問題：workspace 的 release profile 設 `strip = "debuginfo"`，用意是保留符號名稱給 panic 與 Sentry 的 stack trace，但經 zig 連結後 `.symtab` 整個不見了（`strip = "none"` 時 4,005,440 bytes、有 `.symtab`；`debuginfo` 時 376,848 bytes、沒有）。本 PR 不改，列為後續事項。

workflow 的 shell 片段：report job 在成功時設 `release` 為 success（「v0.0.0-rc.1 published and verified」），並把 GitHub Release 裡舊的「驗證結果」換成新的；plan 失敗時描述是「plan failed: 原因」並截到 140 字元；沒有錯誤訊息的失敗寫「build failed」，取消寫「publish cancelled」；失敗時不改 GitHub Release。`setup-proto` 的工具鏈檢查以 `<channel>-<host triple>` 精確比對 rustup 選到的工具鏈名稱：相符時通過（後面附的原因說明不影響）；版本不同、host 不同或只有前綴相同時報錯。

### GitHub：v0.0.0-rc.1

執行 [38052766358](https://github.com/Laysi/trustiot-v2/actions/runs/38052766358) 從推 tag（12:39:36Z）到設定 commit status（12:42:59Z）約 3 分 23 秒，五個 job 都成功。

| 項目 | 結果 |
| --- | --- |
| commit status `release`（`ee8c438`） | success，「v0.0.0-rc.1 published and verified」，2026-10-10T12:42:59Z（[github-rc1-status.json](data/2026-10-10-68-release-workflow/github-rc1-status.json)） |
| GitHub Release | `v0.0.0-rc.1`，prerelease，不是 Latest，2026-10-10T12:42:22Z 由 `github-actions[bot]` 建立，內文含發版說明、digest 表、驗證指令與 verify job 的驗證結果（[github-rc1-release.txt](data/2026-10-10-68-release-workflow/github-rc1-release.txt)） |
| 映像 | `ghcr.io/laysi/trustiot-v2/release-selftest:0.0.0-rc.1` → `sha256:7589584b37823a2ee9226f78e18c337d5a4dd93b18df348888def53fc252b126`，只有 linux/arm64 |
| 映像的簽章與 SBOM | verify job 以 `https://github.com/Laysi/trustiot-v2/.github/workflows/release.yml@refs/tags/v0.0.0-rc.1` 的身分驗證 cosign 簽章通過；SBOM attestation 驗證通過，取出的 SBOM 是 SPDX-2.3、13 個套件 |
| chart | `ghcr.io/laysi/trustiot-v2/charts/trustiot:0.0.0-rc.1` → `sha256:802286856c7fc42ae924ed6293e107c847f75b3a20a77a7a7351a146ebf9c481`；簽章驗證通過；拉下的檔案就是發佈的檔案；version 與 appVersion 都是 `0.0.0-rc.1` |
| 私有 | 匿名拉取映像與 chart 都被拒 |
| 對照 | 以另一個身分（`refs/tags/v0.0.0-rc.1-not-this-release`）驗證映像被拒，證明上面的驗證不是空檢查 |
| `release` environment | publish job 宣告 environment 後照常執行，沒有等待任何核准（見結論） |

SBOM 的下載：verify job 以唯讀權限從 GHCR 取出 SBOM attestation 並解析（上表），這是有讀取權限的使用者會走的路。GitHub Release 的附件 `release-selftest-0.0.0-rc.1.spdx.json` 由 publish job 以 `gh release create` 一併上傳，上傳失敗會讓 publish 失敗；MCP 讀 release 的回應不含附件清單，所以附件是由 publish 成功推得，可在 [Release 頁面](https://github.com/Laysi/trustiot-v2/releases/tag/v0.0.0-rc.1) 直接確認。

本機與 CI 的 digest：chart 的 digest 兩邊相同（`sha256:802286856c…`），因為 helm 由 proto 釘選、時間戳固定。映像的 digest 不同（本機 `sha256:182467660b…`、CI `sha256:7589584b…`）。映像由系統的 podman 組裝，本機是 6.1.3，runner 是 Ubuntu 24.04 的套件版本，不由本 repo 釘選；映像 config 的 history 記錄的是 podman（buildah）自己產生的指令字串（本機寫成 `COPY file:… in /usr/local/bin/release-selftest |force-mtime=…`），不同版本的寫法與 gzip 壓縮結果都可能不同。CI 的映像是私有的，本機沒有 GHCR 讀取憑證，所以沒有逐層比對，也沒有確認兩邊的執行檔是否相同。ci-release.md 只承諾同一組工具重建得到相同 digest，與這個結果一致。

### Sigstore 公開紀錄

以 digest 不帶憑證查 Rekor：映像有 2 筆（12:42:03Z 的簽章、12:42:07Z 的 SBOM attestation），chart 有 1 筆（12:42:16Z），都是 `dsse` 類型。

- 每筆紀錄只存 `envelopeHash`、`payloadHash` 與 `signatures`（簽章與憑證），沒有存 attestation 的內容，所以 SBOM 不會公開。
- 憑證公開了：repo `https://github.com/Laysi/trustiot-v2`（數字 ID `1411578173`）、組織 `https://github.com/Laysi`（`64061142`）、workflow 路徑與 tag、commit `ee8c4388…`、觸發事件 `push`、`github-hosted` runner、Actions 執行的網址，以及「簽章時 repo 是 private」。
- 憑證效期 10 分鐘（例如 12:42:02 到 12:52:02），簽章後就失效，沒有需要保管的金鑰。

### lab 的 Mac mini M1（arm64 實機）

本機建出的 OCI 壓縮檔（manifest `sha256:182467660b…`）在 lab 的 arm64 主機上以 podman 執行（[lab-arm64-run.txt](data/2026-10-10-68-release-workflow/lab-arm64-run.txt)）：

- 2026-10-10T13:20:54Z 取得 lab 的 e2e 鎖（之前由 Laysi/trustiot-v2#62 持有，每 3 分鐘重試），13:21:04Z 釋出。
- 主機：Fedora Linux Asahi Remix 44（kernel 7.1.13，aarch64），podman 5.8.7。映像載入在 `~/e2e-68` 下的暫用 podman storage，跑完連同目錄刪除。
- 載入後的 manifest digest 與本機建置相同，平台 linux/arm64，以 UID 65532 執行。
- 以 `podman run --rm --network none --read-only --cap-drop all --security-opt no-new-privileges` 執行，exit 0，輸出 `{"arch":"aarch64","c_object":"ok","c_pointer_bits":64,...,"version":"0.0.0-rc.1"}`：執行檔在真的 arm64 上會跑，zig 編譯的 C 物件與 Rust 對指標寬度的看法一致，版本是 release 流程蓋上的。

限制：GHCR 上的映像是私有的，lab 主機沒有讀取憑證，所以跑的是本機以同一條流程建出的映像，不是 CI 推送的那一個（兩者 digest 不同的原因見上）。這一項不在驗收條件內，是額外的實機確認。

### 合併 main 之後

2026-10-10T13:54–14:01Z 在 `f10215a` 上跑（[post-merge-checks.txt](data/2026-10-10-68-release-workflow/post-merge-checks.txt)）：

| 項目 | 結果 |
| --- | --- |
| `cargo fmt`；`cargo clippy -D warnings`（workspace，以及 service-kit 關閉預設 feature） | 通過 |
| `cargo test --workspace --locked` | 第一次有 1 項失敗：`crates/service-kit/tests/container.rs` 要 `/readyz` 在 `podman stop` 開始後 1 秒內回 503，當時工作站的負載約 18（8 核）。本分支沒有改 `crates/`，這段程式與 main 相同。同一份程式重跑：23 組，194 項通過，0 項失敗 |
| `cargo test -p service-kit --no-default-features` | 通過，上面那項容器測試也通過 |
| `cargo deny --locked check` | 通過，targets 含映像實際的 `aarch64-unknown-linux-musl` |
| gitleaks（`f10215a` 能走到的 36 個 commit） | 沒有發現 |
| workflow 規則（`scripts/ci/check-workflows.ts`） | 3 個檔案，沒有違規 |
| `scripts/` 的型別檢查與測試 | 型別檢查通過；176 項中 175 項通過，1 項略過（要設 `TRUSTIOT_DEV_E2E=1` 的 podman 端對端測試） |
| `bun test deploy/chart` | 133 項通過，0 項失敗 |
| `bun test scripts/release` | 90 項通過，0 項失敗；其中 11 項測正式版規則：在 main 上、main 的 ci 為該 commit 跑的最新一次 push 執行成功；沒有執行、還在跑、失敗或讀不到時拒絕；候選版與 dry run 不查 ci |
| `plan.ts --dry-run`、`helm package` | 兩份 values 的 `helm lint --strict` 通過；合併進來的 ingress 範本照常打包，`tests/`（含 e2e）不進 chart |

之後 main 繼續前進，每次合併後在合併的 commit 上重跑同一組檢查（原始資料同上）。時間是 UTC，沒寫日期的都是 2026-10-10：

| 合併 | 合併進來的 main | 衝突 | 結果 |
| --- | --- | --- | --- |
| `5f8d02e`（14:05–14:15Z） | `9132855`：transactional outbox（Laysi/trustiot-v2#54） | `Cargo.lock`：取 main 的 lock 再加上 `release-selftest`，所以自測程式的建置相依 `cc` 從 1.5.1 變成 main 的 1.7.0 | `cargo test --workspace` 30 組、223 項通過；gitleaks 掃 39 個 commit 沒有發現；其餘同上表。以 `plan.ts --dry-run` 與 `build.ts` 重跑發佈的建置：`cc` 1.7.0 經 zig 編譯 C 物件成功，執行檔是 aarch64 ELF，映像只有 linux/arm64，SBOM 13 個套件。lab 的 arm64 實機執行用的是 `cc` 1.5.1 的建置，沒有重跑 |
| `08408dc`（14:16–14:23Z） | `ece0566`：從程式碼產生 OpenAPI 規格（Laysi/trustiot-v2#55） | `tools/README.md`、`repository-layout.md` 的工具清單 | `cargo test --workspace` 34 組、240 項通過；main 在 ci.yml 新加的「OpenAPI spec is current」（`cargo run --locked -p openapi` 之後 `api/` 沒有差異）通過；gitleaks 掃 42 個 commit 沒有發現；其餘同上表 |
| `81c5072`（14:35–14:42Z） | `6451abd`：protocol crate（Laysi/trustiot-v2#50）、actionlint（Laysi/trustiot-v2#251）、Laysi/trustiot-v2#62 的獨立 e2e 報告 | `.prototools`、`CLAUDE.md`、`Cargo.toml`、`ci-release.md` 都是兩邊各加一項，全部保留 | `cargo test --workspace` 45 組、338 項通過；main 在 ci.yml 新加的 actionlint（`bun scripts/ci/lint-workflows.ts`）檢查 `ci.yml` 與 `release.yml`，沒有錯誤；`scripts/` 186 項中 185 項通過、1 項略過；其餘同上表 |
| `bc5b0d7`（18:04–18:11Z） | `8931a63`：ingest 的 ClickHouse schema 與 chart 內建的 ClickHouse（Laysi/trustiot-v2#59） | 無 | `cargo test --workspace` 49 組、348 項通過；`bun test deploy/chart` 149 項通過；actionlint 沒有錯誤；OpenAPI 檢查通過；gitleaks 掃 50 個 commit 沒有發現；`plan.ts --dry-run` 兩份 values 的 `helm lint --strict` 通過，chart 打包含新的 ClickHouse 範本、不含 `tests/`；其餘同上表 |
| `1dde1e8`（18:33–19:25Z） | `4d129f2`：nightly workflow、Renovate、setup-proto 支援 arm64 runner（Laysi/trustiot-v2#247） | `.prototools`、`CLAUDE.md`、`ci-release.md`、`scripts/README.md`，兩邊都保留。setup-proto 沒有衝突，人工確認同時保留 main 的 x64 與 arm64 兩組壓縮檔與雜湊，以及本分支的工具鏈精確比對。依 main 的 Renovate 規則，發佈工具在 `.prototools` 加上 `# renovate:` 註解；zig 的上游在 Codeberg（GitHub 上的 repo 在 0.15.2 之後沒有 tag），以 `git-tags` 讀，所以 `scripts/nightly/renovate.test.ts` 改成也接受以完整網址命名的非 GitHub 上游 | `cargo test --workspace` 49 組、348 項通過；`scripts/` 239 項中 238 項通過、1 項略過，含 Renovate 的兩項 `.prototools` 檢查，另以 6 種錯誤的註解確認檢查會失敗；workflow 規則 4 個檔案、actionlint 3 個 workflow（含 `nightly.yml`）沒有錯誤；不經 runner 檢查 setup-proto 在兩種架構的行為：`RUNNER_ARCH` 為 X64、ARM64 時選對壓縮檔與雜湊，其他值失敗；兩組雜湊與 proto 0.60.2 的壓縮檔相符；工具鏈檢查在 aarch64 上接受釘選的版本、拒絕 x86_64 的工具鏈；其餘同上表 |
| `c8b60a9`（19:26–19:48Z） | `404817d`：mesh 路徑正規化與入口加固（Laysi/trustiot-v2#258）、服務間授權與預設拒絕的 NetworkPolicy（Laysi/trustiot-v2#256）、ClickHouse 租戶隔離的獨立驗證（Laysi/trustiot-v2#264） | 無 | `cargo test --workspace` 49 組、348 項通過；`scripts/` 244 項中 243 項通過、1 項略過；`bun test deploy/chart` 在負載約 33 時，main 新加的一項測試（兩次 `helm template`）超過 bun 預設的 5 秒上限而失敗，其餘 173 項通過，以 30 秒上限重跑 174 項全部通過（下一列負載較低時，預設上限也通過）；gitleaks 掃 57 個 commit 沒有發現；`plan.ts --dry-run` 兩份 values 的 `helm lint --strict` 通過，chart 打包含新的 mesh 範本、不含 `tests/`；其餘同上表 |
| `ce5afb2`（2026-10-11 02:51–03:05Z） | `a4bfe2b`：protocol 的 fuzz 與對抗測試（Laysi/trustiot-v2#266）、入口 e2e 的通道復原（Laysi/trustiot-v2#267） | 無。發佈流程用到的檔案（workflow 與 action、`.prototools`、`Cargo.lock`、`scripts/release`、`deploy/images`、chart 的範本與 values 等）都沒有改，所以沒有再打 tag | `cargo test --workspace` 52 組、367 項通過（含 main 新加的 protocol 測試）；`scripts/` 的型別檢查（現在也涵蓋 `deploy/chart/tests/`）通過，244 項中 243 項通過、1 項略過；`bun test deploy/chart` 196 項通過；gitleaks 掃 60 個 commit 沒有發現；`plan.ts --dry-run` 通過；其餘同上表 |

### GitHub：v0.0.0-rc.2（合併 main 之後）

簽章過的 annotated tag `v0.0.0-rc.2` 指向分支當時的 head `82ff556`（四次合併 main 之後，含「正式版要 main 的 ci 通過」的規則、合併後的 setup-proto 與 `cc` 1.7.0），2026-10-10T14:28:17Z 推送。執行 [38059729095](https://github.com/Laysi/trustiot-v2/actions/runs/38059729095) 在 14:31:32Z 設好 commit status，約 3 分 15 秒，五個 job 都成功。

| 項目 | 結果 |
| --- | --- |
| commit status `release`（`82ff556`） | success，「v0.0.0-rc.2 published and verified」，14:31:32Z（[github-rc2-status.json](data/2026-10-10-68-release-workflow/github-rc2-status.json)） |
| GitHub Release | `v0.0.0-rc.2`，prerelease，14:30:58Z 由 `github-actions[bot]` 建立，內文含發版說明、digest 表、驗證指令與驗證結果（[github-rc2-release.txt](data/2026-10-10-68-release-workflow/github-rc2-release.txt)）；發版說明列出 `v0.0.0-rc.1` 以來的 commit，也包含合併進來的 main commit |
| 映像 | `ghcr.io/laysi/trustiot-v2/release-selftest:0.0.0-rc.2` → `sha256:e37a77d34c761c4118c8867504bcd29997f3b647d703f2713bf6fc97bace1fa5`，只有 linux/arm64；簽章與 SBOM attestation 驗證通過，SBOM 是 SPDX-2.3、13 個套件 |
| chart | `ghcr.io/laysi/trustiot-v2/charts/trustiot:0.0.0-rc.2` → `sha256:5e628d7fe41dca5a51e6759140bae85d905e09e3cf47dd20c0f052e7f6f2bf95`；簽章驗證通過；拉下的檔案就是發佈的檔案；version 與 appVersion 都是 `0.0.0-rc.2` |
| 私有、對照 | 匿名拉取映像與 chart 都被拒；以另一個身分（`refs/tags/v0.0.0-rc.2-not-this-release`）驗證映像被拒 |
| Sigstore 公開紀錄 | 3 筆 `dsse` 紀錄（映像的簽章 14:30:39Z、SBOM attestation 14:30:44Z、chart 14:30:52Z），同樣只存 `envelopeHash`、`payloadHash` 與 `signatures`（[rekor-rc2-entries.txt](data/2026-10-10-68-release-workflow/rekor-rc2-entries.txt)） |

候選版不檢查 main 的 ci，所以這次執行沒有呼叫讀取 ci 結果的 GitHub API；它證明的是 plan job 加上 `actions: read` 與 `GITHUB_TOKEN`、setup-proto 合併、`cc` 改成 1.7.0、chart 加入 ingress 範本之後，整條發佈照常運作。

### GitHub：v0.0.0-rc.3（合併 main 的 nightly 與 arm64 setup-proto 之後）

簽章過的 annotated tag `v0.0.0-rc.3` 指向分支當時的 head `c8b60a9`（第七、八次合併 main 之後：setup-proto 同時支援 x64 與 arm64 runner、`.prototools` 的 Renovate 註解，以及 main 的 nightly 與 mesh 範本），2026-10-10T19:49:57Z 推送。執行 [38081354317](https://github.com/Laysi/trustiot-v2/actions/runs/38081354317) 在 19:53:30Z 設好 commit status，約 3 分 33 秒，五個 job 都成功。各 job 仍在 x64 的 `ubuntu-24.04` runner 上執行，以合併後 setup-proto 的 X64 分支（x64 的 proto 壓縮檔與雜湊）安裝工具；Rust 由 build.ts 以 runner 的 rustup 安裝，setup-proto 的工具鏈檢查只在 ci.yml 的 `rust` job 執行。

| 項目 | 結果 |
| --- | --- |
| commit status `release`（`c8b60a9`） | success，「v0.0.0-rc.3 published and verified」，19:53:30Z（[github-rc3-status.json](data/2026-10-10-68-release-workflow/github-rc3-status.json)） |
| GitHub Release | `v0.0.0-rc.3`，prerelease，19:52:53Z 由 `github-actions[bot]` 建立，內文含發版說明、digest 表、驗證指令與驗證結果（[github-rc3-release.txt](data/2026-10-10-68-release-workflow/github-rc3-release.txt)）；發版說明列出 `v0.0.0-rc.2` 以來的 commit，`ci(...)` 與 `chore(...)` 類型的不列（main 的 #247、#252 與本分支的合併 commit） |
| 映像 | `ghcr.io/laysi/trustiot-v2/release-selftest:0.0.0-rc.3` → `sha256:c96b04252b67bed7a91535d8e71dbb0eabcb11d1b67a76877c7d8b23bee0eff8`，只有 linux/arm64；簽章與 SBOM attestation 驗證通過，取出的 SBOM 是 SPDX-2.3、13 個套件 |
| chart | `ghcr.io/laysi/trustiot-v2/charts/trustiot:0.0.0-rc.3` → `sha256:437c93e5d5cb155c6d757ae339ed0d1bbe578e2ae3460cb13840b5f36271f266`；簽章驗證通過；拉下的檔案就是發佈的檔案；version 與 appVersion 都是 `0.0.0-rc.3` |
| 私有、對照 | 匿名拉取映像與 chart 都被拒；以另一個身分（`refs/tags/v0.0.0-rc.3-not-this-release`）驗證映像被拒 |
| Sigstore 公開紀錄 | 3 筆 `dsse` 紀錄（logIndex 3192321025 映像的簽章 19:52:27Z、3192323432 SBOM attestation 19:52:35Z、3192326817 chart 19:52:46Z），同樣只存 `envelopeHash`、`payloadHash` 與 `signatures`；憑證的 Build Signer Digest 與 Source Repository Digest 都是 `c8b60a9`（[rekor-rc3-entries.txt](data/2026-10-10-68-release-workflow/rekor-rc3-entries.txt)） |

之後的第九次合併（`ce5afb2`）沒有改到發佈流程用的任何檔案（見「合併 main 之後」），所以沒有再打 tag。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 推一個 rc tag 後，GHCR 出現 arm64 映像與 chart，`cosign verify` 通過、SBOM 可下載 | **通過**。`v0.0.0-rc.1` 推送後約 3 分鐘，GHCR 出現只有 linux/arm64 的 `release-selftest` 映像與 `trustiot` chart；兩者以 release workflow 的身分驗證 cosign 簽章通過，另一個身分被拒；SBOM 以簽章過的 attestation 從 GHCR 取出（SPDX-2.3，13 個套件），也由 publish job 附在 GitHub Release（見「SBOM 的下載」）。verify job 的結果寫在 Release 內文，commit status `release` 為 success。合併 main 之後以 `v0.0.0-rc.2`、`v0.0.0-rc.3` 各再跑一次，結果相同（見「GitHub：v0.0.0-rc.2」「GitHub：v0.0.0-rc.3」） |
| workflow 權限最小化，沒有長期雲端金鑰 | **通過**。`release.yml` 的預設權限是 `{}`，每個 job 只列自己需要的權限（見 ci-release.md 的表），執行第三方建置程式碼的 build job 只有 `contents: read`；Laysi/trustiot-v2#67 的規則檢查沒有違規，第三方 action 都釘 commit SHA。GHCR 以各 job 的 `GITHUB_TOKEN` 登入，cosign 以 GitHub OIDC 取得效期 10 分鐘的憑證，沒有任何雲端金鑰或簽章金鑰。workflow 唯一引用的 secret 是給發版說明模型用的 `AI_API_KEY`（選用，目前沒有設定），只交給 plan job 裡產生發版說明的那一步 |

範圍內但無法在目前方案落實的一項：issue 的範圍寫「發佈走 GitHub environment 加人工核准」。publish job 已在 `release` environment 執行，但本 repo 是私有庫、組織在 GitHub Free 方案，不能為私有庫設定 environment，必要審查者在私有庫更要 GitHub Enterprise；v0.0.0-rc.1 的 publish 沒有經過任何核准就執行了。2026-10-10 審查決定暫不升級方案、接受這個現況，以補償措施把關，其中「正式版 tag 指向的 commit 在 main 的 ci 必須通過」是這次依決定新增的；公開透明紀錄也在同一次審查決定接受。兩者的理由與重新檢視的條件見 ci-release.md 的「公開的透明紀錄」「發佈核准」與決策 30。

其他後續事項：

- 第一個服務映像加入後刪除 `release-selftest`（ci-release.md 已註明）。
- release profile 的 `strip = "debuginfo"` 經 zig 連結後沒有符號表（見上）。

2026-10-10 合併 main（Laysi/trustiot-v2#67 的 ci.yml）時已處理的事項：`.github/actions/setup-proto/action.yml` 保留一份，含工具鏈檢查；`deny.toml` 的 targets 加上 `aarch64-unknown-linux-musl`，cargo-deny 也檢查映像實際的 target；`scripts/lib/` 的兩個型別錯誤已在 main 修正。第二次合併（Laysi/trustiot-v2#62 的 Istio 入口、Laysi/trustiot-v2#244 的測試時序修正）只有工具清單的衝突；第三次（Laysi/trustiot-v2#54 的 outbox）只有 `Cargo.lock` 的衝突，`cc` 因此改用 1.7.0；第四次（Laysi/trustiot-v2#55 的 OpenAPI）只有工具清單的衝突；第五次（Laysi/trustiot-v2#50 的 protocol、Laysi/trustiot-v2#251 的 actionlint）的衝突都是兩邊各加一項，全部保留；第六次（Laysi/trustiot-v2#59 的 ClickHouse）沒有衝突；第七次（Laysi/trustiot-v2#247 的 nightly、Renovate 與 arm64 的 setup-proto）的衝突在工具清單與文件，兩邊都保留，並依 main 的 Renovate 規則為發佈工具加上註解；第八次（Laysi/trustiot-v2#256、#258、#264）與第九次（Laysi/trustiot-v2#266、#267）沒有衝突。合併後的檢查見「合併 main 之後」。
