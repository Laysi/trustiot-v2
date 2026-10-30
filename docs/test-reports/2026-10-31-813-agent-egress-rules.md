# agent 帳號的出口規則：擋本機、廣播、多播與 metadata 位址（2026-10-31）

對應 issue：Laysi/trustiot-v2#813（父 issue Laysi/trustiot-v2#96）。這份報告記錄 nftables 規則（`edge/agent/systemd/trustiot-agent-egress.nft`）、在 agent 之前載入它的 unit（`trustiot-agent-egress.service`）與閘道器檢查（`edge/agent/scripts/check-gateway.sh`）的驗證，以及對 lab 的閘道器的唯讀檢查。

結果先講：在 Debian 13 容器（與閘道器相同的 nftables 1.1.3 與 systemd 257）裡，agent 帳號連 18 種該拒絕的目的地都被規則拒絕，規則的 counter 也記到；`nobody` 與 root 連同樣的目的地全部連得上，counter 不變。agent 經網路的 DNS 伺服器解析雲端名稱並連上 443。systemd 跑兩個 unit 時，規則在 agent 的行程之前載入；`nftables.service` 的 start、reload、restart 不會讓規則在 agent 執行中消失；規則或 `/etc/nftables.conf` 載入不了、`nftables.service` 的 drop-in 不見時 agent 不啟動。檢查腳本在 lab 的閘道器上唯讀執行，找出 sshd、avahi-daemon、cloudflared 聽在所有介面。

## 環境

- **測試**：建置主機（x86_64 Linux），`cargo test -p agent --test egress --test systemd_unit`，分支 `feat/813-agent-egress-rules`。
  - 容器：`edge/agent/tests/egress/Containerfile`，`debian:trixie-slim`（以 digest 釘選）加上 Debian 的 `nftables`、`iproute2`、`socat`、`dnsmasq-base`、`systemd`。nftables 1.1.3、systemd 257，和 lab 的閘道器相同。
  - 規則只載入容器自己的 network namespace（`--network none`），沒有動到建置主機的規則。網路的測試在容器裡開 IPv6 forwarding（`--sysctl`，只作用在容器自己的 namespace），讓核心加入 subnet-router anycast 位址。
- **lab 的閘道器**（展示用閘道器）：Raspberry Pi OS Lite arm64（Debian 13.7），kernel `6.18.50+rpt-rpi-v8`，nftables 1.1.3。只做唯讀檢查：沒有寫任何檔案、沒有安裝、沒有載入規則、沒有重啟任何服務。主機名稱與位址不寫進報告。

## 指令

```sh
# 建置主機，repo 根目錄
cargo test --locked -p agent --test egress --test systemd_unit -- --nocapture
cargo test --locked -p agent

# lab 的閘道器：唯讀檢查（腳本經 ssh 的標準輸入傳入，閘道器上不留檔案）
: "${GATEWAY_HOST:?set GATEWAY_HOST to the ssh host of the lab gateway}"
ssh "$GATEWAY_HOST" 'sudo -n sh -s' < edge/agent/scripts/check-gateway.sh
ssh "$GATEWAY_HOST" 'cat /etc/resolv.conf; systemctl is-active systemd-resolved NetworkManager; sudo -n /usr/sbin/nft list ruleset; systemctl is-enabled nftables'
```

`tests/egress.rs` 的網路（`tests/egress/lab.sh network`）：容器自己的 namespace 是閘道器，`veth0` 是 `10.81.3.2/24`、`fd81:3::2/64`；另一個 namespace `peer` 是現場的 LAN、DNS 伺服器與雲端（`10.81.3.1`、`fd81:3::1`，`lo` 上另有 `169.254.169.254` 與 `fd00:ec2::254`），預設路由經 `peer`。閘道器上另有 `ip route add local 198.51.100.0/24 dev lo`，以及一條 NAT 的 output 規則把 `203.0.113.80:80` 轉回 `127.0.0.1:8080`。閘道器在 8080 有聽所有位址的服務（socat），`peer` 在 80、443 與 DNS（dnsmasq，`cloud.test`）上有服務，閘道器的 `127.0.0.53` 另有一個 DNS stub。三個帳號：`trustiot-agent`（以 repo 的 sysusers 檔建立）、`nobody`、root。

## 結果

### 拒絕的目的地（`the_agents_account_is_refused_the_gateway_broadcast_multicast_and_metadata`）

| 目的地 | 協定 | `nobody`、root | `trustiot-agent` | 計數的規則 |
| --- | --- | --- | --- | --- |
| `127.0.0.1:8080` | TCP | ok | refused | local |
| `127.0.0.2:8080` | TCP | ok | refused | local |
| `[::1]:8080` | TCP | ok | refused | local |
| `0.0.0.0:8080`（核心改連 loopback） | TCP | ok | refused | local |
| `10.81.3.2:8080`（閘道器自己的 LAN 位址） | TCP | ok | refused | local |
| `[fd81:3::2]:8080`（閘道器自己的 IPv6 位址） | TCP | ok | refused | local |
| `[fe80::81:2%veth0]:8080`（閘道器自己的 link-local 位址） | TCP | ok | refused | local |
| `[fe80::81:2%veth0]:9`（同上） | UDP | ok | not-permitted | local |
| `[fd81:3::]:9`（LAN 前綴的 subnet-router anycast，閘道器開了 IPv6 forwarding） | UDP | ok | not-permitted | anycast |
| `198.51.100.7:8080`（`ip route add local`） | TCP | ok | refused | local |
| `203.0.113.80:80`（NAT 轉回 `127.0.0.1:8080`） | TCP | ok | refused | local |
| `127.0.0.1:9` | UDP | ok | not-permitted | local |
| `10.81.3.255:9`（網段廣播） | UDP | ok | not-permitted | broadcast |
| `255.255.255.255:9` | UDP | ok | not-permitted | broadcast |
| `239.1.2.3:9` | UDP | ok | not-permitted | multicast |
| `[ff05::1:3]:9` | UDP | ok | not-permitted | multicast |
| `169.254.169.254:80`（在 peer 上真的有服務） | TCP | ok | refused | metadata |
| `[fd00:ec2::254]:80`（同上） | TCP | ok | refused | metadata |
| `10.81.3.1:80`、`[fd81:3::1]:80`、`10.81.3.1:9`（LAN 上的設備） | TCP、UDP | ok | ok | 無 |

- refused 是 TCP reset（`Connection refused`，立即），not-permitted 是送出時的 `EPERM`。
- `nobody` 與 root 跑完全部目的地之後，每條規則的 counter 都是 0；`trustiot-agent` 的每一次嘗試都讓表中那條規則的 counter 增加。
- 帳號還不存在時 `nft -f` 失敗（錯誤訊息指出 `trustiot-agent`），table 沒有建立；建立帳號後載入兩次，仍只有一個 table、6 條拒絕規則。

### 名稱解析與雲端（`the_agent_resolves_and_reaches_the_cloud_with_the_rules_in_force`）

- `/etc/resolv.conf` 是 `nameserver 10.81.3.1`（網路的 DNS 伺服器，和閘道器上 NetworkManager 寫的一樣）：`trustiot-agent` 以 glibc（`getent ahostsv4`）解析 `cloud.test` 得到 `10.81.3.1`，連 `cloud.test:443` 成功，規則的 counter 都是 0。
- 改成 `nameserver 127.0.0.53`（本機 stub）：root 照樣解析得到，`trustiot-agent` 解析失敗；`check-gateway.sh` 結束碼 1，印出 `FAIL resolver: /etc/resolv.conf names the nameserver 127.0.0.53, a local address (127.0.0.53), ...`。
- `nameserver 127.0.53`（glibc 讀成 127.0.0.53）：root 解析得到，`trustiot-agent` 解析失敗。
- 檢查腳本對以下每一種 `/etc/resolv.conf` 都是結束碼 1，並指出那個 nameserver 與原因：`127.0.53`、`0:0:0:0:0:0:0:1`、`::ffff:7f00:35`（都換成標準寫法後是 loopback）、`10.81.3.2`（閘道器自己的 LAN 位址）、`10.81.3.255`（廣播）、`224.0.0.251`（多播）、`169.254.169.254`（metadata）、`0.0.0.0`（未指定位址）、`10.81.3.1` 加上 `127.0.0.53`（其中一個不行）、沒有任何 nameserver、只有 glibc 不理的 nameserver（`dns.example.test`、`10.81.3.1.7`、空的；glibc 這時問 127.0.0.1，root 也解析不到）。`10.81.3.1` 加上 `fd81:3::1`（網路上的 DNS 伺服器）、`dns.example.test` 加上 `10.81.3.1` 是 0，agent 解析得到。

### 閘道器檢查（`the_gateway_check_flags_listeners_off_loopback_and_missing_rules`）

- 規則未載入時：結束碼 1，`FAIL listeners: tcp 0.0.0.0:8080 (socat)`、`FAIL listeners: tcp [::]:8080 (socat)`、`FAIL egress: table inet trustiot_agent is not loaded`；聽在 `127.0.0.53` 的 DNS stub 沒有被列出；`ok resolver`。
- 載入規則後：`--allow tcp:8080:sshd`（程式不符）仍是 1；`--allow tcp:8080:socat` 是 0，印出 `ALLOWED listeners: tcp 0.0.0.0:8080 (socat)`。
- 載入規則後 egress 是 `ok egress: table inet trustiot_agent is loaded and is the agent's rules`。以下每種情況都從規則檔重新載入再改，結束碼都是 1，`FAIL egress: table inet trustiot_agent is not the agent's rules: ...` 指出第一行不同的地方：刪掉 multicast 那條規則；刪掉 output chain 的 `meta skuid ... jump agent`（拒絕的規則全部還在，只是沒有被跳到）；在 jump 之前插入 `meta skuid "trustiot-agent" accept`；載入之後把帳號的 uid 改成 4242（規則仍比對舊的 uid）。重新載入規則檔之後回到 0。`--allow icmp:1` 是 2（參數錯誤）。

### systemd（`the_rules_are_in_force_before_the_agent_starts_and_a_failed_load_keeps_it_stopped`）

容器以 systemd 為 PID 1，裝上 repo 的兩個 unit、`nftables.service` 的 drop-in（`nftables-reload.conf`）與 sysusers 檔；`nftables.service` 是 Debian 套件原本的。agent 換成替身（drop-in 只改 `Type=exec` 與 `WatchdogSec=0`，依賴與沙箱都是真的 unit 的）。替身一啟動就以自己的帳號連 `127.0.0.1:8080`，把結果記在狀態目錄。

- `systemctl start trustiot-agent.service`：`nftables.service`、規則 unit、agent 都 active；三者的時間依序是 `nftables.service` 結束、規則載入完成（`ExecMainExitTimestampMonotonic`）、agent 的行程啟動（`ExecMainStartTimestampMonotonic`）；替身記下 `refused`（root 連同一個服務成功）。
- `nftables.service`（Debian 原本的 `/etc/nftables.conf`，開頭是 `flush ruleset`）：已經 active，再 `start` 之後 table 還在；`reload`（drop-in 讓 nft 以 `include` 在一個 transaction 載入 `/etc/nftables.conf` 與規則檔）之後 table 還在、agent 仍 active。先加一個標記用的 table，再把規則檔換成載入不了的內容、或刪掉規則檔，各 `reload` 一次：reload 都失敗，規則的 table 與標記的 table 都還在（`/etc/nftables.conf` 的 `flush ruleset` 也沒有套用），agent 仍 active；放回規則檔。兩個檔案都在時 `reload` 成功，標記的 table 被 flush 掉（`/etc/nftables.conf` 確實有套用），規則的 table 在，agent 仍 active。`restart` 之後規則 unit 與 agent 都換了新的 `InvocationID`（真的重啟了，不是原本那次還在跑），agent 的行程在規則重新載入之後才啟動，table 還在，兩者都 active；`stop` 之後規則 unit 與 agent 都是 inactive。兩次啟動，替身每次都記下 `refused`。
- 載入失敗的三種原因，`systemctl start trustiot-agent.service` 都失敗：`/etc/nftables.conf` 寫壞時 `nftables.service` 是 `failed`；規則檔移走時規則 unit 是 `failed`；`nftables.service` 的 drop-in 移走時規則 unit 是 `failed`（`ExecStartPre=` 拒絕）。三種情況 agent 都是 `inactive`、`ExecMainPID=0`，沒有任何排隊中的工作，替身沒有執行過（沒有紀錄檔），也沒有 table。把檔案放回、`reset-failed` 之後再 start 就成功。

### 突變檢查

把規則檔的 `fib daddr type local` 那一行刪掉、同時把 agent unit 的 `Requires=trustiot-agent-egress.service` 刪掉，四個測試全部失敗：三個網路的測試失敗在 local 的拒絕上（例如 agent 經 127.0.0.53 的 stub 解析成功、規則只剩 4 條），systemd 測試在第一個 start 之後失敗，因為規則 unit 沒有被帶起來（inactive，這是 `Requires=` 的刪除造成的）。檔案還原後再跑全部通過。這次突變在加上 `Requires=nftables.service` 與 `nftables.service` 的 drop-in 之前的版本上做。

把 anycast 那條規則的種類改成不會命中的 `unreachable`（規則數不變）：agent 送往 `[fd81:3::]:9` 的 datagram 成功送出（`ok`，應為 `not-permitted`），第一個測試失敗。沒有這條規則時，開了 IPv6 forwarding 的閘道器上 agent 連得到本機的 UDP 服務。

最終版本（這個分支的兩個 commit）在建置主機上：`cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo test --locked --workspace --exclude agent`、`cargo test --locked -p agent` 都通過，原始輸出見 [tests.txt](data/2026-10-31-813-agent-egress-rules/tests.txt)。

### lab 的閘道器（唯讀）

原始輸出：[gateway-inspection.txt](data/2026-10-31-813-agent-egress-rules/gateway-inspection.txt)、[gateway-check.txt](data/2026-10-31-813-agent-egress-rules/gateway-check.txt)。

- **名稱解析的路徑**：`/etc/resolv.conf` 由 NetworkManager（active）寫入，nameserver 不是 loopback 位址；`systemd-resolved`、dnsmasq、unbound 都是 inactive。`/etc/nsswitch.conf` 是 `files mdns4_minimal [NOTFOUND=return] dns`，`mdns4_minimal` 經 avahi 的 unix socket。所以規則不需要為 DNS 放行任何本機目的地。
- **nftables**：套件已安裝（1.1.3），`nftables.service` 是 `disabled`，目前沒有任何規則；`/etc/nftables.conf` 是 Debian 的預設（`flush ruleset` 開頭，接著一個 chain 全部放行的 `inet filter` table）。新的規則 unit `Requires=nftables.service`，所以裝上之後 agent 啟動時會先帶起它，載入這個預設檔，行為不變。`nf_tables` 已載入，`nft_fib`、`nft_reject` 等模組在 kernel 的模組目錄裡（載入規則時由核心自動載入；這一點在閘道器上的實際載入由映像 Laysi/trustiot-v2#809 驗證）。
- **檢查腳本**（以這個分支最終版的腳本執行）：結束碼 1。sshd 聽 `0.0.0.0:22` 與 `[::]:22`，avahi-daemon 聽 `0.0.0.0:5353`、`*:5353` 與兩個動態埠，cloudflared（遠端存取的通道）有四個綁在 `*` 的 UDP socket（QUIC 用戶端的未連線 socket）；出口規則還沒有安裝（`FAIL egress`）；`ok resolver`。cloudflared 的 TCP listener 在 loopback，沒有被列出。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 以 agent 的 uid 連本機位址（含閘道器自己網卡上的位址）、廣播、多播與 `169.254.169.254`、`fd00:ec2::254`，都被 nftables 拒絕；其他帳號不受影響（測試） | 通過：見「拒絕的目的地」 |
| 規則生效時 agent 照常解析雲端主機名稱並連上雲端（測試，或在 namespace 內以 stand-in 驗證） | 通過，以 stand-in：見「名稱解析與雲端」；閘道器上的解析路徑不經本機 stub（唯讀檢查） |
| 規則在 agent 啟動前生效；載入失敗時的行為寫明並測過（測試） | 通過：見「systemd」；行為寫在 `edge/agent/README.md`「systemd」的「出口規則」 |
| 「本機服務只聽 loopback 或 unix socket」的檢查可以執行，README 寫明怎麼跑 | 通過：見「閘道器檢查」與 lab 的閘道器上的執行；用法在 `edge/agent/README.md`「連線位址的限制」 |
| `edge/agent/README.md`「連線位址的限制」寫明核心這一層 | 通過 |

閘道器上目前違反不變量的 sshd、avahi-daemon 與 cloudflared，以及在閘道器上實際安裝並載入規則，屬於映像（Laysi/trustiot-v2#809）與登入面強化（Laysi/trustiot-v2#815）。
