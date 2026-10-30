# 展示用閘道器安裝 agent、裝回憑證鏈並啟動服務（2026-10-30）

對應 issue：Laysi/trustiot-v2#792（父 issue Laysi/trustiot-v2#458）。這份報告接續 [Laysi/trustiot-v2#618 的登錄報告](2026-10-30-618-lab-registration.md)。這次在 lab 的閘道器裝上 agent 與 sudoers 規則，重跑同一個 `register` 把憑證鏈與根憑證裝回去，再啟用服務並做重開機實測。本 issue 不綁定租戶，也不建立設備，所以 agent 不會去輪詢 lab 的 RS485 轉換器。

結果先講：agent 裝好了，`--check` 通過，服務會在開機時自動啟動。連線則停在 WebSocket 升級：展示環境還沒有 MQTT broker，gateway 主機名稱的入口沒有 `/mqtt` 路由，所以回 404（見「結果」）。2026-10-30 決定把連線的證據（CONNACK 0x00，以及未綁定時 `online` 收到 PUBACK 0x87）移到 Laysi/trustiot-v2#795（展示環境：啟用 MQTT broker、ingest 與 gateway-ops，並開放 `/mqtt` 路由）。「gateway-ops 看得到在線」則移到 Laysi/trustiot-v2#793。

## 環境

- **agent 的建置**：在建置主機（x86_64 Linux）上，從 main `32a5cd92a4c55e9ef3610505d5c9d260325d7ba1` 建置。
  - zig 0.16.0、cargo-zigbuild 0.23.4，都是 `.prototools` 釘選的版本。
  - 不帶任何 feature。
  - SHA-256 是 `4c3154cd4d17fc3799c663511cdaad3b4dc61b0531ee498ef5a6185aec15ed91`，和 Laysi/trustiot-v2#618 從 `6c97de88` 建出的檔案逐位元組相同（兩個 commit 之間只改了文件）。
- **閘道器**：lab 的閘道器，也就是展示用閘道器。
  - 系統：Raspberry Pi OS Lite arm64（Debian 13 trixie），kernel `6.18.50+rpt-rpi-v8`，systemd 257。
  - TPM：`/dev/tpmrm0` 屬於群組 `tss`，持久 handle 是 `0x81000001` 與 `0x81000100`，沿用 `0x81000100`。
  - 硬體 ID 是 `tg-lab-0001`。
  - 遠端存取用的 `cloudflared-ssh.service` 全程沒有動過。
- **工作站**：協調者用 Laysi/trustiot-v2#618 的 `trustiot-workstation` 與工作站憑證執行。
  - 平台管理員的 email 驗證碼由負責人在自己的終端機輸入。
  - 這次不帶 `--upload-agent`：provision 的檢查與安裝都由閘道器上已裝好的 agent 執行，所以都經過 sudoers 規則。
- **平台**：展示環境，經 Cloudflare Access（每個指令都帶 `--headers-file`）。
  - 叢集上是 chart `0.0.0-rc.11`。
  - 啟用的服務是 identity、assets、certs，沒有 MQTT broker、ingest、gateway-ops。
- **時段**：2026-10-30 10:04–10:20 UTC。

## 指令

主機名稱、ssh 的 Host 與帳號都不寫進報告。`<GW>` 是工作站 `~/.ssh/config` 裡這台閘道器的 Host；`<gateway 主機名稱>` 是展示環境的 gateway 主機名稱；`<ssh 帳號>` 是工作站登入閘道器用的帳號。

```sh
# 1. 建置（建置主機，repo 根目錄）
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
sha256sum target/aarch64-unknown-linux-gnu/release/trustiot-agent

# 2. 安裝（照 edge/agent/README.md「systemd」；檔案先 scp 到閘道器上 ssh 帳號的暫存目錄）
: "${GATEWAY_HOST:?set GATEWAY_HOST to the ssh host of the lab gateway}"
scp trustiot-agent trustiot-agent.service trustiot-agent.sysusers agent.toml 080_trustiot-provision "$GATEWAY_HOST:trustiot-792/"
# 以下在閘道器上（cd ~/trustiot-792）
sudo visudo -cf ./080_trustiot-provision
sudo install -o root -g root -m 0755 trustiot-agent /usr/bin/trustiot-agent
sudo install -o root -g root -m 0644 trustiot-agent.service /usr/lib/systemd/system/trustiot-agent.service
sudo install -o root -g root -m 0644 trustiot-agent.sysusers /usr/lib/sysusers.d/trustiot-agent.conf
sudo systemd-sysusers /usr/lib/sysusers.d/trustiot-agent.conf
sudo install -d -o root -g root -m 0755 /etc/trustiot
sudo install -o root -g root -m 0644 agent.toml /etc/trustiot/agent.toml
sudo install -d -o trustiot-agent -g trustiot-agent -m 0700 /var/lib/trustiot-agent
sudo install -o root -g root -m 0440 080_trustiot-provision /etc/sudoers.d/080_trustiot-provision
sudo visudo -c && sudo systemctl daemon-reload

# 3. 登錄前的檢查（閘道器上）
sudo trustiot-agent --check                                    # 預期 1：還沒有憑證
sudo -n trustiot-agent provision --hardware-id tg-lab-0001 --handle 0x81000100 --inspect
sudo -n trustiot-agent provision install --hardware-id tg-lab-0001 </dev/null   # 預期 8：前提都過，憑證被拒
sudo -n -l trustiot-agent provision install --hardware-id tg-lab-0001
sudo cvtsudoers -f json /etc/sudoers.d/080_trustiot-provision
systemd-analyze verify /usr/lib/systemd/system/trustiot-agent.service

# 4. 重跑登錄（工作站，協調者；H 與 ROOT 同 Laysi/trustiot-v2#618 的報告）
trustiot-workstation login --url "https://<app 主機名稱>" --email <平台管理員的 email> --headers-file "$H"
trustiot-workstation register --hardware-id tg-lab-0001 --gateway <GW> --root-sha256 "$ROOT" --remote-sudo --headers-file "$H"
trustiot-workstation verify --hardware-id tg-lab-0001 --headers-file "$H"
trustiot-workstation logout --headers-file "$H"

# 5. 檢查、啟用與重開機（閘道器上）
sudo trustiot-agent --check
sudo systemctl enable --now trustiot-agent.service
systemctl status trustiot-agent; sudo journalctl -u trustiot-agent -o short-iso-precise
cat /proc/sys/kernel/random/boot_id; sudo systemctl reboot
# 重開機後
cat /proc/sys/kernel/random/boot_id; systemctl is-enabled trustiot-agent; systemctl is-active trustiot-agent
systemctl show trustiot-agent -p ActiveEnterTimestamp -p NRestarts
sudo journalctl --utc -b 0 -u trustiot-agent -o short-iso-precise
```

設定檔 `/etc/trustiot/agent.toml` 只有必填的鍵：`cloud.url` 是 `wss://<gateway 主機名稱>/mqtt`，`cloud.trust` 是作業系統的 CA bundle，其餘照 `edge/agent/agent.example.toml`。sudoers 規則只列三個參數寫死的指令：`provision ... --inspect`、`provision`（同樣的參數）與 `provision install --hardware-id tg-lab-0001`。兩個檔案的內容（主機名稱與帳號已遮蔽）在 `install.txt`。

## 結果

原始輸出在 [data/2026-10-30-792-demo-gateway-agent/](data/2026-10-30-792-demo-gateway-agent/)，主機名稱、帳號與本機路徑已遮蔽。

| 步驟 | 結果 | 檔案 |
| --- | --- | --- |
| 建置 | `Finished release`，只有一個 linker 警告（`ignoring deprecated linker optimization setting '1'`）；aarch64 PIE、動態連結 glibc；SHA-256 `4c3154cd…ed91` | `build.txt` |
| 安裝前 | 所有目標路徑都不存在，沒有 `trustiot-agent` 帳號，所以不用備份 | `install.txt` |
| 安裝 | 帳號 `trustiot-agent`（UID/GID 985）；`/etc/trustiot` root 0755、設定檔 root 0644、`/var/lib/trustiot-agent` 屬於 `trustiot-agent`、0700；sudoers 0440，`visudo -c` 全部 `parsed OK`；`systemd-analyze verify` 0；服務先不啟用 | `install.txt` |
| 登錄前的檢查 | `--check` 是 1（沒有 `gateway.pem`，符合預期）。`--inspect` 是 0：金鑰在、`created=false`，公鑰 SHA-256 `2914ee8b…84e2` 與 Laysi/trustiot-v2#618 相同。空的 stdin 送給 `provision install` 是 8（`standard input holds no certificate; nothing was written`），表示設定檔、目錄與帳號的前提都通過（不通過會是 9）。`sudo -n -l` 是 0 | `install.txt` |
| 重跑登錄 | 2026-10-30T10:12:31Z–10:12:35Z。平台 **200**，同一個序號 `5756b62458c56803416c03789c6c22ae`，`key_created=false`。閘道器回 `gateway.pem: written`、`broker-ca.pem: written`，`register` 結束碼 0。`verify` 仍然只有一筆 `certs.gateway_registered`（event `01a18c2f-…`），而且和紀錄相符 | `register.txt` |
| 裝上的檔案 | `gateway.pem` 屬於 `trustiot-agent`、0644，CN `tg-lab-0001`，序號相同，到期 2027-01-28T09:41:47Z，由 `TrustIoT Demo Gateway Issuing CA G1 2026-10-28` 簽發。`broker-ca.pem` 屬於 root、0644，是 `TrustIoT Demo Gateway Root CA 1`，SHA-256 指紋等於 `--root-sha256` | `check-and-start.txt` |
| `sudo trustiot-agent --check` | 結束碼 **0**：`ok: gateway tg-lab-0001, certificate valid until 2027-01-28T09:41:47Z`，另有一行 INFO `no root CRL yet` | `check-and-start.txt` |
| 啟用並啟動 | 10:13:12Z `enable --now`：`enabled`、`active (running)`，10:13:13.97Z 送出 `READY=1` | `check-and-start.txt` |
| 第一次連線 | 10:13:14.52Z：`connecting failed at WebSocket upgrade: the upgrade was answered with HTTP 404 Not Found`。之後以 full jitter 退避（0.9、1.0、0.5、3.6、10.9、21.0、39.2 秒……上限 60 秒），一直重試 | `check-and-start.txt` |
| 404 的來源 | 工作站送出同樣的升級（10:14:40Z），得到 404、空的 body、`Server-Timing: cfOrigin;dur=108`，是 origin 回的。展示叢集上 gateway 主機名稱的 HTTPRoute 只有 `Exact /certs/renew`，沒有 MQTT broker、ingest 與 gateway-ops（chart `0.0.0-rc.11`，`mqttBroker.enabled` 是預設的 `false`） | `check-and-start.txt` |
| 重開機 | 10:15:39Z 重開機，boot ID 由 `b870ddc3…` 變成 `56586149…`。agent 在 10:15:48Z 自動啟動（`enabled`，`NRestarts=0`）。剛開機時網路還沒好，前幾次停在解析主機名稱，之後回到同樣的 404。`cloudflared-ssh` 是 active，持久 handle 不變 | `reboot.txt`、`journal-after-reboot.txt` |
| Cloudflare 安全事件 | 協調者以 GraphQL Analytics API 查詢：`firewallEventsAdaptive` 在 gateway 主機名稱、10:13:00Z–10:20:00Z 是 0 筆，整個 zone 在 10:00:00Z–10:40:00Z 也是 0 筆。`httpRequestsAdaptiveGroups` 在同一個主機名稱與時段是 17 個請求，全部是 `GET /mqtt`，edge 狀態 404（origin 回的），`securityAction` 是 `unknown`（沒有套用任何安全動作）。其中 1 個是工作站的 curl 探測，16 個是 agent 的連線嘗試 | `cloudflare.txt` |

補充：

- **sudoers 規則在這台閘道器上不是安全邊界。** 映像預設的 ssh 帳號本來就能免密碼 sudo 任何指令（`090_<ssh 帳號>-nopasswd`），所以這條規則的作用只是讓 `register --remote-sudo` 走 `factory/workstation/README.md` 要求的固定指令形式。把工作站的存取限縮成只能跑 `provision` 子命令（專用帳號與金鑰、轉接程式），是出廠映像階段的 Laysi/trustiot-v2#219 與 Laysi/trustiot-v2#95。展示用閘道器只留在 lab。
- **重開機前的 agent 日誌**：閘道器沒有持久的 journal，所以重開機前的日誌是當時讀出的輸出，時間是閘道器的時區（+08:00）；重開機後的日誌以 UTC 讀出。
- **服務維持啟用。** agent 每次重試只做外層 TLS 與 WebSocket 升級，不用 TPM；Laysi/trustiot-v2#795 開出 `/mqtt` 路由與 broker 之後，它會自己連上。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| aarch64 release 建置的指令與 SHA-256 記在 issue，沒有 feature | 通過：見「指令」第 1 步與 `build.txt`，SHA-256 `4c3154cd4d17fc3799c663511cdaad3b4dc61b0531ee498ef5a6185aec15ed91` |
| 重跑 `register` 回 200 與同一個序號，憑證鏈與根憑證已裝上 | 通過：平台 200，序號 `5756b62458c56803416c03789c6c22ae`，兩行 `written`；`verify` 只有一筆事件 |
| `sudo trustiot-agent --check` 結束碼 0，閘道器 ID 是 `tg-lab-0001` | 通過 |
| service 啟用並執行，重開機後自動啟動；gateway-ops 看得到 `tg-lab-0001` 在線 | 啟用、執行與重開機後自動啟動：通過。連線（CONNACK 0x00 與未綁定時的 PUBACK 0x87）待測：展示環境還沒有 MQTT broker，2026-10-30 移到 Laysi/trustiot-v2#795。gateway-ops 看得到在線也待測：閘道器沒有綁定時 broker 不給角色，gateway-ops 不記錄它，2026-10-30 移到 Laysi/trustiot-v2#793 |
| 第一次連線的時段在 Cloudflare 沒有被 challenge 或擋下 | 通過，涵蓋到 Cloudflare 這一段：10:13:00Z–10:20:00Z 沒有任何安全事件，agent 的 16 個請求都穿過 Cloudflare 到了 origin，沒有套用安全動作，所以 Bot Fight Mode 與 Browser Integrity Check 沒有攔下 rustls 的 TLS 特徵。完整的 TLS 與 MQTT 路徑在 Laysi/trustiot-v2#795 再確認一次 |
| 測試報告存進 `docs/test-reports/`，沒有主機名稱、內網位址、帳號 | 本報告的 PR 合併時通過（gitleaks 與私有名稱掃描） |
