# mqtt-bridge（MQTT 轉接）

把閘道器經入口送來的 WebSocket 拆回 TCP，原封不動交給 MQTT broker 的閘道器埠（決策 6、27；[部署與可搬移性](../../docs/architecture/deployment.md)的服務表）。Laysi/trustiot-v2#85 建立（2026-10-23）。

```text
閘道器 ──wss://<hosts.gateway>/mqtt──> Cloudflare ──tunnel──> 入口 gateway ──> mqtt-bridge ──TCP──> broker:8883
          └──────────────── 內層 mTLS（TLS 1.3），閘道器直達 broker ────────────────────────────────┘
```

- 入口的例外路徑 `ingress.publicPaths.mqtt`（預設 `/mqtt`，`hosts.gateway`，不經 cookie 檢查）只導到這個服務；服務本身每個路徑都當成 WebSocket 端點，路徑由入口決定。
- 範圍外：閘道器這一側（Laysi/trustiot-v2#86）、地端的來源 IP 模式（Laysi/trustiot-v2#446）。

## 設定

設定的規則與通用設定（埠、日誌、遙測、關機時間）見 [crates/service-kit](../../crates/service-kit/README.md) 的「設定」。服務自己的：

| 環境變數 | 預設 | 說明 |
| --- | --- | --- |
| `TRUSTIOT_CLIENT_IP_SOURCE` | 必填（平台設定） | 來源 IP 從哪個標頭來，chart 依 `ingress.exposure` 推導。只接受 `cf-connecting-ip`（`tunnel`）；`x-forwarded-for-rightmost`（地端與 `direct`）在 Laysi/trustiot-v2#446 之前拒絕啟動，其他值也拒絕啟動（錯誤訊息不回顯設定值）。見「來源 IP」 |
| `TRUSTIOT__BROKER__ADDRESS` | 必填 | broker 閘道器埠的 `host:port`（IPv6 位址要加中括號）。`mqttBroker.enabled` 時 chart 設為 `<release>-mqtt-broker.<namespace>.svc:8883`，沒開 broker 時不設，服務拒絕啟動 |
| `TRUSTIOT__BROKER__CONNECT_TIMEOUT` | `5s` | 連 broker 的 TCP 連線最多等多久，逾時回 502；1 ms 到 20 s，而且要比 `http.request_timeout` 短，否則拒絕啟動 |
| `TRUSTIOT__WEBSOCKET__PING_INTERVAL` | `50s` | 每隔多久對閘道器送一次 WebSocket ping；100 ms 到 100 s（Cloudflare 125 秒無資料就切斷，上限保留一次延遲的空間） |
| `TRUSTIOT__WEBSOCKET__CLOSE_TIMEOUT` | `5s` | 服務送出 close 之後（關機、broker 結束連線、收到文字訊息），等閘道器回 close 多久就直接斷線；1 ms 到 20 s |
| `TRUSTIOT__MAX_CONNECTIONS` | `1536` | 同時轉送的連線上限，超過的升級回 503；1 到 65536。乘上每條連線最壞的 128 KiB 超過容器記憶體上限的四分之三時拒絕啟動（見「記憶體」） |

## 轉送

- 升級前依序檢查，任何一項不合就回 HTTP 錯誤、不碰 broker：關機中（503 `draining`）；WebSocket 握手（GET、HTTP/1.1、`Connection: upgrade`、`Upgrade: websocket`、`Sec-WebSocket-Version: 13`、16 位元組的 `Sec-WebSocket-Key`，405／400／426）；`Sec-WebSocket-Protocol` 要有 `mqtt`（閘道器要求回應帶它，400 `subprotocol`）；可信的來源 IP（400，見下一節）；連線上限（503 `over_capacity`）；連 broker（502 `broker_unavailable`）；連 broker 的期間收到 SIGTERM（503 `draining`，否則這條連線可能在 drain 結束後才開始，收不到 close）。之後才回 101，回應帶 `Sec-WebSocket-Protocol: mqtt`。
- WebSocket 的二進位訊息就是閘道器到 broker 的內層 TLS。服務把每則訊息的內容照原樣、照順序寫給 broker，broker 送來的位元組照順序切成最多 8 KiB 的二進位訊息送回。服務沒有任何金鑰，不終結、不解析、也不記錄這段 TLS；ping、pong 不轉送。
- 文字訊息不屬於 MQTT over WebSocket：收到就送 close 1003，結束連線，內容不轉給 broker。broker 結束 TCP 連線時送 close 1000，broker 連線出錯時送 1011。
- 連 broker 的 TCP 關掉 Nagle（`TCP_NODELAY`），與 broker 那一側的 `set_tcp_nodelay true` 相同；閘道器那一側的 socket 由 service-kit 的 HTTP 伺服器接受，服務設不到（見「待辦」）。
- 閘道器送來的單則訊息或 frame 超過 32 KiB（閘道器每則最多送 32 KiB，Laysi/trustiot-v2#86）就結束連線，不轉送任何內容。tungstenite 一讀到 frame 標頭就依宣告的長度預留記憶體，所以這個上限決定了只送標頭的用戶端能讓服務佔用多少（見「記憶體」）。

測試：`tests/relay.rs`（位元組雙向不變、訊息大小、內層 mTLS 經服務完成而且明文不進日誌、文字訊息、broker 結束連線）、`tests/limits.rs`（上限、broker 連不上、子協定）。

## 來源 IP

決策 6：broker 看不到閘道器的真實 IP，所以由這個服務依入口傳來的可信標頭記錄；稽核與限流以憑證身分為準。

- SaaS（`cf-connecting-ip`）只讀 `CF-Connecting-IP`：Cloudflare 自己設定這個標頭，用戶端送的會被覆蓋。`X-Forwarded-For`（最左段由用戶端決定，Cloudflare 照樣往後附加）、`True-Client-IP`、`X-Real-IP`、`Forwarded` 一律不看，也不在可信標頭缺少時拿來遞補。
- 可信標頭必須剛好一個、值是單一個 IPv4 或 IPv6 位址（前後空白可以），否則回 400（`client_ip_missing`、`client_ip_repeated`、`client_ip_invalid`），不連 broker。入口直接把服務的路徑交給這個服務，服務只在入口後面（mesh 只放行入口 gateway；`mesh.calls` 把它列為任何服務的被呼叫端時 chart 渲染失敗，否則那個服務能自己設這個標頭，還能經它連到 broker 的閘道器埠），所以標頭只可能是 Cloudflare 或入口設的。
- 地端（`x-forwarded-for-rightmost`，入口 gateway 附加在 X-Forwarded-For 最右段的位址）是 Laysi/trustiot-v2#446，在那之前這個值讓服務拒絕啟動，不會退回去讀別的標頭。

記錄在哪裡（2026-10-23 決定，docs 沒有寫明）：在服務自己的日誌，經 OpenTelemetry collector 進 ClickHouse，與其他服務的日誌相同。每條連線兩行：

| 訊息 | 欄位 |
| --- | --- |
| `relaying a gateway connection`（INFO） | `connection.id`（隨機 16 位十六進位）、`client.address`（可信的來源 IP）、`broker.local_address`（服務連 broker 的本端位址與埠）、`broker.peer_address` |
| `gateway connection ended`（INFO） | `connection.id`、`client.address`、`broker.local_address`、`reason`、`duration_ms`、`bytes_to_broker`、`bytes_to_gateway` |

被拒的升級記一行 `refused a gateway WebSocket`（WARN，`reason`，知道來源 IP 時加上 `client.address`）。日誌只有位址、計數與原因，沒有任何轉送的位元組。

對應 broker 的日誌：Mosquitto 開著 `connection_messages`，每條連線記 `New connection from <位址>:<埠>`，接著是用憑證 CN 當 client id 的 `New client connected ... as <閘道器 ID>`。broker 看到的位址在沒有 mesh 時就是服務的 `broker.local_address`（`tests/source_ip.rs` 驗證兩者相同），兩行可以直接以位址與埠對上，得到「閘道器 ID ↔ 來源 IP」。在 Istio ambient 下，服務到 broker 的連線經過兩端的 ztunnel，broker 看到的來源埠不一定等於服務的本端埠（還沒在叢集上確認），這時只能以服務 pod 的位址與時間對：同一個 pod 同一秒內只有一條新連線時是唯一的。要在每條連線都能精確對上，選項是 broker 接受 PROXY protocol（要改決策 6 的 broker 設定）或由 broker 這一側記錄服務傳來的連線 ID，兩者都列為後續工作。

測試：`tests/source_ip.rs`（偽造的 X-Forwarded-For、True-Client-IP、X-Real-IP、Forwarded 不改變記錄的 IP，也不出現在日誌；缺少、重複、格式錯誤的可信標頭回 400 而且不連 broker）、`tests/startup.rs`（不認得的模式與地端模式拒絕啟動）。

## 保持連線

閘道器的 MQTT keepalive 設 60 秒（決策 6），但 Cloudflare 對兩個方向都沒有資料的 WebSocket 在 125 秒切斷，keepalive 0 的連線會被切。服務每 `websocket.ping_interval`（預設 50 秒）送一個空的 WebSocket ping，閘道器的 tungstenite 自動回 pong，連線就不會被當成閒置。ping 的計時從連線開始算，漏掉的不補送。

測試：`tests/keepalive.rs` 把時間縮小，不等 125 秒：一個在 3 秒無資料就切斷的代理擋在前面，服務每 200 ms ping，keepalive 0 的 MQTT 連線閒置 9 秒仍是同一條（PINGREQ 有回應、broker 只看到一條連線、代理沒有切過）；把間隔設成 100 秒時，同一個代理把連線切掉。

## 關機

收到 SIGTERM 時（service-kit 的 `requested`）：

1. readiness 立刻變成 503，入口不再把新連線送來；還是送到的升級回 503 `draining`。
2. 每條轉送中的連線立刻停止轉送，送 close 1001（going away），等閘道器回 close 最多 `websocket.close_timeout`，再關掉 broker 連線。閘道器收到 close 就重連，入口把它交給還就緒的 pod，不必等 grace period 結束（2026-10-10 的可行性測試中，沒有這一步的轉接服務讓閘道器多等了 30 秒）。
3. service-kit 的 drain 不等已升級的連線，服務自己用一個 task 等它們都結束，所以行程在每條連線都送出 close 之後才結束（chart 的 `drainSeconds` 預設 30 秒，足夠 `close_timeout` 的上限 20 秒）。

測試：`tests/shutdown.rs`：6 條連線（其中 1 條從不回 close）時送 SIGTERM，每條會讀的連線都在行程結束前收到 1001（close frame 只可能由還在執行的服務送出），各自立刻重連到第二個服務實例並完成 MQTT CONNECT；drain 期間的升級回 503；行程以 0 結束，每條連線的結束原因都是 `shutdown`，broker 那一側的連線全部關閉。滾動更新時「1 秒內連上新 pod」在叢集上的實測見「待辦」。

## 指標

經 service-kit 的 OpenTelemetry（`telemetry.otlp_endpoint` 有設定時）匯出，meter 名稱 `mqtt-bridge`：

| 指標 | 型別 | 屬性 | 說明 |
| --- | --- | --- | --- |
| `mqtt_bridge.connections.active` | UpDownCounter | | 目前轉送中的連線 |
| `mqtt_bridge.connections.opened` | Counter | | 開始轉送的連線 |
| `mqtt_bridge.connections.ended` | Counter | `reason` | 結束的連線：`gateway_closed`、`gateway_error`、`text_message`、`broker_closed`、`broker_error`、`shutdown` |
| `mqtt_bridge.connections.refused` | Counter | `reason` | 被拒的升級：握手（`method`、`http_version`、`not_upgrade`、`websocket_version`、`websocket_key`、`subprotocol`）、來源 IP（`client_ip_missing`、`client_ip_repeated`、`client_ip_invalid`）、`draining`、`over_capacity`、`broker_unavailable`、`upgrade_failed` |
| `mqtt_bridge.traffic` | Counter（`By`） | `direction`：`to_broker`、`to_gateway` | 轉送的位元組（WebSocket 訊息的內容，不含 frame 標頭） |

測試：`tests/limits.rs` 以假的 collector 收下關機時匯出的指標，逐項比對連線數、原因與位元組數。

## 記憶體

緩衝區（`src/relay.rs`）：

- WebSocket 讀取緩衝區 8 KiB（tungstenite 預設 128 KiB）；寫入不另外緩衝：每則訊息送出（寫進 socket）之後才從 broker 讀下一段，閘道器讀得慢時服務也停止從 broker 讀，壓力留在 TCP 上；寫入緩衝區的硬上限是 16 KiB。
- 從 broker 讀取的緩衝區 8 KiB，送給閘道器的訊息也因此最多 8 KiB。
- 閘道器送來的 frame 與訊息最多 32 KiB。

`/mqtt` 不需要任何憑證就能升級，所以連線上限要以任何人都做得到的最壞情況估，不能以閒置的閘道器估：分段組裝中的訊息（32 KiB）加上下一個 frame 標頭宣告後預留的 32 KiB、從 broker 讀的 8 KiB、寫入緩衝區最多 16 KiB，再加上 task、socket 與 kernel 的 socket 緩衝區（cgroup 也算進去），取整為每條 128 KiB（`relay::WORST_CASE_PER_CONNECTION`）。服務啟動時讀自己 cgroup v2 的 `memory.max`，`max_connections` × 128 KiB 超過它的四分之三就拒絕啟動（`src/memory.rs`），其餘留給執行環境、日誌與遙測的緩衝；沒有上限（`max`）或讀不到時不檢查。預設 1,536 條剛好是服務預設 256 MiB 上限的四分之三。要收更多連線，`services.mqtt-bridge.resources.limits.memory` 與 `TRUSTIOT__MAX_CONNECTIONS`（`services.mqtt-bridge.env`）一起調高。2026-10-10 估的 SaaS 規模是 2,000 台閘道器、兩個副本，平常每個副本約 1,000 條；一個副本不在時，超過上限的閘道器收到 503 後重試，直到另一個副本回來。

2026-10-23 的安全審查（Laysi/trustiot-v2#85）發現：原本 frame 與訊息上限是 64 KiB、預設上限 4,096 條，以閒置連線估在 256 MiB 內，但只送 frame 標頭就能讓每條連線佔 64 KiB 以上，開滿上限就超過記憶體上限、pod 被 kernel 終止，所有閘道器一起斷線。`tests/limits.rs` 驗證超過 32 KiB 的 frame（只送標頭）與分段組成的訊息都會結束連線、不轉送任何內容，`src/memory.rs` 的單元測試驗證預設值符合預設的記憶體上限、超過時拒絕啟動。

2026-10-10 的可行性測試（[docs/feasibility/2026-10-10-edge-poc.md](../../docs/feasibility/2026-10-10-edge-poc.md)）以 PoC 的轉接服務量測 1,000 條連線的記憶體增量：緩衝區 128 KiB 時兩個副本合計 +159 MiB，8 KiB 時 +32 MiB。這個服務沿用 8 KiB 的設定，但還沒有以這個實作量測：1,000 條連線的量測是效能量測，屬於進階驗證，另開 issue 補做（見「待辦」）。

## 測試

```sh
cargo test -p mqtt-bridge
```

整合測試直接啟動服務的執行檔（`env!("CARGO_BIN_EXE_mqtt-bridge")`），前面放假的 broker（回聲、MQTT CONNECT／PINGREQ 的最小回應、要求用戶端憑證的 TLS 1.3），不需要容器、叢集或網路。等待一律以條件為準，上限 120 秒，逾時的訊息寫明在等什麼；只有 `tests/shutdown.rs` 的 drain 延遲（10 秒）與 `tests/keepalive.rs` 的代理切斷時間（3 秒）是測試自己設定的時間。

## 待辦

- 滾動更新時閘道器在 1 秒內連上新 pod：服務這一側（SIGTERM 時每條連線先收到 close）由 `tests/shutdown.rs` 驗證；在叢集上經入口的實測要等閘道器這一側（Laysi/trustiot-v2#86）與部署。
- 1,000 條連線的記憶體增量，以 2026-10-10 的數字為基準；以及開滿上限、每條只送 frame 標頭時的實際用量，確認 128 KiB 的估計。
- 未經驗證的連線佔用名額（安全審查 Medium）：開始轉送後一段時間內 broker 都沒有回應（內層 TLS 沒有進展）就結束連線、pong 逾時、每個來源 IP 的連線上限，以及 Cloudflare 對 `/mqtt` 升級的速率限制；也要確認 Mosquitto 對連上後不送 ClientHello 的連線何時切斷。
- 日誌層級：`telemetry.log_filter` 打開 `tungstenite` 的 trace 時，tungstenite 會把 frame 內容（內層 TLS 的密文）寫進日誌（安全審查 Low）；服務應固定這個 target 的上限。
- nightly 的冒煙測試部署這個服務。
- 閘道器那一側的 socket 關掉 Nagle（service-kit 接受連線時設定）。
- 在叢集上確認 broker 看到的來源埠，或以 PROXY protocol／連線 ID 讓 broker 日誌精確對上來源 IP。
