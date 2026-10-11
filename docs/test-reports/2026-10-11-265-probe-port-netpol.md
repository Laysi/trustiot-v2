# chart：節點來源 169.254.7.127 只放行探針埠（2026-10-11）

對應 issue：Laysi/trustiot-v2#265。原始資料在 [data/2026-10-11-265-probe-port-netpol/](data/2026-10-11-265-probe-port-netpol/)。

## 目的

1. 兩份 values 渲染出的每個 NetworkPolicy，對 `169.254.7.127/32` 的規則只放行該工作負載（每個服務與 bundled ClickHouse）的探針埠。
2. 回歸：同一份測試在修正前的 main（`b82d56e`，PR Laysi/trustiot-v2#256 合併後）失敗，修正後通過。
3. 探針埠從 chart 的探針設定推得：startup、liveness、readiness，`httpGet`、`tcpSocket`、`grpc`，具名的埠換成數字，exec 探針不產生規則。
4. 修正不改變其他物件：ClickHouse StatefulSet 改由共用的 `trustiot.clickhouse.pod` 渲染，內容與修正前相同。

## 環境

- **本機**：x86_64 Fedora（Linux 7.2.8-300.fc45），bun 1.4.2、helm 4.3.0、kubeconform，皆由 `.prototools` 鎖定。只渲染 chart，不需要叢集。
- **受測版本**：分支 `fix/265-probe-port-netpol`（以 main `b82d56e` 為基底）。「修正前」是以 `git archive origin/main deploy/chart .prototools .proto rust-toolchain.toml` 取出的 `b82d56e`，只換上本分支的 `deploy/chart/tests/chart.test.ts`。
- **values**：兩種部署型態 `values-saas.yaml` + `tests/values/saas-instance.yaml`、`values-onprem.yaml` + `tests/values/onprem-instance.yaml`，各自疊加：不加（只有 bundled ClickHouse）、`tests/values/smoke.yaml`、smoke 加上啟用全部服務、`tests/e2e/values-e2e.yaml` 加上 `mesh.calls`。
- **時間**：台北時間 2026-10-11 03:00–03:50。

## 指令

```sh
# 修正後（repo 根目錄）
bun test deploy/chart                      # 全部 chart 測試
bun test deploy/chart -t "authorization"   # 服務間授權的 24 個測試

# 修正前：main b82d56e 的 chart，換上本分支的測試檔
mkdir before && git archive origin/main deploy/chart .prototools .proto rust-toolchain.toml | tar -x -C before
cp deploy/chart/tests/chart.test.ts before/deploy/chart/tests/chart.test.ts
(cd before && bun test ./deploy/chart/tests/chart.test.ts -t "authorization")
```

渲染結果的對照（每個工作負載的探針埠與對節點來源的規則）由一支只讀 `helm template` 輸出的 Bun 腳本產生，兩種型態都加上 smoke 並啟用全部服務：[修正前](data/2026-10-11-265-probe-port-netpol/host-source-rules-before.json)、[修正後](data/2026-10-11-265-probe-port-netpol/host-source-rules-after.json)。

## 結果

### 回歸測試

| 版本 | 結果 | 記錄 |
| --- | --- | --- |
| main `b82d56e` + 新測試 | 24 個中 8 個失敗 | [chart-test-on-main.log](data/2026-10-11-265-probe-port-netpol/chart-test-on-main.log) |
| 分支 | 24 個全部通過 | [chart-test-on-branch.log](data/2026-10-11-265-probe-port-netpol/chart-test-on-branch.log) |
| 分支，全部 chart 測試（`bun test deploy/chart`） | 178 個中 177 個通過；1 個是 5 秒逾時，單獨重跑通過（見下） | 本機 |

在 main 上失敗的 8 個：新加的「`<型態>`: the host source reaches only each workload's probe ports」（saas、onprem）與「probe ports follow the probe definitions」，以及改成要求探針埠的既有測試（每個服務的 NetworkPolicy、ClickHouse 的 NetworkPolicy，saas、onprem 各一，加上 ClickHouse 沒有呼叫端時）。失敗內容都相同：規則有 `from: 169.254.7.127/32`，但沒有 `ports`，等於放行所有埠。

全部 chart 測試那一輪，`ClickHouse refuses everyone while its callers are off...` 在 5 秒預設逾時內只跑完 5.09 秒：本機同時有其他 agent 在跑（load average 約 32，8 核），單獨重跑通過。新加的兩個多次渲染的測試設了 `SLOW_TEST_MS`，與 kubeconform 測試相同。

### 渲染結果（smoke + 全部服務）

| 工作負載 | 探針埠（pod spec） | 修正前對 169.254.7.127/32 的規則 | 修正後 |
| --- | --- | --- | --- |
| `values.yaml` 的 12 個服務與 `smoke-echo`（Deployment） | 8080（`http`） | 不限埠 | 只有 TCP 8080 |
| ClickHouse（StatefulSet） | 8123（`http`） | 不限埠，包括原生協定 9000 | 只有 TCP 8123 |

兩種型態相同。另以 `--set-json` 覆寫 smoke-echo 的 ports 與 probes 驗證推導規則：

| 探針設定 | 放行的埠 |
| --- | --- |
| startup `tcpSocket: admin`（9901）、liveness `grpc: 9090`、readiness `httpGet: 8080`；另有不被探測的 `data`（7000） | 8080、9090、9901（不含 7000） |
| startup `{}`、liveness `httpGet: admin`、readiness `tcpSocket: admin` | 9901（只列一次） |
| startup、readiness 為 exec，liveness `tcpSocket: grpc` | 9090 |
| 全部 exec 或關閉 | 沒有這條規則；沒有呼叫端時 `ingress: []` |
| 全部 exec、入口路由到它 | 只有入口 gateway 的 HBONE 15008 規則 |

探針指向 ports 沒有的名稱（`metrics`）時渲染失敗，訊息與 Deployment 模板的檢查相同。

修正前後逐一比較兩種型態（smoke）渲染出的所有物件（忽略 key 順序）：只有 `NetworkPolicy/trustiot-smoke-echo` 與 `NetworkPolicy/trustiot-clickhouse` 不同，ClickHouse StatefulSet 與其他物件相同。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 兩份 values 渲染出的每個 NetworkPolicy，對 `169.254.7.127/32` 的規則只放行該工作負載的探針埠，有 chart 測試證明 | 通過：`the host source reaches only each workload's probe ports` 對兩種型態、四種疊加組合的每個 Deployment 與 StatefulSet，比對 pod spec 的探針埠與規則 |
| 回歸驗證：上述測試在 #256 的版本上失敗，修正後通過 | 通過：main `b82d56e` 上 8 個失敗，分支全部通過 |
| 所有工作負載在 k3d nightly（PR 的 `smoke` job）兩份 values 都就緒 | 待測：由 PR 的 CI `smoke` job 驗證，結果記錄在 PR 與 issue 的實作說明 |
| README 與決策 27 已更新 | 通過：`deploy/chart/README.md` 的服務間授權一節與「擴充 chart」，`docs/architecture/decisions.md` 決策 27 |
