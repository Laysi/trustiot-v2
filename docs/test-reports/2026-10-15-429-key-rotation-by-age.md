# 內部身分簽章金鑰依年齡自動輪替（2026-10-15）

Laysi/trustiot-v2#429：chart 的 CronJob `<release>-identity-key-rotation` 以身分服務的 ServiceAccount 執行 `migrate identity-signing-keys`，多一個設定 `TRUSTIOT__KEYS__MAX_KEY_AGE_SECONDS`。最新一把簽章金鑰公開超過 `internalIdentity.maxKeyAgeSeconds`（預設 90 天，0 關閉）而且沒有進行中的輪替時，它照手動輪替的排程加一把同一代的新金鑰：先公開，`keyPropagationSeconds` 之後才簽章，舊金鑰在它的 token 都過期之後的下一次執行移除。設計見 `docs/architecture/secrets.md`「依金鑰年齡自動輪替」，操作見 `docs/runbooks/secrets.md`。

## 環境

- **程式碼**：分支 `feat/429-identity-key-auto-rotation`，基於 main `385cf22`。lab 用的執行檔與 chart 都是提交 `4f8c4ac` 的內容。lab 第二次執行的 `key-rotation.e2e.ts` 比 `4f8c4ac` 多了只讀 kubectl 指令的重試與第 6 步每 30 秒的檢查（下方「結果」），在之後的提交裡。
- **執行檔**：在建置主機（經檢查佇列）以 `cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p migrate --bin migrate -p identity --bin identity -p service-kit --bin example-service` 建出（zig、cargo-zigbuild 由 proto 提供；`CARGO_ZIGBUILD_ZIG_PATH` 指向 proto 的 zig，因為 build script 的工作目錄在 repo 之外，proto 的 shim 找不到 `.prototools`）。SHA-256：`migrate` `ef7eccb14b3212500c186156a7ae04a228cfad4a5d4381a347cfa8a0c9339eed`、`identity` `d100880aa5fcb9557b06499a3967e947513554e4eceaaf733ca0dff5b1daf304`、`example-service` `52bfaa6ae7daa2cb3980f00d47cead04fce73f59d9d31235e90c2b4f1a8ea939`（與 Laysi/trustiot-v2#404 那次相同，service-kit 沒有改）。
- **映像**（linux/arm64）：本機以 podman 在這次專用的 store（vfs）打包，不執行 `RUN`：`localhost/trustiot-e2e/migrate:429`（`deploy/images/rust-binary.Containerfile`，image ID `b0aed4ed30ff…`）、`identity:429`（同一個 Containerfile，`2e58a24bc120…`）、`example-service:429`（`deploy/chart/tests/e2e/example-service/Containerfile`，`6ab1b126f45b…`）。帶 AGE 的 Postgres 映像沿用 Laysi/trustiot-v2#404 從 `deploy/images/postgres` 建出的那一個（`b602821f1e77…`；之後 `deploy/images/postgres` 沒有改），在節點上改標為 `postgres:429`。四個映像存成封存檔，在節點上以 `sudo k3s ctr -n k8s.io images import` 匯入；ClickHouse 是 `values.yaml` 釘選的 digest。
- **lab 的 Mac mini M1**：aarch64、k3s v1.36.5+k3s1、Istio 1.30.5 ambient、CloudNativePG 1.30.1（叢集上既有的，沒有改動）。取得 lab 鎖後建立 namespace `e2e-429`（Pod Security `restricted`）。測試以節點的管理者 kubeconfig 操作（只動 `e2e-429`），kubeconfig 只放在本機私有的暫存目錄（0600），API server 經會自動重連的 SSH 轉送，測完刪除。
- **工作站**：x86_64 Linux，bun 1.4.2、helm 4.3.0、kubectl 1.36.5（proto 釘選的版本）。
- **設定**：`deploy/chart/tests/e2e/values-identity-keys-e2e.yaml`（與 Laysi/trustiot-v2#404 相同：叢集內 Postgres 與 ClickHouse、migration Job、真的身分服務，example service 扮演資產服務；`keyPropagationSeconds` 120），另外 `internalIdentity.maxKeyAgeSeconds=900`、`keyRotationSchedule="* * * * *"`（每分鐘一次），只測 saas shape（CronJob 與兩種 shape 的差異無關：兩者用同一個樣板、同一個 ServiceAccount 與 Role）。
- **時間**（Asia/Taipei）：第一次 15:29 到 16:14（第 6 步中斷，見下方）；第二次 16:22 到 17:07。

## 指令

[lab-run-e2e.sh](data/2026-10-15-429-key-rotation-by-age/lab-run-e2e.sh)，在 repo 根目錄：

```sh
KUBECONFIG=<kubeconfig> \
E2E_NAMESPACE=e2e-429 \
E2E_SHAPE=saas \
E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:429 \
E2E_IDENTITY_IMAGE=localhost/trustiot-e2e/identity:429 \
E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:429 \
E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:429 \
E2E_IMAGE_PULL_POLICY=Never \
E2E_REPORT_DIR=<輸出目錄> \
bun test --timeout 3600000 ./deploy/chart/tests/e2e/key-rotation.e2e.ts
```

## 結果

### lab 叢集測試：7 通過、0 失敗

第二次執行 2688.9 秒。輸出：[lab-e2e-saas-output.txt](data/2026-10-15-429-key-rotation-by-age/lab-e2e-saas-output.txt)；報告 JSON：[key-rotation-e2e-saas.json](data/2026-10-15-429-key-rotation-by-age/key-rotation-e2e-saas.json)。kid 是公鑰的 thumbprint，不是祕密。

| 步驟 | 結果 |
| --- | --- |
| 全新安裝：`helm install --wait` | 30.9 秒；Secret 只有代 1 的一把 |
| CronJob：以 `trustiot-identity` 執行，不掛 Secret、不從 Secret 讀環境變數 | 是 |
| 權限：namespace 的 7 個 ServiceAccount 各問 14 個問題（`kubectl auth can-i --as`） | 只有 `trustiot-identity` 對 Secret 與 ConfigMap 的 `get`、`patch` 是 yes，與沒有 CronJob 時相同 |
| 金鑰公開之後的第一次執行（未到期） | 不改任何東西，日誌的 `rotation_due_at` = `published_at + 900` |
| 自動輪替：到期之後新金鑰公開 | 到期後 3 秒（每分鐘的排程）；同一代（代 1）；`signs_from` = `published_at + 120`；ConfigMap 列出兩把；那次執行的日誌是 `generated signing key <新 kid> (generation 1) to replace key <舊 kid>, published 2026-10-15T08:22:57Z, which reached internalIdentity.maxKeyAgeSeconds` |
| 自動輪替：已經在跑的資產服務／身分服務載入兩把公鑰 | 公開後 31.9／11.9 秒，同一個 pod |
| 自動輪替：身分服務改用新金鑰簽章 | `signs_from` 之後 1.9 秒 |
| 自動輪替：`signs_from` 之前 18.9 秒取得的舊 kid token，在切換之後 3.2 秒交給資產服務 | 200，使用者相符 |
| 自動輪替：之後的 token | 新 kid，200 |
| 自動輪替：持續請求（每 2 秒向身分服務要 token、交給資產服務驗證，從安裝後到切換後 30 秒） | 327 次，0 次失敗；kid 只從舊的換到新的一次 |
| 自動輪替：pod 重啟 | 身分服務與資產服務都是同一個 pod，重啟次數 0 → 0 |
| 移除舊金鑰：在 `signs_from + 120 + 305` 秒之後 | CronJob 的一次執行（不是重啟）在 57.3 秒後移除；資產服務 81.9 秒後只列新的一把，同一個 pod；token 200 |
| 自動輪替開著時的手動輪替（`keyGeneration=2`）：升級開始到代 2 公開 | 3.4 秒；只有一把代 2，`signs_from` 晚 120 秒；之後的 token 是它的 kid，200 |
| 自動輪替開著時的撤銷（`keyGeneration=3`、`revokeBeforeGeneration=3`）：升級開始到代 3 公開 | 2.8 秒；自動產生的代 1 金鑰與代 2 都被移除，只剩代 3、立即簽章；資產服務 35.9 秒後只列代 3；token 200 |
| `maxKeyAgeSeconds: 0`：升級之後 | CronJob 不存在；身分服務的 `TRUSTIOT_IDENTITY_MAX_KEY_AGE_SECONDS` 是 0 |
| `maxKeyAgeSeconds: 0`：等到金鑰 990 秒（超過 900 秒再加 90 秒），每 30 秒看一次 Secret | 等 943.1 秒、看 32 次，金鑰沒有換 |
| `maxKeyAgeSeconds: 0`：再重啟身分服務（init container 不依年齡輪替） | 金鑰沒有換（997.1 秒），ConfigMap 只有它；token 200 |
| 每一把私鑰（4 把）都不在 ConfigMap、事件、身分服務（含 init container）與資產服務的日誌、測試期間讀到的 4 次 CronJob 執行的日誌 | 檢查 9 份，沒有找到；4 次執行都成功，各 3 秒 |
| 測完 | release 與保留的 Secret、ConfigMap、PVC、Cluster、ImageCatalog 都已移除 |

- 自動輪替的每一個時間都在手動輪替的範圍內（Laysi/trustiot-v2#404 的報告：已經在跑的 pod 3.9 到 84.4 秒換上新的一組），`keyPropagationSeconds` 的下限 120 秒足夠：持續的請求沒有一次失敗。
- 舊金鑰的移除比 `retire_at` 晚 57.3 秒，是每分鐘一次的排程；預設每天一次時最多晚一天，不影響安全（舊私鑰仍只有身分服務讀得到）。
- 手動輪替時 CronJob 也在每分鐘執行，與升級的 init container 可能同時；結果只有一把代 2，與設計的「同時執行」相同。

**第一次執行（中斷）**：[輸出](data/2026-10-15-429-key-rotation-by-age/lab-e2e-saas-output-interrupted.txt)、[報告 JSON](data/2026-10-15-429-key-rotation-by-age/key-rotation-e2e-saas-interrupted.json)。前 5 步與第二次相同地通過（安裝 38.8 秒；到期後 16 秒公開，資產服務 16 秒後載入；持續請求 329 次 0 次失敗；CronJob 在 `retire_at` 之後 58.8 秒移除舊金鑰；手動輪替 4.2 秒、撤銷 5.7 秒公開）。第 6 步等待金鑰過期的 16 分鐘裡，工作站到 lab 的 SSH 連線逾時（`server <lab host> not responding`），轉送重連不上，之後的 kubectl 全部 `connection refused`，第 6、7 步與清理失敗。這不是被測的程式造成的：之後直接在節點上看，CronJob 已不存在，金鑰（代 3）已公開 1381 秒仍沒有換。手動 uninstall 並刪除保留的物件後，在 `key-rotation.e2e.ts` 加上「只讀的 kubectl 指令連不上 API server 時每 5 秒重試、最多 5 分鐘」（改變叢集的指令不重試）與第 6 步每 30 秒看一次，`lab-run-e2e.sh` 加上「等轉送接通才開始」（第二次執行的第一次嘗試因為轉送還沒接通就在 `helm status` 失敗，沒有建立任何東西），再以同一組映像跑第二次，就是上表。第二次沒有遇到斷線，重試沒有發生。

### 自動化測試

- `crates/identity-keys/src/tests.rs`：`a_key_is_rotated_once_it_reaches_the_maximum_age`（到期的邊界：差 1 秒不到期、剛好到期就輪替，新金鑰同一代、照排程公開與簽章）、`without_a_maximum_age_nothing_rotates_by_age`、`rotation_by_age_keeps_the_generation_and_the_values_stay_a_floor`（values 的代是下限、手動調大一定產生新的一代）、`revocation_removes_the_automatic_keys_of_older_generations`、`nothing_is_due_while_a_rotation_is_under_way_or_a_person_made_key_is_configured`、`rotations_at_the_shortest_age_keep_two_keys_and_always_a_signer`（最短年齡連續輪替時 Secret 最多兩把、隨時有簽章的金鑰）、`a_rotation_is_overdue_two_days_after_it_should_have_happened`、`a_rotation_by_age_is_reported_with_key_ids_and_dates_only`。
- `tools/migrate/tests/signing_keys.rs`（假的 Secret API 與假的時鐘）：`the_cronjob_rotates_a_key_that_reached_the_maximum_age`（未到期不動、到期輪替、之後的執行移除舊金鑰）、`manual_rotation_and_revocation_work_with_rotation_by_age_on`、`the_cronjob_and_an_init_container_with_a_new_generation_at_once`（兩者同時執行時以 `resourceVersion` 決定，只有一把新金鑰）。`tools/migrate/src/config.rs` 的設定檢查（範圍、與 `signingKeySecret` 互斥）。
- `services/identity`：`a_signing_key_overdue_for_its_rotation_is_reported_once`（金鑰超過輪替時間 2 天時記一次 ERROR，換了金鑰才再記），`tests/ext_authz.rs` 的設定錯誤案例。
- `deploy/chart/tests/chart.test.ts`：`the rotation CronJob runs the init container's command as the identity ServiceAccount, with no Role, binding or Secret mount of its own`（以每個可選元件都開啟的設定渲染，仍然只有身分服務的 ServiceAccount 能 get、patch Secret 與 ConfigMap；CronJob 的 pod 不掛 Secret、只掛 projected token；AuthorizationPolicy 沒有規則、NetworkPolicy `ingress: []`）、`the maximum key age and the schedule change the CronJob and the identity service's check only`，以及 schema 與樣板的拒絕案例（負數、超過 365 天、太短、時區前綴、比每天一次稀疏的排程、保留的服務名稱）。
- `scripts/nightly/identity-keys.test.ts`：nightly 對 CronJob 形狀與執行結果的檢查（`a CronJob that strays from the design is reported`、`a Job's outcome`）。nightly 本身（k3d 上以 `kubectl create job --from=cronjob/…` 執行一次）**待測**：要在 PR 加上 `smoke` label 跑一次。

### 建置主機

[checks.txt](data/2026-10-15-429-key-rotation-by-age/checks.txt)：最終的工作目錄（17:18 到 17:27）上 fmt、workspace clippy、service-kit 無預設 feature 的 clippy 與測試（136 通過）、`cargo test --workspace`（`CI=1`，容器測試不略過，758 通過、0 失敗）、`cargo run -p openapi -- --check`、`cargo deny --locked check`、typecheck、`bun test scripts`（897 通過、4 略過）、`bun test deploy/chart`（425 通過）、`bun test deploy/flux`（19 通過）、`check-secret-handling.ts`、`check-workflows.ts` 都通過。

- 第一次提交時 pre-commit 審查擋下：排程的 schema 接受 `@weekly`、`@monthly` 與只在某幾天執行的 cron，金鑰可能比上限多用好幾週。改為只接受 `@daily`、`@midnight`、`@hourly` 與日、月、星期三欄都是 `*` 的 cron，chart 測試加上五個拒絕案例，`bun test deploy/chart` 425 通過之後提交。
- 15:21 在提交之前的工作目錄跑過同一組檢查，也都通過。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 測試把上限設得很短：超過上限的金鑰被換掉時服務不中斷；重疊期間舊 token 仍能驗證，新 token 在每個服務都能驗證；自動化測試與 lab 實測 | 通過：`crates/identity-keys` 與 `tools/migrate` 的自動化測試；lab 以 900 秒的上限實測，持續請求 327 次 0 次失敗，切換前取得的舊 token 在切換後被接受，新 token 被接受，沒有 pod 重啟 |
| 不新增身分服務 ServiceAccount 以外的權限，chart 測試證明 | 通過：chart 測試；lab 以 `kubectl auth can-i` 問 namespace 的每個 ServiceAccount，結果與沒有 CronJob 時相同 |
| 手動輪替與撤銷在自動輪替開啟時照常運作，有測試 | 通過：`crates/identity-keys`、`tools/migrate` 的測試；lab 實測代 2 的手動輪替與撤銷（含自動產生的金鑰） |
| `maxKeyAgeDays: 0`（實作為 `maxKeyAgeSeconds: 0`）時不自動輪替 | 通過：chart 測試（不渲染 CronJob）；lab 等到超過上限 90 秒並重啟身分服務，金鑰沒有換 |
| 文件更新 | 通過：`docs/architecture/secrets.md`、`docs/runbooks/secrets.md`、`deploy/chart/README.md`，另更新 `docs/architecture/ci-release.md`、`deploy/chart/tests/e2e/README.md`、`crates/identity-keys/README.md`、`tools/migrate/README.md`、`services/identity/README.md` |

清理：namespace `e2e-429` 已刪除（刪除前已沒有這次的物件），節點上的四個 `:429` 映像與封存檔已移除，lab 鎖已釋放，SSH 轉送已停止，kubeconfig 已刪除；工作站上這次專用的 podman store 已刪除，使用者的 store 沒有動（`gcr.io/distroless/*` 仍是 amd64）。
