# nightly 的 smoke 部署啟用 certs 服務：本機驗證

對應 issue：Laysi/trustiot-v2#517

本報告記錄 2026-10-23 在開發者工作站上能做的驗證：單元測試、chart 渲染與 kubeconform、以真的 `certs-ca` 試跑測試根 CA 的流程。本 repo 規定工作站不跑 Kubernetes，所以叢集上的結果（兩種 values 的部署、mesh 內的 `ca.pem`／`crl.pem`、經入口的 401／403／201）要等帶 `smoke` label 的 PR 跑完 nightly 才有，由 coordinator 在那次執行之後補進本報告的「nightly 證據」一節。

## 環境

- 開發者工作站（x86_64 Linux），分支 `feat/517-nightly-certs`，rebase 到 main 的 `99b0f551` 之後測試。
- 工具（`.prototools`）：bun 1.4.2、helm 4.3.0、kubeconform 0.8.0、actionlint 1.7.12、Rust 1.99.0。工作站的 OpenSSL 是 4.0.3；nightly 的 runner（`ubuntu-24.04-arm`）是 Ubuntu 24.04 的 OpenSSL 3.0.x，用到的指令（`req -verify -pubkey`、`crl -verify -CAfile`、`crl -issuer -nameopt RFC2253`）兩版都有。
- `certs-ca` 試跑用的是本機建出的 debug 版（`cargo build --locked -p certs --bin certs-ca`），nightly 用的是同一份原始碼交叉編譯的 aarch64 靜態執行檔。

## 指令

```sh
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts/nightly
bun test scripts/ci
bun scripts/ci/check-workflows.ts
bun scripts/ci/check-secret-handling.ts
proto install actionlint && bun scripts/ci/lint-workflows.ts
bun test deploy/chart
bun test deploy/flux
bun test scripts
```

兩種型態以 smoke 的 values、API 的 values 與 certs 的兩份 values（只有根憑證，以及再加上簽發 CA 憑證）渲染後經 kubeconform 驗證（Kubernetes 1.30.0 與 1.36.0），腳本是 [render.ts.txt](data/2026-10-23-517-nightly-certs/render.ts.txt)（憑證是渲染用的一次性 openssl 憑證，金鑰用完即刪）：

```sh
# <dir> is any directory outside the repository
cp docs/test-reports/data/2026-10-23-517-nightly-certs/render.ts.txt <dir>/render.ts
bun <dir>/render.ts "$PWD" <dir>/render
```

以真的 `certs-ca` 跑 `scripts/nightly/certs.ts` 的 `TestRoot`（建根、簽一張 P-256 CSR、比對 key id、確認金鑰簽完即丟棄），腳本是 [dry-run.ts.txt](data/2026-10-23-517-nightly-certs/dry-run.ts.txt)：

```sh
cargo build --locked -p certs --bin certs-ca
cp docs/test-reports/data/2026-10-23-517-nightly-certs/dry-run.ts.txt <dir>/dry-run.ts
bun <dir>/dry-run.ts "$PWD" "$PWD/target/debug/certs-ca" <dir>/dry-run
```

## 結果

| 檢查 | 結果 |
| --- | --- |
| `bun run --cwd scripts typecheck` | 無錯誤 |
| `bun test scripts/nightly` | 325 pass、0 fail（其中 `certs.test.ts` 40 個） |
| `bun test scripts/ci`（含 `changes.test.ts`） | 488 pass、0 fail |
| `check-workflows.ts` | 4 個檔案，無違規 |
| `check-secret-handling.ts` | 514 個檔案、6 個容器建置檔，無違規 |
| `lint-workflows.ts`（actionlint） | 3 個 workflow，無錯誤 |
| `bun test deploy/chart` | 619 pass、0 fail（rebase 前的一次有 1 個 kubeconform 下載 schema 逾時 60 秒，單獨重跑 `-t kubeconform` 4 pass） |
| `bun test deploy/flux` | 75 pass、0 fail |
| `bun test scripts` | 1641 pass、9 skip（既有的 podman e2e 等）、0 fail |
| 渲染 + kubeconform | 兩種型態 × 等待／簽發兩階段 × 兩個 Kubernetes 版本，各 93 個資源全部 valid，[原始輸出](data/2026-10-23-517-nightly-certs/render-kubeconform.txt) |
| `certs-ca` 試跑 | 見下 |

`certs-ca` 試跑（[原始輸出](data/2026-10-23-517-nightly-certs/certs-ca-dry-run.txt)；每一項不成立時腳本就丟出錯誤、以非 0 結束，這次 10 項都是 `ok`，結束碼 0）：

- `root create` 建出 `CN=TrustIoT nightly test root CA saas`，是 CA，P-384（secp384r1），7 天。
- `root sign` 印出的 key id 與 openssl 從 CSR 算出的 key id 相同（`6e22e665…2212`）。
- 簽出的 `CN=TrustIoT nightly issuing CA G1 saas` 由測試根簽發（`checkIssued` 與簽章驗證都為真），是 CA，有效 2 天。
- 簽完 `holdsKey` 是 false，再簽一次被拒絕（`the test root's key was already used or discarded`）。
- run log 共 5 行，沒有 `PRIVATE KEY`；`TestRoot` 與 `requestKeyId` 的暫存目錄（`/tmp/nightly-*`）都已刪除。

單元測試涵蓋的重點（`scripts/nightly/certs.test.ts`）：

- 以 openssl 現場產生的根、簽發 CA、CRL 與工作站憑證，檢查 PEM 解析、chain、CRL 的簽署者（以簽發 CA 驗過、以根驗不過）。
- 簽出的工作站憑證必須由簽發 CA 簽、`ca_key_id` 是 `certs-ca` 印出的 key id、公鑰是送出的 CSR 的公鑰（另一把金鑰、同一個名稱的憑證被拒絕）。
- 存取控制不能空洞地通過：匿名或租戶管理員在簽發或撤銷時被放行，等待與簽發兩個檢查都失敗。
- 以 helm 實際渲染兩種型態：憑證 ConfigMap 的內容，以及設定簽發 CA 憑證前後 Deployment 完全相同（不會重啟）。

## 結論

| 驗收條件 | 本機結果 |
| --- | --- |
| nightly 以兩種 values 部署並啟用 certs：init container 寫入金鑰 Secret 與 CSR、設定簽發 CA 憑證後 `certs.ca.signing` 是 1、mesh 內的 `ca.pem` 與 `crl.pem` 正確 | 程式與單元測試完成，兩種型態的渲染通過 kubeconform，`certs-ca` 流程以真的程式試跑通過；叢集上的結果待 nightly。`certs.ca.signing` 這個指標在 nightly 讀不到（Laysi/trustiot-v2#183 之前 metrics 不輸出，nightly 也不讀 pod log），改以它代表的行為斷言：設定前 `ca.pem` 只有根、平台管理者得到 503 `workstation_certificate.ca_not_ready`，設定後 `ca.pem` 多了簽發 CA、平台管理者得到 201 |
| 經入口呼叫簽發與撤銷：沒有 cookie 401、租戶管理員 403、平台管理員成功 | 檢查與它的負向單元測試完成；待 nightly |
| `bun test scripts/nightly`、`bun test scripts/ci/changes.test.ts` 與 CI 通過 | 本機通過；CI 待 PR |

## nightly 證據

待 coordinator 以帶 `smoke` label 的 PR 跑完 nightly 之後補上：run 編號、兩種型態第 12 項三個檢查的 detail 行，以及 `trustiot-certs-ca-keys`、`trustiot-certs-ca-requests` 兩個物件與 uninstall 檢查的結果。
