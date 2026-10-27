# mqtt-client

雲端服務連 MQTT broker 服務埠（1883，只在 mesh 內）的 MQTT 5 用戶端（Laysi/trustiot-v2#631）。使用者：certs 送 dynamic security 的管理指令（Laysi/trustiot-v2#633）、ingest 收遙測並回確認（Laysi/trustiot-v2#641）、gateway-ops 下發設定（Laysi/trustiot-v2#93）。雲端服務一律經這個 crate 連 broker，不各自接 MQTT 函式庫；閘道器 agent 有自己的用戶端（見下方「為什麼這樣用 rumqttc」）。

broker 那一側（帳號、角色、服務埠的設定）見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「MQTT broker」；topic 與 payload 的格式見 [crates/protocol](../protocol/README.md)。

## 用法

```rust
use mqtt_client::{Client, Event, PublishOptions};
use protocol::topic::Channel;

// config 是服務設定裡的 `mqtt` 區段（mqtt_client::Config），服務的 Validate 呼叫 config.validate()。
let subscriptions = vec![format!("$share/ingest/{}", Channel::Telemetry.filter())];
let (client, mut events) = Client::start(&config, subscriptions)?;

while let Some(event) = events.recv().await {
    match event {
        // 第一次連上與每次重連都有；subscriptions 是每個訂閱的 SUBACK 原因碼。
        Event::Connected { subscriptions } => { /* 需要時做一次完整同步 */ }
        // 連不上或斷線；client 在 retry_in 之後自己重連。
        Event::Disconnected { error, retry_in } => { /* 記錄、更新 readiness */ }
        Event::Message(message) => {
            // message.topic、message.payload、message.retain
            let delivery = Channel::TelemetryAck.delivery();
            let options = PublishOptions { retain: delivery.retain, message_expiry: delivery.message_expiry };
            client.publish(&ack_topic, ack_payload, options).await?;
        }
    }
}
```

- `Client::start` 檢查設定、讀一次密碼檔（不能用就在啟動時失敗）、檢查每個 topic filter，然後在目前的 Tokio runtime 起一個 task 持有連線，第一次連線立刻開始。
- `Client` 可以 clone；最後一個 clone 被 drop 或呼叫 `Client::shutdown` 時送 DISCONNECT、結束 task，`Events::recv` 送完剩下的事件後回 `None`。shutdown 走自己的通道，in-flight 已滿、broker 不回 PUBACK 時也會結束。
- `Client::publish` 一律 QoS 1，等 PUBACK 才回來：原因碼 0x80 以上是 `PublishError::Refused { code, reason }`（例如沒有權限的 topic 是 0x87），0x10（沒有訂閱者）算成功。沒有連線時立刻回 `NotConnected`，不排隊，連線建立中（含 `Client::start` 之後第一次連上之前）也是，所以要發布的呼叫端先等 `Event::Connected`；future 在送出前被取消，那則 publish 就不送；送出後、PUBACK 前斷線是 `ConnectionLost`（broker 可能收到了，也可能沒有）；封包超過 broker 的上限時回 `TooLarge`，不送出、不斷線。
- 訂閱在 `Client::start` 時給，每次連線（含重連）都重新訂閱，一個 SUBSCRIBE 帶全部的 filter，SUBACK 之後才送 `Event::Connected`。shared subscription 寫成 `$share/<群組>/<filter>`。被拒的訂閱（0x80 以上）照樣回報在 `Connected` 裡並記警告，連線不斷；要不要因此停止服務由呼叫端決定。
- 要一直讀 `Events`：QoS 1 的訊息在 `Events::recv` 交出時才回 PUBACK，broker 最多先送 `RECEIVE_MAXIMUM`（32）則未確認的訊息，所以呼叫端不讀，訊息就停在 broker，不會在服務裡無限累積；QoS 0 的訊息在 `Events` 裡已有 `MAX_QUEUED_MESSAGES`（1024）則時丟棄並記警告。在處理訊息的迴圈裡 `await` 一個 publish 不會卡死：連線 task 不等呼叫端讀事件。
- 每次連線都是新的 session（clean start）：斷線期間的訊息不保留，`Event::Connected` 之後該重送的狀態（例如 gateway-ops 的 desired 設定）由呼叫端重送。PUBACK 在交給呼叫端時就送出，服務在處理完之前當掉，那則訊息就沒了；需要確實送達的資料靠應用層的確認（遙測的 `TelemetryAck`）。

## 設定

服務把 `mqtt_client::Config` 放進自己的設定型別（例如欄位 `mqtt`），鍵就是 `mqtt.host` 這些；`Config::validate` 在服務啟動前檢查，錯誤訊息只寫鍵名，不寫值。

| 鍵 | 預設 | 說明 |
| --- | --- | --- |
| `host` | 必填 | broker 的主機名稱或 IP，叢集內是 `<release>-mqtt-broker.<namespace>.svc` |
| `port` | `1883` | 服務埠 |
| `username` | 必填 | `service:<服務>`，服務名稱是 1 到 63 個小寫字母、數字與 `-`；就是 chart 建立的 broker 帳號 |
| `password_file` | 必填 | 密碼檔的路徑：chart 把 broker 帳號 Secret 裡這個服務的 key 掛成檔案（Laysi/trustiot-v2#634） |

沒有 TLS 的設定，也沒有任何關掉檢查的設定：client 以明文 MQTT 連服務埠，加密與身分由 mesh（ztunnel 的 mTLS）負責，AuthorizationPolicy 只放行有帳號的服務。

其他值是常數，不能設定：

| 常數 | 值 | 理由 |
| --- | --- | --- |
| `KEEP_ALIVE` | 30 秒 | broker 的 `max_keepalive` 是 120 秒；broker 在 CONNACK 指定另一個值時照它的。每個 keepalive 送一次 PINGREQ，下一次之前沒收到 PINGRESP 就當成斷線 |
| `CONNECT_TIMEOUT` | 10 秒 | TCP 連線、CONNECT／CONNACK、SUBSCRIBE／SUBACK 加起來的上限 |
| `WRITE_TIMEOUT` | 30 秒 | 一次寫入卡住超過這個時間就當成斷線 |
| `MAX_INFLIGHT` | 16 | 同時等 PUBACK 的 publish 上限；broker 的 Receive Maximum 較小時用它的（chart 的 broker 是 20）。滿了之後的 publish 等空位 |
| `RECEIVE_MAXIMUM` | 32 | 送給 broker 的 Receive Maximum：未確認的 QoS 1 訊息上限 |
| `MAX_PACKET_BYTES` | 1 MiB + 8 KiB | 兩個方向的 Maximum Packet Size，等於 `protocol::topic::MIN_MAX_PACKET_BYTES`（[crates/protocol](../protocol/README.md)「傳輸：MQTT 5」）。送出的上限取它與 broker 在 CONNACK 給的值中較小者 |
| `RECONNECT_DELAY_MIN`、`RECONNECT_DELAY_MAX` | 0.5 秒、30 秒 | 重連的等待從上限 0.5 秒起每次失敗加倍，最多 30 秒，實際等待是上限的一半到上限之間的隨機值，同時斷線的副本不會同時回來。連線維持 `STABLE_CONNECTION`（60 秒）以上才把等待重設回最小值，連上又立刻斷的情況不會變成快速重試 |

## client id 與密碼

- client id 是 `service:<服務>:<16 位隨機 hex>`，每個實例不同。一定有 `:`，閘道器 ID 不能有 `:`（`protocol::GatewayId`），所以服務的連線永遠接不走閘道器的 session；同一個服務的副本也不會互相踢掉。被入侵的服務仍可自己選任何 client id，這個風險見 deploy/chart/README.md「持久化與限制」。
- 密碼每次連線（含重連）都重讀檔案，輪替後的新密碼在下一次連線生效，不必重啟服務。檔案最後一個換行不算密碼的一部分；空檔、超過 `MAX_PASSWORD_BYTES`（1024）或有可列印 ASCII 以外的字元都拒絕（chart 產生的密碼只有可列印 ASCII，`tools/migrate/src/secret.rs`）。重連時讀不到檔案，回報 `Event::Disconnected` 並照常退避重試。
- 密碼的型別 `Password` 在 drop 時清除記憶體（`zeroize`），`Debug` 只印 `Password(<redacted>)`。CONNECT 封包直接編碼到一個一次配置好大小的緩衝區、寫進 socket 之後清除，不經過 codec 的緩衝區；錯誤訊息只寫封包種類，不寫封包內容。`tests/logging.rs` 在 TRACE 下擷取所有 `tracing` 事件與 `log` 紀錄，確認登入、被拒、發布、收訊息的過程中沒有任何一段 5 個字元的密碼片段出現。

## 為什麼這樣用 rumqttc

- **雲端用 rumqttc，閘道器不用。** 閘道器 agent（Laysi/trustiot-v2#86）自己寫 MQTT 5 的編解碼與狀態機，因為它要在 WebSocket 裡跑內層 TLS，rumqttc 不能接自訂的串流。雲端服務在 mesh 裡以明文 TCP 連 1883，沒有這個限制，就不再寫一份編解碼（2026-10-24 拆分 Laysi/trustiot-v2#82 時決定）。
- **只用 rumqttc 的封包型別與 codec（`rumqttc::v5::mqttbytes`），連線、packet id、keepalive、重連由本 crate 自己做。** rumqttc 0.25.1 的 `EventLoop` 不能把 PUBACK 的原因碼交還給發出那則 publish 的呼叫端：它自己配 packet id，事件裡只有 packet id，而且原因碼不是成功時只記一行警告。要對回去，只能依事件的先後去猜，連線中斷、id 衝突、SUBSCRIBE 與 PUBLISH 共用 id 時都可能把 0x87 交給錯的呼叫端。自己配 packet id 就能一對一對應，也能做到「交給呼叫端時才回 PUBACK」的流量控制。代價是 keepalive、packet id、重連這些（`src/driver.rs`）由我們維護與測試；MQTT 5 的編解碼仍是 rumqttc 的。
- **記憶體。** 可行性測試記下 rumqttc 的 `EventLoop` 預設每條連線預配約 13 MB（`MqttState` 依 in-flight 上限預配，要設 `set_outgoing_inflight_upper_limit`；docs/feasibility/2026-10-09-cloud-poc.md「MQTT broker」第 5 點）。本 crate 不用 `MqttState`，每條連線只有 codec 的讀寫緩衝區（隨最大的封包成長，上限是 `MAX_PACKET_BYTES`）、最多 `MAX_INFLIGHT` 個等待中的 publish、最多 `RECEIVE_MAXIMUM` 加 `MAX_QUEUED_MESSAGES` 則交給呼叫端的訊息。實際量測每條連線的記憶體延後到另開的 P2 issue。
- **維護狀態。** rumqttc 最後一次發版是 0.25.1（2025-11）。不開預設 feature：不引入 TLS 與任何 TLS 後端（aws-lc 等），也不引入 WebSocket 與 proxy。只有這個 crate 依賴它，要換函式庫時只動這裡。

## 測試

- `cargo test -p mqtt-client`：設定與密碼檔的檢查、原因碼、退避、client id 不需要 broker；`tests/stand_in.rs` 以本機回送位址上的假 broker 測 in-flight 已滿時的 shutdown，以及連線建立中的 publish 立刻回 `NotConnected` 而且不會在連上後送出。
- `tests/broker.rs` 與 `tests/logging.rs` 以 podman 或 docker 起拋棄式的 Mosquitto 2.1.2（與 chart 同一個映像與 digest，開 dynamic security，只發布到本機回送位址），沒有容器執行環境時略過，設了 `CI` 時失敗：`CI=1 cargo test -p mqtt-client -- --nocapture` 印出測試報告引用的 `evidence:` 行。涵蓋：正確密碼登入、錯的密碼被拒（Mosquitto 2.1.2 的 dynamic security 回 CONNACK 0x87 not authorized，不是 0x86）、直接與 shared 訂閱的 SUBACK 與被拒的訂閱（0x87）、1 MiB 的 payload 原樣收回、沒有權限的 topic 回 0x87、超過上限的封包不送出也不斷線、同一帳號的兩個實例同時連線、broker 重啟後自動重連並通知、broker 換了密碼後在密碼檔更新前被拒、更新後的下一次連線就用新密碼。
- broker 容器的設定在 `tests/support/mod.rs`，不用 testkit 的 `mqtt::mosquitto`：那是閘道器路徑用的，只把 mTLS 的閘道器埠（8883，憑證當使用者名稱）發布到主機，帳密的 listener 只在容器內；這裡要測的是服務埠 1883 的帳密登入、只收 MQTT 5、`max_packet_size`、服務角色（含 `$share`）與換密碼。兩者釘同一個映像與 digest，`scripts/nightly/renovate.test.ts` 數這些副本。
