# 展示前檢查一次跑完（`scripts/demo/preflight.ts`）的實測

對應 issue：Laysi/trustiot-v2#1180。展示 runbook（Laysi/trustiot-v2#1146）「展示前檢查」裡可以只讀的項目，改由一個指令跑完並寫出 `PASS`、`WARN` 或 `FAIL` 與依據。本報告是它在展示機與 lab 閘道器上的第一次實際只讀執行。

## 環境

- 執行位置：有這個 repo 的工作站（Linux x86_64），Bun 以 `.prototools` 的釘選為準。
- 測試的程式：分支 `feat/1180-demo-preflight` 的 commit `bc3d83d2e011`（`scripts/demo/preflight.ts`）。這個 commit 不是 `origin/main`（`e8bd05a89fd4`），所以 Flux 那一項依設計多一個 `WARN`，見「結果」。
- 展示機：lab 的 Mac mini M1 的 k3s（namespace `trustiot`），以 ssh 連線，ssh 主機別名由 `DEMO_SSH_HOST` 提供。Flux 跟著 main，HelmRelease 是 chart `0.0.0-rc.15`；儀表板檢查服務沒有打開。
- 閘道器：lab 的 TrustIoT 閘道器，ssh 主機別名由 `GATEWAY_SSH_HOST` 提供。閘道器每天有一段時間關機（Laysi/trustiot-v2#995 之前沒有啟用 RTC），執行時它沒有開機，也沒有嘗試等它或叫醒它。
- 執行時間：2026-11-07 02:57 到 02:58 UTC（台北時間 10:57）。
- 除了這支腳本，沒有在兩台主機上執行其他指令；開發期間另外只讀查看過 `flux.ts status`、`free -m`、`kubectl top node` 與 ClickHouse 的同一組彙總查詢。

## 指令

```sh
bun scripts/demo/preflight.ts --ssh-host "$DEMO_SSH_HOST" --gateway-ssh-host "$GATEWAY_SSH_HOST"
```

兩個變數是 ssh 主機別名（私人，不寫進 repo）。實際執行時以選項傳入：`--ssh-host <展示機的別名> --gateway-ssh-host <閘道器的別名>`，另加 `--log-dir <工作站上的私有目錄>`。結束時的 exit 狀態是 1。

## 結果

完整輸出（全文，只有行首的時間戳記來自 log）：

```text
2026-11-07T02:57:57.993Z demo pre-flight checks (read only: no lab lock, nothing changed)
2026-11-07T02:58:03.980Z WARN Flux and services: the checkout is not a clean origin/main, so main's pin may differ from what Flux follows (HEAD (bc3d83d2e011) is not origin/main (e8bd05a89fd4); run from origin/main, for example after `git switch --detach origin/main`)
2026-11-07T02:58:03.980Z   ok: lab lock free; HelmRelease deployed 0.0.0-rc.15+f31fd6faf13f = main's pin 0.0.0-rc.15; pulled chart digest = main's pin; 16 of 16 pods Running and ready (3 finished Job pods), 0 restarts in total
2026-11-07T02:58:03.980Z PASS memory: available 2885 MiB of 7543 MiB (38 %), node memory 43 %
2026-11-07T02:58:03.980Z PASS dashboard check: off (no dashboard-check pod; informational: the assistant then writes component-mode dashboards only)
2026-11-07T02:58:03.980Z FAIL data freshness: newest received 2026-11-06 09:01:26 UTC, 64597 s ago (limit 180 s), 19310 rows: the data is stale
2026-11-07T02:58:03.980Z FAIL gateway presence: last presence state "offline" since 2026-11-06 09:02:13 UTC (64550 s ago): not online
2026-11-07T02:58:03.981Z FAIL the gateway itself: the gateway is unreachable (ssh failed (exit 255): websocket: bad handshake Connection closed by UNKNOWN port 65535); it may be powered off
2026-11-07T02:58:03.981Z summary: 2 PASS, 1 WARN, 3 FAIL, 0 SKIP; at least one item failed (exit 1)
```

整次執行約 6 秒。對照各項：

| 項目 | 結果 | 依據 |
| --- | --- | --- |
| Flux 與服務 | `WARN` | lab lock 是 `free`；HelmRelease 已部署的版本 `0.0.0-rc.15+f31fd6faf13f` 等於 main 釘的 `0.0.0-rc.15`；拉到的 chart digest 等於 main 釘的；16 個 pod 都是 Running 且 ready（另有 3 個已完成的 Job pod），重啟共 0 次。`WARN` 只來自「執行的 checkout 不是 `origin/main`」：它是功能分支的 commit，而不是環境有問題。這也是 `WARN` 路徑的實際證據（`WARN` 不使 exit 變成非 0） |
| 記憶體 | `PASS` | available 2885 MiB（總量 7543 MiB，38 %），節點記憶體 43 %，與 runbook 2026-11-07 的讀數（約 2.9 GiB、42 %）一致；門檻是 available 低於 2048 MiB 或節點記憶體達 70 % 才 `WARN` |
| 儀表板檢查 | `PASS`（資訊） | 沒有 `dashboard-check` 的 pod，所以是 off |
| 資料新鮮度 | `FAIL` | 最新接收時間 2026-11-06 09:01:26 UTC，64597 秒前，超過 180 秒；列數 19310，與 runbook 2026-11-07 早上的讀數相同（資料表沒有新資料，因為閘道器關機中） |
| 閘道器上線 | `FAIL` | `gateway-ops` 最後一次狀態是 `offline`，時間 2026-11-06 09:02:13 UTC |
| 閘道器本身 | `FAIL` | ssh 連不上（exit 255）；閘道器沒有開機。這就是連不上的 `FAIL` 路徑，其他項目照常跑完 |

輸出經過檢查：沒有主機名稱（ssh 的別名在錯誤訊息中會被換成 `<ssh-host>`，節點名稱不印）、位址、使用者名稱、家目錄路徑或租戶資料；ClickHouse 只查彙總值，帳號與密碼沒有離開容器。

唯讀的依據：腳本送到展示機的 kubectl 只有 `get`、`top` 與一次 `exec`（在 ClickHouse 容器裡跑 `clickhouse-client --readonly 1`，只選彙總值）；沒有取 lab lock（只讀它的狀態），沒有任何 `apply`、`patch`、`delete`、`scale`、`rollout`、`annotate`、`label` 或重啟。`--readonly 1` 在實際的 ClickHouse 上確認有效（`getSetting('readonly')` 回 1）。腳本單元測試對每一個送出的指令檢查這些。

## 結論

對照 Laysi/trustiot-v2#1180 的驗收條件：

- 只讀地跑完各項並寫出 `PASS`、`WARN`、`FAIL` 與依據，有 `FAIL` 時以非 0 結束：通過（實際執行 exit 1；每一項的通過與每一種失敗由 `scripts/demo/preflight.test.ts` 涵蓋）。
- 在展示機與閘道器實際執行一次、輸出存進 `docs/test-reports/`、不含主機名稱、位址與租戶資料：通過（本報告）。
- `bun test scripts`、型別檢查與 CI：見 PR。
- `scripts/README.md` 寫了用法與每一項的判斷：通過（「展示機（`demo/`）」的 `demo/preflight.ts`）。

待測（需要對的條件在執行當下沒有）：全部 `PASS` 的實機路徑（閘道器開機、資料新鮮、agent `active`、`ntp-synchronized`）。閘道器的 `GATEWAY_COMMAND` 就是 runbook 的那一條，runbook 也記載它 2026-11-07 沒有在閘道器上執行過；單元測試以假的 ssh 涵蓋，實機要等閘道器開機後再跑一次，由彩排時確認。
