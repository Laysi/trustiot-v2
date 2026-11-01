# 展示環境：開啟儀表板檢查服務（Laysi/trustiot-v2#881）

## 環境

- 展示機（lab 的 Mac mini M1，Fedora Linux Asahi Remix 44，aarch64）：k3s `v1.36.5+k3s1`，單一節點；Flux 部署 `clusters/demo`，HelmRelease 是 chart `0.0.0-rc.12`（`trustiot.v8`，`deployed`），GitRepository 在 main `978d7cac`。
- 腳本在工作站執行，經 ssh 在展示機上以 `sudo k3s kubectl` 讀取；分支 `feat/881-demo-dashboard-check`（從 main `978d7cac` 開始）。
- 節點的名稱就是主機名稱，報告與原始資料以 `<demo-node>` 遮蔽；位址不列。

## 指令

以下都只讀，不取 lab 鎖：

```sh
: "${DEMO_SSH_HOST:?set DEMO_SSH_HOST to the ssh host of the demo machine}"
# kubelet 實際的 podPidsLimit：nightly 的同一個檢查，kubectl 經 ssh 在展示機上執行
# （kubectl-demo.sh 只做 exec ssh "$DEMO_SSH_HOST" sudo k3s kubectl "$@"，拒絕含空白或 shell 字元的參數）
bun scripts/nightly/pod-pids-limit.ts --kubectl <dir>/kubectl-demo.sh
# 本 issue 的腳本，dry run
bun scripts/demo/pod-pids.ts --ssh-host "$DEMO_SSH_HOST"
# 部署中的 chart 與 pod
bun scripts/demo/flux.ts status --ssh-host "$DEMO_SSH_HOST"
# 記憶體
ssh "$DEMO_SSH_HOST" 'free -h; sudo k3s kubectl top node; sudo k3s kubectl top pods -A --sort-by=memory'
```

## 結果

### 開始前的狀態（2026-11-01，只讀）

原始輸出：[baseline.txt](data/2026-11-01-881-demo-dashboard-check/baseline.txt)。其中 `/etc/rancher/k3s/config.yaml` 是整份直接印出，只因為印之前已先確認該檔沒有任何密鑰（只有 `disable: [traefik]`）；`pod-pids.ts` 的原則是不印設定檔的其他行（k3s 的設定檔可能有 token），之後重跑一律用它的 dry run（只看 SHA-256 與要加的行），不再直接印這個檔。

| 項目 | 結果 |
| --- | --- |
| kubelet 實際的 `podPidsLimit` | `-1`（不限制）；`pod-pids-limit.ts` 結束碼 1：`podPidsLimit -1, expected 16384` |
| `/etc/rancher/k3s/config.yaml` | 只有 `disable: [traefik]`，沒有 `kubelet-arg`；SHA-256 `3703d69b…5789b` |
| 其他 kubelet 參數來源 | 沒有 `config.yaml.d`；unit 的 ExecStart 是 `k3s server`，沒有旗標；三個環境檔都不存在 |
| `kernel.pid_max` | 4194304（64 位元 systemd 的預設，chart README 要求維持） |
| `pod-pids.ts` dry run | 在檔尾加上 `kubelet-arg:` 與 `  - pod-max-pids=16384` 兩行，SHA-256 變成 `1855b2d7…e83965`；備份 `config.yaml.pre-881`；重啟 k3s；確認碼 `e56ff659fa11` |
| 部署中的 chart | `0.0.0-rc.12`：沒有 `deploy/images/dashboard-check`（2026-10-31 的 Laysi/trustiot-v2#884 才加），要換到之後發的版本 |
| 展示環境打開的服務 | identity、assets、certs、mqtt-bridge、ingest、gateway-ops 與 broker；AI 閘道、assistant 都還沒打開 |
| 記憶體 | `free -h` available 2.6Gi（總共 7.4Gi，swap 用了 882Mi）；節點的 requests 合計 2612Mi（34%）、limits 合計 9970Mi（132%）；用量最大的是 ClickHouse 507Mi、Postgres 240Mi |

### 套用 k3s 的行程數上限（2026-11-01 18:03，UTC+8）

維護者在展示機以 `bun scripts/demo/pod-pids.ts --ssh-host "$DEMO_SSH_HOST" --apply --confirm <dry run 印出的確認碼>` 套用（過程與結果記在 Laysi/trustiot-v2#881 的 2026-11-01 留言）：

| 項目 | 結果 |
| --- | --- |
| lab 鎖 | 取得，結束時釋放 |
| `/etc/rancher/k3s/config.yaml` | SHA-256 `3703d69b…` → `1855b2d7…`（與 dry run 的計畫相同），備份保留為 `config.yaml.pre-881` |
| 重啟 k3s | 約 10 秒後 API server 就緒，節點回到 Ready；cloudflared 沒有中斷 |
| 讀回 kubelet 的 `podPidsLimit` | 16384（腳本在重啟後讀回，不是 16384 時以失敗結束） |
| 之後的 dry run | `already has pod-max-pids=16384`、`nothing to change`；所有 pod 是 Running 或 Completed |

### 待測

以下要等前提完成才做（見結論）：換到含儀表板檢查的 chart、打開 AI 閘道與 assistant（Laysi/trustiot-v2#939）之後打開儀表板檢查，再做隔離實測與 `correct` 樣本頁面的判定。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 展示機的 kubelet 實際的 `podPidsLimit` 是 16384 | 通過：2026-11-01 由維護者以 `pod-pids.ts --apply` 套用，重啟後讀回 16384（見「套用 k3s 的行程數上限」） |
| 展示叢集的隔離實測通過 | 待測：檢查服務還沒在展示環境執行 |
| 檢查服務在展示環境就緒，`correct` 樣本頁面判定 `passed` | 待測：部署中的 chart 沒有它，assistant 也還沒打開 |
| `bun test deploy/flux` 通過；「打開儀表板檢查」與 runbook 的步驟更新 | 通過：`bun test deploy/flux` 79 項通過（建置主機）；deploy/flux/README.md「打開儀表板檢查」、runbook「展示機 k3s 的行程數上限」「打開儀表板檢查」 |
