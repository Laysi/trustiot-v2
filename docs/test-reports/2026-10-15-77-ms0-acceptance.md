# MS0 完成條件的自動測試（2026-10-15）

對應 issue：Laysi/trustiot-v2#77。條件與測試的對照在 [MS0 完成條件與測試](../roadmap/ms0-acceptance.md)；原始資料在 [data/2026-10-15-77-ms0-acceptance/](data/2026-10-15-77-ms0-acceptance/)。

## 環境

- **受測程式碼**：分支 `test/77-ms0-acceptance`，從 main `8e29d03` 開出。
- **chart 與腳本測試**：建置主機（x86_64 Linux），經檢查佇列執行，`CI=1`；bun 1.4.2、helm 4.3.0、kubeconform、actionlint 由 `.prototools` 鎖定。路由清單的變異測試在開發用工作站（x86_64 Linux，同樣的工具版本）。
- **沒有 Kubernetes**：本 repo 規定工作站與建置主機不跑 Kubernetes，lab 的 k3s 沒有 CloudNativePG。叢集上的新檢查（API 檢查的偽造標頭、`Host` 埠號與路徑正規化；入口 e2e 的新測試；每晚的服務間授權 e2e）由 PR 的 nightly smoke（帶 `smoke` label）在 GitHub Actions 的 `ubuntu-24.04-arm` k3d 叢集執行，結果補在下方「nightly smoke（k3d）」。

## 指令

在 repo 根目錄，經檢查佇列：

```sh
bun install --cwd scripts --frozen-lockfile
bun test deploy/chart
bun test scripts
bun run --cwd scripts typecheck
bun scripts/ci/check-workflows.ts
bun scripts/ci/check-secret-handling.ts
proto install actionlint
bun scripts/ci/lint-workflows.ts
```

路由清單的變異測試：把 `deploy/chart/templates/ingress.yaml` 改壞兩種方式，各跑一次 `bun test deploy/chart/tests/chart.test.ts -t "route list"`，再還原重跑。

## 結果

| 項目 | 結果 |
| --- | --- |
| `bun test deploy/chart` | 369 通過、0 失敗（含新的 `ingress-authz.test.ts` 12 個與 chart 測試「route list」7 個） |
| `bun test scripts` | 830 通過、2 略過、0 失敗（含 `scripts/nightly/api.test.ts` 的偽造標頭、`checkIngressEdges` 與 `e2e.test.ts` 的測試組） |
| typecheck、workflow policy、secret handling、actionlint | 通過 |

原始輸出：[checks.txt](data/2026-10-15-77-ms0-acceptance/checks.txt)。

### 路由清單的變異測試

[route-list-mutation.txt](data/2026-10-15-77-ms0-acceptance/route-list-mutation.txt)：

| 改壞的方式 | 結果 |
| --- | --- |
| 例外規則的 `notHosts` 拿掉 `<主機>:*` | 7 個案例全部失敗，例如 `app.saas.trustiot.test:80/healthz: a listed exception, but the cookie check applies to it` |
| 例外規則不再檢查例外路徑在其他主機上的請求 | 5 個失敗，例如 `app.saas.trustiot.test/certs/renew: reaches trustiot-assets through app.saas.trustiot.test/ without the cookie check`；另 2 個（nightly smoke 的 API 路由）沒有與例外重疊的路由，照理通過 |
| 還原 | 7 個全部通過 |

### nightly smoke（k3d）

PR Laysi/trustiot-v2#415 帶 `smoke` label 的 nightly：GitHub Actions run 38598985337，head `8a6b7ac`，runner `ubuntu-24.04-arm`，2026-10-15 00:33–00:45（台北時間），兩份 values 都通過。逐項結果、服務間授權 e2e 的每一次呼叫，以及時間比較，在 [nightly-smoke.txt](data/2026-10-15-77-ms0-acceptance/nightly-smoke.txt)；來源是該次執行的 `nightly-report` artifact（`report.json`、`e2e/mesh-e2e.json`）。

| 檢查 | values-onprem.yaml | values-saas.yaml |
| --- | --- | --- |
| API 檢查：管理者建立租戶 201、成員經入口讀到自己的租戶 200、沒有 cookie 401、沒有租戶的管理者 403 | 通過 | 通過 |
| 偽造身分標頭：直接送給資產服務時被接受（200 且是該租戶），經入口沒有 cookie 401 `unauthenticated`、帶管理者的 cookie 403 `permission_denied` | 通過 | 通過 |
| `Host` 帶埠號，成員的 cookie 得 200 且是自己的租戶 | `:443`、`:8443` | `:80`、`:8443` |
| mesh 正規化的 6 種路徑寫法，成員的 cookie 得 200 且是自己的租戶 | 通過 | 通過 |
| 入口 e2e（`ingress.e2e.ts`） | 69 通過、0 失敗 | 69 通過、0 失敗 |
| 服務間授權 e2e（`mesh.e2e.ts`） | 不跑（只在 SaaS 型態） | 5 通過、0 失敗 |

入口 e2e 從 61 個變成 69 個，新增 8 個：

- 1 個測試檢查 `Host` 的三種寫法：不帶埠號、帶 listener 的埠、帶 8443；
- 7 個測試各檢查一種正規化路徑寫法（`NORMALIZED_WHOAMI_CASES`，替身的 `/api/whoami`）。

API 檢查的路徑寫法則是另一組 6 種（`NORMALIZED_TENANT_PATHS`，資產服務的 `/api/assets/v1/tenant`），即上表那一列。服務間授權 e2e 依序測了四種狀態：兩層都在、只留 NetworkPolicy、兩層都拿掉、只留 AuthorizationPolicy。

- 只要有任一層在，清單外的呼叫都被拒（`ECONNRESET` 或 `ECONNREFUSED`）。
- 冒用 assets 標籤、但 ServiceAccount 不同的 pod：只留 NetworkPolicy 時通過，因為 NetworkPolicy 依標籤放行；只留 AuthorizationPolicy 時被拒，因為它看的是 mTLS 身分。
- 兩層都拿掉時，同樣的呼叫都得 200，證明被拒是政策造成的。

**多花的時間**：與這個變更之前最後一次 nightly smoke（PR Laysi/trustiot-v2#411 的 run 38545261461，head `88d0c21`）比較，smoke 步驟從 667.4 秒變成 740.1 秒，每晚多約 73 秒（約 1.2 分鐘）：

- values-onprem.yaml：131.5 秒變成 133.2 秒；
- values-saas.yaml：267.7 秒變成 329.4 秒。新的服務間授權 e2e 39.7 秒；API 檢查 4.9 秒變成 16.5 秒；入口 e2e 199.4 秒變成 209.8 秒。

映像沒有重建：Rust 映像的快取命中（「Save the Rust images」略過），新的檢查只用已經建好並匯入的映像。

## 結論

逐條對照 Laysi/trustiot-v2#77 的驗收條件（測試與執行位置見 [MS0 完成條件與測試](../roadmap/ms0-acceptance.md)）：

1. **服務骨架經 API 閘道對外，身分服務與資產服務以事件互通**：已有測試，nightly 的 API 檢查自 Laysi/trustiot-v2#65 起每晚通過（[2026-10-14 的報告](2026-10-14-65-assets-skeleton.md)）；本次沒有改它的這一段。
2. **未經 AuthorizationPolicy 允許的服務呼叫被拒絕**：`mesh.e2e.ts` 改為 nightly 每晚執行，PR 的 nightly 5 通過、0 失敗。
3. **身分服務停掉時入口一律拒絕，明列的例外除外；以路由清單測試證明每條路由都要經過驗證**：路由清單測試通過，變異測試證明它抓得到兩種錯誤；停機測試是 nightly 的入口 e2e（已每晚執行）。
4. **偽造、過期或受眾不符的內部身分被拒絕**：既有的 Rust 測試（CI `rust`）；經入口的偽造標頭在 PR 的 nightly 兩份 values 都通過。
5. **未設定租戶的交易讀不到任何列**：既有的 Rust 測試（CI `rust`）。
6. **同一份 chart 搭配兩份 values 都部署得起來**：既有的 nightly smoke。
7. **migration 以 Job 執行**：既有的 nightly 檢查與 Rust、chart 測試（Laysi/trustiot-v2#60）。
8. **main 上的 CI 全綠**：待合併後 main 的 ci.yml。
9. **經入口的驗證冒煙測試（有效 cookie、沒有 cookie、偽造身分標頭）加進 nightly 並通過**：已加進 nightly，PR 的 nightly 兩份 values 都通過。
