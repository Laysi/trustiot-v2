# agent 帳號的預設拒絕（2026-10-31）

對應 issue：Laysi/trustiot-v2#879（父 issue Laysi/trustiot-v2#814）。這份報告記錄預設拒絕的規則檔（`edge/agent/systemd/trustiot-agent-default-deny.nft`）、填入 DNS 放行的 `trustiot-agent-nameservers`（`edge/agent/src/bin/trustiot-agent-nameservers.rs`，讀法在 `edge/agent/src/nameservers.rs`）與它的 path unit、service 的驗證。設計的選擇（雲端只限 port、DNS 由誰更新、規則檔的寫法、給 Laysi/trustiot-v2#865 與 Laysi/trustiot-v2#866 的約定）在 `edge/agent/README.md`「連線位址的限制」的「預設拒絕」。

結果先講：在 Debian 13 容器（與閘道器相同的 nftables 1.1.3 與 systemd 257）裡，把預設拒絕照 Laysi/trustiot-v2#866 的接法 `include` 進出口規則檔之後，agent 帳號只連得到放行集合裡的 TCP 目的地、任何位址的 TCP 443，以及 `/etc/resolv.conf` 裡 nameserver 的 UDP 與 TCP 53。11 種其他出口（清單外的位址、清單內位址的其他 port、清單內目的地的 UDP、UDP 443、其他 UDP、NTP、不是 nameserver 的 DNS 伺服器的 UDP、TCP 與 IPv6）都被拒並由預設拒絕的 counter 計數，經 nameserver 來源路由到別的主機的 UDP 53 也被拒；同樣的目的地在沒有預設拒絕時連得上，`nobody` 照樣連得上，Laysi/trustiot-v2#813 的 counter 不變。nameserver 集合跟著 resolv.conf 改變（就地寫入與 rename 蓋過都由 path unit 套用）。重新載入出口規則時集合保留、chain 換新；`nftables.service` 的 reload、restart 與 `nft flush ruleset` 之後，fieldbus 集合是空的，nameserver 由 drop-in 或 agent 啟動前的 `ExecStartPre=` 填回。映像不安裝這些檔案，映像的出口規則不變。安全審查沒有 Critical、High 或 Medium，三個 Low 都已處理（見「安全審查」）。

## 環境

- **建置主機**：x86_64 Linux（rootless podman），分支 `feat/879-default-deny-rules`，已合併 main `7d2b86b2`（Laysi/trustiot-v2#876）；Rust 1.99.0（`rust-toolchain.toml`）。最後一次執行的程式碼與加入這份報告的 commit 相同（報告與原始輸出之外沒有差別）。
- **容器**：`edge/agent/tests/egress/Containerfile`（Laysi/trustiot-v2#813 的同一個映像），`debian:trixie-slim`（以 digest 釘選）加上 Debian 的 `nftables`、`iproute2`、`socat`、`dnsmasq-base`、`systemd`。這次建置的 `trustiot-agent-allowlist` 與 `trustiot-agent-nameservers` 以 `podman cp` 放進容器的 `/usr/libexec/trustiot-agent/`。
- **網路**：`tests/egress/lab.sh network`：容器自己的 namespace 是閘道器（`10.81.3.2`、`fd81:3::2`），另一個 namespace `peer` 是現場的 LAN 與雲端（`10.81.3.1`、`fd81:3::1`，80 與 443 有服務，`10.81.3.1` 上有 DNS 伺服器）。測試在 peer 另加 TCP 502 的 fieldbus 替身（IPv4 與 IPv6）與第二個 DNS 伺服器（`10.81.3.53`、`fd81:3::53`）。容器的 `/etc/resolv.conf` 若是 podman 掛上的檔案，先卸下，成為容器自己的檔案（閘道器上 NetworkManager 以 rename 取代它）。
- 規則只載入容器自己的 network namespace，沒有動到建置主機的規則。
- 沒有在閘道器上執行：映像在 Laysi/trustiot-v2#866 之前不安裝預設拒絕（見「結論」的待測）。

## 指令

```sh
# 建置主機，repo 根目錄
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked -p agent --test default_deny --no-fail-fast -- --nocapture
CI=1 cargo test --locked -p agent --no-fail-fast
cargo test --locked -p repo-check
CI=1 cargo test --locked --workspace --exclude agent --no-fail-fast
bun install --cwd edge/image --frozen-lockfile && bun run --cwd edge/image typecheck && bun run --cwd edge/image test
```

`CI=1` 讓容器測試在沒有 podman 時失敗而不是略過。

## 結果

原始輸出（evidence 行與測試結果）：[data/2026-10-31-879-agent-default-deny/default-deny.txt](data/2026-10-31-879-agent-default-deny/default-deny.txt)。三個測試都通過，沒有略過。

### 規則檔的形狀（`the_file_declares_the_sets_the_executables_fill_and_replaces_only_its_chains`）

規則檔開頭的宣告正好是 `Allowlist::declarations()` 加上 `Nameservers::declarations()`，接著三條 chain 的 `add` 與 `delete`；指令裡沒有 `delete table`、`flush ruleset`、`flush set`、`flush table`。

### 放行與拒絕（`the_agents_account_reaches_only_the_allowlist_the_cloud_and_its_nameservers`）

- **對照**：只載入 Laysi/trustiot-v2#813 的規則時，下表 11 個目的地 agent 帳號都連得上，也解析得到名稱。所以下面的拒絕都來自預設拒絕。
- **沒有預設拒絕的 table 時**：`trustiot-agent-allowlist` 回結束碼 1；`trustiot-agent-nameservers` 結束碼 1，日誌是 `did not apply the nameservers 10.81.3.1; the sets are unchanged: nft exit status: 1: ... No such file or directory ...`；兩者都沒有建立 table。
- **載入**：出口規則檔加上 `include "/usr/share/trustiot-agent/trustiot-agent-default-deny.nft"`，連續載入兩次：只有一個 `inet trustiot_agent_allow`，4 個集合、3 條 chain（`nft list table` 的內容在原始輸出），`inet trustiot_agent` 也在。
- **集合空著時**：agent 連 `10.81.3.1:502` 被拒、名稱解析失敗、`10.81.3.1:443` 連得上（雲端不受清單影響）。
- **填入之後**：元件回 `ok 2`（`10.81.3.1 502`、`fd81:3::1 502`），nameserver 執行檔印出 `the agent may reach the nameservers 10.81.3.1 on port 53`。下列都連得上，而且預設拒絕的 counter 沒有增加：

| 放行 | 協定與目的地 |
| --- | --- |
| 清單裡的 fieldbus 目的地 | TCP `10.81.3.1:502`、`[fd81:3::1]:502` |
| 雲端的 port | TCP `10.81.3.1:443`、`[fd81:3::1]:443`、`10.81.3.53:443`（任何位址） |
| 以名稱連雲端 | TCP `cloud.test:443`，經 nameserver 解析 |
| DNS | UDP 與 TCP `10.81.3.1:53` |

- **拒絕**：每一筆 agent 帳號被拒（TCP 是 reset，其他是送出時 `EPERM`）、預設拒絕的 counter 增加、`nobody` 連得上：

| 拒絕 | 協定與目的地 | agent |
| --- | --- | --- |
| 清單外的位址的 fieldbus port | TCP `10.81.3.53:502` | refused |
| 清單內位址的其他 port | TCP `10.81.3.1:80` | refused |
| 清單內 IPv6 位址的其他 port | TCP `[fd81:3::1]:80` | refused |
| 清單內目的地的 UDP | UDP `10.81.3.1:502` | not-permitted |
| 清單內 IPv6 目的地的 UDP | UDP `[fd81:3::1]:502` | not-permitted |
| 雲端 port 的 UDP | UDP `10.81.3.1:443` | not-permitted |
| DNS 以外的 UDP | UDP `10.81.3.1:9` | not-permitted |
| NTP | UDP `10.81.3.1:123` | not-permitted |
| 不是 nameserver 的 DNS 伺服器（UDP） | UDP `10.81.3.53:53` | not-permitted |
| 不是 nameserver 的 DNS 伺服器（TCP） | TCP `10.81.3.53:53` | refused |
| 不是 nameserver 的 DNS 伺服器（IPv6） | UDP `[fd81:3::53]:53` | not-permitted |

  整段期間 Laysi/trustiot-v2#813 的 counter 不變。
- **來源路由**：agent 帳號直接送 `192.0.2.99:53` 被拒。改以 IPv4 的 LSRR 選項經 nameserver `10.81.3.1` 繞過去（header 的目的地是 nameserver 的 53 埠，DNS 的放行本來會接受）：建置主機的核心不讓沒有 `CAP_NET_RAW` 的 `nobody` 設定這個選項（`setsockopt` 回 `Operation not permitted`）；探測的行程帶 `CAP_NET_RAW`（模擬允許這樣做的核心）時，`nobody` 送得出，agent 帳號在送出時被拒，由 `IP options` 那條規則計數。IPv6 的 routing header 沒有實測：容器裡沒有能設定它的工具（見「結論」）。
- **nameserver 改變**：resolv.conf 改成 `10.81.3.53` 之後、執行檔執行之前名稱解析失敗；執行之後解析成功，`10.81.3.1` 的 UDP 與 TCP 53 被拒、`10.81.3.53` 的 TCP 53 連得上。改成 `fd81:3::53%veth0`、`192.0.2.1`、`192.0.2.2`、`10.81.3.1` 四行：集合是前三個（zone 去掉），`10.81.3.1` 的 UDP 53 仍被拒，名稱經 IPv6 的 nameserver 解析成功。三個 IPv4 換成另外三個（集合的大小正好是 3，在同一個 transaction 裡清空再填入）：集合正好是新的三個。檔案不見：兩個集合清空，日誌 `the agent may reach the nameservers none on port 53`。
- **重新載入出口規則**：先在 chain 裡多加一條規則，再 `nft -f` 規則檔：多的那條不見、chain 與原本相同，四個集合的元素不變，fieldbus 與 DNS 照樣連得上。
- **以 `/etc/nftables.conf` 的方式載入**（`nft 'include "/etc/nftables.conf"; include "<規則檔>"'`，`nftables.service` 的 reload）：四個集合都清空，chain 相同，fieldbus 被拒、名稱解析失敗；下一份清單元件仍回 `ok 2`（agent 從回答看不出集合被清過，見 README 給 Laysi/trustiot-v2#865 的約定）。
- **`nft flush ruleset`**：元件回 `failed: nft exit status: 1`，nameserver 執行檔結束碼 1；再載入規則檔之後 table 回來、集合是空的。

### systemd 為 PID 1（`the_dns_allowance_follows_resolv_conf_and_every_load_that_empties_it`）

照 README「給 Laysi/trustiot-v2#866 的接法」安裝：出口規則檔加上 `include`、agent 的 unit 以 drop-in 加上 `Wants=`、`After=trustiot-agent-nameservers.path` 與 `ExecStartPre=+/usr/libexec/trustiot-agent/nameservers`、`nftables.service` 的 drop-in 加上第二行 `ExecReload=`；agent 換成 `sleep` 的替身，探測以 `setpriv` 換成 agent 帳號。

| 步驟 | 結果 |
| --- | --- |
| `systemctl start trustiot-agent.service` | `nftables.service`、出口規則的 unit、path unit、agent 都 active；nameserver 集合是 `10.81.3.1`，agent 解析得到名稱 |
| 就地寫入 resolv.conf（`10.81.3.53`） | path unit 啟動 service（新的 InvocationID，Result `success`），集合跟著變；`10.81.3.1` 的 UDP 53 被拒 |
| 以 rename 蓋過 resolv.conf（`fd81:3::53`） | IPv6 集合是 `fd81:3::53`、IPv4 集合清空；`10.81.3.53` 的 UDP 53 被拒 |
| service 的 journal | 三行 `the agent may reach the nameservers ... on port 53`，依序是三次改變 |
| `systemctl reload trustiot-agent-egress.service` | 四個集合不變，fieldbus 照樣連得上 |
| `systemctl reload nftables.service` | agent 沒有重啟（InvocationID 相同）；fieldbus 集合清空、被拒，nameserver 由 drop-in 填回、解析成功；下一份清單 `ok 1` 之後 fieldbus 連得上 |
| `nft flush ruleset`，再 `kill -KILL` 替身 | 清掉時元件回 `failed:`；自動重啟（NRestarts 增加）先重新載入規則、填回 nameserver；fieldbus 集合是空的、被拒，下一份清單 `ok 1` |
| `systemctl restart nftables.service` | agent 跟著重啟，啟動前填回 nameserver，解析成功 |

### 其他檢查

- `systemd_unit`：nameserver service 的每一行設定（`DynamicUser`、只有 `CAP_NET_ADMIN`、只開 `AF_NETLINK`、`IPAddressDeny=any`、唯讀的檔案系統等，共 34 個鍵）、`StartLimitIntervalSec=0`、沒有 `[Install]`；path unit 只看 `/etc/resolv.conf`；`systemd-analyze verify` 兩個 unit 都沒有其他抱怨；`systemd-analyze security --offline` 的評分是 1.1（上限 1.5，與 allowlist 的 instance 相同）。
- `secret_inputs`：nameserver 執行檔只啟動 nft、只以名稱開 `/etc/resolv.conf`（nft 腳本在 `memfd_create` 的匿名檔案裡）、不讀環境變數、帶參數時以結束碼 2 拒絕。systemd 的那個測試在 service 自己的沙箱（seccomp 的 `@system-service`）裡跑過這個執行檔。
- 解析的單元測試（`cargo test -p agent --lib nameservers`）：glibc 的讀法（`inet_aton` 的寫法如 `127.0.53`、`0x7f.1`、`012.0.0.1`、`167772161`，IPv6 的 zone，IPv4-mapped，`nameserver` 後面接 tab 與註解，最多 3 個；名稱、超出範圍或多一段的位址、行尾的 CR 與 NUL 之後的內容都不算）與產生的 nft 腳本只提到兩個集合。
- `edge/image`：型別檢查與測試通過；`AGENT_SYSTEMD_NOT_INSTALLED` 多了預設拒絕與兩個 unit，映像安裝的出口規則檔（`trustiot-agent-egress.nft`）沒有改變。

## 安全審查

以 security reviewer 審查合併前的 commit（`c4c17874`）：沒有 Critical、High 或 Medium。三個 Low 的處理：

1. **來源路由讓 DNS 的放行送到別的主機**：規則加上拒絕帶 IP option 或 IPv6 extension header 的封包（`ip hdrlength != 5`、`ip6 nexthdr != { tcp, udp }`），加上上面的 LSRR 探測。實測顯示建置主機的核心本來就要求 `CAP_NET_RAW`，規則是不依賴核心的另一層。
2. **nft 腳本用 `/tmp` 的 `O_TMPFILE`**：在 agent 的 `ExecStartPre=+` 與 `nftables.service` 下用的是主機的 `/tmp`，唯讀或不支援時會讓 agent 無法啟動。改用 `memfd_create`。
3. **之後的版本在同一個名稱下改集合定義會讓每次載入失敗**：寫進規則檔檔頭與 README（換名稱、刪舊的）。

審查也指出幾個測試缺口：集合滿額換滿額已補測；ICMP 與其他協定（SCTP、UDP-Lite）沒有探測，規則上落到同一條拒絕；path unit 預設的觸發上限（2 秒 200 次）寫進 unit 的註解；systemd 的那個測試在 CI 上略過（沿用 `tests/egress.rs` 的慣例），接法的證據來自建置主機的這次執行。

## 結論

- 驗收條件的核心行為（放行、拒絕、DNS 跟著 resolv.conf、哪些載入保留或清空集合）在容器裡實測通過，systemd 底下的接法也測過。
- 待測（延後，見 issue）：在閘道器上實測要等映像安裝（Laysi/trustiot-v2#866）與 agent 填入清單（Laysi/trustiot-v2#865）；`scripts/check-gateway.sh` 加上預設拒絕的檢查也在那時。IPv6 routing header、ICMP（ping socket）與其他協定的探測，以及閘道器的核心是否要求 `CAP_NET_RAW` 才能設定來源路由，屬於進階驗證。
