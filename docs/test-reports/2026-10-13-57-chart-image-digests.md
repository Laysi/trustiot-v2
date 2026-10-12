# 發佈的 chart 以 digest 固定同一次發佈的映像

對應 issue：Laysi/trustiot-v2#57 的第三條驗收條件「chart 以 digest 引用此映像」。

## 環境

- 本機發佈流程：工作站，Fedora，x86_64，podman 6.1.3、skopeo 1.24.1；`.prototools` 的 helm 4.3.0（`v4.3.0+gbec5b06`）、cosign 3.1.3、bun 1.4.2。暫用 registry 是 `docker.io/library/registry:3`（scripts/README.md 的 digest），只聽 `127.0.0.1:5157`；cosign 測試金鑰為暫用，跑完連同工作目錄刪除。
- 檢查：build host（x86_64，經 check queue `rcheck.sh`），在本分支的快照上執行。
- 分支 `feat/57-chart-image-digests`，commit `e224d5c`（基於 main `5fbe8ea`）。
- 日期：2026-10-13（Asia/Taipei；記錄內的時間是 UTC）。

## 指令

```sh
# 單元測試與 repo 檢查
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts/release/chart-images.test.ts
bun test scripts
bun test deploy/chart
bun test deploy/flux
bun scripts/ci/check-workflows.ts
bun scripts/ci/lint-workflows.ts
bun scripts/ci/check-secret-handling.ts
cargo test --locked -p repo-check

# 本機發佈流程（plan、publish 兩次、verify、render 比對、反向測試），在 repo 根目錄
LOCAL_FLOW_IMAGES=synthetic bash docs/test-reports/data/2026-10-13-57-chart-image-digests/local-flow.sh
```

[local-flow.sh](data/2026-10-13-57-chart-image-digests/local-flow.sh) 就是 scripts/README.md「本機跑一次完整流程」的指令，加上本 issue 的檢查。不設 `LOCAL_FLOW_IMAGES` 時以 `build.ts` 建置真正的映像；`LOCAL_FLOW_IMAGES=synthetic` 改為每個規劃中的映像各做一個極小的 linux/arm64 映像（`FROM scratch`，只 `COPY` 一個檔案）與最小的 SPDX 文件，放在 `build.ts` 輸出的同一個位置，之後的 publish 與 verify 完全相同。

## 結果

### 檢查

build host 上一次執行，全部 exit 0，記錄：[checks.log](data/2026-10-13-57-chart-image-digests/checks.log)。

| 指令 | 結果 |
| --- | --- |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun test scripts/release/chart-images.test.ts` | 16 pass、0 fail |
| `bun test scripts` | 636 pass、2 skip（與本變更無關的 podman e2e 與 aarch64 主機限定測試）、0 fail |
| `bun test deploy/chart` | 288 pass、0 fail |
| `bun test deploy/flux` | 19 pass、0 fail |
| `bun scripts/ci/check-workflows.ts` | 4 個檔案，沒有違規 |
| `bun scripts/ci/lint-workflows.ts`（actionlint 1.7.12） | 3 個 workflow，沒有錯誤 |
| `bun scripts/ci/check-secret-handling.ts` | 262 個檔案，沒有違規 |
| `cargo test --locked -p repo-check` | 26 passed、0 failed |

### 本機發佈流程

完整輸出：[local-flow-synthetic.log](data/2026-10-13-57-chart-image-digests/local-flow-synthetic.log)（版本 `0.0.57-rc.1`，registry `127.0.0.1:5157/laysi/trustiot-v2`）。

| 步驟 | 結果 |
| --- | --- |
| plan | `helm lint --strict` 兩種型態通過；規劃 `identity`、`postgres`、`release-selftest` 與 chart |
| publish 第一次 | 推送並簽章三個映像；記錄 `the chart pins identity to …identity:0.0.57-rc.1@sha256:4e0d9c0b…`、`the chart pins postgres to …postgres:0.0.57-rc.1@sha256:f0004ca2…`、`the chart does not deploy release-selftest`；chart 推送為 `sha256:66e7693c…` 並簽章 |
| publish 第二次（同一份 build 產物，模擬重跑 job） | 映像都是「already … not pushing it again」；chart 是 `already holds this chart (sha256:66e7693c…)`，沒有觸發不可變的檢查；兩次的 `published.json` 逐位元相同 |
| verify | 通過（`verified 3 image(s) and the chart`）；報告的 chart 一行寫出兩個固定的參照，並寫明 `release-selftest` 不由 chart 部署 |
| 拉下的 chart | `helm show values`：`services.identity.image.digest` 由 release 加上（附註解）、`postgres.bundled.image.digest` 填入，本機模式的 `serviceDefaults.image.registry` 與 `postgres.bundled.image.repository` 改成本機 registry；ClickHouse 與 collector 的 digest 不變 |
| render 比對 | 拉下的 chart 疊上 `values-saas.yaml`（與 `values-onprem.yaml`）加 `tests/values/<型態>-instance.yaml`、`services.identity.enabled=true` 後 `helm template`：`identity` 與 `postgres` 各只有一個參照，都等於 `<registry>/<名稱>:0.0.57-rc.1@<skopeo 從 registry 讀到的 manifest digest>`（4 個 `OK`） |
| 反向測試 | 解開 chart，把 postgres 的 digest 換成 identity 映像的 digest，重新打包、以同一把測試金鑰簽章並推到同一個 tag；verify 以 `does not pin this release's images: values.yaml: postgres is …@sha256:4e0d9c0b…, not …@sha256:f0004ca2…`（`values.yaml` 與疊上兩個型態檔各一條）拒絕，exit 非 0 |

registry 裡的 manifest digest（skopeo 直接讀取）與 publish 的結果一致：`identity` `sha256:4e0d9c0b…`、`postgres` `sha256:f0004ca2…`、`release-selftest` `sha256:f48b2d9e…`。

### 單元測試涵蓋的項目（`scripts/release/chart-images.test.ts`）

- 映像對應：`postgres` 對 `postgres.bundled.image`，chart `services` 的 key 對 `services.<名稱>.image`，`release-selftest` 列為不部署，其他映像名稱會讓發佈停止；`deploy/images/` 的每個映像都有歸屬。
- 寫入 digest：repo 的 `values.yaml` 只多出 identity 的三行（註解、`image:`、`digest:`）並填入 postgres 的 digest，其他位元不變；同樣的輸入得到同樣的輸出；已經有 digest、tag 不是空的、repository 不是推送的位置（正式發佈）時拒絕；本機模式改寫 registry；註解、行尾註解、清單與 block scalar 裡的同名 key 不受影響。
- `chartPinProblems`（verify 用的檢查）：digest 錯誤時指出是哪個映像；覆寫檔把 digest 設成 `""` 或 `null`、改 tag、改 registry 都會被抓到；`deploy/chart` 的兩個型態檔與 `deploy/flux` 的 `values-saas.yaml` 副本、`values-instance.yaml` 都沒有覆寫。
- 打包：兩次打包的 `.tgz` 位元相同，repo 的 `values.yaml` 不變；解開後的檢查通過，換成別的 digest 時每個映像在三種疊法各報一次；拉下的套件以兩種型態 `helm template` 時，每個固定的映像都 render 成 `<repository>:<版本>@<digest>`，沒有不帶 digest 的參照。

## 待測

- 以 `build.ts` 建置真正映像的完整本機流程（不設 `LOCAL_FLOW_IMAGES`）：check queue 的 build host 目前沒有 podman 與 skopeo（`which` 找不到），工作站不跑大型建置，所以這次沒有執行。`build.ts` 本次沒有修改，publish 與 verify 只讀它輸出的 OCI 壓縮檔與 SBOM，所以上面的流程涵蓋了改動的每一條路徑。有 podman 與 skopeo 的機器上，以上面的指令（不設環境變數）即可重跑。
- 正式發佈：合併後由 coordinator 打 rc tag，release.yml 在 GHCR 上執行同一套 publish 與 verify。verify 的第一項檢查（套件私有）在套件公開期間預期失敗（Laysi/trustiot-v2#1 第 10 項），chart 的檢查在它之後，所以要以 publish 的記錄與拉下的 chart 確認。

## 結論

- 「chart 以 digest 引用此映像」：通過（本機）。發佈的 chart 以 digest 固定 `postgres`（與 `identity`）映像，render 結果與 registry 的 manifest digest 相同；重跑 publish 得到同一個 chart；verify 檢查並拒絕 digest 不對的 chart。正式 rc 的驗證待合併後執行。
