# fieldbus 放行集合的特權元件（2026-10-31）

對應 issue：Laysi/trustiot-v2#864（父 issue Laysi/trustiot-v2#814）。這份報告記錄 `trustiot-agent-allowlist`（`edge/agent/src/bin/trustiot-agent-allowlist.rs`，清單格式在 `edge/agent/src/allowlist.rs`）與它的 socket、unit（`edge/agent/systemd/trustiot-agent-allowlist.socket`、`trustiot-agent-allowlist@.service`）的驗證。介面、權限邊界與威脅模型在 `edge/agent/README.md` 的「fieldbus 放行集合」。

結果先講：在 Debian 13 容器（與閘道器相同的 nftables 1.1.3 與 systemd 257）裡，元件依每份清單換掉兩個集合，形狀像預設拒絕的 chain 讓 agent 帳號只連得到清單裡的目的地，其他帳號不受影響。17 種不合格的請求都被整份拒絕、整個 ruleset 不變；沒有預設拒絕的 table、集合放不下、table 被刪掉時都回 `failed:`，ruleset 不變、什麼都不建立；4096 個目的地的清單換成另一份 4096 個的清單也成功。以 systemd 為 PID 1 時，socket 只有 agent 帳號連得上，instance 的 UID 不是 0 也不是 agent 的，capability 只有 `CAP_NET_ADMIN`，`NoNewPrivs` 與 seccomp 生效，`/etc` 與 `/var/lib` 唯讀，同時只有一個 instance。安全審查沒有 Critical 或 High，Medium 與 Low 都已處理（見「安全審查」）。

## 環境

- **建置主機**：arm64 Linux VM（SELinux enforcing、rootless podman），分支 `feat/864-egress-allowlist-updater`，commit `9c2bae62`（已合併當時的 main；之後只改了文件：README 威脅模型的補充與本報告）；Rust 1.99.0（`rust-toolchain.toml`）。
- **容器**：`edge/agent/tests/egress/Containerfile`（Laysi/trustiot-v2#813 的同一個映像），`debian:trixie-slim`（以 digest 釘選）加上 Debian 的 `nftables`、`iproute2`、`socat`、`dnsmasq-base`、`systemd`。nftables 1.1.3、systemd 257，和 lab 的閘道器相同（見 `docs/test-reports/2026-10-31-813-agent-egress-rules.md`）。這次建置的 `trustiot-agent-allowlist` 以 `podman cp` 放進容器的 `/usr/libexec/trustiot-agent/allowlist`。
- 規則只載入容器自己的 network namespace，沒有動到建置主機的規則。
- 沒有在閘道器上執行（見「結論」的待測）。

## 指令

```sh
# 建置主機，repo 根目錄
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked -p agent --test allowlist --test systemd_unit --test secret_inputs -- --nocapture
CI=1 cargo test --locked -p agent --no-fail-fast
cargo test --locked -p repo-check
CI=1 cargo test --locked --workspace --exclude agent --no-fail-fast
bun install --cwd edge/image --frozen-lockfile && bun run --cwd edge/image typecheck && bun run --cwd edge/image test
bun install --cwd scripts --frozen-lockfile && bun test scripts/ci
cargo build --locked --release --target aarch64-unknown-linux-gnu -p agent
```

`CI=1` 讓容器測試在沒有 podman 時失敗而不是略過。`tests/allowlist.rs` 第一個測試的網路是 `tests/egress/lab.sh network`：容器自己的 namespace 是閘道器（`10.81.3.2/24`、`fd81:3::2/64`），另一個 namespace `peer` 是現場的 LAN（`10.81.3.1`、`fd81:3::1`，在 80 與 443 上有服務）。Laysi/trustiot-v2#813 的規則已載入，另加一個代表主機規則的 `table inet host_rules`。預設拒絕的替身是 `Allowlist::declarations()` 加上一條 output chain：`meta skuid "trustiot-agent"` 的 TCP 跳到一條只放行 `@fieldbus4`、`@fieldbus6` 的 chain，其餘 TCP reset。

## 結果

原始輸出（evidence 行與每個測試執行檔的結果）：[data/2026-10-31-864-fieldbus-allowlist-updater/tests.txt](data/2026-10-31-864-fieldbus-allowlist-updater/tests.txt)。

### 清單改變時集合跟著改變（`each_list_replaces_the_sets_and_nothing_else_changes`）

- 還沒載入預設拒絕時送出一份 3 個目的地的清單：結束碼 1，`failed: nft exit status: 1`，ruleset 不變，沒有建立 `table inet trustiot_agent_allow`。
- 載入替身之後同一份清單：`ok 3`，兩個集合正好是這 3 個目的地，chain 與其他 table 不變。

| 清單 | agent 連 `10.81.3.1:443` | `10.81.3.1:80` | `[fd81:3::1]:443` | `[fd81:3::1]:80` |
| --- | --- | --- | --- | --- |
| 第一份（`10.81.3.1 443`、`fd81:3::1 443`、`192.0.2.10 502`） | ok | refused | ok | refused |
| 第二份（`10.81.3.1 80`、`fd81:3::1 80`、`192.0.2.10 503`） | refused | ok | refused | ok |

`nobody` 連 `10.81.3.1:80` 在第一份清單時也是 ok（集合只比對 agent 帳號）。refused 是 TCP reset。

### 整份拒絕

每一種請求的結束碼都是 2，回答以 `refused:` 開頭，請求前後的整個 `nft list ruleset`（counter 的數字除外）相同，集合仍是第二份清單：

| 請求 | 回答 |
| --- | --- |
| 空的請求 | `the first line is not "trustiot-agent-allowlist v1"` |
| 另一個版本（`v2`） | 同上 |
| 沒有 `end`（可能被截斷） | `no "end" line, so the list may be incomplete` |
| 夾帶 nft 指令（`flush ruleset`） | `line 3: not an IP address in canonical form` |
| 夾帶規則（`add rule ... accept`） | `line 3: not "<address> <port>"` |
| 目的地後面接指令（`10.81.3.1 443; flush ruleset`） | `line 2: not "<address> <port>"` |
| 目的地帶判決（`... accept`） | 同上 |
| 目的地帶協定（`udp 10.81.3.1 53`） | 同上 |
| 主機名稱（`cloud.test 443`） | `line 2: not an IP address in canonical form` |
| 網段（`10.81.3.0/24 443`） | 同上 |
| loopback（`127.0.0.1 8080`） | `line 3: a loopback address, which the agent never connects to` |
| metadata（`169.254.169.254 80`） | `line 2: a cloud metadata address, which the agent never connects to` |
| port 0 | `line 2: not a TCP port from 1 to 65535 in decimal` |
| 同一個目的地兩次 | `line 3: listed twice` |
| NUL byte | `line 2: not printable ASCII` |
| 4097 個目的地 | `more than 4096 entries` |
| 超過 188,448 bytes | `larger than 188448 bytes` |

格式的其他情況（IPv6 的非標準寫法、IPv4-mapped、前導零、CR、tab、各種被拒絕的位址類別等）由 `src/allowlist.rs` 的單元測試涵蓋（`cargo test -p agent --lib allowlist`）。

### nft 套用失敗時什麼都不變

- **集合放不下**：把 table 換成 `fieldbus4` 有一個元素（`10.81.3.1 . 80`）、`fieldbus6` 是 `size 1`，送出含兩個 IPv6 目的地的清單：結束碼 1，`failed: nft exit status: 1`，整個 ruleset 不變，`fieldbus4` 仍只有 `10.81.3.1 . 80`。也就是排在前面的 `flush set fieldbus4` 與它的新元素都沒有生效：整個檔案是一個 transaction。
- **table 被刪掉**（相當於 `nft flush ruleset` 或 `nftables.service` 的 reload）：結束碼 1，沒有建立 table；重新載入替身之後同一份清單 `ok 3`，chain 與一開始相同。
- **滿額**：4096 個目的地（61,248 bytes）的清單 `ok 4096`，換成另一份 4096 個不同目的地的清單也 `ok 4096`，集合正好是新的那份；空清單 `ok 0`，兩個集合都空。
- 從頭到尾 Laysi/trustiot-v2#813 的 `inet trustiot_agent`、`ip lab_nat` 與 `inet host_rules` 沒有改變。

### systemd 為 PID 1（`only_the_agents_account_reaches_the_component_which_holds_only_cap_net_admin`）

容器以 systemd 為 PID 1，裝上 repo 的兩個 unit 與 sysusers 檔，執行檔放在 unit 寫的路徑。

- agent 帳號還不存在時 socket 啟動失敗；`systemd-sysusers` 之後啟動成功，`/run/trustiot-agent-allowlist` 是 `root root 755`，socket 是 `root trustiot-agent 660`。
- 預設拒絕的 table 還沒載入時，agent 帳號經 socket 得到 `failed: nft exit status: 1`，沒有建立 table；載入 `Allowlist::declarations()` 之後得到 `ok 2`，集合正好是那份清單。
- `nobody` 連 socket：`Permission denied`，沒有回答，集合不變。
- 夾帶 `flush ruleset` 的請求：`refused: line 3: not an IP address in canonical form`，集合不變。
- journal（`-t trustiot-agent-allowlist`）的三筆：失敗（含清單的 SHA-256 與 nft 的錯誤 `No such file or directory`）、`applied 2 destinations (1 IPv4, 1 IPv6), list sha256 ...`、`refused a request of 61 bytes; the sets are unchanged: line 3: ...`；instance 的名稱是 `trustiot-agent-allowlist@<序號>-<PID>-996.service`，996 是 agent 帳號的 UID。
- 一個連線佔住 instance 時，第二個連線沒有回答、集合不變；第一個結束之後同一份請求 `ok 1`。
- 以 drop-in 把 `ExecStart=` 換成印出 `/proc/self/status` 的指令（沙箱與帳號都是真的 unit 的）：

  ```text
  Uid:	63142	63142	63142	63142
  Gid:	63142	63142	63142	63142
  CapInh:	0000000000001000
  CapPrm:	0000000000001000
  CapEff:	0000000000001000
  CapBnd:	0000000000001000
  CapAmb:	0000000000001000
  NoNewPrivs:	1
  Seccomp:	2
  touch: cannot touch '/etc/probe': Read-only file system
  touch: cannot touch '/var/lib/probe': Read-only file system
  ```

  `0x1000` 是 `CAP_NET_ADMIN`（12）。UID 63142 是 `DynamicUser` 配的臨時帳號，不是 root、不是 agent 的 996；`/tmp/probe` 寫得進去（私有的 `/tmp`）。
- 這些都在 `RestrictAddressFamilies=AF_NETLINK`、`TasksMax=4`、`PollLimitBurst=10` 之下通過：nft 只需要 netlink，元件與 nft 兩個 task 就夠。

### unit 檔與原始碼的檢查

- `tests/systemd_unit.rs`：socket 的 10 行設定與 service 的 37 行設定逐行比對（沒有多也沒有少，沒有 `[Install]`、沒有 `TriggerLimitBurst`），agent 的 unit 仍然 `User=trustiot-agent`、沒有 capability；`systemd-analyze verify` 對兩個檔案除了執行檔不在測試機上之外沒有任何抱怨；`systemd-analyze security --offline` 對 instance 的評分是 1.1（上限 1.5）。
- `tests/secret_inputs.rs`：agent 的掃描排除元件的那一個檔案，元件另有自己的檢查：只啟動一個程式，`Command::new(NFT)`（`/usr/sbin/nft`）、`.args(["-f", "-"])`、`.env_clear()`；libc 只用 `O_TMPFILE`；唯一的 `open` 是 `/tmp` 的 `O_TMPFILE`；沒有 `File`、`read_to_string`、`read_dir`、`create`、`remove_file`、`rename`、`include*`、`mod`、`path` 等名稱；`std::fs` 只匯入 `OpenOptions`；不讀任何環境變數。帶參數執行（`--help`、`--file /etc/nftables.conf` 等）時結束碼 2、標準輸出是空的、標準錯誤說它不接受參數。

### 其他檢查

- `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`：通過。
- `CI=1 cargo test --locked -p agent --no-fail-fast`：每個測試執行檔都通過（lib 365 個，另有 2 個既有的量測標成 ignored；`allowlist` 2、`egress` 4、`secret_inputs` 6、`systemd_unit` 13 等），容器測試都有執行。
- `cargo test --locked -p repo-check` 與 `CI=1 cargo test --locked --workspace --exclude agent --no-fail-fast`：通過。
- 合併 main 之後的映像測試（`bun run --cwd edge/image typecheck` 與 `test`，75 個）與 `bun test scripts/ci`（516 個）：通過。main 的映像測試要求映像安裝 `edge/agent/systemd` 的每個檔案；建置還沒有建出元件的執行檔，所以兩個新 unit 列在 `edge/image/src/pins.ts` 的 `AGENT_SYSTEMD_NOT_INSTALLED`，由 Laysi/trustiot-v2#866 安裝。
- `cargo build --locked --release --target aarch64-unknown-linux-gnu -p agent`：建出 `trustiot-agent`（6,297,168 bytes）與 `trustiot-agent-allowlist`（575,552 bytes）。
- 整串指令 `exit 0 at 16:36:03 (build host)`。

### 安全審查

元件持有 `CAP_NET_ADMIN`，輸入（agent 的請求）視為不可信，所以由另一個 agent 以安全審查的角度看過整個 diff 與威脅模型。沒有 Critical 或 High；Medium 與 Low 都已修正或寫進文件：

| 等級 | 發現 | 處理 |
| --- | --- | --- |
| Medium | 威脅模型說「被攻破的 agent 只剩 TCP」，但這取決於預設拒絕是否拒絕非 TCP；DNS 也要有人放行 | README 改成以 Laysi/trustiot-v2#865 的寫法為前提，並在約定裡要求它拒絕非 TCP、放行目前 nameserver 的 DNS、決定誰更新那條規則，兩者都要測 |
| Low | `nftables.service` 的 reload 清掉 table 之後，元件會重建一個沒有 chain 使用的 table 並回 `ok` | 元件不再建立 table 與集合（只有 `flush set` 與 `add element`），table 不在時回 `failed:`；測試涵蓋（上面的「table 被刪掉」與 systemd 測試的第一個請求） |
| Low | 一直連線會放大 journal 的量 | socket 加上 `PollLimitIntervalSec=2s`、`PollLimitBurst=10`；剩下的量寫在威脅模型 |
| Low | journal 只記數量，事後查不出套用了哪份清單 | journal 記清單的 SHA-256（`Allowlist::digest`） |
| 資訊 | 沙箱可以更緊 | 拿掉 `AF_UNIX`（只剩 `AF_NETLINK`），加上 `TasksMax=4` |
| 資訊 | 原始碼檢查可以更具體 | 檢查唯一的 `Command::new`、`.args`、`open`、`OpenOptions` 與禁止的名稱 |
| 資訊 | 給 Laysi/trustiot-v2#865 的約定缺了幾點 | 補上：守門員拒絕的目的地逐筆略過、每個名稱的位址上限、雲端連線（`transport.rs` 的 `connect_tcp`）也要在清單裡 |

審查之後，README 的威脅模型另外寫明清單由 agent 提供，與 Laysi/trustiot-v2#863 對出口規則「重新填入的內容不能來自 agent 寫得到的地方」的建議是什麼關係（「清單由 agent 提供，重新填入也是」）；給 Laysi/trustiot-v2#865 的約定列出每次載入之後重新填入的兩種寫法。這兩段只是文件，不影響上面的測試。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 元件依交給它的清單更新放行集合，清單改變時集合跟著改變（測試） | 通過：「清單改變時集合跟著改變」、滿額與空清單 |
| 只接受主機與 port 清單，格式不符、超過上限或要求其他規則時拒絕整份，集合維持原樣（測試） | 通過：「整份拒絕」的 17 種請求，加上 nft 失敗的三種情況，ruleset 都不變 |
| agent 仍以非 root 執行，沒有新增 capability；元件的權限只有寫明的那些（對 unit 的檢查） | 通過：`tests/systemd_unit.rs` 的逐行比對與評分，systemd 容器裡 instance 實際的 UID、capability、`NoNewPrivs`、seccomp 與唯讀檔案系統 |
| 介面、權限邊界與威脅模型寫進文件 | 通過：`edge/agent/README.md`「fieldbus 放行集合」 |
| `cargo test -p agent` 在有 podman 的建置主機上通過，容器測試沒有略過；測試報告放在 `docs/test-reports/` | 通過：`CI=1` 下 `allowlist` 的兩個測試都執行（evidence 行），本報告 |

待測（進階驗證，延後）：

- 在閘道器上安裝並執行（README「手動安裝」）：映像安裝在 Laysi/trustiot-v2#866，與預設拒絕（Laysi/trustiot-v2#865）一起驗證。
- x86_64 建置主機上重跑容器測試（第二個平台）。
- 故障注入：在元件寫暫存檔或 nft 執行到一半時結束它，確認集合不變（設計上 nft 只在檔案寫完之後才啟動，見 README）。
- `PollLimitBurst` 下持續連線時 journal 的實際量。
