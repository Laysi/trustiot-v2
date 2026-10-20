# 入口 Service 固定的 ClusterIP：nightly 的 smoke 部署實測

對應 issue：Laysi/trustiot-v2#478（父 issue Laysi/trustiot-v2#457），PR Laysi/trustiot-v2#481。

測試日期為 2026-10-21（工作站的當地日期）。GitHub Actions 與下方 log 的時間戳記一律是 UTC，所以顯示為 2026-10-20T21:00Z 到 21:15Z。原始資料在 [data/2026-10-21-478-ingress-fixed-cluster-ip/](data/2026-10-21-478-ingress-fixed-cluster-ip/)。

## 環境

- **受測程式碼**：PR Laysi/trustiot-v2#481 的 head `e025d069c4969a8e2bfdb19f148e794286c54000`（分支 `feat/478-ingress-fixed-cluster-ip`，從 main `4cf0dbc` 開出）。本報告是之後加上的 commit，只改文件。
- **執行的地方**：GitHub Actions 的 nightly workflow（`.github/workflows/nightly.yml`）的 `smoke` job，hosted arm64 runner（`ubuntu-24.04-arm`）。
  - 在 PR 加上 `smoke` label 之後，`pull_request` 的 `labeled` 事件啟動 run [39496862573](https://github.com/Laysi/trustiot-v2/actions/runs/39496862573)。
  - job 從 21:00:58Z 跑到 21:14:20Z；smoke 本身（`bun scripts/nightly/run.ts`）的 `report.json` 記錄 21:01:22Z 開始、21:14:17Z 結束。
- **叢集**：runner 上以 k3d 建的單節點叢集（`--servers 1 --agents 0`），k3s `rancher/k3s:v1.36.5-k3s1@sha256:057cfd5a9d51b89f1f0ee139c28d5d5fdd5aa7ddeac2742a0ba90c6dbc0329bb`。
  - Service 網段是 k3s 預設的 `10.43.0.0/16`（smoke 從 ServiceCIDR `kubernetes` 讀出），靜態區段是 `10.43.0.1` 到 `10.43.1.0`。
- **版本**：`nightly.yml` 的 `jobs.smoke.env` 釘選 Istio 1.30.5（ambient：base、istiod、cni、ztunnel）、Gateway API v1.6.1、CloudNativePG 1.30.1；`.prototools` 釘選 helm 4.3.0 與 kubectl 1.36.5。
- **兩個型態**：同一個叢集依序部署 `values-onprem.yaml`（`ingress.exposure: direct`，命名空間 `nightly-onprem`）與 `values-saas.yaml`（`ingress.exposure: tunnel`，命名空間 `nightly-saas`），前一個型態的命名空間刪除完才開始下一個。
  - 只有 tunnel 型態固定 ClusterIP。SaaS 的位址是 `10.43.0.240`（`scripts/nightly/ingress-address.ts` 的 `SMOKE_INGRESS_CLUSTER_IPS`）。

## 指令

smoke 只在 CI 跑（工作站不跑 Kubernetes）。這次的觸發方式是在 PR Laysi/trustiot-v2#481 加上 `smoke` label；之後 PR 每推一個 commit 都會重跑。main 上可以 `workflow_dispatch` 手動執行 nightly。

workflow 的步驟執行 `bun scripts/nightly/run.ts`。Laysi/trustiot-v2#478 的兩個檢查在 `scripts/nightly/smoke.ts`，執行的指令如下（取自 run.log，路徑縮短；`<out>` 是報告目錄）：

```sh
# 檢查 1，helm 安裝之前：位址在叢集 Service 網段的靜態區段內，而且沒有 Service 用著
kubectl get servicecidr kubernetes --output json
kubectl get services --all-namespaces --output json
# 通過後寫出 <out>/values-ingress-address-saas.yaml，接在 helm 的 -f 最後
helm upgrade --install trustiot deploy/chart --namespace nightly-saas \
  -f deploy/chart/values-saas.yaml -f deploy/chart/tests/values/saas-instance.yaml \
  -f deploy/chart/tests/values/smoke.yaml -f <out>/values-api-saas.yaml \
  -f <out>/values-ingress-address-saas.yaml --wait --wait-for-jobs --timeout 5m
# 檢查 2，Gateway 為 Programmed 之後：Istio 為 Gateway 產生的 Service 恰好一個，ClusterIP 是那個位址
kubectl get services --namespace nightly-saas \
  --selector gateway.networking.k8s.io/gateway-name=trustiot-ingress --output json
```

取回這次 run 的證據（artifact `nightly-report`，含 `report.json`、`run.log` 與 `values-ingress-address-saas.yaml`）：

```sh
gh run download 39496862573 --repo Laysi/trustiot-v2 --name nightly-report --dir <dir>
```

artifact 會過期，所以相關部分摘錄在 data 目錄：

- [clusterip-checks.json](data/2026-10-21-478-ingress-fixed-cluster-ip/clusterip-checks.json)：`report.json` 裡兩個型態的檢查數、失敗數與 Laysi/trustiot-v2#478 的兩個檢查，加上 `values-ingress-address-saas.yaml` 的內容。
- [run-log-excerpt.txt](data/2026-10-21-478-ingress-fixed-cluster-ip/run-log-excerpt.txt)：`run.log` 中這兩個檢查、Gateway 的 `Programmed` 等待與 SaaS 的 helm 安裝那幾行，附原始行號。

## 結果

run 的結論是 success，`report.json` 的 `ok` 為 true。兩個型態都通過：

| 型態 | `ingress.exposure` | 檢查數 | 失敗 |
| --- | --- | --- | --- |
| onprem | `direct` | 20 | 0 |
| saas | `tunnel` | 21 | 0 |

Laysi/trustiot-v2#478 的兩個檢查：

| 型態 | 檢查 | 時間（UTC） | run.log 行號 | 輸出 |
| --- | --- | --- | --- | --- |
| onprem | 1. 安裝之前 | 21:06:08 | 1270–1271 | `exposure direct: the chart takes no fixed ClusterIP` |
| onprem | 2. Gateway 為 Programmed 之後 | 21:06:44 | 1325–1326 | `this shape pins no ClusterIP` |
| saas | 1. 安裝之前 | 21:08:36 | 1500–1503 | `values-ingress-address-saas.yaml: ingress.tunnel.clusterIP 10.43.0.240, unused and in the static band of 10.43.0.0/16` |
| saas | 2. Gateway 為 Programmed 之後 | 21:09:08 | 1555–1557 | `the Service of Gateway trustiot-ingress has ClusterIP 10.43.0.240` |

- saas 的 helm 安裝（21:08:36Z，第 1510 行）的最後一個 `-f` 是 `values-ingress-address-saas.yaml`，內容只有 `ingress.tunnel.clusterIP: 10.43.0.240`。
- 檢查 2 之前，同一型態的「every Gateway is programmed」通過（21:09:08Z，`trustiot-ingress` 的 `Programmed` condition met，第 1551–1554 行）：Kubernetes 接受了帶這個位址的 Service，Istio 也完成了 Gateway。
- 檢查 2 要求以 label `gateway.networking.k8s.io/gateway-name=trustiot-ingress` 選出的 Service 恰好一個，而且 `spec.clusterIP` 與 `spec.clusterIPs[0]` 都等於設定值。所以 Istio 1.30.5 把 `ingress-options` ConfigMap 的 `service` overlay 裡的 `clusterIP` 套到了它第一次建立的 Service。
- saas 型態在檢查 2 之後的檢查也都通過，包括入口 NetworkPolicy 的埠、經入口的 API 檢查（`through the ingress, a new tenant reaches the assets service …`）、`helm uninstall` 的清理與兩個 e2e。
- onprem 型態不設位址：chart 在 `direct` 時拒絕這個欄位，smoke 也不寫這份 values。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 1. `tunnel` 且設了位址時，`ingress-options` 的 `service` overlay 含 `clusterIP: <位址>`；沒設時輸出與加入這個值之前逐位元組相同 | 通過 | `bun test deploy/chart`（`deploy/chart/tests/chart.test.ts` 的 `ingress.tunnel.clusterIP` 測試）在 e025d06 的乾淨 worktree 460 pass、0 fail；CI run [39496848860](https://github.com/Laysi/trustiot-v2/actions/runs/39496848860) 的 `helm` job 通過 |
| 2. 位址不是 IPv4，或 exposure 不是 `tunnel` 卻設了位址時，chart 拒絕並說明原因 | 通過 | 同上；有沒有 schema 驗證都測 |
| 3. smoke 部署以 tunnel 形態設定固定位址時，Istio 產生的入口 Service 的 ClusterIP 等於設定值 | 通過 | 本報告的「結果」：saas 型態的 Service 拿到 `10.43.0.240` |
| 4. values 註解與文件寫明位址要放在靜態區段、為什麼，以及改位址的步驟 | 通過 | `deploy/chart/values.yaml` 的 `ingress.tunnel.clusterIP` 註解；`deploy/chart/README.md`「固定的入口 ClusterIP」 |
| 5. 專案規定的測試與 CI 通過 | 在 e025d06 通過 | CI run 39496848860 success：`workflow-policy`、`changes`、`gitleaks`、`helm`、`rust` 通過，`cargo-deny`、`postgres-image`、`host` 依變更的路徑不執行。加上本報告的 commit 之後，CI 與 smoke 在新的 head 重跑，合併前確認 |

待測：下列行為目前只依 Istio 1.30.5 與 Kubernetes 的原始碼推論，`deploy/chart/README.md` 也這樣標明。它們屬於進階驗證，依 2026-10-21 的決定延後到 Laysi/trustiot-v2#479（P2）。

- 已部署的入口 Service 改位址；
- 把值改回空字串；
- overlay 與 Service 的位址不一致時 Istio 的行為。

展示叢集的部分不在本 issue：

- 位址由 Laysi/trustiot-v2#480 寫進 `values-demo.yaml`。
- 切換路由之前比對 values、ConfigMap 與 Service 的位址：程式在 Laysi/trustiot-v2#483，在展示叢集實際執行在 Laysi/trustiot-v2#469。
