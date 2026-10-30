# gateway-ops（閘道器營運）

每台閘道器的 desired 設定，以版本存在 Postgres（決策 7、清單 E1）。服務在 Laysi/trustiot-v2#93 底下分段實作：2026-10-29 起有服務骨架、Postgres schema、資產事件的投影與帶版本的 desired 設定（Laysi/trustiot-v2#719），以及以 MQTT retained 訊息下發設定、閘道器的套用回報與重發（Laysi/trustiot-v2#720）；映像與 chart 的部署是 Laysi/trustiot-v2#721。閘道器端的套用是 Laysi/trustiot-v2#94。2026-10-30 起也記錄每台閘道器的在線狀態，並以 API 列出租戶的閘道器（Laysi/trustiot-v2#112，清單 C3）。

設定的格式與規則在 [crates/protocol](../../crates/protocol/README.md) 的「設定下發」（`DesiredConfig`）：這個服務產生的每一版都先以同一套驗證（閘道器套用前跑的那一套）檢查過，再以 crates/protocol 的編碼存下。

## 服務與設定

`cargo run -p gateway-ops`（`src/main.rs`）以 `crates/service-kit` 啟動：設定、JSON 日誌、`/healthz`、`/readyz`、指標與優雅關閉（SIGTERM）都照 service-kit。HTTP API 只有閘道器列表（「API」），規格在 `api/openapi.json`。啟動時建立兩個 Postgres 連線池（都是 lazy 連線，連線前做 service-kit 的 session 檢查），以兩條連線連上 MQTT broker（`crates/mqtt-client`，帳號都是 `service:gateway-ops`：一條給下發與套用回報，一條給在線狀態），讀內部身分的公鑰檔，並啟動資產服務事件的消費端（訂閱名稱 `gateway-ops.assets-events`，不可改名：新名稱會從頭重讀整個事件紀錄）。broker 設定不合規則或密碼檔讀不到時，服務不啟動（exit code 1）。

readiness 在下列全部成立時才是 200：

- `postgres`：服務角色與平台角色都能開出通過檢查的 session；
- migration 檢查：資料庫有這個版本預期的每個 Postgres migration 檔（`src/schema.rs`）；
- `mqtt`：連著 broker，而且套用回報的共享訂閱（`$share/gateway-ops/trustiot/v1/gw/+/up/config-report`）以 QoS 1 獲准。broker 斷線、重連中、訂閱被拒或只給 QoS 0 時不是 200；連上後恢復。
- `presence`：在線狀態的連線連著 broker，狀態與心跳的兩個訂閱（「在線狀態」）都以 QoS 1 獲准；規則同 `mqtt`。
- `internal_identity_keys`：service-kit 的 `PublishedKeys` 讀到了可用的內部身分公鑰（和其他有 API 的服務相同）。

| 設定 | 環境變數 | 說明 |
| --- | --- | --- |
| Postgres 位置與 TLS | `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`（只接受 `verify-full`）、`_CA_FILE` | chart 設定的平台設定 |
| 服務角色 | `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | `gateway_ops_app`；密碼來自 migration Job 產生的 Secret |
| 平台角色 | `TRUSTIOT__DATABASE__PLATFORM_USERNAME`、`TRUSTIOT__DATABASE__PLATFORM_PASSWORD` | `gateway_ops_platform`，必須和服務角色不同 |
| 連線數 | `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 服務角色每個 replica 的連線上限，9 到 100，預設 10。下限是服務在一個 API 請求時同時會用到的最多連線數：消費端的交易 1、readiness 的 ping 與 migration 檢查同時跑 2、重發的 job lock 1、發布 2（`delivery::CONCURRENCY` 個發布名額，每個名額先拿著閘道器的發布鎖到 PUBACK，再寫紀錄，同時只用 1 條）、套用回報的記錄 1、在線狀態的寫入 1、API 請求 1（`config.rs` 的 `MIN_DATABASE_CONNECTIONS` 逐條列出）；更大的連線池只讓更多 API 請求同時進行。平台角色另外固定 4 條（下發與在線狀態的讀取共用其中 2 條） |
| MQTT broker | `TRUSTIOT__MQTT__HOST`（必填）、`TRUSTIOT__MQTT__PORT`（選填，預設 1883）、`TRUSTIOT__MQTT__USERNAME`（必填，`service:gateway-ops`）、`TRUSTIOT__MQTT__PASSWORD_FILE`（必填） | 和 ingest 同一個形狀（`mqtt_client::Config`）；密碼只從檔案讀（broker 帳號的 Secret，migration Job 產生），不進環境變數與日誌 |
| 合併節流 | `TRUSTIOT__DELIVERY__THROTTLE`（選填） | 同一台閘道器的變更合併成一次發布的時間窗，100ms 到 60s，預設 1s（「下發」） |
| 心跳逾時的寬限 | `TRUSTIOT__PRESENCE__HEARTBEAT_GRACE`（選填） | 連續兩個心跳間隔之外再等的時間，1s 到 600s，預設 30s（「在線狀態」） |
| 逾時檢查的間隔 | `TRUSTIOT__PRESENCE__SWEEP_INTERVAL`（選填） | 多久找一次心跳逾時的閘道器，1s 到 60s，預設 10s |
| 內部身分的公鑰 | `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 身分服務發布的公鑰檔；API 以它驗證入口轉來的內部身分（chart 在身分服務啟用時給） |

`mqtt` 一段是必填：下發是這個服務存在的目的，readiness 也要看 broker。chart 的 `services.gateway-ops`（Laysi/trustiot-v2#721）預設關閉，開啟時要求 `mqttBroker.enabled`，沒有 broker 就渲染失敗；chart 以 broker 的 Service 與這個服務自己的 broker 帳號填這一段，nightly 部署它並以 readiness 確認它連上 broker（見下方「部署」）。

本機以 `bun scripts/dev.ts env` 印出的 `TRUSTIOT_DEV_GATEWAY_OPS_DATABASE_*` 與 `TRUSTIOT_DEV_GATEWAY_OPS_PLATFORM_DATABASE_*` 設定兩個角色（[scripts/README.md](../../scripts/README.md)）。開發環境的 Mosquitto 用靜態帳號、沒有 `service:gateway-ops`，所以在本機跑 binary 要另外給一個 broker 與帳號；測試用的是 `tests/support/broker.rs` 起的 Mosquitto（「測試」）。

## Postgres schema 與角色

schema `gateway_ops` 由 migration Job（`tools/migrate`）建立並套用 `migrations/postgres/`；registry 把 gateway-ops 列為資產服務事件的消費端，所以 Job 也建立 outbox 的消費端表（`outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version`），並讓 `gateway_ops_platform` 讀得到 `assets.outbox_event`。

每張表都是租戶資料，照 `crates/service-kit/sql/tenant-table.sql`：`tenant_id` 是主鍵的第一欄，`tenant_isolation` policy 把每個角色限制在交易的租戶裡。

| 表 | 內容 | `gateway_ops_app` | `gateway_ops_platform` |
| --- | --- | --- | --- |
| `gateway_bindings` | `assets.gateway_bound` 的投影：每個綁定一列，記綁定事件在資產服務事件紀錄裡的位置；`unbound_at` 在綁定結束時填上 | `SELECT`、`INSERT`、只有 `unbound_at` 的 `UPDATE` | `SELECT`（`platform_access`，所有租戶） |
| `device_models` | `assets.device_model_*` 的投影：名稱、revision、`protocol::config::DeviceModel` 的 JSON；刪除的型號留下 tombstone（`deleted`，沒有 `model`） | `SELECT`、`INSERT`、`UPDATE` | 無 |
| `devices` | `assets.device_*` 的投影：閘道器的硬體 ID、型號、所屬綁定（事件的 `binding_id`）、revision、`protocol::config::Device` 的 JSON；刪除的設備同樣留 tombstone | `SELECT`、`INSERT`、`UPDATE` | 無 |
| `desired_configs` | 每台閘道器每一版的設定：版本、所屬綁定、`bound`、編碼後的 `DesiredConfig`（`payload`）、設備數、產生它的事件；只新增，服務不改、不刪 | `SELECT`、`INSERT` | `SELECT`（`platform_access`，所有租戶） |
| `gateway_presence` | 每個綁定一列（外鍵指向 `gateway_bindings`，在綁定的租戶）：`state`（`online`、`offline`）、目前的 MQTT `session`、離線原因（`shutdown`、`connection_lost`、`heartbeat_timeout`）、狀態改變的時間 `changed_at`、最後收到這個 session 任何訊息的時間 `last_seen_at`、agent 版本，以及最近一次心跳的時間、宣告的間隔與閘道器正在用的設定版本（`config_version`） | `SELECT`、`INSERT`、只有狀態與心跳欄位的 `UPDATE` | `SELECT`（`platform_access`，所有租戶：心跳逾時的檢查） |
| `config_deliveries` | 每個發布過或收到回報的版本一列，在**該版本的租戶**（外鍵指向 `desired_configs`）：broker 第一次與最近一次確認（PUBACK）retained 發布的時間；閘道器對這一版最近一次的回報：`outcome`（`applied`、`rejected`）、`active_version`、`issues`（JSON 陣列）、服務收到的時間 `reported_at` | `SELECT`、`INSERT`、只有時間與回報欄位的 `UPDATE` | 無 |

三個刻意的例外是「`tenant_id` 不在索引第一欄」：`gateway_bindings_by_gateway`（閘道器目前的綁定是跨租戶的問題）、唯一索引 `desired_configs_gateway_version (hardware_id, version)`：一台閘道器的每個版本號只用一次，不管當時是哪個租戶，以及部分索引 `gateway_presence_online (last_seen_at) WHERE state = 'online'`：心跳逾時的檢查跨租戶找最久沒消息的在線閘道器。

### 為什麼跨租戶的讀取用平台角色（決策 29）

產生設定前要知道兩件跨租戶的事：這台閘道器目前綁在哪個租戶，以及它最新的版本號是多少（版本號跨綁定遞增）。和 ingest、certs 的綁定對照表一樣，這是平台角色在 `READ ONLY` 交易裡、透過 `platform_access`（只開 `SELECT`）讀的；服務角色永遠只讀得到交易租戶的列。投影的寫入、讀設備與型號、存新版本，都在消費端以服務角色開的、事件所屬租戶的交易裡，所以產生設定時根本讀不到別的租戶的設備。

## 綁定與租戶

閘道器的租戶一律是它目前的綁定（清單 D3）：綁定事件在資產服務事件紀錄裡的位置（`position_tx`、`position_id`）最後的那一個，而且還沒結束（`unbound_at` 是 NULL）。不採信事件以外的任何來源，也不用生產者的時鐘排序。

設備事件 `assets.device_created`、`_updated`、`_deleted` 自 2026-10-29 起帶選填的 `binding_id`：寫入事件時設備所屬的綁定（[crates/contract](../../crates/contract/README.md)、[services/assets](../assets/README.md)）。一台閘道器的設定只收：

1. 事件租戶就是閘道器目前綁定的租戶的設備；
2. 而且設備的綁定就是目前的綁定。

設備的綁定依序取：事件的 `binding_id`；投影裡已記錄的綁定；都沒有時（2026-10-29 以前寫的事件，而且投影第一次看到這個設備），取同一個租戶、同一台閘道器的綁定裡，事件紀錄位置在這個設備事件**之前**最後的那一個，也就是資產服務寫這個事件時設備所屬的綁定。以事件位置而不是目前狀態推斷，所以重播（`Replay::Reprocess`）得到同樣的結果。仍然找不到時，`binding_id` 存 NULL，這個設備不進任何設定。

不符合的設備事件仍記進投影（它是該租戶的資料），但不產生新版本，也不出現在任何設定裡：

| 情況 | 處理 | 日誌與指標 |
| --- | --- | --- |
| 閘道器目前沒有綁定（還沒綁，或已解綁） | 不產生 | INFO `device event ignored for the gateway configuration`，`reason=unbound` |
| 閘道器綁在別的租戶 | 不產生 | **WARN**，`reason=other_tenant`：資產服務在綁定之外寫了設備，值得追查 |
| 設備屬於同一租戶先前的綁定 | 不產生 | INFO，`reason=ended_binding` |
| 設備的綁定不明（事件沒有 `binding_id`，事件之前這個租戶也沒有綁過這台閘道器） | 不產生 | **WARN**，`reason=unknown_binding`：資產服務只在綁定之內寫設備 |

剩下的風險：推斷用的是綁定事件的位置；解綁（Laysi/trustiot-v2#115）只記時間、沒有位置，所以如果某個 2026-10-29 以前寫的設備事件，寫在租戶的綁定結束之後、同一租戶再次綁定之前，投影會把它歸到結束的那個綁定（不進任何設定）。資產服務不在綁定之外寫設備，所以這不會發生；而 2026-10-29 以後的事件都帶 `binding_id`，不需要推斷。

## 由事件到設定

消費端依序處理資產服務的這些事件；同一個實體的事件依 aggregate 排序，事件不在自己實體的 aggregate 裡就送進 dead letter：

| 事件 | 投影 | 重新產生哪些閘道器 |
| --- | --- | --- |
| `assets.gateway_bound` | 新增綁定；同一個綁定再來一次不變 | 綁定是這台閘道器目前的綁定時，產生它在這個綁定下的設定（新綁定沒有設備時是空的設定，版本照樣往上） |
| `assets.device_model_created`、`_updated`、`_deleted` | revision 較新才取代；刪除留下 tombstone | 用到這個型號、而且目前綁在事件租戶的閘道器 |
| `assets.device_created`、`_updated`、`_deleted` | revision 較新才取代；刪除留下 tombstone | 設備所在的閘道器，符合上一節的條件時 |

產生的規則（`src/generate.rs`）：

- 設定只列這個租戶、這台閘道器、這個綁定、沒有刪除的設備，與它們用到的型號；每個型號只列一次。綁定不明（NULL）的設備不在任何設定裡。
- 順序固定：型號依名稱、設備依 ID，所以投影內容相同就產生相同的設定。
- 刪除的設備真的從下一版消失；刪除只是 tombstone，晚到的舊 revision 不會讓它復活。
- 新的內容和閘道器最新的一版（同租戶、同綁定、`bound`）相同時不存新版：重播事件、重複投遞、晚到的舊 revision、只改了設定不帶的欄位（例如設備名稱）都不會多出版本。
- 存之前以 crates/protocol 的規則驗證，並檢查編碼後的大小。

一個事件影響的閘道器在同一個交易裡處理：投影與新版本一起提交，或一起回滾後重試。

## 版本

- 版本號是 crates/protocol 的 `ConfigVersion`（從 1 起），每台閘道器各自遞增，**跨綁定、跨租戶**：下一版是這台閘道器最新一版加一，不管最新一版屬於哪個租戶。閘道器因此不會看到內容較新、版本卻較小的設定。
- 版本號寫在 `payload` 裡，也在 `version` 欄；讀取介面會比對兩者。
- 只有這個服務的消費端寫 `desired_configs`，而訂閱的鎖讓同一時間只有一個 replica 在處理事件，所以讀到的最新版本在寫入前不會改變。萬一這個前提被打破，跨租戶的唯一索引讓第二個寫入失敗（`23505`），消費端重試時重新讀取。

## 錯誤

產生不出設定時記錄明確的錯誤（清單 E1「引用不存在的資源要明確報錯」）；事件本身照樣算處理完，等修好投影的事件（型號送到、設備修正）再產生。這段期間：

| 情況 | 存什麼 | 為什麼 |
| --- | --- | --- |
| 閘道器最新的一版是為**同一個綁定**做的 | 不存新版，閘道器保留上一版 | 上一版已經驗證過，而且屬於目前的綁定 |
| 最新的一版是別的綁定（包括別的租戶）的，或還沒有任何版本，例如改綁到另一個租戶時新租戶的設定產生不出來 | 存一版**沒有設備**、`bound = true` 的設定，屬於目前的綁定 | 保留上一版等於讓下發（Laysi/trustiot-v2#720）把前一個租戶的設備送給已經綁到別的租戶的閘道器 |
| 事件是刪除設備、刪除在投影裡生效（不是過期的 revision，也不是重複的刪除），而且最新的一版（同一個綁定）列著它 | 存一版「最新的一版去掉這個設備」（也去掉不再有設備使用的型號） | 刪除的設備要真的從閘道器消失（清單 E1）；這一版只比閘道器已經在跑的少，裡面的東西都驗證過。例如設備超過 `MAX_DEVICES`（1000）或超過編碼大小上限時，每個事件都失敗，刪除仍然生效 |

原因（`reason`）：

| `reason` | 原因 |
| --- | --- |
| `unknown_model` | 設備引用的型號不在投影裡（還沒收到，或已刪除） |
| `model_mismatch` | 設備寫的型號名稱和型號不同 |
| `invalid` | 設定違反 crates/protocol 的規則，例如同一個 converter 上混用傳輸方式 |
| `encode` | 編碼失敗，例如超過 MQTT 頻道的大小上限 |
| `corrupt` | 投影裡存的列讀不回來（表的規則應該讓它不會發生） |

每次失敗記一行 ERROR，帶 `hardware_id`、`tenant_id`、`binding_id`、`event_id`、`reason` 與 `error`；`invalid` 的 `error` 只列違反的規則代碼與路徑，不列連線設定。保留上一版時訊息是 `gateway configuration not generated; the gateway keeps its previous version`；存了替代的版本時是 `gateway configuration not generated; a fallback version was stored`，另帶 `fallback`（`empty` 或 `without_deleted_device`）與 `version`。

每台閘道器的設備上限屬於資產服務，要在寫入時擋（清單 B2）；2026-10-29 資產服務還沒有這個上限。這裡只保證超過時刪除仍然生效。

事件本身有問題時（沒有租戶、不在自己的 aggregate、revision 超出範圍、和已記錄的硬體 ID、型號名稱或綁定不一致、表拒絕的值）送進 dead letter，訂閱繼續往下。資料庫錯誤重試。這個版本不認得、但屬於型號、設備或綁定 aggregate 的事件（例如 Laysi/trustiot-v2#115 的解綁事件）一律重試、不確認：確認了就永遠漏掉一次設定變更，停在那裡時消費端的延遲會顯示出來。所以**發版順序**是 gateway-ops 先認得新事件，資產服務才開始寫它。

## 下發

`src/delivery/`：每台閘道器最新的一版（`DesiredConfigs::current`，所以讀取介面拒絕的版本永遠不會送出）以 **retained、QoS 1** 原樣發布到 `trustiot/v1/gw/<硬體 ID>/down/config`（crates/protocol 的 `Channel::Config`）。

- **發布才算數。** 收到 broker 的 PUBACK 之後才算送出：這個 replica 記下每台閘道器送出過的最高版本，之後**永遠不送比它低的版本**（讀到比它低的最新版本代表資料庫倒退，記 ERROR `reason=older_than_sent`，不送）；同時在該版本的租戶記一筆 `config_deliveries`（寫入失敗只記 WARN，發布本身已經完成）。
- **合併節流。** 消費端每存一版就通知下發（`DeliveryHandle::changed`）。同一台閘道器在節流時間窗（`delivery.throttle`，預設 1s）內的通知合併成一次發布，送的是時間窗結束時的最新版本；時間窗從第一個通知起算，後來的通知不會延後它，所以一直在變的閘道器每個時間窗仍會收到一次。`src/delivery/schedule.rs` 以手動時鐘測試這個規則。
- **等交易提交。** 通知發生在事件的交易提交之前，所以發布前會等到通知的版本讀得到；讀到的最新版本比通知的舊時，前 60 秒每個時間窗重讀一次，之後每 60 秒重讀一次並記 WARN `an announced configuration version is not visible yet; still waiting for it`，不放棄：慢的交易仍可能提交；交易回滾時，這台閘道器下一個存下的版本會用同一個版本號，那時就送出。
- **replica 之間。** 任何 replica 都可能發布（自己消費端的變更、重發、套用回報）。每台閘道器的「讀最新版本＋發布」在它自己的 Postgres advisory lock 底下進行（交易層級，`delivery::publish_lock_key`，以服務角色的連線取得），一直拿到收到 PUBACK：下一個拿到鎖的 replica 讀到的版本不會比較舊，而且在 broker 收下前一則之後才送，所以較舊的版本不會蓋掉較新的 retained 訊息。鎖被別的 replica 拿著時不等，過一個時間窗再試。每次發布之後再重讀最新版本，比剛送的新就再排一次；這次重讀失敗，或放開鎖的 commit 失敗時，5 秒後再讀一次（比這個 replica 送過的新才發布）。
- **錯誤。** 連線中斷時這次發布留在排程裡，連回來馬上再送（連線後的重發也會涵蓋）。讀取遇到資料庫錯誤時 5 秒後重試。broker 拒絕（例如帳號無權發布）、超過大小、topic 不合法，以及讀取介面回 `NotForLatestBinding` 或 `Corrupt` 時記 ERROR、不重試：這些重試也不會好。
- **並行與連線。** 每個 replica 同時最多發布 2 台閘道器（`delivery::CONCURRENCY` 個發布名額，排程的發布與重發共用），平台角色的讀取也最多 2 個；同一台閘道器同時只會有一個發布。一次發布依序拿發布名額、讀取的名額，最後才拿服務角色的連線取發布鎖，所以鎖的連線只等鎖底下的讀取與 PUBACK；放開鎖之後在同一個名額裡寫紀錄。發布因此同時最多用 2 條服務角色的連線（連線數下限見「服務與設定」）。
- **日誌。** 每次發布記 INFO `gateway configuration published`，帶 `hardware_id`、`tenant_id`、`version`、`bound`、`trigger`（`change`、`report`、`republish`）；重發的發布記 DEBUG，避免整個機隊一次寫滿日誌。日誌與錯誤都不帶 broker 密碼：密碼只從檔案讀（`crates/mqtt-client`），日誌最多出現檔案路徑。

## 套用回報

閘道器以 QoS 1、不 retained 發布 `ConfigReport` 到 `trustiot/v1/gw/<硬體 ID>/up/config-report`。服務以共享訂閱 `$share/gateway-ops/trustiot/v1/gw/+/up/config-report` 收，每則回報只到一個 replica（`src/delivery/reports.rs`）。連線的事件迴圈只做第 1、2 步，通過的回報放進收件匣（`reports::Inbox`），由另一個 task 依序做第 3 步之後的事，所以資料庫慢或斷線時連線的事件（readiness、重發）照常處理。收件匣每台閘道器最多一則（它最新的那則，新的一到就取代還沒處理的舊的，計入 `gateway_ops.reports.unhandled` 的 `result=superseded`），依閘道器排隊的先後處理，取代不改變那台閘道器在隊伍中的位置，所以一台不停送回報的閘道器只占一個位置，擠不掉別台的回報；收件匣最多 4096 台閘道器，滿了放棄排最久的那則（`result=lost`）。

處理每則回報的步驟：

1. **閘道器以 topic 為準。** 回報屬於 topic 上的硬體 ID；payload 裡多出來、指向別台閘道器的欄位一律忽略（crates/protocol 的解碼略過不認得的欄位），閘道器的憑證只能發布自己的 topic。
2. **協定檢查。** topic 不是 `up/config-report`、帶 retain 旗標（這個頻道不 retained，crates/protocol 規定視為違規丟棄）、以 QoS 0 送達、解不開，或 `outcome` 不是 `applied`／`rejected` 的回報都拒絕。
3. **版本檢查。** 以平台角色讀這台閘道器的最新版本與回報那一版的租戶與綁定（`DesiredConfigs::report_target`），下列情況拒絕、不記錄：

   | `reason` | 情況 |
   | --- | --- |
   | `unknown_gateway` | 這台閘道器沒有任何版本 |
   | `version_ahead` | `version` 或 `active_version` 比最新版本大：服務從沒產生過這一版 |
   | `unknown_version` | 回報的版本不存在 |
   | `not_for_latest_binding` | 最新的一版不屬於閘道器最新的綁定（讀取介面的規則） |
   | `previous_binding` | 回報的版本屬於先前的綁定：記在舊租戶等於讓閘道器寫進它已經不屬於的租戶 |

4. **記錄。** 通過的回報記在**回報那一版的租戶**（`config_deliveries`，服務角色、該租戶的交易）：`outcome`、`active_version`、`issues`、收到的時間；同一版再回報就覆蓋，留下最近一次；和那一版已存的回報完全相同（`outcome`、`active_version`、`issues`）而且寫入不到 60 秒的，不再寫入也不記日誌，只計入 `gateway_ops.reports.repeated`，第 5 步照樣進行。這個判斷是寫入語句本身的條件（`ON CONFLICT ... DO UPDATE ... WHERE`），所以不論回報落在哪個 replica 都成立。記 INFO `apply report recorded`，帶 `hardware_id`、`tenant_id`、`version`、`outcome`、`active_version` 與 `issue_codes`（只有代碼，不帶 issue 的訊息）。

   **檢查與寫入之間。** 第 3 步的平台角色讀取與第 4 步租戶交易裡的寫入不在同一個交易，中間沒有鎖：閘道器剛好在這之間改綁到別的租戶時，回報仍會寫進原租戶。寫進去的只是原租戶自己那一版設定的套用結果（版本、`outcome`、`active_version`、issue），和回報在改綁之前送達的結果相同，不會寫進新租戶，也不帶新租戶的任何資料，所以這個版本不在租戶交易裡再確認一次綁定。之後若回報要寫進比這更敏感的資料，或要以回報驅動權限相關的動作，就要在寫入的交易裡重新確認綁定。

5. **舊版本的回報。** 回報的 `version` 比最新版本舊時（閘道器收到過時的 retained 訊息，或回了 `stale_version`），不論有沒有記錄，都把最新版本再排一次發布（照節流，所以一直送回報的閘道器每個時間窗最多觸發一次發布）。crates/protocol 要求雲端收到 `stale_version` 時以比 `active_version` 大的版本重發：最新版本永遠不比閘道器套用過的小，因為比最新版本大的 `active_version` 在第 3 步就被拒絕。

**資料庫錯誤。** client 把回報交給服務時就回了 PUBACK（crates/mqtt-client），閘道器不會再送同一則；所以第 3、4 步遇到資料庫錯誤時，回報回到收件匣的隊尾，5 秒後重試，從收到起最多 10 分鐘；處理中這台閘道器又送來新回報時，失敗的那則作廢（`result=superseded`），不會之後蓋掉新的。每次資料庫錯誤之後，回報的處理暫停 5 秒：資料庫多半斷了，等著的回報不會一口氣打過去，每次嘗試最多等連線池的 5 秒（`ACQUIRE_TIMEOUT`）。放棄時記 WARN `apply report lost: ...`，計入 `gateway_ops.reports.unhandled`（`result=lost`）。剩下會漏的情況：資料庫斷超過 10 分鐘，或 replica 在重試期間重啟；漏掉的只是那一則回報的紀錄，下一次發布（重發或新版本）後閘道器會再回報。被取代的回報刻意不記：同一台閘道器較新的回報才是它現在的狀態，舊回報那一版的列就少一次回報。

被拒的回報記 WARN `apply report refused`，帶 `reason`、`hardware_id`、`version`、`active_version`；每個 replica 每 60 秒最多記 60 行，被省略的行數記在下一行的 `suppressed_logs`，所以一台送垃圾的閘道器寫不滿日誌。指標不受這個限制。

## 重發

每次連上 broker（包括啟動後的第一次）都重發**所有**閘道器的最新版本（retained、QoS 1）：broker 重啟或清掉 retained 訊息時，閘道器之後連線就拿不到設定（docs/feasibility/2026-10-09-cloud-poc.md「MQTT broker」第 4 點），而服務無法分辨 broker 是否保住了它們。

- 重發在 job lock `gateway-ops`/`config-republish`（`service_kit::job_lock`，服務角色的連線）底下執行，同一時間只有一個 replica 在重發；拿不到鎖的 replica 記 INFO `republish skipped; another replica holds its lock and republishes`，交給持有者，不重複發布。
- 讀 `latest_versions()` 列出每台閘道器，以同樣的發布路徑逐台送出（強制送，即使這個 replica 送過同一版）。期間連線又斷時中止，下一次連上再從頭重發。讀取或鎖失敗時 5 秒後重試，直到連線斷開。
- 完成時記 INFO `republished every gateway's configuration`，帶 `gateways`。

## 未綁定的設定與 retained 訊息

`bound = false` 的設定（「解綁」）和其他版本一樣下發與重發：它沒有設備，閘道器套用後停止輪詢、清掉舊設備。這個版本**不清除** retained 訊息：

- 規則：一台閘道器的 retained 訊息永遠是它的最新版本。解綁後留著的是一版沒有設備的設定，不帶任何租戶的設備，留著不會洩漏資料；閘道器改綁時，新租戶的第一版版本號比較大，直接取代它。
- 為什麼不清：清除是發布一則空的 retained 訊息。閘道器收到空的訊息時保留目前的設定、不回報（crates/protocol），所以在閘道器回報 `applied` 那一版**之前**清除，閘道器就會一直輪詢舊設備；而重發會在下一次連線時把那一版再送回去。
- 之後要清除（例如閘道器報廢）時的條件：閘道器已回報 `applied` 那個 `bound = false` 的版本，而且重發要略過這種閘道器；這屬於閘道器生命週期的工作（Laysi/trustiot-v2#115 之後），不在這裡。

## 指標

meter `gateway-ops`：

| 指標 | 屬性 | 意義 |
| --- | --- | --- |
| `gateway_ops.config.versions` | 無 | 存下的新版本數（包括替代的版本） |
| `gateway_ops.config.failures` | `reason`（上表） | 產生不出設定的次數；大於 0 就有閘道器沒拿到完整的設定 |
| `gateway_ops.config.ignored_device_events` | `reason`：`unbound`、`other_tenant`、`ended_binding`、`unknown_binding` | 沒有產生設定的設備事件 |
| `gateway_ops.delivery.published` | `trigger`：`change`、`report`、`republish` | broker 確認的 retained 發布 |
| `gateway_ops.delivery.coalesced` | 無 | 合併進等待中發布的變更通知 |
| `gateway_ops.delivery.failures` | `reason`：`connection`、`refused`、`too_large`、`invalid`、`read`、`corrupt`、`not_for_latest_binding`、`older_than_sent` | 失敗或沒有送出的發布 |
| `gateway_ops.delivery.republish_runs` | `result`：`done`、`skipped`、`interrupted`、`failed` | 連線後的重發 |
| `gateway_ops.reports.recorded` | `outcome` | 記錄下來的套用回報 |
| `gateway_ops.reports.refused` | `reason`（「套用回報」的表，加上 `topic`、`retained`、`qos0`、`decode`、`unknown_outcome`） | 被拒的套用回報 |
| `gateway_ops.reports.repeated` | 無 | 沒有再寫入的套用回報：和那一版 60 秒內寫入的回報相同（任何 replica 寫的） |
| `gateway_ops.reports.unhandled` | `result`：`retry`、`superseded`、`lost` | 沒處理完的套用回報：資料庫錯誤後排進重試、被同一台閘道器較新的回報取代，或放棄（重試用完、收件匣滿了） |
| `gateway_ops.presence.changes` | `state`、`reason`（`shutdown`、`connection_lost`、`heartbeat_timeout`，在線時是 `none`） | 閘道器在線狀態的改變 |
| `gateway_ops.presence.ignored` | `reason`：`other_session`、`ended_session`、`repeated`、`older`、`replayed`、`unbound`、`cleared` | 沒有改變在線狀態的訊息（「在線狀態」的規則） |
| `gateway_ops.presence.refused` | `refusal`：`topic`、`retained_heartbeat`、`decode`、`unknown_reason` | 碰資料庫前拒絕的狀態與心跳 |
| `gateway_ops.presence.failures` | 無 | 在線狀態的資料庫錯誤 |
| `gateway_ops.presence.dropped` | `reason`：`gateway_backlog`（這台閘道器已有 16 則在等）、`backlog`（全部已有 50 000 則在等） | 等待佇列滿了而丟掉的狀態與心跳 |

## 在線狀態

清單 C3（Laysi/trustiot-v2#112）。協定在 [crates/protocol](../../crates/protocol/README.md) 的 `health`：閘道器的 agent 連線時以 retained 發布 `up/status` 的 `online`，正常結束時發布 `offline`（`shutdown`），last will 是 `offline`（`connection_lost`）；每 60 秒發布一次 `up/heartbeat`（不 retain），MQTT keepalive 60 秒。閘道器端不需要改。

- **訂閱**：在線狀態有自己的 broker 連線，以不共享的訂閱收每台閘道器的 `up/status` 與 `up/heartbeat`：每個 replica 都依 broker 送來的順序收到每台閘道器的每則訊息（每次連線後也重新收到 retained 的狀態），所以沒有 replica 只看到狀態、看不到夾在中間的心跳。broker 的角色 `service:gateway-ops` 本來就收這兩個 channel（`tools/migrate` 的 `mqtt_accounts`）。代價是每個 replica 都處理每則心跳；心跳的柵欄（下面的規則）讓第二份不再寫入。
- **等待佇列**（`src/presence/backlog.rs`）：broker 連線只負責收訊息、檢查後放進該閘道器的佇列，不等資料庫；一個 worker 輪流從每台閘道器的佇列取一則套用。一台閘道器最多 16 則在等、全部最多 50 000 則，超過的丟掉（`gateway_ops.presence.dropped`，WARN 有額度），所以灌訊息的閘道器只丟自己的訊息，其他閘道器每輪最多多等一則。逾時檢查照自己的間隔跑，不會被訊息擠掉；檢查時把佇列裡最舊一則等待的時間加進寬限，閘道器的最新心跳還在佇列裡時不會被誤判逾時。全部的佇列滿了時，逾時的時鐘重新開始。
- **斷線**：連線沒有 DISCONNECT 就斷掉時，broker 馬上發布 last will；半開的連線由 broker 的 keepalive 檢查在 1.5 倍 keepalive（90 秒）內發布 last will（決策 6）。服務收到後把那個 session 標成 `offline`（`connection_lost`）。
- **心跳逾時**：每 `presence.sweep_interval` 找出在線、但同一個 session 已經「兩個宣告的心跳間隔（最多 3600 秒，沒宣告時 60 秒）＋ `presence.heartbeat_grace`」沒有任何消息的閘道器，標成 `offline`（`heartbeat_timeout`），預設 150 秒。這是後備：broker 連線還在但 agent 不再回報，或 last will 遺失（broker 重啟時連同 retained 狀態一起遺失）。只有服務聽得到心跳的時間才算：在線狀態的 broker 連線或資料庫恢復後，重新等滿整段逾時，服務自己斷線不會讓所有閘道器看起來離線。逾時的寫入以條件更新（狀態、session 與 `last_seen_at` 都沒變）進行，所以和同時到的心跳不會互相覆蓋，多個 replica 一起檢查也只寫一次。
- **規則**（`src/presence/rules.rs`，每則訊息在鎖住閘道器那一列的交易裡套用，依連線送來的順序）：
  - 第一則訊息建立那一列。
  - 新 session 的 `online` 或心跳取代舊 session；舊 session 晚到的 `offline` 或心跳不理（`other_session`），所以重新連線後舊連線的 last will 不會把閘道器標成離線。
  - 同一個 session 以 `shutdown` 或 `connection_lost` 結束後，它的 `online` 與心跳不理（`ended_session`）：broker 重送 retained 的舊 `online` 不會讓閘道器復活。
  - 被心跳逾時標成離線的 session 再送心跳時回到 `online`；之後同一個 session 的 last will 或 `shutdown` 只改離線原因。
  - 重複的 `online` 與 `offline` 不再寫入（`repeated`）。
  - 心跳的柵欄：同一個 session 的心跳，`sent_at`（閘道器的時鐘）不比那一列記的最新一則新時不寫（相同是 `repeated`，較舊是 `older`）：別的 replica 已經寫過，或落後的 replica 晚到的舊心跳不會把設定版本或間隔倒回去。新的 session 從沒有柵欄開始。
  - replica 之間：落後的 replica 可能在別的 replica 寫了新 session 之後，才套用舊 session 的 `online`，讓那一列暫時回到舊 session；它自己的訊息流後面就有新 session 的訊息，趕上之後那一列就對了（`rules.rs` 的單元測試列舉兩個 replica 的所有交錯順序）。落後的 replica 若丟了那些訊息，舊 session 由心跳逾時結束，新 session 的下一則心跳讓閘道器回到在線。
- **未綁定的閘道器**：讀不到目前綁定的閘道器不記錄（`gateway_ops.presence.ignored{reason=unbound}`）；未綁定的閘道器沒有 broker 角色，本來就發布不了（決策 6）。寫入的交易在 share lock 下再確認一次綁定還有效，所以在讀綁定與寫入之間被解綁、改綁到別的租戶的閘道器，不會寫進原租戶那一列（也算 `unbound`）；投影本身落後於 broker 角色的切換時，這段時間的訊息照投影當時的綁定記。綁定結束後那一列留在原租戶，列表不再列出它。
- **retained 的重播**：訂閱時 broker 重送的 retained 狀態，不建立綁定的第一列（`replayed`）：它可能是前一個綁定時期留下的。第一列由下一則即時的狀態或心跳建立（心跳每 60 秒一次）。
- **拒絕**：topic 不是閘道器的 `up/status` 或 `up/heartbeat`、retain 的心跳、解不開或不合協定規則的 payload、這個版本不認得的離線原因，都在碰資料庫前拒絕，WARN 有額度。空的 `up/status`（清掉的 retained 訊息）不代表任何事，不理（`cleared`）。
- **資料庫錯誤**：記 WARN，之後 5 秒（`presence::RETRY_DELAY`）不碰資料庫，訊息照樣排進佇列（超過上限的丟掉），逾時的時鐘重新開始。資料庫裡不合規則的列記 ERROR。
- **事件**：每次狀態改變記一行 INFO `gateway presence changed`（`hardware_id`、`tenant_id`、`binding_id`、`state`、`offline_reason`、`session`）並加 `gateway_ops.presence.changes`。這還不是 outbox 事件：migration Job 要求每個 outbox 的生產者至少有一個消費端，而目前沒有服務要跟著在線狀態走；告警（Laysi/trustiot-v2#118）要用時再加 `gateway_ops.gateway_presence_changed` 事件。

## API

`GET /api/gateway-ops/v1/gateways`（`src/gateways.rs`；權限矩陣在 [docs/architecture/permissions.md](../../docs/architecture/permissions.md) 的「gateway-ops」）：呼叫者租戶的 viewer 以上（`UserIdentity::require`），回應 `Cache-Control: no-store`。以服務角色在呼叫者租戶的交易裡讀（決策 29），列出租戶目前綁著的每台閘道器（`gateway_bindings` 裡 `unbound_at` 是空的綁定）：

| 欄位 | 內容 |
| --- | --- |
| `hardware_id`、`binding_id`、`bound_at` | 閘道器與綁定 |
| `presence` | `online`、`offline`，綁定之後還沒收到任何狀態或心跳時是 `unknown` |
| `offline_reason`、`presence_changed_at` | 離線原因與狀態改變的時間 |
| `last_seen_at` | 最後回報時間：最後收到目前 session 任何狀態或心跳的時間 |
| `agent_version` | agent 在目前（或最後一次）連線宣告的版本 |
| `applied_config_version` | 已套用的設定版本：閘道器在目前（或最後一次）連線的最近一次心跳宣告正在用的版本；這次連線還沒送心跳時是 `null`（新的連線不沿用上一次連線的心跳欄位） |
| `desired_config_version` | 這個綁定最新的 desired 版本；和 `applied_config_version` 不同時代表閘道器還沒套用最新設定 |

一台閘道器同時只有一個目前綁定（資產服務的規則），所以每台閘道器一列。列表不分頁：一個租戶的閘道器數量在展示切片的規模內；租戶的閘道器多到需要時再加分頁。

API 和背景工作共用服務角色的連線池；同時讀取的請求最多是連線池扣掉背景工作最多用到的連線（`gateways::request_slots`，連線池 9 條時是 1 個），再多的請求馬上回 503 `unavailable`，不去搶在線狀態與下發要用的連線。

## 解綁

解綁事件本身是 Laysi/trustiot-v2#115；這裡先提供 `AssetsEventsHandler::apply_unbinding`，由它的事件處理在同一個交易裡呼叫：

- 綁定填上 `unbound_at`；它是這台閘道器目前的綁定時，在原租戶存一版**沒有設備**、`bound = false` 的設定（版本照樣遞增）。重複呼叫只存一次；這個服務沒記錄過的綁定什麼都不做（WARN）。
- 之後這個綁定的設備事件都不產生設定（`reason=unbound`）。
- 下發的規則：`bound = false` 的設定照樣送到閘道器，讓它停止輪詢、清掉舊設備（清單 D4「清理舊設定」）；retained 訊息這個版本不清除，規則與原因在「未綁定的設定與 retained 訊息」。閘道器改綁到下一個租戶時，新租戶的第一版設定版本號比這版大，閘道器照常套用。
- 歷史版本不刪：它們是審計紀錄，在原租戶的租戶隔離底下。

## 讀取介面

`gateway_ops::desired::DesiredConfigs`，以平台角色的 `READ ONLY` 交易讀：

- `current(&GatewayId)`：這台閘道器最新的設定（不管租戶），`StoredConfig` 帶租戶、綁定、版本、`bound`、設備數、建立時間與 `payload`（原樣發布）；沒有任何版本時是 `None`。
- `latest_versions()`：每台閘道器最新的版本號，給連線後的重發用。
- `report_target(&GatewayId, ConfigVersion)`：給套用回報用，不讀 `payload`：最新版本與它的租戶、綁定，是否屬於最新綁定，以及回報那一版的租戶與綁定（不存在時是 `None`）。

`current` 與 `latest_versions` 讀到 `payload` 與 `version` 不一致、或欄位不合規則的列時回 `ReadError::Corrupt`。

兩者也都不交出不屬於閘道器最新綁定的設定：最新的一版必須是這台閘道器最新綁定（綁定事件在紀錄裡最後的那一個）的租戶與綁定，而且 `bound` 和綁定是否還有效一致。投影在讓一個綁定成為目前綁定的同一個交易裡就存它的一版，所以不一致代表規則被打破；這時 `current` 回 `ReadError::NotForLatestBinding`，`latest_versions` 略過那台閘道器並記 ERROR，下發端不會把一個租戶的設備送給綁在別的租戶的閘道器。

## 部署

映像是 `deploy/images/gateway-ops`（`image.toml`，arm64，以 `deploy/images/rust-binary.Containerfile` 打包），release.yml 發佈、chart 以 digest 固定（[deploy/images/README.md](../../deploy/images/README.md)）。chart 的 `services.gateway-ops`（Laysi/trustiot-v2#721，細節在 [chart README](../../deploy/chart/README.md) 的「gateway-ops」）：

- 預設 `enabled: false`；開啟時 `mqttBroker.enabled` 也必須是 `true`，否則渲染失敗：設定只經叢集內的 broker 下發。
- `mesh.calls.gateway-ops` 預設 `[mqtt-broker, postgres]`：mesh 只放行它連 broker 的服務埠 1883。API 只有入口連得到，而且要部署在 `ingress.routes` 加一條 `{host: app, path: /api/gateway-ops, service: gateway-ops}`（chart 預設沒有）；它驗證內部身分，所以和其他有 API 的服務一樣需要身分服務啟用。
- 上表的 Postgres 設定由 chart 給：`gateway_ops_app` 與 `gateway_ops_platform` 的密碼由 migration Job 在叢集內產生，放在 Secret `<release>-gateway-ops-password`，只有這個服務的 pod 讀得到；`TRUSTIOT__DATABASE__MAX_CONNECTIONS` 是 `services.gateway-ops.database.maxConnections`（chart 預設 `9`，即下限，更大的連線池只讓更多 API 請求同時進行），連同平台角色的 4 條算進 chart 的連線預算（`tests/chart_connections.rs` 比對程式與預算）。
- broker 帳號 `service:gateway-ops`：chart 給 `TRUSTIOT__MQTT__HOST`、`_PORT`（1883）、`_USERNAME`、`_PASSWORD_FILE`（`/etc/trustiot/mqtt-broker-account/gateway-ops`，只掛這個服務自己的 key，0440，pod 的 `fsGroup` 預設 65530），密碼由 broker 的 init container 在叢集內產生；輪替時服務不重啟，下一次連線重讀密碼檔。服務讀這些設定的程式在 Laysi/trustiot-v2#720。
- 這些變數都保留給 chart，自訂 `env` 不能設定。
- nightly 的 smoke 以兩份 values 連同 broker 部署它，確認它就緒（readiness 包括以 `service:gateway-ops` 連上 broker、套用回報的共享訂閱以 QoS 1 獲准，所以這也證明 chart 交給它的 broker 帳號能用）、兩個角色都有連線。

## 測試

需要 podman 或 docker：測試起一個有 TLS 的 Postgres，以 migration Job 套用整個產品的 schema（`testkit::product`）；下發的測試另起一個 Mosquitto（`tests/support/broker.rs`：和 chart 相同的連線設定、沒有 persistence，帳號與 ACL 取自 migration Job 的 `migrate::mqtt_accounts`，再加一個讀所有 `down/config`、能發布 `up/config-report`、`up/status` 與 `up/heartbeat` 的替身閘道器帳號；在線狀態的測試以 `RawGateway` 直接送 MQTT 封包，自己決定 keepalive、last will 與何時斷線）。沒有容器執行環境時本機跳過，設了 `CI` 時失敗。

```sh
cargo test -p gateway-ops
```

| 檔案 | 驗證 |
| --- | --- |
| `src/generate.rs`、`src/config.rs` 的單元測試 | 固定順序、每個型號一次、編碼後閘道器解得開；缺型號、型號名稱不符、違反規則、超過 `MAX_DEVICES` 的設定被拒，日誌摘要不帶連線設定；刪除時「上一版去掉這個設備」的內容（含 `MAX_DEVICES` 的情況）；設定的驗證（含 broker 與節流的範圍）與 `Debug` 不印密碼 |
| `src/delivery/schedule.rs`、`src/delivery/reports.rs` 的單元測試 | 手動時鐘下十次快速變更合併成一次、送最新版本、晚到的舊通知不降低版本、重發馬上到期且強制送、大量回報每個時間窗一次、忙碌的閘道器等待；回報的 topic 決定閘道器、retain／QoS 0／解不開／未知 outcome 被拒、版本超前與未知版本與先前綁定被拒、舊版本的回報觸發重發、日誌額度 |
| `tests/delivery.rs` | 真的 broker 與 Postgres：十次快速變更只發布一次最新版本（等滿節流）、新訂閱者收到 retained 的最新版本、`config_deliveries` 只記發布的那一版；broker 重啟後 readiness 失敗再恢復、重發所有閘道器、新訂閱者收到 retained 的最新版本；別的 replica 持有重發的鎖時不重發；別的 session 持有某台閘道器的發布鎖時不發布它、釋放後才發布；套用回報（rejected 帶 issue、applied）記在版本的租戶，payload 指向別台閘道器時以 topic 為準，同一則回報 60 秒內再送不再寫入也不記日誌，版本超前、`active_version` 超前、沒有版本的閘道器與解不開的回報被拒且不改任何列，舊版本的回報讓最新版本再發布一次；日誌不帶 broker 密碼；一台閘道器連送 500 則不合理的回報時，另一台的回報照樣記錄，每則洪水回報不是被拒就是被取代，沒有一則遺失 |
| `tests/desired_config.rs` | 新增、修改設備各產生一版，payload 以 crates/protocol 解碼並驗證；同內容、晚到的舊 revision 與整段重播（`Replay::Reprocess`）不產生版本；刪除的設備從下一版消失；別的租戶、先前綁定、綁定不明的設備不在任何設定裡，沒有 `binding_id` 的事件取事件之前的綁定；改綁到別的租戶時版本照樣遞增、不帶舊租戶的設備，新租戶的設定產生不出來時存一版沒有設備的設定；設定產生不出來時刪除仍然生效；解綁存一版沒有設備的設定、只存一次；讀取介面回最新版本、拒絕不屬於最新綁定的版本，平台角色不能寫入，另一個租戶的服務角色讀不到 |
| `tests/generation_errors.rs` | 未知型號、違反規則、型號被刪除時不產生版本、ERROR 帶閘道器與原因、上一版不變；型號送到後產生下一版；別的租戶的設備事件記 WARN |
| `tests/unknown_events.rs` | 這個版本不認得的事件：不在追蹤的 aggregate 時確認並往下走；在型號、設備或綁定的 aggregate 時每次 poll 都重試，不確認、不進 dead letter，後面的事件等著 |
| `src/presence/rules.rs`、`src/presence/store.rs`、`src/presence/backlog.rs`、`src/presence/mod.rs`、`src/gateways.rs` 的單元測試 | 在線狀態的每條規則，包括心跳的柵欄與兩個 replica 的所有交錯順序都停在單一 replica 的結果；等待佇列輪流取、每台閘道器保持順序、上限與最舊一則的時間；API 的同時請求數；宣告的心跳間隔的範圍；topic、retain、解不開的 payload 與未知原因被拒，空的狀態不理；逾時的時鐘在暫停與重新連線後重新開始 |
| `tests/presence.rs` | 真的 broker 與 Postgres：斷線後 last will 把閘道器標成 `connection_lost`（印出量到的時間，必須小於 1.5 倍的 keepalive），半開的連線在 broker 的 keepalive 檢查後同樣離線；舊 session 晚到的 last will 不理；未綁定的閘道器不記錄；壞掉的狀態被拒；心跳停了之後在「兩個間隔＋寬限」之後才標成 `heartbeat_timeout`，再來的心跳讓它回到在線；API 在第一則狀態前列 `unknown`，之後列出在線狀態、最後回報時間、已套用與最新的設定版本，另一個租戶的列表是空的；訂閱前就在 broker 上的 retained 狀態不建立列，下一則即時心跳才建立；一台閘道器連送 2000 則心跳時，另一台斷線的 last will 不必等它們處理完；寫入前綁定已經結束時什麼都不寫 |
| `tests/permissions.rs` | 權限矩陣（A2）：API 的宣告和 OpenAPI 文件、矩陣文件一致；真的 binary 對每一種呼叫者只放行宣告的那些，探測不寫入任何列 |
| `tests/cross_tenant.rs` | 服務角色對六張表的跨租戶負向測試（決策 29，crates/testkit），每個碰得到的關聯都有宣告，以及 API 的列表只列呼叫者租戶的閘道器 |
| `tests/startup.rs` | 真的 binary 以 chart 給的角色與 broker 帳號啟動、ready，訂閱把綁定與設備事件變成版本，消費端的通知讓最新版本以 retained 送出；租戶的 viewer 從 API 看到閘道器（還沒有狀態）與最新版本，沒有 token 是 401；broker 停掉時 `/readyz` 不是 200（失敗的檢查包括 `mqtt` 與 `presence`），broker 重啟後恢復並重發；SIGTERM 以 0 結束；stdout、stderr 都不含 broker 密碼 |
| `tests/chart_connections.rs` | chart 的連線預算（`trustiot.postgres.connectionBudgets`）與程式開的連線池一致：平台角色的 `PLATFORM_CONNECTIONS`、服務角色的 `database.max_connections` 範圍；多一個連線池時失敗 |
