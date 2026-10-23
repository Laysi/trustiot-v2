# nightly 部署 telemetry-query：映像、歷史查詢 API 與租戶的 ClickHouse 帳號

對應 Laysi/trustiot-v2#539。

這份報告記錄 2026-10-23 在開發者工作站上能驗證的部分。驗收條件的主要證據是帶 `smoke` label 的 PR 觸發的 nightly run，它在 Kubernetes（k3d）上執行，工作站不跑 Kubernetes；run 結束後由 coordinator 把 nightly 的證據補進「結果」與「結論」。

## 環境

- 程式碼：分支 `ci/539-nightly-telemetry-query`，基於 main 的 `1cd3da21`。
- 開發者工作站（x86_64 Linux），Bun 1.4.2，其餘工具依 `.prototools` 釘選（helm 4.3.0、kubeconform 0.8.0、actionlint 1.7.12、flux 2.9.6、kubectl 1.36.5）。
- nightly（待補）：`.github/workflows/nightly.yml` 的 `smoke` job，在 PR 帶 `smoke` label 時執行，k3d 叢集，`nightly-onprem` 與 `nightly-saas` 兩個 namespace 各部署一次。

## 指令

```sh
# Bun scripts：安裝、型別檢查、測試（nightly 的單元測試在 scripts/nightly）
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts

# chart 與 GitOps
bun test deploy/chart
bun test deploy/flux

# CI 規則
bun scripts/ci/check-workflows.ts
bun scripts/ci/check-secret-handling.ts
proto install actionlint
bun scripts/ci/lint-workflows.ts

# nightly：PR 加上 smoke label 後由 .github/workflows/nightly.yml 執行
```

另外以 nightly 實際使用的 values 渲染 chart：對每個形態（`discoverShapes`），把 `apiValues(shape, routesOfValues(...))` 寫成 values 檔，與該形態的 values 檔一起 `helm template trustiot deploy/chart --namespace nightly-<shape>`，再把輸出交給 `kubeconform -strict -kubernetes-version <1.30.0|1.36.0>`，schema 來源與 chart 測試相同（預設加上 datreeio CRDs-catalog）。

## 結果

| 檢查 | 結果 |
| --- | --- |
| `bun run --cwd scripts typecheck` | 沒有錯誤 |
| `bun test scripts` | 1606 個測試，1597 pass、0 fail、9 skip |
| `bun test scripts/nightly` | 281 pass、0 fail |
| `bun test scripts/ci` | 488 pass、0 fail |
| `bun test deploy/chart` | 618 pass、0 fail |
| `bun test deploy/flux` | 76 pass、0 fail（含新的「README 打開 telemetry-query 的值與展示叢集 overlay 一起渲染」） |
| `check-workflows` | 4 個檔案，沒有違規 |
| `check-secret-handling` | 511 個檔案（6 個容器建置檔），沒有違規 |
| `lint-workflows` | 3 個 workflow 檔，沒有錯誤 |
| nightly values 渲染（onprem、saas） | `helm template` 成功、沒有警告；兩個形態都渲染出 `trustiot-telemetry-query` 的 NetworkPolicy、ServiceAccount、Service、Deployment、AuthorizationPolicy；Kubernetes 1.30.0 與 1.36.0 的 `kubeconform -strict` 都是 84 個資源 valid、0 invalid、0 error、0 skip |
| nightly smoke（CI） | 待 coordinator 在 smoke run 之後補上 |

`scripts/nightly` 的單元測試涵蓋：

- `NIGHTLY_IMAGES` 含 `telemetry-query`，快取鍵涵蓋 `services/telemetry-query` 與 `deploy/images/telemetry-query`（`image-cache.test.ts`）。
- 兩種形態的 API 檢查 values 渲染出映像為 `localhost/trustiot-e2e/telemetry-query:nightly` 的 Deployment、`/api/telemetry` 到 `trustiot-telemetry-query` 的路由、密碼 Secret `trustiot-telemetry-query-password` 的三個鍵，且 ClickHouse pod 是 `trustiot-clickhouse-0`（`api.test.ts`）。
- 歷史查詢 API 檢查的成功、失敗與逾時：沒有租戶的管理者等到 403 `permission_denied`（拿到 2xx 立刻失敗）、不帶 cookie 是 401、新租戶的成員等到 200 且 `points` 為空（只有 `telemetry.account_not_ready` 的 404 與 503 會繼續等）；其他回應立刻失敗。API 路徑、帳號名稱前綴與角色名稱對照服務原始碼（`services/telemetry-query/src/history.rs`、`src/accounts.rs`、ClickHouse migration `0004_access.sql`），服務改名時測試會失敗。
- 帳號檢查的查詢只選帳號名稱與角色欄位，不選密碼或驗證設定；帳號不存在、預設角色不對、多出角色或帶 `ADMIN OPTION` 都判為失敗。查詢把預設角色清單接成文字、把 `with_admin_option` 轉成數字，結果不受 ClickHouse 輸出陣列或布林值的格式影響；實際的輸出格式要等 nightly 在 ClickHouse 26.8 上確認。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 帶 `smoke` label 的 PR 的 nightly 從原始碼建出 telemetry-query 映像，以兩份 values 部署，pod 就緒 | 待測：要等 nightly run（工作站不跑 Kubernetes）；本機已確認兩份 values 都渲染出它且通過 kubeconform |
| nightly 的 API 檢查：帶 dev session 呼叫 `/api/telemetry/v1/...` 得 200、不帶 cookie 得 401；`bun test scripts/nightly` 通過 | 單元測試通過；叢集上的結果待 nightly run |
| nightly 建立租戶後，該租戶的 ClickHouse 帳號存在（輸出不含密碼） | 待測：要等 nightly run；檢查只印帳號名稱與角色 |
| `deploy/flux/README.md` 寫明展示環境打開 telemetry-query 的值；`bun test deploy/flux` 通過 | 通過 |
