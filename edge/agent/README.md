# agent

閘道器 agent（[決策 10](../../docs/architecture/decisions.md)）：在現場閘道器上讀取設備、把資料送上雲端的程式。目前只有 Modbus 讀取（`modbus` 模組，[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) F1、F2、F5）：Modbus TCP 與 RS485 轉換器的 RTU over TCP，同一個端點的設備共用一條連線；以及每台設備的輪詢排程（`polling` 模組，F4）：固定節拍、逾時、重連退避、TCP keepalive 與 watchdog。systemd 服務、MQTT 連線與離線緩衝之後建立在它們上面。

| 在哪裡 | 內容 | issue |
| --- | --- | --- |
| `src/modbus.rs` | `DeviceReader`：依型號規劃讀取、送出、解碼並蓋時間戳；`Connection`、`Clock` 介面 | Laysi/trustiot-v2#87 |
| `src/modbus/plan.rs` | 合併讀取的規劃 | Laysi/trustiot-v2#87 |
| `src/modbus/pdu.rs` | 功能碼 1–4 的請求與回應 PDU | Laysi/trustiot-v2#87 |
| `src/modbus/decode.rs` | 型別、位元組順序、scale 與 offset | Laysi/trustiot-v2#87 |
| `src/modbus/tcp.rs` | 兩種傳輸方式共用的 `Connection`（`TcpConnection`）：連線管理、送出、接收到找到回應或逾時 | Laysi/trustiot-v2#87、Laysi/trustiot-v2#88 |
| `src/modbus/address.rs` | 連線位址的限制：主機只解析一次，只連檢查過的位址（見「連線位址的限制」） | Laysi/trustiot-v2#653 |
| `src/modbus/mbap.rs` | Modbus TCP 的框架（MBAP）：交易 ID、哪個回應算數 | Laysi/trustiot-v2#87、Laysi/trustiot-v2#88 |
| `src/modbus/rtu.rs` | RTU over TCP 的框架：CRC、由功能碼與 byte 數切框、重新同步、哪個回應算數 | Laysi/trustiot-v2#88 |
| `src/modbus/endpoint.rs` | `Endpoints`、`SharedConnection`：同一個端點的設備共用一條連線、逐個請求輪流 | Laysi/trustiot-v2#88 |
| `src/polling.rs` | `run`：每台設備的輪詢迴圈，固定節拍、重連退避、watchdog；`Settings`、`Disconnect` | Laysi/trustiot-v2#89 |
| 還沒有 | 系統時鐘的 `Clock`（時鐘是否已校正、校正來源） | Laysi/trustiot-v2#90 |
| 還沒有 | 讀取統計（`DeviceStats`）與心跳 | Laysi/trustiot-v2#117 |
| 還沒有 | 執行檔、systemd unit、MQTT 連線 | Laysi/trustiot-v2#86 |

依賴都要能建置成 aarch64（`aarch64-unknown-linux-gnu`），不需要 C 工具鏈；CI 對 `edge/` 下每個成員做這項建置。

## Modbus 讀取

每台設備建一個 `DeviceReader::new(&device, &model)`（`crates/protocol` 的 `config::Device` 與它的 `DeviceModel`，已經過 `DesiredConfig` 的驗證），連線向一個 `Endpoints` 要：`let mut connection = endpoints.connection(&device)`（見下方「共用連線」）。每次輪詢呼叫 `reader.poll(&mut connection, &clock, timeout)`，得到 `Poll`：

- `readings`：每個讀到並解碼成功的資料點一筆 `telemetry::Reading`，時間是該請求的回應到達時 `Clock::now()` 的毫秒時間，帶 `clock_synced`。
- `failed`：其他每個資料點與原因（`health::LastFailure`：種類、時間、例外碼、說明）。
- `requests`：送出或連線失敗的每個請求、它的失敗與丟棄的站號不符回應數，給之後的讀取統計（H1）用。

### 只讀

請求只能從 `protocol::config::FunctionCode` 建立，它只有 1–4，所以程式送不出功能碼 5 以上（閘道器對現場設備唯讀）。整合測試在每次輪詢後檢查假端點的請求紀錄，確認沒有 1–4 以外的功能碼。

### 連線位址的限制

設備的主機由租戶的 editor 在雲端設定。雲端的設備 API（Laysi/trustiot-v2#100）只擋字面上指向閘道器本身的寫法，那是輸入檢查，不是安全邊界：雲端無法知道一個 DNS 名稱在現場解析成什麼，也擋不住檢查之後才改變的解析結果（DNS rebinding）。若 agent 照設定連到閘道器本機，輪詢就成了探測閘道器本機服務的管道，回應還會變成遙測回到雲端。所以真正的把關在閘道器連線的那一刻（`src/modbus/address.rs`，Laysi/trustiot-v2#653）：

- 每次開連線時把主機解析一次（`tokio::net::lookup_host`；IP 位址的寫法不查 DNS，但走同一條檢查），逐一檢查解析出的位址，再直接連到檢查過的 `SocketAddr`。名稱不會再交給連線函式，所以不會有第二次解析。重連時重新解析、重新檢查。
- 比對前先把 IPv4-mapped 的 IPv6 位址轉回 IPv4（`::ffff:127.0.0.1` 就是 `127.0.0.1`），連線也連到轉回後的位址。
- 拒絕的位址：
  - loopback：127.0.0.0/8、`::1`
  - 未指定：0.0.0.0/8（`0.0.0.0` 與 RFC 1122 的「本網路」，連過去都會到閘道器本身）、`::`
  - 閘道器自己網卡上的位址：每次連線時以 `getifaddrs` 讀目前的介面位址（不論介面是否啟用），因為 DHCP 可能改變它們
  - 多播：224.0.0.0/4、`ff00::/8`
  - 廣播：`255.255.255.255`，以及閘道器每個 IPv4 網段的廣播位址（由位址與 netmask 算出；/31 與 /32 沒有廣播位址，/31 的另一個位址是對端）
  - 雲端 metadata：`169.254.169.254`、`fd00:ec2::254`
  - IPv4-compatible 的 `::/96`（`::a.b.c.d`，RFC 4291 2.5.5.1 已廢止；`::` 與 `::1` 照上面的規則）：整段拒絕
  - 帶著 IPv4 位址、轉譯器或中繼可能轉送過去的 IPv6 寫法：內嵌的 IPv4 位址被上面任何一條 IPv4 規則（loopback、0.0.0.0/8、多播、廣播、metadata、閘道器自己的 IPv4 位址）拒絕時就拒絕，說明裡寫的是原本的 IPv6 位址。
    - NAT64 `64:ff9b::/96`：最後 32 位元
    - 本地用 NAT64 `64:ff9b:1::/48`（RFC 8215）：最後 32 位元，也就是業者在其中通常使用的 /96 配置；其中其他前綴長度的配置不解讀
    - 6to4 `2002::/16`：第 16–47 位元
- 其他位址都允許：169.254.0.0/16 的其他位址（沒有 DHCP 的 RS485 轉換器常用 APIPA 位址）、私有位址（10/8、172.16/12、192.168/16、IPv6 ULA），以及閘道器自己網段裡的其他位址，現場設備就在這些網段。IPv6 link-local（`fe80::/10`）不在拒絕之列，但這種設備實際上連不上：Linux 不接受沒有 scope ID 的 link-local 連線，而 `Host` 不收 zone ID，DNS 回答的 scope 也是 0。
- 名稱解析成多個位址時，被拒絕的跳過，依解析的順序連允許的位址，第一個接受的就用；都連不上時回報最後一個錯誤。
- 所有位址都被拒絕時，不開 socket、不送任何封包，讀取以 `ReadError::Connect`（`connect`）失敗，說明是「refused: every address the host resolved to points at the gateway itself or is not allowed (<解析出的位址>)」，最多列 8 個位址，不帶解析結果以外的資料。讀不到介面位址時也不連線（fail closed），一樣是 `connect` 失敗。這和連不上一樣算連線失敗：其餘請求不送，下一次輪詢在退避之後重試，所以名稱之後解析成允許的位址時會自己恢復。

正式的路徑（`Endpoints::new`、`TcpConnection::new`）一律檢查，沒有任何設定能關掉它。測試的假 Modbus 端點（`crates/testkit` 的 `FakeModbus`）聽在 127.0.0.1，所以測試要用 `exempt_for_tests([127.0.0.1])` 放行那個位址。這個方法只存在於本 crate 的單元測試，以及 `test-exempt-addresses` feature 打開時；只有 `Cargo.toml` 裡本 crate 對自己的 dev-dependency 打開它，所以只有測試建置才有，agent 的建置（`cargo build -p agent`）沒有。設定檔、環境變數或經 MQTT 下發的設定都碰不到它。`cargo test -p repo-check` 會在任何正式依賴、其他 feature 或 `default` 打開這個 feature 時失敗。在閘道器上以 systemd 執行 agent 的端到端測試（Laysi/trustiot-v2#102）用的是正式路徑，所以假 Modbus 要放在閘道器以外的位址（另一個網路命名空間或另一台主機），不能放在閘道器本機。

這個檢查擋不到的：經路由器或 NAT 轉回閘道器的位址（含 hairpin NAT 與 port forward）、`64:ff9b:1::/48` 裡 /96 以外的 NAT64 配置、以 `ip route add local` 加入本機路由表但不在任何介面上的網段。這些由下一層的閘道器防火牆處理（Laysi/trustiot-v2#96），不在這裡處理。

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
| 主機只解析成被拒絕的位址（見「連線位址的限制」），或讀不到閘道器的介面位址 | `Connect` | `connect` | 其餘請求不送，沒有開 socket |
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

每條連線都開 TCP keepalive：閒置 30 秒後開始探測，每 10 秒一次，連續 3 次沒有回應，kernel 就關掉連線（`tcp.rs` 的 `KEEPALIVE_*`）。設備在兩次輪詢之間消失（斷電、拔線、路由器丟掉連線狀態）時，大約一分鐘後連線被判定失效，下一個請求直接開新連線，不必先在死連線上等到逾時。輪詢間隔短於 30 秒時連線不會閒置，探測不會送出，這時靠請求逾時與 watchdog。keepalive 看不出 TCP 還會回 ACK、但設備永遠不回答的情況（Laysi/trustiot#164），那是 watchdog 的工作。

### Watchdog

設備一直連著卻不回答時，每個請求都逾時，而逾時後連線保留（見「失敗與輪詢」），輪詢永遠不會自己恢復。每台設備的 watchdog 處理這種情況：

- 計時起點：設備上一次回答之後，第一個沒得到回答的請求送出的時間（`WatchdogFired::silent_since`）。回答是讀到值或例外回應；連線失敗會讓計時歸零（連線已經沒了，由退避處理）。
- 觸發：沒有回答的請求結束時，如果已經從起點過了 `watchdog`，就觸發；還在等的請求最遲在「起點 + `watchdog`」被中止，但一次輪詢開始後的 `connect_timeout + 2 × request_timeout`（一個請求最長能花的時間）內不會中止它，所以輪詢間隔比 watchdog 長的設備每次輪詢至少有一個完整的請求。
- 觸發時：關掉連線（`Disconnect::disconnect`），這個請求以 `ConnectionLost`（`connect`，說明「no answer for N ms: the watchdog closed the connection」）結束，這次輪詢其餘的請求不送；算一次連線失敗，下一次輪詢在退避之後的第一個節拍開新連線。`Polled::watchdog` 記下起點與觸發時間。
- 時間：連續輪詢的設備（例如間隔 1 秒、每次都逾時）在起點之後 `watchdog` 到 `watchdog + connect_timeout + 2 × request_timeout` 之間觸發，再過 `backoff_initial` 加最多一個間隔就在新連線上輪詢。輪詢間隔比 watchdog 長的設備，在起點之後第一個結束時已超過 `watchdog` 的沒有回答的請求觸發，也就是連續兩次輪詢沒有回答時。

`TcpConnection` 與 `SharedConnection` 都實作 `Disconnect`。`SharedConnection` 先等輪到才關，關掉的是整個端點的連線：同一台轉換器後面一台設備壞了而其他設備正常時，壞掉那台的 watchdog 每次觸發都讓其他設備重連一次（觸發之後重新計時，所以兩次觸發至少相隔 `watchdog`）。等輪到的時間也算在沒有回答的時間裡：同一個端點有很多台設備同時不回答時，正常的設備可能等超過 `watchdog` 才輪到，被誤判而關掉連線；要避免，同一個端點的設備數乘上 `request_timeout` 要小於 `watchdog`（預設值下是少於 12 台）。

## 測試

```sh
cargo test -p agent                                     # 全部
cargo test -p agent --lib                               # 單元測試：規劃、PDU、解碼、輪詢、MBAP 與 RTU 切框、共用連線的輪流、keepalive、一小時的節拍漂移、設定範圍、連線位址的限制
cargo test -p agent --test modbus_address_guard         # 正式路徑（Endpoints::new、TcpConnection::new）拒絕閘道器本機與 metadata 位址，假端點收不到連線
cargo test -p agent --test modbus_fake_endpoint         # 對 testkit 假端點：型別、合併讀取、每種故障
cargo test -p agent --test modbus_tcp_framing           # 以 raw socket 驗證哪個回應算數、框架同步
cargo test -p agent --test modbus_rtu_converter         # 對 testkit 假 RS485 轉換器：共用連線、輪流、廣播、站號與形狀的過濾
cargo test -p agent --test modbus_rtu_framing           # 以 raw socket 驗證 RTU 的請求 bytes、回送、雜訊、切斷的框架與重連
cargo test -p agent --test polling_recovery             # 對 testkit 假端點跑輪詢迴圈：watchdog 救回不回答與半開的連線、只有讀到值才重置退避
```

`tools/lab-dataset` 的錄製工具（`lab-dataset record`）以 library 使用 `modbus::endpoint::Endpoints`、`DeviceReader` 與 `polling::run`；改它們的介面時一併跑 `cargo test -p lab-dataset --test record`。

整合測試都用 `#[tokio::test(start_paused = true)]` 加 `testkit::clock::manual()`：逾時只在測試推進時鐘時觸發，機器再忙也不會誤判逾時。位址限制的測試例外：被拒絕的連線不等任何計時器，它們用真的時鐘與 60 秒的連線逾時，正常情況下碰不到。連到假端點的測試都以 `exempt_for_tests` 放行 127.0.0.1（見「連線位址的限制」）。位址限制的單元測試以替身解析器與撥號紀錄驗證只查一次、只撥檢查過的位址；替身只真的撥 loopback，所以測試不會對執行機器所在的網路送封包。假端點的用法見 [`crates/testkit/README.md`](../../crates/testkit/README.md)「假 Modbus 端點」。
