# protocol

閘道器與雲端之間的 MQTT 訊息格式（[功能重建清單](../../docs/requirements/rebuild-checklist.md) M4：資料格式只定義一次）。閘道器 agent 與雲端服務（資料接收、設定下發、閘道器營運）都依賴本 crate，格式一改，兩邊同時編譯失敗或固定樣本測試失敗，不會再出現舊系統三份拷貝各自漂移的情況。本文件是給人看的規格；型別與驗證規則以程式碼為準，兩者不一致時是本文件的錯。

| 模組 | 內容 |
| --- | --- |
| `topic` | topic 樹、`Channel`（每種訊息的方向、QoS、retain、到期時間、大小上限）、`parse_topic` |
| `acl` | Mosquitto dynsec 的閘道器角色與 ACL 規則 |
| `codec` | CBOR 編解碼、版本檢查、`Message` trait、錯誤與 `Issue` |
| `telemetry` | 遙測批次 `TelemetryBatch`、批次確認 `TelemetryAck`、依大小上限拆包的 `BatchBuilder` |
| `config` | desired 設定 `DesiredConfig`（型號、資料點、設備）與套用回報 `ConfigReport` |
| `health` | 在線狀態與 LWT `Status`、心跳 `Heartbeat`（運作指標與設備讀取統計） |
| `live` | 即時訂閱控制 `LiveControl`、即時值 `LiveSamples` |
| `logs` | 日誌上傳 `LogBatch`、拉取請求 `LogRequest`、拉取回應 `LogDump` |

測試：`cargo test -p protocol`。閘道器目標：先執行一次 `rustup target add aarch64-unknown-linux-gnu`，再 `cargo build --locked --target aarch64-unknown-linux-gnu -p protocol`；CI 以同樣方式在 job 中加入該目標（Laysi/trustiot-v2#67），不放進 `rust-toolchain.toml`，免得每個只做雲端服務的開發者與 CI job 都下載 aarch64 的標準函式庫。

## 傳輸：MQTT 5

閘道器以 443 的 WebSocket 連到 MQTT 轉接服務，再到 Mosquitto（[決策 6](../../docs/architecture/decisions.md)）；雲端服務在叢集內直接連 broker。雙方一律用 MQTT 5：

- 發布到 ACL 不允許的 topic 時，MQTT 5 回 PUBACK reason code 0x87（not authorized），MQTT 3.1.1 則照樣回 PUBACK 並默默丟掉訊息（[可行性測試](../../docs/feasibility/2026-10-09-cloud-poc.md) MQTT broker 段）。閘道器要把 reason code ≥ 0x80 的 PUBACK 當成發送失敗，遙測批次留在緩衝裡、記錄錯誤，不能當成已送出。
- 需要 MQTT 5 的訊息到期時間（message expiry interval），讓過時的控制訊息不會在離線閘道器的 session 裡排隊。
- 雲端服務以 shared subscription（`$share/<group>/...`）分攤訊息到多個副本：`Channel::shared_filter`。

broker 與每個用戶端接受的 maximum packet size 至少要是 `MIN_MAX_PACKET_BYTES`（1 MiB + 8 KiB）：broker 的設定由 Laysi/trustiot-v2#84 負責；MQTT 用戶端函式庫的預設上限常遠小於此，閘道器與雲端服務都要明確設定並以 1 MiB 的設定訊息測試。

## 編碼：CBOR

每個 payload 是一個 CBOR（RFC 8949）資料項：以欄位名稱為鍵的 map。選 CBOR 的理由：

1. **值的型別不會走樣。** 遙測值的型別由 CBOR 本身的型別表示：布林、整數、浮點數、文字各自不同，不需要另加型別欄位。JSON 分不出 `60` 與 `60.0`，而且許多 JSON 解析器把超過 2^53 的整數當成浮點數，電表累計值這類大整數會被悄悄改掉。浮點數以二進位傳送，往返完全相同；能無損縮成 f32 的值只佔 5 bytes。其他 CBOR 編碼器常把短的浮點數編成半精度（f16），接收端一樣接受。
2. **自我描述，新增欄位不破壞相容。** 欄位以名稱為鍵，接收端略過不認得的鍵，舊版程式可以讀新版訊息（見「版本與相容性」）。postcard、bincode 這類不帶欄位名稱的格式做不到，新增欄位就要全面同步升級；閘道器散在客戶現場，無法同步升級。
3. **解析有界、不必跳脫。** 字串與陣列都以長度為前綴。解碼前先走一遍 CBOR 結構：宣告的長度或項目數超過剩餘位元組、巢狀超過 `MAX_NESTING`、多餘的尾端位元組都直接拒絕，惡意 payload 只會失敗、不會配置記憶體（`tests/robustness.rs`、`tests/allocation.rs`）。不定長度的陣列與 map 可以接受；不定長度（分段）的字串一律拒絕，因為 `cbor4ii` 1.2.3 解這種字串時不會讀掉結尾的 break，後面的欄位會錯位。本 crate 的編碼器只產生定長的項目。
4. **標準、工具多、可轉成 JSON 看。** RFC 標準格式，各語言都有解碼器；固定樣本同時存一份 JSON，審查時看得懂格式改了什麼。
5. **純 Rust、能編到 aarch64。** 使用 `cbor4ii`（MIT），沒有 C 相依、不需要 protoc 之類的建置工具。

體積不是主要理由：本 crate 的樣本中 CBOR 比緊湊 JSON 小約 20%（遙測 613 對 750 bytes），因為大部分是 UUID 字串；遙測流量的大小主要靠合併成批次與拆包上限控制。

不選 Protobuf：要另外維護 schema 與程式碼產生，Rust 的 prost 建置時需要 protoc；欄位相容規則（以編號為準）也比較容易出錯。不選 MessagePack：與 CBOR 能力相近，但 CBOR 是 IETF 標準。

## Topic

所有 topic 都是六層：`trustiot/v1/gw/{gateway}/{up|down}/{channel}`。

- `v1` 是格式的主版本（`PROTOCOL_VERSION`）。不相容的變更使用新的 topic 樹，新舊閘道器可以並存，雲端同時服務兩者。
- `{gateway}` 是閘道器憑證的 CN（`GatewayId`），也是它的 MQTT username 與 client ID。broker 的 ACL 讓閘道器只能發布到自己的 `up/`、只能訂閱自己的 `down/`，所以雲端以 topic 判定閘道器身分，不採信 payload 內容（清單 I1）。
- `up` 是閘道器到雲端，`down` 是雲端到閘道器。
- 閘道器逐一訂閱每個 `down` channel（`gateway_subscriptions`），不訂閱 `down/#`，新版雲端新增的 channel 不會送到不認得它的舊 agent。

ID（閘道器、設備、串流、session、租約、請求 ID 與資料點鍵）都是 1–64 個 ASCII 英數字、`.`、`_`、`-`，以英數字開頭；不可能含有 `/`、`+`、`#`、`$`，放進 topic 不會變成萬用字元或系統 topic。

| Channel | Topic（`trustiot/v1/gw/{gateway}/` 之後） | 訊息 | 發送方 | QoS | retain | 到期 | 大小上限 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `Telemetry` | `up/telemetry` | `TelemetryBatch` | 閘道器 | 1 | 否 | 無 | 256 KiB |
| `TelemetryAck` | `down/telemetry-ack` | `TelemetryAck` | 資料接收服務 | 1 | 否 | 600 秒 | 4 KiB |
| `Config` | `down/config` | `DesiredConfig` | 設定下發服務 | 1 | 是 | 無 | 1 MiB |
| `ConfigReport` | `up/config-report` | `ConfigReport` | 閘道器 | 1 | 否 | 無 | 256 KiB |
| `Status` | `up/status` | `Status`（也是 LWT） | 閘道器、broker | 1 | 是 | 無 | 4 KiB |
| `Heartbeat` | `up/heartbeat` | `Heartbeat` | 閘道器 | 0 | 否 | 無 | 1 MiB |
| `Live` | `up/live` | `LiveSamples` | 閘道器 | 0 | 否 | 無 | 64 KiB |
| `LiveControl` | `down/live-control` | `LiveControl` | 即時值服務 | 1 | 否 | 30 秒 | 64 KiB |
| `Log` | `up/log` | `LogBatch` | 閘道器 | 1 | 否 | 無 | 256 KiB |
| `LogRequest` | `down/log-request` | `LogRequest` | 閘道器營運服務 | 1 | 否 | 300 秒 | 4 KiB |
| `LogDump` | `up/log-dump` | `LogDump` | 閘道器 | 1 | 否 | 無 | 256 KiB |

雲端以 `Channel::filter` 訂閱所有閘道器，例如 `trustiot/v1/gw/+/up/telemetry`；收到訊息後以 `parse_topic` 取出閘道器與 channel，不符合上表格式的 topic 一律拒絕。retain 的訊息不設到期時間，晚上線的閘道器（或雲端服務）一樣收得到。

## ACL（Mosquitto dynsec）

Mosquitto 以 `use_identity_as_username` 把憑證 CN 當成 username，dynsec 的 ACL topic 中 `%u` 會換成 username（`acl_file` 在 2.1 已棄用）。佈建時為每台閘道器建立 dynsec client 並綁定角色 `trustiot-gateway`（`GATEWAY_ROLE`），角色規則由 `gateway_role_acls()` 產生，可直接作為 `$CONTROL/dynamic-security/v1` 的 `createRole`／`modifyRole` 的 `acls` 參數：

| acltype | topic | allow |
| --- | --- | --- |
| `publishClientSend` | `trustiot/v1/gw/%u/up/telemetry` | 是 |
| `publishClientSend` | `trustiot/v1/gw/%u/up/config-report` | 是 |
| `publishClientSend` | `trustiot/v1/gw/%u/up/status` | 是 |
| `publishClientSend` | `trustiot/v1/gw/%u/up/heartbeat` | 是 |
| `publishClientSend` | `trustiot/v1/gw/%u/up/live` | 是 |
| `publishClientSend` | `trustiot/v1/gw/%u/up/log` | 是 |
| `publishClientSend` | `trustiot/v1/gw/%u/up/log-dump` | 是 |
| `subscribePattern` | `trustiot/v1/gw/%u/down/#` | 是 |
| `publishClientReceive` | `trustiot/v1/gw/%u/down/#` | 是 |
| `unsubscribePattern` | `trustiot/v1/gw/%u/down/#` | 是 |

發布規則是每個 `up` channel 一條精確 topic（`gateway_channel_acl_topic`），不用萬用字元：`up/#` 會讓閘道器在自己的 `up/` 底下任意新增 topic，並在每個 topic 留下一則 retained 訊息，數量沒有上限（Laysi/trustiot-v2#268，驗證報告 [2026-10-11-50-verify](../../docs/test-reports/2026-10-11-50-verify.md) 的 F3）。broker 上已註冊的角色不會跟著程式更新：新增 `up` channel，或角色是以修正前的 `up/#` 規則建立的（2026-10-11 時 Laysi/trustiot-v2#82 與 Laysi/trustiot-v2#84 都還沒有實作，broker 上沒有這樣的角色），都要以 `modifyRole` 帶 `acls: gateway_role_acls()` 換掉整份 ACL，再以 `getRole` 確認；`addRoleACL` 只會加在舊規則旁邊，`up/#` 仍然有效。換掉舊規則後，還要以空 payload 的 retained 訊息清掉 `up/` 下非協定 topic 殘留的 retained 訊息。這個同步流程由 Laysi/trustiot-v2#82 實作。

其餘一律依 broker 的預設拒絕。結果：閘道器只能發布到自己 `up/` 下的協定 channel（發到別台的 topic，或自己 `up/` 下不是協定 channel 的 topic，都得到 PUBACK 0x87）、只能訂閱自己的 `down/`；CN 含 `+`、`#`、`/` 的憑證 Mosquitto 不會代入 `%u`，什麼都讀不到，CA 也不會簽出這種 CN（`GatewayId` 的字元規則）。LWT 發布到 `up/status`，同樣在允許範圍內。雲端服務的身分、角色與 broker 的預設存取設定由 Laysi/trustiot-v2#84 決定；dynsec 的註冊與撤銷流程由 Laysi/trustiot-v2#82 實作。

**閘道器的 retained 訊息**（2026-10-11 決定）：

- 不能全面禁止。`up/status` 依協定是 retained（`Status` 與 LWT），晚上線的雲端服務靠它取得每台閘道器目前的在線狀態；其他 `up` channel 的 retain 一律為否（上方 topic 表與 `Channel::delivery`），閘道器 agent 不得對它們設 retain。
- dynsec 無法依 retain 旗標授權：它的 ACL 檢查只看 acltype、topic 與 client，不看 retain 旗標（Mosquitto 2.1.2 的 `plugins/dynamic-security/acl.c`），所以角色規則擋不住閘道器在 `up/telemetry` 等 channel 設 retain。broker 的 `retain_available false` 會關掉整個 broker（或 listener）的 retained，`up/status` 與 `down/config` 都需要，不能用。
- 實際的界限來自精確規則：同一個 topic 的新 retained 訊息會取代舊的，所以一台閘道器最多在 7 個 `up` channel 各留一則 retained 訊息，每則不超過 broker 的 maximum packet size。broker 層級的上限（maximum packet size、每個 client 的佇列與 retained 的數量和大小）由 Laysi/trustiot-v2#84 設定。
- 雲端服務訂閱時，broker 會重播 retained 訊息（retain 旗標為 1）。在 retain 為否的 channel 上收到 retain 旗標為 1 的訊息，表示閘道器違反協定：丟棄並記錄，不能當成新資料處理，否則每次重新訂閱都會再收一次同一則舊訊息。

`tests/broker.rs` 以真的 Mosquitto 2.1.2（與 `scripts/lib/stack.ts` 相同 digest 的容器，podman 或 docker；沒有容器執行環境時略過，設了 `CI` 則失敗）驗證：角色以 `gateway_role_acls()` 的 JSON 建立；每個 `up` channel 依 `Channel::delivery` 送達雲端，`up/status` 的 LWT 在斷線時送出；每個 `down` channel 送達閘道器；`up/` 下不是協定 channel 的 topic 得到 PUBACK 0x87、雲端收不到、broker 不留 retained，設在這種 topic 的 LWT 也被丟掉；修正前的 `up/#` 規則讓同一項檢查失敗，以 `modifyRole` 換掉 ACL 並清掉殘留的 retained 後通過；dynsec 確實不擋允許 topic 上的 retain 旗標。

## 共同欄位與版本

每個訊息的 map 都有 `v`（主版本，固定為 1）。可以帶 `rev`（協定修訂版）：接收端遇到比自己 `PROTOCOL_REVISION` 新的 `rev` 時拒絕整則訊息（`unsupported_revision`），不會誤讀自己不懂的新功能。目前只有 `DesiredConfig` 一定帶 `rev`。時間一律是 `UnixMillis`：1970-01-01T00:00:00Z 起算的毫秒數，來自寫入方的時鐘。

## 遙測與確認（決策 8；清單 G1、I1）

`up/telemetry` 的 `TelemetryBatch`：

| 欄位 | 型別 | 說明 |
| --- | --- | --- |
| `v` | 整數 | 1 |
| `stream` | ID | 串流 ID；序號在串流內遞增、不重複使用 |
| `seq` | 整數 ≥ 1 | 批次序號 |
| `clock_synced` | 布林 | 整批的取樣時間點時鐘是否已校正；一批不會混合校正前後的資料（Laysi/trustiot#180） |
| `series` | 陣列，至少 1 項 | 每個（設備、資料點）一項，同一批不重複 |
| `series[].device` | ID | 設備 ID（desired 設定中的 `devices[].id`） |
| `series[].point` | ID | 資料點鍵（型號的 `points[].key`） |
| `series[].samples` | 陣列，至少 1 項 | 每筆是二元陣列 `[at, value]`：毫秒時間戳與值；接收端略過新版附加在後面的元素 |

值（`Value`）以 CBOR 型別區分：布林、整數（i64）、浮點數（有限值，NaN 與無限大算讀取失敗，不送出）、文字（最多 1024 bytes）。型號的資料點設定 scale 或 offset 時，送出的值是浮點數。

規則：

1. 序號在批次形成時編定，從 1 開始，同一串流內連續遞增、永不重複使用；序號要和緩衝一起寫入磁碟。閘道器無法得知下一個序號時（例如重裝），一律換新的串流 ID。
2. 同一串流一次只送一批，收到確認才送下一批；確認逾時就原封不動重送同一批。
3. 雲端以「topic 的閘道器 + 串流 ID」為單位記錄已提交的最高序號。只靠串流 ID 選高水位是錯的：閘道器可以自己取任何串流 ID。
4. 雲端在整批落盤並寫入批次完成紀錄後才回 `committed`；序號不高於高水位的批次直接回 `duplicate`；永遠不可能接受的批次（解碼失敗、驗證失敗）回 `rejected`，雲端可用 `TelemetryBatch::peek_header` 從壞掉的 payload 取出串流與序號來回應。

`down/telemetry-ack` 的 `TelemetryAck`：`v`、`stream`、`seq`、`server_time`（雲端時鐘，閘道器可用來察覺自己的時鐘錯誤）與 `outcome`。`outcome` 是以 `status` 區分的 map：

| `status` | 其他欄位 | 閘道器的處理 |
| --- | --- | --- |
| `committed` | `records`：寫入的筆數 | 從緩衝移除這批，送下一批 |
| `duplicate` | `records`：第一次提交時的筆數 | 從緩衝移除這批，送下一批 |
| `rejected` | `code`（`IssueCode`）、`message`（≤ 1024 bytes） | 移除並記錄錯誤；重送只會永遠卡住串流 |
| `retry` | `after_ms` | 保留這批，等待後重送 |
| 其他 | — | 新版雲端的狀態，解成 `Unknown`：保留這批，照一般逾時重送 |

### 拆包

`BatchBuilder` 一筆一筆接收讀值（`Reading`），在批次放不下下一筆、或時鐘校正狀態改變時關閉目前的批次並回傳，下一筆讀值開新的一批。它精確計算編碼後的大小，所以：

- 每批編碼後不超過上限（預設為 channel 上限 256 KiB，可用 `with_limit` 設 4 KiB–256 KiB 之間的較小值）；
- 一大堆讀值依序推入，就拆成序號連續（從指定的第一個序號起）的多批，不會跳號也不會重複；
- 任何合法讀值單獨放進空批次一定放得下（`MIN_BATCH_LIMIT` 保證），被拒絕的讀值不會改變 builder 的狀態，序號用盡（u64）時回錯誤要求換新串流，不會繞回。

## 設定下發（決策 7；清單 C1、C2、E1、F1、F2、F5）

`down/config` 是 retained 訊息 `DesiredConfig`，內容是這台閘道器要輪詢的全部設定：

| 欄位 | 說明 |
| --- | --- |
| `v`、`rev` | 主版本與這份設定需要的最低修訂版（`minimum_revision`） |
| `version` | 這台閘道器的設定版本，從 1 起遞增 |
| `models[]` | 型號：`name`（小寫英數加單一連字號，1–40 字）、`points[]` |
| `models[].points[]` | 資料點：`key`、`function_code`（1–4，只讀）、`address`（0 起算的協定位址）、`data_type`（`bool`、`u16`、`i16`、`u32`、`i32`、`u64`、`i64`、`f32`、`f64`、`string`）、`byte_order`（`abcd` 預設省略、`badc`、`cdab`、`dcba`）、`length`（只有 `string`，1–125 個暫存器）、`scale`、`offset` |
| `devices[]` | 設備：`id`、`model`、`transport`（`modbus_tcp` 或 `rtu_over_tcp`）、`host`（IP 或 DNS 名稱）、`port`、`unit_id`（RTU 為 1–247）、`poll_interval_ms`（100 ms–1 天）；最多 `MAX_DEVICES`（1000）台 |

`DesiredConfig::validate` 集中了所有結構規則：型號與資料點鍵不重複、位址不重疊（四種功能碼各自的位址空間）、功能碼 1、2 只能是 `bool`、字串需要長度、不能超過位址 65535、scale 與 offset 必須是有限值且 scale 不為 0、同一個 host 與 port 不能混用兩種傳輸方式、設備引用的型號必須存在。資產服務（每次寫入）、設定產生器（發布前）與閘道器（套用前）執行同一套規則；`encode` 拒絕送出無效的設定。

`MAX_DEVICES` 是協定能承載的上限：心跳要帶每台設備的統計且不能拆包，1000 台在最壞情況下仍放得進 1 MiB（`tests/limits.rs`）。每台閘道器實際的設備技術上限（清單 B2）由 Laysi/trustiot-v2#116 訂定，不得超過這個值。

閘道器收到設定時比較 `version` 與目前套用中的版本：

| 收到的版本 | 閘道器的處理 | 回報 |
| --- | --- | --- |
| 較新 | 驗證後原子套用；無效就維持上一版 | `applied`，或 `rejected` 附 issues |
| 相同 | 不重新套用 | 再回報一次 `applied`，遺失的回報因此能自行修復 |
| 較舊 | 不套用（過期的 retained 訊息不能讓閘道器倒退，Laysi/trustiot#139） | `ConfigReport::stale`：`rejected` 附 `stale_version` |
| 解不開 | 維持上一版 | `rejected`，版本取自 `DesiredConfig::peek_header`，issues 取自 `DecodeError::to_issues` |

空 payload 表示 retained 設定被清除（例如除役），由閘道器自行決定處理方式。

`up/config-report` 的 `ConfigReport`：`version`（回報的是哪一版）、`outcome`（`applied`、`rejected`）、`active_version`（回報後實際使用的版本，從未套用過則省略）、`issues[]`（拒絕原因，最多 64 項，每項有 `code`、`path`、`message`；`path` 例如 `devices[2].unit_id`）。雲端收到 `stale_version` 時，以高於 `active_version` 的版本重新發布。

## 在線狀態與心跳（清單 C3、H1）

`up/status` 是 retained 訊息 `Status`，以 `state` 區分：

- `online`：`session`、`agent_version`、`protocol_rev`（agent 實作的最高修訂版；雲端不送更高 `rev` 的設定給它）。
- `offline`：`session`、`reason`（`shutdown` 或 `connection_lost`）。

每次連線，閘道器產生新的 `SessionId`，把 LWT 設成這個 session 的 `offline`／`connection_lost`（`Status::last_will`，retain），連上後第一則訊息發布同一個 session 的 `online`；正常關機前自己發布 `offline`／`shutdown`。舊連線的 LWT 可能比新連線的 `online` 晚到，所以雲端忽略 session 不是最新 `online` 的 `offline`；心跳停止超過間隔容許的時間也視為離線。

`up/heartbeat` 的 `Heartbeat` 每 `interval_s` 秒送一次（QoS 0，遺失一則無妨）：

| 欄位 | 說明 |
| --- | --- |
| `session`、`sent_at`、`interval_s` | 連線、送出時間、下一次心跳前的秒數 |
| `agent_version`、`config_version` | 程式版本與套用中的設定版本 |
| `system` | CPU 使用率、記憶體總量與可用量、資料分割區總量與可用量、SoC 溫度、供電不足與降頻旗標（`throttling.now`／`since_boot`，對應 `vcgencmd get_throttled`）、開機時間、開機秒數、開機次數、agent 執行秒數、本次開機後 agent 啟動次數 |
| `buffer` | 離線緩衝：未確認筆數與批數、最舊一筆的時間、寫到磁碟的 bytes、因緩衝滿或超過保存時間而丟棄的筆數 |
| `clock_synced` | 時鐘目前是否已校正 |
| `cert_not_after` | 用戶端憑證到期時間 |
| `devices[]` | 每台設定中的設備一項：最後成功讀取時間、最近一次輪詢的資料點數與失敗數（0 為正常、部分失敗、全部失敗為離線）、請求數、依類型的失敗數（`connect`、`timeout`、`exception`、`unit_mismatch`、`invalid_value`）、依 Modbus 例外碼的次數（最多 16 種）、最近一次失敗（類型、時間、例外碼、≤ 256 bytes 的說明） |

計數器從 agent 啟動起累計，雲端以兩次心跳的差計算速率與成功率（`1 - (connect + timeout + exception) / requests`），`agent_uptime_s` 變小代表 agent 重啟過。

## 即時值（清單 I3）

`down/live-control` 的 `LiveControl`：`lease`（租約 ID，雲端產生）與 `action`。`action` 以 `op` 區分：

- `start`、`renew`：`devices`（1–64 台，不重複）、`interval_ms`（100 ms–60 秒，等於方案的最短採樣間隔）、`ttl_s`（1–600 秒）。兩者處理方式相同：建立或取代租約，從現在起 `ttl_s` 秒後自動結束；`renew` 也能恢復閘道器重啟時遺失的租約。
- `stop`：結束租約；不存在的租約不做任何事。
- 不認得的 `op` 解成 `Unknown`，閘道器忽略整則訊息。

任何租約涵蓋某台設備期間，閘道器每次取樣立即把值發布到 `up/live`（`LiveSamples`：`series[]`，格式與遙測相同），不進緩衝、不等確認、QoS 0。同一次輪詢超過 64 KiB 時以 `LiveSamples::split` 拆成多則，放不下的序列在下一則接續。即時值不取代遙測：同樣的取樣照常進遙測批次。

## 日誌（決策 22）

- `up/log` 的 `LogBatch`：`entries[]`（warn 以上，舊的在前）與 `dropped`（限流丟掉的筆數）。以 `LogBatch::split` 依上限拆成多則，`dropped` 放在第一則。
- `down/log-request` 的 `LogRequest`：`request`（請求 ID）、`from`、`to`（含兩端，閘道器時鐘）、`min_level`。
- `up/log-dump` 的 `LogDump`：`request`、`part`（從 0 起）、`last`、`coverage`（`complete`、`partial`、`unavailable`：記憶體環狀緩衝是否還涵蓋整個時段）、`available_from`（緩衝中最舊一筆的時間）、`entries[]`。以 `LogDump::split` 拆成編號連續的多則，至少一則，雲端即使沒有符合的紀錄也知道涵蓋範圍。

每筆 `LogEntry`：`at`、`level`（`error`、`warn`、`info`、`debug`、`trace`）、`target`（≤ 128 bytes）、`message`（≤ 4096 bytes）、`fields`（最多 32 個，名稱 ≤ 64 bytes、值 ≤ 1024 bytes）。日誌不得含個資（Laysi/trustiot#110）。

## 大小上限與拆包

| 上限 | Channel | 超過時 |
| --- | --- | --- |
| 1 MiB（`MAX_LARGE_BYTES`） | `config`、`heartbeat` | 不拆包：設備數上限 `MAX_DEVICES` 讓最壞情況的心跳放得下；設定超過時 `encode` 拒絕，資產服務要擋下 |
| 256 KiB（`MAX_BULK_BYTES`） | `telemetry`、`log`、`log-dump`、`config-report` | 遙測以 `BatchBuilder`、日誌以 `split` 拆包；回報最多 64 項 issues，最壞情況放得下 |
| 64 KiB（`MAX_MEDIUM_BYTES`） | `live`、`live-control` | 即時值以 `LiveSamples::split` 拆包；租約最多 64 台設備 |
| 4 KiB（`MAX_CONTROL_BYTES`） | `status`、`telemetry-ack`、`log-request` | 所有欄位都有上限，最壞情況放得下 |

`encode` 對超過上限的 payload 回 `EncodeError::TooLarge`，`decode` 在解析前就拒絕超過上限的 payload；不拆包的訊息在最大合法內容下仍放得進上限（`tests/limits.rs`）。

## 版本與相容性

- **主版本**：`v` 欄位與 topic 的 `v1`。不相容的變更（改名、刪欄位、改型別或語意）開新的主版本與新的 topic 樹；接收端遇到其他 `v` 一律拒絕（`unsupported_version`），在解析其他欄位之前就判斷。
- **向前相容**（舊程式讀新訊息）：略過不認得的 map 鍵與遙測樣本陣列後面多出的元素；可擴充的代碼（`IssueCode`、`ConfigOutcome`、`OfflineReason`、`FailureKind`、`LogCoverage`）保留不認得的值（`Other`），確認狀態與即時操作不認得時解成 `Unknown`。需要接收端理解才能正確處理的新功能，提高 `rev`，舊程式就會拒絕而不是誤讀。
- **向後相容**（新程式讀舊訊息）：新增的欄位必須是選填並有預設值。`tests/golden/v1-frozen/` 是 v1 第一版的固定樣本，永遠不重新產生、不修改、不刪除；之後每個版本都要能把它們解碼成相同的內容（`frozen_v1_payloads_still_decode`）。
- **固定樣本**：`tests/golden/current/` 是目前版本編出的每種訊息的位元組（`.cbor.hex`）與 JSON 呈現，格式任何改變都會讓測試失敗。確定要改格式時以 `UPDATE_GOLDEN=1 cargo test -p protocol --test golden` 重新產生並審查 diff；改了 `current` 卻沒有新增 `rev` 或新主版本，代表改壞了相容性。
- **closed enum**（`Status` 的 `state`、`DataType`、`ByteOrder`、`Transport`、`FunctionCode`、`LogLevel`）不認得的值會讓訊息被拒絕：這些值接收端必須理解才能正確動作。

## 相依套件與 aarch64

閘道器是 Raspberry Pi（`aarch64-unknown-linux-gnu`），本 crate 只能依賴純 Rust、不需要 C 工具鏈的 crate（[專案目錄](../../docs/architecture/repository-layout.md)）。`tests/dependencies.rs` 以 `cargo metadata --filter-platform aarch64-unknown-linux-gnu` 列出編進閘道器的所有 crate，與允許清單（`cbor4ii`、解碼半精度浮點數的 `half` 及其相依的 `cfg-if`、`zerocopy`，`serde`，以及只在建置機器上執行的 derive 巨集相關 crate）不同就失敗，也檢查沒有 crate 連結原生函式庫。新增相依時先確認它是純 Rust、能編到 aarch64，再更新清單。
