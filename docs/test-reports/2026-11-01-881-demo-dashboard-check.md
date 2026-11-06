# 展示環境：開啟儀表板檢查服務（Laysi/trustiot-v2#881）

## 環境

- 展示機（lab 的 Mac mini M1，Fedora Linux Asahi Remix 44，aarch64）：k3s `v1.36.5+k3s1`，單一節點；Flux 部署 `clusters/demo`，HelmRelease 是 chart `0.0.0-rc.12`（`trustiot.v8`，`deployed`），GitRepository 在 main `978d7cac`。
- 腳本在工作站執行，經 ssh 在展示機上以 `sudo k3s kubectl` 讀取；分支 `feat/881-demo-dashboard-check`（從 main `978d7cac` 開始）。
- 節點的名稱就是主機名稱，報告與原始資料以 `<demo-node>` 遮蔽；位址不列。
- 第二階段（2026-11-07，UTC+8）：HelmRelease 是 chart `0.0.0-rc.15`（`deployed`），AI 閘道、telemetry-query、assistant 與 `web` 已打開；分支 `feat/881-demo-dashboard-check-on`（從 main `6f3db553` 開始）。展示機只讀：腳本在工作站執行，經 ssh 讀取；單元測試在建置主機（arm64 Linux VM）執行。

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

### 第二階段：打開前的狀態（2026-11-07，只讀）

前提都已具備，打開的 PR 只改 `values-demo.yaml`：

| 項目 | 結果 |
| --- | --- |
| 部署中的 chart | `0.0.0-rc.15+f31fd6faf13f`，`deployed`；含 `deploy/images/dashboard-check` |
| 展示環境打開的服務 | 第一階段的服務，加上 AI 閘道、telemetry-query、assistant 與 `web`；儀表板檢查沒有 |
| kubelet 的 `podPidsLimit` | 16384（2026-11-01 套用的值仍在） |
| 記憶體 | `free -m` available 2904 MiB（總共 7543 MiB，swap 用了 661 MiB）；節點的 requests 合計 2836Mi（37%）、limits 合計 11506Mi（152%）；用量最大的是 ClickHouse 515Mi、Postgres 303Mi。儀表板檢查加上 requests `128Mi`（合計約 2964Mi，39%）、limits `512Mi`，判定放得下 |
| 探測用的映像 | Postgres 的 pod 以 `ghcr.io/laysi/trustiot-v2/postgres:0.0.0-rc.15@sha256:da0ac81e…` 執行，映像在節點上；它有探測要的 bash、getent、coreutils，使用者 26 |

### 第二階段：隔離實測腳本的 dry run（2026-11-07 02:33，UTC+8，只讀）

`bun scripts/demo/dashboard-check.ts --ssh-host "$DEMO_SSH_HOST"` 在合併前只有一個擋下的原因：叢集裡還沒有儀表板檢查的 Service。原始輸出：[phase2-dry-run.txt](data/2026-11-01-881-demo-dashboard-check/phase2-dry-run.txt)。

| 項目 | 結果 |
| --- | --- |
| HelmRelease | `ok`：最新一筆 `0.0.0-rc.15+f31fd6faf13f`（`deployed`） |
| kubelet | `ok`：`podPidsLimit 16384` |
| 探測用的映像 | `ok`：release 的 Postgres 映像，已在節點上 |
| 擋下 | `the release should have one Service of dashboard-check, found 0`（合併、Flux 部署之後才會消失） |
| 維運用的 tunnel | `cloudflared.service kept running (same process)`；結束碼 0 |

### 第二階段：ssh 的長連線（2026-11-07，只讀）

探測容器的 stdin 經 ssh 送到展示機並保持開著。以腳本的 ssh 選項（`BatchMode`、`ServerAliveInterval=30`、`ServerAliveCountMax=6`）與 `keepStdinOpen` 從工作站對展示機執行「讀一行 stdin、沉默 200 秒、印出讀到的行」：201 秒後收到 `got go`，結束碼 0。之前觀察到展示機沉默約 3 分鐘的 ssh 連線會被斷掉，所以腳本的 ssh 一律帶 keepalive。

### 第二階段：自動測試（2026-11-07，建置主機（arm64 Linux VM））

| 指令 | 結果 |
| --- | --- |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun test scripts`（含 `scripts/demo/dashboard-check.test.ts` 與 nightly 的測試） | 2172 項通過，9 項略過，0 失敗 |
| `bun test deploy/flux` | 86 項通過，0 失敗 |
| `cargo test -p repo-check` | 37 項通過 |

### 待測

合併、Flux 部署之後由協調者照 runbook「打開儀表板檢查」第 2–3 步執行，結果補進本報告：

- `flux.ts status`：根 Kustomization 在合併的 commit，HelmRelease 最新一筆 `deployed`，`trustiot-dashboard-check-*` Running 且 ready。
- `bun scripts/demo/dashboard-check.ts --ssh-host "$DEMO_SSH_HOST" --apply --confirm <確認碼>` 的輸出（已遮蔽，可以直接放進 `data/`）：每項探測 `PASS`、assistant 的 `correct` 樣本頁面 `passed`、三個 Deployment 滾動完成，最後一行 `PASS all <n> probes`。
- 任何一項 `FAIL`：revert 打開的 PR（deploy/flux/README.md「打開儀表板檢查」第 4 步），記下失敗的項目。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 展示機的 kubelet 實際的 `podPidsLimit` 是 16384 | 通過：2026-11-01 由維護者以 `pod-pids.ts --apply` 套用，重啟後讀回 16384（見「套用 k3s 的行程數上限」） |
| 展示叢集的隔離實測通過 | 待測：打開的 PR 合併、Flux 部署之後以 `scripts/demo/dashboard-check.ts --apply` 執行；2026-11-07 的 dry run 只差服務還沒部署（見「第二階段」） |
| 檢查服務在展示環境就緒，`correct` 樣本頁面判定 `passed` | 待測：同一次執行裡 assistant 送 `correct` 樣本頁面；前提（chart `0.0.0-rc.15`、assistant 已打開、記憶體）2026-11-07 都已具備 |
| `bun test deploy/flux` 通過；「打開儀表板檢查」與 runbook 的步驟更新 | 通過：`bun test deploy/flux` 79 項通過（第一階段，建置主機）；第二階段打開之後 86 項通過（2026-11-07，建置主機（arm64 Linux VM）），其中 overlay 渲染出 `trustiot-dashboard-check`、沒有路由指向它、assistant 拿到 `TRUSTIOT__DASHBOARD_CHECK__URL`；deploy/flux/README.md「打開儀表板檢查」的四個步驟、runbook「展示機 k3s 的行程數上限」「打開儀表板檢查」 |
