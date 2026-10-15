# 內部身分簽章金鑰在叢集內產生（2026-10-15）

Laysi/trustiot-v2#404：身分服務簽內部身分 token 的 Ed25519 金鑰改由身分服務 pod 的 init container（`migrate identity-signing-keys`）在 chart 宣告的空 Secret `<release>-identity-signing-keys` 產生，公鑰寫進 ConfigMap `<release>-internal-identity-keys`，每個服務掛這個 ConfigMap、每 5 秒重讀，不重啟就換上新的一組。輪替與撤銷是調大 `internalIdentity.keyGeneration` 與 `revokeBeforeGeneration`，人不經手金鑰。設計見 `docs/architecture/secrets.md`「內部身分簽章金鑰」，操作見 `docs/runbooks/secrets.md`。

## 環境

- **程式碼**：分支 `feat/404-identity-key-in-cluster`，合併 main `c5ce531` 之後的工作目錄（當時還沒提交）。lab 測試之後，提交時的 pre-commit 審查要求改了兩處 Rust：私鑰在每條路徑都從記憶體清除（`crates/identity-keys`、`tools/migrate`、`services/identity`），以及 init container 看到 ConfigMap 已經是自己的那組公鑰時，也要確認 Secret 沒有被另一次執行改過才算完成（`tools/migrate`，只影響同時執行的情況）。兩者都不改變 lab 測到的步驟與時間，由建置主機的第二次檢查驗證（下方「建置主機」）；chart 樣板與 lab 測試的相同。
- **執行檔**：在建置主機（經檢查佇列）以 `cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p migrate --bin migrate -p identity --bin identity -p service-kit --bin example-service` 建出（zig、cargo-zigbuild 由 proto 提供）。SHA-256：`migrate` `649f5d7a307c7a3427fe0a034d34f36ba2e194e406c8a2b6a41bf9c8b24a4ee3`、`identity` `78b31a5133dc1f291f739fda35f6226411b959995934f922876b13d4de51a568`、`example-service` `52bfaa6ae7daa2cb3980f00d47cead04fce73f59d9d31235e90c2b4f1a8ea939`。
- **映像**（linux/arm64）：本機以 podman 打包，不執行 `RUN`：`localhost/trustiot-e2e/migrate:404`（`deploy/images/rust-binary.Containerfile`，image ID `814f55a182be…`）、`identity:404`（同一個 Containerfile，`997dd323438f…`）、`example-service:404`（`deploy/chart/tests/e2e/` 的 example service Containerfile，`8e616592e10e…`）。帶 AGE 的 Postgres 映像 `postgres:404` 在建置主機以 podman 從 `deploy/images/postgres` 建出（`b602821f1e77…`）。四個映像存成封存檔，在節點上以 `sudo k3s ctr -n k8s.io images import` 匯入；ClickHouse 是 `values.yaml` 釘選的 digest。
- **lab 的 Mac mini M1**：aarch64、Fedora Linux Asahi Remix 44、k3s v1.36.5+k3s1（containerd 2.3.4-k3s1.36）、Istio 1.30.5 ambient、CloudNativePG 1.30.1（叢集上既有的，沒有改動）。取得 lab 鎖後建立 namespace `e2e-404`（Pod Security `restricted`）。測試以節點的管理者 kubeconfig 操作（只動 `e2e-404`），kubeconfig 只放在本機私有的暫存目錄（0600），API server 經會自動重連的 SSH 轉送，測完刪除。
- **工作站**：x86_64 Linux，bun 1.4.2、helm 4.3.0、kubectl 1.36.5（proto 釘選的版本）。
- **設定**（`deploy/chart/tests/e2e/values-identity-keys-e2e.yaml`）：叢集內 Postgres 與 ClickHouse、migration Job、真的身分服務，example service 扮演資產服務（驗證內部身分、回傳 token 裡的使用者）；`internalIdentity.keyPropagationSeconds` 120（schema 的下限，讓測試不必等預設的 600 秒）。
- **時間**（Asia/Taipei）：saas 12:52 到 13:04；on-prem 第一次 13:04 到 13:09 因網路中斷（見下方「結果」），重跑 13:13 到 13:25。

## 指令

[lab-run-e2e.sh](data/2026-10-15-404-identity-keys-in-cluster/lab-run-e2e.sh)，在 repo 根目錄：

```sh
KUBECONFIG=<kubeconfig> \
E2E_NAMESPACE=e2e-404 \
E2E_SHAPE=<saas|onprem> \
E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:404 \
E2E_IDENTITY_IMAGE=localhost/trustiot-e2e/identity:404 \
E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:404 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:404 \
E2E_IMAGE_PULL_POLICY=Never \
E2E_REPORT_DIR=<輸出目錄> \
bun test --timeout 3600000 ./deploy/chart/tests/e2e/identity-keys.e2e.ts
```

測試做的事（`deploy/chart/tests/e2e/README.md` 的「內部身分簽章金鑰」）：不事先建立任何金鑰，`helm install`；token 由測試以 `kubectl port-forward` 直接問身分服務的 ext_authz 端點（帶 app 主機的 Host 與測試在資料庫建立的 session cookie）取得，再送給 example service 驗證，只記 kid、HTTP 狀態與使用者是否相符；時間取自 pod 的狀態與服務的 INFO 日誌（載入的 kid）。測試行程讀 Secret 只為了比對私鑰有沒有出現在別處，私鑰不寫出。

## 結果

### lab 叢集測試：兩種 shape 各 8 通過、0 失敗

saas 725.7 秒、on-prem 718.5 秒。輸出：[saas](data/2026-10-15-404-identity-keys-in-cluster/lab-e2e-saas-output.txt)、[on-prem](data/2026-10-15-404-identity-keys-in-cluster/lab-e2e-onprem-output.txt)；報告 JSON：[saas](data/2026-10-15-404-identity-keys-in-cluster/identity-keys-e2e-saas.json)、[on-prem](data/2026-10-15-404-identity-keys-in-cluster/identity-keys-e2e-onprem.json)。kid 是公鑰的 thumbprint，不是祕密。

| 步驟 | saas | on-prem |
| --- | --- | --- |
| 全新安裝：`helm install --wait` | 32.7 秒 | 36.7 秒 |
| 全新安裝：Secret 只有代 1 的一把，ConfigMap 只有它的公鑰，立即簽章 | 是 | 是 |
| 全新安裝：金鑰公開之後，身分服務主容器啟動（中間等 migration Job）／開始簽章／就緒 | 27／27.4／28 秒 | 23／23.8／25 秒 |
| 全新安裝：金鑰公開之後，資產服務主容器啟動／載入公鑰／就緒 | 27／27.4／28 秒 | 24／29.9／31 秒 |
| 全新安裝：身分服務簽的 token 被資產服務接受 | 200，使用者相符 | 200，使用者相符 |
| 權限：namespace 的 7 個 ServiceAccount 各問 14 個問題（`kubectl auth can-i --as`） | 只有 `trustiot-identity` 對 Secret 與 ConfigMap 的 `get`、`patch` 是 yes | 同左 |
| 輪替（`keyGeneration=2`）：升級開始到新公鑰公開 | 3.1 秒 | 2.9 秒 |
| 輪替：`signs_from` 之前 110 秒取得的 token 是舊 kid，被接受（此時新公鑰已公開） | 200 | 200 |
| 輪替：已經在跑的資產服務 pod 載入新舊兩把（同一個 pod，重啟次數不變） | 公開後 55.4 秒 | 公開後 59.9 秒 |
| 輪替：新的身分服務 pod 載入兩把私鑰與公鑰 | 公開後 2.6 秒 | 公開後 3.1 秒 |
| 輪替：身分服務改用新金鑰簽章 | `signs_from` 之後 2.6 秒 | `signs_from` 之後 3.1 秒 |
| 輪替：之後的 token 是新 kid，被接受 | 200 | 200 |
| `helm rollback` 回代 1，再升級回代 2 | Secret 與 ConfigMap 都沒有改變，token 200 | 同左 |
| 舊金鑰移除：等到 `signs_from + 120 + 305` 秒之後重啟身分服務 | 等 406.5 秒；Secret 與 ConfigMap 只剩代 2；資產服務 9.4 秒後不再列舊 kid；token 200 | 等 399.8 秒；同左，3.9 秒後 |
| 撤銷（`keyGeneration=3`、`revokeBeforeGeneration=3`）：升級開始到新公鑰公開 | 2.8 秒 | 3.5 秒 |
| 撤銷：只剩代 3、`signs_from = published_at`；新的身分服務 pod 開始簽章／就緒 | 公開後 1.8／2.0 秒 | 公開後 2.2／4.0 秒 |
| 撤銷：已經在跑的資產服務 pod 不再列被撤銷的 kid | 公開後 84.4 秒 | 公開後 78.9 秒 |
| 撤銷之後的 token | 200，新 kid | 200，新 kid |
| 三把私鑰都不在 ConfigMap、事件、身分服務與資產服務的日誌、init container 的日誌 | 沒有找到 | 沒有找到 |
| `helm uninstall` 之後 | 保留 Secret 與 ConfigMap（`keep`），Secret 仍是代 3 那一把 | 同左 |

- 已經在跑的 pod 換上新公鑰的時間落在 3.9 到 84.4 秒，與 kubelet 的定期同步（預設 1 分鐘，每次隨機延後）加上 5 秒的重讀相符，在 `keyPropagationSeconds` 的下限 120 秒之內；預設的 600 秒留了 7 倍以上的餘裕。撤銷時新的身分服務 pod 2 到 4 秒內就以新金鑰簽章，但資產服務最久 84.4 秒後才拒絕被撤銷的金鑰，所以 runbook 的撤銷步驟要人逐一確認每個 pod。
- 全新安裝時，身分服務的主容器在 migration Job 完成後才啟動，啟動時已經讀到 init container 寫入的內容。資產服務在 on-prem 那次啟動時還是空的，5.9 秒後才讀到，所以就緒比 saas 那次多等幾秒。
- init container 的日誌只有 kid、代與時間（JSON 的 `initLog`）。最後一行的欄位 `secret` 顯示 `[REDACTED]`：service-kit 遮蔽名稱含 `secret` 的日誌欄位，這裡的值只是 Secret 的名稱，與 `clickhouse_admin.rs` 的同類日誌相同，保留現狀。
- on-prem 第一次執行時，工作站到 lab 的連線在 13:08 起中斷（經 SSH 轉送連 API server 時 TLS handshake 逾時），全新安裝與權限兩項通過後，輪替的 `helm upgrade` 失敗，之後各項連不到 API server。這不是被測的程式造成的：手動 uninstall 並刪除保留的 Secret、ConfigMap、PVC、Cluster 與 ImageCatalog 後，以同一組映像重跑一次，就是上表的 on-prem 結果。

### 自動化測試

- `deploy/chart/tests/chart.test.ts`：只有身分服務的 ServiceAccount 能 get、patch 這個 Secret 與 ConfigMap（以每個可選元件都開啟的設定渲染，檢查每個 Role、ClusterRole 與綁定）；只有身分服務的 pod 掛 Secret，身分服務的 `automountServiceAccountToken` 是 `true` 時渲染失敗；每個服務掛 ConfigMap；身分服務沒有啟用時三者都不宣告；輪替與撤銷只改身分服務的 pod template。
- `tools/migrate/tests/signing_keys.rs`：init container 對假的 API 與假的時鐘（全新安裝、輪替的順序、移除時間、撤銷、中途停掉、同時執行、較舊的一次執行不撤下較新的公開也不把只是還沒更新的 ConfigMap 當成完成、uid 不符、匯入人建立的私鑰、上限 8 把、私鑰不出現在日誌與 API 回應以外）。
- `crates/service-kit/tests/identity.rs`：`a_rotation_reaches_a_running_service_through_its_keys_file`（服務不重啟換上新的一組）、`a_missing_or_unusable_keys_file_keeps_the_service_unready`，以及 `src/identity/published.rs` 的單元測試（壞掉或不見的檔案保留上一組）。
- `services/identity/tests/ext_authz.rs` 的 `a_key_rotation_needs_no_restart_and_tokens_verify_throughout`（對執行中的身分服務走完整個輪替：重疊期間兩把金鑰的 token 在每個服務都驗證得過，`signs_from` 起改用新金鑰，舊金鑰移除後它的 token 被拒）與 `a_revoked_generation_never_signs`；`services/identity/tests/adversarial.rs` 的 `replicas_switch_keys_by_the_schedule_without_a_restart` 與 `the_service_signs_nothing_without_a_usable_published_signing_key`。

### 建置主機

[checks.txt](data/2026-10-15-404-identity-keys-in-cluster/checks.txt)，兩次：

- 第一次（13:39 到 13:54，審查修正之前的程式碼）：fmt、workspace clippy、`cargo test --workspace`（`CI=1`，容器測試不略過）、service-kit 無預設 feature 的 clippy 與測試、`cargo run -p openapi -- --check`、`cargo deny --locked check`、`bun test scripts`（888 通過、4 略過、0 失敗）、typecheck、`bun test deploy/flux`（19 通過）、`check-secret-handling.ts`、`check-workflows.ts`、`lint-workflows.ts`、`gitleaks dir` 都通過。`bun test deploy/chart` 407 通過、1 失敗：kubeconform 從網路抓 schema 超過 60 秒被終止，與這次的變更無關，第二次重跑通過。
- 第二次（13:54 到 14:00，最終的程式碼）：fmt、workspace clippy、`cargo test -p identity-keys -p migrate -p identity -p service-kit`、`bun test deploy/chart`（408 通過）都通過。變異測試：把 init container 對 Secret 的確認拿掉（`return Ok(true)`），新的 `a_published_view_counts_only_while_the_secret_is_unchanged` 失敗，還原後通過。
- lab 用的 aarch64 執行檔也是在建置主機建的（上方「環境」）。本機另跑了 `bun test scripts/nightly/identity-keys.test.ts`、chart README 的欄位檢查與 `check-secret-handling.ts`（文件修改之後）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 全新安裝不需要事先建立私鑰或填 `publicKeys`，服務都能啟動並驗證身分服務簽的 token；nightly smoke 證明 | lab 通過（兩種 shape：不建立任何金鑰，身分服務與資產服務在金鑰公開後 25 到 31 秒就緒，token 被接受）。nightly 已改為不建立金鑰、檢查 Secret 與 ConfigMap 並記下就緒時間，偽造身分標頭的檢查改用從身分服務 ext_authz 拿到的真 token：**待測**，要在 PR 加上 `smoke` label 跑一次，k3d 上的全新安裝時間也在那時量 |
| 輪替不中斷：輪替期間舊 token 仍能驗證，新 token 在所有服務都能驗證；有自動化測試與 lab 實測證據 | 通過：Rust 的輪替測試（重疊期間兩把的 token 在每個服務都驗證得過）；lab 兩種 shape 實測輪替、rollback、移除與撤銷，資產服務不重啟，每次 token 都是 200 |
| 私鑰只有身分服務讀得到，有 chart 測試證明 | 通過：chart 測試；lab 以 `kubectl auth can-i` 問 namespace 的每個 ServiceAccount，只有身分服務的能 get、patch |
| `docs/architecture/secrets.md`、`docs/runbooks/secrets.md`、`deploy/chart/README.md`、`deploy/flux/` 更新 | 通過：四處都已更新（`deploy/flux/README.md`：values 不放金鑰或公鑰，輪替與撤銷調大 `values-instance.yaml` 的代），另更新 `docs/architecture/ci-release.md`、`docs/architecture/repository-layout.md`、`deploy/chart/tests/e2e/README.md`、`tools/migrate/README.md`、`deploy/images/README.md`、`crates/service-kit/README.md`、`services/identity/README.md`、`scripts/README.md`，新增 `crates/identity-keys/README.md` |

清理：namespace `e2e-404` 已刪除（刪除前已沒有這次的物件，也沒有留下 PV），節點上的四個 `:404` 映像與封存檔已移除，lab 鎖已釋放，SSH 轉送已停止，kubeconfig 已刪除。
