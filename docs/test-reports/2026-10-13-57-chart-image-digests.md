# 發佈的 chart 以 digest 固定同一次發佈的映像

對應 issue：Laysi/trustiot-v2#57 的第三條驗收條件「chart 以 digest 引用此映像」。

## 環境

- 本機發佈流程：工作站，Fedora，x86_64，podman 6.1.3、skopeo 1.24.1；`.prototools` 的 helm 4.3.0（`v4.3.0+gbec5b06`）、cosign 3.1.3、bun 1.4.2。暫用 registry 是 `docker.io/library/registry:3`（scripts/README.md 的 digest），只聽 `127.0.0.1:5157`；cosign 測試金鑰為暫用，跑完連同工作目錄刪除。
- 檢查：build host（x86_64，經 check queue `rcheck.sh`），在本分支的快照上執行。
- 分支 `feat/57-chart-image-digests`，基於 main `5fbe8ea`。最後一次檢查與本機流程在 `7d54e0f` 加上「verify 最後才檢查套件是否私有」那個 commit 的改動上執行（記錄裡的 commit 欄是 `7d54e0f`，改動當時還在工作目錄）；第一次的執行在 `e224d5c`，結果相同。
- 日期：2026-10-13（Asia/Taipei；記錄內的時間是 UTC）。

## 指令

```sh
# 單元測試與 repo 檢查
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts/release/chart-images.test.ts scripts/release/package-privacy.test.ts
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
| `bun test scripts/release/chart-images.test.ts scripts/release/package-privacy.test.ts` | 24 pass（16＋8）、0 fail |
| `bun test scripts` | 644 pass、2 skip（與本變更無關的 podman e2e 與 aarch64 主機限定測試）、0 fail |
| `bun test deploy/chart` | 288 pass、0 fail |
| `bun test deploy/flux` | 19 pass、0 fail |
| `bun scripts/ci/check-workflows.ts` | 4 個檔案，沒有違規 |
| `bun scripts/ci/lint-workflows.ts`（actionlint 1.7.12） | 3 個 workflow，沒有錯誤 |
| `bun scripts/ci/check-secret-handling.ts` | 263 個檔案，沒有違規 |
| `cargo test --locked -p repo-check` | 26 passed、0 failed |

### 本機發佈流程

完整輸出：[local-flow-synthetic.log](data/2026-10-13-57-chart-image-digests/local-flow-synthetic.log)（版本 `0.0.57-rc.1`，registry `127.0.0.1:5157/laysi/trustiot-v2`）。

| 步驟 | 結果 |
| --- | --- |
| plan | `helm lint --strict` 兩種型態通過；規劃 `identity`、`postgres`、`release-selftest` 與 chart |
| publish 第一次 | 推送並簽章三個映像；記錄 `the chart pins identity to …identity:0.0.57-rc.1@sha256:4e0d9c0b…`、`the chart pins postgres to …postgres:0.0.57-rc.1@sha256:f0004ca2…`、`the chart does not deploy release-selftest`；chart 推送為 `sha256:34240729…` 並簽章 |
| publish 第二次（同一份 build 產物，模擬重跑 job） | 映像都是「already … not pushing it again」；chart 是 `already holds this chart (sha256:34240729…)`，沒有觸發不可變的檢查；兩次的 `published.json` 逐位元相同 |
| verify | 通過（`verified 3 image(s) and the chart`）；報告的 chart 一行寫出兩個固定的參照，並寫明 `release-selftest` 不由 chart 部署。本機模式不做私有檢查，報告各列照舊以「。」結束 |
| 拉下的 chart | `helm show values`：`services.identity.image.digest` 由 release 加上（附註解）、`postgres.bundled.image.digest` 填入，本機模式的 `serviceDefaults.image.registry` 與 `postgres.bundled.image.repository` 改成本機 registry；ClickHouse 與 collector 的 digest 不變 |
| render 比對 | 拉下的 chart 疊上 `values-saas.yaml`（與 `values-onprem.yaml`）加 `tests/values/<型態>-instance.yaml`、`services.identity.enabled=true` 後 `helm template`：`identity` 與 `postgres` 各只有一個參照，都等於 `<registry>/<名稱>:0.0.57-rc.1@<skopeo 從 registry 讀到的 manifest digest>`（4 個 `OK`） |
| 反向測試 | 解開 chart，把 postgres 的 digest 換成 identity 映像的 digest，重新打包、以同一把測試金鑰簽章並推到同一個 tag；verify 以 `does not pin this release's images: values.yaml: postgres is …@sha256:4e0d9c0b…, not …@sha256:f0004ca2…`（`values.yaml` 與疊上兩個型態檔各一條）拒絕，exit 非 0 |

registry 裡的 manifest digest（skopeo 直接讀取）與 publish 的結果一致：`identity` `sha256:4e0d9c0b…`、`postgres` `sha256:f0004ca2…`、`release-selftest` `sha256:f48b2d9e…`。

### 單元測試涵蓋的項目

`scripts/release/chart-images.test.ts`：

- 映像對應：`postgres` 對 `postgres.bundled.image`，chart `services` 的 key 對 `services.<名稱>.image`，`release-selftest` 列為不部署，其他映像名稱會讓發佈停止；`deploy/images/` 的每個映像都有歸屬。
- 寫入 digest：repo 的 `values.yaml` 只多出 identity 的三行（註解、`image:`、`digest:`）並填入 postgres 的 digest，其他位元不變；同樣的輸入得到同樣的輸出；已經有 digest、tag 不是空的、repository 不是推送的位置（正式發佈）時拒絕；本機模式改寫 registry；註解、行尾註解、清單與 block scalar 裡的同名 key 不受影響。
- `chartPinProblems`（verify 用的檢查）：digest 錯誤時指出是哪個映像；覆寫檔把 digest 設成 `""` 或 `null`、改 tag、改 registry 都會被抓到；`deploy/chart` 的兩個型態檔與 `deploy/flux` 的 `values-saas.yaml` 副本、`values-instance.yaml` 都沒有覆寫。
- 打包：兩次打包的 `.tgz` 位元相同，repo 的 `values.yaml` 不變；解開後的檢查通過，換成別的 digest 時每個映像在三種疊法各報一次；拉下的套件以兩種型態 `helm template` 時，每個固定的映像都 render 成 `<repository>:<版本>@<digest>`，沒有不帶 digest 的參照。

`scripts/release/package-privacy.test.ts`（verify 最後的私有檢查）：匿名要求被拒是私有、成功是公開、其他失敗是無法判斷（附 registry 回應的第一行）；全部私有時通過；本機模式略過時通過；一個公開時失敗並列出它，私有的不列；多個公開時全部列出，而且名稱在錯誤訊息最前面（`error` 輸出只取第一行的前 200 字）；無法判斷也失敗；報告各列與開頭的「驗證未通過」寫出每個套件的結果。

## 待測

- 以 `build.ts` 建置真正映像的完整本機流程（不設 `LOCAL_FLOW_IMAGES`）：check queue 的 build host 目前沒有 podman 與 skopeo（`which` 找不到），工作站不跑大型建置，所以這次沒有執行。`build.ts` 本次沒有修改，publish 與 verify 只讀它輸出的 OCI 壓縮檔與 SBOM，所以上面的流程涵蓋了改動的每一條路徑。有 podman 與 skopeo 的機器上，以上面的指令（不設環境變數）即可重跑。真正的映像經過同一套 publish 與 verify，已由下一節的 `v0.0.0-rc.6` 在 GHCR 上驗證。

## 正式 rc：`v0.0.0-rc.6`

PR Laysi/trustiot-v2#376 合併為 main `608f5c5`，main 的 CI 通過後，coordinator 在這個 commit 打了簽章的 tag `v0.0.0-rc.6`：release.yml run [38303663554](https://github.com/Laysi/trustiot-v2/actions/runs/38303663554)，每個 job 都在 `ubuntu-24.04-arm` 上執行。記錄在 [rc6-run.txt](data/2026-10-13-57-chart-image-digests/rc6-run.txt)（job 結果與 runner、publish 與 verify 的記錄行）與 [rc6-independent.txt](data/2026-10-13-57-chart-image-digests/rc6-independent.txt)（coordinator 的獨立檢查）。

| 步驟 | 結果 |
| --- | --- |
| plan、build、publish | 成功。publish 推送並簽章三個映像，記錄 `the chart pins identity to ghcr.io/laysi/trustiot-v2/identity:0.0.0-rc.6@sha256:a7ec5c66…`、`the chart pins postgres to …/postgres:0.0.0-rc.6@sha256:a69113bc…`、`the chart does not deploy release-selftest`；chart `sha256:87824383…` 簽章；建立 GitHub 預發佈版本 |
| verify | 三個映像的 tag、平台、簽章與 SBOM attestation（213、142、13 個套件），chart 的 tag、簽章、拉下的檔案、version 與 appVersion，以及 chart 固定的兩個參照（`values.yaml` 單獨與疊上兩個型態檔）都通過；另一個身分的對照驗證被拒。最後只有私有檢查失敗：`identity, postgres can be pulled anonymously …; Every other check passed`，這是 Laysi/trustiot-v2#1 第 10 項記錄的預期結果，`deploy-pr` 因此略過 |
| report | 把 verify 的報告附到 [GitHub Release](https://github.com/Laysi/trustiot-v2/releases/tag/v0.0.0-rc.6)：開頭寫明驗證未通過與原因，每一列寫出檢查結果；commit status `release` 設為 failure，原因同上 |
| 獨立檢查（coordinator 工作站） | `helm pull` 拿到 chart `sha256:87824383…`，`helm show values` 有 release 加上的 `services.identity.image.digest: sha256:a7ec5c66…`（附註解）與 `postgres.bundled.image.digest: sha256:a69113bc…`；registry 裡 `identity:0.0.0-rc.6`、`postgres:0.0.0-rc.6` 的 manifest digest 與這兩個值相同；以 rc.6 的身分 `cosign verify` chart 的 digest，exit 0；拉下的 chart 疊上 `values-saas.yaml` 或 `values-onprem.yaml` 加測試用的 instance 覆寫檔 render，`identity` 與 `postgres` 各只有一個參照，都是 `<repository>:0.0.0-rc.6@<上面的 digest>` |

## 結論

- 「chart 以 digest 引用此映像」：通過。本機流程證明發佈的 chart 以 digest 固定 `postgres`（與 `identity`）映像、重跑 publish 得到同一個 chart、verify 拒絕 digest 不對的 chart；`v0.0.0-rc.6` 在 GHCR 上得到同樣的結果，verify 自己檢查了 chart 固定的參照，coordinator 也以 registry 的 manifest digest 獨立確認。
