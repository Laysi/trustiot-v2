# 週期性重讀：在執行中的部署驗證照常重讀與 panic 後的恢復

對應 issue：Laysi/trustiot-v2#851（延後自 Laysi/trustiot-v2#841，PR Laysi/trustiot-v2#850）。

Laysi/trustiot-v2#841 讓三個每副本各自執行的重讀 loop 改用 `crates/service-kit` 的 `periodic`（`run_every`、`run_blocking_every`），一次 panic 之後下個週期照常再跑：certs 的 `CaState::watch`（排程 `ca-reload`，每 30 秒）、service-kit 的 `PublishedKeys::watch`（`internal-identity-keys-reload`，每 5 秒）與身分服務 issuer 的 `watch`（`identity-signing-keys-reload`，每 5 秒）。當時只有單元測試，這裡在 lab 的 k3s 上的執行中 pod 驗三件事：

1. `deploy/chart/tests/e2e/identity-keys.e2e.ts` 以包含 Laysi/trustiot-v2#841 的映像通過：金鑰輪替時每個 pod 不重啟就載入新公鑰，身分服務從 `signs_from` 起以新金鑰簽章。
2. `helm upgrade` 換掉 certs 掛載的 CA 檔案後，檔案出現在 pod 裡之後的第一次重讀就載入（`loaded the issuing CA`），pod 不重啟、readiness 一直是 ready。
3. 測試映像在重讀的寫鎖內 panic 一次：記一行 ERROR（`job` 是 `ca-reload`），readiness 維持 ready，下一個週期修好鎖並載入新檔案。正式映像沒有任何觸發方式。

## 環境

| 項目 | 內容 |
| --- | --- |
| 分支 | `test/851-periodic-reload-cluster`，從 `main` 的 `f3a2afa4`（已含 PR Laysi/trustiot-v2#850）分出；映像建自 commit `c016f815`（加上測試用 feature，其餘程式與 `f3a2afa4` 相同） |
| 映像 | 建置主機（x86_64 Linux），2026-11-01 04:34 UTC：pinned zig 0.16.0 與 cargo-zigbuild 交叉編譯成 `aarch64-unknown-linux-musl`（與 `scripts/release/rust-build.ts` 相同的 target，預設 feature），以 podman（這次工作自己的 store）打包成 arm64 映像，不在 arm64 映像裡執行任何東西。`identity`、`migrate`、`example-service` 與正式的 `certs` 以預設 feature 建出；certs 的測試映像 `certs-reload-panic` 是同一份程式加上 `--features e2e-reload-panic`。二進位與映像的摘要見 [`build-images-output.txt`](data/2026-11-01-851-periodic-reload-cluster/build-images-output.txt) |
| 測試用根 | `certs-ca root create` 建三個 30 天的測試根：`TrustIoT e2e-851 test root A`、`… root B`、`… root C e2e-reload-panic`（C 的名稱帶 feature 的標記）。私鑰只在建置工作的 shell 變數裡，建完就丟，沒有寫進檔案 |
| 叢集 | lab 的 Mac mini M1，單節點 k3s v1.36.5+k3s1（arm64，Istio ambient、CloudNativePG operator 已裝），持有 lab 鎖（owner 行 `Laysi/trustiot-v2#851 <時間>`，04:37:51Z 取得、05:03 釋放），namespace `e2e-851`（Pod Security `restricted`），每一段之前刪掉重建 |
| Postgres | 節點上已有的 `ghcr.io/laysi/trustiot-v2/postgres:0.0.0-rc.12@sha256:8fa498eb…`（與 PR Laysi/trustiot-v2#921 相同） |
| chart | `deploy/chart`。身分金鑰：`values-saas.yaml` + `tests/values/saas-instance.yaml` + `tests/e2e/values-identity-keys-e2e.yaml`（測試本身的設定）。certs：`values-onprem.yaml` + `tests/values/onprem-instance.yaml` + 本次的 [`values-ca-reload-e2e.yaml`](data/2026-11-01-851-periodic-reload-cluster/values-ca-reload-e2e.yaml)（bundled Postgres 與 ClickHouse，collector 關掉，identity 由 example-service 扮演，做法同 PR Laysi/trustiot-v2#921）；`ca.reload_interval` 用預設的 30 秒 |
| 工作站 | x86_64 Linux，bun 1.4.2，helm 與 kubectl 是 `.prototools` 釘選的版本；經會自動重連的 SSH 轉發連 API server，admin kubeconfig 只在 repository 以外的私有目錄（0600），結束時刪除 |

## 測試用的 panic（只在測試映像）

`services/certs/Cargo.toml` 新增 feature `e2e-reload-panic`，預設關閉。打開時，一個行程第一次把已載入的 CA 換成根主體含 `e2e-reload-panic` 的 CA 時，在 `Loaded::apply` 裡 panic 一次：此時重讀持有寫鎖、已記下新檔案的指紋、還沒存入新的 CA，與單元測試 `a_reload_that_panicked_holding_the_lock_is_repaired_by_the_next_one` 注入的位置相同。行程的第一次載入不觸發，所以以這種根啟動的 pod 照常啟動。說明在 `services/certs/README.md`「測試映像的重讀 panic」。

正式映像不受影響的證據：

| 檢查 | 結果 |
| --- | --- |
| 以預設 feature 建出的 `certs`（與發佈相同的 target）裡找 panic 訊息 `injected reload panic` | 0 次；測試建置 `certs-reload-panic` 裡 1 次（[`build-images-output.txt`](data/2026-11-01-851-periodic-reload-cluster/build-images-output.txt)） |
| 發佈的建置指令 | `scripts/release/rust-build.ts` 的 `rustCrossCompileCommand` 不帶任何 `--features`，`scripts/release/rust-build.test.ts` 逐項斷言整個指令；repository 裡沒有任何腳本或 workflow 帶 `--features` 或 `--all-features` |
| 依賴或 feature 打開它 | `tools/repo-check` 的 `TEST_ONLY_FEATURES` 加上 `("certs", "e2e-reload-panic")`：任何正式或建置依賴、其他 feature 或 `default` 打開它時，`cargo test -p repo-check` 的 `test_only_features_are_only_enabled_by_dev_dependencies` 失敗；單元測試 `test_only_features_are_enabled_only_by_dev_dependencies` 補上 certs 的兩種違規 |
| 程式碼 | 唯一的呼叫在 `#[cfg(feature = "e2e-reload-panic")]` 之下，模組本身也是；沒有設定、環境變數或 API 讀取路徑 |

## 指令

全部從 repository 根目錄執行。

1. 單元測試（建置主機）：

   ```sh
   cargo clippy --locked -p certs --all-targets -- -D warnings
   cargo clippy --locked -p certs --all-targets --features e2e-reload-panic -- -D warnings
   cargo test --locked -p certs --lib --features e2e-reload-panic authority
   cargo test --locked -p repo-check
   ```

2. 映像與測試用根（建置主機，x86_64 Linux）：

   ```sh
   bash docs/test-reports/data/2026-11-01-851-periodic-reload-cluster/build-images.sh
   ```

   產出在建置主機的 `~/.cache/trustiot-v2-851/`：四個映像的 oci-archive 與 `root-a.pem`、`root-b.pem`、`root-c.pem`。

3. lab 上的準備（操作者手動）：取得 lab 鎖（owner 行 `Laysi/trustiot-v2#851 <時間>`）；把四個 oci-archive 經 ssh 串流進節點的 `k3s ctr -n k8s.io images import --platform linux/arm64 -`；建立 namespace：

   ```sh
   kubectl create namespace e2e-851
   kubectl label namespace e2e-851 pod-security.kubernetes.io/enforce=restricted pod-security.kubernetes.io/enforce-version=latest
   ```

4. 兩段測試（工作站），中間刪掉並重建 namespace：

   ```sh
   : "${LAB_SSH:?set LAB_SSH to the ssh host of the lab node}"
   export LAB_SSH
   POSTGRES=ghcr.io/laysi/trustiot-v2/postgres:0.0.0-rc.12@sha256:8fa498ebe48aa084c140d50869650ca6d79fd0d7c5d9172da283b12cfd983f57
   STEP=identity-keys WORK=<repository 以外的私有目錄> E2E_POSTGRES_IMAGE="$POSTGRES" \
     bun docs/test-reports/data/2026-11-01-851-periodic-reload-cluster/lab-run.ts
   STEP=certs WORK=<另一個私有目錄> ART=<放 root-a.pem、root-b.pem、root-c.pem 的目錄> E2E_POSTGRES_IMAGE="$POSTGRES" \
     bun docs/test-reports/data/2026-11-01-851-periodic-reload-cluster/lab-run.ts
   ```

   [`lab-run.ts`](data/2026-11-01-851-periodic-reload-cluster/lab-run.ts) 先確認 lab 鎖的 owner 行是這個 issue、namespace 是 `e2e-<數字>`、`WORK` 不在 repository 裡，再建 SSH 轉發。

   - `STEP=identity-keys`：原封不動執行 `deploy/chart/tests/e2e/identity-keys.e2e.ts`（saas 型態，`E2E_IMAGE_PULL_POLICY=Never`，四個映像是本次的 `localhost/trustiot-e2e/<名稱>:851`），結果寫到 `WORK/out`。
   - `STEP=certs`：`helm install`（根 A）並等 certs 載入 A；`helm upgrade` 換成根 B，等 ConfigMap 更新、等 kubelet 把新檔案換進 pod 的 volume（在節點上讀 `..data` 指向的目錄，名稱是換入的 UTC 時間，到秒；映像沒有 shell，無法從 pod 裡看）、等 certs 記 `loaded the issuing CA` 根 B；`helm upgrade` 換成根 C，等換入、等 certs 載入 C，再多等一個週期加 10 秒。每次等待都取一次 certs pod 的 readiness。每一步存下結果後逐項斷言驗收要的值（見下方「結果」），不符就以失敗結束。

5. 收尾（操作者手動，失敗時也要做）：刪掉 namespace `e2e-851`、從節點移除匯入的 `localhost/trustiot-e2e/*:851` 映像、釋放 lab 鎖。這次都做了：namespace 不存在、節點上沒有 `:851` 的映像、沒有 `e2e-851` 的 PV、鎖已移除。

## 結果

### 1. `identity-keys.e2e.ts`（驗收條件 1）

saas 型態 8 通過、0 失敗，744.5 秒（2026-11-01 04:44–04:57 UTC）。輸出：[`identity-keys-e2e-output.txt`](data/2026-11-01-851-periodic-reload-cluster/identity-keys-e2e-output.txt)；測試的報告 JSON（只有 kid、代與時間）：[`identity-keys-e2e-saas.json`](data/2026-11-01-851-periodic-reload-cluster/identity-keys-e2e-saas.json)。

和週期性重讀有關的步驟：

| 步驟 | 經過的 loop | 結果 |
| --- | --- | --- |
| 輪替（`keyGeneration=2`）：已經在跑的資產服務 pod（example service）載入新舊兩把公鑰，同一個 pod、重啟次數不變 | `PublishedKeys::watch` | 公開後 50.2 秒 |
| 輪替：身分服務載入兩把私鑰與公鑰 | issuer `watch`、`PublishedKeys::watch` | 公開後 2.5 秒（新 pod） |
| 輪替：身分服務改用新金鑰（`signing internal identity tokens` 的日誌） | issuer `watch` 的監看 | `signs_from` 之後 2.5 秒；之後的 token 是新 kid，200 |
| 移除舊金鑰：資產服務不重啟就換成只有代 2 的一組 | `PublishedKeys::watch` | init container 完成後 34.2 秒 |
| 撤銷（`keyGeneration=3`、`revokeBeforeGeneration=3`）：資產服務不再列被撤銷的 kid | `PublishedKeys::watch` | 公開後 55.2 秒；身分服務 1.4 秒後以代 3 簽章，token 200 |

其餘三項（全新安裝、權限、rollback）、沒有私鑰外流與 uninstall 也都通過。時間落在 kubelet 的定期同步（約 1 分鐘加上隨機延後）加 5 秒的重讀之內，與 2026-10-15 的 [Laysi/trustiot-v2#404 報告](2026-10-15-404-identity-keys-in-cluster.md)（55.4／59.9 秒、78.9／84.4 秒）同一個量級。

### 2. CA 檔案的重讀（驗收條件 2）

同一個 certs pod `trustiot-certs-6bb9bd79f4-hlqs8`（uid `c0f103e1-…`，04:57:58Z 啟動、04:57:59Z ready），`helm install` 38.3 秒。certs 啟動時載入根 A（04:57:58.079Z），之後每 30 秒重讀一次，所以重讀的時間點是 04:57:58.08 加上 30 秒的整數倍。

| 時間（UTC） | 事件 |
| --- | --- |
| 04:58:02.429 | `helm upgrade` 開始（根 B） |
| 04:58:09.999 | helm 完成 |
| 04:58:10.260 | ConfigMap 已是根 B |
| 04:59:18 | kubelet 把新檔案換進 pod 的 volume（`..data` → `..2026_11_01_04_59_18.…`） |
| 04:59:28.087 | INFO `loaded the issuing CA`，`root` = `CN=TrustIoT e2e-851 test root B`：換入後 10.1 秒，就是換入之後的第一個重讀時間點（04:57:58.08 + 3 × 30 秒） |

斷言：同一個 pod、重啟 0 次、啟動時間不變；Ready 從 04:57:59Z 起沒有變過（`lastTransitionTime` 早於 upgrade）；這段沒有 ERROR；載入在換入後一個週期之內（10.1 ≤ 30 + 5 + 1 秒）。helm 完成到載入共 78.1 秒，大多是 kubelet 的同步（68 秒）。

### 3. 重讀 panic 之後的恢復（驗收條件 3）

同一個 pod 接著 `helm upgrade` 換成根 C（名稱帶標記），certs 的日誌（[`certs-ca-reload-log.jsonl`](data/2026-11-01-851-periodic-reload-cluster/certs-ca-reload-log.jsonl)，只有 CA 載入與 panic 相關的行）：

| 時間（UTC） | 層級 | 事件 |
| --- | --- | --- |
| 04:59:30.461 | | `helm upgrade` 開始（根 C）；04:59:37.964 helm 完成，04:59:38.215 ConfigMap 已是根 C |
| 05:00:25 | | kubelet 把新檔案換進 pod（`..2026_11_01_05_00_25.…`） |
| 05:00:28.088 | ERROR | panic hook：`thread panicked`，`location` `services/certs/src/authority.rs:981`，`payload` `injected reload panic (feature e2e-reload-panic, test images only)`（換入後第一個重讀時間點，3.1 秒後） |
| 05:00:28.088 | ERROR | `periodic job panicked; it runs again next period`，`job` = `ca-reload`（target `service_kit::periodic`） |
| 05:00:58.079 | WARN | `an earlier reload of the CA files panicked; reading them all again …`（下一個週期，修復 poison 的鎖、忘掉指紋） |
| 05:00:58.090 | INFO | `loaded the issuing CA`，`root` = `CN=TrustIoT e2e-851 test root C e2e-reload-panic`：panic 後 30.0 秒 |

斷言（全部通過）：排程的 ERROR 剛好一行，`job` 是 `ca-reload`；panic 的那次重讀是換入後的第一次（3.1 ≤ 36 秒）；WARN 與載入在 panic 之後的下一個週期（30.0 ≤ 35 秒）；根 C 只載入一次，之後再等 40 秒沒有第二次 panic（每個行程只觸發一次）；同一個 pod、重啟 0 次；Ready 從 04:57:59Z 起沒有變過；兩次 upgrade 期間取的 97 個 readiness 樣本全部 ready。helm history：1 `Install complete`、2 與 3 `Upgrade complete`。

原始資料：[`ca-reload-e2e.json`](data/2026-11-01-851-periodic-reload-cluster/ca-reload-e2e.json)（每一步的時間、pod 狀態、日誌行、readiness 樣本的統計）、[`lab-run-certs-output.txt`](data/2026-11-01-851-periodic-reload-cluster/lab-run-certs-output.txt)（進度輸出）。

### 同一天的第一次 certs 執行

上面是第二次。第一次（04:38–04:43 UTC，同一組映像與根）的 `lab-run.ts` 還沒有讀 kubelet 換入的時間，其餘步驟與斷言相同，全部通過：根 B 在 helm 完成後 78.4 秒、於重讀時間點 04:40:25.943 載入；panic 在 04:41:55.95（ERROR `job` = `ca-reload`），04:42:25.94 WARN 並載入根 C（30.0 秒後）；76 個 readiness 樣本全部 ready、重啟 0 次。事後在節點上讀到根 C 換入的時間是 04:41:37，panic 是之後第一個重讀時間點。為了直接證明「檔案出現在 pod 之後的第一次重讀」，把讀換入時間與對應的斷言加進 `lab-run.ts` 再跑一次，就是上面的結果。

### 單元測試與建置主機的檢查

- `cargo test -p certs --lib --features e2e-reload-panic authority`：3 通過，含新的 `the_test_images_reload_panic_fires_once_and_is_repaired`（第一次載入帶標記的根不 panic；從根 A 換成帶標記的根 B 時在鎖內 panic、讀者仍拿到 A；下一次 `periodic_reload` 載入 B 並清掉 poison；再換成帶標記的根 C 時不再 panic）。
- 預設 feature 下這個測試不編譯，`cargo test -p certs --lib authority` 是原本的 2 個。
- `cargo clippy -p certs --all-targets` 在有與沒有這個 feature 時都沒有警告；`cargo test -p repo-check` 28 通過。
- 最終 tree 的整個 workspace 檢查記在 PR 內文。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| `identity-keys.e2e.ts` 以包含 Laysi/trustiot-v2#841 的映像在 lab 的 k3s 通過，結果記在 `docs/test-reports/` | 通過：saas 型態 8／8，輪替時資產服務的 pod 不重啟、50.2 秒後載入新公鑰，身分服務在 `signs_from` 之後 2.5 秒改用新金鑰 |
| 執行中的部署更新 CA 檔案之後，certs 不重啟就在下一個重讀週期內載入；日誌與 readiness 記在 `docs/test-reports/` | 通過：kubelet 換入後 10.1 秒、在下一個重讀時間點記 `loaded the issuing CA`（根 B），同一個 pod、重啟 0 次，Ready 沒有變過 |
| pod 內一次重讀 panic 之後：記一行 ERROR（`job` 是排程名稱）、readiness 維持 ready、下一個週期照常重讀並載入新檔案；正式映像的行為不變 | 通過：一行 ERROR `job` = `ca-reload`（另有 panic hook 的 `thread panicked`），30.0 秒後的下一個週期 WARN 修復並載入根 C，97 個樣本全部 ready、重啟 0 次；panic 只在 `e2e-reload-panic` feature 的測試建置裡，正式建置的二進位不含它，`repo-check` 防止任何正式依賴或預設打開它 |

依 trustiot-v2 的驗證分級延後、另開低優先級 issue 的進階驗證：

- 把這次手動的叢集流程自動化成 `deploy/chart/tests/e2e` 的 e2e 測試（含 namespace、匯入映像與鎖的收尾）。
- 以另一種型態重跑（`identity-keys.e2e.ts` 的 onprem、certs 的 saas），以及 certs 多副本時每個副本各自重讀與恢復。
- 另外兩個 loop（`PublishedKeys::watch`、issuer `watch`）與根 CRL 重讀在 pod 內的 panic 注入：目前由單元測試涵蓋（PR Laysi/trustiot-v2#850），叢集裡只驗了照常重讀。
