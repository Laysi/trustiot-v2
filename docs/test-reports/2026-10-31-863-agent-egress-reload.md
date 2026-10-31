# agent 每次啟動前重新載入出口規則（2026-10-31）

對應 issue：Laysi/trustiot-v2#863（父 issue Laysi/trustiot-v2#814）。這份報告記錄 agent 的 unit（`edge/agent/systemd/trustiot-agent.service`）新增的 `ExecStartPre=+/usr/sbin/nft -f /usr/share/trustiot-agent/trustiot-agent-egress.nft` 的驗證：root 不經 systemd 清掉規則之後，agent 的自動重啟先重新載入規則，載入不了就不執行。

結果先講：在 Debian 13 容器（systemd 為 PID 1，與閘道器相同的 nftables 與 systemd）裡，`nft flush ruleset` 之後規則的 unit 仍是 active；以 `kill -KILL` 結束 agent 的替身後，`Restart=always` 的自動重啟先重新載入規則（規則的 unit 沒有重跑），替身啟動時連 `127.0.0.1:8080` 被拒。重啟後的行程是 agent 的帳號、五個 capability 集合都是 0、`NoNewPrivs` 為 1。規則檔寫壞時，重啟的 `ExecStartPre=` 失敗、替身沒有執行、規則仍不在；規則檔修好之後，下一次自動重啟帶著規則回來。拿掉 `ExecStartPre=` 那一行的對照組，重啟後的替身連得上 `127.0.0.1:8080`，測試失敗，所以測試確實量得到這個缺口。兩種建置主機（x86_64 Linux、arm64 Linux VM）都通過，systemd 的測試都沒有略過。

## 環境

- 建置主機（x86_64 Linux）與建置主機（arm64 Linux VM，SELinux enforcing），分支 `fix/863-agent-start-reloads-egress`，以 main `7eac625e` 為基底。
- 容器：`edge/agent/tests/egress/Containerfile`（`debian:trixie-slim`，以 digest 釘選，加上 Debian 的 `nftables`、`systemd` 等），與 Laysi/trustiot-v2#813 的測試相同；規則只載入容器自己的 network namespace（`--network none`），沒有動到建置主機的規則。
- agent 換成替身（`tests/egress.rs` 的 `systemd_lab`）：一個 shell script，啟動時試連閘道器的 `127.0.0.1:8080`（root 連得上的本機服務），把 `reached` 或 `refused` 記到 agent 的狀態目錄，然後一直執行。unit 只以 drop-in 改成 `Type=exec`、`WatchdogSec=0`（替身不送 sd_notify），其他設定（依賴、`Restart=always`、沙箱、`ExecStartPre=`）都是 repo 裡的 unit。

## 指令

```sh
# repo 根目錄，在兩種建置主機上各跑一次
cargo fmt --all --check
CI=1 cargo test --locked -p agent --test egress --test systemd_unit -- --nocapture

# 對照組：拿掉 agent unit 的 ExecStartPre= 那一行，只跑新的測試，預期失敗
sed -i '/^ExecStartPre=+/d' edge/agent/systemd/trustiot-agent.service
CI=1 cargo test --locked -p agent --test egress a_restart_after
git checkout edge/agent/systemd/trustiot-agent.service
```

新的測試 `a_restart_after_the_rules_were_flushed_loads_them_again_or_keeps_the_agent_stopped` 的步驟：

1. `systemctl start trustiot-agent.service`：依 `nftables.service`、規則、agent 的順序啟動，替身記下 `refused`。
2. `nft flush ruleset`（root，不經 systemd）：`table inet trustiot_agent` 不見了，`trustiot-agent-egress.service` 與 agent 仍是 `active`（這就是缺口）。
3. `kill -KILL <agent 的 MainPID>`：等替身記下第二筆（上限 300 秒，只用來抓「重啟一直沒來」）。
4. 再清掉一次、在規則檔尾端加一行 `not nft`，再結束替身：等 `NRestarts` 增加且 `Result=exit-code`。
5. 規則檔換回原本的內容：等替身記下第三筆。

## 結果

| 步驟 | 檢查 | 結果 |
| --- | --- | --- |
| 1 | 替身的紀錄 | `refused` |
| 2 | `nft list table inet trustiot_agent` | 失敗（規則不在） |
| 2 | 規則的 unit、agent 的 `ActiveState` | `active`、`active` |
| 3 | 替身的紀錄 | `refused`、`refused` |
| 3 | 規則 | 已載入 |
| 3 | agent 的 `NRestarts` | 增加 1 |
| 3 | 規則的 unit 的 `InvocationID`、`ExecMainExitTimestampMonotonic` | 不變：是 agent 的 `ExecStartPre=` 載入的 |
| 3 | 重啟後行程的 `/proc/<pid>/status` | `Uid` 四欄都是 `trustiot-agent` 的 uid；`CapInh`、`CapPrm`、`CapEff`、`CapBnd`、`CapAmb` 都是 `0000000000000000`；`NoNewPrivs: 1` |
| 4 | agent | 不是 `active`，`MainPID=0`，`Result=exit-code` |
| 4 | 替身的紀錄 | 仍是兩筆：替身沒有執行 |
| 4 | 規則 | 不在；規則的 unit 的 `InvocationID` 不變 |
| 5 | 替身的紀錄 | `refused` 三筆；規則已載入，agent `active`，沒有人手動啟動 |

`tests/systemd_unit.rs` 的新測試 `the_agents_egress_rules_load_again_before_every_start` 檢查：`ExecStartPre=` 是 `+` 加上規則 unit 的 `ExecStart=` 那個指令、前綴只有 `+`（沒有 `-`）、`Restart=always`、`User=`／`Group=` 是 `trustiot-agent`、`CapabilityBoundingSet=` 與 `AmbientCapabilities=` 是空的、`NoNewPrivileges=yes`，以及 unit 裡沒有其他以 `+` 或 `!` 提升權限的 `Exec*=`。`the_unit_passes_no_secret_on_the_command_line_or_in_the_environment`（Laysi/trustiot-v2#812）原本要求沒有 `ExecStartPre=`，改成只允許這一行。`systemd-analyze verify` 與 `security` 的測試照常通過（評分上限 2.0）。

### 兩種建置主機

| 建置主機 | `--test egress`（5 個） | `--test systemd_unit`（10 個） | systemd 的測試 |
| --- | --- | --- | --- |
| arm64 Linux VM | 5 passed（20.32 s） | 10 passed | 執行，沒有略過 |
| x86_64 Linux | 5 passed（31.41 s） | 10 passed | 執行，沒有略過 |

`--nocapture` 的輸出裡沒有任何 `skipped:` 行。

### 對照組（拿掉 `ExecStartPre=`，x86_64 Linux）

```
test a_restart_after_the_rules_were_flushed_loads_them_again_or_keeps_the_agent_stopped ... FAILED
assertion `left == right` failed: the restarted agent ran without its rules
  left: ["refused", "reached"]
 right: ["refused", "refused"]
```

沒有這一行時，重啟後的替身連上了閘道器的本機服務：這就是 Laysi/trustiot-v2#813 的安全審查指出的缺口，新的測試量得到它。

### 完整檢查

在建置主機（arm64 Linux VM）上，以最後的程式碼執行：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test --locked -p repo-check` | 18 passed |
| `CI=1 cargo test --locked -p agent` | 28 個測試執行檔，573 passed，0 failed |
| `CI=1 cargo test --locked --workspace --exclude agent --no-fail-fast` | 結束碼 0，266 個測試執行檔，2083 passed，0 failed，2 ignored |

## 結論

- [x] 規則被清掉（`nft flush ruleset`）之後 agent 重啟時，先重新載入規則或拒絕啟動，不會在沒有規則的狀態下執行：上表步驟 2 到 5，兩種建置主機；對照組證明測試量得到缺口。
- [x] 規則載入失敗時 agent 不啟動，現有的測試照常通過：步驟 4；`the_rules_are_in_force_before_the_agent_starts_and_a_failed_load_keeps_it_stopped` 等原有測試都通過。
- [x] agent 仍以非 root 執行，沒有新增 capability：`tests/systemd_unit.rs` 的 unit 檢查，加上步驟 3 對重啟後行程的 `/proc/<pid>/status`。
- [x] README 寫明做法、理由，以及之後新增的 table 與集合怎麼納入：`edge/agent/README.md`「systemd」的「出口規則」與「這一層擋不到的」。
- [x] `cargo test -p agent --test egress --test systemd_unit` 在有 podman 的建置主機上通過，systemd 的測試沒有略過：見「兩種建置主機」。
- 待測：在真的閘道器上重現（lab 的閘道器還沒有安裝出口規則，Laysi/trustiot-v2#809 的映像安裝之後）。
