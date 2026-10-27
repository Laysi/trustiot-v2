# nightly 部署 telemetry-query：映像、歷史查詢 API 與租戶的 ClickHouse 帳號

對應 Laysi/trustiot-v2#539。

這份報告記錄 2026-10-23 在開發者工作站上的檢查，以及帶 `smoke` label 的 PR（Laysi/trustiot-v2#580）觸發的 nightly run。驗收條件的主要證據是那次 run：它在 Kubernetes（k3d）上執行，工作站不跑 Kubernetes。

## 環境

- 程式碼：分支 `ci/539-nightly-telemetry-query`。工作站的檢查在基於 main `1cd3da21` 的 tree 上執行；之後 rebase 到 main `c96d059d`（只多了 Laysi/trustiot-v2#571 的前端變動，這些檢查都不讀前端），nightly 在 rebase 後的 commit `44ec56a8` 上執行。
- 開發者工作站（x86_64 Linux），Bun 1.4.2，其餘工具依 `.prototools` 釘選（helm 4.3.0、kubeconform 0.8.0、actionlint 1.7.12、flux 2.9.6、kubectl 1.36.5）。
- nightly：`.github/workflows/nightly.yml` 的 `smoke` job，[run 39928847353](https://github.com/Laysi/trustiot-v2/actions/runs/39928847353/job/120331813247)，GitHub Actions runner 上的 k3d 叢集，`nightly-onprem` 與 `nightly-saas` 兩個 namespace 各部署一次，ClickHouse 是 chart 釘選的 26.8 LTS。job 從 2026-10-23 09:01:35Z 跑到 09:17:39Z（約 16 分鐘），在 `timeout-minutes` 65 之內。

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

nightly 的結果取自上面那次 run 的 smoke job log。

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
| nightly smoke（CI） | 通過：兩個 namespace 都部署並檢查成功，見下面「nightly 的證據」 |

### nightly 的證據

telemetry-query 沒有快取，從原始碼建出映像（`aarch64-unknown-linux-musl`）並載入節點：

```text
== platform: telemetry-query image
no cached image at <runner temp>/image-cache/telemetry-query.tar: building localhost/trustiot-e2e/telemetry-query:nightly
built localhost/trustiot-e2e/telemetry-query:nightly from telemetry-query (7563704 bytes)
image localhost/trustiot-e2e/telemetry-query:nightly is present on node k3d-nightly-server-0
```

| 檢查 | `nightly-onprem` | `nightly-saas` |
| --- | --- | --- |
| 就緒 | `deployment "trustiot-telemetry-query" successfully rolled out` | 同左 |
| 叢集內產生的密碼 | `trustiot-telemetry-query-password: telemetry_query_app, telemetry_query_platform, trustiot_telemetry_query` | 同左 |
| 歷史查詢 API | 新租戶的成員 200、沒有資料點（0 s 後）；不帶 cookie 401 `unauthenticated`；沒有租戶的管理者 403 `permission_denied` | 同左（1 s 後） |
| 租戶的 ClickHouse 帳號 | `trustiot_tenant_account_01a168064ef274a5bf7a455d04bf5d46`（租戶 `01a16806-4ef2-74a5-bf7a-455d04bf5d46`）存在，預設角色 `trustiot_tenant_reader`，沒有 ADMIN OPTION | `trustiot_tenant_account_01a168088e7c74c2aa55096ba02aecb5`（租戶 `01a16808-8e7c-74c2-aa55-096ba02aecb5`），同左 |

`nightly-onprem` 的 API 檢查成功列中的這一段（`nightly-saas` 相同，只有租戶 ID 與等待秒數不同）：

```text
GET /api/telemetry/v1/devices/{device_id}/latest of a device that does not exist as the tenant's member: 200 with no points after 0 s; without a cookie: 401 unauthenticated; the administrator without a tenant: 403 permission_denied; ClickHouse user trustiot_tenant_account_01a168064ef274a5bf7a455d04bf5d46 of tenant 01a16806-4ef2-74a5-bf7a-455d04bf5d46 exists with the default role trustiot_tenant_reader, granted without admin option
```

- log 裡的 `clickhouse-client` 只出現在 `kubectl exec` 的指令列（每個 namespace 兩次），密碼是字面的 `"$(cat "$CLICKHOUSE_PASSWORD_FILE")"`，在 pod 裡才展開；查詢的輸出沒有進 log。
- 同一列的既有檢查（租戶 API、偷來的身分標頭、Host 的埠、路徑正規化）在兩個 namespace 都照常通過；解除安裝後 `trustiot-telemetry-query-password` 照預期留下，再由檢查刪除。

`scripts/nightly` 的單元測試涵蓋：

- `NIGHTLY_IMAGES` 含 `telemetry-query`，快取鍵涵蓋 `services/telemetry-query` 與 `deploy/images/telemetry-query`（`image-cache.test.ts`）。
- 兩種形態的 API 檢查 values 渲染出映像為 `localhost/trustiot-e2e/telemetry-query:nightly` 的 Deployment、`/api/telemetry` 到 `trustiot-telemetry-query` 的路由、密碼 Secret `trustiot-telemetry-query-password` 的三個鍵，且 ClickHouse pod 是 `trustiot-clickhouse-0`（`api.test.ts`）。
- 歷史查詢 API 檢查的成功、失敗與逾時：沒有租戶的管理者等到 403 `permission_denied`（拿到 2xx 立刻失敗）、不帶 cookie 是 401、新租戶的成員等到 200 且 `points` 為空（只有 `telemetry.account_not_ready` 的 404 與 503 會繼續等）；其他回應立刻失敗。API 路徑、帳號名稱前綴與角色名稱對照服務原始碼（`services/telemetry-query/src/history.rs`、`src/accounts.rs`、ClickHouse migration `0004_access.sql`），服務改名時測試會失敗。
- 帳號檢查的查詢只選帳號名稱與角色欄位，不選密碼或驗證設定；帳號不存在、預設角色不對、多出角色或帶 `ADMIN OPTION` 都判為失敗。查詢把預設角色清單接成文字、把 `with_admin_option` 轉成數字，結果不受 ClickHouse 輸出陣列或布林值的格式影響；nightly 在 ClickHouse 26.8 上的兩次帳號檢查都通過，確認了實際的輸出格式。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 帶 `smoke` label 的 PR 的 nightly 從原始碼建出 telemetry-query 映像，以兩份 values 部署，pod 就緒 | 通過：run 39928847353 從原始碼建出映像並載入節點，兩個 namespace 的 rollout 都成功，叢集內產生的密碼 Secret 有它的三個帳號 |
| nightly 的 API 檢查：帶 dev session 呼叫 `/api/telemetry/v1/...` 得 200、不帶 cookie 得 401；`bun test scripts/nightly` 通過 | 通過：兩個 namespace 都是新租戶的成員 200（沒有資料點）、不帶 cookie 401，另外沒有租戶的管理者 403；`bun test scripts/nightly` 281 pass |
| nightly 建立租戶後，該租戶的 ClickHouse 帳號存在（輸出不含密碼） | 通過：兩個 namespace 的新租戶帳號都存在，預設角色 `trustiot_tenant_reader`、沒有 ADMIN OPTION；log 只有指令列，沒有密碼與查詢輸出。帳號的 settings profile 與 `SQL_tenant_id` 由服務在回 200 之前自己確認，從管理端再查一次 2026-10-23 延後到 Laysi/trustiot-v2#579 |
| `deploy/flux/README.md` 寫明展示環境打開 telemetry-query 的值；`bun test deploy/flux` 通過 | 通過 |
