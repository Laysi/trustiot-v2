# 叢集內 ClickHouse 管理帳號密碼在叢集內產生（2026-10-15）

Laysi/trustiot-v2#403：叢集內 ClickHouse（`clickhouse.mode: bundled`）的 bootstrap 管理帳號密碼改由 ClickHouse pod 的 init container（`migrate clickhouse-admin-password`）產生進 chart 宣告的空 Secret `<release>-clickhouse-admin-password`，migration Job 經 Kubernetes API 讀它；安裝前不再需要人建立 Secret。設計見 `docs/architecture/secrets.md`「叢集內 ClickHouse 管理帳號的密碼」，輪替與搬移步驟見 `docs/runbooks/secrets.md`。

## 環境

- **程式碼**：分支 `feat/403-clickhouse-admin-in-cluster`，lab 測試時的 HEAD 是 `a91f0e5`（main `37863e0` 之上三個 commit）。
- **建置主機**（經檢查佇列）：workspace 的 fmt、clippy、測試、`cargo deny`、`bun test scripts`、typecheck、`bun test deploy/flux`、`bun test deploy/chart`、兩個 CI 腳本，以及 lab 映像用的 aarch64 執行檔：`cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p migrate --bin migrate`（zig 0.16.0、cargo-zigbuild 由 proto 提供），SHA-256 `b5ecfef0b06248378651de7993a02c9bd31dba71eca422bf56b6d28961158d1a`。
- **映像**：本機以 podman 用 `deploy/images/rust-binary.Containerfile`（`@BIN@` 換成 `migrate`）打包 `localhost/trustiot-e2e/migrate:403`（linux/arm64，image ID `b91004aaee9a…`），存成 OCI 封存檔，在節點上以 `sudo k3s ctr -n k8s.io images import` 匯入。ClickHouse 映像是 `values.yaml` 釘選的 digest，節點上已有。
- **lab 的 Mac mini M1**：aarch64、Fedora Linux Asahi Remix 44、k3s v1.36.5+k3s1、Istio 1.30.5 ambient。沒有 CloudNativePG，這次也不需要（ClickHouse 單獨安裝，migration Job 關閉）。取得 lab 鎖後建立 namespace `e2e-403`（Pod Security `restricted`）與執行身分：ServiceAccount `e2e-403-runner` 綁定內建的 `admin`，另加 Gateway、HTTPRoute 與 AuthorizationPolicy 的 Role（[lab-runner-rbac.yaml](data/2026-10-15-403-clickhouse-admin-in-cluster/lab-runner-rbac.yaml)）。kubeconfig 只放在本機暫存目錄，API server 經會自動重連的 SSH 轉送，測完刪除。
- **時間**：lab e2e 2026-10-15 01:38:37 到 01:40:19（Asia/Taipei），全新安裝檢查 01:40:51 到 01:41:10。

## 指令

叢集測試（[lab-run-e2e.sh](data/2026-10-15-403-clickhouse-admin-in-cluster/lab-run-e2e.sh)，在 repo 根目錄）：

```sh
KUBECONFIG=<只有 e2e-403 權限的 kubeconfig> \
E2E_NAMESPACE=e2e-403 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:403 \
E2E_IMAGE_PULL_POLICY=Never E2E_REPORT_DIR=<輸出目錄> \
bun test --timeout 900000 ./deploy/chart/tests/e2e/clickhouse-admin.e2e.ts
```

全新安裝檢查（[lab-fresh-install.sh](data/2026-10-15-403-clickhouse-admin-in-cluster/lab-fresh-install.sh)）：不建立任何 Secret，以 chart 的預設值 `helm install --wait`，再讀 init container 的結束碼與日誌、Secret 的形狀（只有 key 名稱、長度、代與是否綁定）、ClickHouse 是否接受檔案裡的密碼、檔案權限，最後 uninstall 並刪除 PVC 與保留的 Secret。

建置主機上的檢查指令與輸出見 [checks.txt](data/2026-10-15-403-clickhouse-admin-in-cluster/checks.txt)；tools/migrate 的測試名稱見 [migrate-tests.txt](data/2026-10-15-403-clickhouse-admin-in-cluster/migrate-tests.txt)。

## 結果

### lab 叢集測試：5 通過、0 失敗（101.7 秒）

[輸出](data/2026-10-15-403-clickhouse-admin-in-cluster/lab-e2e-output.txt)、[報告 JSON](data/2026-10-15-403-clickhouse-admin-in-cluster/lab-e2e.json)、[JUnit](data/2026-10-15-403-clickhouse-admin-in-cluster/lab-e2e.junit.xml)。

| 步驟 | 結果 |
| --- | --- |
| 舊的安裝方式：`adminSecret.name` 指向測試建立的 Secret | 該密碼被接受；沒有 init container，沒有產生的 Secret（升級到這個 chart 不改變舊安裝） |
| 搬移：拿掉 `adminSecret` 升級 | ClickHouse 重啟，init container `admin-password` 結束碼 0；Secret 只有 key `trustiot_admin`，值是 64 個十六進位字元，代 `1`，已綁定 uid，`keep`、`trustiot.io/filled-by: migrate`；日誌寫出 `generated the password of trustiot_admin in Secret trustiot-clickhouse-admin-password (it had none)`，不含密碼；新密碼被接受，舊密碼被拒（`Code 516`） |
| 刪掉舊 Secret、刪 pod 重啟 | 密碼不變，日誌沒有再產生，新密碼仍被接受 |
| 輪替：`adminPasswordGeneration=2` | Secret 換成代 `2` 的新密碼（changed），仍綁定；新密碼被接受，前一個被拒（`Code 516`） |
| rollback 回輪替前的版本，再升級回代 2 | 兩次都 unchanged、代仍是 `2`、日誌沒有產生；密碼一直被接受 |
| 權限（`kubectl auth can-i --as`） | `trustiot-clickhouse`：get yes、patch yes、list no、create no；`default` 與 `trustiot-ingress-istio`：全部 no |

### lab 全新安裝：不需事先建立任何 Secret

[輸出](data/2026-10-15-403-clickhouse-admin-in-cluster/lab-fresh-install.txt)：安裝前 namespace 沒有任何 Secret；`helm install --wait` 12 秒內成功；init container `exit 0 Completed`，pod `Ready=True`，`nodeSelector` 是 `kubernetes.io/arch: arm64`；Secret 只有 `trustiot_admin` 一個 key、代 `1`、已綁定；ClickHouse 以 init container 寫的檔案登入成功（`SELECT currentUser()` 回 `trustiot_admin`）；檔案在伺服器容器裡是 `400 101:101`。uninstall 時 Helm 保留 Secret（`resource-policy: keep`），測試再刪除它與 PVC。

### 建置主機

[checks.txt](data/2026-10-15-403-clickhouse-admin-in-cluster/checks.txt)：fmt、兩種 clippy、workspace 測試（含 tools/migrate 的容器測試：Job 只用 init container 寫入的密碼、被拒時重讀、逾時訊息、首次安裝等待填入）、service-kit 無預設 feature 的測試、`bun test scripts`（843 通過、2 略過，略過的兩項需要 arm64 主機或 podman e2e，與這次無關）、`bun test deploy/flux`（19 通過）、`bun test deploy/chart`（378 通過）、`check-secret-handling.ts`、`check-workflows.ts` 都通過。typecheck 第一次有兩個型別錯誤，`a91f0e5` 修正後通過；在 `a91f0e5`（最後的程式碼，Rust 部分與第一次相同）再跑一次 typecheck、`bun test scripts`、`deploy/flux`、`deploy/chart` 與兩個 CI 腳本，全部通過。`cargo deny` 的 advisories 失敗於 RUSTSEC-2026-0346（rustls 0.23.45），main 一樣有，這個分支沒有改 `Cargo.lock`。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 全新安裝不需要事先建立 ClickHouse 管理帳號 Secret，migration Job 與 ClickHouse 都能啟動；nightly smoke 與 lab e2e 證明 | ClickHouse 部分通過（lab 全新安裝與叢集測試）。Job 以產生的密碼連上 ClickHouse：tools/migrate 的容器測試通過（首次安裝等待填入、只用綁定的值、被拒時重讀）；在叢集上要 Postgres，lab 目前沒有 CloudNativePG，所以由 nightly smoke 驗證（不再建立假的管理帳號 Secret，並檢查產生的 Secret）：**待測**，要在 PR 加上 `smoke` label 跑一次 |
| 輪替有文件化步驟並經 lab 實測 | 通過：`docs/runbooks/secrets.md` 的步驟（調大 `adminPasswordGeneration`）在 lab 實測，新密碼生效、舊密碼被拒，rollback 不換 |
| 服務與 collector 的 pod 讀不到此 Secret，有 chart 測試證明 | 通過：chart 測試檢查自訂 `env` 讀它時渲染失敗、它不掛進任何服務或 collector 的 pod、chart 只有 ClickHouse 的 Role 能 get/patch 它、Job 的 Role 只能 get；lab 以 `auth can-i` 確認其他 ServiceAccount 讀不到 |
| `docs/architecture/secrets.md`、`docs/runbooks/secrets.md`、`deploy/chart/README.md`、`deploy/flux/` 更新，移除人建立的步驟 | 通過：四處都已更新，另更新 `docs/architecture/ci-release.md`、`deploy/chart/tests/e2e/README.md`、`tools/migrate/README.md`、`deploy/images/README.md` |

清理：namespace `e2e-403` 已刪除（PVC 與 Secret 隨之刪除，沒有留下這次的 PV），節點上的 `migrate:403` 映像、本機的映像、OCI 封存檔、kubeconfig 與 SSH 轉送都已移除，lab 鎖已釋放。
