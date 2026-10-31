# agent 的沙箱擋住 D-Bus、avahi 與本機 sshd 的 unix socket（2026-10-31）

對應 issue：Laysi/trustiot-v2#826（來自 Laysi/trustiot-v2#813 的安全審查）。這份報告記錄 agent unit（`edge/agent/systemd/trustiot-agent.service`）新增的 `InaccessiblePaths=-/run/dbus -/run/avahi-daemon -/run/ssh-unix-local` 的驗證，以及對 lab 的閘道器 unix socket 的唯讀盤點。

結果先講：在以 systemd 為 PID 1 的 Debian 13 容器裡，以真正的 agent unit 執行的替身連 `/run/dbus/system_bus_socket`、`/var/run/dbus/system_bus_socket`、`/run/avahi-daemon/socket` 與 `/run/ssh-unix-local/socket` 全部得到 `Permission denied`；同一個帳號在沙箱外全部連得上；以 drop-in 清空這個設定時替身也全部連得上。替身照常送出 `READY=1` 與 `WATCHDOG=1`。三個目錄都不存在時 agent 照樣啟動。把這一行從 unit 拿掉，測試失敗（突變檢查）。lab 的閘道器上沒有任何 abstract namespace 的 listener。本機 sshd 的 unix socket 是這張 issue 的安全審查找到的：它讓 agent 能從沙箱外的 sshd session 再連 D-Bus 與 avahi，所以一併藏住。

## 環境

- **測試**：建置主機兩種：x86_64 Linux 與 arm64 Linux VM，分支 `fix/826-agent-sandbox-unix-sockets`。
  - 容器：`edge/agent/tests/egress/Containerfile`，`debian:trixie-slim`（以 digest 釘選）加上 `nftables`、`iproute2`、`socat`、`dnsmasq-base`、`systemd`，這張 issue 再加上 `dbus`、`avahi-daemon`、`libnss-mdns`。`libnss-mdns` 讓 `/etc/nsswitch.conf` 的 hosts 一行與閘道器相同（`files mdns4_minimal [NOTFOUND=return] dns`）。
  - 本機 sshd 的 socket 以替身 socket unit 代替（與閘道器同路徑 `/run/ssh-unix-local/socket`、同權限 0666、`Accept=yes`），容器裡沒有裝 sshd：測試只需要一個 agent 帳號在沙箱外連得上的 socket。
  - 替身 agent 是 perl 腳本（perl-base），以 unit 的 `ExecStart=` 路徑執行，是 unit 的 main process：啟動時連四個路徑並記下結果，送 `READY=1`，之後每秒送 `WATCHDOG=1`，state 目錄出現 `probe-now` 時再連一次。
- **lab 的閘道器**（展示用閘道器）：Raspberry Pi OS Lite arm64（Debian 13），kernel `6.18.50+rpt-rpi-v8`，systemd 257.13、dbus-daemon 1.16.2、avahi-daemon 0.8-16、libnss-mdns 0.15.1。只做唯讀檢查：沒有寫任何檔案、沒有安裝、沒有重啟任何服務。主機名稱與位址不寫進報告。

## 指令

```sh
# 建置主機，repo 根目錄
CI=1 cargo test --locked -p agent --test egress --test systemd_unit -- --nocapture
cargo test --locked -p agent
cargo test --locked --workspace --exclude agent

# 突變檢查：拿掉這一行再跑（跑完還原）
sed -i '/^InaccessiblePaths=/d' edge/agent/systemd/trustiot-agent.service
CI=1 cargo test --locked -p agent --test egress the_agents_sandbox

# lab 的閘道器：唯讀檢查
: "${GATEWAY_HOST:?set GATEWAY_HOST to the ssh host of the lab gateway}"
ssh "$GATEWAY_HOST" 'sudo -n ss -xlH; sudo -n ss -xapH; ls -l /run/dbus /run/avahi-daemon /run/ssh-unix-local /run/systemd; systemctl list-sockets --all; systemctl cat sshd-unix-local.socket sshd-unix-local@.service; sudo -n sshd -T; getent passwd trustiot-agent; grep ^hosts: /etc/nsswitch.conf'
```

實際收集的輸出經過整理（遮掉 pid、fd、sd-bus 的隨機名稱前綴與使用者的 `/run/user/<uid>/` 底下的項目），見 [gateway-unix-sockets.txt](data/2026-10-31-826-agent-sandbox-unix-sockets/gateway-unix-sockets.txt)。

## 結果

### 沙箱擋住的 socket（`the_agents_sandbox_hides_dbus_avahi_and_the_local_sshd_and_keeps_sd_notify`）

| 情況 | 四個路徑 | 說明 |
| --- | --- | --- |
| agent 帳號在沙箱外（`setpriv`） | reached | socket 都在，帳號的權限連得上 |
| 替身在 unit 的沙箱裡 | refused Permission denied | `InaccessiblePaths` 的不可存取目錄 |
| `avahi-daemon.socket` 停掉再啟動後，執行中的替身 | refused Permission denied | 目錄留著，遮蔽仍在；沙箱外仍連得上 |
| drop-in 清空 `InaccessiblePaths=` 後重啟 | reached | 拒絕是這個設定造成的 |
| drop-in 移除後重啟 | refused Permission denied | |
| 停掉 dbus、avahi 與 sshd 的 socket，刪掉三個目錄後啟動 | refused No such file or directory | `-` 前綴：agent 照樣啟動（active） |

sd_notify：unit 是 `Type=notify`、`NotifyAccess=main`、`WatchdogSec=30s`（啟動後 `WatchdogUSec=30s`）。`systemctl start` 成功（收到 `READY=1` 才會返回成功）；`WatchdogTimestampMonotonic` 在啟動後往前走（收到 `WATCHDOG=1`）；`NRestarts=0`。

### 名稱解析（`the_agent_resolves_dns_names_with_avahi_out_of_reach`）

容器的 hosts 一行是 `files mdns4_minimal [NOTFOUND=return] dns`，avahi 的 socket 不存在（agent 帳號連它得到 `No such file or directory`），agent 帳號解析 `cloud.test` 得到網路 DNS 伺服器的回答 `10.81.3.1`。`mdns4_minimal` 不為 `.local` 以外的名稱找 avahi，所以這證明 `dns` 前面那一步會放行這些名稱，不是 avahi 連線失敗時的行為；也不在沙箱裡執行（systemd 的容器裡沒有 agent 帳號連得到的 DNS 伺服器）。

### 突變檢查

把 `InaccessiblePaths=` 一行從 unit 拿掉（其他不變），執行 `CI=1 cargo test --locked --no-fail-fast -p agent --test egress --test systemd_unit`：沙箱的測試失敗，替身在沙箱裡連得上每一個路徑；`systemd_unit` 的 `the_sandbox_lines_stay` 也失敗；其他測試通過。跑完還原 unit。最終版本在 x86_64 的建置主機上跑；只藏 D-Bus 與 avahi 的前一版也在 arm64 的建置主機上跑過一次，結果相同（三個路徑都 reached）。

```
test the_agents_sandbox_hides_dbus_avahi_and_the_local_sshd_and_keeps_sd_notify ... FAILED
assertion `left == right` failed: /run/avahi-daemon/socket in the sandbox: {"/run/avahi-daemon/socket": "reached", "/run/dbus/system_bus_socket": "reached", "/run/ssh-unix-local/socket": "reached", "/var/run/dbus/system_bus_socket": "reached"}
  left: "reached"
 right: "refused Permission denied"
test result: FAILED. 5 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.16s
test the_sandbox_lines_stay ... FAILED
assertion `left == right` failed: InaccessiblePaths= should appear once: []
test result: FAILED. 8 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.11s
```

測試本身每次也以清空設定的 drop-in 做同樣的反證（上表），所以 CI 上只要 systemd 容器跑得起來，就會證明拒絕來自這個設定。

### lab 的閘道器的 unix socket

- **所有人可連的 path socket**：`/run/dbus/system_bus_socket`、`/run/avahi-daemon/socket`、`/run/ssh-unix-local/socket`（這次都藏住）；sd_notify 與 journal 的 `/run/systemd/notify`、`/run/systemd/journal/{socket,stdout,dev-log}`（agent 要用）；systemd 的 varlink：`io.systemd.Credentials`、`io.systemd.Hostname`、`io.systemd.ManagedOOM`、`userdb/io.systemd.DynamicUser`（不代送網路封包）。只有 root 能連的：`/run/systemd/private`、`/run/udev/control`、`io.systemd.sysext`、`journal/io.systemd.journal`。`/var/run` 是指向 `/run` 的連結。
- **本機 sshd**：`sshd-unix-local.socket` 由 systemd 的 ssh generator 產生，每條連線一個 `sshd -i`。`sshd -T`：`usepam yes`、`pubkeyauthentication yes`、`authorizedkeysfile .ssh/authorized_keys .ssh/authorized_keys2`、`allowstreamlocalforwarding yes`，沒有 `AllowUsers`、`DenyUsers`。`trustiot-agent` 帳號已經在 lab 的閘道器上，home 是 `/var/lib/trustiot-agent`、shell 是 `/usr/sbin/nologin`。
- **abstract namespace**：以 `@` 開頭的 unix socket 只有五個，都是 `ESTAB` 的 D-Bus 用戶端連線（systemd 三個、systemd-logind、systemd-timesyncd），沒有任何 listener（`ss -xlH` 裡沒有 `@` 開頭的項目）。
- `/etc/nsswitch.conf` 的 hosts：`files mdns4_minimal [NOTFOUND=return] dns`。

### 檢查

以 `CI=1` 執行（容器的測試不能略過）：

- 建置主機（x86_64 Linux），最終的程式碼（commit `54a0dd09`）：`cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo test --locked -p agent`（`egress` 6 個、`systemd_unit` 9 個測試都通過，systemd 的測試有執行、沒有略過；其他測試檔全部通過）、`cargo test --locked --workspace --exclude agent`（含 `repo-check`，全部通過）。
- 建置主機（arm64 Linux VM），同一份程式碼：`cargo test --locked -p agent --test egress --test systemd_unit` 通過，systemd 的測試有執行；前一版（只藏 D-Bus 與 avahi）另跑過 `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings` 與 `cargo test --locked -p repo-check`，都通過。
- `systemd-analyze security --offline` 的評分仍是 1.5（`systemd_unit` 的 `the_unit_scores_at_most_the_allowed_exposure` 也檢查不高於 2.0）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 以 agent unit 的沙箱執行的行程連不上 `/run/dbus/system_bus_socket` 與 `/run/avahi-daemon/socket`（測試）；拿掉新設定時測試失敗（突變檢查） | 通過：見「沙箱擋住的 socket」與「突變檢查」 |
| 同樣的沙箱下 sd_notify 照常（`READY=1`、`WATCHDOG=1`） | 通過：同一個測試，真正的 `Type=notify` unit 與 watchdog |
| 閘道器上 abstract namespace 的 unix socket、agent 能否濫用，結論寫進 `edge/agent/README.md` | 通過：沒有 listener，目前無從濫用；寫在 README「這一層擋不到的」 |
| `cargo test -p agent --test systemd_unit` 通過（含 `systemd-analyze verify`） | 通過：見「檢查」 |
| README「這一層擋不到的」裡 unix socket 一項改寫成修正後的實際狀態 | 通過 |

待測：在閘道器上以 unit 的沙箱實際執行 agent（Laysi/trustiot-v2#505）；那時要確認這三個目錄在 agent 啟動時都已存在、agent 照常解析雲端名稱。

仍然擋不到的：agent 可以在自己的 home（`/var/lib/trustiot-agent`）放公鑰，再經外部繞回閘道器 sshd 的 TCP 22（hairpin NAT、LAN 上的轉送主機、對外開放的 IPv6）登入，在沙箱外連 D-Bus 與 avahi。sshd 拒絕這個帳號，或帳號的 home 不是 agent 可寫的目錄，是這個修正在那種現場成立的必要條件。閘道器映像（Laysi/trustiot-v2#825）的 `AllowUsers trustiot-admin` 已經拒絕這個帳號；lab 的閘道器還是官方映像，沒有這一行。之後的 sshd 設定（登入面強化，Laysi/trustiot-v2#815）要保留這個拒絕（`edge/agent/README.md`「這一層擋不到的」）。
