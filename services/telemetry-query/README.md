# telemetry-query（遙測查詢服務）

設備圖表的歷史資料由這個服務查 ClickHouse 回答（決策 4、14；[部署與可搬移性](../../docs/architecture/deployment.md)）。Laysi/trustiot-v2#119（2026-10-21）建立：

- 消費身分服務的 `identity.tenant_created`，為每個新租戶建一個 ClickHouse 唯讀帳號（見[租戶帳號](#租戶帳號)）。
- 歷史查詢 API：一個資料點在一段時間內依時區降採樣的序列，與設備每個資料點最新的值（見 [API](#api)）。每個查詢都以呼叫者租戶的帳號執行，所以 ClickHouse 的 row policy、settings profile 與 quota 管住每一個查詢。
- 可查期間先固定為最嚴格方案的例子（[方案](../../docs/product/plans.md) 的 Starter）：原始資料 7 天、聚合資料 3 個月，所有租戶相同；依方案區分的期間是 Laysi/trustiot-v2#453。

不包含：健康狀態歷史、首頁卡片、依方案的期間（Laysi/trustiot-v2#453）；AI 的語意工具與 SQL 解析（Laysi/trustiot-v2#142、Laysi/trustiot-v2#143）。

## 租戶帳號

`src/accounts.rs`。ClickHouse 這邊的物件都在 `services/ingest/migrations/clickhouse/`，由 migration Job 套用：

| 物件 | 來源 | 內容 |
| --- | --- | --- |
| 角色 `trustiot_tenant_reader` | `0004_access.sql`、`0005_tenant_view.sql` | 只能 `SELECT` `trustiot.tenant_telemetry`（DEFINER view）；row policy 以帳號的 `SQL_tenant_id` 過濾，讀不到基底表 |
| settings profile `trustiot_tenant_reader` | `0004_access.sql` | `readonly = 1`、`max_execution_time = 30`、`max_memory_usage = 512 MiB`、`max_threads = 2`、`max_concurrent_queries_for_user = 4`、`max_rows_to_read = 1e9`、`max_result_rows = 1e6`，全部 `CONST` |
| quota `trustiot_tenant_reader` | `0004_access.sql` | `KEYED BY user_name`：每個租戶帳號每分鐘 120 個查詢、60 個錯誤，每小時 3000 個查詢、1800 秒執行時間、200 億列 |
| 服務帳號 `trustiot_telemetry_query` | `0008_telemetry_query.sql`、`0009_telemetry_query_grantees.sql` | 只能 `CREATE USER`、`ALTER USER` 名稱以 `trustiot_tenant_account_` 開頭的帳號，並以 `ADMIN OPTION` 持有租戶角色（才能把它設成新帳號的 default role）；`GRANTEES NONE`，所以不能把租戶角色授與或收回任何既有的使用者或角色；沒有其他權限、沒有 default role，讀不到任何表 |

服務帳號的密碼是內部密鑰，由 migration Job 產生在 Secret `<release>-telemetry-query-password` 的 `trustiot_telemetry_query` key 並設定（`TRUSTIOT__PASSWORDS__CLICKHOUSE_TELEMETRY_QUERY`，[tools/migrate](../../tools/migrate/README.md)）；沒有部署這個服務時，Job 讓這個帳號不能登入。Job 每次執行都檢查這個帳號只有 `0008_telemetry_query.sql` 給的權限，租戶角色只有 `SELECT trustiot.tenant_telemetry`，而且只有這個帳號（有 `ADMIN OPTION`）與租戶帳號（沒有）持有租戶角色，這個帳號的 `GRANTEES` 仍是 `NONE`，多出任何東西或 `GRANTEES` 被改就讓服務帳號不能登入（`HOST NONE`）並停止，修好之後的下一次執行再恢復登入。`GRANTEES NONE` 是因為 ClickHouse 26.8 不限制有 `ADMIN OPTION` 的帳號把角色給誰：少了它，服務帳號在啟用租戶角色的請求裡能把角色給任何使用者或角色（例如給 `trustiot_ingest` 會讓 ingest 變成唯讀、每個租戶都寫不進去），也能從租戶帳號收回；有了它，ClickHouse 拒絕這兩種語句，`CREATE USER ... DEFAULT ROLE trustiot_tenant_reader` 仍可用，服務帳號也改不了自己的 `GRANTEES`（`tools/migrate/tests/clickhouse_telemetry_query.rs`）。服務帳號仍能 `ALTER USER` 每個名稱在前綴下的帳號，包括 `RENAME` 與 `ADD IDENTIFIED`；Job 還不稽核租戶帳號本身（Laysi/trustiot-v2#528），懷疑服務帳號外洩時的處理在 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md)「懷疑時序查詢的服務帳號外洩」。

每個租戶的帳號：

- 名稱 `trustiot_tenant_account_` 加租戶 ID 的 32 個十六進位數字；`HOST ANY`、default role 只有 `trustiot_tenant_reader`、settings profile `trustiot_tenant_reader`、`SQL_tenant_id` 固定為該租戶（`CONST`）。
- 密碼不儲存，而是推導：以服務帳號的密碼為 key，對租戶 ID 做 HMAC-SHA-256。ClickHouse 只收到加鹽的 SHA-256（`IDENTIFIED WITH sha256_hash ... SALT`），SQL 裡從不出現明文密碼。輪替服務帳號的密碼（`migrate.passwordGeneration`）會改變每個租戶的推導密碼，但 ClickHouse 裡的帳號在該租戶下一個查詢之前仍是舊密碼：那個查詢登入失敗時，服務重設它的密碼再查一次（`Accounts::ensure`）。一直沒有查詢的租戶的帳號保留舊密碼，所以懷疑服務帳號外洩時要刪掉租戶帳號（runbook 同上）。
- ClickHouse 的 HTTP handler 在 `trace` 等級把請求的 URI 寫進伺服器日誌，建立與重設租戶帳號的請求的 URI 帶著加鹽的 SHA-256 與 salt（查詢參數 `param_hash`、`param_salt`）；`system.query_log` 會遮住它們，伺服器日誌不會。叢集內的 ClickHouse 以 `information` 等級記錄；外部 ClickHouse 不可以用 `trace` 等級。
- 租戶 ID 是 SQL 文字裡唯一不是查詢參數的值（ClickHouse 26.8 的 `SETTINGS` 值不接受參數），只來自 `TenantId` 的 36 字元文字（十六進位數字與連字號）。

消費端（`TenantCreatedHandler`）：訂閱名稱 `telemetry-query.identity-events`（`SUBSCRIPTION`，改名等於從頭重讀事件），收到 `identity.tenant_created` 時：

1. `CREATE USER IF NOT EXISTS`（以服務帳號，單一查詢啟用租戶角色）。
2. 以租戶帳號登入，確認 `SQL_tenant_id` 是這個租戶、唯一啟用的角色是租戶角色、`readonly = 1`，而且 settings profile 的每個查詢上限都在生效（`accounts.rs` 的 `LIMITS`，單元測試確認它與 `0004_access.sql` 的 profile 相同）；密碼不對時重設密碼再確認一次。
3. 都對了才在事件的租戶交易裡寫 `telemetry_query.tenant_accounts`（`ON CONFLICT DO NOTHING`）。API 看到這一列才查 ClickHouse，否則回 404。

帳號存在但內容不同（別的租戶、別的角色、不是唯讀、查詢上限被改，例如管理者在帳號上把 `max_execution_time` 設在 profile 之後）表示有人改過它：服務帳號（`GRANTEES NONE`）不能授與或收回角色，所以不修，事件進 dead letter，日誌說明要由管理者刪掉這個帳號後重播事件。ClickHouse 連不上或出錯時重試。每處理一個事件記一行 INFO `applied identity.tenant_created`，帶 `tenant_id`、`event_id`、`recorded`、`password_set`；不記密碼與 ClickHouse 的錯誤內文（錯誤內文會引用查詢與參數）。

## API

`src/history.rs`，路徑前綴 `/api/telemetry/v1`（入口不去除前綴），規格在 [api/openapi.json](../../api/openapi.json)。

- `GET /api/telemetry/v1/devices/{device_id}/series?point=&from=&to=&tz=[&interval=]`：資料點 `point` 在 `[from, to)` 的序列。`from`、`to` 是 RFC 3339（`+08:00` 的 `+` 要編碼成 `%2B`，或用 `Z`）。`tz` 是 IANA 時區名稱（必填），小時與日的區間從該時區的整點與午夜開始；分鐘區間從 Unix epoch 起算（每個時區的偏移都是 15 分鐘的倍數，所以 15 分鐘區間也落在當地的刻鐘）。`interval` 是 `raw`、`1m`、`5m`、`15m`、`1h`、`3h`、`6h`、`12h`、`1d`；不給時取最多 1500 個區間（`MAX_BUCKETS`）的最細區間。每個區間回 `time`（區間開始，UTC）、`count`、`min`、`max`、`avg`（bool 當 0 或 1，文字讀數不算，全是文字時為 `null`）與 `last`（區間內最新的讀數，`{"type": "float|int|bool|string", "value": ...}`；`NaN` 與無限大是 `null`）。沒有讀數的區間不回。`raw` 最多 10000 筆（`MAX_RAW_READINGS`），每筆的 `count` 是 1。大於 2^53 的整數在 `min`、`max`、`avg` 會失去精度（`last` 不會）。
- `GET /api/telemetry/v1/devices/{device_id}/latest`：設備在原始資料期間內每個資料點最新的讀數，依資料點排序，最多 1000 個。

可查期間（`RAW_DAYS`、`AGGREGATED_MONTHS`）：`raw` 與 `latest` 是請求時往前 7 天，聚合序列是往前 3 個日曆月。`from` 早於期間開始時改成期間開始，回應的 `truncated` 為 `true`、`earliest` 是期間開始；`to` 也不晚於期間開始時整個拒絕。

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 使用者沒有選租戶 | 403 `permission_denied` |
| 設備 ID 或查詢字串格式不對（缺 `tz`、未知的 `interval`、時間不是 RFC 3339） | 400 `invalid_request` |
| 參數不合規則：時區名稱不合格式或 ClickHouse 不認得、`from` 不早於 `to`、`point` 長度不在 1–128 | 422 `validation_failed`，`details.parameter` 指出參數 |
| 區間數或原始讀數太多 | 422 `validation_failed`、`telemetry.too_many_points`，`details.max` |
| 整個範圍早於可查期間 | 403 `quota_exceeded`、`telemetry.outside_queryable_period`，`details.earliest` |
| 租戶帳號還沒建立 | 404 `not_found`、`telemetry.account_not_ready` |
| 查詢超過帳號的列數或記憶體上限 | 422 `validation_failed`、`telemetry.query_too_heavy` |
| 帳號的 quota 或同時查詢數用完 | 429 `rate_limited` |
| ClickHouse 在時間上限中止查詢 | 504 `timeout` |
| 租戶帳號與服務建立的不同（租戶、角色、`readonly` 或查詢上限） | 503 `unavailable`，不執行查詢；日誌 ERROR 說明 |
| Postgres 或 ClickHouse 出錯 | 503 `unavailable` |
| 其他 | 200，`cache-control: no-store` |

每個查詢之前，服務先以租戶帳號登入確認帳號仍是它建立的樣子（`Accounts::verify`：與消費端第 2 步相同的檢查；不讀表，所以不計入 quota），不同就不執行查詢，所以帳號的查詢上限被改寬時不會被用到。查詢一律帶參數（`{name:Type}`，HTTP 介面的 `param_<name>`），除了 `wait_end_of_query` 不送任何 setting（唯讀帳號會拒絕），不用明確的 `PREWHERE`。每個查詢除了 row policy 外也自己加上 `tenant_id = 呼叫者租戶`，所以即使帳號被改成別的租戶，也回不出別的租戶的資料。ClickHouse 的錯誤內文從不回給用戶端，也不寫進日誌，日誌只記錯誤代碼。

## 資料庫

schema `telemetry_query`，DDL 在 `migrations/postgres/`，由 migration Job 套用（`tools/migrate/src/registry.rs` 的 `PRODUCT`：有平台角色、消費 `identity` 的事件）；檔案清單在 `src/schema.rs`，readiness 檢查同一份。

| 表 | 內容 | RLS |
| --- | --- | --- |
| `tenant_accounts` | `tenant_id`（主鍵）、`provisioned_at`：帳號已建好並確認過的租戶。`telemetry_query_app` 只有 `SELECT`、`INSERT`，平台角色讀不到 | `tenant_isolation` |
| `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version` | 消費端的記錄（Job 建立） | 無：不是租戶資料 |
| `trustiot_migrate_applied` | Job 套用過的檔案，服務角色只讀 | 無：不是租戶資料 |

## 設定

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 平台設定：驗證內部身分 token 的公鑰檔 |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 平台設定：Postgres 位置；只接受 `verify-full` |
| `TRUSTIOT_CLICKHOUSE_URL` | 平台設定：ClickHouse 的 HTTP 介面，`http://host:port`（chart 內建的 ClickHouse，走 mesh 的 mTLS）或 `https://host:port`（外部），不能帶帳密、路徑、查詢字串 |
| `TRUSTIOT_CLICKHOUSE_CA_FILE` | 平台設定：`https` 時必填，唯一信任的 CA |
| `TRUSTIOT__DATABASE__USERNAME`、`_PASSWORD`、`_PLATFORM_USERNAME`、`_PLATFORM_PASSWORD` | `telemetry_query_app` 與 `telemetry_query_platform` 的帳密，來自 Secret `<release>-telemetry-query-password` |
| `TRUSTIOT__CLICKHOUSE__USERNAME`、`TRUSTIOT__CLICKHOUSE__PASSWORD` | 服務帳號 `trustiot_telemetry_query` 與它的密碼（同一個 Secret 的 `trustiot_telemetry_query` key，至少 32 字元；它也是每個租戶密碼的推導 key）。chart 設定，`services.telemetry-query.env` 不能覆寫 |
| `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 服務角色連線池每個副本的上限，2–100；chart 以 `services.telemetry-query.database.maxConnections` 設定。連線池的大小或數量改變時，chart 的連線預算（`deploy/chart/templates/_postgres.tpl`，見 chart README 的「連線預算」）要一起改，`tests/chart_connections.rs` 比對兩者 |

ClickHouse 不是 readiness 檢查：它停機時 API 回 503、消費端重試。單一請求最多等 45 秒（`REQUEST_TIMEOUT`，大於租戶帳號的 30 秒上限，讓 ClickHouse 自己的逾時先發生）。

## 映像

`deploy/images/telemetry-query/image.toml`，與其他 Rust 服務相同（[deploy/images/README.md](../../deploy/images/README.md)）。chart 的 `services.telemetry-query.enabled` 預設 `false`；啟用時 chart 綁上 Postgres 角色、平台角色與 ClickHouse 服務帳號的密碼（`mesh.calls.telemetry-query` 是 `clickhouse`、`postgres`）。

## 測試

在 repo 根目錄執行：

```sh
CARGO_BUILD_JOBS=2 cargo test -p telemetry-query
```

整合測試需要 podman 或 docker：`testkit::product` 起帶 TLS 的 Postgres 並套用產品 schema，`services/ingest/tests/common` 起 ClickHouse（與 chart 相同的映像 digest），套用 `PRODUCT` 的 ClickHouse migration，再以 migration Job 的方式設定服務帳號的密碼，啟動真的 binary。沒有容器時略過，設了 `CI` 時改為失敗。

| 測試 | 驗證 |
| --- | --- |
| `tests/tenant_accounts.rs` | 驗收 1、2：建立兩個租戶後兩個帳號都存在，屬性（default role、hosts、沒有自己的 grant、`SQL_tenant_id`、`readonly`、上限）正確，各自只看得到自己的讀數；以租戶 A 的帳號跑 2026-10-09 PoC 的攻擊類型（基底表、JOIN、子查詢、UNION、CTE、FINAL、PREWHERE、table function、系統表、改 `SQL_tenant_id` 與限制的 setting、EXECUTE AS、URL 的 role、寫入、建帳號、改服務帳號、grant、刪 row policy），都被拒或回不出 B 與第三個租戶的讀數，資料沒被改；超過列數、記憶體、時間上限的查詢分別以 `TOO_MANY_ROWS`、`MEMORY_LIMIT_EXCEEDED`、`TIMEOUT_EXCEEDED` 中止；密碼被改時下一個 API 呼叫自動修好；每個 API 呼叫只算帳號 quota 的一個查詢（查詢前的帳號檢查不讀表、不計入，以 `system.quotas_usage` 確認）；帳號的查詢上限在帳號上被改寬（`readonly` 不變）時 API 回 503、不執行查詢，改回後恢復；帳號被改成別的租戶時 API 回 503、回不出別的租戶，重播事件時進 dead letter；日誌沒有任何密碼 |
| `tests/history.rs` | 驗收 3、4：序列與以另一組 ClickHouse 函式（`toStartOfHour`、`toStartOfDay`、`toStartOfFifteenMinutes`）直接查表的結果逐區間相同（Asia/Taipei、Asia/Kolkata、UTC；`15m`、`1h`、`3h`、`1d` 與自動選擇），區間開始落在當地邊界；整數、bool、文字資料點；`raw` 與 `latest` 等於儲存的讀數；同一個設備 ID 的另一個租戶的讀數不會出現；原始資料與聚合資料的期間截斷與拒絕；401、403、404、400、422 的各種拒絕不帶 ClickHouse 錯誤內文；quota 用完時 429 |
| `tests/cross_tenant.rs` | [crates/testkit](../../crates/testkit/README.md) 的跨租戶負向測試：服務角色讀不到也改不了其他租戶的 `tenant_accounts`；兩個 API 讀的是 ClickHouse，由上面兩個測試手動涵蓋，所以在套件中豁免 |
| `src/` 的單元測試 | 區間選擇、期間截斷、參數規則、讀數的 JSON 形狀、帳號名稱與密碼推導、建帳號的 SQL 只放進租戶 ID |
