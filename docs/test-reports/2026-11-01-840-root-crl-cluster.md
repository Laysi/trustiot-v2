# certs 的根 CRL：在真實叢集上換新、回滾與缺欄位的拒絕

對應 issue：Laysi/trustiot-v2#840。

驗三件事：

1. 用 `helm upgrade` 把 `certificateAuthority.rootCrl` 換成編號較高的根 CRL，執行中的 certs 不重啟就採用它（記 `adopted the root CRL`，`certs.root_crl.*` 指標照常）。
2. 用 `helm rollback` 換回編號較低的那張，certs 保留較新的、記 ERROR，`certs.root_crl.invalid` 變成 1。
3. 沒有 CRL 編號、或沒有 nextUpdate 的根 CRL，certs 拒絕採用並記 ERROR（Rust 測試）。

規則本身寫在 `services/certs/README.md`，「根 CRL」一節。

## 環境

| 項目 | 內容 |
| --- | --- |
| 分支 | `test/840-root-crl-cluster`，從 `main` 的 `8960f597` 分出 |
| Rust 測試 | 建置主機（x86_64 Linux），2026-11-01 02:51 UTC；toolchain 依 `rust-toolchain.toml` |
| 映像與測試用根 | 同一台建置主機，2026-11-01 03:02 UTC；pinned zig 0.16.0 與 cargo-zigbuild，交叉編譯成 `aarch64-unknown-linux-musl`，用 rootless podman（vfs、這次工作自己的 store）打包成 arm64 映像，不在 arm64 映像裡執行任何東西 |
| 叢集 | lab 的 Mac mini M1，單節點 k3s v1.36.5+k3s1（arm64，Istio ambient、CloudNativePG operator 已裝），2026-11-01 03:14–03:18 UTC；持有 lab 鎖（owner 行 `Laysi/trustiot-v2#840 <時間>`），namespace `e2e-840`（Pod Security `restricted`） |
| chart | `deploy/chart`（trustiot-0.1.0），`values-onprem.yaml` + `tests/values/onprem-instance.yaml` + 本次的 [`values-root-crl-e2e.yaml`](data/2026-11-01-840-root-crl-cluster/values-root-crl-e2e.yaml) |
| 映像 | certs、migrate、example-service（扮演 identity）：本分支建的 `localhost/trustiot-e2e/<名稱>:840`（二進位 SHA-256 見 [`build-images-output.txt`](data/2026-11-01-840-root-crl-cluster/build-images-output.txt)）；Postgres：節點上已有的 `ghcr.io/laysi/trustiot-v2/postgres:0.0.0-rc.12@sha256:8fa498eb…`（`deploy/images/postgres` 之後沒有改過）；ClickHouse：chart values 裡的 digest；指標接收端：`otel/opentelemetry-collector-contrib:0.161.0@sha256:fd328de2…` |
| 測試用根 | `certs-ca root create`（CN `TrustIoT e2e-840 test root`，30 天），再以 `certs-ca root crl` 簽根 CRL 1 與 2（nextUpdate 2027-12-06T02:02:47Z，無撤銷項目）；私鑰只存在建置工作的 shell 變數裡，簽完就丟，沒有寫進檔案 |

這次怎麼讓 chart 跑起來：

- certs 需要 Postgres、migration Job 與內部身分公鑰（`TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`），而 chart 只在 identity 啟用時發布這份公鑰，所以 Postgres 與 ClickHouse 用 bundled，identity 由 `crates/service-kit` 的 example-service 扮演（與 `values-postgres-e2e.yaml` 相同做法）。
- chart 的 collector 目前把指標送到 nop（要等 Laysi/trustiot-v2#183），所以把 collector 關掉，certs 的 `TRUSTIOT__TELEMETRY__OTLP_ENDPOINT` 指到本次自己的接收端 pod（debug exporter 印出指標，logs 與 traces 丟到 nop）。接收端不在 mesh 裡，另加一條 NetworkPolicy 只放 certs 的 pod 連 4318。
- 映像以 oci-archive 經 ssh 串流進節點的 `k3s ctr -n k8s.io images import`。

## 指令

全部從 repository 根目錄執行。

1. 測試（建置主機）：

   ```sh
   cargo clippy --locked -p certs --all-targets -- -D warnings
   cargo test --locked -p certs --test root_crl -- --nocapture
   ```

2. 映像與測試用根（建置主機，x86_64 Linux、rootless podman）：

   ```sh
   bash docs/test-reports/data/2026-11-01-840-root-crl-cluster/build-images.sh
   ```

   產出在建置主機的 `~/.cache/trustiot-v2-840/`：三個映像的 oci-archive 與 `root.pem`、`crl-1.pem`、`crl-2.pem`。

3. lab 上的準備（操作者手動）：取得 lab 鎖（owner 行 `Laysi/trustiot-v2#840 <時間>`）；把三個 oci-archive 匯入節點的 containerd（`k3s ctr -n k8s.io images import`）；建立 namespace：

   ```sh
   kubectl create namespace e2e-840
   kubectl label namespace e2e-840 pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/enforce-version=latest
   ```

4. 換新與回滾（工作站，經 ssh 轉發連 API server）：

   ```sh
   LAB_SSH=<lab 節點的 ssh 主機> WORK=<repository 以外的私有目錄> \
   ART=<放 root.pem、crl-1.pem、crl-2.pem 的目錄> \
   E2E_POSTGRES_IMAGE=ghcr.io/laysi/trustiot-v2/postgres:0.0.0-rc.12@sha256:8fa498ebe48aa084c140d50869650ca6d79fd0d7c5d9172da283b12cfd983f57 \
   bun docs/test-reports/data/2026-11-01-840-root-crl-cluster/lab-run.ts
   ```

   [`lab-run.ts`](data/2026-11-01-840-root-crl-cluster/lab-run.ts) 依序：部署指標接收端；`helm install`（根 CRL 1）並 `--wait`；等 certs 記下採用 1、等一輪指標、經 port-forward 取 `GET /certs/root-crl` 確認發出的是 1；`helm upgrade` 換成根 CRL 2，等 ConfigMap 更新與採用 2，確認同一個 pod、重啟 0 次、指標、發出的是 2；`helm rollback` 到 revision 1，等 ConfigMap 換回與 ERROR，確認沒有再採用、同一個 pod、指標、發出的仍是 2。結果寫到 `WORK/out`。

5. 收尾（操作者手動，失敗時也要做）：刪掉 namespace `e2e-840`、從節點移除匯入的 `localhost/trustiot-e2e/*` 映像、釋放 lab 鎖。

`lab-run.ts` 用節點的 admin kubeconfig（只在 `WORK` 裡、0600、結束或收到 SIGINT/SIGTERM/SIGHUP 時刪除）。它只在 namespace `e2e-840` 裡建立物件，但 chart 會建 CloudNativePG 與 Istio 的自訂資源，與既有的 lab e2e 一樣用 admin 身分，沒有另外做限縮權限的帳號。

## 結果

### 缺欄位的根 CRL（驗收條件 3）

`services/certs/tests/root_crl.rs` 新增 `a_root_crl_without_a_crl_number_or_a_next_update_is_refused`。rcgen 簽不出缺 CRL 編號或缺 nextUpdate 的 CRL，所以測試從 `certs-ca` 簽的 CRL 拆出 TBSCertList，用 `root_crl::der_element` 拿掉那一個欄位，再以同一把根私鑰重簽；先用 x509-parser 確認只少了那個欄位、簽章仍由根驗得過，才拿去給 certs。對照組（不拿掉任何欄位，同一條路徑重簽）會被採用。每一種都測兩個情境：已採用根 CRL 1 之後，以及服務第一次載入根 CRL 時；另外確認 `RootCrl::check` 給出相同的理由，最後補上編號 2 的完整 CRL 會被採用。

| 根 CRL | 已採用 1 之後 | 第一張 | 拒絕理由（`problem`） |
| --- | --- | --- | --- |
| 沒有 CRL 編號 | 不採用、ERROR、`kept_number` 1 | 不採用、ERROR、沒有 `kept_number` | `it has no CRL number, by which gateways keep the newest root CRL`（共用檢查會放行，certs 自己拒絕；agent 的出廠安裝也拒絕） |
| 沒有 nextUpdate | 同上 | 同上 | `provided CRL could not be parsed: Other(OtherError(TrailingData(Time)))`（webpki 依 RFC 5280 §5.1.2.5 要求 nextUpdate，在共用檢查就被拒） |

`root_crl` 測試共 6 個全部通過，clippy 沒有警告：[`certs-root-crl-tests.txt`](data/2026-11-01-840-root-crl-cluster/certs-root-crl-tests.txt)。

### 換新與回滾（驗收條件 1、2）

全部在同一個 certs pod `trustiot-certs-69bccc5df9-bv76m`（uid `7c3bb6b4-…`，03:15:08Z 啟動）上發生，整段重啟 0 次。

| 步驟 | helm 完成 | ConfigMap 更新被看到 | certs 的反應 | 距 helm 完成 | 指標（下一輪） | `GET /certs/root-crl` |
| --- | --- | --- | --- | --- | --- | --- |
| `helm install`（CRL 1，37.3 s） | — | — | 03:15:08.744Z INFO `adopted the root CRL`，number 1 | — | 03:16:08Z：remaining 34555599、expiring 0、invalid 0 | CRL 1 |
| `helm upgrade`（CRL 2） | 03:16:21.585Z | 03:16:21.706Z | 03:16:38.753Z INFO `adopted the root CRL`，number 2 | 17.2 s | 03:17:08Z：remaining 34555539、expiring 0、invalid 0 | CRL 2 |
| `helm rollback` 到 1 | 03:17:21.858Z | 03:17:21.986Z | 03:17:38.754Z ERROR `cannot adopt the root CRL in the CA certificates ConfigMap…`，`kept_number` 2；之後沒有再採用 | 16.9 s | 03:18:08Z：remaining 34555479、expiring 0、**invalid 1** | 仍是 CRL 2 |

回滾時的 `problem`：`/etc/trustiot/certs-ca/root-crl.pem: its CRL number 1 is lower than that of root CRL 2 adopted under the same root, so it is older and lacks revocations`。

helm history：revision 1 `Install complete`、2 `Upgrade complete`、3 `Rollback to 1`。

原始資料：

- [`root-crl-e2e.json`](data/2026-11-01-840-root-crl-cluster/root-crl-e2e.json)：每一步的時間、pod、日誌行、指標與發出的 CRL。
- [`certs-root-crl-log.jsonl`](data/2026-11-01-840-root-crl-cluster/certs-root-crl-log.jsonl)：certs 關於根 CRL 的日誌行。
- [`root-crl-gauges.jsonl`](data/2026-11-01-840-root-crl-cluster/root-crl-gauges.jsonl)：接收端印出的三個 gauge。
- [`lab-run-output.txt`](data/2026-11-01-840-root-crl-cluster/lab-run-output.txt)：`lab-run.ts` 的進度輸出（拿掉了 port-forward 的重連行）。
- [`build-images-output.txt`](data/2026-11-01-840-root-crl-cluster/build-images-output.txt)：二進位與映像的摘要、根 CRL 的 SHA-256。

### 前兩次沒跑完的嘗試

同一天在同一個 namespace 先跑了兩次，各自修正後刪掉 namespace 重建，第三次就是上面的結果：

1. certs 起不來：`configuration key platform.internal_identity_keys_file is required`。chart 只在 identity 啟用時發布內部身分公鑰，因此 overlay 改成啟用 identity，由 example-service 扮演。
2. identity 起不來：example-service 要求 `greeting`，overlay 加上 `TRUSTIOT__GREETING`；同時 certs 把 OTLP 日誌送到接收端得到 404，接收端加上 logs 與 traces 管線（送到 nop）。

跑完之後再改了兩個檔案，部署內容不變：

- `build-images.sh` 的輸出不列檔案擁有者（安全審查的建議）。
- `lab-run.ts`：namespace 限 `e2e-<數字>`；檢查 lab 鎖的 owner 行；`WORK` 不得是 repository、在它裡面或包含它（解析後的實際路徑也檢查）；收到 SIGINT/SIGTERM/SIGHUP 時也停掉 ssh 轉發與 port-forward、刪掉 kubeconfig；每一步存下結果後，逐項斷言驗收要的值（發出的 CRL、同一個 pod 且重啟 0 次、gauge 的值、回滾後保留 2 且沒有再採用），不符就以失敗結束。上面這次的結果每一項都符合這些斷言。改動以型別檢查（`scripts/tsconfig.json` 的設定）、`RENDER_ONLY=1` 渲染（61 個物件）、各個防呆的拒絕訊息，以及用假的 ssh 送 SIGTERM（結束碼 130、轉發程序不留、`WORK` 裡的 kubeconfig 已刪）確認過，沒有在叢集上重跑。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. lab k3s 上 `helm upgrade` 換成編號較高的根 CRL，certs 不重啟就採用（日誌 `adopted the root CRL`、`certs.root_crl.*` 指標），結果存在 docs/test-reports/ | 通過：同一個 pod、重啟 0 次，helm 完成後 17.2 s 採用 2，指標 invalid 0、expiring 0，發出的換成 CRL 2 |
| 2. 回滾到編號較低的那張：certs 保留新的、記 ERROR、`certs.root_crl.invalid` = 1，結果存在 docs/test-reports/ | 通過：16.9 s 後記 ERROR（`kept_number` 2），沒有再採用，invalid 1，發出的仍是 CRL 2 |
| 3. 測試證明沒有 CRL 編號、沒有 nextUpdate 的根 CRL 被拒（不採用、記 ERROR） | 通過：`a_root_crl_without_a_crl_number_or_a_next_update_is_refused`，兩種都在已採用 1 之後與第一張時被拒 |

依 trustiot-v2 的驗證分級延後、另開低優先級 issue 的進階驗證：

- 把這次手動的叢集流程自動化成 `deploy/chart/tests/e2e` 的 e2e 測試（含 namespace 與鎖的收尾）。
- 在 saas 形態（`values-saas.yaml`）上重跑一次。
- 指標的第二個來源佐證（例如 chart 的 collector 送出指標之後，從 Laysi/trustiot-v2#183 的後端讀到同樣的值）。

不在範圍內：certs 重啟後是否仍拒絕較舊的根 CRL（跨重啟保留已採用的編號）由 Laysi/trustiot-v2#563 處理。
