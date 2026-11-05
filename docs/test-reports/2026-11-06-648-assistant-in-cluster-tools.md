# assistant：叢集內經 mesh、以使用者身分的工具呼叫（nightly，替身模型）

對應 issue：Laysi/trustiot-v2#648（PR Laysi/trustiot-v2#1084）

本報告記錄 PR 的 nightly smoke 在兩種型態（`values-onprem.yaml`、`values-saas.yaml`）上，以腳本替身擔任 AI 閘道，讓助理經 mesh、以提問者的身分呼叫資產服務與時序查詢服務的結果。檢查的設計與每個呼叫的預期結果寫在 `docs/architecture/ci-release.md`「nightly.yml」與 PR 的內文。

## 環境

- GitHub Actions 的 `nightly` workflow，由帶 `smoke` label 的 PR 觸發（`pull_request`）：[run 42085294277](https://github.com/Laysi/trustiot-v2/actions/runs/42085294277)，job `smoke`。
- 測試的 commit：`93100aee`（分支 `feat/648-in-cluster-tools`，以 main `a87e08d6` 為基底）。
- 叢集：runner 上的 k3d，`rancher/k3s:v1.36.5-k3s1`；Istio ambient、CloudNativePG、內建 ClickHouse 與 OpenTelemetry collector 都由 nightly 安裝（`scripts/nightly/`）。
- 執行時間：2026-11-05 18:33:31Z 到 19:05:32Z（UTC）。
- AI 閘道：以 `helm upgrade` 只把 `services.ai-gateway.image` 換成 `localhost/trustiot-e2e/ai-gateway-double:nightly`（`deploy/chart/tests/e2e/ai-gateway-double/`）。助理每次呼叫模型都帶 `"stream": true`（Laysi/trustiot-v2#1092），替身以 SSE 回應；每一步最多要求 3 個工具呼叫，因為內建 ClickHouse 的租戶設定檔限制每個帳號同時 4 個查詢，而助理會同時執行同一步的呼叫（產品面的問題另開 Laysi/trustiot-v2#1122）。

## 指令

這份檢查只在 CI 執行（工作站不跑 nightly 的叢集）。PR 加上 `smoke` label 後，每次推送都會觸發；也可以在 Actions 手動執行 `nightly` workflow。

本機的單元測試與型別檢查：

```sh
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts/nightly
bun test deploy/chart
bun scripts/ci/check-secret-handling.ts
```

## 結果

兩種型態的 smoke 都通過（onprem 與 saas 各自的所有檢查都是 `ok`），以下是與本 issue 有關的檢查。標記（`nightly-m-…`、`nightly-t-…` 與數值）是這次執行隨機產生的 canary；run ID 是助理的 run，不是祕密。

### 換上替身

| 型態 | 結果 |
| --- | --- |
| onprem | `--dry-run=server` 與升級後的 manifest 比對：130 個物件中只有 ai-gateway Deployment 的映像與 pull policy 改變，migration Job 換名稱（`trustiot-migrate-7de216d5` → `trustiot-migrate-b5fc6780`），9 個等它的 workload 換 pod（映像與設定不變）；1 個 ai-gateway pod 執行替身並就緒 |
| saas | 同上，136 個物件；`trustiot-migrate-17ee4ee4` → `trustiot-migrate-846cd6c6` |

### 資料

每個型態建租戶 A 與 B，各一位 editor、一個裝置型號（資產服務 API）與一台裝置的三筆讀值（內建 ClickHouse）。經入口，每位 editor 只列得到自己的型號、讀得到自己的讀值；A 讀 B 的裝置得到 0 個點。

### 檢查 1：A 問自己的裝置

| 型態 | run | 工具結果 |
| --- | --- | --- |
| onprem | `01a1ad0d-29b4-771b-8f56-a8ff5c2ba6b3` | `device_models` ok（1 列）、`telemetry_latest` ok（2 列）、`telemetry_statistics` ok（series ok, ok）、`telemetry_usage` ok（series ok）、`telemetry_sql` ok（3 列） |
| saas | `01a1ad14-3aeb-7684-b178-82842d04d201` | 同上 |

每個結果都有 A 的資料，模型拿到的 5 個結果與回答都不含 B 的 6 個 canary；回答含 A 的型號與讀值 canary。

### 對照：B 問自己的裝置（與檢查 2 相同的呼叫）

| 型態 | run | 工具結果 |
| --- | --- | --- |
| onprem | `01a1ad0d-36de-718c-8aea-5b25d4f6369a` | `telemetry_latest` ok（2 列）、`telemetry_statistics` ok（series ok, ok）、`telemetry_usage` ok（series ok）、`telemetry_sql`（B 的裝置）ok（3 列）、`telemetry_sql`（不帶條件）ok（1 列）、`telemetry_sql`（基礎表 `trustiot.telemetry`）`input_error`，是 SQL 檢查對表格的拒絕 |
| saas | `01a1ad14-4bda-75cd-8c7e-b9fad8b35ac1` | 同上 |

結果有 B 的資料，6 個結果與回答都不含 A 的 6 個 canary；回答含 B 的讀值 canary。

### 檢查 2：A 以 B 的裝置 ID 問同一組呼叫

| 型態 | run | 工具結果 |
| --- | --- | --- |
| onprem | `01a1ad0d-4380-7554-a0ed-cc170a7c6b08` | `telemetry_latest` ok 且空（0 列）、`telemetry_statistics` ok 且空（series no_data, no_data）、`telemetry_usage` ok、錯誤結果且每個 series 都是 no_data、`telemetry_sql`（B 的裝置）ok 且空（0 列）、`telemetry_sql`（不帶條件）ok（1 列，A 自己的讀值）、`telemetry_sql`（基礎表）`input_error`，是表格拒絕 |
| saas | `01a1ad14-5c43-7798-85ad-92b9ffb7e6c2` | 同上 |

助理日誌裡四個時序工具都是 `ok`，沒有 `denied`：時序查詢服務以 A 的租戶查詢，對 B 的裝置回空的資料。回答與模型拿到的 6 個結果都不含 B 的 6 個 canary。

### 檢查 3：日誌

從 collector 寫進內建 ClickHouse 的 `trustiot_logs.otel_logs` 讀（不執行 `kubectl logs`），兩個型態都一樣：

| run | 助理 | 身分服務 |
| --- | --- | --- |
| 檢查 1 | `question run started`（租戶 A）、5 × `tool call completed` | 對 assistant 簽發 run 委派；以委派交換 token：telemetry-query × 4、assets × 1 |
| 對照 | `question run started`（租戶 B）、6 × `tool call completed` | 對 assistant 簽發 run 委派；以委派交換 token：telemetry-query × 6 |
| 檢查 2 | `question run started`（租戶 A）、6 × `tool call completed` | 對 assistant 簽發 run 委派；以委派交換 token：telemetry-query × 6 |

每次模型呼叫的 `ai-gateway-tenant` 都是提問者的租戶。只核對事件與 ID，不印 token。

### 完整性守門

兩個型態的最後一步都確認換替身與工具檢查各跑過一次並通過，而且工具檢查的結果有全部五個步驟。

## 結論

- [x] 帶 `smoke` label 的 PR 的 nightly 在兩份 values 上完成檢查 1–3：A 的提問經 mesh 以 A 的租戶身分呼叫 telemetry-query 與 assets，回答含 A 的資料；問 B 的裝置時查無資料，回答與工具結果不含 B 的 canary（run 42085294277）。
  - 證明的範圍到租戶與 run 的層級：被呼叫的服務回應成功時不記錄使用者。使用者層級由 `services/identity/src/delegation.rs`（交換出的 token 的 `sub` 是委派的使用者）與它的單元測試證明；執行期的第二個證據是「待測」，在 Laysi/trustiot-v2#1086（P2）。
- [x] 本報告不含 cookie、token 或主機名稱。
- [x] `bun test scripts`、`bun run --cwd scripts typecheck`、`bun test deploy/chart` 通過；替身映像的基底以 digest 釘選；`bun scripts/ci/check-secret-handling.ts` 通過（PR 的驗證一節與 CI）。

延後的進階驗證（多副本與 lab 叢集上重跑、提問中途撤銷委派）在 Laysi/trustiot-v2#1086。
