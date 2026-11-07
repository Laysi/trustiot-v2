# agent 依已套用的設定填 fieldbus 放行集合，規則重新載入後再填回

Laysi/trustiot-v2#865

## 環境

- 建置與測試：arm64 Linux 建置主機（rootless podman，cgroup v2）；`IPAddressDeny=` 那一個測試另外以 root 執行（rootful podman）。
- 測試容器：`edge/agent/tests/egress/Containerfile` 建出的 Debian 13 映像（systemd 257 為 PID 1），`--network none`，網路由 `tests/egress/lab.sh` 在容器裡建立。
- 分支 `feat/865-fieldbus-allowlist`，基於 main `3abe9434`；工具版本照 repo 的 `.prototools` 與 `rust-toolchain.toml`。
- 沒有閘道器、沒有雲端：agent 是這次建置的 `trustiot-agent`，在容器裡以真正的 unit 執行（`tests/fieldbus.rs`）。

## 指令

從 repository 根目錄：

```sh
cargo clippy --locked -p agent --all-targets -- -D warnings
CI=true cargo test --locked -p agent --no-fail-fast
cargo test --locked -p agent --no-fail-fast -- --nocapture 2>&1 | grep -E '^test result|skipped:'
cargo test --locked -p agent --test fieldbus -- --nocapture
cargo test --locked -p agent --lib fieldbus
```

以 root 執行 `IPAddressDeny=` 的測試（rootful podman，暫時的容器儲存）：

```sh
cargo test --locked -p agent --test egress --no-run          # 取得測試執行檔的路徑
# 以 root：
printf '[storage]\ndriver = "overlay"\ngraphroot = "%s/graph"\nrunroot = "%s/run"\n' "$d" "$d" > "$d/storage.conf"
printf '[containers]\napparmor_profile = "unconfined"\n' > "$d/containers.conf"
CONTAINERS_STORAGE_CONF="$d/storage.conf" CONTAINERS_CONF_OVERRIDE="$d/containers.conf" \
    <egress 測試執行檔> with_its_rules_flushed_the_agents_unit_still_refuses_loopback_metadata_multicast_and_broadcast --exact --nocapture
```

`$d` 是一個暫時目錄，用完刪掉。

開發者工作站上（不編譯 agent，只跑 repo 的檢查）：`cargo fmt --all --check`、`cargo test --locked -p repo-check`、`bun test scripts/ci/changes.test.ts`、`bun run --cwd edge/image typecheck`、`bun run --cwd edge/image test`。

## 結果

### 檢查與整個 agent 的測試

| 檢查 | 結果 |
| --- | --- |
| `cargo clippy --locked -p agent --all-targets -- -D warnings` | 通過 |
| `CI=true cargo test --locked -p agent --no-fail-fast` | 35 個測試執行檔全部通過（單元測試 421 通過、2 個 ignored 是既有的） |
| 不設 `CI`、`--nocapture` | 35 個全部通過；`skipped:` 只有一行：`egress` 的 `IPAddressDeny=` 測試（rootless 容器不能載入 cgroup BPF，設計上只在 root 執行時跑，見下方） |
| 以 root 執行 `IPAddressDeny=` 的測試 | 通過 |
| 開發者工作站上的 repo 檢查 | `cargo fmt --all --check`、`repo-check`（37 通過）、`scripts/ci/changes.test.ts`（345 通過）、`edge/image` 的 typecheck 與測試（149 個，146 通過、3 個既有的略過；`agent-unit.test.ts` 改成接受只以 `[ -e <path> ]` 檢查、還沒安裝的預設拒絕檔案）都通過 |

`tests/fieldbus.rs` 在 2026-11-06 跑了 2 次（單獨一次、整個測試套件裡一次），都通過；最後一次的證據行在 [data/2026-11-06-865-fieldbus-allowlist/fieldbus-evidence.txt](data/2026-11-06-865-fieldbus-allowlist/fieldbus-evidence.txt)。下表的時間是這 2 次的範圍。`tests/fieldbus.rs`、`tests/egress.rs` 的 systemd 測試在容器裡的 systemd 開不起來時略過，`CI` 有設時也略過，所以 CI 不一定跑到它們；第 9 條驗收條件的證據是這台建置主機上的執行，CI 要不要強制或另外記錄這些 systemd 測試由 Laysi/trustiot-v2#827 處理。

### `tests/fieldbus.rs`（真的 agent，systemd 為 PID 1）

第一個測試 `the_agent_keeps_the_fieldbus_sets_to_its_devices` 每一步都確認 `fieldbus4` 的內容與兩個 Modbus 替身收到的請求（輪詢恢復）。前五步另外確認預設拒絕的 `default deny` counter 是 0，也就是 agent 沒有搶在清單之前連線。後三步的集合會空一段時間：規則不看連線狀態，已開的設備連線會被拒，reload 傳到 agent 之前閘門也照上一份清單放行新的連線，這些連線被拒並計數是設計上的結果，agent 照退避重連；所以這三步不看 counter，確認的是取樣到的集合都是清單的子集合、集合填回、輪詢恢復、InvocationID 不變。設備位址是 `lab.sh` 的測試網路（`10.81.3.0/24`）。

| 步驟 | 集合 | 其他證據 |
| --- | --- | --- |
| 啟動（存下的設定有 meter-1：`10.81.3.1:502`） | `{10.81.3.1 . 502}` | 輪詢；counter 0 |
| 加 meter-2（`10.81.3.20:502`），重啟 agent | `{10.81.3.1 . 502, 10.81.3.20 . 502}` | 兩台都輪詢；counter 0 |
| 移除 meter-1，重啟 agent | `{10.81.3.20 . 502}` | counter 0；agent 帳號探測 `10.81.3.1:502` 被拒，counter 變 1 |
| 重開容器（`podman restart`） | `{10.81.3.20 . 502}` | 存下的設定在輪詢之前填回；counter 0 |
| `nft flush ruleset` 加 `kill -KILL` agent | `{10.81.3.20 . 502}` | 自動重啟先載入規則（兩個 table 一起回來）再送清單；counter 0 |
| `systemctl reload nftables.service` | 每次取樣數百次（579 次與 928 次，空的 63 次與 59 次），其餘都等於清單，沒有一次含清單以外的目的地 | 填回時間 450–731 毫秒（從 reload 開始到 agent 記下套用）；輪詢恢復；agent 沒有重啟（InvocationID 不變） |
| agent 執行中 `nft flush ruleset` | `{10.81.3.20 . 502}` | 檢查載回規則、自己填回 nameserver 集合，再 reload agent，4.7–7.0 秒後填回（從 flush 到 agent 記下套用）；輪詢恢復；InvocationID 不變 |
| `nft delete table inet trustiot_agent_allow` | `{10.81.3.20 . 502}` | 檢查在 9.0–9.7 秒後發現（檢查的 timer 相位），載回規則、填回 nameserver 集合並 reload agent；輪詢恢復；InvocationID 不變 |

第二個測試 `before_the_component_the_agent_polls_as_before`（沒有元件的執行檔、沒有預設拒絕的檔案、規則檔沒有 `include`，等於 Laysi/trustiot-v2#866 之前的閘道器）：agent 照常輪詢、journal 有「the fieldbus allowlist component is not installed; polling without sending a list」，沒有 `inet trustiot_agent_allow`；檢查跑了兩次，journal 沒有任何紀錄、agent 沒有被 reload；`nftables.service` 的 reload 傳到 agent（「there is no list to send」），InvocationID 不變。

### 單元測試（`cargo test -p agent --lib fieldbus`、`desired::fleet`）

- 清單：IP 主機立即列入、名稱解析之後才列入、守門員拒絕的位址逐筆略過；一個名稱最多取前 4 個接受的位址（`a_name_takes_its_first_four_accepted_addresses_and_other_hosts_keep_theirs`）；1000 台設備、每台名稱解析出很多位址，清單仍在 4096 之內（`no_configuration_can_overflow_the_list`）。
- 先後：新設備的第一次撥號在含它的清單 `ok` 之後，移除的設備在 `Sink::removed` 之後才離開清單（`the_list_leads_every_new_destination_and_trails_every_removed_one`）。
- 裝了沒有：元件的執行檔決定，socket 不算（`the_installed_executable_decides_not_the_socket`）；執行檔在、socket 還不在時，閘門不讓任何連線撥出，socket 出現後不必重啟就套用清單、放行（`installed_with_the_socket_missing_nothing_is_dialed_until_it_answers`）。
- 讀不到介面位址時，IP 主機先不列入（`unreadable_interfaces_leave_ip_hosts_out_until_a_connection_lists_them`；改回只套用固定拒絕時這個測試失敗）。
- 備援重送：清單不是空的時，距上次套用 10 分鐘再送一次同一份、不換世代，空清單不送（`a_list_that_is_not_empty_is_sent_again_as_a_backstop`，暫停的時鐘；拿掉計時器時這個測試失敗）。
- 重試：沒有 `ok` 時不放行、間隔從 1 秒加倍；reload 提早結束 16 秒的等待，但離失敗的那一次至少 1 秒（`a_resend_ends_a_long_backoff_but_not_before_the_first_delay`；把提早結束的分支拿掉時這個測試失敗，等了 16.07 秒）。

### 以 root 執行（rootful podman）

`with_its_rules_flushed_the_agents_unit_still_refuses_loopback_metadata_multicast_and_broadcast` 通過：規則被清掉之後，agent 的 unit 連 loopback、`0.0.0.0`、metadata、多播與 `255.255.255.255` 都被拒，閘道器自己的位址與子網路廣播還能連；拿掉 `IPAddressDeny=` 時同樣的探測都連得上。

主機以 podman 的 AppArmor profile 限制 rootful 容器時，容器裡的 systemd 在開機前以「Failed to fork off sandboxing environment for executing generators: Protocol error」結束，測試略過。在這樣的主機上逐一試過的組合：不加 capability 或只加 `NET_ADMIN` 時開得起來；加 `SYS_ADMIN`（測試需要）就結束；`--privileged` 開得起來（degraded）；`SYS_ADMIN` 配 `apparmor=unconfined`（命令列或 `containers.conf`）開得起來。所以只為這一次執行以 `CONTAINERS_CONF_OVERRIDE` 解除 AppArmor（上面的指令）。rootless podman 不套用這個 profile，不受影響。以 root、不受 AppArmor 限制執行，等於把這個分支的程式碼以 root 交給那台主機，所以只在用完即丟的 VM 或專用的建置主機上這樣跑。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. 新增、移除設備時集合跟著變，其他目的地被拒 | 通過：上表前三列；探測清單外的目的地被拒並計數。執行中收到新設定的先後由 `desired::fleet` 的單元測試證明（容器裡沒有 broker，新設定以存檔加重啟送進去，兩者都經 `Fleet::apply`） |
| 2. 重開之後，存下的設定在輪詢之前填回 | 通過：重開容器那一列，counter 0 |
| 3. `nft flush ruleset` 加 agent 重啟之後，預設拒絕與放行一起回來，否則 agent 不啟動 | 通過：flush 加 `kill -KILL` 那一列；規則檔載入失敗時 agent 不啟動由 `tests/egress.rs` 既有的測試涵蓋（通過） |
| 4. 重新載入規則之後在輪詢之前填回；README「出口規則」與 Laysi/trustiot-v2#864 的威脅模型一致 | 通過：`nftables.service` reload、執行中 flush、只刪 table 三列（集合填回、輪詢恢復、取樣不超出清單）；集合空著的那段時間設備連線被拒是設計上的結果，agent 照退避重連；README 已改（清單由 agent 提供，重新填入也是） |
| 5. 每個名稱的位址有固定上限與固定規則，被拒的位址逐筆略過 | 通過：單元測試（前 4 個接受的位址；編譯時檢查 1000 × 4 ≤ 4096） |
| 6. `nftables.service` reload 而 agent 不重啟時，集合填回、輪詢恢復，期間不放行清單以外的目的地 | 通過：reload 那一列（取樣到的集合都是清單的子集合、輪詢恢復、InvocationID 不變） |
| 7. Laysi/trustiot-v2#866 之前照以前輪詢、不送清單 | 通過：第二個測試 |
| 8. agent 仍不是 root、沒有新的 capability | 通過：`tests/systemd_unit.rs`（帳號、空的 capability、`NoNewPrivileges`、`ExecReload=` 沒有 `+` 或 `!`） |
| 9. 文件更新；podman 主機上 `cargo test -p agent` 通過，容器與 systemd 的測試沒有略過；有測試報告 | 通過：建置主機上 rootless 只略過 `IPAddressDeny=` 那一個（需要 root，以 root 執行時通過）；CI 可能略過 systemd 測試，見 Laysi/trustiot-v2#827；本報告 |

待測：閘道器硬體上的實測（映像安裝這些檔案是 Laysi/trustiot-v2#866，之後一起做）；經真的 broker 下發新設定的端到端。
