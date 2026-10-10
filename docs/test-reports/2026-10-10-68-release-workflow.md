# release workflow：arm64 映像、keyless 簽章、SBOM 與 chart 發佈（2026-10-10）

對應 issue：Laysi/trustiot-v2#68。原始資料在 [data/2026-10-10-68-release-workflow/](data/2026-10-10-68-release-workflow/)。規則與設計見 [docs/architecture/ci-release.md](../architecture/ci-release.md) 的「release.yml」一節。

## 環境

- **commit**：`ee8c438`（分支 `ci/68-release-workflow`，base 為 main 的 `71be25d`）。本機的發佈測試、負向檢查與可重現建置都在這個 commit 上跑；rebase 之前跑過的交叉編譯檢查另外註明。
- **本機**：x86_64 的 Fedora 45（kernel 7.2.8）。podman 6.1.3、skopeo 1.24.1 來自系統；bun 1.4.2、helm 4.3.0、zig 0.16.0、cargo-zigbuild 0.23.4、cargo-auditable 0.7.6、syft 1.52.0、cosign 3.1.3 由 `.prototools` 鎖定版本、以 proto 安裝；Rust 1.99.0（`rust-toolchain.toml`）加上 `aarch64-unknown-linux-musl` target。詳見 [environment.txt](data/2026-10-10-68-release-workflow/environment.txt)。
- **本機 registry**：`docker.io/library/registry:3`（以 digest 釘選），只聽 `127.0.0.1:5068`。本機測試以當場產生的 cosign 金鑰對簽章（`--local-test-key`），不寫入 Sigstore：keyless 簽章需要 GitHub Actions 的 OIDC token，而且本機測試不該在公開紀錄留下任何東西。金鑰在測試的暫存目錄，測完刪除。
- **GitHub**：簽章過的 annotated tag `v0.0.0-rc.1` 指向 `ee8c438`，2026-10-10T12:39:36Z 推送，觸發 release.yml 的執行 [38052766358](https://github.com/Laysi/trustiot-v2/actions/runs/38052766358)，各 job 在 GitHub 代管的 `ubuntu-24.04`（x86_64）runner 上執行。repo 為私有，組織在 GitHub Free 方案。repo 沒有設定 `AI_MODEL` 變數，所以發版說明是 `Laysi/release-notes-action` 的 commit 主旨清單。
- **結果的讀取方式**：本 repo 的 GitHub 讀寫走 GitHub MCP server，它的 token 讀不到 check run 與 Actions 日誌，所以 CI 的結果以 report job 設的 commit status `release`（MCP `pull_request_read` 的 `get_status`）與 GitHub Release 的內文（MCP `get_release_by_tag`）為準。
- **Sigstore 公開紀錄**：以 Rekor 的公開 API（`rekor.sigstore.dev`，v1）不帶任何憑證查詢。

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

workflow 的 shell 片段：report job 在成功時設 `release` 為 success（「v0.0.0-rc.1 published and verified」），並把 GitHub Release 裡舊的「驗證結果」換成新的；plan 失敗時描述是「plan failed: 原因」並截到 140 字元；沒有錯誤訊息的失敗寫「build failed」，取消寫「publish cancelled」；失敗時不改 GitHub Release。`setup-proto` 的工具鏈檢查在版本相符時通過，不符時報錯。

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

### lab 的 Mac mini M1

待測。原本要把本機建出的 OCI 壓縮檔（`sha256:182467660b…`）拿到 lab 的 arm64 主機上以 podman 執行，確認在真的 arm64 上會跑並印出 `"arch":"aarch64"`、`"c_object":"ok"`。2026-10-10T12:50Z 起 lab 的 e2e 鎖由 Laysi/trustiot-v2#62 持有，等候期間沒有釋出。GHCR 上的映像是私有的，lab 主機也沒有讀取憑證，所以即使取得鎖，執行的也是本機建出的同一條流程的產物，不是 CI 推送的那一個。這一項不在驗收條件內：映像的平台由 verify job 檢查（linux/arm64），交叉編譯的 C 物件與 Rust 的一致性由 `release-selftest` 自己在執行時檢查。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 推一個 rc tag 後，GHCR 出現 arm64 映像與 chart，`cosign verify` 通過、SBOM 可下載 | **通過**。`v0.0.0-rc.1` 推送後約 3 分鐘，GHCR 出現只有 linux/arm64 的 `release-selftest` 映像與 `trustiot` chart；兩者以 release workflow 的身分驗證 cosign 簽章通過，另一個身分被拒；SBOM 以簽章過的 attestation 從 GHCR 取出（SPDX-2.3，13 個套件），也由 publish job 附在 GitHub Release（見「SBOM 的下載」）。verify job 的結果寫在 Release 內文，commit status `release` 為 success |
| workflow 權限最小化，沒有長期雲端金鑰 | **通過**。`release.yml` 的預設權限是 `{}`，每個 job 只列自己需要的權限（見 ci-release.md 的表），執行第三方建置程式碼的 build job 只有 `contents: read`；Laysi/trustiot-v2#67 的規則檢查沒有違規，第三方 action 都釘 commit SHA。GHCR 以各 job 的 `GITHUB_TOKEN` 登入，cosign 以 GitHub OIDC 取得效期 10 分鐘的憑證，沒有任何雲端金鑰或簽章金鑰。workflow 唯一引用的 secret 是給發版說明模型用的 `AI_API_KEY`（選用，目前沒有設定），只交給 plan job 裡產生發版說明的那一步 |

範圍內但無法在目前方案落實的一項：issue 的範圍寫「發佈走 GitHub environment 加人工核准」。publish job 已在 `release` environment 執行，但本 repo 是私有庫、組織在 GitHub Free 方案，不能為私有庫設定 environment，必要審查者在私有庫更要 GitHub Enterprise；v0.0.0-rc.1 的 publish 沒有經過任何核准就執行了。補償措施與待決事項寫在 ci-release.md 的「發佈核准（GitHub environment）的缺口」，需要人決定升級方案、把 repo 改為公開，或接受現況。

其他後續事項：

- 第一個服務映像加入後刪除 `release-selftest`（ci-release.md 已註明）。
- release profile 的 `strip = "debuginfo"` 經 zig 連結後沒有符號表（見上）。
- Laysi/trustiot-v2#67 分支的 `deny.toml` 只列 `aarch64-unknown-linux-gnu` 與 `x86_64-unknown-linux-gnu`，映像實際的 target 是 `aarch64-unknown-linux-musl`，只在 musl 才用到的相依不會被 cargo-deny 檢查；該 PR 或後續 issue 要加上這個 target。
- `.github/actions/setup-proto/action.yml` 在本 PR 與 Laysi/trustiot-v2#67 都有，本 PR 的版本只多了 Rust 工具鏈的版本檢查；後合併的一方解衝突時保留這段檢查。
- lab 的 arm64 實機執行（見上）。
