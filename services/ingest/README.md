# ingest（資料接收）

閘道器的遙測資料經 MQTT 寫進 ClickHouse，整批落盤後才回確認（決策 8、清單 I1）。服務在 Laysi/trustiot-v2#91 底下分段實作：2026-10-24 起有服務骨架、Postgres schema 與閘道器綁定對照表（Laysi/trustiot-v2#640，見[服務與設定](#服務與設定)、[閘道器綁定對照表](#閘道器綁定對照表)）；MQTT 的訂閱、寫入與確認是 Laysi/trustiot-v2#641，映像與 chart 的部署是 Laysi/trustiot-v2#642。這個 crate 也放 ClickHouse 的 schema 與它的測試（Laysi/trustiot-v2#59）。

## 閘道器的租戶歸屬

規則在[架構決策](../../docs/architecture/decisions.md)的決策 8、12 與清單 I1（2026-10-24 決定，Laysi/trustiot-v2#638），這裡只摘要 ingest 要做的事：

- 閘道器取自 topic（`trustiot/v1/gw/{gateway}/up/telemetry`）：broker 的 ACL 讓閘道器只能發布到以自己憑證 CN（硬體 ID）為名的 topic（[crates/protocol](../../crates/protocol/README.md) 的「Topic」「ACL」）。ingest 在叢集內連 broker，看不到閘道器的憑證，憑證裡也沒有租戶。
- 租戶取自 ingest 自己的綁定對照表：消費資產服務的 `assets.gateway_bound`（之後加上 Laysi/trustiot-v2#115 的解綁事件），對照表是該硬體 ID 目前的綁定（Laysi/trustiot-v2#640）。不在資料路徑上同步呼叫資產服務；訊息內容裡的任何租戶或綁定資訊都不採信。
- 不認得的閘道器（還沒綁定，或對照表還沒收到事件）回 `retry`、不寫入。
- 串流以「topic 的閘道器 + 串流 ID」為鍵（遙測規則 3）；完成紀錄記下寫入當時的租戶與 `binding_id`，供追查用（Laysi/trustiot-v2#639）。
- 改綁時閘道器一定換新的 SD 卡，新卡使用新的串流 ID，所以 ingest 不另外拒收舊綁定下採集的批次；這個前提與它的限制見決策 12。

## 服務與設定

`cargo run -p ingest`（`src/main.rs`）以 `crates/service-kit` 啟動：設定、JSON 日誌、`/healthz`、`/readyz`、指標與優雅關閉都照 service-kit。服務沒有 HTTP API，只有健康檢查的端點（`tools/openapi` 的 `SERVICES_WITHOUT_API`）。啟動時建立兩個 Postgres 連線池（都是 lazy 連線，連線前做 service-kit 的 session 檢查）與 ClickHouse 的用戶端，並啟動 `assets.gateway_bound` 的消費端。

readiness 在下列全部成立時才是 200：

- `postgres`：服務角色與平台角色都能開出通過檢查的 session；
- migration 檢查：資料庫有這個版本預期的每個 Postgres migration 檔（`src/schema.rs`）；
- `clickhouse`：ClickHouse 以服務帳號回答 `SELECT 1`。這個檢查涵蓋位址、TLS 與帳密，不涵蓋帳號的權限：寫入路徑（Laysi/trustiot-v2#641）會把缺少的權限當成那條語句的錯誤回報。不用碰表的語句，readiness 就不依賴 ClickHouse 的表名（Laysi/trustiot-v2#639 正在改表）。

每個檢查最多 800 毫秒（service-kit），失敗時日誌 WARN `readiness check failed` 帶檢查名稱與原因；原因只有連線錯誤的種類或 ClickHouse 的錯誤代碼，不含回應內文與密碼。

設定（`src/config.rs`，`TRUSTIOT__*` 段落都是 `deny_unknown_fields`；MQTT 的設定由 Laysi/trustiot-v2#641 加上，chart 的變數由 Laysi/trustiot-v2#642 渲染）：

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 平台設定：Postgres 位置；只接受 `verify-full`，只信任這個 CA。本機開發由 `bun scripts/dev.ts env` 印出 |
| `TRUSTIOT__DATABASE__USERNAME`、`_PASSWORD` | 服務角色 `ingest_app` 的帳密（本機開發是 `TRUSTIOT_DEV_INGEST_DATABASE_*`） |
| `TRUSTIOT__DATABASE__PLATFORM_USERNAME`、`_PLATFORM_PASSWORD` | 平台角色 `ingest_platform` 的帳密，必須是另一個角色（本機開發是 `TRUSTIOT_DEV_INGEST_PLATFORM_DATABASE_*`） |
| `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 服務角色連線池每個副本的上限，2–100，預設 10。平台角色的連線池固定 4 條（`PLATFORM_CONNECTIONS`：消費端讀事件、readiness、寫入路徑的查詢）。chart 的連線預算（`deploy/chart/templates/_postgres.tpl`）在 Laysi/trustiot-v2#642 加入 |
| `TRUSTIOT_CLICKHOUSE_URL` | 平台設定：ClickHouse 的 HTTP 介面，`http://host:port`（chart 內建的 ClickHouse，走 mesh 的 mTLS）或 `https://host:port`（外部），不能帶帳密、路徑、查詢字串 |
| `TRUSTIOT_CLICKHOUSE_CA_FILE` | 平台設定：`https` 時必填，是唯一信任的 CA，並檢查伺服器憑證的名稱（相當於 `verify-full`）；不信任系統或公開的根憑證 |
| `TRUSTIOT__CLICKHOUSE__USERNAME`、`_PASSWORD` | 服務的 ClickHouse 寫入帳號，持有角色 `trustiot_ingest`（見下方的角色表）；帳號與密碼的交付是 Laysi/trustiot-v2#642 |

ClickHouse 的用戶端（`src/clickhouse.rs`、`src/transport.rs`）複製自 telemetry-query：HTTP/1.1 走 hyper，`https` 以 rustls 與 ring、只信任設定的 CA；每個請求一條連線，單一請求最多 30 秒。三個複本（migration Job、telemetry-query、ingest）合成一個 crate 是之後的工作。

## Postgres schema 與角色

schema `ingest`，DDL 在 `migrations/postgres/`，由 migration Job 套用（`tools/migrate/src/registry.rs` 的 `PRODUCT`：有平台角色、消費 `assets` 的事件）；檔案清單在 `src/schema.rs`，readiness 檢查同一份。消費端的表在 ingest 自己的 schema（決策 4）。寫入路徑之後以 Postgres 的 advisory lock 讓一個串流同時只由一個副本處理（Laysi/trustiot-v2#641）。

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
| `bound_at` | 事件的 `occurred_at`：資產服務綁定的時間 |
| `unbound_at` | 綁定結束的時間，留給 Laysi/trustiot-v2#115 的解綁事件；目前沒有程式寫入 |
| `recorded_at` | ingest 記下這筆的時間（資料庫時鐘） |

一筆綁定一列，不是一台閘道器一列：綁定是租戶的資料，在事件的租戶交易裡寫入；閘道器解綁後綁到另一個租戶時（Laysi/trustiot-v2#115），新的綁定是另一個租戶的新一列，舊的留在原租戶。以硬體 ID 為鍵的單列表做不到這件事，因為新租戶的交易看不到也改不了舊租戶的列。

消費端（`GatewayBoundHandler`）：

- 只處理 `assets.gateway_bound`，資產服務的其他事件略過。
- `INSERT ... ON CONFLICT (tenant_id, binding_id) DO NOTHING`：同一個事件再送一次、整個訂閱重播（`Replay::Reprocess`），都只有一列，原本那列不被改寫。
- 已有同一個綁定、卻是另一台閘道器的事件（不可能是同一個事件的重送），以及沒有租戶、解不開（版本比這個 build 新的除外：那種照 `crates/outbox` 的規則重試到服務升級）或表拒收其值（SQLSTATE 22 類、CHECK）的事件，每次重試都會失敗，所以記進 dead letter，後面的事件照常處理。其他資料庫錯誤（斷線、切換）照 `crates/outbox` 的規則重試。
- 每次處理以 INFO 記錄 `applied assets.gateway_bound`，帶硬體 ID、`binding_id`、租戶、事件 ID 與是否新增。

查詢（`GatewayBindings::gateway_tenant(hardware_id) -> Option<GatewayTenant>`，`GatewayTenant` 是租戶與 `binding_id`）：取該硬體 ID 最新的一筆綁定（依 `bound_at`，同時間再依 UUIDv7 的 `binding_id`），那筆還沒結束才回答，否則是 `None`。所以較舊的綁定事件晚到（重播、dead letter 重送）只會在它自己的租戶多一列，不會蓋過較新的綁定；舊綁定也不會在新綁定結束之後重新生效。`None` 與查詢失敗時，寫入路徑回 `retry`、不寫入（決策 8）。

### 跨租戶的查詢為什麼用平台角色（決策 29）

「硬體 ID → 租戶」本質上是跨租戶的查詢：topic 只有閘道器，還不知道租戶。issue 允許兩種做法，這裡比照資產服務自己的 `assets.gateway_bindings`，採用「租戶資料表 + 平台角色唯讀查詢」，不採用「不含租戶資料的表」：

- 綁定是租戶的資料，由租戶的事件寫入；放在沒有 RLS 的表，服務角色的每一筆租戶交易都讀得到每台閘道器屬於哪個租戶，等於把跨租戶的讀取權給了所有程式路徑。現在只有平台角色讀得到，而且只能讀。
- 平台角色的查詢經 `PlatformReader`：每筆都是 `READ ONLY` 交易，資料庫拒絕任何寫入；`ingest_platform` 本來就沒有這張表的 `INSERT`、`UPDATE`、`DELETE`。reader 建立時以 INFO 記一次（actor 是 ingest 服務、purpose `gateway-tenant`），每次查詢只記 DEBUG，因為寫入路徑每一批都會查。
- `platform_access` 只有 `FOR SELECT`、條件是 `true`（只用表自己的欄位），符合 service-kit 連線檢查對平台角色的要求。
- 依硬體 ID 查詢的索引 `gateway_bindings_by_gateway` 不以 `tenant_id` 開頭，這是刻意的例外：它回答的問題本身就跨租戶。
- certs 的同一份投影（Laysi/trustiot-v2#632）在本 issue 實作時還沒進 main，所以依資產服務與決策 29 決定；兩邊的做法若之後不同，以決策 29 為準統一。

### 測試

需要 podman 或 docker（沒有時略過，設了 `CI` 時改為失敗）：

- `tests/startup.rs`：真的 binary 以 `testkit::product` 的 Postgres（平台設定與 `bun scripts/dev.ts env` 給的變數相同）與 `tests/common` 的 ClickHouse（套用全部 ClickHouse migration，建立持有 `trustiot_ingest` 的帳號）啟動。兩者都連得上時 `/readyz` 是 200；ClickHouse 連不上、ClickHouse 拒絕密碼、Postgres 連不上時都不是 200，日誌點名失敗的檢查，也不含密碼。
- `tests/gateway_bindings.rs`：以資產服務自己的 `GatewayBinder` 綁定閘道器，ingest 的表有該租戶與 `binding_id`，查詢回答它們；整個訂閱重播與同一個綁定的重複事件都只有一列；較舊的綁定事件晚到不蓋過較新的，較新的會取代；無法記錄的事件進 dead letter、後面的事件照常處理；平台角色寫不進這張表。
- `tests/cross_tenant.rs`：`crates/testkit` 的跨租戶負向測試宣告 `ingest.gateway_bindings`，服務角色讀不到也改不了別的租戶的綁定。

```sh
cargo test -p ingest --test startup --test gateway_bindings --test cross_tenant -- --nocapture
```

## ClickHouse schema

DDL 是 `migrations/clickhouse/` 的 SQL 檔，部署時由 migration Job（Laysi/trustiot-v2#60）以 bootstrap 管理帳號依檔名順序套用。執行期程式碼不建表、不改表（決策 14、[Laysi/trustiot#103](https://github.com/Laysi/trustiot/issues/103)）。真正的防線是資料庫權限：服務帳號只有讀寫與刪列的權限，改 schema 的語句一律被拒。`tools/repo-check` 另外掃描 `services/*/src`，出現 `CREATE`／`ALTER`／`DROP`／`RENAME`／`TRUNCATE`／`ATTACH`／`DETACH`／`EXCHANGE` 加 `TABLE`／`DATABASE`／`VIEW`／`DICTIONARY` 的陳述式就失敗。這個掃描只是文字比對的輔助檢查，拆開字串或在執行期組出來的語句它看不到（2026-10-11 驗證報告的發現 7），不能取代資料庫權限。刪除一批不完整的資料用 `DELETE FROM`（lightweight delete），不用 `ALTER TABLE ... DELETE`。

| 檔案 | 內容 |
| --- | --- |
| `0001_databases.sql` | `trustiot`（遙測與批次完成表）、`trustiot_logs`（服務與閘道器日誌，給 OTel collector，Laysi/trustiot-v2#73；獨立資料庫，可以分開決定保存與搬遷） |
| `0002_telemetry.sql` | 遙測窄表 `trustiot.telemetry` |
| `0003_batch_commits.sql` | 批次完成表 `trustiot.batch_commits` |
| `0004_access.sql` | 角色、row policy、settings profile、quota |
| `0005_tenant_view.sql` | 租戶唯一能讀的 view `trustiot.tenant_telemetry`、它的 DEFINER 帳號與 row policy（Laysi/trustiot-v2#260） |
| `0006_logs.sql` | OTel collector 寫入的日誌表 `trustiot_logs.otel_logs`、`otel_traces`，與 collector 的角色 `trustiot_otel_collector`（Laysi/trustiot-v2#73） |
| `0007_monitoring.sql` | 監控角色 `trustiot_monitoring`：只有 `system.disks` 的 `SELECT`，給磁碟水位告警（Laysi/trustiot-v2#92） |
| `0008_telemetry_query.sql` | telemetry-query 的服務帳號 `trustiot_telemetry_query`：只能建立與修改名稱以 `trustiot_tenant_account_` 開頭的帳號，並把租戶角色 `trustiot_tenant_reader` 給它們（Laysi/trustiot-v2#119；[services/telemetry-query](../telemetry-query/README.md#租戶帳號)） |
| `0009_telemetry_query_grantees.sql` | 服務帳號 `GRANTEES NONE`：它以 `ADMIN OPTION` 持有租戶角色，只能在 `CREATE USER ... DEFAULT ROLE` 時把角色給新建的帳號，不能把角色授與或收回任何既有的使用者或角色（Laysi/trustiot-v2#119 的安全審查） |

檔案格式（migration Job 與測試都依賴）：

- 檔名 `NNNN_小寫底線.sql`，從 `0001` 連號，依檔名順序套用。
- 一個檔案可以有多條陳述式；以行首 `--` 開頭的行是註解；陳述式以行尾的 `;` 結束，其他地方（包含字串與註解欄位）不能出現 `;`。這樣不必解析 SQL 就能逐條以 HTTP 送出。
- 每條陳述式都要能重複執行：`CREATE` 一律 `IF NOT EXISTS`；不用 `OR REPLACE`（重建角色或 profile 會讓既有的授權失效）、`DROP`、`TRUNCATE`、`DETACH`、`EXCHANGE`。之後的變更寫成新的檔案，例如 `ALTER TABLE ... MODIFY TTL`；view 的欄位改變用 `ALTER TABLE <view> MODIFY QUERY`，因為 `CREATE VIEW IF NOT EXISTS` 不會改動已存在的 view。
- 表只用 MergeTree 系列引擎，保留搬到 ClickHouse Cloud（SharedMergeTree）的選項。view 一律寫明 `DEFINER = <帳號> SQL SECURITY DEFINER`（`tests/migrations.rs` 檢查）：伺服器對一般 view 的預設是 `SQL SECURITY INVOKER`，讀 view 的帳號就得有底表的權限。

### 遙測窄表 `trustiot.telemetry`

一筆讀值一列，欄位固定：

| 欄位 | 型別 | 說明 |
| --- | --- | --- |
| `tenant_id` | UUID | 租戶，取自 ingest 的綁定對照表（見[閘道器的租戶歸屬](#閘道器的租戶歸屬)），不採信資料內容；不能是全零 UUID（constraint） |
| `gateway_id` | UUID | 送出這批的閘道器 |
| `stream_id`、`batch_seq` | UUID、UInt64 | 這列來自哪個串流的哪一批；沒有完成紀錄的批次以這兩欄刪掉已寫入的列後重寫（I1） |
| `device_id` | UUID | 設備 |
| `point` | LowCardinality(String) | 資料點在設備型號中的 key（C1），1–128 字元 |
| `ts` | DateTime64(3, 'UTC') | 閘道器的讀值時間，毫秒，UTC |
| `received_at` | DateTime64(3, 'UTC') | 伺服器收到的時間，由 ingest 填入；分區與保存期限都以它計算 |
| `value_type` | Enum8 | `float`、`int`、`bool`、`string`，指出值在哪一欄 |
| `value_float`、`value_int`、`value_bool`、`value_string` | Float64、Int64、Bool、String | 各型別的值；其他欄維持預設值（constraint `value_matches_type`） |

- `ORDER BY (tenant_id, device_id, point, ts)`，`PARTITION BY toYYYYMM(received_at)`。
- 保存期限：`TTL toDateTime(received_at) + toIntervalDay(<天數>)`，平台預設 180 天（`src/retention.rs`）。時鐘未校正的閘道器送來舊時間戳，也不會讓資料提早過期（[Laysi/trustiot#180](https://github.com/Laysi/trustiot/issues/180)）。`0002_telemetry.sql` 建表時寫的 400 天只是建表的初值（它開頭註解寫的「改天數要寫新的 migration」是這個做法之前的寫法，已套用的檔案不能改）：migration Job 每次執行都把 `telemetry` 與 `batch_commits` 的 TTL 改成設定的天數（`TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS`，必須長於閘道器緩衝的最長保存時間 30 天），不另寫 migration 檔。值、限制與理由見[資料保存期限與磁碟水位](../../docs/architecture/data-retention.md)。
- 落盤：`fsync_after_insert = 1`、`fsync_part_directory = 1`，INSERT 成功代表資料已在磁碟上，不只在 page cache（2026-10-09 可行性測試：伺服器預設兩者皆為 0）。
- 去重：`non_replicated_deduplication_window = 1000`，帶相同 `insert_deduplication_token` 的重送區塊被丟棄；窗口只以區塊數計算，長時間離線的重送靠 `batch_commits` 判斷。

### 批次完成表 `trustiot.batch_commits`

`tenant_id`、`gateway_id`、`stream_id`、`seq`、`row_count`（筆數）、`committed_at`（完成時間）。一批資料全部寫進 `telemetry` 並落盤後，ingest 才寫一筆完成紀錄，紀錄也落盤後才回確認；串流的高水位只取 `max(seq)`，不取自 `telemetry`（跨 partition 的 INSERT 不是原子的）。`ReplacingMergeTree`、`ORDER BY (stream_id, seq)`；同樣開 fsync；`TTL toDateTime(committed_at) + toIntervalDay(<天數>)`，天數與 `telemetry` 相同（由 migration Job 設定，見上一節），`committed_at` 不早於該批的 `received_at`，所以完成紀錄的保存期限不短於原始資料，測試會比對兩張表的 TTL 天數。

### 日誌表 `trustiot_logs.otel_logs`、`otel_traces`

OpenTelemetry collector（決策 22，`deploy/chart` 的 `observability.collector`）把服務遮蔽後的日誌與 trace 寫進這兩張表。欄位名稱與型別照 collector 的 ClickHouse exporter（opentelemetry-collector-contrib v0.161.0，chart 固定的版本）：exporter 關掉 `create_schema`，只送 `INSERT`，啟動時以 `DESC TABLE` 讀欄位；chart 測試比對 exporter 寫入的欄位都在表裡。升級 collector 而 exporter 多了欄位時，寫新的 migration（`ALTER TABLE ... ADD COLUMN IF NOT EXISTS`）。

- 與 exporter 預設 DDL 的差別：多一欄 `ReceivedAt`（`DEFAULT now()`，伺服器寫入的時間），分區與保存期限以它計算，發送端時鐘錯誤不會讓資料提早過期或永不過期（[Laysi/trustiot#180](https://github.com/Laysi/trustiot/issues/180)）；不建 Kubernetes resource attribute 的 materialized 欄位，也不建 trace id 查詢表與 materialized view，資料庫裡只有 MergeTree 表。
- 保存期限：`TTL ReceivedAt + INTERVAL 30 DAY`，`ttl_only_drop_parts = 1`（整個 part 過期才刪），`PARTITION BY toDate(ReceivedAt)`。改天數寫新的 migration（`ALTER TABLE ... MODIFY TTL`）。
- 角色 `trustiot_otel_collector`：兩張表的 `INSERT` 與 `SHOW COLUMNS`，沒有 `SELECT`，碰不到其他資料庫，也不能改表。collector 的帳號由 migration Job 以 chart 指定的 Secret 密碼建立，語句見 `0006_logs.sql` 的註解與 `deploy/chart/README.md`。讀日誌的帳號（Grafana）隨 Laysi/trustiot-v2#183 加入。

## 租戶隔離（row policy 與租戶 view）

帳號、角色、row policy、settings profile 與 quota 一律以 SQL 建立，存在 ClickHouse 自己的 access storage（`local_directory`），不寫伺服器設定檔（決策 14；ClickHouse Cloud 不能改設定檔）。唯一的例外是 bootstrap 管理帳號：映像依環境變數 `CLICKHOUSE_USER`、`CLICKHOUSE_PASSWORD_FILE`、`CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` 產生，相當於 ClickHouse Cloud 開好的 `default` 帳號，只給 migration Job 用。

租戶帳號只能讀 view `trustiot.tenant_telemetry`，兩張表都沒有任何權限；理由與取捨見下一節「租戶讀取路徑」。

| 物件 | 內容 |
| --- | --- |
| 角色 `trustiot_tenant_reader` | 只有 `SELECT ON trustiot.tenant_telemetry`；每租戶唯讀帳號掛這個角色 |
| 角色 `trustiot_otel_collector` | 日誌表的 `INSERT, SHOW COLUMNS`，見上一節；在 `trustiot` 資料庫沒有任何權限 |
| 角色 `trustiot_monitoring` | 只有 `SELECT ON system.disks`，給評估磁碟水位告警的帳號（Grafana，Laysi/trustiot-v2#183）；讀不到任何 TrustIoT 資料庫或其他系統表 |
| 角色 `trustiot_ingest` | `telemetry` 的 `SELECT, INSERT, ALTER DELETE`，`batch_commits` 的 `SELECT, INSERT`；沒有任何改欄位、TTL 或表設定的權限。ClickHouse 26.8 的 `DELETE FROM` 檢查的就是 `ALTER DELETE`（沒有另外的權限）。這個權限實際上還允許 `ALTER TABLE ... DELETE` 與分區管理語句：`DETACH PARTITION`、`DROP PARTITION`、`DROP DETACHED PARTITION`、`ATTACH PARTITION FROM`、`REPLACE PARTITION FROM`（2026-10-11 驗證報告的發現 6）。程式碼只用 `DELETE FROM`；服務帳號的密碼要當成能卸下或刪除整個分區的權限保管 |
| view `trustiot.tenant_telemetry` | `DEFINER = trustiot_tenant_view_definer SQL SECURITY DEFINER`；欄位同 `telemetry`，但不含只有 ingest 用的 `stream_id`、`batch_seq` |
| 帳號 `trustiot_tenant_view_definer` | view 的 DEFINER。`IDENTIFIED WITH no_password HOST NONE`：不能登入，沒有密碼要輪替；只有 `SELECT ON trustiot.telemetry` |
| row policy `deny_by_default`（兩張表與 view） | `USING 0 TO ALL`。ClickHouse 的伺服器預設 `users_without_row_policies_can_read_rows = true`：表上有 policy、但沒有 policy 適用於某帳號時，那個帳號看得到全部的列。這條讓每個帳號都至少有一條 policy，沒有被其他 policy 點名的角色或帳號一列也讀不到，不必改伺服器設定 |
| row policy `tenant_view_definer`（`telemetry`） | `USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_view_definer`。租戶過濾真正生效的地方：DEFINER view 裡的查詢沿用呼叫者的設定，`SQL_tenant_id` 就是呼叫帳號的常數租戶 |
| row policy `tenant_isolation`（view 與 `telemetry`） | `USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader`；沒有這個設定的帳號查詢時報錯（`UNKNOWN_SETTING`），不會拿到資料。view 上這條是第二層；表上這條在租戶角色沒有表權限時不起作用，只在萬一誤加了表權限時仍然過濾列 |
| row policy `service_all_rows`（兩張表） | `USING 1 TO trustiot_ingest`；服務帳號另建的 policy |
| settings profile `trustiot_tenant_reader` | `readonly = 1` 與查詢上限（逾時 30 秒、記憶體 512 MiB、2 個 thread、同時 4 個查詢、讀取 10 億列、結果 100 萬列），全部 `CONST` |
| quota `trustiot_tenant_reader` | `KEYED BY user_name`：每分鐘 120 個查詢、60 個錯誤；每小時 3,000 個查詢、執行 1,800 秒、讀取 200 億列。沒有 `KEYED BY` 時所有租戶共用一個計數，A 用完會擋到 B |

profile 與 quota 的數字是例子，建立租戶時依方案訂實際值（Laysi/trustiot-v2#119）。

新增租戶只要兩句（Laysi/trustiot-v2#119 在建立租戶時執行；測試以同樣的語句建立租戶帳號），租戶帳號查詢 `trustiot.tenant_telemetry`：

```sql
CREATE USER <name> IDENTIFIED WITH sha256_password BY '<password>'
    SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '<tenant UUID>' CONST;
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
- **建帳號（Laysi/trustiot-v2#119）**：範本不變，仍是上面兩句，只是查詢對象改成 `trustiot.tenant_telemetry`。之後要給租戶讀的表（例如 MS3 的彙總表）比照辦理：租戶角色只拿 DEFINER view 的 `SELECT`，租戶過濾寫成 DEFINER 帳號在表上的 row policy。

已知的限制：

- 租戶從 `EXPLAIN indexes` 看得到整張表的 part 數與 granule 總數（一個 granule 約 8,192 列），也就是所有租戶合計的大約筆數；看不到任何一列的內容，也分不出各租戶的筆數。`system.tables` 對 view 的 `total_rows` 是 `NULL`，`EXPLAIN ESTIMATE` 只算到自己的列。
- 讀取統計（`X-ClickHouse-Summary` 的 `read_rows`）以 granule 為單位：自己的資料落在 granule 邊界時，數字會包含同一個 granule 裡其他租戶的列數。對其他租戶下的條件不會改變這個數字（測試比對過）。
- ClickHouse 26.8 的 quota 不計算沒有讀表的常數查詢（例如 `SELECT 1`）。

## 測試

在 repo 根目錄執行：

```sh
# 全部：靜態檢查 migration 檔，再對真的 ClickHouse 套用並跑租戶隔離測試
CARGO_BUILD_JOBS=2 cargo test -p ingest

# 印出每個攻擊、每個內省探測與結果
CARGO_BUILD_JOBS=2 cargo test -p ingest --test clickhouse -- --nocapture
```

`tests/clickhouse.rs` 需要 ClickHouse：設了 `TEST_CLICKHOUSE_URL`（加上有 access management 權限的 `TEST_CLICKHOUSE_USER`、`TEST_CLICKHOUSE_PASSWORD`）就用它；否則以 podman 或 docker 起一個暫時的容器（記憶體上限 1500 MB，測完移除）；兩者都沒有時略過，但設了 `CI` 時改為失敗。映像與 `scripts/lib/stack.ts`、`deploy/chart/values.yaml` 固定同一個 digest，`bun test deploy/chart` 檢查三處一致。

測試內容：

1. migration 套用兩次（重複套用不出錯）。
2. 內省（Laysi/trustiot-v2#260）：測試列出 ClickHouse 26.8 全部 94 個 table function 的分類，伺服器有沒分類的函式、或外部來源類的函式變成唯讀帳號可用時失敗。指名表的 24 個函式，以及 `EXPLAIN indexes`、`EXPLAIN ESTIMATE`、讀取統計、`hasColumnInTable()`、`system.tables`／`system.columns` 的統計，都對兩張表與 view 各以租戶 B 的 ID 和不存在的 ID 執行：兩次的答案（含讀取列數）必須相同，對表必須被拒或沒有任何列。
3. 租戶帳號讀得到的系統表與 information_schema 表必須在 24 張的白名單內，內容不含 B 的資料；`system.tables` 只列出 view。
4. 從伺服器讀回表設定：兩張表都是 MergeTree 系列、開了 `fsync_after_insert` 與 `fsync_part_directory`、`telemetry` 以 `received_at` 分區、完成表的 TTL 天數不短於 `telemetry`；`trustiot_logs` 存在；MergeTree 以外只有租戶 view。
5. 角色、row policy、profile、quota 與 DEFINER 帳號全部存在 `local_directory`（以 SQL 建立）；租戶角色只有 view 的 `SELECT`，DEFINER 帳號只有 `telemetry` 的 `SELECT` 且不能登入。
6. 租戶 A、B 經 view 各自讀到應有的列，服務帳號讀到全部；只有 `SELECT` 權限、沒有 policy 點名的角色讀到 0 列。
7. 以租戶 A 的帳號經 view 跑 2026-10-09 可行性測試的攻擊類型（直接查、JOIN、子查詢、UNION、FINAL、merge()、view()、系統表）、改設定、表函式、冒用身分與寫入，以及 Laysi/trustiot-v2#59 驗證新增的攻擊；WHERE 裡的 `throwIf()` 不會碰到其他租戶的列。每一項都得到預期的答案或被拒絕，回應中從未出現 B 的租戶 ID 或 B 的資料。同一批攻擊改查 `telemetry` 時一律被拒。
8. 新帳號預設讀不到：沒有角色的帳號被拒，直接被授與 `SELECT` 的帳號、bootstrap 管理帳號、有租戶設定但不在租戶角色的帳號都讀到 0 列或報錯；租戶帳號讀兩張表一律被拒。
9. `SET ROLE NONE` 之後讀不到資料、查詢上限仍有效；沒有 `SQL_tenant_id` 設定的租戶帳號查詢時報錯。
10. 呼叫帳號的讀取列數與記憶體上限涵蓋 view 內讀的列；quota 把 view 讀的列算在呼叫帳號上。
11. ingest 帳號能寫入、以 `DELETE FROM` 刪除一批（租戶經 view 也看不到了）、寫完成紀錄並讀回高水位；建表、改表、改 TTL、改表設定、刪表、清表、改 view、建帳號、改 policy 一律被拒；constraint 擋下全零租戶與放錯欄位的值。
12. quota 以帳號分開計算：一個租戶帳號用完 quota 被擋下時，另一個照常查詢。
13. 日誌表：兩張都是 MergeTree，從伺服器讀回 `TTL ReceivedAt + toIntervalDay(30)` 與以 `ReceivedAt` 分區；以文件寫的兩句建立 collector 帳號，它能寫入兩張表並讀欄位（`DESC TABLE`），`ReceivedAt` 由伺服器填入；讀日誌、讀寫遙測表、建表、改 TTL、刪資料、清表、刪表、建帳號一律被拒。
14. 日誌與遙測的權限互不相通：租戶角色、ingest 角色與 view 的 DEFINER 在 `trustiot_logs` 沒有任何權限，collector 角色的權限只有兩張日誌表的 `INSERT`、`SHOW COLUMNS`（從 `system.grants` 讀回）；租戶帳號與 ingest 帳號讀、寫或查看日誌表一律被拒，collector 帳號讀 `telemetry` 與 `tenant_telemetry` 也被拒。

`tests/disk_watermark.rs`：磁碟水位告警（[`alerts/clickhouse_disk_watermark.sql`](alerts/clickhouse_disk_watermark.sql)，門檻與處理方式見[資料保存期限與磁碟水位](../../docs/architecture/data-retention.md#磁碟水位告警)）。一律以 podman 或 docker 起一個資料目錄是 256 MiB tmpfs 的暫時容器（不用 `TEST_CLICKHOUSE_URL`：共用伺服器的磁碟不是測試能填滿的），在容器裡填到 85 % 與 95 %，以監控角色的帳號評估規則，依序得到 `ok`、`warning`、`critical`，釋放後回到 `ok`；監控角色只有 `system.disks` 的 `SELECT`，讀寫遙測表、租戶 view、日誌表，改 TTL、建帳號一律被拒，沒有這個角色的帳號讀不到 `system.disks`。`cargo test -p ingest --test disk_watermark -- --nocapture` 印出每一步的磁碟用量。

`tests/migrations.rs` 不需要伺服器：檢查檔名連號、陳述式切分、可重複執行、沒有破壞資料的語句、表只用 MergeTree 系列引擎，以及 view 一律是 `SQL SECURITY DEFINER`。
