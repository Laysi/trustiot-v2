# 入口 e2e 的 SPA 區塊、CSRF 案例與 nightly 跨站檢查：不需叢集的驗證

對應 Laysi/trustiot-v2#943（父 issue Laysi/trustiot-v2#498）。這份報告只涵蓋不需要叢集的部分：新斷言的判斷條件、身分服務替身的跨站規則、nightly API 檢查的跨站檢查，以及每個條件的變異測試（mutation run）。經真實入口與 istiod 的結果在 nightly（PR 加 `smoke` label）跑完後另行記錄。

## 環境

- 本機（x86_64 Linux），沒有 Kubernetes 叢集。
- Bun 1.4.2、helm 4.3.0、kubeconform 0.8.0（皆為 `.prototools` 的版本）。
- 分支：Laysi/trustiot-v2#943 的工作分支，起點 main `d2e47c2f`。

## 指令

```sh
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test deploy/chart
bun test scripts
# 單獨跑新的測試
bun test deploy/chart/tests/e2e/ingress-answers.test.ts deploy/chart/tests/e2e/identity-double.test.ts
bun test scripts/nightly/api.test.ts -t "checkCrossSiteRequests|the cookie name and the paths"
# 變異測試：一次套用一個文字變異、跑指定的測試檔、還原（定義與程式在 data 目錄）
bun mutate.ts <tree> mutations-chart.json
bun mutate.ts <tree> mutations-api.json
```

`mutate.ts` 是本次的一次性工具，原樣放在 [data/…/mutate.ts.txt](data/2026-11-01-943-spa-csrf-cluster-free/mutate.ts.txt)；chart 這邊的變異在 `deploy/chart/tests` 的副本上跑，nightly 這邊在工作目錄跑、每次跑完還原。

## 結果

| 指令 | 結果 |
| --- | --- |
| `bun run --cwd scripts typecheck` | 通過（含 `deploy/chart/tests/**`，所以也涵蓋 `ingress.e2e.ts`） |
| `bun test deploy/chart` | 796 pass、0 fail（11 個檔案） |
| `bun test scripts` | 1909 pass、9 skip、0 fail（57 個檔案；skip 是既有的、需要 podman 或叢集的測試） |
| `ingress-answers.test.ts` + `identity-double.test.ts` | 31 pass |
| `api.test.ts` 的 `checkCrossSiteRequests` 與路徑對照 | 7 pass |

### 變異測試

每一列是一個刻意弄壞的條件，KILLED 表示對應的測試抓到了。原始輸出：[mutations-chart.out.txt](data/2026-11-01-943-spa-csrf-cluster-free/mutations-chart.out.txt)、[mutations-api.out.txt](data/2026-11-01-943-spa-csrf-cluster-free/mutations-api.out.txt)；變異定義：[mutations-chart.json](data/2026-11-01-943-spa-csrf-cluster-free/mutations-chart.json)、[mutations-api.json](data/2026-11-01-943-spa-csrf-cluster-free/mutations-api.json)。

| 編號 | 弄壞的地方 | 結果 |
| --- | --- | --- |
| D1 | 替身不擋跨站請求（等同 cookie 檢查拿不到兩個標頭時的放行） | KILLED（11 fail） |
| D2 | 替身的 `received` 多列出 cookie | KILLED（18 fail） |
| D3 | 替身回報錯的 `sec-fetch-site` | KILLED（8 fail） |
| D4 | 替身不看 `Origin` | KILLED（6 fail） |
| D5 | 身分標頭的 claims 又帶 session 值（改動前的 `sub`） | KILLED（7 fail） |
| A1 | 跨站案例不檢查 403 | KILLED |
| A2 | 跨站案例不檢查 message key | KILLED |
| A3 | 不比對 cookie 檢查收到的兩個標頭 | KILLED |
| A4 | SPA 案例不檢查 `web` 收到的 cookie | KILLED |
| A5 | SPA 案例不檢查 `web` 收到的身分標頭（含底線寫法） | KILLED |
| A6 | 放行案例不比對 cookie 檢查拿到的路徑 | KILLED |
| A7 | SPA 案例不比對請求行 | KILLED |
| A8 | 放行案例接受兩個身分標頭 | KILLED |
| A9 | SPA 案例不檢查是 `web` 回應 | KILLED |
| A10 | SPA 案例不檢查回顯的 cookie 與偽造值 | KILLED |
| A11 | 失敗訊息印出 whoami 回顯的整段標頭（含 cookie） | KILLED |
| N1 | nightly 檢查把任何 403 當成跨站拒絕 | KILLED |
| N2 | 拿掉 same-origin `GET` 的對照組 | KILLED |
| N3 | 拿掉 app `Origin` 的 `POST`（405）對照組 | KILLED |
| N4 | 拿掉外站 `Origin` 的案例 | KILLED |
| N5 | 拿掉 `Sec-Fetch-Site: cross-site` 的案例 | KILLED |
| N6 | `runApiCheck` 不呼叫 `checkCrossSiteRequests` | SURVIVED：`runApiCheck` 需要叢集，單元測試不涵蓋；由 nightly 報告的 API 檢查細節是否含 `cross-site requests with the member's cookie` 確認 |

`api.test.ts` 的假入口以三種設定模擬 istiod：兩個標頭都交給 cookie 檢查（通過）、都不交（`GET` 跨站得到 200，檢查失敗並指出 `includeRequestHeadersInCheck`）、只交其中一個（在缺的那個案例失敗）。

## 結論

| 驗收條件 | 狀態 |
| --- | --- |
| 入口 e2e 的 SPA 區塊與 CSRF 案例在 nightly 通過（PR 加 `smoke` label），附上 job 連結 | 待測：需要有 Istio ambient 的叢集，在 nightly 執行；本報告只證明斷言本身會在壞掉的條件下失敗 |
| nightly 的 API 檢查會擋下跨站請求（測試），並在 nightly 通過 | 測試部分通過（`checkCrossSiteRequests` 的單元測試與 N1–N5）；nightly 部分待測 |
| `bun test deploy/chart` 與 `bun run --cwd scripts typecheck` 通過 | 通過 |
