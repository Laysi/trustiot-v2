# ingest（資料接收）

閘道器的遙測資料經 MQTT 寫進 ClickHouse，整批落盤後才回確認（決策 8、清單 I1）。服務在 Laysi/trustiot-v2#91 底下分段實作：2026-10-24 起有服務骨架、Postgres schema 與閘道器綁定對照表（Laysi/trustiot-v2#640，見[服務與設定](#服務與設定)、[閘道器綁定對照表](#閘道器綁定對照表)），2026-10-29 起有 MQTT 的訂閱、寫入與確認（Laysi/trustiot-v2#641，見[寫入流程](#寫入流程)）；2026-10-29 起有映像（`deploy/images/ingest`）與 chart 的部署（Laysi/trustiot-v2#642，見[部署](#部署)）。這個 crate 也放 ClickHouse 的 schema、保存期限的值（`src/retention.rs`）、彙總表的對帳與電能查詢語句（`src/rollup.rs`）與它們的測試（Laysi/trustiot-v2#59、Laysi/trustiot-v2#92、Laysi/trustiot-v2#141）。

## 閘道器的租戶歸屬

規則在[架構決策](../../docs/architecture/decisions.md)的決策 8、12 與清單 I1（2026-10-24 決定，Laysi/trustiot-v2#638），這裡只摘要 ingest 要做的事：

- 閘道器取自 topic（`trustiot/v1/gw/{gateway}/up/telemetry`）：broker 的 ACL 讓閘道器只能發布到以自己憑證 CN（硬體 ID）為名的 topic（[crates/protocol](../../crates/protocol/README.md) 的「Topic」「ACL」）。ingest 在叢集內連 broker，看不到閘道器的憑證，憑證裡也沒有租戶。
- 租戶取自 ingest 自己的綁定對照表：消費資產服務的 `assets.gateway_bound`（之後加上 Laysi/trustiot-v2#115 的解綁事件），對照表是該硬體 ID 目前的綁定（Laysi/trustiot-v2#640）。不在資料路徑上同步呼叫資產服務；訊息內容裡的任何租戶或綁定資訊都不採信。
- 不認得的閘道器（還沒綁定，或對照表還沒收到事件）回 `retry`、不寫入。
- 串流以「topic 的閘道器 + 串流 ID」為鍵（遙測規則 3）；完成紀錄記下寫入當時的租戶與 `binding_id`，供追查用（Laysi/trustiot-v2#639）。
- 改綁時閘道器一定換新的 SD 卡，新卡使用新的串流 ID，所以 ingest 不另外拒收舊綁定下採集的批次；這個前提與它的限制見決策 12。

## 服務與設定

`cargo run -p ingest`（`src/main.rs`）以 `crates/service-kit` 啟動：設定、JSON 日誌、`/healthz`、`/readyz`、指標與優雅關閉都照 service-kit。服務沒有 HTTP API，只有健康檢查的端點（`tools/openapi` 的 `SERVICES_WITHOUT_API`）。啟動時建立兩個 Postgres 連線池（都是 lazy 連線，連線前做 service-kit 的 session 檢查）與 ClickHouse 的用戶端，啟動 `assets.gateway_bound` 的消費端，並以 `crates/mqtt-client` 連 broker、啟動遙測的寫入流程（見[寫入流程](#寫入流程)）。關閉時先停止接新的訊息，等處理中的批次寫完並回確認，再中斷 broker 的連線。

readiness 在下列全部成立時才是 200：

- `postgres`：服務角色與平台角色都能開出通過檢查的 session；
- migration 檢查：資料庫有這個版本預期的每個 Postgres migration 檔（`src/schema.rs`）；
- `clickhouse`：ClickHouse 以服務帳號回答 `SELECT 1`。這個檢查涵蓋位址、TLS 與帳密，不涵蓋帳號的權限：寫入流程把缺少的權限當成那條語句的錯誤，回 `retry` 並記在指標與日誌。不用碰表的語句，readiness 就不依賴 ClickHouse 的表名。
- `mqtt`：連上 broker，而且遙測的共用訂閱以 QoS 1 獲准（`Subscriptions::all_granted`；broker 拒絕或只給 QoS 0 都不算）。斷線時立刻不是 200，重連並重新訂閱成功後恢復；用戶端自己重連，間隔最長 `RECONNECT_DELAY_MAX`（30 秒），每次連線最多 `CONNECT_TIMEOUT`（10 秒）。

每個檢查最多 800 毫秒（service-kit），失敗時日誌 WARN `readiness check failed` 帶檢查名稱與原因；原因只有連線錯誤的種類或 ClickHouse 的錯誤代碼，不含回應內文與密碼。

設定（`src/config.rs`，`TRUSTIOT__*` 段落都是 `deny_unknown_fields`；chart 給的變數見[部署](#部署)）：

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 平台設定：Postgres 位置；只接受 `verify-full`，只信任這個 CA。本機開發由 `bun scripts/dev.ts env` 印出 |
| `TRUSTIOT__DATABASE__USERNAME`、`_PASSWORD` | 服務角色 `ingest_app` 的帳密（本機開發是 `TRUSTIOT_DEV_INGEST_DATABASE_*`） |
| `TRUSTIOT__DATABASE__PLATFORM_USERNAME`、`_PLATFORM_PASSWORD` | 平台角色 `ingest_platform` 的帳密，必須是另一個角色（本機開發是 `TRUSTIOT_DEV_INGEST_PLATFORM_DATABASE_*`） |
| `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 服務角色連線池每個副本的上限，3–100，預設 10：一條給綁定的消費端、一條留給 readiness（`postgres` 的 ping 與 migration 檢查），其餘給寫入流程的串流鎖（見[寫入流程](#寫入流程)）。平台角色的連線池固定 4 條（`PLATFORM_CONNECTIONS`：消費端讀事件、readiness、寫入路徑的查詢）。chart 的連線預算（`deploy/chart/templates/_postgres.tpl`）以 14 條計算這個服務的每個副本，`tests/chart_connections.rs` 比對程式開的連線池 |
| `TRUSTIOT_CLICKHOUSE_URL` | 平台設定：ClickHouse 的 HTTP 介面，`http://host:port`（chart 內建的 ClickHouse，走 mesh 的 mTLS）或 `https://host:port`（外部），不能帶帳密、路徑、查詢字串 |
| `TRUSTIOT_CLICKHOUSE_CA_FILE` | 平台設定：`https` 時必填，是唯一信任的 CA，並檢查伺服器憑證的名稱（相當於 `verify-full`）；不信任系統或公開的根憑證 |
| `TRUSTIOT__CLICKHOUSE__USERNAME`、`_PASSWORD` | 服務的 ClickHouse 寫入帳號，持有角色 `trustiot_ingest`（見下方的角色表）；部署時是 migration Job 建立的 `trustiot_ingest_writer`，密碼由 Job 產生（見[部署](#部署)） |
| `TRUSTIOT__MQTT__HOST` | 必填：broker 的主機名稱或 IP，叢集內是 `<release>-mqtt-broker.<namespace>.svc`。走 broker 的服務埠（明文 MQTT，只有 mesh 連得到，由 ztunnel 的 mTLS 保護；見 [crates/mqtt-client](../../crates/mqtt-client/README.md)） |
| `TRUSTIOT__MQTT__PORT` | broker 的服務埠，預設 1883 |
| `TRUSTIOT__MQTT__USERNAME` | 必填：服務的 broker 帳號 `service:ingest`，它的角色由 migration Job 建立（`tools/migrate/src/mqtt_accounts.rs` 的 `SERVICES`）：訂閱閘道器的遙測與日誌（直接與 `$share/` 共用訂閱；日誌頻道還沒有程式使用）、只能發布遙測的確認 |
| `TRUSTIOT__MQTT__PASSWORD_FILE` | 必填：存放這個帳號密碼的檔案（chart 掛上 broker 帳號 Secret 裡 ingest 的那個 key）；每次連線都重讀，換密碼後下一次重連就生效。設定與日誌只出現路徑，不出現密碼 |
| `TRUSTIOT__TELEMETRY_LIMITS__BATCHES_PER_MINUTE` | 每台閘道器的批次桶每分鐘補充幾批，1–60000，預設 600（每秒 10 批）；超過的訊息回 `retry`、不處理（見[寫入流程](#寫入流程)）。每個副本各自計算，broker 把一台閘道器的訊息分給幾個副本，它的總額度就是幾個副本的和 |
| `TRUSTIOT__TELEMETRY_LIMITS__BURST` | 每台閘道器的桶最多存幾批，1–10000，預設 60：安靜一段時間後可以連續送的批數 |

ClickHouse 的用戶端（`src/clickhouse.rs`、`src/transport.rs`）複製自 telemetry-query：HTTP/1.1 走 hyper，`https` 以 rustls 與 ring、只信任設定的 CA；每個請求一條連線，單一請求最多 30 秒。三個複本（migration Job、telemetry-query、ingest）合成一個 crate 是之後的工作。

## 部署

映像是 `deploy/images/ingest`（arm64 的 Rust 執行檔，基底以 digest 固定，見 [deploy/images/README.md](../../deploy/images/README.md)）。chart 以 `services.ingest.enabled: true` 部署它，連同 `mqttBroker.enabled: true`：沒有 broker 時渲染失敗，因為服務沒有 broker 就沒有事可做。chart 給它的設定、帳號與檢查見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「ingest」，摘要：

- Postgres：`ingest_app`、`ingest_platform` 的密碼由 migration Job 產生在 Secret `<release>-ingest-password`；`database.maxConnections` 預設 10。
- ClickHouse：同一個 Secret 的 `trustiot_ingest_writer`，Job 建立這個只持有 `trustiot_ingest` 的帳號（見下方的角色表）。
- broker：帳號 `service:ingest`，密碼檔由 chart 從 broker 帳號 Secret 只掛 `ingest` 這個 key（`trustiot.mqttBroker.serviceCredentials`），pod 的 `fsGroup` 是 65531。換 broker 密碼時服務不重啟。
- 每台閘道器的限額（`TRUSTIOT__TELEMETRY_LIMITS__*`）chart 沒有欄位，需要時以 `services.ingest.env` 設定。
- nightly 的 smoke 測試確認 pod 就緒、兩個 Postgres 角色都有連線、寫入帳號只有它的預設角色；還不從閘道器送遙測。

## Postgres schema 與角色

schema `ingest`，DDL 在 `migrations/postgres/`，由 migration Job 套用（`tools/migrate/src/registry.rs` 的 `PRODUCT`：有平台角色、消費 `assets` 的事件）；檔案清單在 `src/schema.rs`，readiness 檢查同一份。消費端的表在 ingest 自己的 schema（決策 4）。寫入流程以 Postgres 的 advisory lock 讓一個串流同時只由一個副本處理（service-kit 的 `JobLock`，以服務角色的連線池取得；見[寫入流程](#寫入流程)），不另外建表。

| 角色 | 權限 |
| --- | --- |
| `ingest_owner` | migration 角色，擁有 schema 與表；服務不用它登入 |
| `ingest_app` | 服務角色：`gateway_bindings` 的 `SELECT`、`INSERT`（RLS 只開放交易的租戶），消費端的記錄表，`trustiot_migrate_applied` 的讀取 |
| `ingest_platform` | 平台角色：`assets.outbox_event` 的讀取（Job 依 registry 設定），`gateway_bindings` 的 `SELECT`（`platform_access` policy，只有 `FOR SELECT`）；沒有任何寫入權限 |

| 表 | 內容 | RLS |
| --- | --- | --- |
| `gateway_bindings` | 閘道器綁定對照表，見下一節 | `tenant_isolation`，加上給 `ingest_platform` 的 `platform_access`（`FOR SELECT`） |
| `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version` | 消費端的記錄（Job 建立） | 無：不是租戶資料 |
| `trustiot_migrate_applied` | Job 套用過的檔案，服務角色只讀 | 無：不是租戶資料 |

## 閘道器綁定對照表

`ingest.gateway_bindings`（`migrations/postgres/0001_ingest.sql`、`src/bindings.rs`）是遙測資料唯一的租戶來源（決策 8、12）。來源是資產服務的 `assets.gateway_bound`（`crates/contract/src/events/assets.rs`，Laysi/trustiot-v2#101）；訂閱名稱 `ingest.assets-events`，不改名（改名會從頭重讀事件）。新的訂閱從事件紀錄的開頭讀起（`crates/outbox` 的預設），所以 ingest 部署之前就有的綁定也會進對照表。

| 欄位 | 說明 |
| --- | --- |
| `tenant_id`、`binding_id` | 主鍵。租戶取自事件信封的 `tenant_id`（資產服務寫入時 RLS 檢查過的租戶，消費端也以它開交易），不取自 payload |
| `hardware_id` | 閘道器的硬體 ID，與 `crates/protocol` 的 `GatewayId` 同一個格式（CHECK） |
| `bound_at` | 事件的 `occurred_at`：資產服務綁定的時間。只供閱讀，不用來排序：它來自某一個資產服務副本的時鐘 |
| `position_tx`、`position_id` | 事件在 `assets.outbox_event` 的位置（`crates/outbox` 的 `Position`），綁定的先後以它為準 |
| `unbound_at` | 綁定結束的時間，留給 Laysi/trustiot-v2#115 的解綁事件；目前沒有程式寫入。不與 `bound_at` 比較（兩者可能來自不同副本的時鐘），所以時鐘偏差不會讓解綁事件被拒收、進 dead letter |
| `recorded_at` | ingest 記下這筆的時間（資料庫時鐘） |

一筆綁定一列，不是一台閘道器一列：綁定是租戶的資料，在事件的租戶交易裡寫入；閘道器解綁後綁到另一個租戶時（Laysi/trustiot-v2#115），新的綁定是另一個租戶的新一列，舊的留在原租戶。以硬體 ID 為鍵的單列表做不到這件事，因為新租戶的交易看不到也改不了舊租戶的列。

消費端（`GatewayBoundHandler`）：

- 只處理 `assets.gateway_bound`，資產服務的其他事件略過。
- `INSERT ... ON CONFLICT (tenant_id, binding_id) DO NOTHING`：同一個事件再送一次、整個訂閱重播（`Replay::Reprocess`），都只有一列，原本那列不被改寫。
- 已有同一個綁定、卻是另一台閘道器的事件（不可能是同一個事件的重送），以及沒有租戶、解不開（版本比這個 build 新的除外：那種照 `crates/outbox` 的規則重試到服務升級）或表拒收其值（SQLSTATE 22 類、CHECK）的事件，每次重試都會失敗，所以記進 dead letter，後面的事件照常處理。其他資料庫錯誤（斷線、切換）照 `crates/outbox` 的規則重試。
- 每次處理以 INFO 記錄 `applied assets.gateway_bound`，帶硬體 ID、`binding_id`、租戶、事件 ID 與是否新增。

查詢（`GatewayBindings::gateway_tenant(hardware_id) -> Option<GatewayTenant>`，`GatewayTenant` 是租戶與 `binding_id`）：取該硬體 ID 的事件在資產服務事件紀錄裡最後的那筆綁定（`position_tx DESC, position_id DESC`），那筆還沒結束才回答，否則是 `None`。不依 `bound_at` 或 UUIDv7 的 `binding_id` 排序：兩者都來自寫入事件的那個資產服務副本的時鐘（`EventEnvelope::new` 的 `Utc::now()`），一個時鐘偏差的副本就能讓較舊的綁定排在最新。事件紀錄的順序不受時鐘影響：閘道器的前一筆綁定結束之後才能綁定，新綁定在較晚的交易寫入，它的事件在紀錄裡一定比較後面。所以較早的綁定事件晚到（重播、dead letter 重送）只會在它自己的租戶多一列，不會蓋過較晚的綁定；舊綁定也不會在新綁定結束之後重新生效。certs 的同一份投影（Laysi/trustiot-v2#632）用同一條規則。`None` 與查詢失敗時，寫入路徑回 `retry`、不寫入（決策 8）。

### 跨租戶的查詢為什麼用平台角色（決策 29）

「硬體 ID → 租戶」本質上是跨租戶的查詢：topic 只有閘道器，還不知道租戶。issue 允許兩種做法，這裡比照資產服務自己的 `assets.gateway_bindings`，採用「租戶資料表 + 平台角色唯讀查詢」，不採用「不含租戶資料的表」：

- 綁定是租戶的資料，由租戶的事件寫入；放在沒有 RLS 的表，服務角色的每一筆租戶交易都讀得到每台閘道器屬於哪個租戶，等於把跨租戶的讀取權給了所有程式路徑。現在只有平台角色讀得到，而且只能讀。
- 平台角色的查詢經 `PlatformReader`：每筆都是 `READ ONLY` 交易，資料庫拒絕任何寫入；`ingest_platform` 本來就沒有這張表的 `INSERT`、`UPDATE`、`DELETE`。reader 建立時以 INFO 記一次（actor 是 ingest 服務、purpose `gateway-tenant`），每次查詢只記 DEBUG，因為寫入路徑每一批都會查。
- `platform_access` 只有 `FOR SELECT`、條件是 `true`（只用表自己的欄位），符合 service-kit 連線檢查對平台角色的要求。
- 依硬體 ID 查詢的索引 `gateway_bindings_by_gateway` 不以 `tenant_id` 開頭，這是刻意的例外：它回答的問題本身就跨租戶。
- certs 的同一份投影（Laysi/trustiot-v2#632，`certs.gateway_bindings`）也是租戶資料表加上平台角色的唯讀查詢，兩個服務都以事件在資產服務紀錄裡的位置決定目前的綁定；之後任一邊改變做法時，另一邊一起改。

### 測試

需要 podman 或 docker（沒有時略過，設了 `CI` 時改為失敗）：

- `tests/startup.rs`：真的 binary 以 `testkit::product` 的 Postgres（平台設定與 `bun scripts/dev.ts env` 給的變數相同）、`tests/common` 的 ClickHouse（套用全部 ClickHouse migration，建立持有 `trustiot_ingest` 的帳號）與 Mosquitto（`tests/support/broker.rs`，`service:ingest` 的角色取自 migration Job 自己的定義）啟動。三者都連得上時 `/readyz` 是 200；ClickHouse 連不上、ClickHouse 拒絕密碼、Postgres 連不上時都不是 200，日誌點名失敗的檢查，也不含密碼；broker 停下時不是 200、日誌點名 `mqtt`，broker 回來後恢復 200。最後經這個 binary 送一批：回 `committed`，同一批再送回 `duplicate`，兩次 ClickHouse 的列數相同。
- `tests/telemetry.rs`：寫入流程本身，見下一節的「測試」。
- `tests/gateway_bindings.rs`：以資產服務自己的 `GatewayBinder` 綁定閘道器，ingest 的表有該租戶與 `binding_id`，查詢回答它們；整個訂閱重播與同一個綁定的重複事件都只有一列；事件紀錄裡較晚的綁定成為目前的綁定，即使它的 `occurred_at` 較早（時鐘偏差的副本）；較早的綁定晚到才記錄時不蓋過它；無法記錄的事件進 dead letter、後面的事件照常處理；平台角色寫不進這張表。
- `tests/cross_tenant.rs`：`crates/testkit` 的跨租戶負向測試宣告 `ingest.gateway_bindings`，服務角色讀不到也改不了別的租戶的綁定。

```sh
cargo test -p ingest --test startup --test gateway_bindings --test cross_tenant -- --nocapture
```

## 寫入流程

`src/telemetry/`（Laysi/trustiot-v2#641）：閘道器的遙測批次經 broker 寫進 ClickHouse，整批與完成紀錄都落盤之後才在閘道器的 `down/telemetry-ack` 回確認（協定與確認的格式見 [crates/protocol](../../crates/protocol/README.md)）。寫入 ClickHouse 的約定在下方「[I1 重寫與對帳](#i1-重寫與對帳)」，這裡是服務怎麼照它做。

一則訊息的處理：

1. **訂閱**：每個副本以共用訂閱 `$share/ingest/trustiot/v1/gw/+/up/telemetry`（QoS 1）訂閱，broker 把每一則交給其中一個副本。閘道器取自 topic（broker 的 ACL 讓閘道器只能發布到自己的 topic），不取自 payload。
2. **每台閘道器的限額**（`src/telemetry/limits.rs`）：在做任何事之前，訊息先扣它的閘道器的批次桶（`TRUSTIOT__TELEMETRY_LIMITS__*`），而且一台閘道器同時處理中的批次不超過寫入流程同時處理數的一半（`gateway_in_progress`）。超過任一項的訊息只讀標頭，回 `retry`（`after_ms` 10 秒，`THROTTLED_RETRY_AFTER_MS`），不查綁定、不拿鎖、不下任何 ClickHouse 語句，也不佔寫入流程的名額；同時送出的這種確認最多 16 則（`THROTTLED_REPLIES`），再多就不回確認、只記指標（閘道器等不到確認會自己重送）。所以一台被入侵的閘道器以重播已確認的批次或垃圾 payload 洗版，拿不走其他閘道器的處理名額、資料庫連線與 ClickHouse。每台閘道器的日誌也有額度（`LOG_BURST` 5 行，之後每 10 秒 1 行），用完時寫入流程關於這台閘道器的逐則日誌就不記，下一行記下中間略過幾行（`suppressed_logs`）；指標照樣逐則計數。狀態在副本的記憶體裡，以 topic 的閘道器為鍵，閒置的閘道器定期清掉。
3. **丟棄、不回確認**：retain 旗標為 1 的訊息（遙測從不 retain；broker 在每次新訂閱時重播 retained 訊息，所以這是閘道器違反協定的舊訊息）、以 QoS 0 交付的訊息（閘道器會重送），以及連批次標頭都讀不出來的 payload，都只記錄日誌與指標。
4. **`rejected`**：payload 解不開時以 `peek_header` 讀出的串流與序號回 `rejected`（`decode_failed`）。協定接受、但這張表存不了的批次也整批 `rejected`：設備 ID 不是 UUID（`invalid_value`，資產服務建立的設備都是 UUID）或讀值時間超過 `DateTime64` 的範圍（`out_of_range`）。回 `retry` 只會讓這個串流永遠卡住。
5. **租戶**：每次嘗試在訊息到達時查一次綁定對照表，整次嘗試都用這個答案；批次內容裡的租戶或綁定一律不看（protocol 的解碼本來就忽略未定義的欄位）。沒有綁定時回 `retry`（`after_ms` 30 秒）、不寫入；查詢失敗回 `retry`（5 秒）。
6. **一個串流一個副本**：以 Postgres 的 advisory lock（service-kit 的 `JobLock`，key 由 `ingest` 與 `telemetry/<閘道器>/<串流>` 算出）。拿不到鎖的副本回 `retry`（2 秒）；寫入期間鎖所在的 session 斷了也回 `retry`、不寫完成紀錄。閘道器一個串流一次只有一批在途（遙測規則 2），所以正常時不會發生，只有閘道器重送與副本切換時兩個副本才會同時拿到同一個串流。
7. **在鎖內寫入**：
   1. 從完成表讀這個串流的高水位，序號不高於它就回 `duplicate`，`records` 是那批完成紀錄的 `row_count`；
   2. 找前一次嘗試留下的列（`LEFTOVER_TENANTS`：條件同下面的 DELETE），有的話以同步的 `DELETE FROM` 刪掉，而且不管它們是寫給哪個租戶的；寫給目前租戶以外的租戶的（兩次嘗試之間閘道器改綁），立刻對帳那個租戶的彙總，並記 WARN；
   3. 以 `REWRITE_SETTINGS` 寫入整批；
   4. 對帳目前租戶的 1 分鐘與 1 小時彙總（`RECONCILE_1M`、`RECONCILE_1H`；只寫一次就成功的批次什麼都不寫）；
   5. 數這批在表裡的列（`BATCH_ROWS`，條件同下面的 DELETE，不分租戶），不等於這批的筆數就回 `retry`、不寫完成紀錄（`step` 是 `row_count`）。這縮小、但沒有關閉一個窗口：副本寫入期間鎖的 session 斷了、另一個副本接手刪掉並重寫這批時，原來的副本可能已經過了 JobLock 的檢查、正要寫完成紀錄；要關閉它得讓寫入本身帶鎖的 fencing，目前沒有；
   6. 寫完成紀錄：租戶與 `binding_id` 是第 5 步查到的，`committed_at` 是寫完列之後服務的時間、但不早於 `received_at`（同一個時鐘在訊息到達時記下），所以完成紀錄不會比它描述的列早過期；
   7. 回 `committed`，`records` 是這批的筆數。

   每一句都帶這個串流的 `stream_settings`（query ID `trustiot-rewrite/<閘道器>/<串流>`），ClickHouse 用戶端的請求逾時是 `REQUEST_TIMEOUT`（30 秒，`src/clickhouse.rs`），不短於 `CLIENT_TIMEOUT_SECONDS`。任何一步失敗都回 `retry`（5 秒）、不寫完成紀錄，閘道器重送後下一次嘗試從頭來。
8. **確認**：以 QoS 1 發布到 `trustiot/v1/gw/<閘道器>/down/telemetry-ack`，不 retain；發布失敗只記錄，閘道器沒收到確認就會重送。

DELETE 的條件（`DELETE_ROWS`）不含租戶：設備與資料點在這批的 `(device_id, point)` 之中、讀值時間在這批的第一筆與最後一筆之間（排序鍵 `(tenant_id, device_id, point, ts)` 的後三欄），閘道器、串流、序號相同，`received_at` 在對帳窗口（`RECONCILE_WINDOW_DAYS`，31 個收到日）之內（分區鍵 `toYYYYMM(received_at)`）。所以它只讀這批碰到的 partition 與 granule，不掃整張表；測試量過找殘留列、數列與 DELETE 三句讀的列數（見下面「測試」）。

點名這批 `(device_id, point)` 的語句（找殘留列、DELETE、數列與兩句對帳）把清單放在 URL 的查詢參數 `param_series`。服務的 HTTP 用戶端（`http` crate）不接受 64 KiB 以上的 URI，請求根本送不出去（ClickHouse 那端也只收 128 KiB 以內的單一欄位，`http_max_field_value_size`）；一批滿 256 KiB、資料點名稱 64 字元的批次有約 1,800 個 series，編碼後約 240 KB（2026-10-29 以 26.8 實測，安全審查發現），只放一句的話這批永遠回 `retry`、高水位不前進，閘道器的遙測卡住直到緩衝丟資料。所以清單依編碼後的長度切成每段不超過 32 KiB（`MAX_SERIES_PARAM_BYTES`，`ReconcileParams::split`），每段各跑一次這些語句；條件都以 series 為單位，分段跑的結果與一次跑相同。大部分的批次只有一段，滿的批次約 8 段。

剩下的一個窗口：閘道器在同一批的兩次嘗試之間改綁，而第二次嘗試剛好在刪掉舊租戶的列之後、對帳舊租戶之前結束，舊租戶的彙總就還算著刪掉的列，下一次嘗試也找不到列來點名那個租戶。[docs/runbooks/migrations.md](../../docs/runbooks/migrations.md) 的一致性檢查與修正（`src/rollup.rs` 的 `REPAIR_1M`、`REPAIR_1H`）找得到並修正它。

同時處理的批次：每個副本最多 `TRUSTIOT__DATABASE__MAX_CONNECTIONS` − 2 批（每批在鎖的期間佔一條服務角色的連線，一條留給綁定的消費端，一條留給 readiness，所以寫入流程滿載時 `/readyz` 照樣拿得到連線），超過時暫停從 broker 收訊息；一台閘道器最多佔一半（見上面第 2 步）。

指標（經 service-kit 的 OpenTelemetry 匯出）：

| 指標 | 說明 |
| --- | --- |
| `ingest.telemetry.batches` | 遙測頻道上的訊息數，`outcome` 是 `committed`、`duplicate`、`rejected`、`retry`、`retain_dropped`、`qos0_dropped`、`undecodable`、`throttled`（超過閘道器的限額） |
| `ingest.telemetry.records` | 寫進 ClickHouse 並回 `committed` 的讀值筆數 |
| `ingest.telemetry.duration` | 從訊息到達到送出確認的秒數，依 `outcome` |
| `ingest.telemetry.write_failures` | 失敗的語句，`store`（`clickhouse`、`postgres`）與 `step`（例如 `insert`、`row_count`、`commit`、`gateway_tenant`、`stream_lock`）；每一筆都回了 `retry` |

處理失敗與積壓的告警在 Laysi/trustiot-v2#183。日誌只記閘道器、串流、序號、筆數、步驟與錯誤的種類，不記 payload 的內容與任何密碼。

### 測試

`tests/telemetry.rs` 在同一個 process 裡啟動寫入流程（`telemetry::Pipeline`，與 binary 相同的程式），連拋棄式的 Postgres（`testkit::product`）、ClickHouse（`tests/common`）與 Mosquitto（`tests/support/broker.rs`），ClickHouse 先放 20 萬列別的串流的資料。測試掛鉤（`telemetry::hooks`）讓測試停在寫入的某一步、決定在哪裡結束，並固定 `received_at`。一個測試依序涵蓋：

1. 一批回 `committed`，同一批再送回 `duplicate`、列數不變；批次內容裡加上的租戶欄位被忽略，列與完成紀錄的租戶是綁定表的。
2. 沒有綁定的閘道器回 `retry`、一列都沒寫；設備 ID 不是 UUID、payload 的內文壞掉都以讀出的標頭回 `rejected`、一列都沒寫，連標頭都沒有的只記日誌。
3. 收回 ingest 帳號在 `stream_commits` 的 `INSERT`（ClickHouse 失敗）：回 `retry`、沒有完成紀錄；授權回來後重送回 `committed`，列只有一份。兩次嘗試之間改綁：列與完成紀錄只在新租戶下。
4. 直接（非共用）訂閱時 broker 重播的 retained 訊息被丟棄並記錄，列不變。
5. 在「列已寫入、完成紀錄未寫」時結束：寫入流程停在 `RowsWritten` 時被中止，鎖釋放後以新的寫入流程重送那一批（同樣的 `received_at`，所以重寫的區塊與刪掉的完全相同，開著去重時會被丟掉）：停在刪除之後與完成紀錄之前時都沒有任何確認，回 `committed` 時完成紀錄已經在了，列完整、沒有重複。
6. 兩個副本各在自己的共用群組，同一個串流的每一批兩邊都收到：一個停在拿到鎖之後時，重送的那一批在兩邊都回 `retry`（持有鎖的副本的第二個處理與另一個副本）；之後每一批只有一個 `committed`，列沒有重複。
7. 滿的批次：以 agent 的 `BatchBuilder` 在 256 KiB 裡放進最多的 series（每台設備 4 個資料點、名稱都是 64 字元，約 1,800 個 series），一次就回 `committed`；收回完成表的 `INSERT` 再送下一批，回 `retry` 後授權回來重送，找殘留列、DELETE 與對帳都分段點名每個 series，回 `committed`、列只有一份。修正前第一批就以 `leftover_rows` 失敗、回 `retry`。
8. 一台閘道器以已確認的批次洗版（桶設為 5 批、每分鐘 1 批，送 200 則）：桶以外的都回 `retry`（10 秒），ClickHouse 的 `query_log` 裡這個串流讀高水位的次數不超過桶允許的批數（被限的訊息沒有下任何語句），關於它的日誌行數不超過日誌額度；洗版之後送出的另一台閘道器的批次回 `committed`。
9. 兩張彙總表與原始列一致（`RAW_VS_1M`、`RAW_VS_1H` 沒有差異）；每批的 `committed_at` 不早於它的 `received_at`；`system.query_log` 裡寫入流程的每一句都帶串流的 query ID、`replace_running_query = 1`、`max_execution_time = 20`、`async_insert = 0`，找殘留列、數列與 DELETE 讀的列數都遠少於整張表；`telemetry.tenant_id` 的欄位說明已更正；日誌不含密碼。

```sh
cargo test -p ingest --test telemetry -- --nocapture
```

斷電重設後已確認的資料不掉、寫到一半斷電再重送不重複的實測在 Laysi/trustiot-v2#643；吞吐量與積壓的量測還沒做。

## ClickHouse schema

DDL 是 `migrations/clickhouse/` 的 SQL 檔，部署時由 migration Job（Laysi/trustiot-v2#60）以 bootstrap 管理帳號依檔名順序套用。執行期程式碼不建表、不改表（決策 14、[Laysi/trustiot#103](https://github.com/Laysi/trustiot/issues/103)）。真正的防線是資料庫權限：服務帳號只有讀寫與刪列的權限，改 schema 的語句一律被拒。`tools/repo-check` 另外掃描 `services/*/src`，出現 `CREATE`／`ALTER`／`DROP`／`RENAME`／`TRUNCATE`／`ATTACH`／`DETACH`／`EXCHANGE` 加 `TABLE`／`DATABASE`／`VIEW`／`DICTIONARY` 的陳述式就失敗。這個掃描只是文字比對的輔助檢查，拆開字串或在執行期組出來的語句它看不到（2026-10-11 驗證報告的發現 7），不能取代資料庫權限。刪除一批不完整的資料用 `DELETE FROM`（lightweight delete），不用 `ALTER TABLE ... DELETE`。

| 檔案 | 內容 |
| --- | --- |
| `0001_databases.sql` | `trustiot`（遙測與批次完成表）、`trustiot_logs`（服務與閘道器日誌，給 OTel collector，Laysi/trustiot-v2#73；獨立資料庫，可以分開決定保存與搬遷） |
| `0002_telemetry.sql` | 遙測窄表 `trustiot.telemetry` |
| `0003_batch_commits.sql` | 舊的批次完成表 `trustiot.batch_commits`，`0012` 起停用（見下文） |
| `0004_access.sql` | 角色、row policy、settings profile、quota |
| `0005_tenant_view.sql` | 租戶唯一能讀的 view `trustiot.tenant_telemetry`、它的 DEFINER 帳號與 row policy（Laysi/trustiot-v2#260） |
| `0006_logs.sql` | OTel collector 寫入的日誌表 `trustiot_logs.otel_logs`、`otel_traces`，與 collector 的角色 `trustiot_otel_collector`（Laysi/trustiot-v2#73） |
| `0007_monitoring.sql` | 監控角色 `trustiot_monitoring`：只有 `system.disks` 的 `SELECT`，給磁碟水位告警（Laysi/trustiot-v2#92） |
| `0008_telemetry_query.sql` | telemetry-query 的服務帳號 `trustiot_telemetry_query`：只能建立與修改名稱以 `trustiot_tenant_account_` 開頭的帳號，並把租戶角色 `trustiot_tenant_reader` 給它們（Laysi/trustiot-v2#119；[services/telemetry-query](../telemetry-query/README.md#租戶帳號)） |
| `0009_telemetry_query_grantees.sql` | 服務帳號 `GRANTEES NONE`：它以 `ADMIN OPTION` 持有租戶角色，只能在 `CREATE USER ... DEFAULT ROLE` 時把角色給新建的帳號，不能把角色授與或收回任何既有的使用者或角色（Laysi/trustiot-v2#119 的安全審查） |
| `0010_telemetry_rollups.sql` | 1 分鐘與 1 小時彙總表 `trustiot.telemetry_1m`、`telemetry_1h`，填入它們的 materialized view 與這些 view 的 DEFINER 帳號 `trustiot_rollup_definer`，租戶讀的 view `tenant_telemetry_1m`、`tenant_telemetry_1h`，以及補進既有資料與檢查一致性用的 backfill view（補資料由 migration Job 在套用之後執行；Laysi/trustiot-v2#141；見下方「彙總表」） |
| `0011_queryable_period.sql` | 可查期間由 ClickHouse 強制：租戶 view 的 DEFINER 與租戶角色在三張表、三個 view 上的 row policy 加上時間窗條件，窗長取自每個租戶帳號的 `CONST` 設定（Laysi/trustiot-v2#143；見下方「可查期間」） |
| `0012_gateway_stream_ids.sql` | `telemetry` 的 `gateway_id`、`stream_id` 改成文字（協定的 `GatewayId`、`StreamId`）並加上 ID 格式的 constraint；新的批次完成表 `trustiot.stream_commits`（以閘道器 + 串流為鍵）與它的權限、row policy；收回 ingest 對 `batch_commits` 的權限；以 `CREATE OR REPLACE VIEW` 重建租戶 view，欄位型別跟著改（Laysi/trustiot-v2#639） |
| `0013_tenant_id_comment.sql` | 更正 `telemetry.tenant_id` 的欄位說明：租戶取自 ingest 的閘道器綁定表，不是閘道器的連線憑證（`COMMENT COLUMN`，只改 metadata，不重寫資料；Laysi/trustiot-v2#641） |

檔案格式（migration Job 與測試都依賴）：

- 檔名 `NNNN_小寫底線.sql`，從 `0001` 連號，依檔名順序套用。
- 一個檔案可以有多條陳述式；以行首 `--` 開頭的行是註解；陳述式以行尾的 `;` 結束，其他地方（包含字串與註解欄位）不能出現 `;`。這樣不必解析 SQL 就能逐條以 HTTP 送出。
- 每條陳述式都要能重複執行，而且不毀損資料。`tests/migrations.rs` 以允許清單檢查，只接受下列寫法，其他一律拒絕，訊息寫出檔名與陳述式；之後的變更寫成新的檔案：
  - `CREATE DATABASE`、`TABLE`、`VIEW`、`MATERIALIZED VIEW`、`ROLE`、`USER`、`QUOTA`、`SETTINGS PROFILE`、`ROW POLICY`，一律 `IF NOT EXISTS`。不用 `OR REPLACE`：重建角色或 profile 會讓既有的授權失效。
  - `ALTER TABLE <表>` 只帶一個命令：`ADD COLUMN IF NOT EXISTS`、`ADD CONSTRAINT IF NOT EXISTS`、`MODIFY COLUMN`、`COMMENT COLUMN`、`MODIFY COMMENT` 或 `MODIFY TTL`；命令之後不能再出現另一個命令（例如 `DELETE`、`UPDATE`、`CLEAR`、`MOVE`、`REPLACE`、`PARTITION`、`MATERIALIZE`）、欄位 `TTL` 或 `REMOVE`。改型別的 `MODIFY COLUMN` 可能改寫或截斷已存的值，所以只接受 `tests/migrations.rs` 的 `LOSSLESS_TYPE_CHANGES` 列出的轉換（表、欄位、新型別與不會遺失資料的理由；目前是 `0012` 把 `telemetry` 的 `gateway_id`、`stream_id` 從 UUID 改成 String）；新的轉換要在那裡寫明理由，並加上讀回舊值的測試。不改型別、只改 comment、codec 或 default 的 `MODIFY COLUMN` 不受限。
  - `GRANT`、`REVOKE`、`ALTER USER`、`ALTER ROW POLICY`（例如 `0011` 在租戶 policy 加上可查期間）、`SET DEFAULT ROLE`。
  - 下一條所說的 `CREATE OR REPLACE VIEW`。
  - 任何陳述式在字串以外都不能有 `DROP`、`TRUNCATE`、`DETACH`、`EXCHANGE`、`RENAME`。所以 `REPLACE TABLE`、`RENAME TABLE`、`ALTER TABLE ... DELETE WHERE`、`CLEAR COLUMN`、搬移或取代 partition 都會被擋下。
- 搬資料的 `INSERT ... SELECT` 不寫在檔案裡，允許清單也不接受 `INSERT`：彙總表的 backfill 由 migration Job 在套用所有檔案之後執行（見下方「既有資料的 backfill」）。Job 的這類語句同樣要能重複執行（寫入的是與現狀的差，第二次什麼都不寫），而且帶 `max_execution_time`（低於 Job 每個請求等的 30 秒），逾時的語句在伺服器上也停下，不會和下一次執行的同一句同時跑。
- 唯一的例外：一般 view（不是 materialized view）的欄位改變用 `CREATE OR REPLACE VIEW`。只有在較早的檔案以 `CREATE VIEW IF NOT EXISTS` 建立過同名的 view，而且 `AS SELECT` 之前的文字（名稱、`DEFINER = <帳號>`、`SQL SECURITY DEFINER`）與原本的陳述式逐字相同時才放行，所以重建不能換 DEFINER，也不能「取代」一張表；comment 也照原本的寫。`CREATE VIEW IF NOT EXISTS` 不會改動已存在的 view，而 ClickHouse 26.8 拒絕對一般 view 執行 `ALTER TABLE ... MODIFY QUERY`（`NOT_IMPLEMENTED`，2026-10-24 實測；`0005_tenant_view.sql` 註解寫的做法不成立，已套用的檔案不能改）；materialized view 不在例外內，它的查詢改變要另訂做法。一般 view 不存資料，授權與 row policy 依名稱掛在 view 上，重建後照樣生效；Atomic 資料庫裡重建是一次交換，查詢只會看到舊的或新的 view。這個做法假設 view 所在的資料庫是 Atomic 或 Replicated 引擎：`0001_databases.sql` 以伺服器的預設引擎建立 `trustiot`（26.8 的預設是 Atomic），舊的 Ordinary 資料庫裡重建不是原子的。
- 表只用 MergeTree 系列引擎，保留搬到 ClickHouse Cloud（SharedMergeTree）的選項。view 與 materialized view 一律寫明 `DEFINER = <帳號> SQL SECURITY DEFINER`（`tests/migrations.rs` 檢查）：伺服器對一般 view 的預設是 `SQL SECURITY INVOKER`，讀 view 的帳號就得有底表的權限。

### 遙測窄表 `trustiot.telemetry`

一筆讀值一列，欄位固定：

| 欄位 | 型別 | 說明 |
| --- | --- | --- |
| `tenant_id` | UUID | 租戶，取自 ingest 的綁定對照表（見[閘道器的租戶歸屬](#閘道器的租戶歸屬)），不採信資料內容；不能是全零 UUID（constraint） |
| `gateway_id` | String | 送出這批的閘道器：協定的 `GatewayId`，取自 MQTT topic 的閘道器段（即憑證的 CN、硬體 ID），不採信資料內容 |
| `stream_id`、`batch_seq` | String、UInt64 | 這列來自哪個串流的哪一批。串流 ID 是協定的 `StreamId`，由閘道器自己產生，只在同一台閘道器內唯一，所以一批是（`gateway_id`、`stream_id`、`batch_seq`）；沒有完成紀錄的批次以這三欄刪掉已寫入的列後重寫（I1） |
| `device_id` | UUID | 設備 |
| `point` | LowCardinality(String) | 資料點在設備型號中的 key（C1），1–128 字元 |
| `ts` | DateTime64(3, 'UTC') | 閘道器的讀值時間，毫秒，UTC |
| `received_at` | DateTime64(3, 'UTC') | 伺服器收到的時間，由 ingest 填入；分區與保存期限都以它計算 |
| `value_type` | Enum8 | `float`、`int`、`bool`、`string`，指出值在哪一欄 |
| `value_float`、`value_int`、`value_bool`、`value_string` | Float64、Int64、Bool、String | 各型別的值；其他欄維持預設值（constraint `value_matches_type`） |

- `ORDER BY (tenant_id, device_id, point, ts)`，`PARTITION BY toYYYYMM(received_at)`。
- 沒有 projection，也沒有 skip index：依 `received_at` 找列只靠月分區與每個 part 的 `received_at` 最小、最大值。2026-10-31 評估過依 `(tenant_id, gateway_id, received_at)` 排序的 projection 與 `received_at` 的 minmax skip index，決定都不加（Laysi/trustiot-v2#852）：projection 會讓 I1 的輕量 DELETE 被拒絕（`lightweight_mutation_projection_mode` 預設 `throw`，`SUPPORT_IS_DISABLED`）、多存一份資料，而且讀取時 row policy 不再是獨立的 Row level filter；skip index 對現有的讀取幾乎沒有效果（I1 的三句、彙總對帳與匯出另有主鍵條件，只靠接收時間的只有不常跑的彙總 backfill）。數字、理由與重新評估的條件在 [datasets/lab/README.md](../../datasets/lab/README.md)「匯出」的「依接收時間找列的索引」。
- ID：`gateway_id`、`stream_id` 在 `0002` 建表時是 UUID，`0012` 改成 String（兩欄都不在排序鍵，`MODIFY COLUMN` 原地改型別；已有的列改存 UUID 的文字形式，不遺失資料）。constraint `gateway_id_valid`、`stream_id_valid` 套用協定的 ID 規則（1–64 個 ASCII 英數字與 `.` `_` `-`，以英數字開頭），只檢查新寫入的列。`device_id` 維持 UUID：設備由資產服務以 UUID 建立，查詢端（telemetry-query、assistant）也以 UUID 解析。
- 保存期限：`TTL toDateTime(received_at) + toIntervalDay(<天數>)`，平台預設 180 天（`src/retention.rs`）。時鐘未校正的閘道器送來舊時間戳，也不會讓資料提早過期（[Laysi/trustiot#180](https://github.com/Laysi/trustiot/issues/180)）。`0002_telemetry.sql` 建表時寫的 400 天只是建表的初值（它開頭註解寫的「改天數要寫新的 migration」是這個做法之前的寫法，已套用的檔案不能改）：migration Job 每次執行都把 `telemetry` 與完成表 `stream_commits` 的 TTL 改成設定的天數（`TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS`，必須長於閘道器緩衝的最長保存時間 30 天），不另寫 migration 檔。值、限制與理由見[資料保存期限與磁碟水位](../../docs/architecture/data-retention.md)。
- 落盤：`fsync_after_insert = 1`、`fsync_part_directory = 1`，INSERT 成功代表資料已在磁碟上，不只在 page cache（2026-10-09 可行性測試：伺服器預設兩者皆為 0）。
- 去重：`non_replicated_deduplication_window = 1000`，帶相同 `insert_deduplication_token` 的重送區塊被丟棄；窗口只以區塊數計算，長時間離線的重送靠完成表 `stream_commits` 判斷。

### 批次完成表 `trustiot.stream_commits`

`0012_gateway_stream_ids.sql` 建立（Laysi/trustiot-v2#639）。一批資料全部寫進 `telemetry` 並落盤後，ingest 才寫一筆完成紀錄，紀錄也落盤後才回確認。

| 欄位 | 型別 | 說明 |
| --- | --- | --- |
| `tenant_id` | UUID | 寫入當時閘道器所屬的租戶，取自 ingest 的綁定對照表（見[閘道器的租戶歸屬](#閘道器的租戶歸屬)）；不能是全零 UUID（constraint `tenant_id_set`） |
| `binding_id` | UUID | 寫入當時生效的閘道器綁定，同樣取自綁定對照表，供追查；不能是全零 UUID（`binding_id_set`） |
| `gateway_id` | String | 協定的 `GatewayId`，取自 topic（`gateway_id_valid`） |
| `stream_id` | String | 協定的 `StreamId`，由閘道器產生（`stream_id_valid`） |
| `seq` | UInt64 | 批次序號，從 1 開始（`seq_positive`） |
| `row_count` | UInt64 | 這批寫進 `telemetry` 的筆數；重複的批次回 `duplicate` 時回報這個數 |
| `committed_at` | DateTime64(3, 'UTC') | 整批落盤的時間 |

ingest 的寫入流程（[寫入流程](#寫入流程)，Laysi/trustiot-v2#641）與部署（Laysi/trustiot-v2#642）遵守的不變式：

- **鍵是閘道器 + 串流。** `ReplacingMergeTree`、`ORDER BY (gateway_id, stream_id, seq)`。協定的遙測規則 3：雲端以「topic 的閘道器 + 串流 ID」記錄高水位，串流 ID 由閘道器自己取，兩台閘道器可以用同一個串流 ID。高水位是 `SELECT max(seq) FROM trustiot.stream_commits WHERE gateway_id = <topic 的閘道器> AND stream_id = <串流>`，沒有紀錄時是 0（序號從 1 開始）；查詢落在排序鍵的前綴上。`gateway_id` 一律取自 topic，不取自 payload。
- **租戶與綁定只記錄、不當鍵。** 高水位屬於閘道器與它的串流，不因閘道器改綁別的租戶而重來；`tenant_id`、`binding_id` 記下這批是在哪個綁定下寫的。
- **高水位只取自這張表，不取自 `telemetry`。** 跨 partition 的 INSERT 不是原子的，`telemetry` 可能有沒完成的批次的一部分；沒有完成紀錄的批次以 `DELETE FROM trustiot.telemetry` 刪掉後重寫，條件是閘道器、串流、序號，再加上限定讀取範圍的設備、資料點、讀值時間與收到日（`src/telemetry/store.rs` 的 `DELETE_ROWS`，見[寫入流程](#寫入流程)），不含租戶。
- **紀錄可能重複，讀的人不數筆數。** 同一個（閘道器、串流、序號）寫兩次時，`ReplacingMergeTree` 只在同一個 partition 內、而且要等 merge 才合併。`max(seq)` 與任一筆的 `row_count` 不受影響；不要以紀錄的筆數當成批次數。
- **落盤**：同樣開 `fsync_after_insert`、`fsync_part_directory`。
- **保存期限**：`TTL toDateTime(committed_at) + toIntervalDay(<天數>)`，天數與 `telemetry` 相同（migration Job 設定，見上一節）。`committed_at` 不早於該批的 `received_at`，所以完成紀錄不會比它描述的資料早過期，測試會比對兩張表的 TTL 天數。建表時是 3650 天（Job 接受的最長保存期限），在 Job 對齊之前也不短於 `telemetry`。
- **權限**：`trustiot_ingest` 只有 `SELECT, INSERT`，不能刪除或修改紀錄；row policy 同其他表（`deny_by_default`、`service_all_rows`）；租戶角色沒有任何權限。
- **其他寫入者**：測試資料的載入工具 `tools/lab-dataset` 也寫完成紀錄（只在 `src/clickhouse.rs` 的 `commit_batch` 一處，見 [datasets/lab/README.md](../../datasets/lab/README.md)「批次與完成紀錄」）：改這張表的鍵或欄位時一併修改。它沒有綁定對照表，`binding_id` 是由資料集、租戶與閘道器推導的代用 ID。

**舊表 `trustiot.batch_commits` 停用。** `0003` 建的表 `ORDER BY (stream_id, seq)`，鍵裡沒有閘道器，兩台閘道器同一個串流 ID 與序號的紀錄會被合併，一台的高水位蓋掉另一台的，造成資料被當成重複而遺失；它的 `gateway_id`、`stream_id` 也是 UUID，寫不進硬體 ID。排序鍵的欄位不能改型別，migration 又不能刪表，所以改用新表：`0012` 收回 `trustiot_ingest` 對舊表的所有權限，row policy 留著（任何帳號仍然一列也讀不到），migration Job 不再管它的 TTL。到 `0012` 為止沒有任何服務寫過它（只有 `tools/lab-dataset` 在開發與測試環境寫過測試資料的紀錄），所以沒有要搬的高水位；萬一有紀錄，也只留在舊表，不再被讀取。之後要刪掉這張表時，由管理者手動執行或另訂規則，migration 檔不 `DROP`。

### 日誌表 `trustiot_logs.otel_logs`、`otel_traces`

OpenTelemetry collector（決策 22，`deploy/chart` 的 `observability.collector`）把服務遮蔽後的日誌與 trace 寫進這兩張表。欄位名稱與型別照 collector 的 ClickHouse exporter（opentelemetry-collector-contrib v0.161.0，chart 固定的版本）：exporter 關掉 `create_schema`，只送 `INSERT`，啟動時以 `DESC TABLE` 讀欄位；chart 測試比對 exporter 寫入的欄位都在表裡。升級 collector 而 exporter 多了欄位時，寫新的 migration（`ALTER TABLE ... ADD COLUMN IF NOT EXISTS`）。

- 與 exporter 預設 DDL 的差別：多一欄 `ReceivedAt`（`DEFAULT now()`，伺服器寫入的時間），分區與保存期限以它計算，發送端時鐘錯誤不會讓資料提早過期或永不過期（[Laysi/trustiot#180](https://github.com/Laysi/trustiot/issues/180)）；不建 Kubernetes resource attribute 的 materialized 欄位，也不建 trace id 查詢表與 materialized view，資料庫裡只有 MergeTree 表。
- 保存期限：`TTL ReceivedAt + INTERVAL 30 DAY`，`ttl_only_drop_parts = 1`（整個 part 過期才刪），`PARTITION BY toDate(ReceivedAt)`。改天數寫新的 migration（`ALTER TABLE ... MODIFY TTL`）。
- 角色 `trustiot_otel_collector`：兩張表的 `INSERT` 與 `SHOW COLUMNS`，沒有 `SELECT`，碰不到其他資料庫，也不能改表。collector 的帳號由 migration Job 以 chart 指定的 Secret 密碼建立，語句見 `0006_logs.sql` 的註解與 `deploy/chart/README.md`。讀日誌的帳號（Grafana）隨 Laysi/trustiot-v2#183 加入。

### 彙總表 `trustiot.telemetry_1m`、`trustiot.telemetry_1h`

1 分鐘與 1 小時的彙總（清單 I2、J3，Laysi/trustiot-v2#141），給長期趨勢、月增率與年增率、碳排月報與電能計算。原始資料只存 180 天，之後只剩彙總資料。表、view 與權限在 `0010_telemetry_rollups.sql`，對帳與電能查詢的語句在 `src/rollup.rs`。

一格（cell）一列，鍵是 `tenant_id`、`device_id`、`point`、`bucket`（讀值時間 `ts` 取到 UTC 的分鐘或小時）、`gateway_id`、`stream_id`、`received_day`（伺服器收到的日期，UTC）。每格記：

| 欄位 | 型別 | 說明 |
| --- | --- | --- |
| `readings` | `SimpleAggregateFunction(sum, Int64)` | 讀值筆數，四種型別都算 |
| `numeric_readings` | `SimpleAggregateFunction(sum, Int64)` | 有數值的筆數：`float`、`int`、`bool`（0 或 1） |
| `value_sum` | `SimpleAggregateFunction(sum, Float64)` | 數值的和；平均是 `value_sum / numeric_readings` |
| `value_min`、`value_max` | `SimpleAggregateFunction(min` 或 `max, Nullable(Float64))` | 最小、最大值，沒有數值時是 `NULL` |
| `first`、`last` | `AggregateFunction(argMin` 或 `argMax, Tuple(ts, value_type, value_float, value_int, value_bool, value_string), DateTime64(3, 'UTC'))` | 這格依 `ts` 最早與最晚的一筆，帶型別與值；累計型資料點算區間差用 |

- 引擎 `AggregatingMergeTree`，`ORDER BY` 是上面的鍵，`PARTITION BY toYYYYMM(received_day)`；fsync 與 `non_replicated_deduplication_window = 1000` 同 `telemetry`。
- 鍵含閘道器、串流與收到日的理由：一格只屬於一個串流，ingest 對帳時只動那個串流的格，不會和其他串流的寫入搶同一格；一格只屬於一個收到日，整格依收到日一起過期，對帳也不會碰到原始資料可能已過期的日子。代價是同一個 bucket 可能有多列（多個串流、跨日補傳），由租戶 view 合併。`gateway_id`、`stream_id` 以 `toString()` 存成字串，Laysi/trustiot-v2#639 把 `telemetry` 的識別碼改成字串時這兩張表不必改。
- 填入：materialized view `telemetry_1m_mv`、`telemetry_1h_mv` 在寫入 `telemetry` 的同一個 INSERT 裡把區塊加進彙總表，所以原始表看得到的讀值彙總表也立即看得到，包含還沒有完成紀錄的批次（同 `tenant_telemetry`，2026-10-25 決定）。`DELETE FROM` 不會傳到彙總表，I1 重寫之後由 ingest 對帳（下方「I1 重寫與對帳」）。
- 重送：同一個區塊重送時 `telemetry` 依去重窗口丟掉它，materialized view 產生的區塊在彙總表也被丟掉，不會重複計算。內容不同、彙總結果剛好相同的兩個原始區塊不會被當成重複：view 的區塊以它來自的原始區塊去重。
- 保存期限：`TTL received_day + toIntervalMonth(<月數>) + toIntervalDay(1)`，平台值 1 分鐘 13 個月、1 小時 48 個月（`src/retention.rs`）。以收到日計算：一格要等它的收到日整天都超過保存期限才過期，最多比它最新的一列晚一天，不會提早。migration Job 每次執行都把兩張表的 TTL 對齊設定，規則與理由見[資料保存期限與磁碟水位](../../docs/architecture/data-retention.md)「彙總資料的保存期限」。
- 數值的限制：`value_sum` 是浮點數的和，與逐筆加總可能差在最後幾位（測試以 1e-9 的相對誤差比較）；`string` 型別的資料點只有筆數與 `first`、`last` 有意義。

租戶讀 `trustiot.tenant_telemetry_1m`、`trustiot.tenant_telemetry_1h`，做法同 `tenant_telemetry`（下方「租戶讀取路徑」）：DEFINER view，租戶角色只有 view 的 `SELECT`，租戶過濾是 DEFINER `trustiot_tenant_view_definer` 在彙總表上的 row policy。view 依 `tenant_id, device_id, point, bucket` 合併同一個 bucket 在不同閘道器、串流與收到日的列，欄位是上表的值（`first`、`last` 是具名 Tuple，例如 `last.value_float`）；I1 對帳後筆數為 0 的格不列出。查詢對鍵欄位的條件經 `GROUP BY` 傳到表的主鍵。

時區（2026-10-25 決定）：bucket 以 UTC 的分鐘與小時切。整點時差的時區（台灣 UTC+8）的每日、每月數字可以從 1 小時表精確算出。有半小時或 15 分鐘時差的時區（例如 +5:30、+6:30、+5:45），本地的日界不在 UTC 整點上，要用 1 分鐘表，而 1 分鐘表只存 13 個月，所以這些時區超過 13 個月的日、月數字只能近似。之後若加 30 分鐘表，也只能從 1 分鐘表補，最多補回 13 個月。

#### 電能：累計型資料點的區間差（J3）

`src/rollup.rs` 的 `ENERGY_1M`、`ENERGY_1H` 從租戶 view 算累計型資料點（電表讀數、脈衝計數）在一段 bucket 內的增量，一律以區間差計算，不加總讀值：每個 bucket 內是最後一筆減第一筆，相鄰兩個 bucket 之間是後一格的第一筆減前一格的最後一筆。

- 讀值下降是歸零（reset），設備型號有溢位值 `wraps_at`（W）時是溢位（wrap）。bucket 內的下降以「最後一筆小於這格的最大值，或最小值小於第一筆」偵測，這時 bucket 內的增量：有 W 是 `(W − first) + last`，沒有是 `(max − first) + last`。兩格之間後一格的第一筆小於前一格的最後一筆時：有 W 是 `first + W − 前一格的 last`，沒有是 `first`（從 0 重新累計）。
- 精確的條件：每個 bucket 內至多一次歸零或溢位，而且之後這格的最後一筆低於下降前的峰值。超出條件時是近似值，例如一格內 5、7、0、9：1 分鐘表分得開，得到正確的 11；同一小時合成一格時得到 13。
- 只用「最小值小於第一筆」偵測會漏掉第一筆就是 0 的歸零（0、5、0、3 的增量是 8，會算成 3），所以另外比對最後一筆與最大值。
- 參數：`device`、`point`、`from`、`to`（UTC，含 `from` 不含 `to`）、`wraps_at`（不會溢位的填 0）。結果是 `from` 之後第一筆到 `to` 之前最後一筆的增量，沒有讀值時是 0。
- 使用者與 AI 查用量走 telemetry-query 的語意查詢 API（Laysi/trustiot-v2#142）：格內與跨格的規則相同，另有累計檢查與可疑跳增；它還不知道設備型號的 `wraps_at`（型號標記是 Laysi/trustiot-v2#691），溢位一律當成歸零，少算最多一個讀數間隔（[services/telemetry-query](../telemetry-query/README.md)「語意查詢」）。

#### I1 重寫與對帳

ingest 的寫入流程（`src/telemetry/`，Laysi/trustiot-v2#641，見[寫入流程](#寫入流程)）照下面的步驟寫入與重寫遙測列：寫入串流的每一句都照第 1 步帶 `stream_settings`。同樣的步驟寫在 `src/rollup.rs` 的模組說明；改寫入流程時兩處與這裡一起改。寫入流程不先判斷這是不是重寫：每次嘗試都先找這批已寫入的列，有才刪除（第 2 步），之後一律對帳（第 4 步，只寫一次就成功的批次什麼都不寫）。

materialized view 只處理 INSERT。重寫一批（沒有完成紀錄的批次）時，前一次嘗試已寫進 ClickHouse 的列，刪掉之後仍算在彙總表裡；前一次的原始列寫入了、彙總那一份沒寫（兩者之間當機）時，彙總表又少算。一個串流的一批這樣重寫：

1. 這個串流的每一句寫入都帶 `stream_settings`：每次嘗試的 INSERT（包括第一次），以及下面第 2 到 4 步。它們用這個串流自己的 query ID（`stream_query_id`，`trustiot-rewrite/<閘道器>/<串流>`），設 `replace_running_query = 1`，`max_execution_time` 是 `STATEMENT_MAX_SECONDS`（20 秒），`async_insert = 0`。前一次嘗試的某一句還在伺服器上執行時（client 已經放棄或當機），ClickHouse 26.8 在新的一句開始之前取消同一個帳號、同一個 query ID 的那一句，最多等 `replace_running_query_max_wait_ms`（`REPLACE_WAIT_MS`，5 秒）讓它結束。它停下之前已寫入的區塊留著，和這批其他的列一樣算；它結束之後不會再有列落地。到時還沒結束，新的一句以 `QUERY_WITH_SAME_ID_IS_ALREADY_RUNNING` 失敗，別的帳號用同一個 query ID 時也一樣：舊的那句已收到取消，但要到它下一次檢查取消時才停下，在那之前仍佔著這個 query ID，所以這次嘗試停在這裡、不寫完成紀錄，下一次嘗試再等它一次。ingest 的 ClickHouse client 對這些語句的請求逾時至少要 `CLIENT_TIMEOUT_SECONDS`（30 秒），比等待加上執行的上限長，所以 client 放棄之前伺服器已經停下那一句。一個串流仍只由一個 ingest replica 處理；這一步確保前一次嘗試的語句不會和下一次重疊。
   - INSERT 一定要同步（`async_insert = 0`；伺服器的預設是 1）：在 ClickHouse 26.8 上，非同步 INSERT 會繞過這一步。等待落地的（`wait_for_async_insert = 1`）中斷不了，取消或 `max_execution_time` 都要等它緩衝的列落地之後才結束那一句；不等待的立刻放掉 query ID，它的列與 view 寫入的部分可能在重寫的 DELETE 之後才落地，原始列與彙總就一起多算一次，兩邊一致，對帳看不出來。要把寫入路徑改成非同步 INSERT（例如為了吞吐量），先重新驗證這份約定。同步 INSERT 下每一批的 INSERT 各自產生原始、1 分鐘與 1 小時的 part；對吞吐量的影響還沒有量測。
   - query ID 的規則：沒有任何服務把呼叫端給的 query ID 轉給 ClickHouse。佔著某個串流 query ID 的語句在結束之前會卡住這個串流的寫入，這是安全的失敗：不寫完成紀錄，這批會重送。query ID 裡的閘道器 ID 取自訊息的 topic（閘道器的憑證），不取自 payload，所以閘道器只點得到自己的串流。
2. 同步刪除這批的列：`DELETE FROM trustiot.telemetry`，`lightweight_deletes_sync = 2`。條件不含租戶，前一次嘗試寫給哪個租戶的列都刪；寫給目前租戶以外的租戶的列（兩次嘗試之間閘道器改綁），刪除後立刻以那個租戶執行第 4 步的對帳，再寫入整批。
3. 以 `REWRITE_SETTINGS`（`deduplicate_insert = 'disable'`）寫入整批。開著去重時，內容與刪除之前寫過的區塊相同（同樣的列，或同樣的 `insert_deduplication_token`）的區塊會被直接丟掉、不報錯，這批的列就不見了。ClickHouse 26.8 的 `insert_deduplicate = 0` 關不掉它：設定 `deduplicate_insert`（預設 `enable`）取代了它，同步與非同步 INSERT 都一樣，只有設成 `backward_compatible_choice` 時才看 `insert_deduplicate`。
4. 以這批的參數（`ReconcileParams`）執行 `RECONCILE_1M` 與 `RECONCILE_1H`：對這批碰到的、這個串流的每一格，寫入從原始列重算的值與彙總表現在的值的差，一致時什麼都不寫。series 多到一個 URL 欄位放不下的批次，依 `ReconcileParams::split` 分段、每段各跑一次（見[寫入流程](#寫入流程)）。這兩句帶 `deduplicate_insert_select = 'disable'`，與之前某次相同的差值不會被當成重複丟掉。
5. 寫完成紀錄，再回確認。

- 第 5 步之前任何時候當機都沒有完成紀錄，閘道器會重送，下一次從第 1 步重來；對帳只看兩張表的現狀，重跑無害。只寫一次就成功的批次不需要對帳（view 在同一個 INSERT 裡執行），跑了也什麼都不寫。
- 對帳只信任原始那一側，同一個串流有其他寫入正在進行時不能對帳；一個串流一個 replica、一次只有一批在途（遙測規則 2）保證這點。
- 筆數與和精確修正；最小、最大、第一筆、最後一筆是從原始那一側合併進去，不會撤回。遙測規則 2 規定重送的批次內容不變，前一次嘗試的列就是重寫那批的列，不會改變這些值。
- 對帳只往回看 31 個收到日（`RECONCILE_WINDOW_DAYS`，比閘道器緩衝的最長保存時間 30 天多一天；2026-10-26 負責人決定）：第一次在某天 23:59 收到的一批，可能在它第 30 天結束的午夜之後才重寫（重送與重寫本身要花時間，或閘道器的時鐘有偏差），只看 30 天時第一次嘗試的那個收到日已經在窗口外，那些格就一直多算。原始資料的保存期限至少比窗口長兩天（`MIN_RAW_RETENTION_DAYS`，33 天，`RawRetention`），所以比較的原始列一定還在：這兩天讓對帳碰不到在午夜前後過期的原始列，也碰不到時鐘走得比較快的 replica 上的 TTL merge，而一句讀的是它開始時的 part。
- ingest 角色在兩張彙總表只有 `SELECT, INSERT`，不能刪除或改表。

#### 既有資料的 backfill

`0010` 本身不寫入任何列。migration Job 套用完所有檔案、把各表的 TTL 對齊設定之後，才把套用之前就在 `telemetry` 裡的列補進彙總表（`tools/migrate` 的 `backfill_aggregates`，語句是 `src/rollup.rs` 的 `BACKFILL_1M`、`BACKFILL_1H`），完成後在 `trustiot_migrate.applied` 記一筆 `backfill/telemetry_rollups`，之後的執行不再補。先對齊 TTL 的理由：檔案以平台值（1 分鐘 13 個月）建表，設定比較長時，補進去的舊格會在對齊之前被 merge 依 13 個月刪掉。

- **只補彙總缺的格，不改任何已有的格**：backfill view `telemetry_1m_backfill`、`telemetry_1h_backfill`（以 `trustiot_rollup_definer` 執行，參數 `first_day`、`last_day`、`raw_retention_days`）列出從原始列重算的值與現有彙總不一致的格，`aggregated` 欄標出彙總裡有沒有這一格；Job 只寫入 `aggregated = 0` 的格。所以再補一次什麼都不寫，前一次失敗後再執行只補還缺的格；`trustiot_migrate.applied` 遺失（例如 ClickHouse 從沒有 `trustiot_migrate` 的備份還原）後重新套用檔案、再補一次，也不會拿掉或改動任何一格，即使原始資料已經過期。
- **只比對原始資料一定還在的收到日**：view 一律排除 `today − raw_retention_days + 2`（UTC）之前的收到日，原始與彙總兩邊都排除。原始 TTL 在收到後 `raw_retention_days` 天刪掉一列，所以 `today − raw_retention_days + 1` 是列還完整的最舊一天，它最早的列在下一個午夜過期；多留的一天讓在午夜前開始或跨過午夜的一句也碰不到執行中過期的列。更舊的格比原始列活得久（彙總保存得比較久），這種差異是預期的，view 不會把它列成要修正的差（寫進去會抹掉只剩彙總的歷史）。`raw_retention_days` 大於 3650 時以 3650 計，避免日期運算溢位，只會多排除一些日子。
- Job 傳的 `raw_retention_days` 是這次執行之前 `telemetry` 的 TTL 與設定值中較短的一個：這次縮短保存期限時，較舊的列正在過期；這次延長時，舊的較短期限已經刪掉的列不會回來。Job 只補 `today − raw_retention_days + 3` 到今天（`backfill_day_count`），比 view 的界限少一天：每一句依它開始時伺服器的日期算界限，執行途中過了午夜（UTC）界限就往後一天，多留的這一天讓列出的每一天仍在界限以內，同一天的 1 分鐘與 1 小時兩句補的是同樣的格。界限一個午夜只動一天，Job 至少每兩個請求逾時就往後補一天，之後的午夜追不上它。
- **平常沒有帳號讀得到 backfill view**：row policy `migration_account` 不點名任何帳號（`TO NONE`），管理帳號也讀到 0 列（`deny_by_default`）。Job 補之前把它改成點名自己（`TO CURRENT_USER`），補完改回 `TO NONE`（補失敗時也先改回再停止），所以換了管理帳號也照樣補得到。Job 在補的途中被停掉（例如超過 Job 的 `activeDeadlineSeconds`，或 pod 被刪）時來不及改回，policy 留在點名 Job 的帳號，直到下一次執行：每一次有 ClickHouse 檔案的執行，不論有沒有 backfill 紀錄，都在 backfill 那一步之後讀兩個 policy，點名了任何帳號、角色或所有人（`TO ALL`）就改回 `TO NONE`、報告這項變更並再讀一次，仍點名時停止（`tools/migrate` 的 `hide_backfill_views`）。
- 一個收到日一句，每句 `max_execution_time = 25`，在 Job 30 秒的請求逾時之內：逾時的一句在伺服器上也停下，不會和下一次執行同時寫。原始保存期限 3650 天時最多 7,296 句（3,648 天各兩句）；大表上一天要多久還沒有實測，Laysi/trustiot-v2#641 之前也沒有持續寫入的資料。
- backfill 假設執行時沒有其他寫入 `telemetry` 的程式：一個區塊的原始部分已寫入、view 部分還沒寫入時，那一格在彙總裡還不存在，backfill 補一次、view 再加一次。第一次執行時成立：之前的版本沒有 ingest 的寫入路徑；不要在 Job 執行時用 `tools/lab-dataset` 載入資料。`applied` 遺失後在有寫入的部署上再補時，正在寫入的格可能多算，以 runbook 的檢查找出並修正。
- 檢查一段收到日的彙總是否與原始資料一致、並修正（包括拿掉原始列已刪除、沒有對帳的格：`src/rollup.rs` 的 `REPAIR_1M`、`REPAIR_1H`）：步驟見 [migration Job runbook](../../docs/runbooks/migrations.md)「彙總表的 backfill 與一致性檢查」。修正會改動已有的格，`raw_retention_days` 傳得比原始列實際保存的天數長，就會抹掉只剩彙總的歷史，所以 runbook 寫明要傳哪個值。

## 租戶隔離（row policy 與租戶 view）

帳號、角色、row policy、settings profile 與 quota 一律以 SQL 建立，存在 ClickHouse 自己的 access storage（`local_directory`），不寫伺服器設定檔（決策 14；ClickHouse Cloud 不能改設定檔）。唯一的例外是 bootstrap 管理帳號：映像依環境變數 `CLICKHOUSE_USER`、`CLICKHOUSE_PASSWORD_FILE`、`CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` 產生，相當於 ClickHouse Cloud 開好的 `default` 帳號，只給 migration Job 用。

租戶帳號只能讀三個 view：原始資料的 `trustiot.tenant_telemetry` 與彙總的 `tenant_telemetry_1m`、`tenant_telemetry_1h`，所有表都沒有任何權限；理由與取捨見下一節「租戶讀取路徑」。

| 物件 | 內容 |
| --- | --- |
| 角色 `trustiot_tenant_reader` | 只有三個租戶 view 的 `SELECT`（`trustiot.tenant_telemetry`、`tenant_telemetry_1m`、`tenant_telemetry_1h`）；每租戶唯讀帳號掛這個角色 |
| 角色 `trustiot_otel_collector` | 日誌表的 `INSERT, SHOW COLUMNS`，見上一節；在 `trustiot` 資料庫沒有任何權限 |
| 角色 `trustiot_monitoring` | 只有 `SELECT ON system.disks`，給評估磁碟水位告警的帳號（Grafana，Laysi/trustiot-v2#183）；讀不到任何 TrustIoT 資料庫或其他系統表 |
| 角色 `trustiot_ingest` | `telemetry` 的 `SELECT, INSERT, ALTER DELETE`，`stream_commits` 與兩張彙總表的 `SELECT, INSERT`（彙總表給 I1 對帳），停用的 `batch_commits` 沒有任何權限（`0012` 收回）；沒有任何改欄位、TTL 或表設定的權限。ClickHouse 26.8 的 `DELETE FROM` 檢查的就是 `ALTER DELETE`（沒有另外的權限）。這個權限實際上還允許 `ALTER TABLE ... DELETE` 與分區管理語句：`DETACH PARTITION`、`DROP PARTITION`、`DROP DETACHED PARTITION`、`ATTACH PARTITION FROM`、`REPLACE PARTITION FROM`（2026-10-11 驗證報告的發現 6）。程式碼只用 `DELETE FROM`；服務帳號的密碼要當成能卸下或刪除整個分區的權限保管 |
| view `trustiot.tenant_telemetry` | `DEFINER = trustiot_tenant_view_definer SQL SECURITY DEFINER`；欄位同 `telemetry`（`gateway_id` 是 String），但不含只有 ingest 用的 `stream_id`、`batch_seq`。view 保留建立時的欄位型別，`CREATE VIEW IF NOT EXISTS` 也不改既有的 view，所以表的欄位型別改變時要以 `CREATE OR REPLACE VIEW` 重建（`0012`，見「檔案格式」）；DEFINER、row policy（含 `0011` 的可查期間）與授權不變。彙總的 `tenant_telemetry_1m`、`tenant_telemetry_1h` 同樣以這個帳號執行（上方「彙總表」） |
| 帳號 `trustiot_tenant_view_definer` | 三個租戶 view 的 DEFINER。`IDENTIFIED WITH no_password HOST NONE`：不能登入，沒有密碼要輪替；只有 `telemetry` 與兩張彙總表的 `SELECT` |
| 帳號 `trustiot_rollup_definer` | 填彙總表的 materialized view 與 backfill view 的 DEFINER。同樣 `no_password HOST NONE`；只有 `telemetry` 的 `SELECT` 與兩張彙總表的 `SELECT, INSERT`，以及它在這三張表上看得到全部列的 row policy `rollup_definer`。沒有這條 policy 時 `deny_by_default` 會讓 view 讀到 0 列：寫入照樣成功，彙總表卻什麼都沒加 |
| row policy `deny_by_default`（每張表與每個 view） | `USING 0 TO ALL`。ClickHouse 的伺服器預設 `users_without_row_policies_can_read_rows = true`：表上有 policy、但沒有 policy 適用於某帳號時，那個帳號看得到全部的列。這條讓每個帳號都至少有一條 policy，沒有被其他 policy 點名的角色或帳號一列也讀不到，不必改伺服器設定 |
| row policy `tenant_view_definer`（`telemetry` 與兩張彙總表） | `USING tenant_id = toUUID(getSetting('SQL_tenant_id')) AND <時間窗> TO trustiot_tenant_view_definer`。租戶過濾與可查期間真正生效的地方：DEFINER view 裡的查詢沿用呼叫者的設定，`SQL_tenant_id` 與時間窗設定就是呼叫帳號的常數（時間窗見下方「可查期間」） |
| row policy `tenant_isolation`（三個租戶 view、`telemetry` 與兩張彙總表） | `USING tenant_id = toUUID(getSetting('SQL_tenant_id')) AND <時間窗> TO trustiot_tenant_reader`；沒有這些設定的帳號查詢時報錯（`UNKNOWN_SETTING`），不會拿到資料。view 上這條是第二層；表上這條在租戶角色沒有表權限時不起作用，只在萬一誤加了表權限時仍然過濾列 |
| 帳號 `trustiot_ingest_writer` | 部署時服務用的寫入帳號，不在 DDL 檔案裡：migration Job 在服務啟用時以 `CREATE USER ... DEFAULT ROLE trustiot_ingest` 建立，只持有這個角色、沒有直接權限與 settings；密碼由 Job 產生在 chart 的 Secret `<release>-ingest-password`。Job 每次執行都檢查角色的權限與持有者、帳號的角色、設定與登入方式，不符時先讓帳號不能登入再停止（`tools/migrate/src/clickhouse.rs`，Laysi/trustiot-v2#642） |
| row policy `service_all_rows`（`telemetry`、`stream_commits`、停用的 `batch_commits` 與兩張彙總表） | `USING 1 TO trustiot_ingest`；服務帳號另建的 policy |
| row policy `deny_by_default`（materialized view `telemetry_1m_mv`、`telemetry_1h_mv`） | 從 materialized view `SELECT` 時，ClickHouse 以 view 的 DEFINER 讀目標表，讀者在彙總表上的 policy 不適用：沒有這條，任何拿到整個資料庫 `SELECT` 的帳號（管理帳號也是）都能經 view 讀到每個租戶的彙總（2026-10-25 測試發現）。有了它所有帳號經 materialized view 都讀到 0 列，寫入不受影響 |
| row policy `migration_account`（backfill view） | `USING 1 TO NONE`：平常不點名任何帳號，管理帳號經 `deny_by_default` 也讀到 0 列。migration Job 補資料時、管理者依 runbook 檢查或修正時，暫時改成 `TO CURRENT_USER`，做完改回 `TO NONE`（上方「既有資料的 backfill」） |
| settings profile `trustiot_tenant_reader` | `readonly = 1` 與查詢上限（逾時 30 秒、記憶體 512 MiB、2 個 thread、同時 4 個查詢、讀取 10 億列、結果 100 萬列），全部 `CONST` |
| quota `trustiot_tenant_reader` | `KEYED BY user_name`：每分鐘 120 個查詢、60 個錯誤；每小時 3,000 個查詢、執行 1,800 秒、讀取 200 億列。沒有 `KEYED BY` 時所有租戶共用一個計數，A 用完會擋到 B |

profile 與 quota 的數字是例子，建立租戶時依方案訂實際值（Laysi/trustiot-v2#119）。

新增租戶只要兩句（Laysi/trustiot-v2#119 在建立租戶時執行；測試以同樣的語句建立租戶帳號），租戶帳號查詢三個租戶 view。三個時間窗設定是可查期間（下方「可查期間」），值由 telemetry-query 依方案寫入：

```sql
CREATE USER <name> IDENTIFIED WITH sha256_password BY '<password>'
    SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '<tenant UUID>' CONST,
    SQL_queryable_raw_days = <天> CONST, SQL_queryable_1m_months = <月> CONST,
    SQL_queryable_1h_months = <月> CONST;
GRANT trustiot_tenant_reader TO <name>;
```

profile 同時掛在帳號與角色上：帳號在 session 裡 `SET ROLE NONE` 會失去 view 的 `SELECT` 權限，查詢上限仍然有效。quota 掛在角色上，`SET ROLE NONE` 之後不再計數，但這時帳號已讀不到任何表或 view，查詢上限也仍然限制它能用的資源。

### 租戶讀取路徑：DEFINER view（Laysi/trustiot-v2#260）

**缺口**：ClickHouse 26.8 的 `mergeTreeAnalyzeIndexes` 與 `mergeTreeAnalyzeIndexesUUID` 是分析 MergeTree 索引的 table function，只要表的 `SELECT` 權限就能呼叫，卻不套用 row policy。回應不含欄位值，但會隨其他租戶的資料是否符合條件而不同（2026-10-11 驗證報告的發現 1）。26.8 沒有能單獨關掉它們的權限或設定。

**做法**：租戶角色拿不到任何表的權限，只能讀 DEFINER view。每個指名表的 table function 都檢查呼叫者對那張表的權限，所以對表一律被拒（`ACCESS_DENIED`）；對 view，這類函式因為它不是 MergeTree 表而無法執行。租戶過濾寫成 DEFINER 帳號在表上的 row policy，view 自己的 policy 是第二層。

比較過的做法：

| 做法 | 結論 |
| --- | --- |
| DEFINER view，租戶過濾是 DEFINER 帳號在表上的 row policy | 採用。表的 row policy 以 row-level filter 執行，先於 PREWHERE 與使用者的 WHERE，也用在主鍵分析 |
| DEFINER view，DEFINER 讀全部的列，只靠 view 上的 row policy 過濾 | 不採用。view 的條件與使用者的 WHERE 合併進同一個 PREWHERE，沒有先後保證：2026-10-11 實測，WHERE 裡的 `throwIf()` 碰到同一個 granule 內其他租戶的列而報錯，成為逐值判斷的管道 |
| 租戶條件寫在 view 的查詢（WHERE）裡 | 不採用。同樣的錯誤管道；而且 bootstrap 帳號建 view 時沒有 `SQL_tenant_id`，必須改寫成 `getSettingOrDefault` |
| 保留表的 `SELECT`，以權限或設定關掉這些函式 | 做不到。26.8 沒有對應的權限或設定（只有 `eval` 有實驗開關），之後的版本新增的內省函式也可能一樣不套用 row policy |
| 每個租戶一個 view、表或資料庫 | 不採用。建租戶時要在執行期做 DDL（違反決策 14），物件數隨租戶增加 |
| telemetry-query 在應用層擋 table function | 不當主要防線。決策 14 要求隔離由資料庫強制，AI 產生的 SQL 不受信任；可以另外做成縱深防禦 |

**取捨**（都有測試或 2026-10-11 的實測，見 `docs/test-reports/2026-10-11-260-introspection.md`）：

- **寫入與刪除**：ingest 照舊直接寫 `telemetry`，`DELETE FROM`（`ALTER DELETE` 權限）路徑不變；刪掉的列在 view 上也立刻看不到。
- **查詢上限與 quota**：view 以 DEFINER 身分讀表，但查詢的設定仍是呼叫者的（`readonly`、`max_threads`、`max_execution_time`、`SQL_tenant_id`）；外層查詢的讀取列數與記憶體上限涵蓋 view 內讀的列，quota 把 view 讀的列算在呼叫帳號上。
- **效能**：view 裡同樣用主鍵分析，WHERE 也會自動轉成 PREWHERE。以 90 萬列（3 個租戶）比較 6 種查詢，經 view 與直接查表的讀取列數與 granule 數完全相同；在負載很高的測試主機上，經 view 的中位數多 5–25 毫秒。view 的 policy 讓 PREWHERE 多一次 `tenant_id` 比對，不多讀任何 granule。view 不接受明寫的 `PREWHERE`（`ILLEGAL_PREWHERE`），也沒有 `_part` 等虛擬欄位；查詢一律寫 `WHERE`。
- **DEFINER 帳號**：不能登入、沒有密碼，不需要輪替。view 還在用它時，ClickHouse 拒絕刪除這個帳號（`HAVE_DEPENDENT_OBJECTS`）。要換 DEFINER 時，新的 migration 執行 `ALTER TABLE trustiot.tenant_telemetry MODIFY SQL SECURITY DEFINER DEFINER = <帳號>`；新帳號若沒有自己的 row policy，view 讀到 0 列（`deny_by_default`），不會開放全部的列。伺服器設定 `allow_no_password = 0` 時，`CREATE USER` 會失敗，migration 停下而不是放行；搬到 ClickHouse Cloud 時要驗證這句。
- **建帳號（Laysi/trustiot-v2#119）**：範本不變，仍是上面兩句，只是查詢對象改成 `trustiot.tenant_telemetry`。彙總表（Laysi/trustiot-v2#141）照這個做法，之後要給租戶讀的表也比照辦理：租戶角色只拿 DEFINER view 的 `SELECT`，租戶過濾寫成 DEFINER 帳號在表上的 row policy，migration Job 檢查租戶角色的權限正好是這些 view 的 `SELECT`（`tools/migrate` 的 `TENANT_VIEWS`）。

已知的限制：

- 租戶從 `EXPLAIN indexes` 看得到整張表的 part 數與 granule 總數（一個 granule 約 8,192 列），也就是所有租戶合計的大約筆數；看不到任何一列的內容，也分不出各租戶的筆數。`system.tables` 對 view 的 `total_rows` 是 `NULL`，`EXPLAIN ESTIMATE` 只算到自己的列。
- 讀取統計（`X-ClickHouse-Summary` 的 `read_rows`）以 granule 為單位：自己的資料落在 granule 邊界時，數字會包含同一個 granule 裡其他租戶的列數。對其他租戶下的條件不會改變這個數字（測試比對過）。
- ClickHouse 26.8 的 quota 不計算沒有讀表的常數查詢（例如 `SELECT 1`）。

### 可查期間（Laysi/trustiot-v2#143）

方案的可查期間（清單 B3、J3；[方案](../../docs/product/plans.md)）由 ClickHouse 強制，不靠服務的程式碼：歷史 API、語意工具與 AI 的 SQL 讀到的都是同一個範圍，不論查詢寫了什麼條件。`0011_queryable_period.sql` 在租戶過濾的同一批 row policy 加上時間窗條件，窗長是租戶帳號自己的 `CONST` 設定，一張表一個：

| 設定 | 表 | 單位 | 條件（列看得到的時候） |
| --- | --- | --- | --- |
| `SQL_queryable_raw_days` | `telemetry`（`tenant_telemetry`） | 天 | `ts >= now64(3, 'UTC') - toIntervalDay(<天>)` |
| `SQL_queryable_1m_months` | `telemetry_1m`（`tenant_telemetry_1m`） | 月 | `bucket >= now64(3, 'UTC') - toIntervalMonth(<月>)` |
| `SQL_queryable_1h_months` | `telemetry_1h`（`tenant_telemetry_1h`） | 月 | `bucket >= now64(3, 'UTC') - toIntervalMonth(<月>)` |

- **邊界**：含起點，不取整；沒有上限（時間在未來的讀值看得到）。依讀值時間（`ts`、格的 `bucket`），不是收到時間。`now64` 是 ClickHouse 伺服器在查詢開始時的時鐘。減月份時保留日期，該月沒有那一天時取月底（2026-05-31 減 3 個月是 2026-02-28）。
- **生效的位置**：與租戶過濾相同。DEFINER 帳號在三張表上的 policy 先於 PREWHERE 與使用者的 WHERE 執行，使用者的任何條件都碰不到窗外的列；租戶角色在三個 view 與三張表上的 policy 是第二層。服務、彙總的 DEFINER 與 migration 帳號的 policy 不變，仍讀全部的列，所以寫入、彙總、backfill 與對帳不受影響。
- **值與失敗時**：`accurateCast(..., 'UInt16')` 拒絕負數、過大與非數字的值（`CANNOT_CONVERT_TYPE`、`CANNOT_PARSE_TEXT`），帳號沒有這些設定時每次讀取都報 `UNKNOWN_SETTING`：設定缺漏或錯誤時讀不到任何列。`least(..., 36500)` 與 `least(..., 1200)` 只防止日期運算溢位，服務寫入的值都在保存期限內。租戶帳號是 `readonly = 1`，設定又是 `CONST`，帳號自己以 `SETTINGS`、URL 參數或 `SET` 都改不了（`READONLY`）。
- **誰寫入**：telemetry-query 建帳號時寫入，每次以帳號查詢之前讀回；缺少或與方案不同時，以 `ALTER USER ... MODIFY SETTINGS` 重設（只改這三個設定，profile 與 `SQL_tenant_id` 不變），所以方案變更從帳號的下一個查詢起生效。服務自己裁切範圍時從讀回的窗長算，起點比資料庫晚 5 分鐘並進位到格（[telemetry-query](../telemetry-query/README.md#可查期間)）。

**重建 view 之後**：`0012_gateway_stream_ids.sql` 以 `CREATE OR REPLACE VIEW` 重建 `tenant_telemetry`（欄位型別跟著表改），它的 row policy 依名稱掛在 view 上，`0011` 的時間窗照樣生效；`tests/migrations.rs` 的升級測試與 telemetry-query 的 `tests/queryable_period.rs` 在重建後的 view 上檢查。

**部署**：migration Job 先套用 `0011`，新版 telemetry-query 才啟動。這之間，舊版 pod 以還沒有時間窗設定的既有帳號查詢會得到 `UNKNOWN_SETTING`，回應 503；新版 pod 在每個帳號的第一個查詢補上設定。也就是部署 `0011` 時，每個租戶的遙測查詢會有一段短暫的 503，直到舊 pod 換掉為止（2026-10-26 決定接受，v2 尚未上線）。

**回復**：只回復程式碼時，已有時間窗設定的帳號照常運作（舊版不讀這些設定）；舊版新建的帳號沒有設定，查詢會得到 503，直到 policy 也回復。回復 policy 要寫成新的 migration 檔（套用過的檔案不能改），把 9 條 policy 的條件改回只有租戶過濾；帳號上留下的設定無害，不必刪：

```sql
ALTER ROW POLICY tenant_view_definer ON trustiot.telemetry AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_view_definer;
ALTER ROW POLICY tenant_isolation ON trustiot.telemetry AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader;
ALTER ROW POLICY tenant_isolation ON trustiot.tenant_telemetry AS PERMISSIVE FOR SELECT
USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader;
-- telemetry_1m、tenant_telemetry_1m、telemetry_1h、tenant_telemetry_1h 同上，
-- 表上兩條（tenant_view_definer、tenant_isolation），view 上一條（tenant_isolation）。
```

## 測試

在 repo 根目錄執行：

```sh
# 全部：靜態檢查 migration 檔，再對真的 ClickHouse 套用並跑租戶隔離測試
CARGO_BUILD_JOBS=2 cargo test -p ingest

# 印出每個攻擊、每個內省探測與結果
CARGO_BUILD_JOBS=2 cargo test -p ingest --test clickhouse -- --nocapture
```

`tests/clickhouse.rs` 需要 ClickHouse：設了 `TEST_CLICKHOUSE_URL`（加上有 access management 權限的 `TEST_CLICKHOUSE_USER`、`TEST_CLICKHOUSE_PASSWORD`）就用它；否則以 podman 或 docker 起一個暫時的容器（記憶體上限 1500 MB，測完移除）；兩者都沒有時略過，但設了 `CI` 時改為失敗。映像與 `scripts/lib/stack.ts`、`deploy/chart/values.yaml` 固定同一個 digest，`bun test deploy/chart` 檢查三處一致。這個容器與 HTTP client 在 `tests/common/mod.rs`，`services/telemetry-query` 與 `tools/lab-dataset` 的測試也以 `#[path]` 引用它，改它時一併跑這兩個 crate 的測試。

測試內容：

1. migration 套用兩次（重複套用不出錯）。
2. 內省（Laysi/trustiot-v2#260）：測試列出 ClickHouse 26.8 全部 94 個 table function 的分類，伺服器有沒分類的函式、或外部來源類的函式變成唯讀帳號可用時失敗。指名表的 24 個函式，以及 `EXPLAIN indexes`、`EXPLAIN ESTIMATE`、讀取統計、`hasColumnInTable()`、`system.tables`／`system.columns` 的統計，都對每張表與三個租戶 view 各以租戶 B 的 ID 和不存在的 ID 執行：兩次的答案（含讀取列數）必須相同，對表必須被拒或沒有任何列。
3. 租戶帳號讀得到的系統表與 information_schema 表必須在 24 張的白名單內，內容不含 B 的資料；`system.tables` 只列出三個租戶 view。
4. 從伺服器讀回表設定：每張表都是 MergeTree 系列、開了 `fsync_after_insert` 與 `fsync_part_directory`、`telemetry` 以 `received_at` 分區、完成表 `stream_commits` 以 `(gateway_id, stream_id, seq)` 排序且 TTL 天數不短於 `telemetry`；閘道器與串流 ID 是 String、租戶與設備 ID 是 UUID；兩張彙總表是 `AggregatingMergeTree`，去重窗口同 `telemetry`，以 `received_day` 分區，TTL 是 13 與 48 個月；`trustiot_logs` 存在；MergeTree 以外只有三個租戶 view、兩個 materialized view 與兩個 backfill view。
5. 角色、row policy、profile、quota 與兩個 DEFINER 帳號全部存在 `local_directory`（以 SQL 建立）；租戶角色只有三個租戶 view 的 `SELECT`；租戶 view 的 DEFINER 只有 `telemetry` 與兩張彙總表的 `SELECT`，彙總的 DEFINER 只有 `telemetry` 的 `SELECT` 與彙總表的 `SELECT, INSERT`，兩者都不能登入；materialized view 與 backfill view 以彙總的 DEFINER 執行；`0012` 重建 `tenant_telemetry` 之後 DEFINER 與 comment 仍在，`gateway_id` 是 String，租戶經 view 讀到自己的文字閘道器 ID。
6. 租戶 A、B 經 view 各自讀到應有的列，服務帳號讀到全部；只有 `SELECT` 權限、沒有 policy 點名的角色讀到 0 列。
7. 以租戶 A 的帳號經 view 跑 2026-10-09 可行性測試的攻擊類型（兩個彙總 view 各跑一次同類的攻擊，A、B 的讀值在同一個設備、資料點與分鐘）（直接查、JOIN、子查詢、UNION、FINAL、merge()、view()、系統表）、改設定、表函式、冒用身分與寫入，以及 Laysi/trustiot-v2#59 驗證新增的攻擊；WHERE 裡的 `throwIf()` 不會碰到其他租戶的列。每一項都得到預期的答案或被拒絕，回應中從未出現 B 的租戶 ID 或 B 的資料。同一批攻擊改查 `telemetry` 時一律被拒。
8. 新帳號預設讀不到：沒有角色的帳號被拒，直接被授與 `SELECT` 的帳號、bootstrap 管理帳號、有租戶設定但不在租戶角色的帳號讀每張表、每個租戶 view 與 materialized view 都讀到 0 列或報錯（backfill view 只有套用檔案的管理帳號讀得到）；租戶帳號讀任何表、materialized view 或 backfill view 一律被拒；`merge('trustiot', '.*')` 只算到自己在三個租戶 view 的列（`0011` 之前這句因欄位轉換失敗而報錯，不是權限拒絕）。
9. `SET ROLE NONE` 之後讀不到資料、查詢上限仍有效；沒有 `SQL_tenant_id` 設定的租戶帳號查詢時報錯。測試的租戶帳號都帶設定允許的最長時間窗（讀值在固定的過去時間）；可查期間本身由 `services/telemetry-query/tests/queryable_period.rs` 測試。
10. 呼叫帳號的讀取列數與記憶體上限涵蓋 view 內讀的列；quota 把 view 讀的列算在呼叫帳號上。
11. ingest 帳號能寫入、以 `DELETE FROM` 依閘道器 + 串流 + 序號刪除一批（租戶經 view 也看不到了）、寫完成紀錄並讀回高水位、讀寫彙總表；建表、改表、改 TTL、改表設定、刪表、清表、改 view、建帳號、改 policy、刪除彙總表的列、刪除或修改完成紀錄、讀寫停用的 `batch_commits` 一律被拒；constraint 擋下全零租戶、放錯欄位的值與不合協定規則的閘道器或串流 ID。ingest 角色的權限從 `system.grants` 讀回，正好是 `telemetry` 的 `SELECT, INSERT, ALTER DELETE` 與 `stream_commits`、兩張彙總表的 `SELECT, INSERT`。
12. 完成紀錄以閘道器 + 串流為鍵（Laysi/trustiot-v2#639）：兩台閘道器用同一個串流 ID 與序號寫完成紀錄（其中一筆寫兩次），`OPTIMIZE ... FINAL` 合併之後各自的紀錄仍在、`max(seq)` 各自正確，只有同一台的重複紀錄被合併；紀錄帶著寫入時的租戶與 `binding_id`；兩台閘道器同串流同序號的遙測，刪掉其中一台的那批不影響另一台；完成紀錄的 ID、`binding_id` 與 `seq` 不合規則時被 constraint 擋下。
13. quota 以帳號分開計算：一個租戶帳號用完 quota 被擋下時，另一個照常查詢。
14. 日誌表：兩張都是 MergeTree，從伺服器讀回 `TTL ReceivedAt + toIntervalDay(30)` 與以 `ReceivedAt` 分區；以文件寫的兩句建立 collector 帳號，它能寫入兩張表並讀欄位（`DESC TABLE`），`ReceivedAt` 由伺服器填入；讀日誌、讀寫遙測表、建表、改 TTL、刪資料、清表、刪表、建帳號一律被拒。
15. 日誌與遙測的權限互不相通：租戶角色、ingest 角色與 view 的 DEFINER 在 `trustiot_logs` 沒有任何權限，collector 角色的權限只有兩張日誌表的 `INSERT`、`SHOW COLUMNS`（從 `system.grants` 讀回）；租戶帳號與 ingest 帳號讀、寫或查看日誌表一律被拒，collector 帳號讀 `telemetry`、兩張完成表與 `tenant_telemetry` 也被拒。

`tests/disk_watermark.rs`：磁碟水位告警（[`alerts/clickhouse_disk_watermark.sql`](alerts/clickhouse_disk_watermark.sql)，門檻與處理方式見[資料保存期限與磁碟水位](../../docs/architecture/data-retention.md#磁碟水位告警)）。一律以 podman 或 docker 起一個資料目錄是 256 MiB tmpfs 的暫時容器（不用 `TEST_CLICKHOUSE_URL`：共用伺服器的磁碟不是測試能填滿的），在容器裡填到 85 % 與 95 %，以監控角色的帳號評估規則，依序得到 `ok`、`warning`、`critical`，釋放後回到 `ok`；監控角色只有 `system.disks` 的 `SELECT`，讀寫遙測表、租戶 view、日誌表，改 TTL、建帳號一律被拒，沒有這個角色的帳號讀不到 `system.disks`。`cargo test -p ingest --test disk_watermark -- --nocapture` 印出每一步的磁碟用量。

`tests/rollups.rs`（Laysi/trustiot-v2#141）：一律起拋棄式容器（測試要分兩段套用檔案、卸下 materialized view、強制 merge，不能在共用的伺服器上做）。每一步之後，租戶經彙總 view 讀到的值都要等於測試以 Rust 從原始列（同一個租戶經 `tenant_telemetry` 讀）算出的值：

1. 套用 `0010` 之前已有的列由 Job 的 backfill 語句補進彙總表，再補一次什麼都不寫；管理帳號在 policy 點名自己之前，從 backfill view 一列也讀不到。原始 TTL 縮短到 10 天並強制 merge，讓 12 天前與 10 天前（只過期一部分：收到時間是當天第一毫秒的列過期、最後一毫秒的留下）的原始列過期之後：重新套用 `0010`、以 10 天或 400 天再補一次都不寫任何列；以 10 天檢查沒有任何差異、修正不寫任何列，兩天的格都不變；以 400 天檢查則會把這兩天列成負的差（view 要有界限的原因）。範圍內，刪掉一批（沒有對帳）後 backfill 不動它的格、修正把它們拿掉；彙總遺失的一天由 backfill 補回。
2. 同一個區塊重送（同樣的 `insert_deduplication_token`）只算一次。
3. I1 重寫：前一次嘗試只寫進部分的列，同步刪除後以 `REWRITE_SETTINGS` 重寫；只跑完 1 分鐘的對帳時 1 小時仍不一致，兩句都跑完後一致，再跑一次什麼都不寫；其他串流的格不變。
4. 去重的風險（E7a）：同步刪除之後，以同樣的 token、以同樣的內容、以同樣的內容加 `insert_deduplicate = 0` 重寫，三種情況的列都被靜默丟掉；改用 `REWRITE_SETTINGS` 後全部寫入，對帳後彙總一致。
5. 原始列寫入、1 小時的 view 那一份沒寫（卸下 `telemetry_1h_mv` 模擬兩者之間當機）：對帳補回 1 小時表，1 分鐘表不必修。
6. 串流的 query ID（第 1 步）：同一個串流還有一句在執行時，同一個 query ID 而不帶 `replace_running_query` 的語句被拒（`QUERY_WITH_SAME_ID_IS_ALREADY_RUNNING`）；帶 `stream_settings` 的對帳取消那一句（`QUERY_WAS_CANCELLED`）並照常執行。
7. 電能：測試自己寫的列涵蓋 bucket 內先到峰值再歸零、第一筆是 0 的歸零、bucket 內的溢位、兩格之間的歸零與平穩遞增，兩個 view 的答案都等於逐筆相鄰差的和；超出條件的一格（5、7、0、9）1 分鐘表正確，1 小時表得到上面說明的近似值。
8. 保存期限：收到日在 380 到 410 天前的列，強制 merge 後 1 分鐘表依 chrono 算出的「13 個月加一天」保留或刪除，沒有提早刪的，1 小時表全部保留。

測試裡每一句寫入串流的語句都帶 `stream_settings`。

`tools/lab-dataset/tests/rollups.rs` 載入 `lab_dataset::index_for_tests()` 的資料集（預設是合成 fixture，含沒有完成紀錄的批次與補傳；設 `TRUSTIOT_LAB_DATASET_INDEX` 改用別的索引，例如 lab 的錄製資料 Laysi/trustiot-v2#681，見 [datasets/lab](../../datasets/lab/README.md)），比對兩個彙總 view 與資料集的每一筆讀值；對索引裡每個累計型資料點（`kind = "cumulative"`，含 `wraps_at`）算電能：兩個 view 的答案要等於以同一個公式從資料集算出的值，每個 bucket 都符合精確條件時也要等於逐筆相鄰差的和，不符合時印出差多少；並確認以 `stream_settings` 對每一批對帳都不寫任何列。比對的預期值全部從資料集與它的索引算，沒有合成 fixture 專屬的數字。

`tests/migrations.rs` 的靜態檢查不需要伺服器：檢查檔名連號、陳述式切分、陳述式只用允許清單上的寫法（可重複執行、不毀損資料）、表只用 MergeTree 系列引擎，以及 view 一律是 `SQL SECURITY DEFINER`。另一個測試對 `0012_gateway_stream_ids.sql` 做升級比對，一律以 podman 或 docker 依序起兩個暫時的容器（不用 `TEST_CLICKHOUSE_URL`：要比對的是什麼都還沒套用過的伺服器）：一台全新套用全部檔案，另一台先套用 `0001`–`0011` 並以舊的形狀寫入兩筆 UUID 閘道器與串流 ID 的遙測（一筆是現在的讀值，一筆的讀值時間在 30 天前）和一筆舊完成紀錄，再套用 `0012`；兩台各再套用 `0012` 一次，表、欄位、授權、角色授與與 row policy 都不變，而且兩台相同。升級的那台在背景改寫完舊的 part 之後，舊的遙測以 UUID 的文字形式讀回（也經租戶 view），ingest 角色讀不到舊完成表；租戶帳號帶 telemetry-query 給的 7 天可查期間，重建前後經 view 都只讀到現在的那筆，證明 `0011` 掛在 view 名稱上的 policy 在重建後仍然生效。
