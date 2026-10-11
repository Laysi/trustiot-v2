# tunnel-recovery 單元測試在 WSL2 失敗的根本原因與修正

對應 issue：Laysi/trustiot-v2#288（e2e 輔助：tunnel-recovery 單元測試在 WSL2 的 Ubuntu 上有 5 個失敗）。相關：Laysi/trustiot-v2#261（PR Laysi/trustiot-v2#267）。

## 環境

- 本機：Fedora，Linux 7.2.8-300.fc45 x86_64，Bun 1.4.2。
- 建置主機 `ruisi-wsl`：Ubuntu（WSL2），Linux 6.18.33.2-microsoft-standard-WSL2 x86_64，Bun 1.4.2（皆由 `.prototools` 固定）。所有指令都經 `rcheck.sh 288 '<command>'` 執行，不改主機設定。
- 分支 `fix/288-tunnel-recovery-wsl`，基於 main e8f180b。
- 不跑 Kubernetes，也不需要。

## 指令

```sh
# 修正前（main e8f180b 的檔案，在 ruisi-wsl）
rcheck.sh 288 'bun test deploy/chart/tests/e2e/tunnel-recovery.test.ts 2>&1'
rcheck.sh 288 'bun diag/d3.ts 2>&1'      # 診斷：freePort() 之後立刻連線（腳本只用於調查，原始碼：data 目錄的 diag-listener-after-stop.ts.txt）

# 修正後
rcheck.sh 288 'for i in 1 2 3 4 5; do bun test deploy/chart/tests/e2e/tunnel-recovery.test.ts deploy/chart/tests/e2e/ports.test.ts 2>&1 | tail -8; done'
rcheck.sh 288 'bun test deploy/chart 2>&1 | tail -8'
rcheck.sh 288 'bun test docs/test-reports/data/2026-10-11-288-tunnel-recovery-wsl/legacy-vs-fixed.test.ts 2>&1'
bun test deploy/chart                       # 本機
bun test docs/test-reports/data/2026-10-11-288-tunnel-recovery-wsl/legacy-vs-fixed.test.ts   # 本機
bun run --cwd scripts typecheck
```

## 結果

### 1. 修正前：5 個失敗

[before-wsl-tunnel-recovery.txt](data/2026-10-11-288-tunnel-recovery-wsl/before-wsl-tunnel-recovery.txt)：15 個測試中 10 通過、5 失敗、3 個 `Unhandled error between tests`，與 issue 描述一致。錯誤是 `ConnectionError: no HTTP response for /api/whoami: ""`，以及 5 秒逾時。

### 2. 根本原因

測試與 `ingress.e2e.ts`、`mesh.e2e.ts` 的 `openTunnel()` 都用同一個模式取得本機埠：

1. `freePort()` 用 `Bun.listen({ port: 0 })` 取得埠號，馬上 `stop(true)`，回傳埠號。
2. 以這個埠號啟動通道子行程（假的或真的 `kubectl port-forward`）。
3. 立刻連該埠探測「通道是否就緒」，連得上就開始送請求。

這個模式假設 `Listener.stop(true)` 回傳時，監聽 socket 已經關閉，所以之後連得上這個埠的一定是通道。這個假設在 WSL2 上不成立：Bun 1.4.2 的 `stop(true)` 要等事件迴圈再跑一輪才真正關閉監聽 socket，在那之前，核心仍會接受連線。診斷腳本在 `freePort()` 回傳後立刻連線 20 次：

| 環境 | 立刻連線 | 等 20 毫秒後連線 |
| --- | --- | --- |
| 本機（Linux 7.2.8，Fedora） | 20 次都被拒絕（`ECONNREFUSED`） | 20 次都被拒絕 |
| ruisi-wsl（6.18.33.2-microsoft-standard-WSL2） | 20 次都連線成功 | 20 次都被拒絕 |

原始輸出：[diag-listener-after-stop-local.txt](data/2026-10-11-288-tunnel-recovery-wsl/diag-listener-after-stop-local.txt)、[diag-listener-after-stop-wsl.txt](data/2026-10-11-288-tunnel-recovery-wsl/diag-listener-after-stop-wsl.txt)。

於是在 WSL2 上，啟動通道後的第一次探測連到的是那個將死的監聽（通道行程要幾十毫秒才會啟動並綁定埠），探測「成功」，測試馬上送請求，請求被即將關閉的 socket 吞掉，得到空回應：`no HTTP response for /api/whoami: ""`。失敗的 5 個測試都需要請求成功穿過剛啟動的假 port-forward（或需要它能正確地先成功再斷線）；通過的 10 個測試不經假 port-forward，或本來就預期連線失敗。

首個測試的 `expect(tunnel.exitCode).toBe(1)` 得到 `null`，是連鎖效應：前一個測試逾時後，`afterEach` 殺掉子行程，殘留的非同步斷言在測試之間才執行。

### 3. 同樣的弱點也在真正的 e2e

`ingress.e2e.ts` 與 `mesh.e2e.ts` 的 `openTunnel()` 用同一個 `cluster.ts` 的 `freePort()`，加上同樣的「連得上就算就緒」探測，因此在 WSL2 上有同樣的競態：探測可能連到將死的監聽，而不是 `kubectl port-forward`，之後的第一個請求會得到空回應。`withTunnelRecovery` 無法救回這種情況，因為通道行程沒有結束，錯誤照樣丟出。（本機與 CI 的 runner 沒有這個行為，所以在 #261 的驗證中沒有發現。）

### 4. 修正

- 新增 `deploy/chart/tests/e2e/ports.ts` 的 `freePort()`：`stop(true)` 之後，輪詢到連線被拒絕才回傳埠號；2 秒內一直連得上（別人佔用了）就換一個埠，最多 5 次。
- `cluster.ts` 改為重新匯出這個 `freePort`，`ingress.e2e.ts` 與 `mesh.e2e.ts` 不用改；測試檔改用同一個函式，不再有自己的副本。`ports.ts` 不依賴 e2e 的環境變數，單元測試可以匯入。
- 新增 `ports.test.ts`：20 次檢查「剛取得的埠沒有任何東西接受連線」，並檢查回傳的埠可以馬上綁定。
- `deploy/chart/tests/e2e/README.md` 補充使用規則。
- 沒有改 `tunnel-recovery.ts`、`raw-client.ts`、`fake-port-forward.ts`，也沒有改任何 WSL 或主機設定。

### 5. 修正後：ruisi-wsl 連續 5 次通過

[after-wsl-5-runs.txt](data/2026-10-11-288-tunnel-recovery-wsl/after-wsl-5-runs.txt)：`tunnel-recovery.test.ts` 加 `ports.test.ts`，5 次都是 17 pass、0 fail。`bun test deploy/chart` 在 ruisi-wsl：202 pass、0 fail（[after-wsl-deploy-chart.txt](data/2026-10-11-288-tunnel-recovery-wsl/after-wsl-deploy-chart.txt)）。

### 6. 本機

`bun test deploy/chart`：202 pass、0 fail（[after-local-deploy-chart.txt](data/2026-10-11-288-tunnel-recovery-wsl/after-local-deploy-chart.txt)）。`bun run --cwd scripts typecheck` 無錯誤。

### 7. 測試仍能證明 #261 的行為

[legacy-vs-fixed.test.ts](data/2026-10-11-288-tunnel-recovery-wsl/legacy-vs-fixed.test.ts) 是 #261 報告的同名腳本，只把 `freePort()` 換成 `ports.ts` 的版本。兩種邏輯對同一個故障注入（請求先失敗，行程 300 毫秒後才結束）的結果在本機（[legacy-vs-fixed-local.txt](data/2026-10-11-288-tunnel-recovery-wsl/legacy-vs-fixed-local.txt)）與 ruisi-wsl（[legacy-vs-fixed-wsl.txt](data/2026-10-11-288-tunnel-recovery-wsl/legacy-vs-fixed-wsl.txt)）相同：

| 邏輯 | 結果 |
| --- | --- |
| #261 之前 `raw()` 的判斷（失敗當下檢查行程是否已結束） | 丟出 `no HTTP response for /api/whoami: ""`，重開次數 0 |
| 現在的 `withTunnelRecovery` | 重開 1 次，回應 200 `gateway` |

`tunnel-recovery.test.ts` 本身沒有改動斷言：「the request fails before the process has exited」測試仍然要求請求在行程結束前失敗，「reopens the tunnel and sends the request once more」仍然要求重開一次且重送一次，舊邏輯無法通過後者。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 根本原因寫在測試報告，附修正前後的輸出 | 通過：結果 1 到 5 |
| 在 ruisi-wsl 上連續 5 次 `bun test deploy/chart/tests/e2e/tunnel-recovery.test.ts` 全部通過 | 通過：結果 5（同一次執行也包含 `ports.test.ts`；單獨跑該檔案的情形由其中的 15 個測試涵蓋） |
| 本機與 CI 上 `bun test deploy/chart` 仍全部通過 | 本機與 ruisi-wsl 通過（結果 5、6）；CI 見 PR |
| 故障注入的版本在舊實作上失敗 | 通過：結果 7 |

後續建議（不在本 issue 範圍）：`scripts/tests/e2e.test.ts` 也有一個 listen-and-stop 的 `freePort()`，但它把埠交給 podman 發布，不探測，目前沒有這個競態。
