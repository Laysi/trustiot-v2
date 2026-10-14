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

待 PR 的 nightly 執行後補上：每份 values 的 API 檢查（含偽造標頭、`Host` 埠號、路徑正規化）、入口 e2e 的測試數、SaaS 的服務間授權 e2e，以及與 main 最近一次 nightly 比較多花的時間。

## 結論

逐條對照 Laysi/trustiot-v2#77 的驗收條件（測試與執行位置見 [MS0 完成條件與測試](../roadmap/ms0-acceptance.md)）：

1. **服務骨架經 API 閘道對外，身分服務與資產服務以事件互通**：已有測試，nightly 的 API 檢查自 Laysi/trustiot-v2#65 起每晚通過（[2026-10-14 的報告](2026-10-14-65-assets-skeleton.md)）；本次沒有改它的這一段。
2. **未經 AuthorizationPolicy 允許的服務呼叫被拒絕**：`mesh.e2e.ts` 改為 nightly 每晚執行。待 PR 的 nightly 結果。
3. **身分服務停掉時入口一律拒絕，明列的例外除外；以路由清單測試證明每條路由都要經過驗證**：路由清單測試通過，變異測試證明它抓得到兩種錯誤；停機測試是 nightly 的入口 e2e（已每晚執行）。
4. **偽造、過期或受眾不符的內部身分被拒絕**：既有的 Rust 測試（CI `rust`）；經入口的偽造標頭待 PR 的 nightly 結果。
5. **未設定租戶的交易讀不到任何列**：既有的 Rust 測試（CI `rust`）。
6. **同一份 chart 搭配兩份 values 都部署得起來**：既有的 nightly smoke。
7. **migration 以 Job 執行**：既有的 nightly 檢查與 Rust、chart 測試（Laysi/trustiot-v2#60）。
8. **main 上的 CI 全綠**：待合併後 main 的 ci.yml。
9. **經入口的驗證冒煙測試（有效 cookie、沒有 cookie、偽造身分標頭）加進 nightly 並通過**：已加進 nightly；待 PR 的 nightly 結果。
