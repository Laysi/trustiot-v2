# telemetry-query（遙測查詢服務）

設備圖表的歷史資料由這個服務查 ClickHouse 回答（決策 4、14；[部署與可搬移性](../../docs/architecture/deployment.md)）。Laysi/trustiot-v2#119（2026-10-21）建立：

- 消費身分服務的 `identity.tenant_created`，為每個新租戶建一個 ClickHouse 唯讀帳號（見[租戶帳號](#租戶帳號)）。
- 歷史查詢 API：一個資料點在一段時間內依時區降採樣的序列，與設備每個資料點最新的值（見 [API](#api)）。每個查詢都以呼叫者租戶的帳號執行，所以 ClickHouse 的 row policy、settings profile 與 quota 管住每一個查詢。
- AI 的唯讀 SQL：內部 endpoint `POST /internal/telemetry/v1/sql`，AI 助理以使用者的身分送來一個 `SELECT`，服務自己檢查後以呼叫者租戶的帳號執行（Laysi/trustiot-v2#143，見 [AI 的唯讀 SQL](#ai-的唯讀-sql)）。
- 可查期間先固定為最嚴格方案的例子（[方案](../../docs/product/plans.md) 的 Starter）：原始資料 7 天、聚合資料 3 個月，所有租戶相同，由 ClickHouse 依租戶帳號的設定強制（Laysi/trustiot-v2#143，見[可查期間](#可查期間)）；依方案區分的期間是 Laysi/trustiot-v2#453。

不包含：健康狀態歷史、首頁卡片、依方案的期間（Laysi/trustiot-v2#453）；AI 的語意工具（Laysi/trustiot-v2#142）；儀表板重跑 SQL 的呼叫端（Laysi/trustiot-v2#150）。

## 租戶帳號

`src/accounts.rs`。ClickHouse 這邊的物件都在 `services/ingest/migrations/clickhouse/`，由 migration Job 套用：

| 物件 | 來源 | 內容 |
| --- | --- | --- |
| 角色 `trustiot_tenant_reader` | `0004_access.sql`、`0005_tenant_view.sql`、`0010_telemetry_rollups.sql` | 只能 `SELECT` 三個 DEFINER view：原始資料的 `trustiot.tenant_telemetry` 與 1 分鐘、1 小時彙總的 `tenant_telemetry_1m`、`tenant_telemetry_1h`（Laysi/trustiot-v2#141，[services/ingest](../ingest/README.md)「彙總表」）；row policy 以帳號的 `SQL_tenant_id` 過濾，讀不到基底表 |
| settings profile `trustiot_tenant_reader` | `0004_access.sql` | `readonly = 1`、`max_execution_time = 30`、`max_memory_usage = 512 MiB`、`max_threads = 2`、`max_concurrent_queries_for_user = 4`、`max_rows_to_read = 1e9`、`max_result_rows = 1e6`，全部 `CONST` |
| quota `trustiot_tenant_reader` | `0004_access.sql` | `KEYED BY user_name`：每個租戶帳號每分鐘 120 個查詢、60 個錯誤，每小時 3000 個查詢、1800 秒執行時間、200 億列 |
| 服務帳號 `trustiot_telemetry_query` | `0008_telemetry_query.sql`、`0009_telemetry_query_grantees.sql` | 只能 `CREATE USER`、`ALTER USER` 名稱以 `trustiot_tenant_account_` 開頭的帳號，並以 `ADMIN OPTION` 持有租戶角色（才能把它設成新帳號的 default role）；`GRANTEES NONE`，所以不能把租戶角色授與或收回任何既有的使用者或角色；沒有其他權限、沒有 default role，讀不到任何表 |

服務帳號的密碼是內部密鑰，由 migration Job 產生在 Secret `<release>-telemetry-query-password` 的 `trustiot_telemetry_query` key 並設定（`TRUSTIOT__PASSWORDS__CLICKHOUSE_TELEMETRY_QUERY`，[tools/migrate](../../tools/migrate/README.md)）；沒有部署這個服務時，Job 讓這個帳號不能登入。Job 每次執行都檢查這個帳號只有 `0008_telemetry_query.sql` 給的權限，租戶角色只有三個租戶 view 的 `SELECT`，而且只有這個帳號（有 `ADMIN OPTION`）與租戶帳號（沒有）持有租戶角色，這個帳號的 `GRANTEES` 仍是 `NONE`，多出任何東西或 `GRANTEES` 被改就讓服務帳號不能登入（`HOST NONE`）並停止，修好之後的下一次執行再恢復登入。`GRANTEES NONE` 是因為 ClickHouse 26.8 不限制有 `ADMIN OPTION` 的帳號把角色給誰：少了它，服務帳號在啟用租戶角色的請求裡能把角色給任何使用者或角色（例如給 `trustiot_ingest` 會讓 ingest 變成唯讀、每個租戶都寫不進去），也能從租戶帳號收回；有了它，ClickHouse 拒絕這兩種語句，`CREATE USER ... DEFAULT ROLE trustiot_tenant_reader` 仍可用，服務帳號也改不了自己的 `GRANTEES`（`tools/migrate/tests/clickhouse_telemetry_query.rs`）。服務帳號仍能 `ALTER USER` 每個名稱在前綴下的帳號，包括 `RENAME` 與 `ADD IDENTIFIED`；Job 還不稽核租戶帳號本身（Laysi/trustiot-v2#528），懷疑服務帳號外洩時的處理在 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md)「懷疑時序查詢的服務帳號外洩」。

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

- `GET /api/telemetry/v1/devices/{device_id}/series?point=&from=&to=&tz=[&interval=]`：資料點 `point` 在 `[from, to)` 的序列。`from`、`to` 是 RFC 3339（`+08:00` 的 `+` 要編碼成 `%2B`，或用 `Z`）。`tz` 是 IANA 時區名稱（必填），小時與日的區間從該時區的整點與午夜開始；分鐘區間從 Unix epoch 起算（每個時區的偏移都是 15 分鐘的倍數，所以 15 分鐘區間也落在當地的刻鐘）。`interval` 是 `raw`、`1m`、`5m`、`15m`、`1h`、`3h`、`6h`、`12h`、`1d`；不給時取最多 1500 個區間（`MAX_BUCKETS`）的最細區間。每個區間回 `time`（區間開始，UTC）、`count`、`min`、`max`、`avg`（bool 當 0 或 1，文字讀數不算，全是文字時為 `null`）與 `last`（區間內最新的讀數，`{"type": "float|int|bool|string", "value": ...}`；`NaN` 與無限大是 `null`）。沒有讀數的區間不回。`raw` 最多 10000 筆（`MAX_RAW_READINGS`），每筆的 `count` 是 1。大於 2^53 的整數在 `min`、`max`、`avg` 會失去精度（`last` 不會）。聚合區間讀彙總 view（[services/ingest](../ingest/README.md)「彙總表」）：分鐘區間讀 1 分鐘格，小時與日區間讀 1 小時格；1 小時格從 UTC 整點開始，所以偏移不是整小時的時區（Asia/Kolkata、Australia/Adelaide）改讀 1 分鐘格，範圍改依 1 分鐘格的時間窗裁切（由 ClickHouse 判斷：格的開始在該時區取整點後是否不變）。每個區間是開始時間落在 `[from, to)` 的整格加總：`from` 或 `to` 不在格的邊界時，第一個區間從下一格開始，最後一個區間可能含到 `to` 之後一格內的讀數。`avg` 是各格總和相加後除以數值讀數數，與直接平均原始讀數相比，浮點數的最後幾位可能不同。這些差異是 2026-10-26 決定接受的（Laysi/trustiot-v2#143）。同一個變更也新增一種拒絕：`to` 晚於請求時的 366 天後（`MAX_RANGE_END_AHEAD`）時回 422、`details.parameter` 是 `to`，因為 1 分鐘與 1 小時格的時間是 ClickHouse 的 `DateTime`，只到 2106-02-07，更晚的值會靜靜溢位成空的回應，而 RFC 3339 的解析接受到 +262142 年；在任何時間運算之前就拒絕，所以不會溢位。
- `GET /api/telemetry/v1/devices/{device_id}/latest`：設備在原始資料期間內每個資料點最新的讀數，依資料點排序，最多 1000 個。

可查期間（`RAW_DAYS`、`AGGREGATED_MONTHS`；見[可查期間](#可查期間)）：`raw` 與 `latest` 是請求時往前 7 天，聚合序列是往前 3 個日曆月。`from` 早於服務的裁切起點時改成裁切起點，回應的 `truncated` 為 `true`、`earliest` 是裁切起點；`to` 也不晚於裁切起點時整個拒絕。每個區間依它讀的 view 套用該 view 的時間窗，裁切起點比資料庫的時間窗晚 5 分鐘，再進位到整毫秒（原始資料）、整分鐘（分鐘區間）或整小時（小時與日區間），所以小時與日區間的 `earliest` 最多比 3 個月前晚約 1 小時 5 分。自動選擇區間時，先把範圍裁切到兩個彙總 view 中較早的起點，再取能涵蓋整個範圍的 view 裡、區間數不超過上限的最細區間；都放不下時改從較晚的起點再選一次。

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 使用者沒有選租戶 | 403 `permission_denied` |
| 設備 ID 或查詢字串格式不對（缺 `tz`、未知的 `interval`、時間不是 RFC 3339） | 400 `invalid_request` |
| 參數不合規則：時區名稱不合格式或 ClickHouse 不認得、`from` 不早於 `to`、`to` 晚於請求時的 366 天後、`point` 長度不在 1–128 | 422 `validation_failed`，`details.parameter` 指出參數 |
| 區間數或原始讀數太多 | 422 `validation_failed`、`telemetry.too_many_points`，`details.max` |
| 整個範圍早於可查期間 | 403 `quota_exceeded`、`telemetry.outside_queryable_period`，`details.earliest` |
| 租戶帳號還沒建立 | 404 `not_found`、`telemetry.account_not_ready` |
| 查詢超過帳號的列數或記憶體上限 | 422 `validation_failed`、`telemetry.query_too_heavy` |
| 帳號的 quota 或同時查詢數用完 | 429 `rate_limited` |
| ClickHouse 在時間上限中止查詢 | 504 `timeout` |
| 租戶帳號與服務建立的不同（租戶、角色、`readonly` 或查詢上限），或可查期間的設定改不回服務的值 | 503 `unavailable`，不執行查詢；日誌 ERROR 說明 |
| Postgres 或 ClickHouse 出錯 | 503 `unavailable` |
| 其他 | 200，`cache-control: no-store` |

每個查詢之前，服務先以租戶帳號登入確認帳號仍是它建立的樣子（`Accounts::verify`：與消費端第 2 步相同的檢查；不讀表，所以不計入 quota），不同就不執行查詢，所以帳號的查詢上限被改寬時不會被用到。查詢一律帶參數（`{name:Type}`，HTTP 介面的 `param_<name>`），除了 `wait_end_of_query` 不送任何 setting（唯讀帳號會拒絕），不用明確的 `PREWHERE`。每個查詢除了 row policy 外也自己加上 `tenant_id = 呼叫者租戶`，所以即使帳號被改成別的租戶，也回不出別的租戶的資料。ClickHouse 的錯誤內文從不回給用戶端，也不寫進日誌，日誌只記錯誤代碼。

## AI 的唯讀 SQL

`src/sql/`、`src/sql_api.rs`（Laysi/trustiot-v2#143）：AI 助理（之後還有 Laysi/trustiot-v2#150 的儀表板）以使用者的身分送來一個 `SELECT`，服務自己檢查，再以呼叫者租戶的帳號執行，所以 row policy、可查期間、settings profile 與 quota 照樣管住它。請求與回應的型別在 [crates/contract](../../crates/contract/README.md) 的 `api::telemetry_query`。

- **路由**：`POST /internal/telemetry/v1/sql`，不在 `api/openapi.json`。入口 gateway 拒絕所有 `/internal*`（chart 的 `<release>-ingress-deny-internal`），mesh 只開放 assistant 呼叫這個服務（`mesh.calls`）。
- **呼叫者**：內部身分 token 必須是 token 交換換出的（有 `act`），`act` 在 `SQL_CALLERS` 裡（目前只有 `assistant`），而且帶租戶與這個版本認得的角色；viewer、editor、admin 都可以（2026-10-26 決定）。使用者自己經入口的請求沒有 `act`，回 403。
- **請求**：`{"sql": "...", "max_rows": 200}`。`sql` 最多 16 KiB（`MAX_SQL_BYTES`），整個 body 最多 64 KiB（`MAX_REQUEST_BYTES`，超過也當作 SQL 太長）；`max_rows` 是 1–10000（`MAX_ROWS`），不給時是 200（`DEFAULT_MAX_ROWS`，助理的工具固定送 200）。
- **檢查**是純函式、不做 I/O，被拒絕的語句不會到 Postgres 或 ClickHouse：
  1. 字元：反斜線、控制字元、非 ASCII 的空白與不可見的格式字元一律拒絕。
  2. token：只接受一般識別字（`[A-Za-z_][A-Za-z0-9_]*`，最多 128 bytes）、十進位數字、`'…'` 字串、`--` 與 `/* */` 註解和固定的運算子，最多 4096 個 token；第一個字是 `SELECT`、`WITH` 或 `(`，`;` 之後不能再有東西。
  3. 以 sqlparser 的 ClickHouse 方言解析（遞迴上限 50），只能是一個查詢。sqlparser 讀不成運算式的關鍵字（例如沒有 `WHEN` 的 `case`、沒有括號的 `cast`）會退回當成名稱，ClickHouse 不會這樣讀，所以名稱是這些關鍵字時當作解析不了（`syntax`）。
  4. 走訪語法樹，沒有列出的一律拒絕：只能讀三個租戶 view（`tenant_telemetry`、`tenant_telemetry_1m`、`tenant_telemetry_1h`，可省略 `trustiot.`）與查詢自己的 CTE，而且至少讀一個 view；不能有 `SETTINGS`、`FORMAT`、`INTO`、`FINAL`、`SAMPLE`、`PREWHERE`、`WITH TOTALS`、`WITH FILL`、table function 等；函式只能用白名單（`src/sql/functions.rs`，聚合函式可加 `If`、`OrNull`、`OrDefault`、`Distinct` 這幾種組合子）；分桶、格式化或讀取時間的函式要寫明時區字串，例如 `toStartOfDay(bucket, 'Asia/Taipei')`。
  5. 把檢查過的樹印回文字：去掉註解、view 補上 `trustiot.`、別名一律帶 `AS`、`CAST` 改成轉換函式、函式名稱改成 ClickHouse 定義的拼法，而且每個本身是運算的運算元都加上括號（`1 + 2 || 3` 印成 `1 + (2 || 3)`）：sqlparser 與 ClickHouse 的運算子優先順序不同，括號讓 ClickHouse 照檢查過的分組讀（`tests/sql_api.rs` 以 ClickHouse 的 `EXPLAIN AST` 比對）。同一串 `+`/`-`、`*`/`/`/`%`、`||`、`AND`、`OR` 不加括號，因為兩邊都由左往右讀。印出的文字再完整檢查一次，必須得到同一棵樹與同一段文字，然後加上 `LIMIT max_rows + 1` 與 `FORMAT JSONCompactEachRowWithNamesAndTypes`。送給 ClickHouse 的只有這段文字。加上的括號讓巢狀變深，所以印出的文字以較高的遞迴上限讀回，一條路徑上加上的括號最多 24 層（`MAX_ADDED_NESTING`）；超過時，或印出的文字超過 16 KiB 或 4096 個 token，都是查詢的問題，回 `too_complex` 或 `too_long`，不是 500。

  檢查在自己的執行緒上跑（16 MiB 的 stack），每個副本最多同時 4 個（`MAX_CONCURRENT_CHECKS`），其餘等待；呼叫端不再等待時，執行緒照樣佔著名額直到結束。
- **執行**：與歷史 API 相同，先確認租戶帳號已建立、仍是服務建立的樣子並帶著可查期間（`Accounts::verify`），再以租戶帳號執行；ClickHouse 的回答最多讀 8 MiB（`MAX_RESULT_BYTES`）。列數超過 `max_rows` 時整個拒絕（`too_many_rows`），不截斷，模型不會拿到不完整的答案還不知道。
- **回應**：200 `{"sql", "columns": [{"name", "type"}], "rows", "queryable_from": {"tenant_telemetry", "tenant_telemetry_1m", "tenant_telemetry_1h"}, "queried_at"}`。`sql` 是實際執行的文字（不含服務加上的 `LIMIT` 與 `FORMAT`）；`rows` 是 ClickHouse JSON 格式的值（pin 住的 ClickHouse 版本把 64 位元整數寫成 JSON 數字，時間寫成文字）；`queryable_from` 是每個 view 的 `Windows::earliest`：之前的列不在租戶的可查期間，之後 5 分鐘內的列可能有也可能沒有。
- **日誌**：SQL 是租戶的內容，日誌只記長度（`sql_bytes`）、SHA-256 的前 8 bytes（`sql_hash`，用來認出重複的 SQL）、拒絕類別與 ClickHouse 的錯誤代碼，不記 SQL 本身；回應也從不帶 SQL 或 ClickHouse 的錯誤內文。

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 沒有 `act`、`act` 不在 `SQL_CALLERS`、沒有租戶，或角色這個版本不認得 | 403 `permission_denied` |
| body 不是請求、`max_rows` 不在 1–10000 | 400 `invalid_request`，`details.parameter` |
| SQL 被檢查拒絕，或 ClickHouse 因查詢本身的問題拒絕 | 422 `validation_failed`，`details` 是拒絕（下表） |
| 租戶帳號還沒建立 | 404 `not_found`、`telemetry.account_not_ready` |
| 帳號的 quota 或同時查詢數用完 | 429 `rate_limited` |
| ClickHouse 在時間上限中止查詢 | 504 `timeout` |
| 帳號與服務建立的不同、Postgres 或 ClickHouse 出錯，或 ClickHouse 的錯誤代碼不在下表 | 503 `unavailable` |
| 檢查本身出錯，或帳號拒絕了通過檢查的查詢（`ACCESS_DENIED`、`READONLY`：檢查與帳號的權限不一致，要調查） | 500 `internal`，日誌 ERROR |

拒絕的 `details` 是 `{"class": ...}` 加上該類別的固定欄位（`SqlRefusal`）：

| `class` | 欄位 | 何時 |
| --- | --- | --- |
| `too_long` | `max_bytes` | SQL 超過 16 KiB，或印回的文字（補上 `trustiot.`、加上括號）超過 |
| `too_complex` | | token 太多或巢狀太深，包括印回時加上的 token 與括號 |
| `syntax` | `line`、`column`（知道時） | 解析不了；sqlparser 誤把關鍵字讀成名稱（第 3 步）；該寫名稱的地方寫了字串（`AS 'x'`、`WITH 'x' AS (…)`、`WINDOW 'w' AS (…)`，ClickHouse 不接受） |
| `character_not_allowed` | `line`、`column` | 檢查第 1 步的字元 |
| `literal_not_allowed` | `line`、`column` | 第 2 步以外的 token：引號識別字、其他形式的字串或數字、參數等 |
| `not_a_select` | | 不是 `SELECT` |
| `multiple_statements` | | 不只一個語句 |
| `table_not_allowed` | `table` | 三個 view 以外的表、table function、與 view 同名的 CTE |
| `function_not_allowed` | `function` | 白名單以外的函式 |
| `clause_not_allowed` | `clause` | 不允許的子句或語法，名稱是固定清單（`settings`、`format`、`final`、`prewhere`、`sample`、`with_totals`、`cast_type` 等） |
| `no_tenant_view` | | 沒有讀任何 view |
| `time_zone_required` | `function` | 時間函式沒有寫時區 |
| `too_many_rows` | `max` | 回答超過 `max_rows` 列 |
| `result_too_large` | `max_bytes` | 回答超過 8 MiB |
| `query_too_heavy` | | 超過帳號讀取列數、記憶體等上限 |
| `query_error` | `kind` | ClickHouse 執行時的錯誤，依錯誤代碼分成 `unknown_column`、`ambiguous_column`、`unknown_table`、`type_mismatch`、`bad_arguments`、`cannot_parse_value`、`aggregate_misuse`、`division_by_zero`、`syntax`、`other` |

`table` 與 `function` 是 SQL 裡通過識別字規則的名稱，其他欄位都是固定值，所以拒絕不會把任意文字帶回模型。新版本增加的類別、子句或 `kind`，舊的呼叫端解碼成 `unrecognized`。

## 可查期間

方案的可查期間由 ClickHouse 強制（Laysi/trustiot-v2#143，2026-10-26 決定；`services/ingest/migrations/clickhouse/0011_queryable_period.sql`，規則與部署、回復步驟見 [services/ingest](../ingest/README.md)「可查期間」）：租戶帳號帶三個 `CONST` 設定，一張表一個時間窗，三個租戶 view 背後的 row policy 濾掉窗外的列，不論查詢寫了什麼條件。這個服務只負責寫入與讀回時間窗（`src/period.rs`、`src/accounts.rs`）：

- **寫入**：建帳號時把 `Windows::PLATFORM`（原始資料 7 天、1 分鐘與 1 小時彙總各 3 個月）一起寫進 `CREATE USER`。
- **讀回與修正**：每個查詢前的帳號檢查（`Accounts::verify`）也讀回三個時間窗與 ClickHouse 的時鐘。缺少（`0011` 之前建立的帳號）或與服務的值不同時，以服務帳號 `ALTER USER ... MODIFY SETTINGS` 寫回（只改這三個設定）、再檢查一次，記一行 INFO `set the tenant account's queryable period`；寫回後仍不同時視為帳號被改過，回 503、記 ERROR、不執行查詢。所以方案變更（Laysi/trustiot-v2#453）只要改服務寫入的值，從帳號的下一個查詢起生效。
- **裁切**：API 自己裁切範圍時，起點是 `Windows::earliest`：時間窗起點加 5 分鐘（`PERIOD_MARGIN`），再進位到整毫秒、整分鐘或整小時。服務的時鐘與 ClickHouse 相差不到 5 分鐘時，裁切起點之後的每一列與每一格都在資料庫的時間窗內，所以回應的 `earliest` 之後不會因為時間窗而缺資料；相差超過 60 秒時記 WARN（`CLOCK_SKEW_WARNING`）。資料庫的時間窗才是邊界，裁切只讓回應說得出從哪裡開始是完整的。

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
| `tests/tenant_accounts.rs` | 驗收 1、2：建立兩個租戶後兩個帳號都存在，屬性（default role、hosts、沒有自己的 grant、`SQL_tenant_id`、`readonly`、上限）正確，各自只看得到自己的讀數；以租戶 A 的帳號跑 2026-10-09 PoC 的攻擊類型（基底表、JOIN、子查詢、UNION、CTE、FINAL、PREWHERE、table function、系統表、改 `SQL_tenant_id` 與限制的 setting、EXECUTE AS、URL 的 role、寫入、建帳號、改服務帳號、grant、刪 row policy），都被拒或回不出 B 與第三個租戶的讀數，資料沒被改；超過列數、記憶體、時間上限的查詢分別以 `TOO_MANY_ROWS`、`MEMORY_LIMIT_EXCEEDED`、`TIMEOUT_EXCEEDED` 中止；密碼被改時下一個 API 呼叫自動修好；每個 API 呼叫只算帳號 quota 的一個查詢（查詢前的帳號檢查不讀表、不計入，以 `system.quotas_usage` 確認）；帳號的查詢上限在帳號上被改寬（`readonly` 不變）時 API 回 503、不執行查詢，改回後恢復；帳號被改成別的租戶時 API 回 503、回不出別的租戶，重播事件時進 dead letter；帳號沒有時間窗設定時讀不到資料，時間窗被改成別的值時，下一個 API 呼叫都把它改回服務的值；租戶角色不再是帳號的 default role 時，每次呼叫都回 503 並記錯誤日誌，服務不自動改回，管理者改回後恢復；日誌沒有任何密碼 |
| `tests/queryable_period.rs` | Laysi/trustiot-v2#143 驗收 2：服務建好帳號後停掉，直接以租戶帳號登入（前面沒有服務、也沒有 SQL 檢查），只驗資料庫本身：9 條租戶 row policy 都帶各自表的時間窗；三個租戶 view 各自只顯示窗內的讀數與格；59 種通往窗外的查詢（條件、遮蔽 `ts`／`bucket`／`tenant_id` 的別名、JOIN、子查詢、CTE、集合運算、window function、table function、基底表）都回不出窗外或別的租戶的讀數，被拒的理由與記錄的相同；以 `SETTINGS`、URL、session 的 `SET` 放寬時間窗或換身分都被拒或無效；呼叫者的運算式不會作用在窗外的列（`throwIf`、轉型失敗）；時間窗起點隨伺服器時鐘移動；服務帳號改時間窗（全部放寬、只縮短 1 小時彙總、改回）從下一個查詢起生效；沒有時間窗或時間窗的值錯誤的帳號讀不到任何列 |
| `tests/history.rs` | 驗收 3、4：序列與以另一組 ClickHouse 函式（`toStartOfHour`、`toStartOfDay`、`toStartOfFifteenMinutes`）直接查原始表的結果逐區間相同（Asia/Taipei、Asia/Kolkata、UTC；`15m`、`1h`、`3h`、`1d` 與自動選擇），區間開始落在當地邊界；`from`、`to` 不在格邊界時，結果等於整格範圍的參考結果（UTC 與 Asia/Taipei 讀 1 小時格，Asia/Kolkata 改讀 1 分鐘格）；整數、bool、文字資料點；`raw` 與 `latest` 等於儲存的讀數；同一個設備 ID 的另一個租戶的讀數不會出現；原始資料與聚合資料的期間截斷與拒絕；`to` 超過一年後（+262142 年、2106-02-08）的 422；401、403、404、400、422 的各種拒絕不帶 ClickHouse 錯誤內文；quota 用完時 429 |
| `tests/sql_api.rs` | Laysi/trustiot-v2#143 驗收 1、3、4、5，與驗收 2 經過 endpoint 的部分，token 與 assistant 的 token 交換換出的相同（`act`）：模型會寫的查詢（`tests/data/model-queries.sql`）都回 200，回應的 `sql`、欄位型別與 `queryable_from` 正確；viewer、editor、admin 都可以，使用者自己的 token、別的服務、沒有租戶回 403，沒有 token 回 401，壞的 body 回 400，超過 64 KiB 的 body 回 `too_long`；每種拒絕類別只回固定的 `details`，沒有 SQL 或 ClickHouse 的錯誤內文；PoC 的攻擊類型（`tests/support/poc.rs`）與可查期間的路線（`tests/support/period.rs`）都被拒絕，或回不出期間外與別的租戶的讀數；同一段 SQL 以兩個租戶執行，各自只看到自己的讀數；不是 `SELECT` 的語句都沒有到 ClickHouse（`system.query_log` 裡沒有，租戶帳號只執行過 `SELECT`）；印出的文字與原文在 ClickHouse 的 `EXPLAIN AST` 相同；白名單的函式都在 pinned ClickHouse 的 `system.functions`，大小寫規則與種類相同 |
| `tests/cross_tenant.rs` | [crates/testkit](../../crates/testkit/README.md) 的跨租戶負向測試：服務角色讀不到也改不了其他租戶的 `tenant_accounts`；兩個公開 API 讀的是 ClickHouse，由 `tenant_accounts.rs` 與 `history.rs` 手動涵蓋，所以在套件中豁免；內部的 SQL endpoint 不在 OpenAPI 文件裡（套件只認得公開的操作），由 `sql_api.rs` 手動涵蓋（同一段 SQL 以兩個租戶執行） |
| `tests/sql_check.rs` | Laysi/trustiot-v2#143 的 SQL 檢查（不需要容器）：改 schema 的語句（`ALTER`、`DROP`、`TRUNCATE`、`CREATE`、`RENAME`、`EXCHANGE`、`ATTACH`、`DETACH`、`UNDROP`）不是查詢，接在查詢後面時是多個語句，寫在註解裡時不會出現在送出的文字；`SET ROLE`、`SET DEFAULT ROLE` 與其他不是一個 SELECT 的語句在解析前就被拒絕（lexer 本身與整個檢查）；長度、token 數與識別字長度的上限，過深、過長的查詢是 `too_complex` 而不會用完堆疊，在上限內的長查詢通過，檢查在自己的執行緒上執行；經服務的 router，被拒絕的語句、不能呼叫的呼叫者與不是請求的 body 連 Postgres 與 ClickHouse 都不會連（計數連線的本機埠），通過檢查的對照組會連到 Postgres，超過長度的 SQL 回 `too_long`。這些輸入放在 `tests/` 而不是 `src/`，因為 repo-check 拒絕 `services/*/src` 裡任何改 schema 的文字，sql-check 拒絕 `src` 裡的 `SET ROLE` 與以 `format!` 組出的 SQL（本檔在 sql-check 的例外清單上） |
| `src/` 的單元測試 | 區間選擇、期間截斷、參數規則、讀數的 JSON 形狀、帳號名稱與密碼推導、建帳號的 SQL 只放進租戶 ID；SQL 檢查（`src/sql/`）：模型會寫的查詢通過且印出的文字固定，字元、token、資料表、子句、函式、運算式與時區規則的拒絕與位置，列數上限的改寫；同時執行的檢查最多 `MAX_CONCURRENT_CHECKS`（4）個執行緒，呼叫者不再等待時執行緒仍佔著名額直到結束；SQL endpoint（`src/sql_api/tests.rs`）：只有在租戶內的 AI 服務能呼叫，ClickHouse 錯誤代碼對應的類別與狀態，拒絕的 body，回答的解析，日誌只有長度與雜湊；ClickHouse client 每次呼叫最多讀到它的上限 |
