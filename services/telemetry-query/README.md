# telemetry-query（遙測查詢服務）

設備圖表的歷史資料由這個服務查 ClickHouse 回答（決策 4、14；[部署與可搬移性](../../docs/architecture/deployment.md)）。Laysi/trustiot-v2#119（2026-10-21）建立：

- 消費身分服務的 `identity.tenant_created`，為每個新租戶建一個 ClickHouse 唯讀帳號（見[租戶帳號](#租戶帳號)）。
- 歷史查詢 API：一個資料點在一段時間內依時區降採樣的序列，與設備每個資料點最新的值（見 [API](#api)）。每個查詢都以呼叫者租戶的帳號執行，所以 ClickHouse 的 row policy、settings profile 與 quota 管住每一個查詢。
- 可查期間先固定為最嚴格方案的例子（[方案](../../docs/product/plans.md) 的 Starter）：原始資料 7 天、聚合資料 3 個月，所有租戶相同；依方案區分的期間是 Laysi/trustiot-v2#453。

Laysi/trustiot-v2#142（2026-10-26）加上語意查詢 API：多個資料點在使用者時區的相對期間內的統計與用量（用電量等累計值），給 AI 的語意工具與儀表板用（見[語意查詢](#語意查詢)）。

不包含：健康狀態歷史、首頁卡片、依方案的期間（Laysi/trustiot-v2#453）；AI 的 SQL 解析（Laysi/trustiot-v2#143）；異常區段、斷訊、停機與比較（Laysi/trustiot-v2#455）。

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

## 語意查詢

`src/semantic/`，Laysi/trustiot-v2#142（J3）。AI 的 `telemetry_statistics`、`telemetry_usage` 工具（[services/assistant](../assistant/README.md)「工具」）與儀表板的資料集用它：工具的輸入就是請求本文，存下來的呼叫之後以檢視者的身分重送，租戶、角色與可查期間照檢視者套用。

- `POST /api/telemetry/v1/statistics`：每個資料點的筆數、數值讀數筆數、最小與最大（附所在格的開始時間）、平均（bool 當 0 或 1，所以開關的平均是開著的比例）、第一與最後讀數；有 `interval` 時另有每個區間一列與平均最高、最低的區間。
- `POST /api/telemetry/v1/usage`：累計值（電能、水、瓦斯、脈衝、生產計數）的用量，見下方「用量」。

請求本文（`SeriesQuery`，未知欄位一律拒絕，沒有租戶或使用者欄位）：

| 欄位 | 規則 |
| --- | --- |
| `series` | 1 到 20 個不重複的 `{device_id, point}`；`point` 是 1 到 128 個英數字、`.`、`_`、`-`，以英數字開頭。回應依請求的順序 |
| `tz` | 必填，服務編入的 IANA 時區資料庫（jiff-tzdb）裡的名稱，大小寫要完全相同 |
| `period` | 相對期間，在 `tz` 裡依請求的時間解析：`today`、`yesterday`、`this_week`、`last_week`（週從星期一開始，ISO 8601）、`this_month`、`last_month`、`this_year`、`last_year`、`last_<N>_days`（含今天的 N 個當地日，N 是 1 到 366，從 N−1 天前的當地午夜到明天的當地午夜）、`last_<N>_hours`（到現在的 N 小時，N 是 1 到 72，現在取整到下一分鐘）。日曆期間涵蓋整段，還在進行的期間回應的 `ends_after_now` 為 `true` |
| `from`、`to` | 不給 `period` 時兩個都要給：RFC 3339，`[from, to)`，取整到分鐘 |
| `interval` | `1m`、`5m`、`15m`、`1h`、`1d`、`1w`（星期一開始）、`1mo`，預設 `total`（整段一個數字）；小時、日、週、月依 `tz` 的當地時間，分鐘區間從 Unix epoch 起算 |

回應的 `range` 是實際查詢的 `from`、`to`、`truncated`、`earliest`、`ends_after_now`、`resolution`（讀的表：`1m` 或 `1h`，也是最小與最大的時間精度）與 `queried_at`。回應裡每個時間都帶 `tz` 在那個時間的時差（例如 `+08:00`），模型不必換算時區。

### 讀哪一張表

`interval` 不是分鐘區間、而且從（截斷後的）`from` 到 `to` 在 `tz` 裡的每個區間邊界都落在 UTC 的整點時（`from`、`to` 是整點，範圍內 `tz` 用到的每個時差都是整小時），讀 1 小時表；否則讀 1 分鐘表，例如 Asia/Kolkata（+05:30）或 `last_24_hours`。兩張表的結果相同，只有最小與最大的時間精度不同。

這個選擇在查詢前以 jiff 與編入的時區資料庫決定，一次查詢。Laysi/trustiot-v2#143 的 SQL 歷史查詢則在 ClickHouse 裡決定（`toStartOfHour(bucket, tz) != bucket` 時改讀 1 分鐘表），兩者不同是因為語意查詢一個請求只送一個語句、而且要先知道讀哪張表才能算可查期間的開始與 1 分鐘格數上限。代價是 jiff 與 ClickHouse 的時區資料庫版本不同時，某個時區剛改過的時差可能讓兩邊判斷不一致；兩邊對照的測試列在後續工作。

### 可查期間的邊界

每張表的可查期間從 `S = 取整到該表的格（現在 − 3 個日曆月 + 5 分鐘）` 開始（`range::queryable_start`），只讀 `bucket >= S` 的格。多加的 5 分鐘與向上取整讓這個邊界永遠不早於 Laysi/trustiot-v2#143 在資料庫裡的過濾（`bucket >= now − 3 個日曆月`），請求在路上多花的時間也不會讀到期間之外。`from` 早於 `S` 時改成 `S`，`truncated` 為 `true`、`earliest` 是 `S`；`to` 不晚於 1 分鐘表的 `S` 時整個拒絕（403 `telemetry.outside_queryable_period`，`details.earliest` 是 `S`，帶 `tz` 的時差）。用量往前找基準讀數時也不超過 `S`，所以被截斷的範圍沒有 `baseline`。

### 用量

用量是相鄰兩筆讀數的增量加總。彙總表每格保留第一與最後讀數、最小與最大，服務在 ClickHouse 裡逐格計算（`statements.rs` 的 `usage!`）：

- 格內（兩筆以上讀數）：`最後 − 第一`；格內下降過（`最後 < 最大` 或 `最小 < 第一`）時是 `(最大 − 第一) + 最後`：先漲到峰值，再從 0 起算。
- 跨格：`這格第一 − 上一格最後`；下降時是 `這格第一`：下降當成重置或回捲、從 0 重新開始。
- 每個增量算在後一筆讀數所在的格，所以各區間加起來等於總量。
- 只用第一與最後讀數是 int 或 float、四個值都是有限數字的格（`skipped_cells` 是其他的格數）。
- 範圍前 24 小時內（不早於可查期間）最後一筆讀數是基準（`baseline`），第一個區間的用量從它算起；沒有基準時，範圍內第一筆讀數之前的用量不算。
- 沒有讀數的區間不回（未知，不是 0）；斷訊（讀數間隔不小於該序列中位數間隔的 1.5 倍）後的用量算在斷訊後第一個區間，該列 `after_gap` 為 `true`，序列有 `gaps` 與最長的 `longest_gap`。

精度的限制：

- 每次下降最多少算一個讀數間隔的用量（重置前最後一筆到重置之間的增量）。32 位元計數器回捲時也是如此：例如 fixture 的 `pulses` 算出 3048，考慮回捲是 3099。
- 1 小時表一格裡有下降時，`(最大 − 第一) + 最後` 只有在下降後的值都低於峰值時才等於逐筆的結果。一小時內的誤讀（例如讀到 0 或讀到半個 32 位元的值）在 1 小時表與 1 分鐘表可能算出不同的總量，兩者都會被標成可疑跳增（見下方）。
- 大於 2^53 的整數換成浮點數，會失去精度。

### 累計檢查

只有像計數器的資料點才算用量。檢查讀的是用量往前後各找 24 小時的整個視窗（`check.from`、`check.to`），在比較（格內一次、跨格一次）中數下降（`drops`）、上升（`rises`）與比較次數（`comparisons`）：

- 下降不超過 2 次，或不超過上升次數的 2%（`COUNTER_MAX_DROPS`、`COUNTER_MAX_DROP_PERCENT`），是計數器；
- 是計數器但比較少於 10 次（`MIN_COMPARISONS`）：`verdict` 是 `too_few_readings`，用量照算，工具請模型說明數字未經確認；
- 否則 `not_cumulative`：不回用量，工具請模型改用統計，不要把讀數相加。

fixture 的結果（1 分鐘表）：`energy_kwh` 下降 1 次、上升 46 次、比較 47 次，`pulses` 1、45、46，`cycle_count` 0、37、41，都是計數器；兩個 `power_kw` 是 26、21、47 與 26、20、46，`not_cumulative`。1 小時表上 fixture 只有 4 小時、7 次比較：三個計數器是 `too_few_readings`，兩個 `power_kw`（6、1 與 5、2）仍是 `not_cumulative`。

已知的限制：

- 鋸齒形的瞬時值（例如慢慢注滿、很快放空的水位）下降次數少、上升次數多，可能被當成計數器。
- 閒置的計數器在視窗裡重置 3 次以上、上升又很少時會被拒絕。
- 判斷依整個視窗，不知道設備型號；型號上標明累計值的做法是後續工作。

### 可疑跳增

比平常快很多的增量被標成可疑跳增，總量不變（照上面的規則照算），工具請模型一併說明總量可能含錶的誤讀：

- 速率是增量除以時間（跨格是兩筆讀數的間隔，格內是格內第一到最後讀數的時間），時間至少取該序列的中位數間隔；
- 速率超過視窗內跨格上升速率中位數的 20 倍（`SUSPICIOUS_FACTOR`）就可疑；視窗內至少要有 10 次跨格上升（`MIN_RISES_FOR_SUSPICION`）才判斷。所以最小的可疑增量約是一個正常讀數間隔用量的 20 倍；
- 下降後當成從 0 起算的增量也照樣判斷，所以讀到 0 之後回到原值、讀到半個 32 位元的值、換上一顆從 5000 開始的新錶，都會被標出；重置或回捲後的值很小，不會被標出；長時間斷訊後以平常速率累積的大增量，因為除以斷訊的時間，也不會被標出。

每個序列回可疑跳增的次數（`suspicious_steps`）與最大的一次（`largest_suspicious_step`，時間與增量），每列也有該區間的次數。

### 上限與錯誤

- 每次最多 20 個資料點、10,000 列（資料點數乘區間數，以名目長度計算，每個資料點加 2；超過時 422 `telemetry.too_many_points`，`details.parameter` 是 `interval`）。
- 讀 1 分鐘表的用量最多讀 1,000,000 格（資料點數乘視窗的分鐘數；超過時 422 `telemetry.query_too_heavy`，`details.parameter` 是 `series`）：改用整點的範圍與整小時的時區就會讀 1 小時表，或減少資料點、縮短範圍。
- 本文不是描述的 JSON 物件或有多的欄位：400 `invalid_request`、`telemetry.query_invalid`。欄位不合規則：422 `validation_failed`，`details.parameter` 是 `series`（`telemetry.series_invalid`、`telemetry.point_invalid`）、`tz`、`period`（`telemetry.period_invalid`）或 `from`。其他錯誤與歷史 API 相同（上表）。
- 每個請求一個語句，帶參數、以租戶帳號執行，與歷史 API 相同；語句另加 `tenant_id = 呼叫者租戶`，資料點以 `Array(Tuple(UUID, String))` 參數比對。

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
| `tests/semantic.rs` | Laysi/trustiot-v2#142 的驗收：以 [datasets/lab](../../datasets/lab/README.md) 的資料集（預設是合成 fixture，`TRUSTIOT_LAB_DATASET_INDEX` 可換成 lab 錄的資料集）照 ingest 的寫法載入兩個租戶，統計（UTC、Asia/Taipei 讀 1 小時表，Asia/Kolkata 讀 1 分鐘表；`15m`、`1h`、`1d`、`1w`、`1mo`、`total`）與用量（另有從第二個小時開始、有 `baseline` 的範圍）逐區間等於測試自己從讀數算出的結果（不經服務的 SQL，依上面的規則逐格計算）；fixture 的重置與 32 位元回捲各一次、不可疑，瞬時值是 `not_cumulative`、開關與文字是 `not_numeric`，漏讀的 35 分鐘是斷訊；最新值等於最後的讀數；租戶 A 問租戶 B 的設備是 `no_data`，本文帶租戶欄位回 400；惡意的資料點、時區、期間在 API 被拒，直接以租戶帳號執行服務的語句時被當成值、比對不到任何列；跨過可查期間開始的資料集被截斷（`truncated`、沒有 `baseline`、檢查視窗從期間開始），整段在之前時 403；為規則寫的序列：讀到 0、讀到半個 32 位元的值、換錶各被標成可疑跳增而總量照算，長時間斷訊後以平常速率累積的增量不可疑，閒置在 0、跳三次的瞬時值是 `not_cumulative` |
| `tests/cross_tenant.rs` | [crates/testkit](../../crates/testkit/README.md) 的跨租戶負向測試：服務角色讀不到也改不了其他租戶的 `tenant_accounts`；四個 API 讀的是 ClickHouse，由上面的測試手動涵蓋，所以在套件中豁免 |
| `src/` 的單元測試 | 區間選擇、期間截斷、參數規則、讀數的 JSON 形狀、帳號名稱與密碼推導、建帳號的 SQL 只放進租戶 ID；語意查詢：時區名稱、每種期間的寫法與在 Asia/Taipei、Asia/Tokyo、America/New_York（日光節約）的解析、可查期間的邊界與取整、表的選擇、截斷與拒絕、列數與 1 分鐘格數上限、累計檢查在邊界上的判定、語句只用參數、回應的組合、規格裡兩組區間的名稱不相撞 |
