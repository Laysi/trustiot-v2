# mqtt-bridge 對外開放前的防護

Laysi/trustiot-v2#600。

## 環境

- **程式碼**：分支 `fix/600-mqtt-bridge-exposure-limits`：service-kit 與 mqtt-bridge 兩個 commit，基於 main `74216494`（開發時基於 `87b51f33`，rebase 後重跑整個 workspace 的檢查）；2026-10-24 安全審查之後的 fix commit，以及 merge main `6da9b15f`（tokio 1.53.3，RUSTSEC-2026-0386）。
- **整合測試與 clippy**：建置主機（經檢查佇列，x86_64 Linux，`CI=1`），在與工作目錄內容一致的複本上執行。Rust 1.99.0（`rust-toolchain.toml`）；tokio-tungstenite 0.30.0。測試直接啟動服務的執行檔（監聽 127.0.0.1 的隨機埠），前面放測試自己的假 broker（回聲、不回應、MQTT CONNECT／PINGREQ 的最小回應），不需要容器、叢集或網路。
- **Mosquitto 量測**：開發者工作站（x86_64 Linux），2026-10-23 13:54–13:57 UTC。chart 釘選的 broker 映像 `docker.io/library/eclipse-mosquitto@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408`（日誌：`mosquitto version 2.1.2 starting`），以 podman 起一個用完即刪的容器，放在自己的網路，只發布到 127.0.0.1。設定 [mosquitto.conf](data/2026-10-23-600-mqtt-bridge-exposure-limits/mosquitto.conf) 與 chart 的監聽設定相同的部分：TLS 1.3、要求用戶端憑證、只收 MQTT 5。CA 與憑證是當場產生、兩天到期、用完即刪的測試憑證，不在 repo 裡。量測腳本用 `.prototools` 的 Bun 執行。沒有碰 lab 或展示機。

## 指令

整合測試、clippy（建置主機）：

```sh
cargo fmt --all --check
cargo clippy -p service-kit -p mqtt-bridge --all-targets -- -D warnings
CI=1 cargo test -p mqtt-bridge
CI=1 cargo test -p service-kit --lib telemetry
cargo clippy --workspace --all-targets -- -D warnings
CI=1 cargo test --workspace
cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings
```

Mosquitto 量測（`<dir>` 是任一個暫存目錄；`certs/` 放 openssl 產生的 `ca.pem`、`server.pem`／`server.key`（SAN `localhost`、`127.0.0.1`）與 `client.pem`／`client.key`，全部 P-256）：

```sh
podman network create issue600-mosq
podman run -d --rm --name issue600-mosq --network issue600-mosq -p 127.0.0.1:18883:8883 \
  -v <dir>/certs:/certs:ro,Z -v <dir>/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro,Z \
  docker.io/library/eclipse-mosquitto@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408 \
  /usr/sbin/mosquitto -c /mosquitto/config/mosquitto.conf
bun measure.ts 18883 <dir>/certs
bun trickle.ts 18883
podman logs issue600-mosq
podman rm --force --volumes issue600-mosq
podman network rm issue600-mosq
```

腳本：[measure.ts](data/2026-10-23-600-mqtt-bridge-exposure-limits/measure.ts)（A、B、C）、[trickle.ts](data/2026-10-23-600-mqtt-bridge-exposure-limits/trickle.ts)（D、E）。

## 結果

### Mosquitto 對不前進的連線

[broker-timeouts.txt](data/2026-10-23-600-mqtt-bridge-exposure-limits/broker-timeouts.txt)、[mosquitto.log](data/2026-10-23-600-mqtt-bridge-exposure-limits/mosquitto.log)（用戶端位址遮成 `<client>`）。

| 情境 | 用戶端做什麼 | broker 切斷的時間（從 TCP 連上算） |
| --- | --- | --- |
| A | 只建立 TCP，什麼都不送 | 30 867 ms |
| B | 送一個 ClientHello（1401 bytes），之後不送 | 30 866 ms |
| C | 完成 mTLS（TLS 1.3、用戶端憑證），不送 MQTT CONNECT | 30 865 ms |
| D | ClientHello 每 5 秒送 1 byte | 30 706 ms |
| E | 送完 ClientHello，之後每 5 秒送 1 byte | 30 706 ms |

五條在 broker 日誌都是 `disconnected: exceeded timeout`。Mosquitto 2.1.2 對還沒完成 MQTT CONNECT 的連線，從 TCP 連上起約 30 秒就切斷，不管 TLS 交握做到哪裡，也不會因為對方持續慢慢送 byte 而延後。

### 測試

建置主機，2026-10-24，安全審查的修正與 merge main `6da9b15f` 之後：

- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings`：沒有問題。
- `CI=1 cargo test -p mqtt-bridge`：53 個通過（單元 30、整合 23）；`CI=1 cargo test -p service-kit --lib telemetry`：19 個通過。
- `cargo deny --locked check`：`advisories ok, bans ok, licenses ok, sources ok`。

建置主機，2026-10-23（審查之前）：

- `cargo test -p mqtt-bridge`（mqtt-bridge commit 的內容）：50 個測試全部通過（單元 27、整合 23），其中 `tests/exposure.rs` 6 個。`cargo clippy -p mqtt-bridge --all-targets -- -D warnings`、`cargo fmt --all --check`：沒有問題。
- `cargo test -p service-kit --lib telemetry`：19 個通過，包含 `log_ceilings_hold_whatever_the_filter_says` 與 `without_a_ceiling_the_filter_alone_decides`。
- rebase 到 main `74216494` 之後：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` 沒有問題，`cargo test --workspace` 1404 個通過、0 個失敗。rebase 之前另外跑過 `cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings`，沒有警告（rebase 只多了 assets 服務的變更，沒有改到 service-kit）。

| 測試 | 驗證什麼 |
| --- | --- |
| `exposure::a_connection_whose_broker_never_answers_is_ended_and_frees_its_slot` | broker 不回應（像 Mosquitto 等不到 ClientHello）、進展期限 500 ms、總上限 1：持有期間另一條回 503；期限過後連線結束，日誌原因 `handshake_timeout`、持續時間不少於 500 ms；用戶端到結束前都不讀、回不了 close，持續時間仍少於 500 ms 加上設定的 20 秒 close 等待（服務不等回應）；之後讀到 close 1008，下一條連線能轉送 |
| `exposure::progress_is_the_broker_answering_the_gateways_answer` | 期限 5 秒、回聲 broker：只走一輪（ClientHello、broker 的第一輪）的連線以 1008 結束；比它早開始、走了兩輪的連線期限也過了，仍能轉送，結束的只有一條 |
| `exposure::a_gateway_that_answers_no_ping_is_ended` | ping 300 ms、pong 逾時 3 秒：不再讀的連線以 `pong_timeout` 結束、沒有收到 close、持續時間不少於 3 秒；比它早開始、一直在讀的連線收到過 ping，PINGREQ 仍有 PINGRESP |
| `exposure::a_gateway_that_stops_reading_while_it_sends_is_ended` | 閘道器先走兩輪，之後只送不讀，塞滿兩邊的 TCP 緩衝：服務送往閘道器的寫入一直等，連線仍以 `pong_timeout` 結束、不少於 3 秒；唯一的名額給下一條連線 |
| `exposure::one_source_cannot_take_more_than_its_share` | 總上限 16、每來源 2：同一個 `CF-Connecting-IP` 的第三條回 429，各自偽造不同的 `X-Forwarded-For`／`True-Client-IP`／`X-Real-IP` 也一樣；另一個 IPv4 與另一個 IPv6 /64 不受影響，同一個 /64 裡的另一個位址被拒；IPv6 的 /56（上限 4）滿了時它的新 /64 被拒、/48（上限 8）分散在別的 /56 滿了之後新的 /56 也被拒，另一個 /48 不受影響；每個滿的桶只記一行 WARN（原因 `over_source_limit`、`client.address`、`source.bucket` 依序是 `203.0.113.7`、`2001:db8:1:2::/64`、`2001:db8:1::/56`、`2001:db8:1::/48`），同一個 IPv4 的第二次拒絕不另記一行；偽造的位址不在日誌；broker 的連線數等於持有的連線數；關掉一條後同一個來源能再連 |
| `exposure::tungstenite_trace_logging_cannot_be_turned_on` | `telemetry.log_filter` 設成 `info,tungstenite=trace,tokio_tungstenite=trace`：日誌有 tungstenite 的 DEBUG 行（過濾器確實放行到 DEBUG），沒有任何 TRACE 行，送出的 canary 不論文字或十六進位都不在輸出 |
| `limits::connection_and_traffic_metrics_reach_the_collector` | 每來源上限 1：203.0.113.7 兩次、它的 6to4 位址 `2002:cb00:7107::1` 一次，都回 429；OTLP 匯出的 `mqtt_bridge.connections.refused{reason=over_source_limit}` 為 3，日誌只有一行（`source.bucket` 是 `203.0.113.7`） |
| `relay::tests` 的進展與 pong 期限（5 個） | 進展要 broker、閘道器、broker 三輪，閘道器在 broker 開口前送的不算；暫停的時鐘下：沒回應的 ping 保留最早的期限、有回應之後下一個 ping 從自己的時間起算、有回應的 ping 不會到期 |
| `source_limit::tests`（6 個）、`config::tests` | 來源以 IPv4 或 IPv6 /64 計，/56 與 /48 另有兩倍、四倍的上限，被拒的升級不佔任何桶、每個名額只還一次；IPv4-mapped 與 6to4 位址算成它的 IPv4；每個桶一分鐘一行、下一行帶出略過的次數，追蹤的桶滿了時次數由下一行的 `suppressed_elsewhere` 帶出（以指定的時間點測，不等一分鐘）；三個新設定的預設值、上下限與越界的拒絕，每來源上限不超過總上限 |

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| relay 開始後，內層連線在設定時間內沒有進展的連線被結束、名額釋出（測試用縮短的時間） | 通過 | `exposure::a_connection_whose_broker_never_answers_is_ended_and_frees_its_slot`（期限 500 ms、上限 1 條：持有期間第二條回 503；close 1008、原因 `handshake_timeout`、持續時間不少於 500 ms；之後下一條連線能轉送）、`exposure::progress_is_the_broker_answering_the_gateways_answer`（期限 5 秒：只走一輪的連線被結束；比它早開始、走了兩輪的連線仍能轉送）；進展的狀態機另有單元測試 |
| 送出 ping 之後沒有回應的連線在 pong 逾時後被結束 | 通過 | `exposure::a_gateway_that_answers_no_ping_is_ended`（ping 300 ms、逾時 3 秒：不再讀的連線以 `pong_timeout` 結束、沒有 close、不早於 3 秒；比它早開始、一直在讀的連線仍能轉送）、`exposure::a_gateway_that_stops_reading_while_it_sends_is_ended`（閘道器一直送、不讀，塞滿兩邊的 TCP 緩衝：仍以 `pong_timeout` 結束，名額給下一條連線） |
| 同一個來源 IP 超過上限的升級被拒絕，其他來源不受影響，偽造轉送標頭無法繞過 | 通過 | `exposure::one_source_cannot_take_more_than_its_share`（每來源 2 條：第三條回 429，偽造的 `X-Forwarded-For`／`True-Client-IP`／`X-Real-IP` 不改變計算；其他 IPv4 與其他 /64 不受影響，同一個 /64 的另一個位址被拒；一個 /48 分散在多個 /64 也不能超過它的上限，另一個 /48 不受影響；拒絕寫 WARN 日誌、原因 `over_source_limit`、每個桶一分鐘一行、偽造的位址不在日誌；broker 的連線數等於持有的連線數；釋出的名額能再用）、`limits::connection_and_traffic_metrics_reach_the_collector`（6to4 位址與它的 IPv4 同一個桶；三次拒絕，`refused{reason=over_source_limit}` 為 3、日誌一行）；`source_limit` 單元測試 |
| `telemetry.log_filter` 開到 `tungstenite=trace` 時日誌仍沒有 frame 內容 | 通過 | `exposure::tungstenite_trace_logging_cannot_be_turned_on`（`info,tungstenite=trace,tokio_tungstenite=trace`：有 tungstenite 的 DEBUG 行，沒有任何 TRACE 行，canary 的文字與十六進位都不在輸出）、`service-kit` 的 `log_ceilings_hold_whatever_the_filter_says` 與對照組 `without_a_ceiling_the_filter_alone_decides` |
| README 寫明上限、預設值、Mosquitto 對沒有 ClientHello 的連線的切斷時間與 Cloudflare 的決定；runbook 寫明速率限制怎麼設定 | 通過 | `services/mqtt-bridge/README.md` 的「設定」「保持連線」「連線上限」「日誌」；`docs/runbooks/demo-environment.md` 的「`/mqtt` 的速率限制」 |

Cloudflare 速率限制規則本身還沒套用：套用是有 Cloudflare 權限的人照 runbook 做的人工步驟。1,000 條連線與叢集滾動更新的驗證是 Laysi/trustiot-v2#599 的範圍，這次不測。
