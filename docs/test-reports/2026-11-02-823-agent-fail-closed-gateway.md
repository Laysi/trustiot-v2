# agent 沒有有效身分時不運作、不收密碼：在 lab 的閘道器以實體 TPM 與 systemd 重跑

Laysi/trustiot-v2#823（重跑 Laysi/trustiot-v2#812 在建置主機上的證據，[2026-10-31 的報告](2026-10-31-812-agent-fail-closed.md)）

lab 的閘道器兼作展示用閘道器，執行中的 `trustiot-agent.service` 正在累積第一版 lab 資料集的資料（Laysi/trustiot-v2#680），所以這次不停、不改它：五種情況與設了密碼的 TPM 金鑰，都由同一台閘道器上另裝的一份測試用 unit 啟動，用的是測試用的設定與身分檔；命令列與環境變數的檢查，唯讀地在執行中的 agent 上做。做完移除測試用的一切，再對照前後的紀錄。

## 環境

- lab 的閘道器：aarch64，Raspberry Pi OS（pi-gen reference 2026-10-06，Debian 13.7 trixie），systemd 257（257.13-1~deb13u1），實體 TPM（`/dev/tpmrm0`），tpm2-tools 5.7，OpenSSL 3.5.7。
- 已部署的 agent：`/usr/bin/trustiot-agent`，`--version` 印 `trustiot-agent 0.1.0`，SHA-256 `4c3154cd4d17fc3799c663511cdaad3b4dc61b0531ee498ef5a6185aec15ed91`。這個雜湊與 [2026-10-30 的部署報告](2026-10-30-792-demo-gateway-agent.md)記的相同，即 commit `32a5cd92` 建出的檔案（建置紀錄在該報告的 `build.txt`）。
  - 部署版本與 main（`2e882e56`）在 fail-closed 路徑上相同。`32a5cd92` 是 main 的祖先；拒絕啟動的檢查都在 `src/setup.rs` 的 `Setup::new`：依序讀憑證與私鑰（TPM 金鑰要開得起來、測試簽章要過、要與憑證的公鑰相同）、外層信任、`broker.ca`、根 CRL、用戶端憑證檢查。之後動到 `src/main.rs`、`src/setup.rs` 的 commit 只改了根 CRL 的載入與更新（Laysi/trustiot-v2#839、Laysi/trustiot-v2#896）和心跳（Laysi/trustiot-v2#971）。PR Laysi/trustiot-v2#822 只加了測試與文件，沒有改 `src/`。所以這五種情況在部署的執行檔上都有意義，不需要換執行檔。
- 已部署的 unit（`/usr/lib/systemd/system/trustiot-agent.service`）比 main 的 `edge/agent/systemd/trustiot-agent.service` 舊，完整差異在 [`unit-main-vs-deployed.diff`](data/2026-11-02-823-agent-fail-closed-gateway/unit-main-vs-deployed.diff)：
  - 沒有出口規則的部分：`Requires=`、`After=trustiot-agent-egress.service`、`Wants=trustiot-agent-egress-check.timer` 與 `ExecStartPre=+/usr/sbin/nft ...`；
  - 沒有 `InaccessiblePaths=`，也沒有 `IPAddressDeny=`；
  - 其餘差異只在註解。

  閘道器上也沒有任何 nftables 表（`nft list tables` 是空的，前後一樣）。所以已部署的 unit 本來就沒有會動到共用狀態的行，測試用 unit 不必刪掉這類行。
- 執行中的 agent：PID 6372，`ActiveEnterTimestamp=Sat 2026-10-31 01:18:16 CST`，`NRestarts=0`。設定檔的金鑰在 TPM 的 `0x81000100`，屬性是 `fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign`（沒有 noDA）。
- TPM 起始狀態：persistent handle 只有 `0x81000001`（SRK，資料分割區的 LUKS 解鎖用）與 `0x81000100`（閘道器的金鑰）。`ownerAuthSet`、`endorsementAuthSet`、`lockoutAuthSet` 都是 0，`TPM2_PT_LOCKOUT_COUNTER` 是 0x0，`TPM2_PT_MAX_AUTH_FAIL` 是 0x20。
- 開始前確認閘道器上沒有別人在做事：
  - `who` 只有這次的連線；
  - 沒有執行中的 transient unit，只有一個更早留下、已經 failed 的 `run-p5373-i5374.service`（不是這次建立的，沒有動它）；
  - 沒有 `tpm2_*` 行程。
- 時間（+08:00，2026-11-02）：
  - 13:57:28 做之前的紀錄；
  - 13:57:58 準備；
  - 13:58:15–13:59:20 跑七次啟動；
  - 隨後清除；
  - 13:59:37 做之後的紀錄；
  - 14:01:20 做 IP 計量的對照。
- broker 端：展示叢集的 `trustiot` namespace 的 `trustiot-mqtt-broker-0`（Mosquitto），以 ssh 到展示機器、執行 `sudo k3s kubectl get` 與 `logs` 唯讀讀取。沒有 exec、port-forward 或任何寫入，也沒有拿 lab lock。

### 測試用 unit 與已部署 unit 的差異

測試用 unit 是 `/run/systemd/system/trustiot-agent-823@.service`，一個 template，每種情況一個 instance（`trustiot-agent-823@<情況>.service`），這樣每種情況各有自己的 `systemctl show` 與 journal。它由 `systemctl cat trustiot-agent` 的輸出以 `sed` 改出來（`prepare.sh`），實際差異（[`prepare.txt`](data/2026-11-02-823-agent-fail-closed-gateway/prepare.txt) 的「diff deployed unit -> test unit」）只有：

| 行 | 已部署 | 測試用 | 原因 |
| --- | --- | --- | --- |
| 檔案位置 | `/usr/lib/systemd/system/trustiot-agent.service` | `/run/systemd/system/trustiot-agent-823@.service` | 在 tmpfs，重開機也不留下 |
| `Description=` | `TrustIoT gateway agent` | 加上 `fail-closed test copy %i (Laysi/trustiot-v2#823)` | 在 journal 裡辨認 |
| `ExecStart=` | `--config /etc/trustiot/agent.toml` | `--config /etc/trustiot-823/%i.toml` | 每種情況自己的設定檔 |
| `Restart=` | `always` | `no` | 見下方 |
| `ConfigurationDirectory=` | `trustiot` | `trustiot-823` | 自己的設定目錄 |
| `StateDirectory=` | `trustiot-agent` | `trustiot-agent-823` | 自己的狀態目錄，systemd 以 `STATE_DIRECTORY` 傳給 agent |
| `IPAccounting=yes` | 沒有 | 加上 | 以 cgroup 的 BPF 計數，算 unit 收送的每個 IPv4／IPv6 封包 |
| `[Install]` | `WantedBy=multi-user.target` | 刪掉 | 測試用 unit 不 enable |

其餘每一行都相同，包括 `Type=notify`、`NotifyAccess=main`、`User=trustiot-agent`、`Group=trustiot-agent`、`SupplementaryGroups=tss`、`StateDirectoryMode=0700`、`UMask=0077`、`Environment=MALLOC_MMAP_THRESHOLD_=65536`、`WatchdogSec=30s`、`TimeoutStartSec=30s`、`StartLimitIntervalSec=0`，以及所有沙箱行（`DeviceAllow=/dev/tpmrm0 rw`、`ProtectSystem=strict`、`SystemCallFilter=` 等）。

**`Restart=no` 的原因**：issue 要每種情況啟動一次。設了密碼的金鑰每啟動一次就有一次 TPM 授權失敗；它是 noDA，不計入鎖定計數，但次數還是愈少愈好。`Restart=always` 會在一分鐘內一直重試，同樣的失敗只會重複。`Restart=always` 在拒絕啟動時的重試行為已在建置主機上鎖住（`tests/systemd_unit.rs`），不是這次要驗的。

### 測試用的設定與身分檔

- `/etc/trustiot-823/<情況>.toml`：由 `/etc/trustiot/agent.toml` 以 `sed` 複製，只改以下各行（`prepare.txt` 有逐檔的 diff）：
  - `broker.ca` 指向複本 `/etc/trustiot-823/broker-ca.pem`；
  - `broker.crl` 指向 `/var/lib/trustiot-agent-823/root-crl.pem`。這個檔案不存在，部署的 `broker.crl` 也不存在；
  - `identity.certificate` 與 `identity.key` 依情況而定。

  `cloud.url` 與 `cloud.trust` 不變。
- `/var/lib/trustiot-agent-823/desired-config.cbor`：執行中 agent 的 `desired-config.cbor` 的複本，與原檔逐位元組相同（`cmp`）。
- 真的憑證鏈（leaf 加簽發 CA，公開資料）複製到 `real/` 與 `no-key-file/`。
- 「`broker.ca` 沒簽的憑證」用一個拋棄式的 CA 簽發：一個根，加一個由它簽的簽發 CA，都在 `/run/trustiot-823/ca`（tmpfs，root 0700），做完就刪。被簽的 leaf：
  - 公鑰取自真的憑證，也就是 TPM 裡真的金鑰的公鑰；
  - 主體、key usage（critical, digitalSignature）、EKU（clientAuth）、basic constraints（critical, CA:FALSE）都與真的憑證相同；
  - 檔案是 leaf 加拋棄式簽發 CA，與真的憑證鏈同樣的結構；
  - `openssl verify` 以拋棄式的根驗得過，以 `broker.ca` 驗不過。

  所以唯一的錯誤是簽發者。`prepare.txt` 裡那段兩行的 diff 只是 `openssl -ext` 列印擴充欄位的順序不同（basic constraints 在前或在後），內容相同。
- TPM 測試金鑰：以 owner hierarchy（沒有密碼）建立一個 transient primary（`ecc256:null:aes128cfb`）當 parent，不用 SRK，SRK 屬於 LUKS 解鎖。在它下面以 `tests/tpm_swtpm.rs` 的同一組屬性建了兩把金鑰，放到確認過是空的 persistent handle，之後 primary 就丟掉。
  - `0x81000823`：`fixedtpm|fixedparent|sensitivedataorigin|userwithauth|noda|sign`，ECC P-256、ECDSA-SHA256，空密碼。「別張憑證的私鑰」用它。
  - `0x81000824`：同樣的屬性，加上一個隨機密碼。密碼是 `od -An -N16 -tx1 /dev/urandom` 的輸出，經 pipe 直接進 `tpm2_create -p file:-` 的標準輸入：不在變數、檔案或命令列裡，沒有人知道它。
  - `0x81000825`：確認是空的，從來沒有建立過。「沒有私鑰」用它。

## 指令

腳本都在 data 目錄，以 root 在閘道器上執行；`<gateway>` 是閘道器的 ssh 連線，`<demo-host>` 是展示機器的 ssh 連線。

```sh
D=docs/test-reports/data/2026-11-02-823-agent-fail-closed-gateway
# 之前的紀錄（唯讀）
ssh <gateway> 'sudo bash -s' < $D/inventory.sh > inventory-before.txt
# 執行中 agent 的命令列與環境變數（唯讀）
ssh <gateway> 'pid=$(systemctl show -p MainPID --value trustiot-agent); sudo cat /proc/$pid/cmdline | tr "\0" "\n"; sudo cat /proc/$pid/environ | tr "\0" "\n"; sudo grep -E "^(Uid|Gid|Groups|CapInh|CapPrm|CapEff|CapBnd|CapAmb|NoNewPrivs|Seccomp):" /proc/$pid/status'
# 拋棄式 CA、TPM 測試金鑰、設定、狀態目錄與測試用 unit（任何一項已存在或 handle 被占用就拒絕）
ssh <gateway> 'sudo bash -s' < $D/prepare.sh > prepare.txt
# 每種情況啟動一次；結束碼 3 表示 TPM 鎖定計數或執行中的 agent 變了，要立刻停
for c in no-config no-certificate no-key no-key-file other-ca other-key password-key; do
  ssh <gateway> "sudo bash -s -- $c" < $D/case.sh > case-$c.txt || break
done
# 清除：停掉並刪掉測試用 unit、刪掉測試的檔案、只 evict 兩把測試金鑰（要有 noda 才 evict）
ssh <gateway> 'sudo bash -s' < $D/cleanup.sh > cleanup.txt
# 之後的紀錄
ssh <gateway> 'sudo bash -s' < $D/inventory.sh > inventory-after.txt
# 執行中 agent 在這段時間的 journal
ssh <gateway> 'sudo journalctl -u trustiot-agent --since "2026-11-02 13:50:00" --no-pager -o short-iso-precise'
# broker 端（唯讀），略去 readiness probe 與 certs 服務每 5 分鐘的 listClients
ssh <demo-host> 'sudo k3s kubectl -n trustiot logs trustiot-mqtt-broker-0 -c mosquitto --since-time=2026-11-02T05:50:00Z --timestamps' \
  | grep -vE 'probe\.sock|probe:[0-9]+|listClients'
# IP 計量的對照：一個 transient unit 連一次 loopback 的 port 1
ssh <gateway> 'sudo systemd-run --wait --collect --quiet --unit=trustiot-823-ipaccounting-control -p IPAccounting=yes -p DynamicUser=yes /bin/bash -c "exec 3<>/dev/tcp/127.0.0.1/1"; sudo journalctl -u trustiot-823-ipaccounting-control -o cat'
```

## 結果

原始資料在 [`data/2026-11-02-823-agent-fail-closed-gateway/`](data/2026-11-02-823-agent-fail-closed-gateway/)。下列項目都換成了占位符：

- 位址（`127.0.0.1` 保留）寫成 `<ip>`；
- 閘道器 ID 寫成 `<gateway-id>`，主機名稱寫成 `<gateway-hostname>`，叢集節點寫成 `<cluster-node>`，登入帳號寫成 `<login>`；
- systemd 的 invocation ID 與 MQTT session ID 寫成 `<id-N>`，同一個值用同一個編號；
- 憑證、`broker-ca.pem`、`agent.toml`、`desired-config.cbor` 的 SHA-256 寫成 `<sha256:檔名>`，同一個雜湊用同一個占位符，所以前後是否相同仍看得出來。

執行檔、已部署 unit 與 sysusers 的雜湊照原樣寫。

### 七次啟動

每次啟動的結果都一樣：

- `systemctl start` 的結束碼是 1（`Job ... failed because the control process exited with error code`）。
- `systemctl show` 的值是 `ActiveState=failed`、`SubState=failed`、`Result=exit-code`、`ExecMainCode=1`（exited）、`ExecMainStatus=1`、`NRestarts=0`，`ActiveEnterTimestampMonotonic=0`，也就是 unit 從來沒有進入 active：systemd 沒收到 `READY=1`。`StatusText=` 是空的：也沒收到任何 `STATUS=`。
- `IPIngressPackets=0`、`IPEgressPackets=0`、`IPIngressBytes=0`、`IPEgressBytes=0`：unit 沒有收送任何 IP 封包，連 DNS 都沒有。所以它沒有連 broker 的前端，也沒有連現場的轉換器。
- journal 沒有 `polling with the saved configuration`、`applied the configuration`、`polled`（計數都是 0）。
- 狀態目錄沒有出現 `buffer/`：沒有開離線緩衝。
- `desired-config.cbor` 的 SHA-256 前後相同（`<sha256:desired-config.cbor>`，也與執行中 agent 的那份相同）。
- TPM 的鎖定計數前後都是 0x0；執行中 agent 的 `MainPID`、`NRestarts`、`ActiveEnterTimestamp` 前後相同（`6372 0 Sat 2026-10-31 01:18:16 CST`）。

journal 的拒絕原因（`ERROR trustiot_agent: refusing to start: ...`，略去時間與行程）：

| 情況 | 設定 | journal 的原因 | 紀錄 |
| --- | --- | --- | --- |
| 沒有設定檔 | `/etc/trustiot-823/no-config.toml` 不存在 | `reading /etc/trustiot-823/no-config.toml: No such file or directory (os error 2)` | [`case-no-config.txt`](data/2026-11-02-823-agent-fail-closed-gateway/case-no-config.txt) |
| 沒有憑證 | 憑證路徑指向空目錄，金鑰是真的 `0x81000100` | `identity.certificate / identity.key: reading the certificate file /var/lib/trustiot-agent-823/no-certificate/gateway.pem: I/O error: No such file or directory (os error 2)` | [`case-no-certificate.txt`](data/2026-11-02-823-agent-fail-closed-gateway/case-no-certificate.txt) |
| 沒有私鑰（TPM，正式的設定方式） | 真的憑證，金鑰指向空的 handle `0x81000825` | `identity.certificate / identity.key: the TPM key: the TPM refused the command: TPM_RC_HANDLE (0x0000018b, handle 1)` | [`case-no-key.txt`](data/2026-11-02-823-agent-fail-closed-gateway/case-no-key.txt) |
| 沒有私鑰（檔案金鑰，補充；與建置主機的寫法相同） | 真的憑證，`key = { source = "file", path = ... }` 指向不存在的檔案 | `identity.certificate / identity.key: reading the private key file /var/lib/trustiot-agent-823/no-key-file/gateway.key: I/O error: No such file or directory (os error 2)` | [`case-no-key-file.txt`](data/2026-11-02-823-agent-fail-closed-gateway/case-no-key-file.txt) |
| `broker.ca` 沒簽的憑證 | 拋棄式 CA 簽的憑證（公鑰是真的金鑰的），金鑰是真的 `0x81000100` | `broker.ca: the gateway certificate does not pass as a client certificate of /etc/trustiot-823/broker-ca.pem: invalid peer certificate: UnknownIssuer` | [`case-other-ca.txt`](data/2026-11-02-823-agent-fail-closed-gateway/case-other-ca.txt) |
| 別張憑證的私鑰 | 真的憑證，金鑰是測試金鑰 `0x81000823`（空密碼、noDA） | `identity.certificate / identity.key: the private key does not belong to the certificate` | [`case-other-key.txt`](data/2026-11-02-823-agent-fail-closed-gateway/case-other-key.txt) |
| 設了密碼的 TPM 金鑰 | 真的憑證，金鑰是測試金鑰 `0x81000824`（隨機密碼、noDA） | `identity.certificate / identity.key: the TPM key: the TPM refused the command: TPM_RC_BAD_AUTH (0x000009a2, session 1)` | [`case-password-key.txt`](data/2026-11-02-823-agent-fail-closed-gateway/case-password-key.txt) |

每次啟動從 systemd 的 `Starting` 到行程結束約 100–200 ms。

- 「`broker.ca` 沒簽的憑證」一案用了真的金鑰：agent 開啟它並以空密碼做一次測試簽章，與執行中的 agent 啟動時做的相同，鎖定計數不變。
- 設了密碼的金鑰回 `TPM_RC_BAD_AUTH`，不是 `TPM_RC_AUTH_FAIL`，與 swtpm 上的結果（`tests/tpm_swtpm.rs`）相同：noDA 金鑰的授權失敗不計入字典攻擊計數。鎖定計數在這次前後都是 0x0。

### 確認「沒有連線」不是看不到

- 測試用 unit 的行程在 `systemctl start` 回來時已經結束，事後的 `ss` 自然是空的（紀錄裡的 `(none)`），所以沒有連線的證據是 unit 的 IP 計量。
- 對照（[`ipaccounting-control.txt`](data/2026-11-02-823-agent-fail-closed-gateway/ipaccounting-control.txt)）：同一台閘道器上，一個帶 `IPAccounting=yes` 的 transient unit 只做一次連到 loopback port 1 的 TCP 連線（被拒絕）。systemd 記下 `40B incoming IP traffic, 60B outgoing IP traffic`，也就是一個 SYN 與一個 RST。所以一次連線嘗試就會讓計數不是 0；七次啟動都是 0。
- 沒有做「身分完整」的對照啟動：測試用 unit 以真的身分啟動，會以同一個 client ID 連 broker，搶走執行中 agent 的 MQTT session。身分完整時同一個執行檔會送 `READY=1`、連線、輪詢：執行中的 agent 就是這樣，見下節；以測試 unit 做的對照在建置主機上（`without_a_valid_identity_...` 的對照組）。

### 執行中的 agent：命令列與環境變數

[`running-agent-proc.txt`](data/2026-11-02-823-agent-fail-closed-gateway/running-agent-proc.txt)，2026-11-02 13:57:43，以 root 唯讀讀取 `/proc/6372/`：

- `cmdline`：`/usr/bin/trustiot-agent`、`--config`、`/etc/trustiot/agent.toml`，沒有其他參數。
- `environ`：`LANG`、`PATH`、`NOTIFY_SOCKET`、`WATCHDOG_PID`、`WATCHDOG_USEC`、`USER`、`LOGNAME`、`HOME`、`INVOCATION_ID`、`JOURNAL_STREAM`、`STATE_DIRECTORY`、`CONFIGURATION_DIRECTORY`、`SYSTEMD_EXEC_PID`、`MALLOC_MMAP_THRESHOLD_`。除了 `MALLOC_MMAP_THRESHOLD_` 由 unit 設定，其他都是 systemd 自己給的。
- 兩者合起來，以不分大小寫的 `pin|pass|auth|secret|token|key` 比對，0 行符合。
- `status`：uid／gid 985（`trustiot-agent`），補充群組 105（tss），五組 capability 都是 0，`NoNewPrivs: 1`，`Seccomp: 2`。

### 前後對照與執行中的 agent

[`inventory-before.txt`](data/2026-11-02-823-agent-fail-closed-gateway/inventory-before.txt)（13:57:28）與 [`inventory-after.txt`](data/2026-11-02-823-agent-fail-closed-gateway/inventory-after.txt)（13:59:37，清除之後）：

| 項目 | 之前 | 之後 |
| --- | --- | --- |
| TPM persistent handle | `0x81000001`、`0x81000100` | `0x81000001`、`0x81000100`（中間加過 `0x81000823`、`0x81000824`，見 `prepare.txt`；清除時 evict，見 `cleanup.txt`） |
| `TPM2_PT_LOCKOUT_COUNTER` | 0x0 | 0x0（`properties-variable` 全部相同） |
| `inLockout` | 0 | 0 |
| 執行中 agent 的 `MainPID` | 6372 | 6372 |
| `ActiveEnterTimestamp` | Sat 2026-10-31 01:18:16 CST | 相同 |
| `NRestarts` | 0 | 0 |
| `InvocationID` | `<id-1>` | `<id-1>` |
| 執行檔、unit、sysusers、`agent.toml`、`broker-ca.pem`、`gateway.pem`、`desired-config.cbor` 的 SHA-256、權限、擁有者、修改時間 | 見紀錄 | 全部相同 |
| 雲端連線（443） | 本地 port 40390、inode 99074，`bytes_sent:850216` | 同一條連線（port 與 inode 相同），`bytes_sent:852332`、`bytes_received:215005` |
| 轉換器連線（Modbus RTU over TCP） | inode 50095，`data_segs_out:3640`、`bytes_sent:29120` | 同一條連線，`data_segs_out:3642`、`bytes_sent:29136`：這段時間又輪詢了 2 次 |
| 測試用的 unit、目錄與檔案 | 不存在 | 不存在 |
| nftables 表 | 沒有 | 沒有 |

- 執行中 agent 的 journal（[`running-agent-journal.txt`](data/2026-11-02-823-agent-fail-closed-gateway/running-agent-journal.txt)）：2026-11-02 13:50:00 到 13:59:45 沒有任何紀錄，沒有斷線、重連或錯誤。這段時間之前的最後一次重連在 2026-11-02 02:09:03（`WebSocket protocol error: Connection reset` 之後 1 秒內連回，同一個行程，`NRestarts` 沒有變），與這次測試無關。
- 這個版本的 agent 在 info 等級不記每次輪詢，所以「照常輪詢」以轉換器連線的 TCP 計數為證據，「資料照常上傳」以雲端連線的位元組數為證據。
- broker 端（[`broker.txt`](data/2026-11-02-823-agent-fail-closed-gateway/broker.txt)）：Mosquitto 從 2026-11-02T05:50:00Z 到 05:59:55Z（13:50:00–13:59:55 +08:00，涵蓋全部七次啟動與清除）共 239 行。其中 237 行是 readiness probe，2 行是 certs 服務的 `listClients`，沒有任何其他紀錄：沒有新連線、沒有 TLS 錯誤、沒有閘道器的斷線或連線。
  - 閘道器最後一次連上 broker 是 2026-11-01T18:09:03Z，就是上面那次重連。
  - 這個 broker 連 TLS 失敗的連線嘗試也會記（例如 2026-11-01T10:56Z 的 `New connection from ... on port 8883` 與 `Protocol error`），所以沒有紀錄表示沒有連線到達 broker。
  - 不經 broker 就在外層被擋下的嘗試，由上面的 IP 計量排除。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 五種情況在閘道器上各做一次，每種都：unit 沒有進入 active；journal 有 `refusing to start` 與原因；broker 沒有這台閘道器的連線；`desired-config.cbor` 不變 | 通過。沒有設定檔、沒有憑證、沒有私鑰（TPM 的空 handle，另加檔案金鑰）、`broker.ca` 沒簽的憑證、別張憑證的私鑰，各啟動一次：每次都以結束碼 1 結束，沒有進入 active，也沒有任何通知；journal 有原因；unit 沒有收送任何 IP 封包，broker 也沒有紀錄；沒有輪詢的紀錄；`desired-config.cbor` 不變；沒有開離線緩衝 |
| 設了密碼的 TPM 金鑰啟動失敗 | 通過：`TPM_RC_BAD_AUTH`，以結束碼 1 拒絕啟動，鎖定計數維持 0x0 |
| 執行中的 agent 的命令列與環境變數沒有 PIN 或密碼 | 通過：命令列只有 `--config` 與路徑；環境變數只有 systemd 給的與 `MALLOC_MMAP_THRESHOLD_` |
| 做完後閘道器還原成原本的身分與設定，agent 恢復連線與輪詢 | 通過（依 2026-11-02 的做法）。執行中 agent 的身分與設定從頭到尾沒有被改過（雜湊、權限、修改時間前後相同）。它沒有停過（`MainPID`、`ActiveEnterTimestamp`、`InvocationID` 相同，`NRestarts=0`），雲端連線沒有斷，輪詢沒有中斷。測試用的 unit、檔案、拋棄式 CA 與兩把 TPM 測試金鑰都已移除，persistent handle 與做之前相同 |

紀錄不含 LAN 位址、主機名稱與客戶名稱。

與 issue 範圍的差異：

- issue 寫的是用 `edge/agent/systemd/trustiot-agent.service` 啟動。這次用的是閘道器上已部署的 unit 的複本，它比 main 少了出口規則的部分與兩行沙箱設定（見「環境」），但 fail-closed 的判斷在 agent 自己的程式裡，與這些行無關。
- 閘道器換上 main 的 unit 與執行檔之後，要不要再跑一次，留給之後部署新版本時決定，不屬於這張 issue。
