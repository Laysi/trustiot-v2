# agent 執行中出口規則被清掉時不放行（2026-10-31）

對應 issue：Laysi/trustiot-v2#878（預設拒絕 Laysi/trustiot-v2#865 的前提條件）。這份報告記錄兩層補強的驗證：agent 的 unit 新增的 `IPAddressDeny=`（cgroup BPF，不靠 nftables），以及每 10 秒檢查一次、把被清掉的規則載回的 `trustiot-agent-egress-check.timer` 與 `.service`。設計與理由在 `edge/agent/README.md` 的「執行中被清掉的規則」。

結果先講：

- agent 執行中 root 執行 `nft flush ruleset` 之後，agent 的 unit 連 loopback（`127.0.0.1`、`127.0.0.2`、`::1`、`0.0.0.0`）、metadata（`169.254.169.254`、`fd00:ec2::254`）、多播（`239.1.2.3`、`ff05::1:3`）與 `255.255.255.255` 都被拒：UDP 立刻 `EPERM`，TCP 逾時。拿掉 `IPAddressDeny=` 的對照組，同樣的替身在規則被清掉後連得到 loopback、metadata 與多播。
- 檢查在 timer 的上限（11 秒）之內發現規則不見並載回：開發中與最後的八次執行量到 8.6 到 10.1 秒（從 `nft flush ruleset` 到檢查的 journal 訊息）。載回之後 agent 被重啟、重啟後的替身看到規則在；規則檔寫壞時 agent 停住，修好之後帶著規則自己回來；檢查換成什麼都不做的對照組，規則一直不在。
- agent 需要的連線（LAN 上的設備，含 APIPA 位址的；雲端的名稱與 443；網路上的 DNS 伺服器，含 link-local 位址的）在規則在與被清掉時都照常。
- 閘道器自己的 LAN 位址、link-local 位址與子網路廣播不在 `IPAddressDeny=` 裡（會隨網路改變），規則被清掉時還連得到，要等檢查；測試把這一點寫死，改變時會知道。
- 開發中兩種 unit 依賴的寫法被測試抓到問題、沒有採用（見「沒有採用的寫法」）：其中一種會讓 agent 的行程意外結束後再也不重啟。

## 環境

- 建置主機（x86_64 Linux），分支 `fix/878-egress-flush-gap`，最後一次合併的 main 是 `320c1223`（含 Laysi/trustiot-v2#874）。核心設定 `kernel.unprivileged_bpf_disabled = 2`、cgroup v2。
- 容器：`edge/agent/tests/egress/Containerfile`（Debian 13，與閘道器相同的 nftables 與 systemd 257），systemd 為 PID 1，agent 換成替身（`tests/egress.rs` 的 `STAND_IN`）。規則只載入容器自己的 network namespace（`--network none`），沒有動到建置主機的規則。
- 替身：每次啟動時試送一個 UDP 封包到 `198.51.100.1`（閘道器以本機路由表持有的位址，只有出口規則拒絕它、`IPAddressDeny=` 不拒），記下 `reached` 或 `refused`；之後一直執行，看到狀態目錄裡的 `probe-now` 就在 agent 的 unit 底下（沙箱與 `IPAddressDeny=` 都套用）逐行執行 `lab probe self ...` 或 `lab resolve self ...`，把結果寫回 `probe-results`。
- rootless 與 rootful：systemd 要能載入 cgroup BPF 程式才會套用 `IPAddressDeny=`。rootless podman 的容器做不到（主機限制 BPF 給特權行程），而且 systemd 不報錯、照常啟動 unit（2026-10-31 以暫時的 unit 實測：rootless 時 `IPAddressDeny=any` 的 unit 照樣送得出封包，rootful 時被拒）。所以驗證 `IPAddressDeny=` 的測試先以一個 `IPAddressDeny=any` 的暫時 unit 確認，套用不了就印出原因並略過；它以 root 執行一次（rootful podman，私有的容器儲存，用完刪掉，[`rootful.sh`](data/2026-10-31-878-egress-flush-gap/rootful.sh)）。其他測試 rootless 與 rootful 都跑。
- 閘道器：2026-10-31 唯讀檢查 lab 的閘道器（ssh，只讀，沒有寫任何檔案）：核心 6.18（`CONFIG_BPF_SYSCALL=y`、`CONFIG_CGROUP_BPF=y`、`CONFIG_BPF_JIT=y`）、cgroup v2、systemd 257，具備套用 `IPAddressDeny=` 的條件。沒有在閘道器上安裝或執行新的 unit（見「沒有做的」）。

## 指令

```sh
# repo 根目錄，建置主機（x86_64 Linux）；rcheck 的工作設 CI=1
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p repo-check
cargo test --locked -p agent                       # 含 egress 與 systemd_unit，rootless
CI=1 cargo test --locked -p agent --test egress --test systemd_unit -- --nocapture
cargo test --locked --workspace --exclude agent --no-fail-fast
bun install --cwd edge/image --frozen-lockfile && bun run --cwd edge/image typecheck && bun run --cwd edge/image test

# 以 root 跑一次 egress 的全部測試（rootful podman）
bash docs/test-reports/data/2026-10-31-878-egress-flush-gap/rootful.sh
```

## 結果

### 驗收條件 1：規則被清掉之後，agent 仍連不上本機、metadata 與多播位址

測試 `with_its_rules_flushed_the_agents_unit_still_refuses_loopback_metadata_multicast_and_broadcast`（rootful，[`rootful-egress.txt`](data/2026-10-31-878-egress-flush-gap/rootful-egress.txt)）。檢查的 timer 換成什麼都不做（這個測試只看規則被清掉、檢查還沒載回之前的狀態），`nft flush ruleset` 之後：

| 目的地 | 結果 |
| --- | --- |
| UDP `127.0.0.1`、`127.0.0.2`、`::1` | `not-permitted`（`EPERM`） |
| TCP `127.0.0.1:8080`、`0.0.0.0:8080`（root 連得上的本機服務） | `timeout`（SYN 被丟掉，10 秒的連線逾時） |
| UDP `169.254.169.254`、`fd00:ec2::254`；TCP `169.254.169.254:80`（有服務在聽） | `not-permitted`；`timeout` |
| UDP `239.1.2.3`、`ff05::1:3`、`255.255.255.255` | `not-permitted` |
| UDP 閘道器自己的 `10.81.3.2`、`fe80::81:2%veth0`、子網路廣播 `10.81.3.255` | `ok`（不在 `IPAddressDeny=`，等檢查載回規則） |

對照組：加一個 drop-in 清空 `IPAddressDeny=`、重啟 agent（重啟載入規則，替身記下 `refused`）、再清掉規則：UDP `127.0.0.1`、TCP `127.0.0.1:8080`、TCP `169.254.169.254:80`、UDP `239.1.2.3` 都是 `ok`。所以上表的拒絕是 `IPAddressDeny=` 的效果，不是規則或環境。

rootless 時這個測試印出 `skipped: systemd in the container does not apply IPAddressDeny= ...` 並略過（[`rootless-agent-fmt-clippy.txt`](data/2026-10-31-878-egress-flush-gap/rootless-agent-fmt-clippy.txt) 的 egress 那一行是 8 個通過，含這個略過的）。

### 驗收條件 2：在時間上限內重新載入，或 agent 停止

測試 `rules_flushed_while_the_agent_runs_come_back_within_the_checks_bound_or_the_agent_stops`（rootless 與 rootful 都通過）：

1. 啟動 agent，替身記下 `refused`，timer 是 `active`。第一次檢查之後 `Result=success`，`journalctl -u trustiot-agent-egress-check.service` 是空的（`LogLevelMax=notice`：正常的檢查連 systemd 的啟動、結束訊息都不留）。
2. `nft flush ruleset`（root，不經 systemd），同時以 `systemd-cat` 在 journal 記下時間。檢查載回規則；flush 到檢查的「table inet trustiot_agent is missing」訊息的間隔不超過 timer 的 `OnUnitActiveSec=` 加 `AccuracySec=`（11 秒，從 timer 檔讀出）。量到的值（開發中與最後的八次執行）：8.55、9.24、9.42、9.61、9.67、9.77、9.78、10.08 秒；最後一次 rootful 的執行是 9.61 秒（[`rootful-egress.txt`](data/2026-10-31-878-egress-flush-gap/rootful-egress.txt)）。間隔由 timer 決定：規則被清掉的時刻落在兩次檢查之間的哪裡，就差多少。
3. 檢查讓 agent 重啟：替身記下第二筆 `refused`（規則在），agent 的 `InvocationID` 換了，規則的 unit 的 `InvocationID` 沒變（檢查自己載入，規則的 unit 沒有重跑），timer 仍是 `active`。
4. 規則檔尾端加一行 `not nft`、再清掉規則：檢查記下「did not load」並以失敗結束，agent 的 `MainPID=0`、`Result=exit-code`、不是 `active`，替身沒有執行（仍是兩筆），規則仍不在。之後再等兩次同樣失敗的檢查：agent 在等自動重啟（不是 `active`），檢查不去重啟它，所以 `NRestarts`（手動的重啟會把它歸零）只增不減，`RestartSteps=` 拉長的間隔照常（安全審查 L1 之後加的，見「安全審查之後的修改」）。
5. 規則檔換回原本的內容：`Restart=always` 的下一次自動重啟帶著規則回來（替身第三筆 `refused`），agent 與 timer 都是 `active`。
6. 對照組：檢查換成 `/bin/true`、再清掉規則，等檢查又執行了兩次，規則仍不在。

既有的 nftables.service 測試（`the_rules_are_in_force_before_the_agent_starts_and_a_failed_load_keeps_it_stopped`）與 Laysi/trustiot-v2#863 的測試照常通過（rootless 與 rootful）；兩者都把檢查換成什麼都不做，只測自己的機制。#863 的測試多了一項：`kill -KILL` 之後恰好一次自動重啟（`NRestarts` 加 1）、timer 仍在。

另一個測試 `the_enabled_agent_starts_at_boot_with_its_check`：`systemctl enable trustiot-agent.service` 後重啟容器，agent 與 timer 都是 `active`，替身記下 `refused`，`journalctl --boot` 沒有 `ordering cycle`。

### 驗收條件 3：agent 需要的連線照常

同一個 rootful 測試，規則在與被清掉時各一次，結果相同（[`rootful-egress.txt`](data/2026-10-31-878-egress-flush-gap/rootful-egress.txt)）：

| 連線 | 結果 |
| --- | --- |
| TCP LAN 上的設備 `10.81.3.1:80`、`[fd81:3::1]:80` | `ok` |
| TCP APIPA 位址的設備 `169.254.81.1:80`（沒有 DHCP 的轉換器的情況） | `ok` |
| 經網路上的 DNS 伺服器（`nameserver 10.81.3.1`）解析 `cloud.test` | `10.81.3.1` |
| TCP `cloud.test:443` | `ok` |
| 經 link-local 位址的 DNS 伺服器（`nameserver fe80::81:1%veth0`）解析 `cloud.test` | `10.81.3.1` |

Laysi/trustiot-v2#813 的 `the_agent_resolves_and_reaches_the_cloud_with_the_rules_in_force` 也照常通過。

### 驗收條件 4：agent 不是 root、沒有新增 capability；新的特權 unit 權限寫明並有檢查

- agent 的 unit 只加了 `IPAddressDeny=` 與 `Wants=` timer，帳號、`CapabilityBoundingSet=`（空）、`NoNewPrivileges` 不變；`tests/systemd_unit.rs` 原有的檢查照常通過，#863 的測試確認重啟後的行程帳號與 capability。
- 檢查的 unit：root（`systemctl` 經 systemd 的私有 socket 只接受 root）、`CapabilityBoundingSet=CAP_NET_ADMIN`、`NoNewPrivileges`、`RestrictAddressFamilies=AF_NETLINK AF_UNIX`、`IPAddressDeny=any`、`ProtectSystem=strict` 等，寫在 `edge/agent/README.md` 與 unit 檔的註解。`tests/systemd_unit.rs` 的 `a_check_loads_the_egress_rules_again_while_the_agent_runs` 檢查這些設定、指令的每一段、沒有 `+`/`!`/`-` 前綴、只用 `try-` 的 systemctl；`the_unit_passes_systemd_analyze_verify` 對四個 unit 執行 `systemd-analyze verify`；`the_unit_scores_at_most_the_allowed_exposure` 要求檢查的 unit 評分不高於 2.0（實測 1.4）。

### 驗收條件 5：給 #865 的約定

寫在 `edge/agent/README.md` 的「執行中被清掉的規則」：檢查以 `try-reload-or-restart` 讓 agent 重新填入（現在是重啟，agent 啟動時本來就先送清單；#865 加上 `ExecReload=` 就改成 reload）、填回的時間上限、預設拒絕的 table 要放進同一個規則檔並加進檢查，以及檢查看不到的兩種 reload 由誰處理。

### 驗收條件 6：映像測試、`cargo test -p agent`、測試報告

- `bun run --cwd edge/image test` 與 `typecheck` 通過（`AGENT_SYSTEMD_FILES` 加上兩個新檔案）。
- `cargo test --locked -p agent`（rootless，`CI=1`）全部通過；egress 的 8 個測試中，systemd 的測試都有執行，只有驗證 `IPAddressDeny=` 的那一個在 rootless 時略過，它在 rootful 的執行通過。
- `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo test --locked -p repo-check`、`cargo test --locked --workspace --exclude agent --no-fail-fast` 通過。

## 沒有採用的寫法

兩種 timer 與 agent 之間的依賴在開發中被 #863 的 `kill -KILL` 測試抓到問題：

- timer `BindsTo=trustiot-agent.service`、agent `Requires=` timer（[`rejected-bindsto.txt`](data/2026-10-31-878-egress-flush-gap/rejected-bindsto.txt)）：行程被結束時 agent 的 unit 短暫變成 failed，`BindsTo=` 停掉 timer，這個停止又經 `Requires=` 傳回 agent，自動重啟被取消；40 秒後 agent 仍是 `failed`，300 秒內沒有重啟。上線的話，agent 的任何一次當掉都會讓它一直停著。
- agent `Requires=` timer（timer `PartOf=` 或 `Requires=` agent）（[`rejected-requires.txt`](data/2026-10-31-878-egress-flush-gap/rejected-requires.txt)）：自動重啟會發生（journal 有「Scheduled restart job, restart counter is at 1」），但重啟傳到 timer 之後又重啟 agent 一次，`NRestarts` 回到 0，`RestartSteps=` 的間隔就不會拉長。

採用的是 agent `Wants=` timer、timer `PartOf=` 並 `After=` agent：當掉之後恰好一次自動重啟、計數加 1，timer 跟著重啟。代價是 root 單獨停掉 timer 時 agent 繼續執行（`IPAddressDeny=` 仍在）。

## 安全審查之後的修改

安全審查（2026-10-31）沒有 Critical 或 High。依它的 Medium 與 Low 改了三處，之後（2026-11-01）重跑 `cargo fmt --all --check`、`cargo clippy --locked -p agent --all-targets -- -D warnings`、`systemd_unit`（16 個通過）與 `egress`（rootless 8 個通過，`IPAddressDeny=` 那一個照預期略過；以 `rootful.sh` 執行 8 個通過），結果在 [`after-review.txt`](data/2026-10-31-878-egress-flush-gap/after-review.txt)。檢查發現規則不見的時間 9.46 秒（rootless）與 9.41 秒（rootful）；規則檔寫壞時，三次失敗的檢查時 agent 的 `NRestarts` 是 `[0, 2, 3]`（只增不減）：

- **L1**：規則檔載入失敗時，原本每 10 秒就 `try-restart` agent 一次，agent 在等自動重啟時也照樣重啟，蓋過 `RestartSteps=` 拉長的間隔。現在先 `systemctl is-active --quiet trustiot-agent.service`，agent 正在執行才重啟。上方驗收條件 2 的第 4 步加了 `NRestarts` 只增不減的測試。
- **L2**：`nft list tables` 本身失敗（nft 或核心介面出問題，不是 table 不見）時，原本會進入「載入失敗、重啟 agent」的分支；agent 的 `ExecStartPre=` 不在檢查的沙箱裡、可能照樣成功，10 秒後又被重啟。現在先問 `nft list tables`，失敗就記一筆錯誤、以失敗結束，不載入也不重啟。`tests/systemd_unit.rs` 依順序檢查指令的每一段。
- **M1**：`rootful.sh` 原本最後執行 `podman system reset --force`，私有儲存的設定沒有生效時可能清掉 root 原本的容器儲存。拿掉這一行，`mktemp` 失敗或目錄不在時在以 root 執行任何東西之前停下，名稱篩選加上引號。

沒有改、記在這裡的：檢查只看 `table inet trustiot_agent` 在不在，只刪掉其中幾條規則或集合時看不出來（L3；root 本來就能改規則，#865 的預設拒絕 table 照 README 的約定加進檢查）；`IPAddressDeny=` 無聲失效的自我檢查見下方「待測」（L4）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. `nft flush ruleset` 之後，agent 帳號仍連不上本機、link-local（含 169.254.169.254）與多播位址 | 通過：loopback、`0.0.0.0`、兩個 metadata 位址、多播、`255.255.255.255` 被拒（rootful）。link-local 只拒絕 metadata 位址：閘道器自己的 link-local 位址與 LAN 位址在檢查載回規則之前（最多 11 秒）連得到，是刻意的取捨（APIPA 的設備與 link-local 的 DNS 伺服器，見 README），由測試寫死 |
| 2. 時間上限內重新載入或 agent 停止；nftables.service 的測試照常 | 通過：上限 11 秒，實測 8.6–10.1 秒；載入失敗時 agent 停住並自動恢復 |
| 3. agent 需要的連線照常 | 通過 |
| 4. 非 root、沒有新增 capability；特權 unit 權限寫明並有檢查 | 通過 |
| 5. 給 #865 的約定 | 通過（`edge/agent/README.md`） |
| 6. 映像測試、`cargo test -p agent` 的 systemd 測試沒有略過、測試報告 | 通過：systemd 的測試都有執行；驗證 `IPAddressDeny=` 的那一個 rootless 時略過、以 root 執行通過 |

待測：

- 在閘道器上安裝新的 unit、實際清掉規則：要等映像（Laysi/trustiot-v2#809、Laysi/trustiot-v2#866）裝上這些檔案。容器裡的 systemd 與 nftables 版本與閘道器相同，閘道器的核心具備條件（上方「環境」）。
- 閘道器上 `IPAddressDeny=` 確實生效的自我檢查（例如 agent 啟動後確認 cgroup 掛了 BPF 程式，或 `scripts/check-gateway.sh` 加一項）：systemd 套用不了時不會報錯，這一層會無聲失效。它是安全關卡，只在預設拒絕上線時才有意義，所以不降為低優先級，而是寫進 Laysi/trustiot-v2#865 或 Laysi/trustiot-v2#866 當作前提條件。
- 在建置主機（arm64 Linux VM）上重跑：這次只在 x86_64 跑，依專案的驗證分級屬於進階驗證，延後。
