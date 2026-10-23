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
| `TRUSTIOT__BROKER__HANDSHAKE_TIMEOUT` | `10s` | 開始轉送之後，內層連線要在多久內有進展（broker 送出資料、閘道器回應、broker 再送出資料），否則送 close 1008 結束連線；100 ms 到 60 s。見「連線上限」 |
| `TRUSTIOT__WEBSOCKET__PING_INTERVAL` | `50s` | 每隔多久對閘道器送一次 WebSocket ping；100 ms 到 100 s（Cloudflare 125 秒無資料就切斷，上限保留一次延遲的空間） |
| `TRUSTIOT__WEBSOCKET__PONG_TIMEOUT` | `30s` | 送出 ping 之後，閘道器多久沒有送來任何東西（pong、資料或 ping）就結束連線，不送 close；送給閘道器的一則訊息多久沒被收走也一樣；100 ms 到 100 s。見「保持連線」 |
| `TRUSTIOT__WEBSOCKET__CLOSE_TIMEOUT` | `5s` | 服務送出 close 之後（關機、broker 結束連線、收到文字訊息），等閘道器回 close 多久就直接斷線；1 ms 到 20 s |
| `TRUSTIOT__MAX_CONNECTIONS` | `1536` | 同時轉送的連線上限，超過的升級回 503；1 到 65536。乘上每條連線最壞的 128 KiB 超過容器記憶體上限的四分之三時拒絕啟動（見「記憶體」） |
| `TRUSTIOT__MAX_CONNECTIONS_PER_IP` | `32` | 每個來源（可信的來源 IP；IPv6 以 /64 計）同時轉送的連線上限，超過的升級回 429；1 到 65536，大於 `max_connections` 時以 `max_connections` 為準（只調低總上限的小型部署不必跟著改）。每個副本各自計算。見「連線上限」 |

## 轉送

- 升級前依序檢查，任何一項不合就回 HTTP 錯誤、不碰 broker：關機中（503 `draining`）；WebSocket 握手（GET、HTTP/1.1、`Connection: upgrade`、`Upgrade: websocket`、`Sec-WebSocket-Version: 13`、16 位元組的 `Sec-WebSocket-Key`，405／400／426）；`Sec-WebSocket-Protocol` 要有 `mqtt`（閘道器要求回應帶它，400 `subprotocol`）；可信的來源 IP（400，見下一節）；這個來源的連線上限（429 `over_source_limit`，見「連線上限」）；連線上限（503 `over_capacity`）；連 broker（502 `broker_unavailable`）；連 broker 的期間收到 SIGTERM（503 `draining`，否則這條連線可能在 drain 結束後才開始，收不到 close）。之後才回 101，回應帶 `Sec-WebSocket-Protocol: mqtt`。
- WebSocket 的二進位訊息就是閘道器到 broker 的內層 TLS。服務把每則訊息的內容照原樣、照順序寫給 broker，broker 送來的位元組照順序切成最多 8 KiB 的二進位訊息送回。服務沒有任何金鑰，不終結、不解析、也不記錄這段 TLS；ping、pong 不轉送。
- 文字訊息不屬於 MQTT over WebSocket：收到就送 close 1003，結束連線，內容不轉給 broker。broker 結束 TCP 連線時送 close 1000，broker 連線出錯時送 1011。
- 連 broker 的 TCP 關掉 Nagle（`TCP_NODELAY`），與 broker 那一側的 `set_tcp_nodelay true` 相同；閘道器那一側的 socket 由 service-kit 的 HTTP 伺服器接受，服務設不到（見「待辦」）。
- 閘道器送來的單則訊息或 frame 超過 32 KiB（閘道器每則最多送 32 KiB，Laysi/trustiot-v2#86）就結束連線，不轉送任何內容。tungstenite 一讀到 frame 標頭就依宣告的長度預留記憶體，所以這個上限決定了只送標頭的用戶端能讓服務佔用多少（見「記憶體」）。

測試：`tests/relay.rs`（位元組雙向不變、訊息大小、內層 mTLS 經服務完成而且明文不進日誌、文字訊息、broker 結束連線）、`tests/limits.rs`（上限、broker 連不上、子協定）、`tests/exposure.rs`（「連線上限」與「日誌」）。

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

被拒的升級記一行 `refused a gateway WebSocket`（WARN，`reason`，知道來源 IP 時加上 `client.address`）。日誌只有位址、計數與原因，沒有任何轉送的位元組（見「日誌」）。

對應 broker 的日誌：Mosquitto 開著 `connection_messages`，每條連線記 `New connection from <位址>:<埠>`，接著是用憑證 CN 當 client id 的 `New client connected ... as <閘道器 ID>`。broker 看到的位址在沒有 mesh 時就是服務的 `broker.local_address`（`tests/source_ip.rs` 驗證兩者相同），兩行可以直接以位址與埠對上，得到「閘道器 ID ↔ 來源 IP」。在 Istio ambient 下，服務到 broker 的連線經過兩端的 ztunnel，broker 看到的來源埠不一定等於服務的本端埠（還沒在叢集上確認），這時只能以服務 pod 的位址與時間對：同一個 pod 同一秒內只有一條新連線時是唯一的。要在每條連線都能精確對上，選項是 broker 接受 PROXY protocol（要改決策 6 的 broker 設定）或由 broker 這一側記錄服務傳來的連線 ID，兩者都列為後續工作。

測試：`tests/source_ip.rs`（偽造的 X-Forwarded-For、True-Client-IP、X-Real-IP、Forwarded 不改變記錄的 IP，也不出現在日誌；缺少、重複、格式錯誤的可信標頭回 400 而且不連 broker）、`tests/startup.rs`（不認得的模式與地端模式拒絕啟動）。

## 保持連線

閘道器的 MQTT keepalive 設 60 秒（決策 6），但 Cloudflare 對兩個方向都沒有資料的 WebSocket 在 125 秒切斷，keepalive 0 的連線會被切。服務每 `websocket.ping_interval`（預設 50 秒）送一個空的 WebSocket ping，閘道器的 tungstenite 自動回 pong，連線就不會被當成閒置。ping 的計時從連線開始算，漏掉的不補送。

pong 逾時（Laysi/trustiot-v2#600）：送出 ping 之後 `websocket.pong_timeout`（預設 30 秒）內，閘道器沒有送來任何東西（pong、二進位訊息或它自己的 ping 都算），服務就結束連線（原因 `pong_timeout`），不送 close（對方已經不回應），名額立刻釋出。計時以最早一個還沒得到回應的 ping 為準，之後的 ping 不延後它；收到任何東西之後，那個 ping 就算有回應，下一個 ping 重新計時。這個期限獨立於送往閘道器的寫入：閘道器不再讀取時，寫入會因 TCP 背壓一直等，所以送給閘道器的每一則訊息（broker 的資料或 ping）也要在 `pong_timeout` 內送出，否則同樣以 `pong_timeout` 結束；不然一個先送了資料、之後不再讀取的連線，會讓下一個 ping 永遠送不出去而一直佔著名額。所以閘道器斷電或網路中斷時，最晚 `ping_interval` + `pong_timeout`（預設 80 秒）後釋出名額，不必等 TCP 自己逾時。這只處理不再回應的對方：會自動回 pong 的 client 不受影響，佔用名額的攻擊由「連線上限」的其他項目處理。

測試：`tests/exposure.rs` 把 ping 間隔縮成 300 ms、pong 逾時縮成 3 秒：不再讀取的連線以 `pong_timeout` 結束，沒有收到 close，結束時已轉送至少 3 秒；比它早開始、一直在讀的連線仍能轉送。另一個測試的閘道器一直送資料給回聲的 broker、自己不讀，直到兩邊的 TCP 緩衝都塞滿：連線同樣以 `pong_timeout` 結束，名額釋出給下一條連線。`tests/keepalive.rs` 把時間縮小，不等 125 秒：一個在 3 秒無資料就切斷的代理擋在前面，服務每 200 ms ping，keepalive 0 的 MQTT 連線閒置 9 秒仍是同一條（PINGREQ 有回應、broker 只看到一條連線、代理沒有切過）；把間隔設成 100 秒時，同一個代理把連線切掉。

## 連線上限

`/mqtt` 不經 Cloudflare Access，任何人都能升級，所以名額要防止被少數來源佔滿、讓真正的閘道器連不上（2026-10-23 對 Laysi/trustiot-v2#85 的安全審查 Medium，Laysi/trustiot-v2#600）。服務分三層處理，Cloudflare 的速率限制在最前面：

| 防護 | 擋什麼 | 設定與預設 |
| --- | --- | --- |
| 每個來源的連線上限 | 少數來源開滿 `max_connections` | `max_connections_per_ip`，32 |
| 內層連線的進展期限 | 連上之後不完成內層 TLS 的連線一直佔著名額 | `broker.handshake_timeout`，10 秒 |
| pong 逾時 | 對方已經消失（斷電、斷網）的連線一直佔著名額 | `websocket.pong_timeout`，30 秒（「保持連線」） |
| Cloudflare 的速率限制 | 同一個 IP 短時間內大量升級 | 每個 IP 每 10 秒 40 次，2026-10-23 決定（[展示環境 runbook](../../docs/runbooks/demo-environment.md)「`/mqtt` 的速率限制」） |

**每個來源的連線上限**：來源就是「來源 IP」一節的可信來源 IP（SaaS 只讀 `CF-Connecting-IP`），所以用戶端偽造的 X-Forwarded-For、True-Client-IP、X-Real-IP 改變不了它算在哪個來源。IPv4 每個位址一個來源；IPv6 以 /64 為一個來源，因為一個用戶通常拿到整個 /64，可以任意換位址；IPv4-mapped 的 IPv6 位址算成那個 IPv4。超過時回 429 `over_source_limit`，記一行 `refused a gateway WebSocket`（WARN，`reason`、`client.address`），計入 `mqtt_bridge.connections.refused`；這時還沒有升級，沒有任何 frame。這一項排在總上限之前，所以總名額已滿時，超過自己上限的來源仍得到 429；不重試別的 pod（503 才可能被入口重試）。計數在每個副本各自做，兩個副本時一個來源最多約 64 條。預設 32 的理由：同一個場域的閘道器可能共用一個對外位址（NAT），要留空間；一個來源最多佔預設總上限的 1/48，開滿總上限至少要 48 個來源（IPv6 是 48 個 /64）。這是產品判斷，場域的閘道器更多時以 `services.mqtt-bridge.env` 調高。計數表只有開著連線的來源，最多 `max_connections` 筆。

**內層連線的進展期限**：服務看不到內層 TLS，只能看位元組的順序。開始轉送之後 `broker.handshake_timeout` 內，必須依序出現「broker 送出資料、閘道器送出資料、broker 再送出資料」，否則服務送 close 1008（policy）結束連線，原因 `handshake_timeout`。broker 要求用戶端憑證（TLS 1.3），它的第二輪只會出現在閘道器送出第二段交握之後：交握完成時的 session ticket 與 CONNACK，或是拒絕時的 alert（之後 broker 自己關閉連線）。所以連上之後什麼都不送、只送 ClientHello 就停，或送不完整的 ClientHello 的連線，都在期限到時結束；交握完成之後，期限就不再適用。極限：broker 的第一段交握被切成兩次讀取、而對方剛好在中間送了任意位元組時，服務會誤以為有進展；這時仍由下面 Mosquitto 自己的 30 秒期限結束連線。預設 10 秒：TLS 1.3 的交握是兩個來回加上閘道器以 TPM 簽章，慢的行動網路也在幾秒內完成。

**Mosquitto 自己何時切斷**（2026-10-23 以 chart 鎖定的映像 `eclipse-mosquitto:2.1.2-alpine`、與 chart 相同的 8883 listener 設定實測，[測試報告](../../docs/test-reports/2026-10-23-600-mqtt-bridge-exposure-limits.md)）：沒有完成 MQTT CONNECT 的連線，從 TCP 連上起約 30 秒被 broker 結束（日誌 `disconnected: exceeded timeout`），不論是只連 TCP 不送 ClientHello、送了 ClientHello 就停、完成 mTLS 交握但不送 CONNECT，或每 5 秒送一個位元組：送進來的位元組不會延後它。broker 結束時服務送 close 1000（`broker_closed`）。所以沒有這個服務的期限，一條連線也最多佔 30 秒；服務的期限更短，也不依賴 broker 的版本與設定。

**還擋不住的**：會完成 TCP、在期限內反覆重連的來源。每條連線最多佔 10 秒，要一直佔滿預設的 1,536 個名額，每秒要開約 150 條新連線，而且至少來自 48 個來源（每個來源 32 條）；Cloudflare 的速率限制讓每個 IP 每秒最多 4 次升級，也要約 38 個 IP 才開得出這個速度。擁有大量 IP 的攻擊者仍能做到，這時以 Cloudflare 的防護與擴充副本處理。

測試：`tests/exposure.rs`（時間都縮小、由測試設定）：

- 不回應的 broker 前，連線在 500 ms 的期限後收到 1008、原因 `handshake_timeout`，名額釋出後下一條連線就能轉送；期限內佔著唯一的名額時，別的連線得到 503。
- 只完成一輪（送一則、收到回音就停）的連線以 `handshake_timeout` 結束；比它早開始、多完成一輪的連線（期限比它早到）仍在轉送。
- 同一個來源 IP 第 3 條被拒（429），帶著不同的 X-Forwarded-For、True-Client-IP、X-Real-IP 也一樣，偽造的位址不出現在日誌，被拒的升級沒有連到 broker；別的 IPv4 與別的 /64 不受影響，同一個 /64 的第 3 個位址被拒；關掉一條之後同一個來源又能連上。
- `tests/limits.rs` 的指標測試核對 `refused{reason=over_source_limit}`。

## 日誌

`telemetry.log_filter` 設成 `tungstenite=trace` 或 `trace` 時，tungstenite 會在 TRACE 寫出每個讀寫的 frame，含內容的十六進位（內層 TLS 的密文），二進位訊息內容剛好是 UTF-8 時還會以文字寫出（2026-10-23 的安全審查 Low）。服務以 service-kit 的日誌上限（`ServiceInfo::with_log_ceilings`，[crates/service-kit](../../crates/service-kit/README.md)「日誌與 trace」）把 `tungstenite` 與 `tokio_tungstenite` 固定在 DEBUG（`src/lib.rs` 的 `LOG_CEILINGS`），怎麼設都打不開它們的 TRACE；它們的 DEBUG 只有收到的 close frame 與錯誤。服務自己只記位址、計數與原因。

測試：`tests/exposure.rs` 以 `info,tungstenite=trace,tokio_tungstenite=trace` 啟動，送一則內容可辨識的訊息經 broker 回來：日誌有 tungstenite 的 DEBUG 行（設定確實到了它），沒有它的 TRACE 行，內容與它的十六進位都不在任何一行。service-kit 的單元測試另外確認沒有上限時同樣的 TRACE 事件會被寫出。

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
| `mqtt_bridge.connections.ended` | Counter | `reason` | 結束的連線：`gateway_closed`、`gateway_error`、`text_message`、`broker_closed`、`broker_error`、`shutdown`、`handshake_timeout`、`pong_timeout` |
| `mqtt_bridge.connections.refused` | Counter | `reason` | 被拒的升級：握手（`method`、`http_version`、`not_upgrade`、`websocket_version`、`websocket_key`、`subprotocol`）、來源 IP（`client_ip_missing`、`client_ip_repeated`、`client_ip_invalid`）、`draining`、`over_source_limit`、`over_capacity`、`broker_unavailable`、`upgrade_failed` |
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

整合測試直接啟動服務的執行檔（`env!("CARGO_BIN_EXE_mqtt-bridge")`），前面放假的 broker（回聲、不回應、MQTT CONNECT／PINGREQ 的最小回應、要求用戶端憑證的 TLS 1.3），不需要容器、叢集或網路。等待一律以條件為準，上限 120 秒，逾時的訊息寫明在等什麼；測試自己設定的時間只有 `tests/shutdown.rs` 的 drain 延遲（10 秒）、`tests/keepalive.rs` 的代理切斷時間（3 秒），以及 `tests/exposure.rs` 的進展期限（500 ms、5 秒）與 pong 逾時（3 秒）；那些測試只斷言順序，以及連線不早於設定的期限結束。假的 broker 不說內層 TLS，所以其他測試的進展期限設成上限 60 秒（`tests/support` 的 `Bridge::start`）。

## 待辦

- 滾動更新時閘道器在 1 秒內連上新 pod：服務這一側（SIGTERM 時每條連線先收到 close）由 `tests/shutdown.rs` 驗證；在叢集上經入口的實測要等閘道器這一側（Laysi/trustiot-v2#86）與部署。
- 1,000 條連線的記憶體增量，以 2026-10-10 的數字為基準；以及開滿上限、每條只送 frame 標頭時的實際用量，確認 128 KiB 的估計。
- nightly 的冒煙測試部署這個服務。
- 閘道器那一側的 socket 關掉 Nagle（service-kit 接受連線時設定）。
- 在叢集上確認 broker 看到的來源埠，或以 PROXY protocol／連線 ID 讓 broker 日誌精確對上來源 IP。
