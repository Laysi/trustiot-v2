# agent

閘道器 agent（[決策 10](../../docs/architecture/decisions.md)）：在現場閘道器上讀取設備、把資料送上雲端的程式。目前有執行檔 `trustiot-agent`、systemd unit、到 broker 的連線（443 的 WebSocket 內走 mTLS 與 MQTT 5）、TPM 裡的身分金鑰（`tpm` 模組，[決策 12](../../docs/architecture/decisions.md)）、在線狀態與基本心跳；以及 Modbus 讀取（`modbus` 模組，[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) F1、F2、F5）：Modbus TCP 與 RS485 轉換器的 RTU over TCP，同一個端點的設備共用一條連線；以及每台設備的輪詢排程（`polling` 模組，F4）：固定節拍、逾時、重連退避、TCP keepalive 與每台設備的 watchdog（關掉不再回答的設備的連線，與重啟整個服務的 systemd watchdog 是兩回事）。執行檔還沒有跑輪詢；把輪詢接進服務、離線緩衝與設定套用之後建立在它們上面。

| 在哪裡 | 內容 | issue |
| --- | --- | --- |
| `src/main.rs` | 執行檔 `trustiot-agent`：參數、日誌、兩個 runtime、sd_notify、SIGTERM | Laysi/trustiot-v2#86 |
| `src/config.rs`、`agent.example.toml` | 設定檔（TOML）與它的檢查 | Laysi/trustiot-v2#86 |
| `src/setup.rs`、`src/identity.rs` | 讀設定、憑證與金鑰，閘道器 ID 取自憑證的 CN | Laysi/trustiot-v2#86 |
| `src/tpm.rs`、`src/tpm/` | `TpmKey`：TPM 內的身分金鑰（TPM2_ReadPublic、TPM2_Sign）與 rustls 的 `SigningKey` | Laysi/trustiot-v2#86 |
| `src/tls.rs`、`src/transport.rs` | 兩層 TLS 的設定；TCP → 外層 TLS → WebSocket → 內層 mTLS | Laysi/trustiot-v2#86 |
| `src/mqtt/packet.rs`、`src/mqtt/connection.rs` | MQTT 5 編解碼（只含用到的部分）與一條連線的狀態機 | Laysi/trustiot-v2#86 |
| `src/link.rs`、`src/backoff.rs` | 對外的 `Link`（publish、收訊息、狀態）與重連退避 | Laysi/trustiot-v2#86 |
| `src/heartbeat.rs`、`src/sdnotify.rs`、`src/liveness.rs` | 基本心跳；systemd 的 `READY=1`、`WATCHDOG=1`、`STATUS=`；連線的工作是否卡住（給 systemd watchdog 判斷） | Laysi/trustiot-v2#86 |
| `systemd/` | `trustiot-agent.service` 與帳號（sysusers.d） | Laysi/trustiot-v2#86 |
| `src/modbus.rs` | `DeviceReader`：依型號規劃讀取、送出、解碼並蓋時間戳；`Connection`、`Clock` 介面 | Laysi/trustiot-v2#87 |
| `src/modbus/plan.rs` | 合併讀取的規劃 | Laysi/trustiot-v2#87 |
| `src/modbus/pdu.rs` | 功能碼 1–4 的請求與回應 PDU | Laysi/trustiot-v2#87 |
| `src/modbus/decode.rs` | 型別、位元組順序、scale 與 offset | Laysi/trustiot-v2#87 |
| `src/modbus/tcp.rs` | 兩種傳輸方式共用的 `Connection`（`TcpConnection`）：連線管理、送出、接收到找到回應或逾時 | Laysi/trustiot-v2#87、Laysi/trustiot-v2#88 |
| `src/modbus/mbap.rs` | Modbus TCP 的框架（MBAP）：交易 ID、哪個回應算數 | Laysi/trustiot-v2#87、Laysi/trustiot-v2#88 |
| `src/modbus/rtu.rs` | RTU over TCP 的框架：CRC、由功能碼與 byte 數切框、重新同步、哪個回應算數 | Laysi/trustiot-v2#88 |
| `src/modbus/endpoint.rs` | `Endpoints`、`SharedConnection`：同一個端點的設備共用一條連線、逐個請求輪流 | Laysi/trustiot-v2#88 |
| `src/polling.rs` | `run`：每台設備的輪詢迴圈，固定節拍、重連退避、每台設備的 watchdog；`Settings`、`Disconnect` | Laysi/trustiot-v2#89 |
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
| `broker.ca` | 內層 mTLS 的信任錨：我們的根 CA 憑證（PEM）。只能放根 CA（主體與簽發者相同、以自己的金鑰簽章、basicConstraints 的 cA），可以放多張（之後換根用），但主體（subject）不能重複；放了簽發 CA 或 leaf、或兩張根同名，就拒絕啟動，錯誤訊息指出第幾張。內層只信任這些根，不用作業系統或 WebPKI 的根，也不用 `cloud.trust`。broker 的名稱固定是 `mqtt-broker.trustiot.internal`（Laysi/trustiot-v2#557），沒有設定的鍵：舊設定檔的 `broker.name` 是未知的鍵，會拒絕啟動，刪掉那一行即可。`cloud.url` 的主機不可以是這個名稱：兩層驗證的是不同的伺服器。規則見下方「內層 TLS 對 broker 的信任」 |
| `broker.crl` | 選填：根 CRL 的檔案（範例是 `/var/lib/trustiot-agent/root-crl.pem`）。一個 PEM 區塊 `X509 CRL`、最多 64 KiB、不帶 issuing distribution point，要由 `broker.ca` 的根簽章，啟動時就驗證。檔案不存在表示還沒收到根 CRL：照常連線、不檢查撤銷，啟動時記一筆 INFO；檔案在但不能用（不是一個 PEM CRL、太大、不是根簽的）就拒絕啟動。沒有這個鍵時也不檢查撤銷 |
| `identity.certificate` | 閘道器的憑證鏈（leaf 在前，接著簽發 CA）。閘道器 ID 取自 leaf 的 CN（必須是合法的 `GatewayId`），不另外設定，兩者不可能不一致 |
| `identity.key` | 正式環境是 `{ source = "tpm", device = "/dev/tpmrm0", handle = 0x81000100 }`（handle 要在 persistent 範圍 0x81000000–0x81FFFFFF）；`{ source = "file", path = "..." }` 只給開發與測試，啟動時會記一筆警告 |
| `connection.*` | 心跳間隔（10–3600 秒，預設 60）、重連退避的起始與上限（預設 1 秒、60 秒）、同時等 PUBACK 的 QoS 1 訊息數（1–256，預設 32） |

啟動時讀設定、確認 `broker.ca` 只有根 CA、載入根 CRL（記下 CRL 編號與 nextUpdate；已過期時另記一筆警告）、建好兩層 TLS 的設定、讀憑證並確認金鑰與憑證的公鑰相同（TPM 的金鑰也一樣）；有 EKU 時必須含 clientAuth，有 key usage 時必須含 digitalSignature。接著以 webpki 的用戶端憑證檢查（rustls 的 `WebPkiClientVerifier`）確認憑證鏈接到 `broker.ca`，broker 一定會拒絕的憑證（例如別的 CA 簽的）就不啟動。只有時間不對（過期或還沒生效）時記一筆警告，寫出憑證的 notBefore 與 notAfter，照樣啟動：閘道器有硬體 RTC，但時間仍可能是錯的（例如 RTC 沒設定過或遺失了時間），效期以 broker 的時鐘為準，拒絕啟動只會把時鐘的問題變成停機；真的過期時 broker 會拒絕，由換發（Laysi/trustiot-v2#83）處理。全部通過才通知 systemd `READY=1`，不等第一次連線：沒有網路的現場也要能啟動，之後自己重試。`--check` 做完這些檢查就結束（0 是可用並印出閘道器 ID 與憑證的到期時間，1 是不可用並印出原因），給維運與更新後的健康檢查用。設備的設定（`DesiredConfig`）之後由 MQTT 送來（Laysi/trustiot-v2#94），不在這個檔案裡。

日誌寫到 stderr，由 systemd 交給 journal；等級由 `TRUSTIOT_AGENT_LOG` 設定（預設 `info`）。

執行緒：連線（TLS 握手與 MQTT）在自己的執行緒、單執行緒的 tokio runtime 上跑，因為完整的內層握手要在 rustls 的同步簽章呼叫裡等 TPM 約 70 ms（[TPM 測試](../../docs/feasibility/2026-10-12-tpm-poc.md)），這樣只卡住這條執行緒；心跳、systemd watchdog 的通知、訊號與之後的 Modbus 輪詢在另一個兩條執行緒的 runtime 上。

## 連線（WebSocket 內的 mTLS、MQTT 5）

只有一種連線方式，SaaS 與地端相同（[決策 6](../../docs/architecture/decisions.md)、[部署](../../docs/architecture/deployment.md)）：

1. TCP（`TCP_NODELAY`，每個解析出的位址各等 10 秒），連到 `cloud.url` 的主機。開 TCP keepalive：閒置 60 秒後每 15 秒探一次，4 次沒回應就由核心斷線，是 MQTT keepalive 之外的保險。
2. 外層 TLS：SNI 是 URL 的主機，ALPN `http/1.1`，信任 `cloud.trust` 的檔案；不用 `webpki-roots` 也不讀作業系統的憑證庫。
3. WebSocket：`GET /mqtt`，subprotocol `mqtt`，只送收 binary message；對方沒回 `mqtt` subprotocol 就失敗。每則送出的 message 最多 32 KiB（一筆內層 TLS record 放得下），收到的 message 與 frame 最多 64 KiB，讀取緩衝 16 KiB。text message 視為協定錯誤；close 視為連線結束。
4. 內層 mTLS：SNI 與名稱驗證都用固定的 `mqtt-broker.trustiot.internal`，只信任 `broker.ca` 的根 CA，有根 CRL 時以它檢查簽發 CA，用閘道器的憑證與金鑰（TPM）。
5. MQTT 5。

開一條連線（上面 1–4 步）最多 30 秒（`CONNECT_TIMEOUT`），之後等 CONNACK 另外最多 30 秒；逾時都算連線失敗，照退避重試。

兩層都只用 TLS 1.3，套件順序是 ChaCha20-Poly1305、AES-256-GCM、AES-128-GCM（閘道器的 CPU 沒有 AES 指令，rustls 預設會選 AES-256-GCM），都開 session resumption（每層各記 8 個）：resumption 不需要 TPM 簽章，重連省下約 70 ms。加密實作只用 `ring`。Mosquitto 依自己的套件順序選，不依用戶端的順序，所以 broker 的 mTLS listener 要以 `ciphers_tls1.3` 把 `TLS_CHACHA20_POLY1305_SHA256` 排第一（chart 給閘道器的 listener 在 2026-10-23 補上，Laysi/trustiot-v2#564），否則協商出 AES-256-GCM（2026-10-21 實測，[測試報告](../../docs/test-reports/2026-10-21-86-agent-mqtt-connection.md)）。

### 內層 TLS 對 broker 的信任

依 Laysi/trustiot-v2#556 的決定（[services/certs](../../services/certs/README.md)「閘道器對 broker 伺服器憑證的信任」與「根 CRL」），在 `src/tls.rs`：

- **信任錨**：`broker.ca` 的根 CA，可以多張；簽發 CA 或 leaf 拒絕啟動。以簽發 CA 當信任錨時，根 CRL 撤銷它也沒有用，換一代簽發 CA 還要改每台閘道器的設定。每張根的主體要不同：CRL 只以主體指出簽發者，同名的兩個根會讓 webpki 拿錯的金鑰驗 CRL，經另一個根的連線全部失敗。
- **憑證鏈**：broker 送它的憑證與簽發 CA，agent 由此接到根；broker 只送自己的憑證時連線失敗（`UnknownIssuer`）。
- **名稱**：SNI 與驗證都用 `tls::BROKER_NAME`（`mqtt-broker.trustiot.internal`），SaaS 與地端相同，與 `cloud.url` 無關；名稱驗證不能關。
- **用途**：broker 的憑證要有 EKU serverAuth（webpki 的檢查），只有 clientAuth 的憑證（例如同一個簽發 CA 簽的閘道器憑證）被拒。沒有 EKU 的憑證 webpki 不檢查用途，由 CA 不簽這種憑證來保證（services/certs）。
- **撤銷**：rustls 的 `WebPkiServerVerifier` 以 `with_crls` 帶入根 CRL、設 `allow_unknown_revocation_status`，不用 `only_check_end_entity_revocation` 與 `enforce_revocation_expiration`。所以根以下的每張憑證都以根 CRL 檢查，被撤銷的簽發 CA 拒絕；broker 自己的憑證由簽發 CA 簽，閘道器沒有簽發 CA 的 CRL，狀態未知，照樣通過；根 CRL 過期仍然以它檢查、照常連線。沒有根 CRL 時不檢查撤銷。
- **載入**：只在啟動時讀一次。根 CRL 要由 `broker.ca` 中名稱相符的根簽章（根有 key usage 時要有 cRLSign），webpki 也要能解析；不符就拒絕啟動，壞掉或偽造的檔案不會被默默略過。帶 issuing distribution point 的根 CRL 也拒絕：webpki 只把它用在範圍內的憑證，範圍排除簽發 CA 時等於不檢查，又因為允許狀態未知而不會報錯（根 CRL 的格式規則在 services/certs「根 CRL」）。
- **簽章演算法**：載入時驗根的自簽章與根 CRL 的簽章，用的是握手時 webpki 用的同一份清單（ring provider 的 `signature_verification_algorithms`），依 AlgorithmIdentifier 的位元組比對；x509-parser 只負責解析。所以不會有載入時通過、每次握手卻失敗的根或根 CRL（例如 SHA-1，或多帶參數的 ecdsa-with-SHA256）。
- **錯誤訊息裡的名稱**：憑證與 CRL 的名稱來自任何人都能寫的檔案，寫進錯誤訊息前把控制字元換成空白、最多 200 個字元，免得偽造或切開日誌的行。
- **已知的限制**：一張根 CRL 只涵蓋簽它的根底下的簽發 CA。換根期間 `broker.ca` 有兩個根時，另一個根底下的簽發 CA 不檢查撤銷，支援多張根 CRL 與換根一起規劃（Laysi/trustiot-v2#563 的範圍）。根 CRL 檔被刪除時 agent 當成「還沒有」而不檢查撤銷；有了取用管道之後這是降級的入口，防止的做法在 Laysi/trustiot-v2#563（記下曾接受過的最高編號）。
- **還沒做**（Laysi/trustiot-v2#563，等 Laysi/trustiot-v2#560 的發佈管道）：接受編號更大的新根 CRL 並存下、定期取得、換上新的根 CRL 時清掉 resumption 的快取、在心跳回報根 CRL 的編號與是否過期。在那之前換根 CRL 要重新啟動 agent，新的行程也就有新的 resumption 快取。

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

每台設備建一個 `DeviceReader::new(&device, &model)`（`crates/protocol` 的 `config::Device` 與它的 `DeviceModel`，已經過 `DesiredConfig` 的驗證），連線向一個 `Endpoints` 要：`let mut connection = endpoints.connection(&device)`（見下方「共用連線」）。每次輪詢呼叫 `reader.poll(&mut connection, &clock, timeout)`，得到 `Poll`：

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

### RTU over TCP（RS485 轉換器）

設定的 `transport` 是 `rtu_over_tcp` 時，`TcpConnection` 以 RTU 框架經 TCP 連到 RS485 轉換器（Modbus over Serial Line V1.02）：站號、PDU、CRC-16（多項式 0xA001 反射、初值 0xFFFF，低位元組在前）。讀取請求固定 8 bytes：站號、功能碼、起始位址、筆數、CRC。

RTU 沒有交易 ID，也沒有長度欄位；序列線上以 3.5 個字元的靜默結束一個框架，TCP 不帶這段靜默。所以回應的長度由功能碼與 byte 數推出，由 CRC 判斷一段 bytes 是不是框架：

- 功能碼 1–4：byte 數要是一個讀取回得出來的（位元 1–250；暫存器是 2–250 的偶數），否則不是框架；長度是 5 加 byte 數。
- 功能碼 5、6、15、16（同一條匯流排上其他主站的寫入回應）：8 bytes。
- 例外回應（功能碼 0x81 以上）：5 bytes。
- 其他功能碼、不合理的 byte 數或 CRC 錯：不是框架，跳過一個 byte 再試（重新同步），不關閉連線。

哪個回應算數：來自請求的站號、功能碼與 byte 數都符合請求的完整框架（例外回應要是這個功能碼的）。其他的讀進來丟掉，請求繼續等到逾時：

- 站號不符的計入 `unit_mismatches`（轉換器把每台設備的回應送給每條連線，Laysi/trustiot#183）。
- 站號相同但對不上的（功能碼或 byte 數不同）分不出是同一站號較早請求晚到的回應，還是其他主站的，所以不中止請求，只記下來。
- 會回送（echo）請求的轉換器：開頭與送出的請求完全相同的 8 bytes 跳過；收到的還只是請求的前段時先等後段。
- 湊不成框架的 bytes（雜訊、CRC 錯的框架）逐 byte 跳過。看起來像長框架開頭的雜訊不會蓋住它後面已經完整到達的回應：這時往後找一個站號、功能碼、byte 數與 CRC 都符合請求的完整框架，找到就從那裡繼續；條件這麼嚴，雜訊被誤認成回應的機會實際上不存在。

逾時時，期間收到的東西決定回報什麼：收到過該站號但對不上的框架是 `Malformed`（說明對不上的原因）；只收到湊不成框架的 bytes 是 `Malformed`，說明先寫觀察到的事實（「N bytes 湊不成 RTU 框架，也沒有答案」），再列出可能的原因：轉換器與設備的序列埠設定（鮑率、同位元）不同，或匯流排上的雜訊；說明不指定單一原因。這比單純的 `timeout` 多告訴你收到了什麼；其他是 `Timeout`。兩種都不中止輪詢。

兩個請求之間到達的完整框架丟掉（站號不符的一樣計入下一個請求的 `unit_mismatches`），送出下一個請求前清空整個緩衝，與 Modbus TCP 不同：沒有交易 ID，被切斷的框架與回應的開頭分不出來，留著的話，它的後半段接上去可能被讀成一個完整的晚到回應；丟掉之後，後半段到達時當成雜訊跳過。

### 共用連線

`Endpoints::new(connect_timeout)` 管理每個端點的連線；`endpoints.connection(&device)` 對傳輸方式、主機與 port 都相同的設備回傳同一個 `SharedConnection`，第一次用到時建立，到第一次讀取才連線。主機以 `Host::canonical()` 比較並連線：`::ffff:192.0.2.10` 與 `192.0.2.10`、只差大小寫的 DNS 名稱是同一個端點。`SharedConnection` 的 clone 是同一條連線的另一個 handle，每台設備的輪詢用自己的 handle：`reader.poll(&mut connection, &clock, timeout)`。

為什麼：RS485 轉換器把每台設備的回應送給每條 TCP 連線，舊系統每台設備各開一條連線，結果回應落到錯的設備、請求在匯流排上碰撞（Laysi/trustiot#183，[現場硬限制](../../docs/requirements/field-constraints.md)）。一個端點一條連線、一次一個請求，回應到達時只有它回答的那個請求在等；框架再丟掉其他站號的回應。

- Modbus TCP 的設備也一樣，同一組主機與 port 共用一條連線：前面接序列匯流排的 Modbus TCP 閘道器同樣只有一條匯流排，而且可能只接受少數幾條連線；一個端點只用一條連線就不會跟自己搶，同一組主機、port 與站號的兩台設備也不會再在兩條連線上互相干擾。
- 傳輸方式也是鍵的一部分：設定驗證已經拒絕同一個主機與 port 混用兩種傳輸方式（`MixedTransport`），沒經過驗證的設定也不會讓設備被用錯的框架讀取。
- 輪流的單位是請求，不是輪詢：每個請求等到輪到它才取得連線，送出、等到回應、失敗或逾時就交出去。等待是先到先得（tokio 的 `Mutex` 是公平的），同時輪詢的設備依 A1 B1 C1 A2 B2 C2 的順序輪流，一台不回應的設備，每個請求讓其他設備多等一個請求逾時，而不是等它整個輪詢。
- 請求的逾時從送出時算起，不含等輪到的時間。還在等輪到的讀取被丟掉，就放棄它的順位，不會卡住後面的請求。

### 失敗與輪詢

| 狀況 | `ReadError` | 回報的種類（`FailureKind`） | 輪詢 |
| --- | --- | --- | --- |
| 連不上（含連線逾時） | `Connect` | `connect` | 其餘請求不送 |
| 請求途中連線被關閉或 reset、送出失敗（重送一次之後仍然如此，見下方） | `ConnectionLost` | `connect` | 其餘請求不送，下一次輪詢重連 |
| 串流不是 Modbus TCP（例如講 RTU over TCP 的轉換器被設成 Modbus TCP；只有 Modbus TCP 會這樣，RTU 改為重新同步） | `Protocol` | `invalid_value` | 其餘請求不送，下一次輪詢重連 |
| 等不到符合的回應 | `Timeout` | `timeout` | 繼續下一個請求，連線保留 |
| 例外回應 | `Exception` | `exception`（帶例外碼） | 繼續 |
| 符合的回應與請求對不上（功能碼、byte 數；Modbus TCP） | `Malformed` | `invalid_value` | 繼續 |
| RTU：等到逾時只收到該站號但對不上的框架 | `Malformed`（說明對不上的原因） | `invalid_value` | 繼續，連線保留 |
| RTU：等到逾時只收到湊不成框架的 bytes | `Malformed`（說明列出可能的原因：轉換器與設備的序列埠設定（鮑率、同位元）不同，或匯流排上的雜訊） | `invalid_value` | 繼續，連線保留 |
| 解碼不出可送的值 | — | `invalid_value`（只有該資料點） | 繼續 |

連線中斷算 `connect`：與連不上一樣是 TCP 連線的問題，`timeout` 則是連線正常但設備不回應；連線正常但回來的東西無法解讀算 `invalid_value`。只收到其他站號回應的請求算 `timeout`，那些回應另外計入 `unit_mismatches`，與 `health::FailureCounts` 的定義相同。逾時之後不中止輪詢：有些設備對某些位址不回應，中止會讓後面的資料點一直讀不到；設備整台不回應時一次輪詢要等「請求數 × 逾時」；超過節拍的輪詢跳過它蓋過的節拍，不會累積，連續不回答太久由 watchdog 處理（見「排程與自我恢復」）。

`TcpConnection::new(transport, host, port, connect_timeout)` 依 `transport` 選框架，兩種傳輸方式的連線管理相同。它在第一次讀取時才連線，連線失敗後的下一次讀取重新連線。設備、閘道器或轉換器可能隨時關掉保持中的連線（閒置逾時，或每次回答後就關）：

- 關閉在請求之前已經到達：送出前就換新連線。
- 請求送出後才發現連線已關閉或被 reset：在新連線上重送一次，讀取不會改變設備的狀態；重送也失敗才是 `ConnectionLost`。剛開好的連線上失敗不重送，所以不會一直重試。

呼叫端以自己的逾時丟掉讀取中的 future 也是安全的：請求寫到一半被丟掉的連線，下一次讀取會換掉；已收到的 bytes 留在緩衝。預設逾時是請求 5 秒、連線 10 秒（`DEFAULT_REQUEST_TIMEOUT`、`DEFAULT_CONNECT_TIMEOUT`，F4）。每條連線都開 TCP keepalive（見「排程與自我恢復」）。

### 限制

- 合併讀取會讀到中間沒設定的位址。設備對不存在的位址回例外碼 02 時，整個合併讀取的資料點都失敗；目前沒有自動拆開重試。
- 規定回應要帶請求的站號。回應固定帶其他站號（例如 0 或 255）的 Modbus TCP 設備讀不到，健康報告裡會看到 `unit_mismatch` 與 `timeout`。
- Modbus TCP 只能以站號與交易 ID 認回應。另一個 client（例如現場的 SCADA）經同一台會把回應送給所有連線的閘道器讀同一個站號，而且剛好用到同一個交易 ID、讀的筆數相同時，它的回應會被當成這個請求的；隨機的起始 ID 只降低機率。agent 自己的設備不會這樣互相干擾：同一組主機與 port 的設備共用一條連線、輪流送出請求，設定成同一組主機、port 與站號的兩台設備也一樣（設定驗證目前沒有擋這種設定）。
- RTU 沒有交易 ID：站號、功能碼與 byte 數都與在等的請求相同的回應，就被當成它的回答。所以（一）設備在請求逾時（預設 5 秒）之後才回答，而當時在等的下一個請求剛好是同一個站號、同一個功能碼、同樣的筆數時，這個晚到的回答會被當成下一個請求的值；（二）另一個主站經同一台轉換器以同樣方式讀同一個站號時也一樣。（一）即使 agent 是轉換器上唯一的主站也會發生，目前沒有對策；由測試 `known_limitation_a_late_answer_of_the_same_shape_is_taken_for_the_next_request` 重現。（二）只在 agent 是同一台轉換器上唯一的主站時才不會發生；目前只有 lab 閘道器的 RS485 轉換器有這樣的規定（[milestones](../../docs/roadmap/milestones.md)「展示切片」：agent 開始輪詢 lab 的 RS485 轉換器後，其他程式不再連同一台轉換器），現場的規則還沒有決定。
- 兄弟設備晚到的回答（它的請求已經逾時），計入當時在等的請求的 `unit_mismatches`，那可能是另一台設備的請求，不是晚到的那台。
- 沒有可設定的請求間隔：前一個請求結束就送下一個，序列線上 3.5 字元的靜默由轉換器負責。
- 半開連線（設備不送 FIN 或 RST 就忘了連線）在 `TcpConnection` 只會表現為逾時，連線保留；換掉它的是輪詢迴圈的 watchdog（見「排程與自我恢復」）。
- 主機寫 DNS 名稱時，名稱解析在 blocking 執行緒上進行，連線逾時之後它仍會跑完；現場設備多半以 IP 設定。

## 排程與自我恢復

現場曾有設備靜默停擺 2.5 小時、外觀卻完全正常（Laysi/trustiot#151），也有設備一直連著卻永遠不回應、keepalive 偵測不到（Laysi/trustiot#159、Laysi/trustiot#164；[現場硬限制](../../docs/requirements/field-constraints.md)）。所以每台設備由一個輪詢迴圈負責（[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) F4）：

```rust
let settings = polling::Settings::default(); // 或讀自設定，先 settings.validate()?
let mut endpoints = Endpoints::new(settings.connect_timeout);
let mut connection = endpoints.connection(&device);
polling::run(&reader, polling::poll_period(&device), &mut connection, &clock, &settings, |event| {
    // Event::Polled：一次輪詢的結果（Poll、預定與實際開始時間、跳過的節拍數、watchdog 是否觸發）
    // Event::Waiting：迴圈睡到下一個節拍（連續失敗次數、退避時間）
    ControlFlow::Continue(())
})
.await;
```

迴圈一直跑到 `on_event` 回傳 `ControlFlow::Break`，或呼叫端丟掉這個 future（丟掉是安全的，連線仍可用）。

### 設定

每台設備用同一份 `polling::Settings`。`Settings::validate()` 檢查範圍，錯誤寫出第一個超出範圍的設定名稱、值與範圍。

| 設定 | 預設 | 範圍 | 意思 |
| --- | --- | --- | --- |
| `request_timeout` | 5 秒 | 100 ms–60 秒 | 一個請求等回應的時間（送出起算，不含共用連線等輪到的時間） |
| `connect_timeout` | 10 秒 | 100 ms–60 秒 | 連線的時間，含名稱解析；建立 `Endpoints`（或 `TcpConnection`）時傳入 |
| `watchdog` | 60 秒 | `connect_timeout + 2 × request_timeout`（預設 20 秒）–1 小時 | 設備在開著的連線上連續不回答多久，watchdog 就關掉連線 |
| `backoff_initial` | 1 秒 | 100 ms–60 秒 | 第一次連線失敗後的等待 |
| `backoff_max` | 60 秒 | `backoff_initial`–1 小時 | 連續連線失敗時等待的上限 |

輪詢間隔是每台設備設定的 `poll_interval_ms`（100 ms–1 天，`DesiredConfig` 驗證），`polling::poll_period(&device)` 轉成 `Duration`。TCP keepalive 的參數是固定值，不是設定（見下方）。

### 固定節拍

第 k 次輪詢預定在「迴圈開始 + k × 間隔」，由開始時間算，不由上一次輪詢的結束時間算，所以輪詢花多少時間都不會讓節拍漂移。一次輪詢超過下一個節拍時，它蓋過的節拍跳過、不補做（`Polled::missed` 記跳過幾個），下一次輪詢在它結束後的第一個節拍開始；剛好在節拍上結束就立刻開始下一次。退避期間的節拍也一樣跳過。

### 每次等待都有上限

| 等待 | 上限 |
| --- | --- |
| 連線（含名稱解析） | `connect_timeout` |
| 送出請求、等回應 | `request_timeout`；另外每個請求也受 watchdog 的期限限制 |
| 輪詢之間 | 睡到下一個節拍 |
| 共用連線等輪到 | 前面每台設備的請求各自受上面的上限限制 |

### 重連退避

一次輪詢的連線失敗（連不上、請求途中連線中斷、串流不是 Modbus，或被 watchdog 關掉，即 `ReadError::ends_poll()` 的失敗）之後，下一次輪詢至少等 `backoff_initial`，之後每次再失敗就加倍，最多 `backoff_max`，然後從下一個節拍開始。**只有讀到值的輪詢（至少一個資料點讀到並解碼成功）才把退避歸零**：連線成功不算，例外回應也不算（設備回答了，但沒有值），所有資料點都解碼失敗的回應也不算。所以設備接受連線之後立刻關掉或 reset，agent 不會以輪詢的頻率一直重連。只有逾時、例外或對不上的回應（連線還在）的輪詢不加長也不歸零退避，下一次輪詢照節拍。

### TCP keepalive

每條 Modbus 連線都開 TCP keepalive：閒置 30 秒後開始探測，每 10 秒一次，連續 3 次沒有回應，kernel 就關掉連線（`tcp.rs` 的 `KEEPALIVE_*`）。設備在兩次輪詢之間消失（斷電、拔線、路由器丟掉連線狀態）時，大約一分鐘後連線被判定失效，下一個請求直接開新連線，不必先在死連線上等到逾時。輪詢間隔短於 30 秒時連線不會閒置，探測不會送出，這時靠請求逾時與 watchdog。keepalive 看不出 TCP 還會回 ACK、但設備永遠不回答的情況（Laysi/trustiot#164），那是 watchdog 的工作。

### Watchdog

這是每台設備各自的 watchdog，只關掉該設備的 Modbus 連線；與重啟整個服務的 systemd watchdog（見上方「systemd」）無關。設備一直連著卻不回答時，每個請求都逾時，而逾時後連線保留（見「失敗與輪詢」），輪詢永遠不會自己恢復。每台設備的 watchdog 處理這種情況：

- 計時起點：設備上一次回答之後，第一個沒得到回答的請求送出的時間（`WatchdogFired::silent_since`）。回答是讀到值或例外回應；連線失敗會讓計時歸零（連線已經沒了，由退避處理）。
- 觸發：沒有回答的請求結束時，如果已經從起點過了 `watchdog`，就觸發；還在等的請求最遲在「起點 + `watchdog`」被中止，但一次輪詢開始後的 `connect_timeout + 2 × request_timeout`（一個請求最長能花的時間）內不會中止它，所以輪詢間隔比 watchdog 長的設備每次輪詢至少有一個完整的請求。
- 觸發時：關掉連線（`Disconnect::disconnect`），這個請求以 `ConnectionLost`（`connect`，說明「no answer for N ms: the watchdog closed the connection」）結束，這次輪詢其餘的請求不送；算一次連線失敗，下一次輪詢在退避之後的第一個節拍開新連線。`Polled::watchdog` 記下起點與觸發時間。
- 時間：連續輪詢的設備（例如間隔 1 秒、每次都逾時）在起點之後 `watchdog` 到 `watchdog + connect_timeout + 2 × request_timeout` 之間觸發，再過 `backoff_initial` 加最多一個間隔就在新連線上輪詢。輪詢間隔比 watchdog 長的設備，在起點之後第一個結束時已超過 `watchdog` 的沒有回答的請求觸發，也就是連續兩次輪詢沒有回答時。

`TcpConnection` 與 `SharedConnection` 都實作 `Disconnect`。`SharedConnection` 先等輪到才關，關掉的是整個端點的連線：同一台轉換器後面一台設備壞了而其他設備正常時，壞掉那台的 watchdog 每次觸發都讓其他設備重連一次（觸發之後重新計時，所以兩次觸發至少相隔 `watchdog`）。等輪到的時間也算在沒有回答的時間裡：同一個端點有很多台設備同時不回答時，正常的設備可能等超過 `watchdog` 才輪到，被誤判而關掉連線；要避免，同一個端點的設備數乘上 `request_timeout` 要小於 `watchdog`（預設值下是少於 12 台）。

## TPM 簽章

閘道器的身分金鑰在 TPM 裡、不能匯出（決策 12）。`TpmKey::open(TpmTransport::Device("/dev/tpmrm0".into()), handle)` 經核心的資源管理器使用持久 handle 上的金鑰（預設 `DEFAULT_KEY_HANDLE` = `0x81000100`），`TpmKey` 實作 rustls 的 `SigningKey`，inner mTLS 的 CertificateVerify 就在 TPM 內簽。

- 不用 tss-esapi 或 libtss2：只送兩個指令，TPM2_ReadPublic 與 TPM2_Sign，依 TPM 2.0 Part 2、3 自己編解碼，沒有 C 函式庫要交叉編譯或隨附。
- 回應一律當成不可信的輸入：檢查 tag、長度與回應碼，每個長度都不超過剩下的 bytes，結構要剛好結束，回應最多 4096 bytes；格式不符是錯誤，不會 panic。回應碼解成名稱，例如 `TPM_RC_HANDLE (0x0000018b, handle 1)`。
- `open` 先讀公開區域，金鑰必須是 ECC NIST P-256、scheme 固定為 ECDSA-SHA256，屬性要有 `fixedTPM`、`fixedParent`、`sensitiveDataOrigin`、`userWithAuth`、`sign`，不能有 `restricted`、`decrypt`（2026-10-12 PoC 的範本，產線工具 Laysi/trustiot-v2#80 照這個產生）。接著簽一段固定的測試訊息並以公鑰驗證，簽不出來（例如金鑰設了密碼）就在啟動時失敗，不會等到每次握手才失敗。啟動多一次簽章，閘道器上約 70 ms。
- 簽章：SHA-256 在軟體算，以空密碼的 password session（`TPM_RS_PW`）與 null hashcheck ticket 送 TPM2_Sign，r、s 轉成 DER。rustls 只會拿到 `ecdsa_secp256r1_sha256`。
- 憑證要與金鑰相符：`public_key_spki_der()` 是金鑰的 SubjectPublicKeyInfo，rustls 的 `CertifiedKey::keys_match` 經 `SigningKey::public_key` 比對。
- 一條專用執行緒（`tpm`）擁有裝置，請求經有上限的佇列（8 個）一個一個送，佇列滿了回 `Busy`。rustls 的 `Signer::sign` 是同步呼叫，等待期間佔住呼叫它的執行緒（閘道器上約 70 ms），所以握手不能跑在負責 Modbus 輪詢的執行緒上。等待最多 `RESPONSE_TIMEOUT`（10 秒，排隊也算在內；核心對這些指令的逾時更短）：簽章佔住連線執行緒，這段時間加一次進度計數要小於 systemd watchdog 通知的間隔（15 秒），否則 systemd 會在 agent 自己放棄之前重啟它（見上方 systemd）。執行緒停了（panic）時簽章立即回錯誤，不會卡住；最後一個 `TpmKey` 與 `Signer` 被丟掉時執行緒結束並關閉裝置。
- `TpmTransport::Channel` 接受其他 `TpmChannel`（測試以 TCP 連 swtpm）；正式環境只用 `Device`。

## 測試

```sh
cargo test -p agent                                     # 全部
cargo test -p agent --lib                               # 單元測試：設定、憑證、TLS、MQTT 編解碼、退避、心跳、sd_notify、liveness，以及 Modbus 的規劃、PDU、解碼、輪詢、MBAP 與 RTU 切框、共用連線的輪流、keepalive、一小時的節拍漂移、設定範圍
cargo test -p agent --test link                         # MQTT 行為，對腳本化的 broker：在線狀態、訂閱、in-flight 上限、PUBACK、keepalive、退避、協定錯誤、停止
cargo test -p agent --test liveness                     # systemd watchdog 的進度判斷：連線的每個等待都宣告期限、卡住的連線工作讓通知停下
cargo test -p agent --test transport                    # 整條連線在行程內：兩層 TLS 協商的內容、resumption、重連、每種拒絕、內層的憑證鏈、名稱、用途與根 CRL
cargo test -p agent --test broker                       # 對 Mosquitto 容器（podman 或 docker）：mTLS、retained、0x87、will、broker 與轉接重啟後重連
cargo test -p agent --test binary                       # 執行檔：--check（含根 CRL 的狀態）、sd_notify、SIGTERM
cargo test -p agent --test systemd_unit                 # unit 檔與 systemd-analyze
cargo test -p agent --test dependencies                 # 閘道器建置的相依
cargo test -p agent --test modbus_fake_endpoint         # 對 testkit 假端點：型別、合併讀取、每種故障
cargo test -p agent --test modbus_tcp_framing           # 以 raw socket 驗證哪個回應算數、框架同步
cargo test -p agent --test modbus_rtu_converter         # 對 testkit 假 RS485 轉換器：共用連線、輪流、廣播、站號與形狀的過濾
cargo test -p agent --test modbus_rtu_framing           # 以 raw socket 驗證 RTU 的請求 bytes、回送、雜訊、切斷的框架與重連
cargo test -p agent --test polling_recovery             # 對 testkit 假端點跑輪詢迴圈：watchdog 救回不回答與半開的連線、只有讀到值才重置退避
cargo test -p agent --test tpm_swtpm                    # swtpm 容器：簽章、mTLS 握手、拒絕不能用的金鑰
```

`tools/lab-dataset` 的錄製工具（`lab-dataset record`）以 library 使用 `modbus::endpoint::Endpoints`、`DeviceReader` 與 `polling::run`；改它們的介面時一併跑 `cargo test -p lab-dataset --test record`。

- `link` 與 Modbus 的整合測試都用停住的時鐘（`start_paused`，Modbus 另加 `testkit::clock::manual()`）：keepalive、退避與逾時只在其他事情都停下時才觸發，機器再忙也不會誤判。
- `transport`、`broker`、`binary` 走真的 socket，每個等待都以 `testkit::clock::bounded` 設上限。WebSocket 前端（替代入口與 mqtt-bridge）、測試 CA 與 Mosquitto 容器來自 `testkit::mqtt`，用法見 [`crates/testkit/README.md`](../../crates/testkit/README.md)「MQTT 測試環境」。所有憑證與金鑰在測試時產生，不進 repo。
- `broker` 需要 podman 或 docker：沒有時略過，`CI` 有設時失敗。
- 假 Modbus 端點的用法見 [`crates/testkit/README.md`](../../crates/testkit/README.md)「假 Modbus 端點」。
- `tpm_swtpm` 以 `tests/swtpm/Containerfile`（Debian 的 swtpm 與 tpm2-tools，基底以 multi-arch digest 釘選）建映像，build context 是空目錄；映像留在本機（`localhost/trustiot-agent-swtpm:<Containerfile 的雜湊>`），下次直接沿用，不要時以 `podman rmi` 移除。測試時以 tpm2-tools 佈建金鑰，比對 tpm2-tools 算的 SubjectPublicKeyInfo、以公鑰驗證簽章，並做一次真的 TLS 1.3 mTLS 握手（測試 CA 為 TPM 公鑰簽發用戶端憑證，CertificateVerify 剛好一次 TPM2_Sign，resumption 不簽章）。沒有 podman 或 docker 時略過，設了 `CI` 時失敗。解析與執行緒的行為在單元測試（`cargo test -p agent --lib tpm`）以 swtpm 錄下的回應與假 TPM 測。
