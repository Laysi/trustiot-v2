# 2026-11-03 AI 助理的被呼叫端白名單與 AI 閘道檢查（Laysi/trustiot-v2#630）

對應 issue：Laysi/trustiot-v2#630。chart 只接受 `mesh.calls.assistant` 列出 `trustiot.assistant.callees`（`ai-gateway`、`assets`、`dashboard-check`、`postgres`、`telemetry-query`）；AI 助理啟用時，AI 閘道也必須啟用並列在清單上。

## 環境

- 分支 `fix/630-assistant-callee-allowlist`，基底 main `8a74d4d9`。
- helm、bun 用 `.prototools` 鎖定的版本。
- `bun test deploy/chart`、突變測試與 `bun run --cwd scripts typecheck` 在建置主機上執行；`helm template` 的前後比對與 PR Laysi/trustiot-v2#941 的檢查在工作站上執行。兩邊都是 Linux。

## 指令

```sh
# chart 的所有檢查
bun test deploy/chart

# 本 issue 的測試
bun test deploy/chart/tests/chart.test.ts -t 'may not list|both refuse the assistant|an enabled assistant fails'

# 突變測試：從 templates/_mesh.tpl 拿掉 include "trustiot.assistant.checkCalls"
# （只拿掉 onBehalfCalls 的、只拿掉 plan 的、兩個都拿掉），各跑一次上面那組測試，最後還原

# 腳本的型別檢查與 GitOps 檢查
bun install --cwd scripts --frozen-lockfile && bun run --cwd scripts typecheck
bun test deploy/flux

# 預設渲染不變：main 與本分支各跑一次，比對 sha256
helm template trustiot deploy/chart -n trustiot -f deploy/chart/values-saas.yaml -f deploy/chart/tests/values/saas-instance.yaml
helm template trustiot deploy/chart -n trustiot -f deploy/chart/values-onprem.yaml -f deploy/chart/tests/values/onprem-instance.yaml
# 以上兩組再加上 services.<每個服務>.enabled=true、mqttBroker.enabled=true 與 chart.test.ts 的 AI 設定

# PR Laysi/trustiot-v2#941（展示叢集開啟 AI 閘道、telemetry-query 與 assistant）：
# 把它的 deploy/flux/README.md、deploy/flux/clusters/demo/trustiot/values-demo.yaml、deploy/flux/tests/flux.test.ts
# 暫時放進本分支，執行 bun test deploy/flux，之後還原
```

## 結果

| 檢查 | 結果 |
| --- | --- |
| `bun test deploy/chart` | 844 pass、0 fail（含本 issue 新增的測試） |
| 突變：只拿掉 `onBehalfCalls` 的檢查 | 「plan 與 token exchange 都檢查」的測試失敗，其餘通過（17 pass、1 fail） |
| 突變：只拿掉 `plan` 的檢查 | 同上（17 pass、1 fail） |
| 突變：兩個都拿掉 | 白名單的 4 個測試、L2 的 2 個測試（兩種形態）與結構測試都失敗（11 pass、7 fail） |
| 還原後 | 18 pass、0 fail |
| `bun run --cwd scripts typecheck` | exit 0 |
| `bun test deploy/flux`（本分支） | 79 pass、0 fail |
| `bun test deploy/flux`（套上 PR Laysi/trustiot-v2#941 的三個檔案） | 79 pass、0 fail：新規則接受展示叢集開啟 assistant 的設定 |
| 預設渲染的前後比對（4 組） | 全部相同，見 [data/…/render-hashes.txt](data/2026-11-03-630-assistant-callee-allowlist/render-hashes.txt) |
| 渲染失敗的訊息（列了 `identity` 或 `certs`、跳過 schema 驗證、關閉 AI 閘道、沒列 `ai-gateway`、以 null 刪掉清單、清單不是 list） | 見 [data/…/render-refusals.txt](data/2026-11-03-630-assistant-callee-allowlist/render-refusals.txt) |

只拿掉其中一處檢查時，渲染仍然失敗：`trustiot.mesh.plan` 本身就會呼叫 `trustiot.mesh.onBehalfCalls`，而 `templates/mesh.yaml` 每次都會渲染 plan。所以只靠行為測試抓不到「只剩一處」的退化，這部分改由結構測試保證兩個 define 都各自呼叫檢查。

## 結論

- 列了白名單以外的被呼叫端時渲染失敗，訊息寫出那一項、允許的清單與要改的地方：**通過**。測過 `identity`、`certs`、`alerts`、`clickhouse`，在服務全部關閉、全部開啟、跳過 schema 驗證三種情況下都失敗。
- 助理啟用時，AI 閘道沒有啟用，或清單沒列 `ai-gateway`，渲染失敗並說明原因：**通過**。測過 AI 閘道關閉、清單沒列、空清單、以 null 刪掉清單、只開助理五種情況，兩種形態都失敗；助理關閉時這些設定都照常渲染。
- 兩份 values 的預設渲染不變：**通過**。既有的 chart 測試與「呼叫矩陣」測試都通過，四組渲染輸出逐位元組相同。
- README「AI 助理」寫明這兩條規則：**通過**（`deploy/chart/README.md`）。
- `bun test deploy/chart` 與 `bun run --cwd scripts typecheck`：**通過**。
