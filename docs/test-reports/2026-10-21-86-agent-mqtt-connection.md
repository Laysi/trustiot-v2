# 閘道器 agent 的執行檔、systemd 與連線（2026-10-21）

對應 issue：Laysi/trustiot-v2#86 閘道器 agent 骨架（設定、aarch64 執行檔、systemd unit、443 的 WebSocket 內走 mTLS 與 MQTT 5、在線狀態、重連、記憶體上限）。原始資料在 [data/2026-10-21-86-agent-mqtt-connection/](data/2026-10-21-86-agent-mqtt-connection/)。

## 環境

- 機器：一台 x86_64 Linux 工作站，以 podman 執行容器，沒有 arm64 模擬器；`systemd-analyze` 用這台機器上的（CI 用 runner 的）。
- 工具：rustc 1.99.0（`rust-toolchain.toml`）；zig 0.16.0、cargo-zigbuild 0.23.4（`.prototools`）。
- 程式：分支 `feat/86-agent-mqtt-connection`，以 main 的 `cc185b9` 為基礎，變更尚未提交；2026-10-22 的審查修正以 `630021f`（同一張 issue 的 TPM 部分）為基礎。rustls 0.23.46 加 ring 0.17.14、tokio 1.53.2、tokio-tungstenite 0.30.0；MQTT 5 編解碼是 agent 自己的（`edge/agent/src/mqtt/`）。
- 金鑰：這些測試都用 `identity.key` 的 `file` 來源。TPM 的簽章金鑰（`src/tpm.rs`）是同一張 issue 的另一部分，本報告不涵蓋。
- broker：Mosquitto 2.1.2（`docker.io/library/eclipse-mosquitto:2.1.2-alpine@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408`），設定見 `crates/testkit/src/mqtt/mosquitto.rs`：mTLS listener（`require_certificate`、`use_identity_as_username`、TLS 1.3、`ciphers_tls1.3` 把 ChaCha20 排第一）、dynamic security 與協定的閘道器角色。入口與 mqtt-bridge 由測試行程內的 WebSocket 前端（`testkit::mqtt::front`）替代。
- 測試 CA、所有憑證與金鑰都在測試時以 rcgen 產生，沒有進 repo。

## 指令

全部在 repo 根目錄執行。

```sh
CI=true cargo test --locked -p agent               # 217 個測試，含 Mosquitto 與 swtpm 容器
cargo test --locked -p agent --test broker         # 驗收 1、2：對 Mosquitto
cargo test --locked -p agent --test transport      # 整條連線在行程內：協商內容、resumption、重連、拒絕
cargo test --locked -p agent --test link           # 驗收 3 與 MQTT 行為（停住的時鐘）
cargo test --locked -p agent --test binary         # 執行檔：--check、sd_notify、SIGTERM
cargo test --locked -p agent --test systemd_unit   # 驗收 4
cargo test --locked -p agent --test dependencies   # 閘道器建置的相依
systemd-analyze verify --man=no edge/agent/systemd/trustiot-agent.service
systemd-analyze security --offline=true --no-pager edge/agent/systemd/trustiot-agent.service

# aarch64 執行檔
proto install zig && proto install cargo-zigbuild
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent

# 驗收 5：在 HEAD 是未改動程式碼的乾淨工作樹執行（見「結果」第 5 節）
bash docs/test-reports/data/2026-10-13-8-contract-type-changes/run-mutations.sh

cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo deny --locked check
bun scripts/ci/check-workflows.ts && bun scripts/ci/lint-workflows.ts && bun scripts/ci/check-secret-handling.ts
bun test scripts/ci/changes.test.ts scripts/ci/workflow-conditions.test.ts
```

## 結果

`CI=true cargo test --locked -p agent`：217 個測試全部通過（2026-10-23，含 TPM 部分與 Modbus 的測試；[cargo-test-agent.txt](data/2026-10-21-86-agent-mqtt-connection/cargo-test-agent.txt)）。以下依驗收條件整理。

### 1. WebSocket 內的 mTLS 連到 broker（驗收 1）

`tests/broker.rs` 的 `the_agent_talks_mqtt_5_with_mosquitto_through_the_websocket_with_mtls_inside`：agent 由設定檔啟動（`Config::load` 加 `Setup::new`，與執行檔相同），經前端的外層 TLS 與 WebSocket，內層 mTLS 直達 Mosquitto 的 mTLS listener。

- Mosquitto 的日誌：用戶端以 MQTT 5（`p5`）連線，client ID 與 username 都是憑證的 CN（`as gw-test-1 ... u'gw-test-1'`），協商出 `TLSv1.3 cipher TLS_CHACHA20_POLY1305_SHA256`。
- 雲端帳號訂閱 `trustiot/v1/gw/gw-test-1/up/#`，收到這次 session 的 `online`，retained、QoS 1。
- 連線前發布的 retained 設定在 agent 訂閱時送到（`retained = true`）。這個測試明列訂閱 `config`；執行檔目前不訂閱任何頻道（「審查後的修正」的「收到的 `down` 訊息確認之後丟掉」一列）。
- QoS 1 的遙測拿到 PUBACK 0x00。從閘道器角色拿掉 `up/log` 的 `publishClientSend` 之後，log 的 publish 回 `PublishError::Refused(0x87)`，連線不中斷。
- SIGTERM 等同的停止：`offline`／`shutdown`（retained），broker 日誌是沒有原因的 `disconnected.`（收到 DISCONNECT 0x00）；之後新的訂閱拿到的 retained 狀態仍是 `offline`／`shutdown`，表示 will 沒有發布。

`tests/transport.rs` 在行程內跑同一條路，記錄每一層協商的內容：外層 TLS 1.3、ChaCha20-Poly1305、ALPN `http/1.1`、SNI 是 URL 的主機；WebSocket 路徑 `/mqtt`、subprotocol `mqtt`；內層 TLS 1.3、ChaCha20-Poly1305，broker 端看到的用戶端憑證就是閘道器的憑證。64 KiB 的 `live` 訊息與 retained 設定雙向通過每一層。以下都拒絕連線，錯誤指出是哪一步：外層憑證來自別的 CA（`outer TLS`，UnknownIssuer）、前端只有 TLS 1.2（`outer TLS`）、前端沒回 `mqtt` subprotocol（`WebSocket upgrade`）、broker 憑證的名稱不符（`inner mTLS`）、broker 憑證來自別的 CA（`inner mTLS`，UnknownIssuer）、broker 不信任閘道器的憑證（broker 端 UnknownIssuer，agent 在 CONNACK 之前收到 alert）。2026-10-22 加上兩層互不信任的測試：broker 的憑證由外層的 CA 簽發（`inner mTLS`，UnknownIssuer）、前端的憑證由 broker CA 簽發（`outer TLS`，UnknownIssuer）、外層憑證是同一個 CA 簽給別的主機（`outer TLS`，not valid for name）、broker 只有 TLS 1.2（`inner mTLS`）。

### 2. broker 或轉接服務重啟後重連（驗收 2）

`tests/broker.rs` 的 `the_agent_reconnects_after_the_relay_or_the_broker_restarts`，對 Mosquitto：

1. 前端（轉接服務的替身）不送 WebSocket close 就切斷所有連線：Mosquitto 發布 will，內容等於 `Status::last_will(<第一個 session>)`，retained、QoS 1；agent 的狀態變成 `Disconnected`。
2. 前端在同一個 port 重新開始：agent 自己重連，新的 session，雲端收到新 session 的 `online`。
3. `podman restart`（SIGTERM 後重新啟動，retained 訊息消失）：agent 自己重連，第三個 session；新的訂閱拿到的 retained 狀態是第三個 session 的 `online`。

`tests/transport.rs` 的 `after_the_broker_or_the_relay_goes_away_the_agent_reconnects_and_resumes_tls` 另外確認：broker 端的連線斷掉後重連時，內層與外層 TLS 都是 resumption（`HandshakeKind::Resumed`），不需要再簽章；前端當掉再起來後同樣重連。`tests/link.rs` 以停住的時鐘確認退避：連續 6 次 CONNACK 0x87，相鄰兩次的間隔都在 0.1 秒到 min(60, 2^(n-1)) 秒之間，之後照常連上；收不到 PINGRESP 時在 keepalive 加一半之後斷線，以新的 session 重連。

### 3. 每條連線的記憶體有上限（驗收 3）

`tests/link.rs` 的 `the_inflight_limit_bounds_unacknowledged_publishes`，`inflight_limit = 8`，50 個呼叫端同時 publish，broker 不回 PUBACK。2026-10-22 起這 50 個呼叫端以 `Link::reserve` 等位置、拿到位置才建 payload；`publish` 本身在佇列滿時立刻回 `Busy`（見「審查後的修正」）：

| 觀察 | 結果 |
| --- | --- |
| 送出的 PUBLISH | 正好 8 則，下一個封包是一整個 keepalive（60 秒）之後的 PINGREQ |
| `in_flight()` | 8 |
| `queued()`（等著送出的佇列） | 8（`REQUEST_QUEUE`），不再增加 |
| 呼叫端 | 50 個都在等（16 個的訊息在 in-flight 或佇列裡，其餘 34 個在 `reserve` 等位置，不帶 payload） |
| 回一個 PUBACK | 剛好再送出 1 則，之後 30 秒內沒有其他封包（2026-10-22 起；再等一整個 keepalive 會碰到第一批 publish 的 PUBACK 期限，見「審查後的修正」） |
| 全部回完（其中一個 0x87） | 49 個 `Acknowledged`、1 個 `Refused(0x87)`，過程中 `in_flight()` 都不超過 8 |

把 connection.rs 的條件 `st.publishes < st.limit` 改成 `<=` 時，這個測試與 broker 的 Receive Maximum 測試都失敗（只在本機試過，沒有留下改動）。其他上限：broker 的 Receive Maximum 2、Maximum Packet Size 1000、Server Keep Alive 10 秒都照辦（`the_brokers_limits_from_connack_are_respected`）；收到的訊息佇列 16 則加 1 則等著放進去，第 17 則 QoS 1 訊息在應用程式取走一則之前不回 PUBACK（`down_messages_are_delivered_acknowledged_and_bounded`）；宣告長度超過 Maximum Packet Size 的封包看到標頭就斷線（日誌記 0x95，送 DISCONNECT 0x04），讀取緩衝不為它配置（`broken_packets_end_the_connection_with_the_will` 與 connection.rs 的單元測試）。各項上限與最壞情況的估算見 `edge/agent/README.md`「記憶體上限」。

### 4. systemd unit 以非 root 執行（驗收 4）

`tests/systemd_unit.rs` 檢查 `edge/agent/systemd/trustiot-agent.service`：`User=` 與 `Group=` 都是 `trustiot-agent`（不是 root，沒有 `DynamicUser`），帳號由 `trustiot-agent.sysusers` 建立（系統帳號、`/usr/sbin/nologin`）；`Type=notify`、`NotifyAccess=main`、`WatchdogSec=30s`、`Restart=always`、`StartLimitIntervalSec=0`、`TimeoutStopSec=20s`；沙箱各行（`NoNewPrivileges`、空的 capability、`ProtectSystem=strict`、`DevicePolicy=closed` 只開 `/dev/tpmrm0` 等），以及不能設的 `PrivateDevices`、`ProcSubset`、`PrivateUsers` 沒有出現。

`systemd-analyze verify` 唯一的意見是測試機器上沒有 `/usr/bin/trustiot-agent`；`systemd-analyze security --offline` 的評分是 1.5（OK），剩下的是網路、TPM 裝置、tss 群組與 `/proc`（[systemd-analyze.txt](data/2026-10-21-86-agent-mqtt-connection/systemd-analyze.txt)）。CI 的 runner 有 systemd-analyze，`CI` 有設時缺少它會失敗。

`tests/binary.rs` 執行真的執行檔，測試扮演 systemd 的通知 socket：沒有 broker 時仍送 `READY=1`、`STATUS=... disconnected ...`，每秒送 `WATCHDOG=1`（`WATCHDOG_USEC` 設 2 秒）；連上後 `STATUS=` 顯示 session；SIGTERM 後送 `STOPPING=1`、`offline`／`shutdown`、DISCONNECT 0x00，以 0 結束。`--check` 對可用的設定印出 `ok: gateway gw-test-1, certificate valid until 4096-01-01T00:00:00Z, ...` 並以 0 結束，金鑰不是憑證的金鑰時以 1 結束。

### 5. protocol 型別變更使 edge/agent 編譯失敗（驗收 5）

在 `run-mutations.sh` 所在的目錄加上 [p2-protocol-heartbeat-interval-rename.patch](data/2026-10-13-8-contract-type-changes/p2-protocol-heartbeat-interval-rename.patch)：`Heartbeat.interval_s` 改名為 `interval_secs`，以 `serde(rename)` 保持傳輸格式（agent 的心跳會用到這個欄位）。腳本的 `p1-*` 分支改成 `p[0-9]*`，p2 也以 protocol 為「被改的 crate」。

腳本拒絕在有未提交改動的樹上執行（每個變更以 `git reset --hard` 還原），所以把工作樹（已追蹤與未追蹤、未被忽略的檔案）複製到暫存目錄，`git init` 並提交成快照後在那裡執行；輸出裡的 `base commit` 是那個快照，不是本 repo 的 commit。結果在 [run-mutations.out](data/2026-10-21-86-agent-mqtt-connection/run-mutations.out)：

| 變更 | `cargo check --workspace --all-targets` | `--keep-going` 列出失敗的 crate |
| --- | --- | --- |
| 基準（沒有變更） | 0 | — |
| m1–m4（contract） | 101 | 依賴 contract 的 crate 與 2026-10-13 相同（`service-kit`、`outbox`、`identity`、`testkit`）；m2 另有 contract 自己在 2026-10-13 之後新增的 `events` 測試 |
| p1（`TelemetryAck.server_time`） | 101 | 只有 protocol 自己的測試目標：agent 還沒用到 `TelemetryAck` |
| p2（`Heartbeat.interval_s`） | 101 | `agent`（lib，`edge/agent/src/heartbeat.rs` 的 `build` E0560；lib test 另有 E0609；輸出裡的行號是當時快照的）與 protocol 自己的 9 個測試目標 |

p2 在 CI 的 clippy 步驟（`cargo clippy --workspace --all-targets -- -D warnings`）也以 `could not compile agent (lib)` 失敗。

### 6. aarch64 執行檔

以 cargo-zigbuild 建置成功（[aarch64-build.txt](data/2026-10-21-86-agent-mqtt-connection/aarch64-build.txt)）：aarch64 的 PIE 執行檔，動態連結 glibc（只需要 `libc`、`libm`、`libpthread`、`libdl`，最新的符號版本 `GLIBC_2.30`，閘道器的 Raspberry Pi OS 是 glibc 2.41），release 版 2,742,960 bytes。不用 Zig 直接 `cargo build --target aarch64-unknown-linux-gnu` 會在 `ring` 的 build script 找不到 `aarch64-linux-gnu-gcc` 而失敗，所以 CI 的 aarch64 建置步驟在 x64 主機上改用 cargo-zigbuild；把那個步驟的指令原樣在工作站執行，protocol 與 agent 都建置成功。工作站沒有 arm64 模擬器，執行檔沒有在 aarch64 上啟動過。

`tests/dependencies.rs` 確認閘道器建置（`--filter-platform aarch64-unknown-linux-gnu`）沒有 `webpki-roots`、`rustls-native-certs`、OpenSSL、aws-lc、tss-esapi，唯一宣告 `links` 的 crate 是 `ring`。

### 7. 發現：Mosquitto 依自己的順序選套件

agent 的內層 TLS 依序提供 ChaCha20-Poly1305、AES-256-GCM、AES-128-GCM。Mosquitto 的 listener 沒設 `ciphers_tls1.3` 時協商出 `TLS_AES_256_GCM_SHA384`；設成 `TLS_CHACHA20_POLY1305_SHA256:TLS_AES_256_GCM_SHA384:TLS_AES_128_GCM_SHA256` 後協商出 ChaCha20（[mosquitto-cipher-order.txt](data/2026-10-21-86-agent-mqtt-connection/mosquitto-cipher-order.txt)）。所以 Mosquitto 依自己的順序選，不是決策 6 原本寫的依用戶端的順序；2026-10-13 的測試用戶端只提供 ChaCha20，看不出這點。閘道器的 CPU 沒有 AES 指令，broker 給閘道器的 listener 要設這一行；已更正決策 6 與 2026-10-13 的可行性文件，testkit 的 Mosquitto 也加上這一行，`tests/broker.rs` 檢查日誌裡的套件。

### 8. 發現：Mosquitto 只在 DISCONNECT 0x04 或沒有 DISCONNECT 時發布 will

MQTT 5 規格寫的是：除了 DISCONNECT 0x00 之外，連線怎麼結束 broker 都要發布 will。對 testkit 的 Mosquitto 2.1.2（`MOSQUITTO_IMAGE`）實測：一個原始的 MQTT 5 用戶端拿閘道器憑證連上（Clean Start，will 是自己 `up/status` 上 retained、QoS 1 的訊息），照下表的方式結束連線，等 broker 關閉連線後讀 retained 狀態（每一列之前先放一則 retained 的基準訊息，所以不必等「沒有發生」）。測試是 `tests/broker.rs` 的 `which_ends_make_mosquitto_publish_the_will`，這張表就是它的斷言；輸出在 [mosquitto-will-by-disconnect.txt](data/2026-10-21-86-agent-mqtt-connection/mosquitto-will-by-disconnect.txt)。

| 連線怎麼結束 | will 是否發布 |
| --- | --- |
| DISCONNECT 0x00 | 否 |
| DISCONNECT 0x04（Disconnect with Will Message） | 是 |
| DISCONNECT 0x80、0x81、0x82、0x83、0x94、0x95 | 否 |
| TLS close_notify 與 TCP FIN，沒有 DISCONNECT | 是 |

所以 agent 除了正常停止之外，每一種失敗都以 DISCONNECT 0x04 結束（見「審查後的修正」最後幾列與 `edge/agent/README.md`「連線結束與 will」）。

### 其他檢查

- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`：通過。
- `cargo deny --locked check`：advisories、bans、licenses、sources 都 ok。新增的 crate 只有 `tokio-tungstenite` 0.30.0（MIT）與 `tungstenite` 0.30.0（MIT OR Apache-2.0）；duplicate 的警告都是 main 原本就有的。2026-10-22 起 agent 直接使用 `socket2` 0.6.5（MIT OR Apache-2.0，原本就經由 tokio 在 `Cargo.lock` 裡），開 `all` feature 以設定 TCP keepalive 的間隔與次數。
- `bun scripts/ci/check-workflows.ts`、`bun scripts/ci/lint-workflows.ts`、`bun scripts/ci/check-secret-handling.ts`：沒有違規。`bun test scripts/ci/changes.test.ts scripts/ci/workflow-conditions.test.ts`：309 個通過。
- 每次容器測試後確認沒有留下 Mosquitto 容器，podman 的 volume 數不變。

## 審查後的修正（2026-10-22）

合併 TPM 部分（`630021f`）之後的獨立安全審查沒有發現嚴重或高風險的問題，但列出下面幾項要在 PR 前修正。標明「提交前的 Codex 審查」的各列是同一天提交前的 Codex 審查（pre-commit gate）找到的，標明「掃同類問題」的各列是依它們掃過同類程式碼（測試輔助程式、同一類的 broker 回覆）的結果，或是為它們寫測試時發現的。每一項都有回歸測試；測試先寫，在修正前執行確認失敗，修正後通過（MQTT 5 規則逐條檢查、佇列屬於一條連線、合約稽核、寫入的逾時、watchdog、PUBACK 期限的起點與只訂閱有處理程式的頻道七列，測試與修正一起寫，以下面把修正改回去的方式確認測試會失敗）。各列寫的 DISCONNECT 0x80、0x82、0x83 是當時送的碼：後來實測 Mosquitto 收到這些碼會丟掉 will，所以現在 agent 對每一種失敗都送 0x04（「Mosquitto 只在 DISCONNECT 0x04 …」一列），測試也跟著改；先前說「非 0 的 DISCONNECT 讓 broker 發布 will」是錯的。舊的測試名稱 `broken_packets_are_answered_with_the_matching_disconnect` 現在叫 `broken_packets_end_the_connection_with_the_will`。另外把每個修正改回去（一次一個，改完立刻還原並比對檔案相同），確認對應的測試會失敗，輸出在 [review-mutations.txt](data/2026-10-21-86-agent-mqtt-connection/review-mutations.txt)。

| 項目 | 修正 | 測試 | 把修正改回去時 |
| --- | --- | --- | --- |
| 兩層的信任互相分開 | 原本就分開，補上測試；設定另外拒絕 `broker.name` 等於 `cloud.url` 的主機 | `tests/transport.rs`：broker 憑證由外層的 CA 簽發、前端憑證由 broker CA 簽發、外層憑證簽給別的主機、broker 只有 TLS 1.2，都拒絕；`config.rs` 的 `values_that_break_a_rule_are_refused_with_their_key` | 讓外層也信任 `broker.ca`、內層也信任 `cloud.trust`：前兩個測試失敗（握手成功，等到 CONNACK 逾時）；拿掉名稱檢查：設定測試失敗 |
| 退避只在連線維持 60 秒後歸零 | 規則不變，原本沒有測試；退避的亂數來源改成可注入（`LinkSettings::jitter`） | `tests/link.rs` 的 `only_a_connection_that_stayed_up_resets_the_backoff`：固定抽最長延遲，7 次連上就被 DISCONNECT 的連線，間隔是 1、2、4、8、16、32 秒；維持 61 秒的連線斷掉後回到 1 秒 | 改成「連上過就歸零」：第二個間隔是 1 秒而不是 2 秒；改成「從不歸零」：穩定連線之後等了 60 秒 |
| Server Keep Alive 0 | 0 時保留自己的 60 秒 keepalive，只採用非 0 的值；TCP 另開 keepalive（60 秒後每 15 秒、4 次，`socket2`） | `a_server_keep_alive_of_zero_keeps_the_agents_own_keepalive`：CONNACK 帶 0、broker 不回應時，60 秒送 PINGREQ、90 秒斷線；`transport.rs` 的單元測試讀回 socket 的 keepalive 設定 | 照收 0：等不到 PINGREQ |
| 等 CONNACK 時收到 SIGTERM | 在 5 秒內繼續等 CONNACK，來了就照一般的停止（`online`、`offline`／`shutdown`、PUBACK、DISCONNECT 0x00）；沒來就關閉 | `a_stop_while_waiting_for_connack_still_ends_with_offline_and_a_normal_disconnect`、`a_stop_without_any_connack_gives_up_after_the_stop_deadline` | 回到直接返回：串流被丟掉，broker 會發布 will |
| broker 的 reason string 含控制字元 | 解碼時與記錄時都把控制字元（含 `\n`、`\r`、`\t`、C1、ESC）換成空白 | `reason_strings_from_the_broker_lose_their_control_characters`（DISCONNECT 帶 `"line one\r\nline two\tend\u{85}\u{1b}[31m"`），`packet.rs` 的單元測試 | 保留控制字元：錯誤訊息含控制字元 |
| `cloud.url` 接受 `wss://0x7f000001/mqtt` | 最後一段是數字（十進位、八進位、`0x` 十六進位）的主機一律拒絕，與 WHATWG URL 的 ends-in-a-number 規則相同 | `transport.rs` 的單元測試：`0x7f000001`、`0x7f.1`、`0X7F.0.0.1`、`127.1`、`2130706433`、`017700000001`、`192.0.2.1.`、`gw.trustiot.123`、`gw.trustiot.0x1f` 拒絕，`1gw.trustiot.example`、`gw-0x1.trustiot.example` 接受 | 拿掉檢查：`0x7f000001` 被接受 |
| 啟動與 `--check` 接受 broker 會拒絕的憑證 | 以 rustls 的 `WebPkiClientVerifier` 依 `broker.ca` 檢查用戶端憑證鏈與 EKU，key usage 有的話必須含 digitalSignature；只有時間不對時記警告（寫出 notBefore 與 notAfter）並照樣啟動；`--check` 印出 notAfter | `tests/binary.rs`：別的 CA 簽的憑證以 1 結束、錯誤指出 `broker.ca`；2020-01-01 到 2021-01-01 的憑證以 0 結束、stderr 有 `WARN` 與兩個日期；`identity.rs` 的 key usage 案例 | 拿掉憑證鏈檢查：兩個 binary 測試失敗；拿掉 key usage 檢查：單元測試失敗 |
| TPM 逾時比 watchdog 長 | `tpm::RESPONSE_TIMEOUT` 從 30 秒改成 10 秒；連線執行緒的進度計數改名為 `sdnotify::PROGRESS_TICK` | `tests/systemd_unit.rs` 的 `a_stuck_tpm_cannot_outlast_the_watchdog_or_the_start_timeout`：從 unit 讀出 `WatchdogSec` 與 `TimeoutStartSec`，檢查逾時加一次計數小於 `WatchdogSec` 的一半、兩次逾時小於 `TimeoutStartSec` | 改回 30 秒（也試了 14 秒）：失敗 |
| PUBACK 沒有期限、QoS 0 排在滿的 in-flight 後面 | PUBACK 與 SUBACK 的期限是 keepalive 的兩倍，逾時送 DISCONNECT 0x80 並重連，呼叫端拿到 `ConnectionLost`；QoS 0 與 QoS 1 各有一個 8 則的佇列，QoS 0 不受 in-flight 上限限制 | `a_puback_that_never_comes_ends_the_connection_after_its_deadline`（broker 回 PINGRESP 但不回 PUBACK：120 秒時 DISCONNECT 0x80，之後重連）、`qos_0_publishes_pass_a_full_inflight_window` | 拿掉期限：等不到 DISCONNECT；QoS 0 也看 in-flight：心跳排在後面 |
| 1 MiB 的設定訊息 | 原本就支援，補上測試（protocol README 的要求） | `tests/broker.rs`：Mosquitto 上 retained 的 1 MiB 設定經 WebSocket 前端與內層 TLS 送到，長度與內容不變 | — |
| SUBACK 拒絕訂閱時只記錄、仍回報 Connected（提交前的 Codex 審查） | 任何一個 `down` 頻道的訂閱被拒（≥ 0x80）就送 DISCONNECT 0x83（當時以為 broker 會因此發布 will，其實 Mosquitto 收到 0x83 會丟掉 will；現在送 0x04，見「Mosquitto 只在 DISCONNECT 0x04 …」一列）、回 `LinkError::SubscriptionRefused`（topic 與 reason code），照退避重連；`Connected` 改在 SUBACK 同意全部訂閱後才回報，順序是 CONNACK → online → SUBSCRIBE → SUBACK → Connected；退避歸零的 60 秒也從這時算；停止中收到的拒絕照常完成停止 | `a_refused_subscription_ends_the_connection_and_never_reports_connected`：SUBSCRIBE 之後、SUBACK 之前狀態仍是 `Connecting`；一個頻道回 0x87 時收到 DISCONNECT 0x83、連線關閉、錯誤含 topic 與 `0x87 (not authorized)`、1 秒（固定抽最長延遲）後重連並連上 | 改回只記錄：等不到 DISCONNECT（下一個封包是 PINGREQ）；改回 SUBSCRIBE 後就回報 Connected：SUBACK 前狀態已是 Connected |
| 寫錯的 port 被當成 443（提交前的 Codex 審查） | 寫了 port 就必須是 1–65535 的十進位數字，否則 `cloud.url` 設定錯誤；只有沒寫 port 才用 443 | `transport.rs` 的單元測試：`:0`、`:65536`、`:99999`、`:abc`、`:`、`:+443`、`:-1` 拒絕，`:65535` 接受 | 解析失敗時退回 443：`:0` 被接受成 443 |
| PUBACK 的 reason code 沒有檢查，小於 0x80 都算成功（提交前的 Codex 審查） | 解碼時以 MQTT 5 的允許集合檢查 reason code：PUBACK（0x00、0x10、0x80、0x83、0x87、0x90、0x91、0x97、0x99），並一併掃過 CONNACK、SUBACK（每一個）與 broker 的 DISCONNECT；不在集合裡是協定錯誤，送 DISCONNECT 0x82 並重連，呼叫端拿到 `ConnectionLost`。成功只認明列的碼（CONNACK 0x00、PUBACK 0x00 與 0x10、SUBACK 0x01；SUBACK 0x00 起初只記警告，之後改成訂閱失敗，見「SUBACK 只給 QoS 0」一列），SUBACK 0x02（超過要求的 QoS 1）是協定錯誤，程式裡不再有「小於 0x80」的判斷；3.1.1 broker 的兩位元組 CONNACK 拒絕另外說明 | `packet.rs` 的 `only_the_reason_codes_mqtt_5_allows_are_accepted`（四種封包、0x00–0xFF 每一個碼）；`tests/link.rs`：`a_puback_with_a_reason_code_mqtt_5_does_not_allow_is_never_a_success`（0x01、0x02、0x7F：DISCONNECT 0x82、`ConnectionLost`）、`connack_and_suback_codes_outside_their_sets_end_the_connection`（CONNACK 0x01、0x10、0x7F；SUBACK 0x02、0x7F：DISCONNECT 0x82、不回報 Connected），`broken_packets_are_answered_with_the_matching_disconnect` 加上 PUBACK 0x01／0x7F 與 DISCONNECT 0x04／0x10 | 允許任何小於 0x80 的碼：單元測試與 PUBACK 測試失敗（PUBACK 0x01 之後送出的是 PINGREQ）；拿掉 QoS 2 檢查：SUBACK 0x02 變成拒絕訂閱（DISCONNECT 0x83 而不是 0x82） |
| testkit 前端的 `restart()` 在舊 listener 關閉前重新綁定（提交前的 Codex 審查） | `stop()` 改成 async：通知接受迴圈結束、關掉 listener、中止並等每條連線結束後才回來；`restart()` 先 `stop().await` 再綁定。`Drop` 仍只能 abort | testkit 的 `a_restart_rebinds_the_same_port_once_the_old_listener_is_gone`（有連線進行中時連續 restart 三次，之後 `stop().await` 讓 port 拒絕連線）；修正前在單執行緒 runtime 上第一次 restart 就 `AddrInUse` | 改回只 abort 不等：`restart 0: Address already in use` |
| 掃同類問題：Mosquitto 的 port、`mosquitto_sub`、binary 測試的關閉 port | `Mosquitto::start` 挑空 port 後交給 podman 之前可能被拿走，現在遇到 port 被占用就換一個重試（最多 5 次）；`Subscription` 被丟掉時原本只停掉本機的 `exec`，容器裡的 `mosquitto_sub` 繼續訂閱，現在依 pid 檔一起停掉；`tests/binary.rs` 的「沒人聽的 port」原本綁定後立刻放掉，可能被其他測試拿走，現在以只綁定、不 listen、不設 `SO_REUSEADDR` 的 socket 保留到測試結束 | testkit 的 `a_port_taken_before_the_runtime_binds_it_is_retried_with_another`、`a_dropped_subscription_stops_mosquitto_sub_in_the_container`；`tests/binary.rs` 的 `a_closed_port_refuses_connections_and_cannot_be_taken` | 只試一次：第一個 port 被占用就失敗；只停本機 client：`mosquitto_sub` 還在；保留用的 socket 設 `SO_REUSEADDR`：別的 socket 綁得上 |
| 佇列滿時 `publish` 的呼叫端各自握著 payload 等，記憶體上限只算到佇列（提交前的 Codex 審查） | `Link::publish` 不再等佇列的位置：以 `try_reserve` 立刻排入，滿了回新的 `PublishError::Busy`；要等的呼叫端用新的 `Link::reserve(channel).await` 拿 `PublishPermit`（tokio mpsc 的 `reserve_owned`，等待時不帶 payload，依先後拿到位置），再 `permit.publish(payload)`。QoS 0／QoS 1 兩個佇列與 in-flight 的規則不變。心跳改成遇到 `Busy` 就跳過（連續跳過只有第一次記警告）。README「記憶體上限」寫明 link 持有的項目與等待者不持有 payload | `tests/link.rs`：`publish_never_waits_for_room_so_callers_beyond_the_queue_hold_nothing`（佇列滿時 1000 個同時的 `publish` 第一次 poll 就全部 `Busy`，佇列與 in-flight 不變，QoS 0 的心跳照樣送出）、`reserve_waits_without_a_payload_and_hands_out_places_in_order`（三個等待者在拿到位置前都沒建 payload，之後依要求順序送出）；`the_inflight_limit_bounds_unacknowledged_publishes` 的 50 個等待者改用 `reserve` | `publish` 改回等位置（`reserve_owned().await`）：1000 個呼叫沒有一個在第一次 poll 回來；`reserve` 改成不等（滿了回 `Busy`）：等待者沒有在等 |
| 掃同類問題：link 停止之後才用的 `PublishPermit`（為上一列寫測試時發現） | 停止前拿到的 permit 在 supervisor 結束後才 `publish`：request 放進已沒有接收端的佇列，回覆的通道既不會被回答也不會被丟掉，呼叫端永遠等下去。等回覆時一併等佇列關閉（`Sender::closed`），回 `Stopped`；還在佇列裡的 request 隨 supervisor 丟掉時也回 `Stopped`，不是 `ConnectionLost` | `publishing_fails_at_once_while_offline_and_checks_the_channel_first` 加上 permit：同樣檢查頻道與大小、離線時回 `NotConnected`、用過或被拒的 permit 交回位置、停止前拿到的 permit 在停止後回 `Stopped`；`publishes_still_queued_when_the_supervisor_goes_away_report_the_stop` | 拿掉等佇列關閉的分支：停止後的 permit 等滿 60 秒的上限失敗；拿掉佇列已關閉時回 `Stopped` 的判斷：回 `ConnectionLost` |
| SUBACK 只給 QoS 0 仍回報 Connected：`down` 頻道的訊息不受 Receive Maximum 管、broker 可能丟掉（提交前的 Codex 審查） | SUBACK 0x00 與被拒的訂閱同樣處理：送 DISCONNECT 0x83（同樣誤以為會發布 will；現在送 0x04，見「Mosquitto 只在 DISCONNECT 0x04 …」一列）、回新的 `LinkError::SubscriptionDowngraded`（含 topic），照退避重連，不回報 `Connected`。掃同類問題：CONNACK 宣告 Maximum QoS 0 或 Retain Available 0，卻接受了 QoS 1、retained 的 will（MQTT-3.2.2-12、-13 要求這樣的 broker 拒絕），原本照常送出 QoS 1、retained 的 `online`（違反 MQTT-3.2.2-11、-14）：改成協定錯誤，送 DISCONNECT 0x82，不送 `online`；因此拿掉再也用不到的 `PublishError::NotSupported`。protocol README 寫明 broker 要支援 QoS 1 與 retained 訊息 | `a_broker_that_cannot_deliver_at_qos_1_or_retain_is_never_connected`：CONNACK Maximum QoS 0、Retain Available 0 各收到 DISCONNECT 0x82 而不是 `online`，錯誤寫明原因；SUBACK 一個頻道 0x00 時收到 DISCONNECT 0x83、不是 Connected、錯誤含 topic 與「only QoS 0」；Maximum QoS 1、Retain Available 1 且全部 QoS 1 時連上 | 改回 0x00 只記警告：等不到 DISCONNECT（下一個封包是 PINGREQ）；拿掉 Maximum QoS 0 或 Retain Available 0 的檢查：CONNACK 之後送出 `online` |
| 掃同類問題：testkit 前端的 `restart()` 在整個 testkit 測試一起跑時偶爾 `AddrInUse`（送審前跑檢查時發現） | 同一個測試行程裡 Mosquitto 的測試會啟動 podman：子行程在 fork 到 exec 之間拿著所有描述子的副本（close-on-exec 到 exec 才關），所以前端 drop 掉的 listener 會再 listen 一下，重新綁定同一個 port 失敗，停下後的連線也可能成功。新增 `stop_listening`：drop 之前先對 listener `shutdown`，socket 本身立刻離開 LISTEN，不管誰還拿著副本；前端停下時與 Mosquitto 挑 port 的試探 listener 都用它。改動前整個 testkit lib 測試跑 40 次失敗 9 次，改動後 40 次都通過 | `mqtt::tests::a_stopped_listener_frees_its_port_while_a_copy_of_its_descriptor_is_open`（留一個描述子副本代替子行程：系統挑的 port 與指定號碼綁定的 port，停下後都拒絕連線、能立刻重新綁定）；既有的 `a_restart_rebinds_the_same_port_once_the_old_listener_is_gone` | `stop_listening` 只 drop：上述測試失敗（連線成功）；拿掉前端的 `stop_listening`：整個 lib 測試跑 40 次，重新綁定的測試失敗 11 次 |
| packet ID 不在等回覆清單裡的 PUBACK 只記警告、連線照常（提交前的 Codex 審查） | 照 README 已寫的規則當成協定錯誤：送 DISCONNECT 0x82 並重連。packet ID 從 1 開始配，只在收到自己的 PUBACK 或連線結束時離開清單，所以這樣的 PUBACK（重複的、ID 0 的、從沒送過的）一定是 broker 的錯。掃同類問題：沒有 PINGREQ 的 PINGRESP 原本也直接收下，改成同樣的協定錯誤（同一時間最多一個 PINGREQ） | `broken_packets_are_answered_with_the_matching_disconnect` 加上四種：PUBACK 999、PUBACK 的 ID 0、`online` 的 PUBACK 再來一次、沒有 PINGREQ 的 PINGRESP，都收到 DISCONNECT 0x82 且連線關閉 | 改回只記警告：等滿 60 秒的上限，等不到 DISCONNECT；PINGRESP 不檢查：同樣等不到 |
| Mosquitto 只在 DISCONNECT 0x04 或沒有 DISCONNECT 的斷線時發布 will：agent 以 0x80、0x82、0x83 結束失敗的連線時，retained 的 `online` 留在 broker 上（提交前的 Codex 審查） | 先實測（[mosquitto-will-by-disconnect.txt](data/2026-10-21-86-agent-mqtt-connection/mosquitto-will-by-disconnect.txt)，Mosquitto 2.1.2）：DISCONNECT 0x00、0x80、0x81、0x82、0x83、0x94、0x95 都讓 broker 丟掉 will，只有 0x04 與不送 DISCONNECT 的斷線會發布（MQTT 5 規格寫的是 0x00 以外都發布）。修正：正常停止（`offline`／`shutdown` 拿到 PUBACK 後送 0x00）之外的每一種結束都送 DISCONNECT 0x04：協定錯誤（0x81、0x82、0x94、0x95 改成只寫進日誌）、訂閱被拒或只給 QoS 0（原本 0x83）、等不到 PUBACK 或 SUBACK（原本 0x80）；寫不出去的情況（連線已斷、PINGRESP 沒來、`online` 被拒）照舊不送 DISCONNECT 直接關閉，同樣會發布 will。README、程式註解與本報告裡「非 0 的 DISCONNECT 讓 broker 發布 will」的說法已更正 | `tests/broker.rs`（真的 Mosquitto）：`which_ends_make_mosquitto_publish_the_will`（上面的實測表，當成斷言）、`a_refused_subscription_leaves_the_will_as_the_status_and_the_agent_tries_again`（角色不准訂閱 `down/log-request`：SUBACK 0x87，agent 斷線，retained 狀態變成該 session 的 `offline`／`connection_lost`，之後換新的 session 重試）；`tests/link.rs` 原本期待 0x80、0x82、0x83 的測試都改成 0x04，壞封包的測試另外檢查錯誤種類寫在連線狀態裡 | 訂閱被拒改回 0x83：真的 Mosquitto 的測試等滿 120 秒都沒有 will（期間有 219 則重試的狀態），link 測試看到 0x83；協定錯誤改回各自的碼：壞封包的測試看到 0x81；期限改回 0x80：期限的測試看到 0x80 |
| 掃同類問題：`WsFront` 被 drop 時只中止接受連線的迴圈，沒有先停止 listen（提交前的 Codex 審查） | `Serving` 與迴圈共用 `Arc<TcpListener>`，`Drop` 先對它呼叫 `stop_listening` 再中止迴圈，與 `stop` 相同 | `mqtt::front::tests::stopping_or_dropping_closes_the_port_while_a_copy_of_the_listener_is_open`：留一個 listener 描述子的副本代替子行程，`stop` 之後與 drop 之後都拒絕連線、能立刻重新綁定 | `Drop` 不呼叫 `stop_listening`：drop 之後連線成功；迴圈結束時不呼叫：`stop` 之後連線成功（原本只能以整套測試跑 40 次統計，現在每次都失敗） |
| 停止時，已寫出但還沒有 PUBACK 的 publish 回 `Stopped`，與沒送出的分不出來（提交前的 Codex 審查） | 連線怎麼結束都一樣：等 PUBACK 的 publish 一律回 `ConnectionLost`（可能已送達），`Stopped` 只給還在佇列裡、沒寫出去的。掃同類問題：QoS 0 寫到一半失敗時原本直接丟掉回覆，若同時正在停止，佇列已關閉，呼叫端會拿到 `Stopped`；現在寫入失敗就明確回 `ConnectionLost`。`PublishError::Stopped` 的文件與 README 寫明兩者的分別 | `a_stop_reports_written_publishes_as_maybe_delivered_and_queued_ones_as_not_sent`（正常停止：已寫出的回 `ConnectionLost`；in-flight 滿時停止，`offline` 送不出去，停止期限後 DISCONNECT 0x04：已寫出的 `ConnectionLost`、佇列裡的 `Stopped`）、`a_qos_0_write_that_fails_during_a_stop_is_not_reported_as_never_sent`（1 MiB 的心跳卡在寫入時停止，寫入被切斷後回 `ConnectionLost`；起初是寫入逾時後，現在是停止期限到時，見「停止時正在進行的寫入」一列） | 依停止與否改回 `Stopped`：已寫出的拿到 `Stopped`；QoS 0 寫入失敗改回丟掉回覆：拿到 `Stopped` |
| 可變長度整數接受多餘的位元組，例如以 `0x80 0x00` 表示 0（MQTT-1.5.5-1；提交前的 Codex 審查）。依此逐條掃過 MQTT 5 對用戶端收到的封包（CONNACK、PUBLISH、PUBACK、SUBACK、PINGRESP、DISCONNECT，以及拒收的 AUTH）的 MUST 規則 | 修正：`read_varint` 拒絕最後一個位元組是 0 的多位元組編碼，是格式錯誤（日誌記 0x81，送 DISCONNECT 0x04）；Remaining Length、屬性長度、屬性識別碼與 Subscription Identifier 都經過它。掃過的規則，已符合、補上測試：固定標頭的旗標（CONNACK、PUBACK、SUBACK、PINGRESP、DISCONNECT 為 0；PUBLISH 的 QoS 3、QoS 0 帶 DUP）、CONNACK 的保留旗標、clean start 之後的 Session Present（`connection` 原本就拒絕，但沒有測試）、UTF-8 字串（`from_utf8` 拒絕 surrogate 與過長的編碼，另外拒絕 U+0000，開頭的 U+FEFF 保留；控制字元與 non-character 在規格只是「不應」，照常接受）、topic 不得為空（不接受 topic alias）也不得有萬用字元、每種封包允許的屬性、重複的屬性（User Property 與 Subscription Identifier 除外）、屬性值的範圍（Receive Maximum、Maximum Packet Size、Subscription Identifier 不得為 0；Maximum QoS、Retain Available、三個 Available、Payload Format Indicator 只能是 0 或 1）、欄位不得超出或填不滿 Remaining Length、AUTH 與其他用戶端不會收到的封包類型是協定錯誤。另外修正四項：CONNACK 沒有屬性長度原本被接受，MQTT 5 的 CONNACK 沒有省略的形式（MQTT-2.2.2-1，PUBACK 與 DISCONNECT 才有），現在是格式錯誤；拒絕的 CONNACK 帶 Session Present 1（MQTT-3.2.2-6）是協定錯誤；CONNACK 不再接受 Authentication Method 與 Authentication Data（agent 的 CONNECT 沒有 Authentication Method，MQTT-4.12.0-6）及 Response Information（agent 沒有要求，MQTT-3.1.2-28）；Response Topic 有萬用字元（MQTT-3.3.2-14）是格式錯誤。PUBACK、SUBACK 的 packet ID 0 原本在 `connection` 當成對不上的 ID（協定錯誤），現在解碼時就是格式錯誤，與 PUBLISH 相同。決定不做：Payload Format Indicator 為 1 時不檢查 payload 是否為 UTF-8（規格寫接收端 MAY 檢查，down 頻道都是 CBOR）；不在 down 頻道上的 PUBLISH 照舊記錄後丟掉並回 PUBACK（MQTT-3.3.2-3 是 broker 的義務，規格沒有規定用戶端怎麼處理）。README「協定錯誤」寫明這些規則 | `packet.rs`：新的 `every_mqtt_5_must_rule_on_received_packets_is_enforced`（45 個封包，每個只違反一條規則，另有用戶端不會收到的 9 種封包類型）；`varints_round_trip_at_every_width_boundary` 加上 5 種多餘位元組的編碼；`publish_from_the_server_decodes_with_its_flags` 加上規格允許的情況（沒有萬用字元的 Response Topic、重複的與最大的 Subscription Identifier、保留的 U+FEFF、控制字元）；`connack_reads_the_server_limits` 加上 Session Present 與重複的 User Property。connection.rs 的單元測試：多餘位元組的 Remaining Length 不配置緩衝。`tests/link.rs`：新的 `a_connack_that_breaks_mqtt_5_ends_the_connection_with_the_will`（clean start 之後的 Session Present、拒絕時的 Session Present、沒有屬性長度、Authentication Method、多餘位元組的 Remaining Length：都是 DISCONNECT 0x04、不送 `online`、連線狀態的錯誤寫明規則）；`broken_packets_end_the_connection_with_the_will` 加上多餘位元組的 Remaining Length、Response Topic 的萬用字元與 AUTH，packet ID 0 的 PUBACK 改成期待格式錯誤 | 拿掉最短編碼的檢查：單元測試與兩個 link 測試失敗（`0xD0 0x80 0x00` 被當成 PINGRESP，錯誤變成沒有 PINGREQ；CONNACK 之後送出 `online`）；CONNACK 的屬性改回可以省略：單元測試失敗，link 測試看到 `online`；拿掉拒絕時 Session Present 的檢查：單元測試失敗，link 測試只看到拒絕、沒有 DISCONNECT；CONNACK 加回三個屬性：單元測試失敗，link 測試看到 `online`；拿掉 Response Topic 的檢查：單元測試失敗，link 測試等滿 60 秒沒有斷線；PUBACK 改回不檢查 ID 0：單元測試失敗，link 測試的錯誤是協定錯誤而不是格式錯誤；SUBACK 改回不檢查 ID 0：單元測試失敗；拿掉 `connection` 對 clean start 之後 Session Present 的檢查：link 測試看到 `online`；讓 AUTH 通過：單元測試與 link 測試失敗 |
| 停止時正在進行的寫入不受停止期限限制：寫入最多等 keepalive 的 1.5 倍（預設 90 秒），SIGTERM 不會打斷它，連線自己的停止超過 5 秒的期限，要靠 `main` 的 10 秒（`STOP_LIMIT`）結束行程（提交前的 Codex 審查；它說會超過 systemd 的 20 秒 `TimeoutStopSec`，這點不成立：`main` 在 10 秒後不再等連線，行程照樣結束，broker 因連線沒有 DISCONNECT 而發布 will） | `write` 也等著 shutdown：停止開始後（寫入前或寫入中），寫入最多只到停止的期限（從 SIGTERM 起 5 秒）；寫入中才看到 shutdown 時由它開始停止，與主迴圈相同。期限到了還沒寫完就回錯誤，連線不送 DISCONNECT 直接關閉：封包只寫了一半，之後送的任何位元組都會被 broker 當成那個封包的後半，broker 因連線沒有 DISCONNECT 而發布 will。掃同類問題：停止開始於 CONNECT 寫入期間時，等 CONNACK 沿用同一個期限，不再從寫完才起算；其他等待（CONNACK、`offline` 的 PUBACK、收到的訊息等佇列、關閉的 2 秒、supervisor 的連線與退避）原本就以停止期限或 shutdown 為界。TLS 握手中的 TPM 簽章是同步呼叫，最多佔住連線執行緒 10 秒，這段時間停止只能靠 `main` 的 `STOP_LIMIT`（還沒有連線，沒有在線狀態要收尾）。README 的 SIGTERM 段寫明 | `tests/link.rs`：`a_stop_cuts_a_stuck_write_at_the_stop_deadline_and_sends_nothing_after_it`（1 MiB 的心跳卡在 broker 不讀的連線上時停止：supervisor 在 5 秒結束，不是寫入逾時的 90 秒；呼叫端拿到 `ConnectionLost`；broker 沒讀的位元組是心跳 PUBLISH 的前段，結尾全是 payload 的 0，沒有 DISCONNECT）、`a_stop_during_the_connect_write_keeps_one_deadline_for_connack`（連線只有 16 bytes 的緩衝，CONNECT 寫到一半時停止，broker 3 秒後才讀，之後不回 CONNACK：停止在 5 秒結束，不是 8 秒） | 寫入不看停止期限：第一個測試到寫入逾時（約 90 秒）才結束；寫入中不理會 shutdown：同樣到寫入逾時，第二個測試在 8 秒才結束；等 CONNACK 時另起期限：第二個測試在 8 秒才結束 |
| agent 自己的封包不檢查 broker 的 Maximum Packet Size：`online`／`offline`、SUBSCRIBE、PUBACK、PINGREQ 與 DISCONNECT 照送（MQTT-3.2.2-15 不准；只有應用程式的訊息有檢查）。原本的上限測試宣告 100 bytes，`online` 是 140 bytes，agent 照送而沒有發現（提交前的 Codex 審查） | `write` 寫出任何封包之前比對 broker 的 Maximum Packet Size：應用程式的訊息在 `send_request` 已先檢查、單獨回 `PublishError::TooLarge`，所以到這裡還放不下的都是連線需要的封包：不送，以 DISCONNECT 0x04 結束連線（停在封包邊界，可以送），回新的 `LinkError::PacketTooLarge`，照退避重試。`close` 在 DISCONNECT 本身放不下時不送，直接關閉（broker 一樣發布 will）。掃同類問題：連線送出的每個封包都經過 `write` 或 `close`（CONNECT 在 CONNACK 之前，還沒有上限）。順便更正 `LinkError::Protocol` 的文件（現在送的是 0x04）。上限測試改成宣告 1000 bytes、以 2000 bytes 的遙測測拒絕 | `tests/link.rs` 的 `packets_above_the_brokers_maximum_packet_size_are_never_sent`：CONNACK 宣告 50 bytes（放得下 3 bytes 的 DISCONNECT，放不下 `online`）：broker 只收到 DISCONNECT 0x04；宣告 2 bytes：什麼都沒收到就關閉；兩次的錯誤都寫明上限 | 拿掉 `write` 的檢查：broker 收到 `online`；`close` 不看上限：2 bytes 時 broker 收到 DISCONNECT |
| 建立連線的過程中 `publish` 照樣收下請求：等 CONNACK、`online` 的 PUBACK 或 SUBACK 時收下的請求，等連線好了才送出，與文件寫的「沒有連線時立刻回 `NotConnected`、不緩衝」不符；佇列又是各條連線共用，斷線時留在佇列裡的請求會在下一條連線送出（提交前的 Codex 審查） | 佇列改成每條連線各一組：連線在 SUBACK 同意全部訂閱、變成 `Connected` 時才把自己的佇列交給 `Link`，結束時收回，並回答佇列裡還沒寫出去的每個請求（`NotConnected`；link 停止時 `Stopped`），所以請求不可能帶到下一條連線或下一次建立連線的過程。`publish`、`reserve`、`PublishPermit::publish` 只用當下那條連線的佇列，`PublishPermit` 屬於拿到位置的那條連線。新增 `Link::publish_in_session`，只送到指定 session 的連線；心跳帶著自己的 session，改用它。已經交給連線寫出的請求，supervisor 整個被丟掉時也回 `ConnectionLost`（回覆的通道在 drop 時回答）。取捨：比在請求上標 session 的改法動得多，但請求跨連線在結構上就不會發生 | `tests/link.rs`：`publishes_during_connect_and_setup_fail_at_once_and_are_never_sent_later`（等 CONNACK、`online` 的 PUBACK、SUBACK 時，兩種 QoS 的 `publish` 與 `reserve` 都立刻回 `NotConnected`；連上後 broker 收到的第一則是之後才送的訊息）、`requests_still_queued_when_a_connection_ends_are_answered_and_never_sent_later`（兩種 QoS 的佇列、寫到一半的心跳、拿到的位置）、`a_message_for_an_ended_session_is_never_sent_on_the_next_connection`、`a_permit_belongs_to_the_connection_it_was_reserved_on`、`when_the_supervisor_goes_away_written_publishes_are_lost_and_queued_ones_stopped` | 連線一開始就交出佇列：等 CONNACK 時的 publish 沒有立刻失敗；結束時改回 `ConnectionLost`、或不回答留在佇列的請求：佇列測試失敗或逾時；`publish_in_session` 不看 session、停止後 `publish`／`reserve` 或 permit 不拒絕、回覆的通道 drop 時不回答：各自的測試失敗 |
| 合約稽核（2026-10-23）：README、testkit README 的「MQTT 測試環境」與 `Link`、`PublishPermit`、`LinkState`、`LinkError`、`PublishError` 的文件寫的每一項保證，逐一對照程式在連線中、建立連線、`Connected`、停止中、停止後的行為，並找出證明它的測試（提交前的 Codex 審查要求，免得每次審查只找到一項） | 程式不符的一項：收到的訊息等著放進佇列（不讀 socket）時 PINGRESP 的期限照跑，應用程式約 90 秒沒取走訊息，連線就以 `KeepAlive` 結束，與 README「這期間不判定逾時」不符；PUBACK 的期限雖然暫停，恢復讀取的那一刻卻已經過期。改成：這期間照樣送一次 PINGREQ（broker 要在 keepalive 內收到封包），但不等 PINGRESP；PUBACK、SUBACK、PINGRESP 的期限從恢復讀取的時刻重新起算。文件不符的：README 的 PUBLISH 一條（心跳用 `publish_in_session`；建立連線的過程中立刻回 `NotConnected`；佇列屬於一條連線）、keepalive 一條、記憶體表，並補上「開連線與等 CONNACK 各最多 30 秒」；`CONNECT_TIMEOUT` 與 `LinkSettings::connect_timeout` 寫明也限制 CONNACK；`Link::reserve` 寫明停止時等待以 `Stopped` 結束。其餘保證程式本來就照做，沒有測試證明的都補上（右欄）。不在範圍：Modbus（Laysi/trustiot-v2#87，已在 main）與 TPM（`630021f`）不在這次的變更裡；`LinkError::Internal` 只在程式錯誤時出現，測試觸發不了；心跳在舊 session 結束、新連線剛好上來時的競態無法以決定性的測試重現，由 `publish_in_session` 本身的測試涵蓋 | `tests/link.rs`：`waiting_for_the_application_suspends_the_puback_and_pingresp_deadlines`（十分鐘不取訊息，連線仍在、只送一次 PINGREQ；恢復後 broker 20 秒才回 PINGRESP、100 秒才回 PUBACK，都不算逾時）、`a_refused_online_status_ends_the_connection_and_the_link_tries_again`（不送 DISCONNECT 直接關閉，照退避重連）、`a_suback_that_never_comes_ends_the_connection_with_the_will`（120 秒後 DISCONNECT 0x04）、`a_subscription_refused_during_a_stop_still_ends_with_a_normal_disconnect`、`a_write_the_broker_does_not_take_ends_the_connection`（keepalive 60 秒時 90 秒、4 秒時 10 秒；不送 DISCONNECT，`ConnectionLost`）、`incoming_ends_once_the_supervisor_has_stopped_and_every_message_was_taken`、`connection_attempts_and_connack_are_bounded_by_the_connect_timeout`（另含沒有 DISCONNECT 就關閉的錯誤）、`pings_start_when_the_broker_is_quiet_even_while_the_agent_keeps_sending`、`puback_codes_decide_the_outcome_and_a_refusal_carries_the_brokers_reason`（0x10 是成功；拒絕帶著 reason string，錯誤訊息最多 200 字）、`a_heartbeat_that_finds_the_queue_full_is_skipped_not_delayed`；`reason_strings_from_the_broker_lose_their_control_characters` 加上 200 字的上限，`a_refused_subscription_ends_the_connection_and_never_reports_connected` 加上 `LinkState::Disconnected` 的 `retry_in` 就是實際等的退避，佇列與停止的測試加上等位置的 `reserve`（連線結束時 `NotConnected`、停止時 `Stopped`）。`transport.rs`：每個解析出的位址各 10 秒（TCP 的迴圈抽成 `first_connection`，以停住的時鐘測）、送出的 WebSocket message 最多 32 KiB、回答 ping、text message 是錯誤、close 是結束。`main.rs`：連線執行緒的計數沒有前進時不送 `WATCHDOG=1`（這個測試後來移到 `src/liveness.rs`，見「watchdog 只證明連線執行緒在跑」一列）。`tests/binary.rs`：`STATUS=` 寫出失敗的步驟（TCP）。testkit 的 `WsFront`：路徑不對回 404、沒要求 `mqtt` 回 400、`ping_interval` 會送 ping、雙向轉送 | 29 項各改回一次（上面的程式修正拆成四處各改回一次），對應的測試都失敗：例如等著交出訊息時照跑 PINGRESP 的期限，十分鐘內連線就斷了；恢復讀取時不重新起算，broker 在期限內回的 PINGRESP 或 PUBACK 之前連線就已結束 |
| 寫入的逾時不看 broker 的 Server Keep Alive：一直用 agent 設定的 keepalive（60 秒，逾時 90 秒），broker 在 CONNACK 給了較短的值（例如 10 秒，它 15 秒沒收到封包就斷線）時，卡住的寫入仍等到 90 秒（提交前的 Codex 審查，2026-10-23） | 寫入的逾時改用生效中的 keepalive（CONNACK 的 Server Keep Alive 不是 0 時是它），仍是 1.5 倍、至少 10 秒。掃同類問題：PINGREQ、PINGRESP、PUBACK 與 SUBACK 的期限本來就用生效中的值，只有 CONNECT 送的是設定的 keepalive（本來就該如此）；README 的 keepalive 一條寫明這一點 | `tests/link.rs` 的 `a_write_the_broker_does_not_take_ends_the_connection` 加一種情況：agent 設定 60 秒、CONNACK 的 Server Keep Alive 20 秒，寫入在 30 秒放棄 | 改回設定的 keepalive：這種情況等到 90 秒，測試失敗 |
| watchdog 只證明連線執行緒在跑：計數由連線 runtime 上一個獨立的計時工作每秒加一，supervisor 卡在一個不會結束的 await 時，主 runtime 照樣每 15 秒送 `WATCHDOG=1`，systemd 不會重啟卡住的連線（提交前的 Codex 審查，2026-10-23） | 依協調者定的設計改成以期限判斷（不照迴圈的次數餵：沒有網路的現場退避 60 秒是正常的等待，而 `WatchdogSec` 是 30 秒）。連線的工作在每個等待開始前，向 `liveness::Progress` 宣告這一步與最晚結束的時刻：開連線是 `connect_timeout` 加 `tpm::RESPONSE_TIMEOUT`（TPM 簽章佔住執行緒時逾時會晚到）；寫入是寫入的逾時；等 CONNACK；主迴圈是它下一個計時器，停止中是停止的期限，等應用程式取走訊息是從開始等起 5 分鐘（`DELIVERY_LIMIT`，中間的寫入不延長）；退避是延遲；關閉 2 秒。`liveness::watchdog`（取代 `main.rs` 原本的函式）只在宣告的等待沒有超過最晚時刻加 `SLACK`（10 秒）、而且連線執行緒的計數有前進時送 `WATCHDOG=1`；超過時不送，並以 `STATUS=` 寫出卡在哪一步、已經多久。逐一檢查連線路徑上的每個 await（supervisor 的開連線與退避、CONNACK、主迴圈的 select、`write`、`close`），都在宣告過的等待裡。取捨：等應用程式取走訊息原本沒有上限，現在超過 5 分鐘算卡住，`Incoming` 的文件與 README 寫明；關閉的 2 秒小於 `SLACK`，沒有單獨的測試 | `src/liveness.rs`：60 秒的退避期間一直有送；超過期限加 SLACK 就不送並寫出 `STATUS=`（例如 `stuck waiting for CONNACK for 45 s (allowed 40 s)`），換到期限內的等待又恢復；計數停住時不送；啟動前也有期限。`tests/liveness.rs`：真的連線路徑每一步宣告的期限（開連線 40 秒、CONNACK 30 秒、SUBACK 與連線中 60 秒、等應用程式 5 分鐘且中間的 PINGREQ 與 publish 不延長、退避 60 秒、停止 5 秒、寫入 90 秒）；端對端：broker 不回 CONNACK 時 agent 自己逾時再試，watchdog 一直有送；在同一個等待裡把 supervisor 凍住（不再 poll，等同卡在不會結束的 await），計數照常前進，從第三次起不送，`STATUS=` 寫出 `waiting for CONNACK` | 12 項各改回一次，對應的測試都失敗：只看計數、或不看宣告的期限：凍住的 supervisor 照樣有送；拿掉計數的檢查；開連線不加 TPM 的時間；退避或停止沒有期限；寫入或 CONNACK 不宣告；等應用程式隨每次寫入重算；連線中不看 keepalive 的計時器 |
| PUBACK、SUBACK 的期限從封包登記等回覆時算起，那時還沒寫出去：上傳很慢、寫入花了 80 秒時，broker 只剩 40 秒回覆，健康的連線可能被當成失敗（提交前的 Codex 審查，2026-10-23） | 封包仍在寫出之前登記（寫入失敗時才回答得到呼叫端），寫完之後把它的起點改成寫完的時刻（`State::written`）；應用程式的 QoS 1 訊息、`online`／`offline` 與 SUBSCRIBE 都這樣做。掃同類問題：PINGRESP 的期限本來就從 PINGREQ 寫完算起，CONNACK 的期限從 CONNECT 寫完算起；停止的 5 秒刻意從 SIGTERM 算起，也限制寫入（見「停止時正在進行的寫入」一列） | `tests/link.rs`：`a_slow_write_does_not_use_up_the_time_for_its_puback`（broker 80 秒不讀，第 140 秒還沒有 DISCONNECT，之後回 PUBACK 就是 `Acknowledged`）、`slow_writes_during_setup_do_not_use_up_the_time_for_their_acknowledgements`（`online` 或 SUBSCRIBE 寫了 80 秒，第 140 秒才回 PUBACK 與 SUBACK，照樣 `Connected`） | 三處各拿掉一次：第 120 秒就送 DISCONNECT 0x04，測試失敗 |
| 收到的 `down` 訊息確認之後丟掉：連線訂閱全部四個 `down` 頻道，QoS 1 的訊息一放進 `Incoming` 就回 PUBACK，但執行檔的 `drain` 只記錄就丟掉；broker 不會再送，retained 的設定、`log-request` 等就此遺失（提交前的 Codex 審查，2026-10-23） | 依協調者的決定，link 只訂閱有處理程式的頻道。訂閱明列在 `LinkSettings::subscriptions`（新的 `Subscriptions`：只能是 `down` 頻道、不能重複，預設一個都沒有）；`main.rs` 傳空的並拿掉 `drain`，程式註解與 README 寫明各頻道由哪張 issue 連同處理程式一起加上（`config` Laysi/trustiot-v2#94、`telemetry-ack` Laysi/trustiot-v2#90、`log-request` Laysi/trustiot-v2#185、`live-control` Laysi/trustiot-v2#121）。沒有訂閱時不送 SUBSCRIBE（MQTT-3.8.3-2；編碼器也拒絕沒有 topic filter 的 SUBSCRIBE），`online` 拿到 PUBACK 就 `Connected`；有訂閱時 `Connected` 改成 `online` 的 PUBACK 與 SUBACK 都到了才算（原本只看 SUBACK），兩種情況同一條規則。broker 送來沒有訂閱的頻道照舊記錄、回 PUBACK、丟掉，當成 broker 的錯誤。掃同類問題：連線路徑上用到訂閱的每一處（建立 SUBSCRIBE、SUBACK 的檢查與 reason code 的數目、建立連線這一步的期限、收到的 PUBLISH）；watchdog 的這一步改名 `SettingUp`（等 `online` 的 PUBACK 或 SUBACK），`STATUS=` 也跟著改；需要收訊息的測試（含 Mosquitto 的 1 MiB 設定與行程內的 transport 測試）改成明列自己的訂閱 | `tests/link.rs`：`without_subscriptions_no_subscribe_follows_online_and_its_puback_connects`、`connected_waits_for_both_the_puback_of_online_and_the_suback_in_either_order`、`a_message_on_a_channel_the_link_did_not_subscribe_to_is_acknowledged_and_dropped`；`tests/binary.rs` 的 `connected_the_agent_subscribes_to_nothing_reports_its_session_and_stops_on_sigterm`（`online` 之後的下一個封包就是心跳）；`link.rs` 的 `subscriptions_are_down_channels_each_once_in_order`；`packet.rs` 的 `encoders_refuse_what_mqtt_cannot_carry` 加上空的 SUBSCRIBE | 十個改動都讓對應的測試失敗。一律訂閱全部 `down` 頻道、空的也送 SUBSCRIBE、空的時寫完 `online` 就 `Connected`：空集合的測試失敗；不等 `online` 的 PUBACK、SUBACK 一到就 `Connected`（兩種寫法）：先後順序的測試失敗；不過濾沒有訂閱的頻道：丟掉的測試失敗；接受 `up` 頻道或重複的頻道：單元測試失敗；編碼器接受空的 SUBSCRIBE：編碼器的測試失敗；執行檔訂閱全部 `down` 頻道：執行檔的測試失敗 |
| 公開 repo 的資訊規則 | 本報告的環境、`systemd-analyze.txt` 與 agent README 拿掉作業系統與本機工具的版本；2026-10-13 雙層 TLS 可行性文件的伺服器端也拿掉作業系統與 podman 版本 | — | — |

`the_inflight_limit_bounds_unacknowledged_publishes` 隨之調整：第二段（回一個 PUBACK 剛好再送一則）原本再等一整個 keepalive 的 PINGREQ，那時第一批 publish 已等了兩個 keepalive，會先碰到 PUBACK 期限；改成確認之後 30 秒內沒有其他封包。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. 整合測試：agent 經 WebSocket、內層 mTLS 連到測試 broker | 通過（第 1 節） |
| 2. broker 或轉接服務重啟後自動重連（測試） | 通過（第 2 節：前端當掉與重啟、Mosquitto 重啟，各得到新的 session 與 `online`） |
| 3. 每條連線的記憶體有上限（in-flight 上限可設定，測試或量測紀錄） | 通過（第 3 節，測試）。在閘道器上量測實際 RSS：待測，屬於量測，依驗證分級延後 |
| 4. systemd unit 以非 root 執行（檢查 unit 檔） | 通過（第 4 節）。在 lab 的閘道器上以 systemd 實際執行（沙箱、watchdog、`/dev/tpmrm0` 權限）：待測，需要映像安裝與 TPM 金鑰，見下方 |
| 5. protocol 型別變更使 edge/agent 編譯失敗（重跑 run-mutations.sh 加一個 protocol 變更） | 通過（第 5 節）。2026-10-13 報告中 protocol 部分的「待測」由本報告補上 |

待測與原因：

- aarch64 執行檔在閘道器上啟動、以 systemd 執行並經真的入口與 mqtt-bridge 連線：工作站沒有 arm64 模擬器，經入口與 mqtt-bridge 的端對端測試是 Laysi/trustiot-v2#102。
- 連 TPM 金鑰的整條連線：TPM 部分由同一張 issue 的另一部分實作與測試。
