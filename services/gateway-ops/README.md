# gateway-ops（閘道器營運）

每台閘道器的 desired 設定，以版本存在 Postgres（決策 7、清單 E1）。服務在 Laysi/trustiot-v2#93 底下分段實作：2026-10-29 起有服務骨架、Postgres schema、資產事件的投影與帶版本的 desired 設定（Laysi/trustiot-v2#719，本文件）；以 MQTT retained 訊息下發設定、閘道器的套用回報是 Laysi/trustiot-v2#720，映像與 chart 的部署是 Laysi/trustiot-v2#721。閘道器端的套用是 Laysi/trustiot-v2#94，上線狀態是 Laysi/trustiot-v2#112。

設定的格式與規則在 [crates/protocol](../../crates/protocol/README.md) 的「設定下發」（`DesiredConfig`）：這個服務產生的每一版都先以同一套驗證（閘道器套用前跑的那一套）檢查過，再以 crates/protocol 的編碼存下。

## 服務與設定

`cargo run -p gateway-ops`（`src/main.rs`）以 `crates/service-kit` 啟動：設定、JSON 日誌、`/healthz`、`/readyz`、指標與優雅關閉（SIGTERM）都照 service-kit。服務還沒有 HTTP API，只有健康檢查的端點（`tools/openapi` 的 `SERVICES_WITHOUT_API`）。啟動時建立兩個 Postgres 連線池（都是 lazy 連線，連線前做 service-kit 的 session 檢查），並啟動資產服務事件的消費端（訂閱名稱 `gateway-ops.assets-events`，不可改名：新名稱會從頭重讀整個事件紀錄）。

readiness 在下列全部成立時才是 200：

- `postgres`：服務角色與平台角色都能開出通過檢查的 session；
- migration 檢查：資料庫有這個版本預期的每個 Postgres migration 檔（`src/schema.rs`）。

| 設定 | 環境變數 | 說明 |
| --- | --- | --- |
| Postgres 位置與 TLS | `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`（只接受 `verify-full`）、`_CA_FILE` | chart 設定的平台設定 |
| 服務角色 | `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | `gateway_ops_app`；密碼來自 migration Job 產生的 Secret |
| 平台角色 | `TRUSTIOT__DATABASE__PLATFORM_USERNAME`、`TRUSTIOT__DATABASE__PLATFORM_PASSWORD` | `gateway_ops_platform`，必須和服務角色不同 |
| 連線數 | `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 服務角色每個 replica 的連線上限，2 到 100，預設 10；平台角色另外固定 4 條 |

本機以 `bun scripts/dev.ts env` 印出的 `TRUSTIOT_DEV_GATEWAY_OPS_DATABASE_*` 與 `TRUSTIOT_DEV_GATEWAY_OPS_PLATFORM_DATABASE_*` 設定兩個角色（[scripts/README.md](../../scripts/README.md)）。

## Postgres schema 與角色

schema `gateway_ops` 由 migration Job（`tools/migrate`）建立並套用 `migrations/postgres/`；registry 把 gateway-ops 列為資產服務事件的消費端，所以 Job 也建立 outbox 的消費端表（`outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version`），並讓 `gateway_ops_platform` 讀得到 `assets.outbox_event`。

每張表都是租戶資料，照 `crates/service-kit/sql/tenant-table.sql`：`tenant_id` 是主鍵的第一欄，`tenant_isolation` policy 把每個角色限制在交易的租戶裡。

| 表 | 內容 | `gateway_ops_app` | `gateway_ops_platform` |
| --- | --- | --- | --- |
| `gateway_bindings` | `assets.gateway_bound` 的投影：每個綁定一列，記綁定事件在資產服務事件紀錄裡的位置；`unbound_at` 在綁定結束時填上 | `SELECT`、`INSERT`、只有 `unbound_at` 的 `UPDATE` | `SELECT`（`platform_access`，所有租戶） |
| `device_models` | `assets.device_model_*` 的投影：名稱、revision、`protocol::config::DeviceModel` 的 JSON；刪除的型號留下 tombstone（`deleted`，沒有 `model`） | `SELECT`、`INSERT`、`UPDATE` | 無 |
| `devices` | `assets.device_*` 的投影：閘道器的硬體 ID、型號、所屬綁定（事件的 `binding_id`）、revision、`protocol::config::Device` 的 JSON；刪除的設備同樣留 tombstone | `SELECT`、`INSERT`、`UPDATE` | 無 |
| `desired_configs` | 每台閘道器每一版的設定：版本、所屬綁定、`bound`、編碼後的 `DesiredConfig`（`payload`）、設備數、產生它的事件；只新增，服務不改、不刪 | `SELECT`、`INSERT` | `SELECT`（`platform_access`，所有租戶） |

兩個刻意的例外是「`tenant_id` 不在索引第一欄」：`gateway_bindings_by_gateway`（閘道器目前的綁定是跨租戶的問題）與唯一索引 `desired_configs_gateway_version (hardware_id, version)`：一台閘道器的每個版本號只用一次，不管當時是哪個租戶。

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

## 指標

meter `gateway-ops`：

| 指標 | 屬性 | 意義 |
| --- | --- | --- |
| `gateway_ops.config.versions` | 無 | 存下的新版本數（包括替代的版本） |
| `gateway_ops.config.failures` | `reason`（上表） | 產生不出設定的次數；大於 0 就有閘道器沒拿到完整的設定 |
| `gateway_ops.config.ignored_device_events` | `reason`：`unbound`、`other_tenant`、`ended_binding`、`unknown_binding` | 沒有產生設定的設備事件 |

## 解綁

解綁事件本身是 Laysi/trustiot-v2#115；這裡先提供 `AssetsEventsHandler::apply_unbinding`，由它的事件處理在同一個交易裡呼叫：

- 綁定填上 `unbound_at`；它是這台閘道器目前的綁定時，在原租戶存一版**沒有設備**、`bound = false` 的設定（版本照樣遞增）。重複呼叫只存一次；這個服務沒記錄過的綁定什麼都不做（WARN）。
- 之後這個綁定的設備事件都不產生設定（`reason=unbound`）。
- 下發（Laysi/trustiot-v2#720）的規則：`bound = false` 的設定仍要送到閘道器，讓它停止輪詢、清掉舊設備（清單 D4「清理舊設定」）；閘道器確認套用之後，下發端可以清掉這台閘道器的 retained 訊息。閘道器改綁到下一個租戶時，新租戶的第一版設定版本號比這版大，閘道器照常套用。
- 歷史版本不刪：它們是審計紀錄，在原租戶的租戶隔離底下。

## 讀取介面（給 Laysi/trustiot-v2#720）

`gateway_ops::desired::DesiredConfigs`，以平台角色的 `READ ONLY` 交易讀：

- `current(&GatewayId)`：這台閘道器最新的設定（不管租戶），`StoredConfig` 帶租戶、綁定、版本、`bound`、設備數、建立時間與 `payload`（原樣發布）；沒有任何版本時是 `None`。
- `latest_versions()`：每台閘道器最新的版本號，給啟動後重新發布用。

兩者讀到 `payload` 與 `version` 不一致、或欄位不合規則的列時回 `ReadError::Corrupt`。

兩者都不交出不屬於閘道器最新綁定的設定：最新的一版必須是這台閘道器最新綁定（綁定事件在紀錄裡最後的那一個）的租戶與綁定，而且 `bound` 和綁定是否還有效一致。投影在讓一個綁定成為目前綁定的同一個交易裡就存它的一版，所以不一致代表規則被打破；這時 `current` 回 `ReadError::NotForLatestBinding`，`latest_versions` 略過那台閘道器並記 ERROR，下發端不會把一個租戶的設備送給綁在別的租戶的閘道器。

## 部署

映像是 `deploy/images/gateway-ops`（`image.toml`，arm64，以 `deploy/images/rust-binary.Containerfile` 打包），release.yml 發佈、chart 以 digest 固定（[deploy/images/README.md](../../deploy/images/README.md)）。chart 的 `services.gateway-ops`（Laysi/trustiot-v2#721，細節在 [chart README](../../deploy/chart/README.md) 的「gateway-ops」）：

- 預設 `enabled: false`；開啟時 `mqttBroker.enabled` 也必須是 `true`，否則渲染失敗：設定只經叢集內的 broker 下發。
- `mesh.calls.gateway-ops` 預設 `[mqtt-broker, postgres]`：mesh 只放行它連 broker 的服務埠 1883；沒有任何服務能呼叫它（它沒有 API）。
- 上表的 Postgres 設定由 chart 給：`gateway_ops_app` 與 `gateway_ops_platform` 的密碼由 migration Job 在叢集內產生，放在 Secret `<release>-gateway-ops-password`，只有這個服務的 pod 讀得到；`TRUSTIOT__DATABASE__MAX_CONNECTIONS` 是 `services.gateway-ops.database.maxConnections`（chart 預設 `4`），連同平台角色的 4 條算進 chart 的連線預算（`tests/chart_connections.rs` 比對程式與預算）。
- broker 帳號 `service:gateway-ops`：chart 給 `TRUSTIOT__MQTT__HOST`、`_PORT`（1883）、`_USERNAME`、`_PASSWORD_FILE`（`/etc/trustiot/mqtt-broker-account/gateway-ops`，只掛這個服務自己的 key，0440，pod 的 `fsGroup` 預設 65530），密碼由 broker 的 init container 在叢集內產生；輪替時服務不重啟，下一次連線重讀密碼檔。服務讀這些設定的程式在 Laysi/trustiot-v2#720。
- 這些變數都保留給 chart，自訂 `env` 不能設定。
- nightly 的 smoke 以兩份 values 連同 broker 部署它，確認它就緒、兩個角色都有連線、broker 的日誌記錄了 `service:gateway-ops` 的登入。

## 測試

需要 podman 或 docker：測試起一個有 TLS 的 Postgres，以 migration Job 套用整個產品的 schema（`testkit::product`）。沒有容器執行環境時本機跳過，設了 `CI` 時失敗。

```sh
cargo test -p gateway-ops
```

| 檔案 | 驗證 |
| --- | --- |
| `src/generate.rs`、`src/config.rs` 的單元測試 | 固定順序、每個型號一次、編碼後閘道器解得開；缺型號、型號名稱不符、違反規則、超過 `MAX_DEVICES` 的設定被拒，日誌摘要不帶連線設定；刪除時「上一版去掉這個設備」的內容（含 `MAX_DEVICES` 的情況）；設定的驗證與 `Debug` 不印密碼 |
| `tests/desired_config.rs` | 新增、修改設備各產生一版，payload 以 crates/protocol 解碼並驗證；同內容、晚到的舊 revision 與整段重播（`Replay::Reprocess`）不產生版本；刪除的設備從下一版消失；別的租戶、先前綁定、綁定不明的設備不在任何設定裡，沒有 `binding_id` 的事件取事件之前的綁定；改綁到別的租戶時版本照樣遞增、不帶舊租戶的設備，新租戶的設定產生不出來時存一版沒有設備的設定；設定產生不出來時刪除仍然生效；解綁存一版沒有設備的設定、只存一次；讀取介面回最新版本、拒絕不屬於最新綁定的版本，平台角色不能寫入，另一個租戶的服務角色讀不到 |
| `tests/generation_errors.rs` | 未知型號、違反規則、型號被刪除時不產生版本、ERROR 帶閘道器與原因、上一版不變；型號送到後產生下一版；別的租戶的設備事件記 WARN |
| `tests/unknown_events.rs` | 這個版本不認得的事件：不在追蹤的 aggregate 時確認並往下走；在型號、設備或綁定的 aggregate 時每次 poll 都重試，不確認、不進 dead letter，後面的事件等著 |
| `tests/cross_tenant.rs` | 服務角色對四張表的跨租戶負向測試（決策 29，crates/testkit），以及每個碰得到的關聯都有宣告 |
| `tests/startup.rs` | 真的 binary 以 chart 給的角色啟動、ready，訂閱把綁定與設備事件變成版本，SIGTERM 以 0 結束 |
| `tests/chart_connections.rs` | chart 的連線預算（`trustiot.postgres.connectionBudgets`）與程式開的連線池一致：平台角色的 `PLATFORM_CONNECTIONS`、服務角色的 `database.max_connections` 範圍；多一個連線池時失敗 |
