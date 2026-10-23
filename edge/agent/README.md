# agent

閘道器 agent（[決策 10](../../docs/architecture/decisions.md)）：在現場閘道器上讀取設備、把資料送上雲端的程式。目前有執行檔 `trustiot-agent`、systemd unit、到 broker 的連線（443 的 WebSocket 內走 mTLS 與 MQTT 5）、TPM 裡的身分金鑰（`tpm` 模組，[決策 12](../../docs/architecture/decisions.md)）、在線狀態與基本心跳，以及 Modbus 讀取（`modbus` 模組，[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) F1、F5）；排程與離線緩衝之後建立在它們上面。

| 在哪裡 | 內容 | issue |
| --- | --- | --- |
| `src/main.rs` | 執行檔 `trustiot-agent`：參數、日誌、兩個 runtime、sd_notify、SIGTERM | Laysi/trustiot-v2#86 |
| `src/config.rs`、`agent.example.toml` | 設定檔（TOML）與它的檢查 | Laysi/trustiot-v2#86 |
| `src/setup.rs`、`src/identity.rs` | 讀設定、憑證與金鑰，閘道器 ID 取自憑證的 CN | Laysi/trustiot-v2#86 |
| `src/tpm.rs`、`src/tpm/` | `TpmKey`：TPM 內的身分金鑰（TPM2_ReadPublic、TPM2_Sign）與 rustls 的 `SigningKey` | Laysi/trustiot-v2#86 |
| `src/tls.rs`、`src/transport.rs` | 兩層 TLS 的設定；TCP → 外層 TLS → WebSocket → 內層 mTLS | Laysi/trustiot-v2#86 |
| `src/mqtt/packet.rs`、`src/mqtt/connection.rs` | MQTT 5 編解碼（只含用到的部分）與一條連線的狀態機 | Laysi/trustiot-v2#86 |
| `src/link.rs`、`src/backoff.rs` | 對外的 `Link`（publish、收訊息、狀態）與重連退避 | Laysi/trustiot-v2#86 |
| `src/heartbeat.rs`、`src/sdnotify.rs`、`src/liveness.rs` | 基本心跳；systemd 的 `READY=1`、`WATCHDOG=1`、`STATUS=`；連線的工作是否卡住（給 watchdog 判斷） | Laysi/trustiot-v2#86 |
| `systemd/` | `trustiot-agent.service` 與帳號（sysusers.d） | Laysi/trustiot-v2#86 |
| `src/modbus.rs` | `DeviceReader`：依型號規劃讀取、送出、解碼並蓋時間戳；`Connection`、`Clock` 介面 | Laysi/trustiot-v2#87 |
| `src/modbus/plan.rs` | 合併讀取的規劃 | Laysi/trustiot-v2#87 |
| `src/modbus/pdu.rs` | 功能碼 1–4 的請求與回應 PDU | Laysi/trustiot-v2#87 |
| `src/modbus/decode.rs` | 型別、位元組順序、scale 與 offset | Laysi/trustiot-v2#87 |
| `src/modbus/tcp.rs` | Modbus TCP 的 `Connection`（MBAP） | Laysi/trustiot-v2#87 |
| 還沒有 | RTU-over-TCP 與同一台轉換器共用連線 | Laysi/trustiot-v2#88 |
| 還沒有 | 固定節拍、重連退避、TCP keepalive、watchdog（Modbus 讀取） | Laysi/trustiot-v2#89 |
| 還沒有 | 離線緩衝、端對端確認與重送；系統時鐘的 `Clock`（時鐘是否已校正、校正來源） | Laysi/trustiot-v2#90 |
| 還沒有 | 讀取統計（`DeviceStats`）與心跳的其餘指標（CPU、磁碟、溫度、緩衝） | Laysi/trustiot-v2#117 |
| 還沒有 | 套用雲端送來的設定 | Laysi/trustiot-v2#94 |
| 還沒有 | 憑證換發 | Laysi/trustiot-v2#83 |

依賴都要能建置成 aarch64（`aarch64-unknown-linux-gnu`，glibc，不是 musl：[記憶體測試](../../docs/feasibility/2026-10-13-gateway-memory.md)），CI 對 `edge/` 下每個成員做這項建置。唯一編譯 C 的相依是 `ring`（rustls 的加密實作），交叉建置時要有目標的 C 編譯器，用 `.prototools` 鎖定的 zig 與 cargo-zigbuild：

```sh
proto install zig && proto install cargo-zigbuild
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
```

`tests/dependencies.rs` 確認閘道器建置裡沒有公開根憑證庫（`webpki-roots`、`rustls-native-certs`）、OpenSSL、aws-lc 與 TPM 的 C 函式庫，而且只有 `ring` 編譯原生程式碼。

## 執行檔與設定

```text
trustiot-agent [--config <file>] [--check]
```

設定檔預設是 `/etc/trustiot/agent.toml`，格式與每個鍵的說明見 [`agent.example.toml`](agent.example.toml)。未知的鍵、相對路徑、超出範圍的值都拒絕啟動，錯誤訊息指出是哪個鍵：

| 鍵 | 內容 |
| --- | --- |
| `cloud.url` | 雲端的閘道器入口：`wss://<我們的網域>[:port]/mqtt`。只接受 `wss`、DNS 名稱（不接受 IP，也不接受最後一段是數字的名稱，例如 `127.1`、`0x7f000001`：解析器會把它們當成 IPv4 位址）、路徑 `/mqtt`，不能帶查詢字串與帳密。寫了 port 就必須是 1–65535 的數字（`:`、`:0`、`:65536`、`:abc`、`:+443` 都拒絕），沒寫才是 443 |
| `cloud.trust` | 外層 TLS 信任的 PEM 檔，至少一個，每個檔至少要有一張可用的憑證。SaaS 用作業系統的 CA bundle；地端用客戶的 CA 或我們的 CA；工廠防火牆會解開 443 時，把它的攔截 CA 加成另一個檔 |
| `broker.name`、`broker.ca` | 內層 mTLS 的 broker 名稱與我們的 CA；內層只信任這個 CA。`broker.name` 不可與 `cloud.url` 的主機相同：兩層驗證的是不同的伺服器 |
| `identity.certificate` | 閘道器的憑證鏈（leaf 在前）。閘道器 ID 取自 leaf 的 CN（必須是合法的 `GatewayId`），不另外設定，兩者不可能不一致 |
| `identity.key` | 正式環境是 `{ source = "tpm", device = "/dev/tpmrm0", handle = 0x81000100 }`（handle 要在 persistent 範圍 0x81000000–0x81FFFFFF）；`{ source = "file", path = "..." }` 只給開發與測試，啟動時會記一筆警告 |
| `connection.*` | 心跳間隔（10–3600 秒，預設 60）、重連退避的起始與上限（預設 1 秒、60 秒）、同時等 PUBACK 的 QoS 1 訊息數（1–256，預設 32） |

啟動時讀設定、建好兩層 TLS 的設定、讀憑證並確認金鑰與憑證的公鑰相同（TPM 的金鑰也一樣）；有 EKU 時必須含 clientAuth，有 key usage 時必須含 digitalSignature。接著以 webpki 的用戶端憑證檢查（rustls 的 `WebPkiClientVerifier`）確認憑證鏈接到 `broker.ca`，broker 一定會拒絕的憑證（例如別的 CA 簽的）就不啟動。只有時間不對（過期或還沒生效）時記一筆警告，寫出憑證的 notBefore 與 notAfter，照樣啟動：閘道器沒有 RTC，時鐘可能是錯的，不啟動就沒有機會校正；真的過期時 broker 會拒絕，由換發（Laysi/trustiot-v2#83）處理。全部通過才通知 systemd `READY=1`，不等第一次連線：沒有網路的現場也要能啟動，之後自己重試。`--check` 做完這些檢查就結束（0 是可用並印出閘道器 ID 與憑證的到期時間，1 是不可用並印出原因），給維運與更新後的健康檢查用。設備的設定（`DesiredConfig`）之後由 MQTT 送來（Laysi/trustiot-v2#94），不在這個檔案裡。

日誌寫到 stderr，由 systemd 交給 journal；等級由 `TRUSTIOT_AGENT_LOG` 設定（預設 `info`）。

執行緒：連線（TLS 握手與 MQTT）在自己的執行緒、單執行緒的 tokio runtime 上跑，因為完整的內層握手要在 rustls 的同步簽章呼叫裡等 TPM 約 70 ms（[TPM 測試](../../docs/feasibility/2026-10-12-tpm-poc.md)），這樣只卡住這條執行緒；心跳、watchdog、訊號與之後的 Modbus 讀取在另一個兩條執行緒的 runtime 上。

## 連線（WebSocket 內的 mTLS、MQTT 5）

只有一種連線方式，SaaS 與地端相同（[決策 6](../../docs/architecture/decisions.md)、[部署](../../docs/architecture/deployment.md)）：

1. TCP（`TCP_NODELAY`，每個解析出的位址各等 10 秒），連到 `cloud.url` 的主機。開 TCP keepalive：閒置 60 秒後每 15 秒探一次，4 次沒回應就由核心斷線，是 MQTT keepalive 之外的保險。
2. 外層 TLS：SNI 是 URL 的主機，ALPN `http/1.1`，信任 `cloud.trust` 的檔案；不用 `webpki-roots` 也不讀作業系統的憑證庫。
3. WebSocket：`GET /mqtt`，subprotocol `mqtt`，只送收 binary message；對方沒回 `mqtt` subprotocol 就失敗。每則送出的 message 最多 32 KiB（一筆內層 TLS record 放得下），收到的 message 與 frame 最多 64 KiB，讀取緩衝 16 KiB。text message 視為協定錯誤；close 視為連線結束。
4. 內層 mTLS：broker 名稱是 `broker.name`，只信任 `broker.ca`，用閘道器的憑證與金鑰（TPM）。
5. MQTT 5。

開一條連線（上面 1–4 步）最多 30 秒（`CONNECT_TIMEOUT`），之後等 CONNACK 另外最多 30 秒；逾時都算連線失敗，照退避重試。

兩層都只用 TLS 1.3，套件順序是 ChaCha20-Poly1305、AES-256-GCM、AES-128-GCM（閘道器的 CPU 沒有 AES 指令，rustls 預設會選 AES-256-GCM），都開 session resumption（每層各記 8 個）：resumption 不需要 TPM 簽章，重連省下約 70 ms。加密實作只用 `ring`。Mosquitto 依自己的套件順序選，不依用戶端的順序，所以 broker 的 mTLS listener 要以 `ciphers_tls1.3` 把 `TLS_CHACHA20_POLY1305_SHA256` 排第一，否則協商出 AES-256-GCM（2026-10-21 實測，[測試報告](../../docs/test-reports/2026-10-21-86-agent-mqtt-connection.md)）。

### MQTT 用戶端

MQTT 5 的編解碼由本 crate 自己寫，只做用到的部分（2026-10-21）：rumqttc 只能跑在它自己的 TCP、TLS、WebSocket 上，放不進「WebSocket 裡的 TLS」，而用本機 loopback listener 繞過會多一個攻擊面；可以跑在任意串流上的 crate 都不合適：`mqtt5-protocol` 從 2025-11 到 2026-10 發了 33 版、依賴作者自己的 `bebytes`，`mqtt-protocol-core` 約 4 萬行、6 個 proc-macro 相依，`mqttbytes` 最後一版在 2021 年。`src/mqtt/packet.rs` 是不做 I/O 的編解碼，`src/mqtt/connection.rs` 是一條連線的狀態機，`src/link.rs` 是對外的介面與重連。

- **CONNECT**：client ID 是憑證的 CN（= 閘道器 ID = Mosquitto 的 username），不送帳號密碼；keepalive 60 秒；will 見下方「在線狀態」；Receive Maximum 16、Maximum Packet Size `MIN_MAX_PACKET_BYTES`（1 MiB + 8 KiB）。CONNACK 的 Receive Maximum、Maximum Packet Size 照辦，Server Keep Alive 不是 0 時照辦；是 0 時（broker 不檢查連線）agent 仍用自己的 60 秒，否則連線無聲斷掉時不會發現；reason code ≥ 0x80 是拒絕（例如 0x87），記錄後退避重連。
- **Session**：每次連線都是 Clean Start 1、Session Expiry 0，broker 不替離線的閘道器保留 session 與排隊的訊息。理由：MQTT 5 規定接續 session 時用戶端要以原本的 packet ID 重送沒有 PUBACK 的 PUBLISH，那需要跨連線保留訊息內容，屬於離線緩衝（Laysi/trustiot-v2#90）；而且每次連線本來就有新的在線 session。影響：設定以 retained 訊息在訂閱時送到；遙測確認遺失時由 Laysi/trustiot-v2#90 的重送補上；閘道器離線時雲端送的 `log-request`、`live-control` 會遺失，雲端要在閘道器重新上線（`online`）後再送。
- **PUBLISH**：`Link::publish(channel, payload)` 只接受 `up` 的頻道，`status` 保留給 agent 自己；超過頻道上限或 broker 的 Maximum Packet Size 時直接回錯誤。QoS 與 retain 依 `Channel::delivery()`。`publish` 從不等佇列的位置：該 QoS 的佇列滿了就立刻回 `PublishError::Busy`，什麼都沒排進去；要等位置的呼叫端先以 `Link::reserve(channel).await` 拿 `PublishPermit`（等待時不帶 payload，依要求的先後拿到位置），再 `permit.publish(payload)`。心跳用 `Link::publish_in_session(session, ...)`：只送到該 session 的連線，那條連線結束後就回 `NotConnected`，不會帶著舊的 session 送上下一條連線；遇到 `Busy` 就跳過這一次，下一次照原本的間隔（連續跳過時只有第一次記警告）。QoS 1 等到 PUBACK 才回來：只有 0x00 與 0x10（no matching subscribers）是 `Acknowledged`；PUBACK 允許的其他碼（都 ≥ 0x80，例如 ACL 不允許的 0x87）是 `PublishError::Refused`，呼叫端要當成沒送出（[protocol README](../../crates/protocol/README.md)「傳輸：MQTT 5」）；MQTT 5 不允許出現在 PUBACK 的碼（例如 0x01、0x7F）是協定錯誤（見下方），呼叫端拿到 `ConnectionLost`，絕不算送達。連線在 PUBACK 之前斷掉是 `ConnectionLost`（可能送到也可能沒有），正常停止時也一樣。只有 `Connected` 的連線收 publish：連線中、建立連線的過程（等 CONNACK、`online` 的 PUBACK、SUBACK）與等著重連時，`publish` 與 `reserve` 立刻回 `NotConnected`，停止中與停止後立刻回 `Stopped`，都不排隊，緩衝是 Laysi/trustiot-v2#90 的事。佇列屬於一條連線：連線變成 `Connected` 時建立，連線結束時，還在佇列裡、沒有寫出去的請求（包括等位置的 `reserve` 與拿到但還沒用的 `PublishPermit`）都回 `NotConnected`（link 停止時是 `Stopped`），絕不會在下一條連線送出。
- **訂閱**：link 只訂閱有人處理的 `down` 頻道，訂閱哪些明列在 `LinkSettings::subscriptions`（`Subscriptions`：只能是 `down` 頻道、不能重複，照給的順序，各以自己的 topic 訂閱，QoS 1）。理由：QoS 1 的訊息一放進 `Incoming` 的佇列就回 PUBACK，broker 從此不會再送；訂閱了卻沒有人取走的頻道，訊息等於確認之後丟掉（例如 retained 的設定）。目前 agent 還沒有任何頻道的處理程式，所以 `main.rs` 一個都不訂閱：連上後不送 SUBSCRIBE（MQTT 5 不准沒有 topic filter 的 SUBSCRIBE，MQTT-3.8.3-2；編碼時也拒絕），`online` 拿到 PUBACK 就回報 `Connected`。之後的 issue 寫處理程式時，同一個變更把它的頻道加進 `main.rs` 的訂閱：`config`（Laysi/trustiot-v2#94）、`telemetry-ack`（Laysi/trustiot-v2#90）、`log-request`（Laysi/trustiot-v2#185）、`live-control`（Laysi/trustiot-v2#121）。有訂閱時，連上後先送 `online`、再送 SUBSCRIBE；`online` 拿到 PUBACK、SUBACK 也以 QoS 1 同意全部的訂閱（兩者先後不拘）之後才回報 `Connected`（`LinkState`、`STATUS=`；心跳與退避歸零的 60 秒都從這時算）。任何一個被拒（reason code ≥ 0x80，例如角色沒有該頻道時的 0x87）就記下 topic 與 reason code、送 DISCONNECT 0x04（broker 發布 will，雲端在 `online` 之後看到 `connection_lost`；見下方「連線結束與 will」），照退避重連（`LinkError::SubscriptionRefused`）：收不到設定或請求的閘道器不算連上。只給 QoS 0（0x00）的訂閱也一樣處理（`LinkError::SubscriptionDowngraded`）：QoS 0 的訊息不受 agent 的 Receive Maximum 管，agent 的佇列滿時 broker 可能直接丟掉。停止中才收到的拒絕不影響正常的停止。收到的訊息經 `Incoming::recv()` 交給應用程式，應用程式要及時取走：一則訊息等超過 5 分鐘（`DELIVERY_LIMIT`）算 agent 卡住，systemd 的 watchdog 會重啟它（見下方「systemd」）；topic 不是自己的 `down` 頻道、是沒有訂閱的頻道（broker 的錯誤：它不該送沒有訂閱的 topic）或超過頻道上限的，丟掉並記錄，仍回 PUBACK；QoS 1 的訊息放進佇列之後才回 PUBACK。
- **keepalive**：這一條的 keepalive 都是生效中的值：broker 在 CONNACK 給了非 0 的 Server Keep Alive 時是它，否則是 agent 自己的 60 秒。任一方向 keepalive 時間內沒有封包就送 PINGREQ，keepalive 的一半之內沒收到 PINGRESP 就斷線重連；寫入超過 keepalive 的 1.5 倍（至少 10 秒）也斷線。PUBACK 或 SUBACK 在封包寫完之後超過 keepalive 的兩倍（預設 120 秒）沒來也算連線失敗（從寫完算起：上傳很慢時寫入本身花的時間不算在 broker 回覆的時間裡）：送 DISCONNECT 0x04（broker 發布 will）後重連，等著的呼叫端拿到 `ConnectionLost`；否則只回 PINGRESP 不回 PUBACK 的 broker 會讓 in-flight 一直滿著。收到的訊息等著放進佇列（這時不讀 socket）的期間不判定 PUBACK、SUBACK 與 PINGRESP 的逾時，因為讀不到它們：這期間照樣在 keepalive 到時送一次 PINGREQ（broker 要在 keepalive 內收到封包），恢復讀取之後，這三個期限從恢復的時刻重新起算。
- **協定錯誤**：broker 送來壞掉或不該出現的封包時，在日誌記下 MQTT 5 對應的 reason code，再送 DISCONNECT 0x04 並關閉（broker 才會發布 will，見下方「連線結束與 will」）：格式錯誤 0x81（例如可變長度整數沒有用最短的編碼、固定標頭的旗標不對、字串不是合法的 UTF-8 或含 U+0000、topic 或 Response Topic 有萬用字元、packet ID 0、CONNACK 沒有屬性長度、欄位填不滿或超出 Remaining Length）、協定錯誤 0x82（例如第二個 CONNACK、clean start 之後或拒絕時 Session Present 為 1、QoS 2、對不上的 SUBACK 或 PUBACK：packet ID 不在等回覆的清單裡，包括重複的 PUBACK；沒有 PINGREQ 的 PINGRESP；該封包不允許的屬性、重複的屬性、超出範圍的屬性值，包括 agent 沒有要求的 Authentication Method、Response Information；AUTH 封包）、topic alias 0x94（沒有宣告支援）、超過 Maximum Packet Size 0x95（看到標頭就拒絕，不先配置）。reason code 不在 MQTT 5 給該封包的集合裡（CONNACK、PUBACK、SUBACK 的每一個、broker 的 DISCONNECT）也是協定錯誤 0x82；成功只認明列的碼：CONNACK 0x00、PUBACK 0x00 與 0x10、SUBACK 0x01（0x00 只給 QoS 0，當成訂閱失敗，見上；0x02 超過要求的 QoS 1，協定錯誤），從不以「小於 0x80」判斷。CONNACK 接受了 QoS 1、retained 的 will，卻宣告 Maximum QoS 0 或 Retain Available 0，也是協定錯誤 0x82（MQTT 5 要求這樣的 broker 拒絕 CONNECT），不送 `online`。MQTT 3.1.1 的 broker 以 3.1.1 的 return code（例如 0x01）拒絕時，錯誤訊息直接說 broker 不支援 MQTT 5。broker 送 DISCONNECT 時記下它的 reason code 與說明後重連。broker 送來的說明（reason string）解碼時就把控制字元（換行、tab、跳脫序列等）換成空白，記錄時最多 200 字，不能偽造或拆開日誌的行。UTF-8 字串裡的控制字元與 non-character 在 MQTT 5 只是「不應」出現，解碼時照常接受（reason string 如上換成空白）；Payload Format Indicator 為 1 時也不檢查 payload 是否為 UTF-8（MQTT 5 讓接收端決定，down 頻道都是 CBOR，由 `protocol` 解碼）。
- **broker 的 Maximum Packet Size**：MQTT 5 不准用戶端送超過 broker 在 CONNACK 宣告的 Maximum Packet Size 的封包（MQTT-3.2.2-15）。應用程式的訊息超過時 `publish` 回 `PublishError::TooLarge`，連線不受影響；agent 自己需要的封包（`online`／`offline`、SUBSCRIBE、PUBACK、PINGREQ）放不下時不送，以 DISCONNECT 0x04 結束連線（連 DISCONNECT 都放不下時不送，直接關閉；兩種 broker 都會發布 will），照退避重試（`LinkError::PacketTooLarge`）。
- **連線結束與 will**：只有正常停止（`offline`／`shutdown` 拿到 PUBACK）送 DISCONNECT 0x00，讓 broker 丟掉 will；其他任何結束都要讓 will 發布，否則 retained 的 `online` 會留在 broker 上，雲端以為閘道器還連著。MQTT 5 的規格寫 0x00 以外的結束都發布 will，但 Mosquitto 只在收到 DISCONNECT 0x04、或連線沒有 DISCONNECT 就斷掉時發布；0x80、0x81、0x82、0x83、0x94、0x95 都會讓它丟掉 will（2.1.2 實測，`tests/broker.rs` 的 `which_ends_make_mosquitto_publish_the_will`）。所以 agent 對每一種失敗（協定錯誤、訂閱被拒或只給 QoS 0、等不到 PUBACK 或 SUBACK、停止時等不到 `offline` 的 PUBACK）都送 0x04，MQTT 5 對應的 reason code 只寫進日誌；寫不出去時（連線已斷、PINGRESP 沒來、`online` 被拒）不送 DISCONNECT，直接關閉。

### 在線狀態與心跳

依 [protocol README](../../crates/protocol/README.md) 的在線狀態規則：

- 每次連線產生新的 `SessionId`；CONNECT 的 will 是這個 session 的 `Status::last_will`（`offline`／`connection_lost`），QoS 1、retained，發在 `up/status`。
- CONNACK 之後第一則訊息是同一個 session 的 `online`（QoS 1、retained）；它被拒絕（例如角色少了 `up/status`）時斷線重連，雲端不會以為閘道器在線。
- SIGTERM：送 `offline`／`shutdown`，等它的 PUBACK 最多 5 秒，再送 DISCONNECT 0x00，broker 丟掉 will；等不到 PUBACK 就送 DISCONNECT 0x04（請 broker 發布 will），雲端看到的是 `connection_lost`。這 5 秒從收到 SIGTERM 算起，也限制正在進行的寫入（例如上傳很慢時的大訊息）：期限到了還沒寫完就切斷；封包只寫了一半，之後不能再送任何封包（broker 會把 DISCONNECT 當成那個封包的後半），所以不送 DISCONNECT 直接關閉，broker 同樣發布 will。連線自己的停止最多約 7 秒（5 秒加上送 DISCONNECT 與關閉的 2 秒）；`main` 最多等 10 秒（`STOP_LIMIT`）就結束行程，unit 的 `TimeoutStopSec` 是 20 秒。正在等 CONNACK 時收到 SIGTERM，在同樣的 5 秒內繼續等：CONNACK 來了就照上面的步驟（先 `online`，再 `offline`／`shutdown` 與 DISCONNECT 0x00，不訂閱）；沒來就直接關閉。
- 心跳：每次連上馬上送一次，之後每 `heartbeat_interval_s` 送一次（QoS 0），帶這次連線的 session。目前有的欄位：agent 版本與執行時間、作業系統的開機時間與 uptime、記憶體（`/proc/meminfo`）、用戶端憑證的到期時間；設備統計、緩衝、CPU、磁碟、溫度、降頻與開機次數是 Laysi/trustiot-v2#117 的，現在是空的或 0；時鐘是否校正是 Laysi/trustiot-v2#90 的，現在是 `false`。

### 重連

連線失敗或中斷後，以 full jitter 退避：第 n 次失敗等 0.1 秒到 min(上限, 起始 × 2^(n-1)) 之間的隨機時間（預設起始 1 秒、上限 60 秒），永遠不放棄。連線（從回報 `Connected` 算起）維持 60 秒以上才把退避歸零，所以一連上就被踢的情況不會變成每秒重連（`tests/link.rs` 以固定抽最長延遲驗證：連上就斷的連線延遲照樣加倍，維持 60 秒的才回到起始值）。哪一步失敗（解析、TCP、外層 TLS、WebSocket、內層 mTLS、CONNACK）記在日誌與 `systemctl status` 的 `STATUS=` 裡。

### 記憶體上限

每條連線的記憶體有上限，不靠 systemd 的 `MemoryMax`（閘道器上沒有，[記憶體測試](../../docs/feasibility/2026-10-13-gateway-memory.md)）。link 持有的只有下表各項；等位置的呼叫端在 link 裡不持有任何 payload：`publish` 在佇列滿時第一次 poll 就回 `Busy`（payload 隨即丟掉，不排隊），`reserve` 的等待者只占一個等待的位置、還沒有訊息。呼叫端自己留著的資料（例如之後的離線緩衝，Laysi/trustiot-v2#90）由各自的上限管。

| 項目 | 上限 |
| --- | --- |
| 等 PUBACK 的 QoS 1 訊息 | `inflight_limit`（預設 32，broker 的 Receive Maximum 較小時取它）；只記 packet ID、送出時間與回覆的通道，不留訊息內容 |
| 等著送出的 publish | 目前連線的 QoS 0 與 QoS 1 各一個佇列，各 8 則（`REQUEST_QUEUE`；佇列隨連線建立與結束，任何時候只有一組），每則不超過頻道上限（QoS 0：心跳 1 MiB、live 64 KiB；QoS 1：256 KiB），另有一則正在寫入連線；佇列滿了 `publish` 回 `Busy`、`reserve` 等位置。QoS 0（心跳、live）不受 in-flight 上限限制，in-flight 滿了照樣送出 |
| 收到、還沒被取走的訊息 | 佇列 16 則（`INCOMING_QUEUE`，也是送給 broker 的 Receive Maximum）加 1 則等著放進去；這時不再讀 socket，broker 的 Receive Maximum 讓它不再送 |
| MQTT 讀取緩衝 | 一個封包，最多 `MIN_MAX_PACKET_BYTES`；超過 64 KiB 的封包處理完就縮回 |
| WebSocket | 讀取緩衝 16 KiB，收到的 message 最多 64 KiB，送出的最多 32 KiB |

最壞情況（每則都到頻道上限）約 29 MiB；平常的遙測與心跳只用到其中一小部分。`tests/link.rs` 以停住的時鐘驗證前三項：PUBACK 不回來時正好送出 `inflight_limit` 則、佇列停在 8 則、以 `reserve` 等位置的呼叫端都在等，下一個封包是一整個 keepalive 之後的 PINGREQ；每回一個 PUBACK 剛好再送一則。佇列滿時，一千個同時的 `publish` 在第一次 poll 就全部回 `Busy`，佇列與 in-flight 都不變；`reserve` 的等待者在拿到位置之前不建 payload，位置依要求的先後給。

## systemd

[`systemd/trustiot-agent.service`](systemd/trustiot-agent.service) 裝成 `/usr/lib/systemd/system/trustiot-agent.service`，執行檔在 `/usr/bin/trustiot-agent`，帳號由 [`systemd/trustiot-agent.sysusers`](systemd/trustiot-agent.sysusers)（裝成 `/usr/lib/sysusers.d/trustiot-agent.conf`）建立。由映像（Laysi/trustiot-v2#95）安裝。

- `Type=notify`：`READY=1` 在檢查完設定、憑證與金鑰之後送，不等連線；`STATUS=` 顯示連線狀態。
- `WatchdogSec=30s`：agent 每 15 秒送一次 `WATCHDOG=1`，要同時滿足兩個條件，否則不送，systemd 就重啟它。第一，連線的工作（supervisor 與連線的狀態機）有進展：它在每個等待開始前宣告這一步最晚何時結束（`src/liveness.rs`）：開連線最多 `connect_timeout` 加一次 TPM 回應的時間（共 40 秒）、一次寫入最多 keepalive 的 1.5 倍、等 CONNACK 30 秒、連線中與等 `online` 的 PUBACK 或 SUBACK 時等到下一個計時器（通常是 keepalive 到時的 PINGREQ）、等應用程式取走收到的訊息 5 分鐘（`DELIVERY_LIMIT`）、退避的延遲、停止 5 秒、關閉 2 秒。過了最晚時間再加 10 秒（`SLACK`）還沒結束，就不再送，並在 `STATUS=` 寫出卡在哪一步、已經多久。只看執行緒不夠：工作卡在一個永遠不結束的 await 時，執行緒照樣在跑。正常但很長的等待（沒有網路的現場退避最多 60 秒）有自己的期限，不會害 agent 被重啟。第二，連線執行緒的計數（每秒加一，`PROGRESS_TICK`）有前進：執行緒被佔住（例如 TPM 簽章）時計數會停。送 `WATCHDOG=1` 的工作在主 runtime 上，主 runtime 卡住時它也停住。TPM 簽章會佔住連線執行緒，所以 `tpm::RESPONSE_TIMEOUT` 加一次計數要小於 15 秒，開啟金鑰的兩個 TPM 指令要在 `TimeoutStartSec=30s` 之內（`tests/systemd_unit.rs` 檢查）。
- `Restart=always`，間隔從 2 秒逐步拉長到 1 分鐘，`StartLimitIntervalSec=0`：無人看管的閘道器永遠重啟。`After=network.target`，不等 `network-online.target`，沒有網路的現場不拖慢開機。
- 以專用帳號 `trustiot-agent` 執行（不是 root，也不是 `DynamicUser`：TPM 要固定的群組），`SupplementaryGroups=tss` 讀寫 `/dev/tpmrm0`（Debian 的 `tpm-udev` 建立 tss 群組）。`/etc/trustiot` 由 `ConfigurationDirectory` 建立、屬於 root、對 agent 唯讀；`/var/lib/trustiot-agent`（`StateDirectory`，0700）是 agent 自己的。
- 沙箱：`NoNewPrivileges`、空的 `CapabilityBoundingSet` 與 `AmbientCapabilities`、`ProtectSystem=strict`、`ProtectHome`、`PrivateTmp`、`DevicePolicy=closed` 只開 `/dev/tpmrm0`、`RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6`、`SystemCallFilter=@system-service` 並去掉 `@privileged @resources`、`MemoryDenyWriteExecute` 等。刻意不設的：`PrivateDevices`（會藏起 TPM）、`ProcSubset=pid`（心跳要讀 `/proc/meminfo`、`/proc/uptime`）、`PrivateUsers`（tss 群組會失效）。`systemd-analyze security --offline` 的評分是 1.5（2026-10-21），剩下的是網路、TPM 裝置、tss 群組與 `/proc`。

`tests/systemd_unit.rs` 檢查帳號不是 root、上述各行，以及 `systemd-analyze verify` 與評分不高於 2.0（機器上沒有 systemd-analyze 時略過，`CI` 有設時失敗）。

## Modbus 讀取

每台設備建一個 `DeviceReader::new(&device, &model)`（`crates/protocol` 的 `config::Device` 與它的 `DeviceModel`，已經過 `DesiredConfig` 的驗證），每次輪詢呼叫 `reader.poll(&mut connection, &clock, timeout)`，得到 `Poll`：

- `readings`：每個讀到並解碼成功的資料點一筆 `telemetry::Reading`，時間是該請求的回應到達時 `Clock::now()` 的毫秒時間，帶 `clock_synced`。
- `failed`：其他每個資料點與原因（`health::LastFailure`：種類、時間、例外碼、說明）。
- `requests`：送出或連線失敗的每個請求、它的失敗與丟棄的站號不符回應數，給之後的讀取統計（H1）用。

### 只讀

請求只能從 `protocol::config::FunctionCode` 建立，它只有 1–4，所以程式送不出功能碼 5 以上（閘道器對現場設備唯讀）。整合測試在每次輪詢後檢查假端點的請求紀錄，確認沒有 1–4 以外的功能碼。

### 合併讀取

同一個功能碼、位址相距不超過 `MAX_GAP`（10 個沒讀的位址）的資料點合併成一個讀取，一個讀取最多 125 個暫存器或 2000 個位元（一個回應的上限）；依功能碼、位址排序後由前往後合併。中間沒讀的位址也會被讀取，結果丟掉。

### 解碼與縮放

型別與位元組順序依 `config::DataType`、`config::ByteOrder` 的定義（`ByteOrder` 是兩個獨立的對調：`badc`、`dcba` 對調每個暫存器內的兩個 byte，`cdab`、`dcba` 反轉暫存器的順序）。

- 整數送 `Value::Int`；`u64` 大於 `i64::MAX` 是讀取失敗（`invalid_value`），除非有縮放。
- 浮點數送 `Value::Float`；NaN 或無限大是讀取失敗。
- 字串每個暫存器兩個字元（`badc` 時低位元組在前，不做字組對調），到第一個 NUL 為止並去掉前後空白；不是 UTF-8 的 byte 換成 U+FFFD。
- 暫存器上的 `bool` 是「不為 0」。
- 設了 `scale` 或 `offset` 的數值一律送 `Value::Float`：`raw * scale + offset`，沒設的 scale 當 1、offset 當 0；整數先轉成 `f64`，超過 2^53 會失去最低位。結果不是有限數（例如溢位）是讀取失敗。

### 哪個回應算數（Modbus TCP）

回應的站號與交易 ID 都符合請求才算數。其他回應讀進來丟掉，請求繼續等到逾時：站號不符的計入 `unit_mismatches`（轉換器把回應送給所有連線，Laysi/trustiot#183）；站號相同、交易 ID 不同的是先前逾時的請求晚到的回應。所以晚到的回應不會被當成下一個請求的值。兩個請求之間到達的完整框架回答不了任何還在等的請求，在下一個請求送出前就丟掉（站號不符的一樣計入）。交易 ID 從隨機值開始，與同一台設備的其他 client（通常從 0 或 1 數起）同時用到同一個 ID 的機會很小。

收到的 bytes 先放進緩衝、湊滿一個 MBAP 框架才處理，逾時打斷時已收到的部分留在緩衝，框架不會錯位。緩衝最多一個框架加一次 socket 讀取；協定 ID 不是 0、長度不在 2–254 的標頭表示串流不是 Modbus TCP，連線關閉。

### 失敗與輪詢

| 狀況 | `ReadError` | 回報的種類（`FailureKind`） | 輪詢 |
| --- | --- | --- | --- |
| 連不上（含連線逾時） | `Connect` | `connect` | 其餘請求不送 |
| 請求途中連線被關閉或 reset、送出失敗（重送一次之後仍然如此，見下方） | `ConnectionLost` | `connect` | 其餘請求不送，下一次輪詢重連 |
| 串流不是 Modbus TCP（例如講 RTU over TCP 的轉換器被設成 Modbus TCP） | `Protocol` | `invalid_value` | 其餘請求不送，下一次輪詢重連 |
| 等不到符合的回應 | `Timeout` | `timeout` | 繼續下一個請求，連線保留 |
| 例外回應 | `Exception` | `exception`（帶例外碼） | 繼續 |
| 符合的回應與請求對不上（功能碼、byte 數） | `Malformed` | `invalid_value` | 繼續 |
| 解碼不出可送的值 | — | `invalid_value`（只有該資料點） | 繼續 |

連線中斷算 `connect`：與連不上一樣是 TCP 連線的問題，`timeout` 則是連線正常但設備不回應；連線正常但回來的東西無法解讀算 `invalid_value`。只收到其他站號回應的請求算 `timeout`，那些回應另外計入 `unit_mismatches`，與 `health::FailureCounts` 的定義相同。逾時之後不中止輪詢：有些設備對某些位址不回應，中止會讓後面的資料點一直讀不到；設備整台不回應時一次輪詢要等「請求數 × 逾時」，由 Laysi/trustiot-v2#89 的排程處理。

`TcpConnection` 在第一次讀取時才連線，連線失敗後的下一次讀取重新連線。設備或閘道器可能隨時關掉保持中的連線（閒置逾時，或每次回答後就關）：

- 關閉在請求之前已經到達：送出前就換新連線。
- 請求送出後才發現連線已關閉或被 reset：在新連線上重送一次，讀取不會改變設備的狀態；重送也失敗才是 `ConnectionLost`。剛開好的連線上失敗不重送，所以不會一直重試。

呼叫端以自己的逾時丟掉讀取中的 future 也是安全的：請求寫到一半被丟掉的連線，下一次讀取會換掉；已收到的 bytes 留在緩衝。預設逾時是請求 5 秒、連線 10 秒（`DEFAULT_REQUEST_TIMEOUT`、`DEFAULT_CONNECT_TIMEOUT`，F4）。

### 限制

- 合併讀取會讀到中間沒設定的位址。設備對不存在的位址回例外碼 02 時，整個合併讀取的資料點都失敗；目前沒有自動拆開重試。
- 規定回應要帶請求的站號。回應固定帶其他站號（例如 0 或 255）的 Modbus TCP 設備讀不到，健康報告裡會看到 `unit_mismatch` 與 `timeout`。
- Modbus TCP 只能以站號與交易 ID 認回應。另一個 client（例如現場的 SCADA）經同一台會把回應送給所有連線的閘道器讀同一個站號，而且剛好用到同一個交易 ID、讀的筆數相同時，它的回應會被當成這個請求的；隨機的起始 ID 只降低機率。會廣播回應的通常是 RTU over TCP 的轉換器，由 Laysi/trustiot-v2#88 以共用連線、輪流讀取處理；兩台設備設定成同一組主機、port 與站號時也一樣，設定驗證目前沒有擋。
- 半開連線（設備不送 FIN 或 RST 就忘了連線）只會表現為逾時；偵測並重連是 Laysi/trustiot-v2#89 的 keepalive 與 watchdog。
- 主機寫 DNS 名稱時，名稱解析在 blocking 執行緒上進行，連線逾時之後它仍會跑完；現場設備多半以 IP 設定。

## TPM 簽章

閘道器的身分金鑰在 TPM 裡、不能匯出（決策 12）。`TpmKey::open(TpmTransport::Device("/dev/tpmrm0".into()), handle)` 經核心的資源管理器使用持久 handle 上的金鑰（預設 `DEFAULT_KEY_HANDLE` = `0x81000100`），`TpmKey` 實作 rustls 的 `SigningKey`，inner mTLS 的 CertificateVerify 就在 TPM 內簽。

- 不用 tss-esapi 或 libtss2：只送兩個指令，TPM2_ReadPublic 與 TPM2_Sign，依 TPM 2.0 Part 2、3 自己編解碼，沒有 C 函式庫要交叉編譯或隨附。
- 回應一律當成不可信的輸入：檢查 tag、長度與回應碼，每個長度都不超過剩下的 bytes，結構要剛好結束，回應最多 4096 bytes；格式不符是錯誤，不會 panic。回應碼解成名稱，例如 `TPM_RC_HANDLE (0x0000018b, handle 1)`。
- `open` 先讀公開區域，金鑰必須是 ECC NIST P-256、scheme 固定為 ECDSA-SHA256，屬性要有 `fixedTPM`、`fixedParent`、`sensitiveDataOrigin`、`userWithAuth`、`sign`，不能有 `restricted`、`decrypt`（2026-10-12 PoC 的範本，產線工具 Laysi/trustiot-v2#80 照這個產生）。接著簽一段固定的測試訊息並以公鑰驗證，簽不出來（例如金鑰設了密碼）就在啟動時失敗，不會等到每次握手才失敗。啟動多一次簽章，閘道器上約 70 ms。
- 簽章：SHA-256 在軟體算，以空密碼的 password session（`TPM_RS_PW`）與 null hashcheck ticket 送 TPM2_Sign，r、s 轉成 DER。rustls 只會拿到 `ecdsa_secp256r1_sha256`。
- 憑證要與金鑰相符：`public_key_spki_der()` 是金鑰的 SubjectPublicKeyInfo，rustls 的 `CertifiedKey::keys_match` 經 `SigningKey::public_key` 比對。
- 一條專用執行緒（`tpm`）擁有裝置，請求經有上限的佇列（8 個）一個一個送，佇列滿了回 `Busy`。rustls 的 `Signer::sign` 是同步呼叫，等待期間佔住呼叫它的執行緒（閘道器上約 70 ms），所以握手不能跑在負責 Modbus 輪詢的執行緒上。等待最多 `RESPONSE_TIMEOUT`（10 秒，排隊也算在內；核心對這些指令的逾時更短）：簽章佔住連線執行緒，這段時間加一次進度計數要小於 watchdog 通知的間隔（15 秒），否則 systemd 會在 agent 自己放棄之前重啟它（見上方 systemd）。執行緒停了（panic）時簽章立即回錯誤，不會卡住；最後一個 `TpmKey` 與 `Signer` 被丟掉時執行緒結束並關閉裝置。
- `TpmTransport::Channel` 接受其他 `TpmChannel`（測試以 TCP 連 swtpm）；正式環境只用 `Device`。

## 測試

```sh
cargo test -p agent                                     # 全部
cargo test -p agent --lib                               # 單元測試：設定、憑證、TLS、MQTT 編解碼、退避、心跳、sd_notify、Modbus
cargo test -p agent --test link                         # MQTT 行為，對腳本化的 broker：在線狀態、in-flight 上限、PUBACK、keepalive、退避、協定錯誤、停止
cargo test -p agent --test transport                    # 整條連線在行程內：兩層 TLS 協商的內容、resumption、重連、每種拒絕
cargo test -p agent --test broker                       # 對 Mosquitto 容器（podman 或 docker）：mTLS、retained、0x87、will、broker 與轉接重啟後重連
cargo test -p agent --test binary                       # 執行檔：--check、sd_notify、SIGTERM
cargo test -p agent --test systemd_unit                 # unit 檔與 systemd-analyze
cargo test -p agent --test dependencies                 # 閘道器建置的相依
cargo test -p agent --test modbus_fake_endpoint         # 對 testkit 假端點：型別、合併讀取、每種故障
cargo test -p agent --test modbus_tcp_framing           # 以 raw socket 驗證哪個回應算數、框架同步
cargo test -p agent --test tpm_swtpm                    # swtpm 容器：簽章、mTLS 握手、拒絕不能用的金鑰
```

- `link` 與 Modbus 的整合測試都用停住的時鐘（`start_paused`，Modbus 另加 `testkit::clock::manual()`）：keepalive、退避與逾時只在其他事情都停下時才觸發，機器再忙也不會誤判。
- `transport`、`broker`、`binary` 走真的 socket，每個等待都以 `testkit::clock::bounded` 設上限。WebSocket 前端（替代入口與 mqtt-bridge）、測試 CA 與 Mosquitto 容器來自 `testkit::mqtt`，用法見 [`crates/testkit/README.md`](../../crates/testkit/README.md)「MQTT 測試環境」。所有憑證與金鑰在測試時產生，不進 repo。
- `broker` 需要 podman 或 docker：沒有時略過，`CI` 有設時失敗。
- 假 Modbus 端點的用法見 [`crates/testkit/README.md`](../../crates/testkit/README.md)「假 Modbus 端點」。
- `tpm_swtpm` 以 `tests/swtpm/Containerfile`（Debian 的 swtpm 與 tpm2-tools，基底以 multi-arch digest 釘選）建映像，build context 是空目錄；映像留在本機（`localhost/trustiot-agent-swtpm:<Containerfile 的雜湊>`），下次直接沿用，不要時以 `podman rmi` 移除。測試時以 tpm2-tools 佈建金鑰，比對 tpm2-tools 算的 SubjectPublicKeyInfo、以公鑰驗證簽章，並做一次真的 TLS 1.3 mTLS 握手（測試 CA 為 TPM 公鑰簽發用戶端憑證，CertificateVerify 剛好一次 TPM2_Sign，resumption 不簽章）。沒有 podman 或 docker 時略過，設了 `CI` 時失敗。解析與執行緒的行為在單元測試（`cargo test -p agent --lib tpm`）以 swtpm 錄下的回應與假 TPM 測。
