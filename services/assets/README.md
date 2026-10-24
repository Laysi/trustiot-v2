# assets（資產服務）

租戶的場域、設備與資料點的擁有者（決策 4；[部署與可搬移性](../../docs/architecture/deployment.md)）。

目前有：

- 自己的 schema `assets`、服務角色與平台角色、RLS（MS0 骨架，Laysi/trustiot-v2#65，2026-10-14）。
- 消費身分服務的 `identity.tenant_created`：每個新租戶在這裡都有一列（見[租戶](#租戶)）。這是第一個跨服務事件，示範其他服務怎麼從事件建立自己的租戶資料。
- 一個需要內部身分 token 的範例 API：`GET /api/assets/v1/tenant` 回呼叫者的租戶。
- 設備型號的 API 與它的事件 `assets.device_model_*`（Laysi/trustiot-v2#99，2026-10-21，見[設備型號](#設備型號)）。
- 閘道器設備池與平台管理員的綁定 API、綁定事件 `assets.gateway_bound`（Laysi/trustiot-v2#101，2026-10-23，見[閘道器設備池與綁定](#閘道器設備池與綁定)）。
- 設備的 API 與它的事件 `assets.device_*`（Laysi/trustiot-v2#100，2026-10-24，見[設備](#設備)）。
- 還沒有場域。

## 閘道器設備池與綁定

`src/gateways/`（rebuild checklist D3 的 API 部分）：出廠登錄過的閘道器都進入設備池，平台管理員再把它綁定到一個租戶。還沒有畫面；租戶管理員自己綁定是 Laysi/trustiot-v2#114，解綁與重新綁定是 Laysi/trustiot-v2#115。

**設備池**（`pool.rs`）：消費端訂閱 `assets.certs-events`（`pool::SUBSCRIPTION`，不改名），讀 `certs` schema 的 outbox。每個 `certs.gateway_registered`（平台層級、沒有租戶的事件）在沒有租戶的服務角色交易裡 `INSERT INTO assets.gateway_pool ... ON CONFLICT DO NOTHING`：硬體 ID 是主鍵，所以重播或同一個事件送兩次都不多一列。certs 的其他事件略過；帶租戶的 `certs.gateway_registered` 是永久錯誤，進 dead letter。表會拒絕的值也是永久錯誤：`workstation` 是空的或超過 255 字時，handler 在寫入前就拒絕；寫入時資料庫回 SQLSTATE class 22（資料例外）或 23514（CHECK）時也一樣，因為每次重試都會失敗。這些事件進 dead letter、看得到，訂閱繼續往下走；否則 outbox 對資料庫錯誤一律重試，一個事件就會卡住之後每一台閘道器（之後的綁定都會 404）。其他資料庫錯誤照 outbox 的規則重試。每處理一個記一行 INFO `applied certs.gateway_registered`，帶 `hardware_id`、`event_id`、`created`。同一台閘道器重複出廠登錄時 certs 回第一次的登錄、不再寫事件，設備池也只有一列。

**綁定**（`binding.rs`）：一次綁定是目標租戶的一個交易，失敗時整個回滾，什麼都不留下，重試從同一個狀態開始（舊系統的綁定失敗後一直卡在 409，Laysi/trustiot#84）：

1. 租戶必須是這個服務知道的（`assets.tenants`，RLS 只給交易的租戶）；不知道就 422 `gateway_binding.tenant_unknown`（沒有這個租戶，或剛建立、`identity.tenant_created` 還沒到）。
2. 這台閘道器在這個租戶已經有目前的綁定：回那個綁定（200），什麼都不寫。所以這個 `PUT` 是冪等的，回應遺失後重送是安全的。
3. 否則插入新的綁定（`ON CONFLICT DO NOTHING`）。閘道器必須在設備池裡（外鍵，Postgres 以表擁有者的權限檢查，服務角色本身讀不到設備池）；不在就 404。別的租戶握有它時（跨租戶的唯一索引 `gateway_bindings_one_current`，會等並行的綁定提交）什麼都沒插入，再以新的 snapshot 讀一次這個租戶的目前綁定：有就是並行的同一個請求先完成了（200），沒有就是別的租戶握有（409 `gateway_binding.bound_to_another_tenant`，不說是哪個租戶）。
4. 同一個交易寫 `assets.gateway_bound`，actor 是平台管理員，然後提交，回 201。

**平台寫進租戶的模式只在一個地方**：`GatewayBinder` 是服務裡唯一替 token 沒有指名的租戶開交易的程式碼（租戶來自 body）。它私下持有服務角色的 `BackgroundTenantDb`（啟動時只交給消費端與它），唯一的方法在開交易前自己再檢查一次呼叫者：平台管理員自己的請求，`act`（別的服務代為交換的 token）一律拒絕（Laysi/trustiot-v2#104）。handler 在讀任何資料之前先做同樣的檢查，所以拒絕不會讀到任何東西。它開的每個租戶交易都記一行 INFO 稽核日誌 `tenant transaction for a platform administrator`，帶平台管理員的 `user_id`、目標 `tenant_id`、`hardware_id` 與 `purpose`，不記 token；這和 `PlatformDb::begin` 記 actor 的那一行相同（背景 handle 本身只在 DEBUG 記錄，而且不帶使用者）。之後的安全審查與 Laysi/trustiot-v2#114 都從這裡看起。

| 請求 | 成功 |
| --- | --- |
| `GET /api/assets/v1/platform/gateways` | 200，`{"gateways": [...], "next_after"}`：依硬體 ID（位元組順序）排序，每筆有 `hardware_id`、`registered_at`、`workstation`、`pooled_at`、`binding`（目前的綁定，沒有就 `null`）。參數：`state` 是 `unbound`（待綁定的）或 `bound`，沒有就全部；`after` 是上一頁的 `next_after`；`limit` 1–500，預設 100 |
| `GET /api/assets/v1/platform/gateways/{hardware_id}` | 200，同上的一筆 |
| `PUT /api/assets/v1/platform/gateways/{hardware_id}/binding`，body `{"tenant_id"}` | 201 新的綁定 `{"binding_id", "hardware_id", "tenant_id", "bound_at"}`，寫 `assets.gateway_bound`；已經綁定在同一個租戶時 200 回原本的綁定，不寫任何東西 |

錯誤（M1，依序檢查）：沒有有效的內部身分 token 401；不是平台管理員、或 token 帶 `act` 403（在讀任何資料之前）；路徑的硬體 ID 不合規則 404；列表參數未知或格式不對 400 `invalid_request`；綁定的 `Content-Type` 不是 `application/json` 400，body 超過 4 KiB 400 `gateway_binding.body_too_large`，不是 JSON 物件 400 `gateway_binding.body_invalid`；未知欄位 422 `gateway_binding.unknown_field`，`tenant_id` 缺漏或 `null` 422 `gateway_binding.field_required`，不是 UUID 422 `gateway_binding.tenant_id_invalid`；租戶不認得 422；不在設備池 404；別的租戶握有 409；資料庫出錯 503（什麼都沒寫，可以重試）。每個回應都帶 `cache-control: no-store`。每次綁定記一行 INFO `bound a gateway`，帶 `user_id`、`tenant_id`、`hardware_id`、`binding_id`、`created`；拒絕記 INFO，失敗記 ERROR，只帶錯誤種類；不記 token。

**事件**：`assets.gateway_bound`（`contract::events::assets::GatewayBound`），payload 是 `binding_id` 與 `hardware_id`，租戶在信封上，actor 是做綁定的平台管理員（使用者 ID），沒有任何密鑰。aggregate 是 `gateway_binding`、ID 是綁定 ID：aggregate 屬於它第一個事件的租戶，閘道器之後可能換租戶，所以每次綁定一個新的 aggregate。certs（換發與 broker 權限，Laysi/trustiot-v2#632）、ingest（Laysi/trustiot-v2#640）與閘道器設定（gateway-ops，Laysi/trustiot-v2#93）之後各自消費它，維護自己的「硬體 ID → 租戶」對照表；閘道器的租戶一律是它目前的綁定（[架構決策](../../docs/architecture/decisions.md)的決策 12，2026-10-24）。

**為 Laysi/trustiot-v2#115 留的設計**：綁定從不刪除也不改寫。解綁將設定 `unbound_at`（欄位已在，現在一律是 `NULL`），重新綁定則在目標租戶新增一列、新的 `binding_id`；`gateway_bindings_one_current` 只限 `unbound_at IS NULL` 的列，所以歷史不必遷移。


## 設備型號

`src/device_models/`（rebuild checklist C1、F5）：租戶的暫存器對應表，同型的設備共用一個型號，閘道器依它輪詢。資料點的格式就是閘道器設定用的 `protocol::config::DataPoint`（M4，只定義一次），規則也是 `crates/protocol` 的那一套（`config::validate_model_points`），所以存下的型號一定通過閘道器設定的檢查。

| 請求 | 角色 | 成功 |
| --- | --- | --- |
| `GET /api/assets/v1/device-models` | viewer 以上 | 200，`{"models": [...]}`：依名稱排序，每筆有 `model_id`、`name`、`revision`、`point_count`、`created_at`、`updated_at` |
| `POST /api/assets/v1/device-models` | editor 以上 | 201，整個型號，`Location` 指向它；寫 `assets.device_model_created` |
| `POST /api/assets/v1/device-models/import` | editor 以上 | 同上；body 是匯出的文件 |
| `GET /api/assets/v1/device-models/{model_id}` | viewer 以上 | 200，整個型號（`model_id`、`name`、`revision`、`created_at`、`updated_at`、`points`） |
| `GET /api/assets/v1/device-models/{model_id}/export` | viewer 以上 | 200，`{"format": "trustiot.device_model", "format_version": 1, "name", "points"}`，`Content-Disposition: attachment; filename="<名稱>.device-model.json"`；不含 ID、版本與時間，可以匯入到別的租戶或部署 |
| `PUT /api/assets/v1/device-models/{model_id}` | editor 以上 | 200，更新後的型號，`revision` 加一；寫 `assets.device_model_updated`。body 是 `{"revision", "points", "name"?}`：`revision` 必填，是呼叫者讀到的版本，必須仍是目前的版本，否則 409 `revision_mismatch`（`details.current`），兩個 editor 同時編輯時不會無聲地蓋掉對方的資料點；`points` 整批取代舊的資料點；`name` 有送就必須是原名（名稱建立後不能改） |
| `DELETE /api/assets/v1/device-models/{model_id}` | editor 以上 | 204，型號與資料點一起刪除；寫 `assets.device_model_deleted` |

**規則**：三個寫入入口（建立、更新、匯入）都先以 `document.rs` 逐欄讀 JSON，再跑 `crates/protocol` 的跨欄與跨資料點規則，全部通過才寫入；不通過就 422 `validation_failed`，每個問題一個 field error（JSON Pointer 與 `device_model.*` message key），最多列 100 個，其餘以 `details.omitted_field_errors` 計數，什麼都不寫、也不寫事件。

| 欄位 | 規則 | message key |
| --- | --- | --- |
| 未知欄位（含 `tenant_id`） | 一律拒絕；欄位名稱是 64 字以內的可見 ASCII 時才出現在 pointer 裡 | `unknown_field` |
| 必填欄位缺漏或 `null` | `name`、`points`；更新的 `revision`；資料點的 `key`、`function_code`、`address`、`data_type`；匯入的 `format`、`format_version` | `field_required` |
| `name` | 小寫英數字以單一連字號連接、字母開頭、1–40 字（`protocol::ModelName`）；租戶內唯一（重複 409 `name_taken`）；建立後不能改（422 `name_immutable`） | `name_invalid`（`details.max` 40） |
| `points` | 陣列，最多 1000 個，可以是空的 | `points_invalid`、`too_many_points` |
| 更新的 `revision` | 正整數；不是目前的版本時 409 `revision_mismatch` | `revision_invalid` |
| `key` | 1–64 個 ASCII 英數字、`.`、`_`、`-`，英數字開頭（`protocol::PointKey`）；型號內唯一 | `point_key_invalid`、`point_key_duplicate` |
| `function_code` | 只有讀取功能碼 1–4 | `function_code_invalid` |
| `address` | 0–65535 的整數；資料點的暫存器不能超過 65535，同一功能碼的位址不能重疊（四種功能碼各自一個位址空間） | `address_invalid`、`address_out_of_range`、`points_overlap` |
| `data_type` | `bool`、`u16`、`i16`、`u32`、`i32`、`u64`、`i64`、`f32`、`f64`、`string`；功能碼 1、2 只能是 `bool` | `data_type_invalid`、`function_type_mismatch` |
| `byte_order` | `abcd`（預設）、`badc`、`cdab`、`dcba`；單一位元沒有位元組順序，字串只能 `abcd` 或 `badc` | `byte_order_invalid`、`byte_order_unexpected` |
| `length` | 只有 `string` 有、而且必填：1–125 個暫存器 | `length_invalid`、`length_required`、`length_unexpected` |
| `scale`、`offset` | 有限的數字，`scale` 不為 0；只有數值型別能縮放（F5：值為 `raw * scale + offset`） | `scaling_invalid` |
| `description` | 給人與 AI 讀的說明：去掉前後空白後 1–512 字（空字串視為沒有），不能有換行與 tab 以外的控制字元、格式字元（零寬字元、雙向覆寫）或行與段落分隔字元。說明是租戶自由填寫的文字：AI 功能讀它時必須當成不可信任的資料，不能當成指令 | `description_invalid`（`details.max` 512） |
| 匯入的 `format`、`format_version` | 必須是 `trustiot.device_model` 與 `1` | `format_unsupported`、`format_version_unsupported` |

其他錯誤（M1）：沒有有效的內部身分 token 401；沒有租戶或角色不夠 403；`model_id` 不是 UUID 或不是呼叫者租戶的型號 404；寫入的 `Content-Type` 不是 `application/json` 400（跨來源的頁面無法不經 CORS preflight 送出這個型別，所以不能借使用者的 cookie 改型號）；body 超過 2 MiB 400 `body_too_large`；body 不是 JSON 物件 400 `body_invalid`；刪除仍有設備使用的型號 409 `in_use`；租戶的 `identity.tenant_created` 還沒處理 503 `tenant_not_ready`；資料庫出錯 503。每個回應都帶 `cache-control: no-store`。每次成功的寫入記一行 INFO（`created a device model`、`updated a device model`、`deleted a device model`），只帶 `user_id`、`tenant_id`、`model_id`、`revision`，不記名稱、鍵與說明。

**仍被使用的型號不能刪**：`device_model_points` 以外，參照型號的表（設備，Laysi/trustiot-v2#100）必須以 `FOREIGN KEY (tenant_id, model_id) REFERENCES assets.device_models (tenant_id, model_id) ON DELETE RESTRICT` 參照，刪除時資料庫回 SQLSTATE 23001（`restrict_violation`；`NO ACTION` 的外鍵回 23503，兩者都對應），API 回 409 `in_use`。`assets.devices` 就這樣參照型號；`tests/device_models.rs` 以一張同樣參照的替身表驗證這條路（不必準備閘道器與綁定），`tests/devices.rs` 以真的設備再驗一次。

**事件**（`contract::events::assets`，aggregate `device_model`、ID 是型號 ID，在變更的同一個租戶交易裡寫入 `assets.outbox_event`）：

| 事件 | payload |
| --- | --- |
| `assets.device_model_created` | `model_id`、`revision`（1）、`model`：`protocol::config::DeviceModel`（名稱與資料點，閘道器設定的格式，不含說明） |
| `assets.device_model_updated` | 同上，`revision` 是更新後的版本 |
| `assets.device_model_deleted` | `model_id`、`name`、`revision`（最後的版本） |

閘道器設定（Laysi/trustiot-v2#93）只靠事件就能維護自己的型號副本。它登記為消費端之前，`tools/migrate` 的 registry 讓 `assets` 暫時當自己的讀者（producer 至少要有一個讀者），沒有任何訂閱讀這些事件。

## 設備

`src/devices/`（rebuild checklist C2）：閘道器輪詢的 Modbus 端點。每台設備屬於一台閘道器（綁定到租戶的那台）與租戶的一個型號；設定的格式就是閘道器設定用的 `protocol::config::Device`（M4），規則也跑 `crates/protocol` 那一套，所以存下的設備一定通過閘道器設定的檢查。方案的取樣間隔下限與每台閘道器的設備數上限是 Laysi/trustiot-v2#116，設備畫面是 Laysi/trustiot-v2#126。

| 請求 | 角色 | 成功 |
| --- | --- | --- |
| `GET /api/assets/v1/devices` | viewer 以上 | 200，`{"devices": [...]}`：依名稱排序 |
| `POST /api/assets/v1/devices` | editor 以上 | 201，整台設備，`Location` 指向它；寫 `assets.device_created`。body 是 `{"name", "hardware_id", "model_id", "transport", "host", "port", "unit_id"?, "poll_interval_ms"?}` |
| `GET /api/assets/v1/devices/{device_id}` | viewer 以上 | 200，整台設備：`device_id`、`name`、`hardware_id`、`model_id`、`transport`、`host`、`port`、`unit_id`、`poll_interval_ms`、`revision`、`created_at`、`updated_at` |
| `PUT /api/assets/v1/devices/{device_id}` | editor 以上 | 200，更新後的設備，`revision` 加一；寫 `assets.device_updated`。body 是 `{"revision", "transport", "host", "port", "unit_id"?, "poll_interval_ms"?, "name"?, "hardware_id"?, "model_id"?}`：整批取代連線設定，沒送的 `unit_id`、`poll_interval_ms` 回到預設值；`revision` 必須仍是目前的版本，否則 409 `revision_mismatch`（`details.current`）；`name`、`hardware_id`、`model_id` 有送就必須是原值 |
| `DELETE /api/assets/v1/devices/{device_id}` | editor 以上 | 204；寫 `assets.device_deleted` |

**規則**：兩個寫入入口先以 `document.rs` 逐欄讀 JSON，全部通過後再把這台設備放進一份只有它的閘道器設定、跑 `crates/protocol` 的設定檢查（`DesiredConfig::validate`，閘道器套用設定前跑的同一個函式）；之後 `store.rs` 才在租戶交易裡查閘道器、型號與同一台閘道器的其他設備。任何一步不通過就什麼都不寫、也不寫事件。欄位錯誤是 422 `validation_failed`，每個問題一個 field error（JSON Pointer 與 `device.*` message key），最多 20 個。

| 欄位 | 規則 | message key |
| --- | --- | --- |
| 未知欄位（含 `tenant_id`、`mode`） | 一律拒絕 | `unknown_field` |
| 必填欄位缺漏或 `null` | 建立的 `name`、`hardware_id`、`model_id`；兩個入口的 `transport`、`host`、`port`；更新的 `revision` | `field_required` |
| `name` | 型號的命名規則：小寫英數字以單一連字號連接、字母開頭、1–40 字（`protocol::ModelName`）；租戶內唯一（重複 409 `name_taken`）；建立後不能改（422 `name_immutable`） | `name_invalid`（`details.max` 40） |
| `hardware_id` | 硬體 ID 規則（`protocol::GatewayId`）；必須是目前綁定到呼叫者租戶的閘道器（`unbound_at IS NULL`），否則 422 `gateway_not_bound`；建立後不能換（422 `gateway_immutable`） | `hardware_id_invalid`（`details.max` 64） |
| `model_id` | UUID；必須是呼叫者租戶的型號，否則 422 `model_not_found`；建立後不能換（422 `model_immutable`） | `model_id_invalid` |
| `transport` | `modbus_tcp` 或 `rtu_over_tcp` | `transport_invalid`（`details.allowed`） |
| `host` | IP 位址或最多 253 位元組的 DNS 名稱（`protocol::config::Host`：不能是未指定、多播、廣播位址，最後一段是數字（全是十進位數字，或 `0x` 加十六進位數字，WHATWG URL 的「ends in a number」規則）的名稱不是名稱，因為解析器會把 `0x7f000001` 這類寫法讀成 IPv4 位址）；另外不能是 loopback 位址（含 `::ffff:127.0.0.1`）、`localhost`、`*.localhost`、`ip6-localhost` 或 `ip6-loopback`（不分大小寫，結尾可帶 `.`）。私有位址（10/8、172.16/12、192.168/16、IPv6 ULA）與 169.254/16 都接受：現場設備就在這些網段 | `host_invalid`（`details.max` 253） |
| `port` | 1–65535 的整數，必填、沒有預設 | `port_invalid`（`details.min`、`details.max`） |
| `unit_id` | 整數，預設 1；Modbus TCP 0–255，RTU over TCP 1–247 | `unit_id_invalid`（`details.min`、`details.max`） |
| `poll_interval_ms` | 整數毫秒，預設 60000；1000–86400000 | `poll_interval_invalid`（`details.min`、`details.max`） |
| 更新的 `revision` | 正整數 | `revision_invalid` |

**同一個端點的設備**：同一台閘道器上 `host` 相同（以 `Host::canonical` 比對，`::ffff:192.0.2.10` 與 `192.0.2.10` 相同、DNS 名稱不分大小寫）且 `port` 相同的設備，在同一個端點後面，例如同一台 RS485 轉換器：

- 都必須用同一種傳輸方式，否則 409 `transport_mismatch`（`details.device_id` 與 `details.transport` 是那個端點上已有的設備）。閘道器設定也拒絕混用（`protocol` 的 `MixedTransport`），在 API 擋下才不會讓整份設定被閘道器拒絕。
- 站號不能重複，否則 409 `unit_taken`（`details.device_id` 是用掉那個站號的設備）：兩台設備會回應同一個請求。

兩條規則都在每個（租戶、閘道器）的 advisory lock 下檢查（`pg_advisory_xact_lock`，key 是 `assets.devices/<租戶>/<硬體 ID>`），所以同時建立的兩台不會一起通過；站號另有唯一約束 `devices_unit_unique` 兜底。更新先拿閘道器的鎖、再鎖設備的列，順序固定，不會互相等死。

**選定的規則與理由**（rebuild checklist C2 要求統一的預設取樣間隔與連線設定的檢查，沒有規定數值；以下是保守的選擇）：

| 規則 | 理由 |
| --- | --- |
| 預設取樣間隔 60 秒 | 每個方案都允許的最短間隔（[方案](../../docs/product/plans.md)：Starter 1 分鐘、Business 10 秒、Enterprise 1 秒），用預設值建立的設備在 Laysi/trustiot-v2#116 檢查方案下限時不必改。舊系統沒有統一的預設值 |
| 取樣間隔下限 1 秒 | 沒有方案賣比 1 秒更短的間隔；`protocol` 接受 100 毫秒，但存下一個之後的方案檢查必須推翻的值沒有意義。方案自己的下限在 #116 另外檢查 |
| 取樣間隔上限 1 天 | `protocol` 的上限 |
| 預設站號 1 | 舊系統的預設、RTU 的第一個位址 |
| 連接埠沒有預設 | 現場的連接埠各不相同（502、8899、4196 都見過），預設值只會讓錯的設定看起來是對的 |
| 拒絕 loopback 與 `localhost` | 指向閘道器自己的連接埠，那裡沒有現場設備，當成打錯拒絕；Debian 的 `/etc/hosts` 把 `ip6-localhost`、`ip6-loopback` 指向 `::1`，所以一起拒絕。這只是輸入檢查，不是安全邊界：雲端無法照閘道器在現場網路上的方式解析名稱，其他任何 DNS 名稱在那裡都可能解析成 loopback（或在檢查之後才改成），所以不讓輪詢連到閘道器本機的服務，要由閘道器在解析與連線時把關（`edge/agent` 的 `modbus::tcp::connect`），這個檢查不能取代它（後續工作） |
| 收緊主機規則要一起處理既有的列 | 讀取設備時會用 `protocol::config::Host::new` 重新檢查存下的 `host`，不通過就當成資料損壞（503），列表（整個租戶的）、讀取、更新那台設備都會失敗（刪除不讀主機，仍然可以）。所以之後收緊 `Host` 的規則（例如這次的「結尾是數字」），要在同一個變更裡先查出現有不符合新規則的列並處理（遷移或通知租戶改掉），不能只改程式。本服務自己的 loopback 名稱檢查（`localhost`、`ip6-*`）讀取時不重查，收緊它不影響既有的列，但仍要決定既有的列怎麼辦 |
| 只有 `modbus_tcp`、`rtu_over_tcp` | `protocol` 只有這兩種；模擬模式已棄用（F6），被動模式（F3）還沒做 |
| 設備名稱在租戶內唯一 | 與型號一樣，名稱是使用者辨認設備的方式 |
| 不分辨閘道器是別的租戶的、沒綁定、還是不在設備池 | 都回同一個 `gateway_not_bound`，租戶無法藉此探知別的租戶握有哪些閘道器 |
| 綁定結束後只能刪除 | 設備的外鍵指向建立時的綁定（綁定不會被刪除）。更新在拿到閘道器的 advisory lock、鎖住設備的列之後，檢查那個綁定還是閘道器目前的綁定（`unbound_at IS NULL`）；不是就回 409 `device.binding_ended`，什麼都不寫、不發事件，因為閘道器可能已經綁到別的租戶，更新事件不能把設備送回去。讀取與刪除照常可以做。解綁或重新綁定時已有的設備怎麼處理是 Laysi/trustiot-v2#115 的範圍 |

建立設備時，綁定是在拿到閘道器的 advisory lock 之後、同一個交易裡讀的。服務角色對 `gateway_bindings` 沒有 `UPDATE`，不能以 `FOR SHARE` 鎖住綁定的列，所以解綁（Laysi/trustiot-v2#115）設定 `unbound_at` 前要在同一個租戶交易裡拿同一把鎖（`assets.devices/<租戶>/<硬體 ID>`），解綁與建立、更新設備才不會交錯，解綁之後也不會再有設備掛上或更新那個綁定。

其他錯誤（M1）：沒有有效的內部身分 token 401；沒有租戶或角色不夠 403；`device_id` 不是 UUID 或不是呼叫者租戶的設備 404；更新綁定已結束的設備 409 `device.binding_ended`；寫入的 `Content-Type` 不是 `application/json` 400；body 超過 16 KiB 400 `body_too_large`；body 不是 JSON 物件 400 `body_invalid`；資料庫出錯 503（什麼都沒寫）。每個回應都帶 `cache-control: no-store`。每次成功的寫入記一行 INFO（`created a device`、`updated a device`、`deleted a device`），只帶 `user_id`、`tenant_id`、`device_id`、`revision`，不記名稱、主機與連接埠。

**事件**（`contract::events::assets`，aggregate `device`、ID 是設備 ID，在變更的同一個租戶交易裡寫入 `assets.outbox_event`）：

| 事件 | payload |
| --- | --- |
| `assets.device_created` | `device_id`、`revision`（1）、`name`、`hardware_id`、`model_id`、`device`：`protocol::config::Device`（`id` 是設備 ID，`model` 是型號名稱，加上連線設定與取樣間隔，閘道器設定的格式） |
| `assets.device_updated` | 同上，`revision` 是更新後的版本 |
| `assets.device_deleted` | `device_id`、`name`、`hardware_id`、`model_id`、`revision`（最後的版本）：閘道器設定與 field-context 從這些就能移除設備的對應；租戶只在 envelope 的 `tenant_id` |

## 租戶

`src/tenants.rs`：

- **消費端**：訂閱名稱 `assets.identity-events`（`SUBSCRIPTION`，改名等於從頭重讀事件，所以不改），讀 `identity` schema 的 outbox（[crates/outbox](../../crates/outbox/README.md)）。收到 `identity.tenant_created` 時，在事件的租戶交易裡 `INSERT INTO assets.tenants (tenant_id) ... ON CONFLICT (tenant_id) DO NOTHING`：RLS 只放行那個租戶的列，`tenant_id` 是整個主鍵，所以重播（`outbox::Replay`）或同一個事件送兩次都不會多一列。身分服務的其他事件略過。每處理一個 `identity.tenant_created` 記一行 INFO `applied identity.tenant_created`，帶 `tenant_id`、`event_id` 與 `created`（這次是否新增了列）；不記租戶名稱（事件本身也不帶名稱）。沒有租戶的 `identity.tenant_created` 是永久錯誤，進 dead letter。
- **API**：`GET /api/assets/v1/tenant`（`TENANT_PATH`，入口不去除前綴）。

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 使用者沒有選租戶、或不是成員（token 沒有 `tenant`） | 403 `permission_denied` |
| 這個服務還沒處理到該租戶的 `identity.tenant_created` | 404 `not_found` |
| 資料庫出錯 | 503 `unavailable`（ERROR 日誌只記 `user_id` 與錯誤種類，不記資料） |
| 其他 | 200，`{"tenant_id", "created_at"}`（`created_at` 是這個服務知道這個租戶的時間），`cache-control: no-store` |

讀取在 token 租戶的 RLS 交易裡進行（`TenantDb::begin`），讀出的列不是那個租戶就當作錯誤，不回傳。

## 資料庫

schema `assets`，DDL 在 `migrations/postgres/`，由 migration Job（登記在 `tools/migrate/src/registry.rs` 的 `PRODUCT`：有平台角色、消費 `identity` 與 `certs` 的事件、發出自己的事件）以 migration 角色依檔名順序套用；執行期程式碼不建表、不改表。檔案清單在 `src/schema.rs`，Job 與服務的 readiness 編譯同一份：資料庫缺少這一版預期的檔案（或套用了別的版本）時 `/readyz` 回 503，日誌寫出檔案（Laysi/trustiot-v2#402，[service-kit 的 migration 檢查](../../crates/service-kit/README.md#migration-檢查migrations)）。

| 角色 | 權限 |
| --- | --- |
| `assets_owner`（migration 角色） | 擁有 schema 與所有表 |
| `assets_app`（服務角色） | `assets.tenants` 的 `SELECT`、`INSERT`；`device_models` 的 `SELECT`、`INSERT`、`UPDATE`、`DELETE`；`device_model_points` 的 `SELECT`、`INSERT`、`DELETE`；`gateway_pool` 只有 `INSERT`（讀不到設備池，綁定靠外鍵找到閘道器）；`gateway_bindings` 的 `SELECT`、`INSERT`；`devices` 的 `SELECT`、`INSERT`、`DELETE`，`UPDATE` 只限更新可以改的欄（`transport`、`host`、`host_key`、`port`、`unit_id`、`poll_interval_ms`、`revision`、`updated_at`，`0005_device_update_columns.sql`；名稱、閘道器、綁定、型號改不了，這也是 `SELECT ... FOR UPDATE` 需要的權限）；`outbox_event` 的 `INSERT`、`outbox_aggregate` 的 `SELECT`、`INSERT`、`UPDATE`（Job 照 outbox 範本授權，只限自己租戶的列）；消費端自己的表（Job 授權） |
| `assets_platform`（平台角色） | 讀 `identity.outbox_event` 與 `certs.outbox_event`（Job 授權）；照 outbox 範本能寫平台層級（沒有租戶）的事件。在閘道器設定（Laysi/trustiot-v2#93）登記為消費端之前，`assets` 暫時是自己事件的讀者，所以它也讀得到 `assets.outbox_event`（每個租戶的型號與綁定事件），沒有任何訂閱讀它。為平台管理員的設備池列表讀 `gateway_pool` 與每個租戶的 `gateway_bindings`（`platform_access` 只限 `SELECT`）；讀不到 `assets` 的其他租戶資料 |

| 表 | 內容 | RLS |
| --- | --- | --- |
| `tenants` | `tenant_id`（主鍵）、`created_at` | 照 `crates/service-kit/sql/tenant-table.sql` 的 `tenant_isolation` |
| `device_models` | `tenant_id`、`model_id`（兩者是主鍵）、`name`（租戶內唯一，`CHECK` 也守命名規則）、`revision`、`created_at`、`updated_at`；`tenant_id` 參照 `tenants` | `tenant_isolation` |
| `device_model_points` | 每個資料點一列：`position`（順序）、`key`（型號內唯一）、`function_code`、`address`、`data_type`、`byte_order`、`length`、`scale`、`value_offset`、`description`，各欄有 `CHECK` 守單欄的範圍；跨欄與跨資料點的規則（重疊、型別與功能碼）只在 API 檢查；以 `(tenant_id, model_id)` 參照 `device_models`，`ON DELETE CASCADE` | `tenant_isolation` |
| `gateway_pool` | 設備池：出廠登錄過的每台閘道器一列，`hardware_id`（主鍵，`CHECK` 守硬體 ID 規則）、`registered_at`（事件的 `occurred_at`）、`workstation`、`pooled_at`；不是租戶資料，沒有 `tenant_id` | 無：閘道器出廠時不屬於任何租戶，屬於哪個租戶記在 `gateway_bindings` |
| `gateway_bindings` | 綁定：`tenant_id`、`binding_id`（兩者是主鍵）、`hardware_id`（參照 `gateway_pool`）、`bound_at`、`unbound_at`（留給 Laysi/trustiot-v2#115）；`gateway_bindings_one_current` 是跨租戶的唯一索引（一台閘道器最多一個 `unbound_at IS NULL` 的綁定），刻意不以 `tenant_id` 開頭：它守的就是跨租戶的規則，別的租戶只會知道「已被綁定」，看不到是誰 | `tenant_isolation`；`assets_platform` 另有唯讀的 `platform_access` |
| `devices` | 設備：`tenant_id`、`device_id`（兩者是主鍵）、`name`（租戶內唯一，`CHECK` 守命名規則）、`binding_id` 與 `hardware_id`（以 `(tenant_id, binding_id, hardware_id)` 參照 `gateway_bindings`，所以只能指向自己租戶的綁定）、`model_id`（參照 `device_models`，`ON DELETE RESTRICT`）、`transport`、`host`、`host_key`（`Host::canonical`）、`port`、`unit_id`、`poll_interval_ms`、`revision`、`created_at`、`updated_at`，各欄有 `CHECK` 守單欄的範圍；`devices_unit_unique` 守同一個端點的站號不重複。`assets_platform` 沒有權限 | `tenant_isolation` |
| `outbox_event`、`outbox_aggregate` | 這個服務發出的事件與每個 aggregate 的序號（由 Job 照 [outbox 範本](../../crates/service-kit/sql/outbox-event.sql) 建立） | 範本的政策：服務角色只寫自己租戶的事件 |
| `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version` | 消費端的記錄（由 Job 建立，見 [crates/outbox](../../crates/outbox/README.md)） | 無：不是租戶資料 |
| `trustiot_migrate_applied` | migration Job 在這個 schema 套用過的檔案（component、檔名、SHA-256），由 Job 寫入；`assets_app` 只能 `SELECT`，給 readiness 的 migration 檢查用，`assets_platform` 讀不到 | 無：不是租戶資料 |

兩個連線池：服務角色的 `TenantDb`（API 與消費端每個事件的交易，`BackgroundTenantDb` 只交給兩個消費端與 `GatewayBinder`），以及平台角色的 `PlatformDb`（兩個消費端以唯讀交易讀事件、readiness、平台管理員讀設備池，4 條連線，`PLATFORM_CONNECTIONS`）。readiness 要兩個角色都開得出通過 service-kit session 檢查的連線，而且資料庫有這一版預期的每個 migration 檔案。

## 設定

設定由 `service_kit::config` 載入（kit README 的「設定」）。

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 平台設定：驗證內部身分 token 的公鑰檔（chart 掛進每個 pod 的 ConfigMap `<release>-internal-identity-keys` 的 `jwks.json`）；每 5 秒重讀，換公鑰不必重啟，還沒有可用的公鑰時不就緒（service-kit README「公鑰與輪替」） |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 平台設定：Postgres 位置；只接受 `verify-full`，`_CA_FILE` 必填 |
| `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | 服務角色 `assets_app` 的帳密：密碼來自 Secret `<release>-assets-password` 的 `assets_app` key |
| `TRUSTIOT__DATABASE__PLATFORM_USERNAME`、`TRUSTIOT__DATABASE__PLATFORM_PASSWORD` | 平台角色 `assets_platform` 的帳密：密碼來自同一個 Secret 的 `assets_platform` key。chart 只對 outbox 消費端綁平台角色的密碼（`_migrate.tpl` 的 `trustiot.migrate.platformLogins`），migration Job 產生這兩個密碼並設定角色 |
| `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 服務角色連線池每個副本的上限，2–100，預設 10；平台角色另有 4 條；chart 以 `services.assets.database.maxConnections` 設定。連線池的大小或數量改變時，chart 的連線預算（`deploy/chart/templates/_postgres.tpl`，見 chart README 的「連線預算」）要一起改，`tests/chart_connections.rs` 比對兩者 |

健康埠（預設 8081）只有 `/healthz` 與 `/readyz`，API 在 8080。設定不能用時以 exit code 1 結束。

## 映像

`deploy/images/assets/image.toml`：與身分服務相同，交叉編譯成 `aarch64-unknown-linux-musl` 的靜態執行檔，以 `deploy/images/rust-binary.Containerfile` 包成映像（[deploy/images/README.md](../../deploy/images/README.md)）。chart 的 `services.assets.enabled` 預設 `false`；nightly 的 smoke 以從原始碼建出的映像啟用它與身分服務，經入口建立租戶並讀它（[CI 與發佈](../../docs/architecture/ci-release.md) 的「nightly.yml」）。

## 測試

在 repo 根目錄執行：

```sh
CARGO_BUILD_JOBS=2 cargo test -p assets
```

整合測試都需要 podman 或 docker：以 `testkit::product` 起一個帶 TLS 的暫時 Postgres，由 migration Job 的程式套用整個產品的 schema，再以 `assets_app`、`assets_platform` 啟動真的 binary。沒有容器時略過，設了 `CI` 時改為失敗。

| 測試 | 驗證 |
| --- | --- |
| `tests/tenant_created.rs` | 以身分服務自己的 `TenantStore` 建立租戶：資產服務最後有那一列，API 只回 token 租戶的資料；重播整個事件記錄、同一個事件再送一次，都不多一列、`created_at` 不變，日誌依序記 `created` 為 true、false、false，沒有 dead letter；沒有 token、錯的受眾 401，沒有租戶 403，還沒處理的租戶 404 |
| `tests/background_identity.rs` | Laysi/trustiot-v2#106：背景工作不需要使用者的身分。公鑰集合是空的、沒有任何 token 驗得過時，帶著真的使用者 token（受眾 `assets`、新租戶的 viewer）的請求回 503 `unavailable`，readiness 也是 503；消費端照樣以服務自己的身分（`ServiceIdentity`、自己的資料庫角色）處理 `identity.tenant_created`，新租戶在 `assets.tenants` 有一列（日誌 `created` 為 true） |
| `tests/cross_tenant.rs` | [crates/testkit](../../crates/testkit/README.md) 的跨租戶負向測試：服務角色讀不到也改不了其他租戶的 `tenants`、`device_models`、`device_model_points`、`gateway_bindings`、`devices`，寫不了其他租戶的事件與 aggregate 序號；API（租戶、型號的列表、讀取、匯出、更新、刪除、建立、匯入，設備的列表、讀取、更新、刪除）不把一個租戶的資料給另一個租戶，也不寫進另一個租戶；建立設備的 body 必須指向呼叫者租戶已綁定的閘道器與型號，suite 的 body 範本做不到，所以豁免，由 `tests/devices.rs` 測別的租戶的閘道器與型號都被拒絕、兩個租戶都沒有寫入；消費端自己的表、設備池 `gateway_pool` 與 migration 紀錄不是租戶資料而豁免；設備池的三個平台路由只給平台管理員，租戶使用者一律 403，由 `tests/gateway_pool.rs` 測，在這裡豁免 |
| `tests/gateway_pool.rs` | Laysi/trustiot-v2#101：以 `certs_platform` 照出廠登錄的方式寫 `certs.gateway_registered`，閘道器出現在設備池（未綁定），同一個硬體 ID 再來一次仍只有一列、內容不變、沒有 dead letter；`workstation` 超過 255 字的事件，與表的 CHECK 拒絕的事件（測試暫時加一個 CHECK）都進 dead letter，之後的合法閘道器照樣進入設備池；綁定回 201、寫出綁定與一個 `assets.gateway_bound`（目標租戶、actor 是平台管理員、payload 只有 `binding_id` 與 `hardware_id`），同一個綁定再送回 200、什麼都不寫，別的租戶 409、什麼都不寫；以 trigger 讓事件寫入失敗時回 503、綁定與事件都沒留下，重試 201；兩個租戶同時綁定一台時一個 201 一個 409，同一個租戶同時兩次時一個 201 一個 200，都只有一個綁定與一個事件；沒有 token、錯的受眾 401，租戶管理員與帶 `act` 的平台管理員 403，而且他們沒有任何平台交易或租戶交易的 INFO 日誌；對照組是平台管理員本人：讀取有 `platform database access`，綁定有一行 `tenant transaction for a platform administrator`（帶 `user_id`、`tenant_id`、`hardware_id`）；路徑、列表參數、body 的每種錯誤；日誌沒有 token |
| `tests/device_models.rs` | Laysi/trustiot-v2#99：建立、匯入、讀取、匯出、更新、刪除各寫出對的列與事件（`revision` 依序、事件內容就是閘道器格式的型號），匯出的文件匯入後相同；每一條命名與資料點規則經建立、更新、匯入三個入口都回 422 與對的 field error，什麼都不寫；重複的名稱 409、改名 422、過期的 `revision` 409；被參照的型號刪除 409 `in_use`；viewer 只能讀、沒有租戶 403、沒有 token 401；還沒處理到 `identity.tenant_created` 的租戶 503 |
| `tests/devices.rs` | Laysi/trustiot-v2#100：建立（沒送站號與取樣間隔時是預設的 1 與 60000）、讀取、列表、更新、刪除各回對的內容，並依序寫出 `assets.device_created`、`device_updated`、`device_deleted`（aggregate `device`、序號依序、payload 的 `device` 通過閘道器設定的檢查、payload 不含租戶）；設備存在時刪它的型號 409 `device_model.in_use`，刪掉設備後可以刪；每種不合法的連線設定與取樣間隔經建立、更新兩個入口都回 422 與對的 field error，非 JSON、錯的 `Content-Type`、超過 16 KiB 回 400，什麼都不寫；違反命名規則的名稱、重複的名稱、改名、換閘道器、換型號、過期的 `revision`、讀成數字的名稱（`0x7f000001` 等）與 `ip6-localhost`、`ip6-loopback`、別的租戶的閘道器、沒綁定或綁定已結束的閘道器、不在設備池的閘道器、別的租戶的型號都被拒絕，兩個租戶都沒有寫入；同一個端點的站號重複（含 IPv4-mapped 的寫法）與傳輸方式混用 409，換連接埠或換閘道器就可以；兩個請求同時搶同一個站號或混用傳輸方式時一個 201 一個 409；私有位址、169.254/16、IPv6 ULA 與 `0x7f.plc.example` 這類名稱可以建立；綁定結束（直接在資料庫設定 `unbound_at`，閘道器再綁到別的租戶）後更新回 409 `device.binding_ended`、建立回 422，兩個租戶都沒有寫入也沒有事件，讀取與刪除照常；viewer 只能讀、沒有租戶 403、沒有 token 或錯的受眾 401；日誌不含主機 |
| `tests/migrations.rs` | Laysi/trustiot-v2#402：Job 寫的紀錄就是 `src/schema.rs` 預期的清單，服務 ready 並記 INFO；以 superuser 拿掉 `0001_assets.sql` 的紀錄時，新的行程 503，ERROR 日誌的 `missing` 寫出它，已經通過的行程維持 ready；紀錄回來後同一個行程轉為 ready；SHA-256 不同時 503，`changed` 寫出它；資料庫多一個較新版本的紀錄（映像被回退）時照常 ready，INFO 的 `newer` 寫出它；`assets_app` 只讀得到自己的紀錄、不能改，讀不到 `identity` 的紀錄與 `trustiot_migrate.applied`；`assets_platform`（能進 `identity` schema 讀事件）讀不到任何一個服務的紀錄；`identity_app` 讀不到 `assets` 的紀錄 |
