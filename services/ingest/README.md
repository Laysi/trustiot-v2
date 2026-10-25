# ingest（資料接收）

閘道器的遙測資料經 MQTT 寫進 ClickHouse，整批落盤後才回確認（決策 8、清單 I1）。服務本身在 Laysi/trustiot-v2#91 實作；目前這個 crate 只放 ClickHouse 的 schema、保存期限的值（`src/retention.rs`）、彙總表的對帳與電能查詢語句（`src/rollup.rs`）與它們的測試（Laysi/trustiot-v2#59、Laysi/trustiot-v2#92、Laysi/trustiot-v2#141）。

## 閘道器的租戶歸屬

規則在[架構決策](../../docs/architecture/decisions.md)的決策 8、12 與清單 I1（2026-10-24 決定，Laysi/trustiot-v2#638），這裡只摘要 ingest 要做的事：

- 閘道器取自 topic（`trustiot/v1/gw/{gateway}/up/telemetry`）：broker 的 ACL 讓閘道器只能發布到以自己憑證 CN（硬體 ID）為名的 topic（[crates/protocol](../../crates/protocol/README.md) 的「Topic」「ACL」）。ingest 在叢集內連 broker，看不到閘道器的憑證，憑證裡也沒有租戶。
- 租戶取自 ingest 自己的綁定對照表：消費資產服務的 `assets.gateway_bound`（之後加上 Laysi/trustiot-v2#115 的解綁事件），對照表是該硬體 ID 目前的綁定（Laysi/trustiot-v2#640）。不在資料路徑上同步呼叫資產服務；訊息內容裡的任何租戶或綁定資訊都不採信。
- 不認得的閘道器（還沒綁定，或對照表還沒收到事件）回 `retry`、不寫入。
- 串流以「topic 的閘道器 + 串流 ID」為鍵（遙測規則 3）；完成紀錄記下寫入當時的租戶與 `binding_id`，供追查用（Laysi/trustiot-v2#639）。
- 改綁時閘道器一定換新的 SD 卡，新卡使用新的串流 ID，所以 ingest 不另外拒收舊綁定下採集的批次；這個前提與它的限制見決策 12。

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
| `0010_telemetry_rollups.sql` | 1 分鐘與 1 小時彙總表 `trustiot.telemetry_1m`、`telemetry_1h`，填入它們的 materialized view 與這些 view 的 DEFINER 帳號 `trustiot_rollup_definer`，租戶讀的 view `tenant_telemetry_1m`、`tenant_telemetry_1h`，以及補進既有資料與檢查一致性用的 backfill view（補資料由 migration Job 在套用之後執行；Laysi/trustiot-v2#141；見下方「彙總表」） |

檔案格式（migration Job 與測試都依賴）：

- 檔名 `NNNN_小寫底線.sql`，從 `0001` 連號，依檔名順序套用。
- 一個檔案可以有多條陳述式；以行首 `--` 開頭的行是註解；陳述式以行尾的 `;` 結束，其他地方（包含字串與註解欄位）不能出現 `;`。這樣不必解析 SQL 就能逐條以 HTTP 送出。
- 檔案裡可以有搬資料的 `INSERT ... SELECT`（`0010` 的 backfill），同樣要能重複執行：寫入的是與現狀的差，第二次什麼都不寫。Job 每個請求等 30 秒，所以這種陳述式要帶 `max_execution_time`（低於 30 秒），逾時的語句在伺服器上也停下，不會和下一次執行的同一句同時跑。
- 每條陳述式都要能重複執行：`CREATE` 一律 `IF NOT EXISTS`；不用 `OR REPLACE`（重建角色或 profile 會讓既有的授權失效）、`DROP`、`TRUNCATE`、`DETACH`、`EXCHANGE`。之後的變更寫成新的檔案，例如 `ALTER TABLE ... MODIFY TTL`；view 的欄位改變用 `ALTER TABLE <view> MODIFY QUERY`，因為 `CREATE VIEW IF NOT EXISTS` 不會改動已存在的 view。
- 表只用 MergeTree 系列引擎，保留搬到 ClickHouse Cloud（SharedMergeTree）的選項。view 與 materialized view 一律寫明 `DEFINER = <帳號> SQL SECURITY DEFINER`（`tests/migrations.rs` 檢查）：伺服器對一般 view 的預設是 `SQL SECURITY INVOKER`，讀 view 的帳號就得有底表的權限。

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

`tenant_id`、`gateway_id`、`stream_id`、`seq`、`row_count`（筆數）、`committed_at`（完成時間）。一批資料全部寫進 `telemetry` 並落盤後，ingest 才寫一筆完成紀錄，紀錄也落盤後才回確認；串流的高水位只取 `max(seq)`，不取自 `telemetry`（跨 partition 的 INSERT 不是原子的）。`ReplacingMergeTree`、`ORDER BY (stream_id, seq)`；同樣開 fsync；`TTL toDateTime(committed_at) + toIntervalDay(<天數>)`，天數與 `telemetry` 相同（由 migration Job 設定，見上一節），`committed_at` 不早於該批的 `received_at`，所以完成紀錄的保存期限不短於原始資料，測試會比對兩張表的 TTL 天數。測試資料的載入工具 `tools/lab-dataset` 也寫完成紀錄（只在 `src/clickhouse.rs` 的 `commit_batch` 一處，見 [datasets/lab/README.md](../../datasets/lab/README.md)「批次與完成紀錄」）：改這張表的鍵或欄位時（例如 Laysi/trustiot-v2#639）一併修改。

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

#### I1 重寫與對帳（Laysi/trustiot-v2#641 的前提）

目前還沒有程式寫入、刪除或重寫遙測列：ingest 的寫入路徑在 Laysi/trustiot-v2#91，I1 的重寫在 Laysi/trustiot-v2#641。Laysi/trustiot-v2#91 寫入串流的每一句 INSERT 都要照下面第 1 步帶 `stream_settings`，Laysi/trustiot-v2#641 必須照下面的步驟做，並且在彙總表（Laysi/trustiot-v2#141）合併之後才合併；同樣的步驟寫在 `src/rollup.rs` 的模組說明。

materialized view 只處理 INSERT。重寫一批（沒有完成紀錄的批次）時，前一次嘗試已寫進 ClickHouse 的列，刪掉之後仍算在彙總表裡；前一次的原始列寫入了、彙總那一份沒寫（兩者之間當機）時，彙總表又少算。一個串流的一批這樣重寫：

1. 這個串流的每一句寫入都帶 `stream_settings`：每次嘗試的 INSERT（包括第一次），以及下面第 2 到 4 步。它們用這個串流自己的 query ID（`stream_query_id`，`trustiot-rewrite/<閘道器>/<串流>`），設 `replace_running_query = 1`，`max_execution_time` 是 `STATEMENT_MAX_SECONDS`（20 秒）。前一次嘗試的某一句還在伺服器上執行時（client 已經放棄或當機），ClickHouse 26.8 在新的一句開始之前取消同一個帳號、同一個 query ID 的那一句，最多等 `replace_running_query_max_wait_ms`（`REPLACE_WAIT_MS`，5 秒）讓它結束；到時還沒結束，新的一句以 `QUERY_WITH_SAME_ID_IS_ALREADY_RUNNING` 失敗（舊的那句仍是取消狀態），別的帳號用同一個 query ID 時也一樣：這次嘗試停在這裡、不寫完成紀錄，之後重來。ingest 的 ClickHouse client 對這些語句的請求逾時至少要 `CLIENT_TIMEOUT_SECONDS`（30 秒），比等待加上執行的上限長，所以 client 放棄之前伺服器已經停下那一句。一個串流仍只由一個 ingest replica 處理；這一步確保前一次嘗試的語句不會和下一次重疊。
2. 同步刪除這批的列：`DELETE FROM trustiot.telemetry`，`lightweight_deletes_sync = 2`。
3. 以 `REWRITE_SETTINGS`（`deduplicate_insert = 'disable'`）寫入整批。開著去重時，內容與刪除之前寫過的區塊相同（同樣的列，或同樣的 `insert_deduplication_token`）的區塊會被直接丟掉、不報錯，這批的列就不見了。ClickHouse 26.8 的 `insert_deduplicate = 0` 關不掉它：設定 `deduplicate_insert`（預設 `enable`）取代了它，同步與非同步 INSERT 都一樣，只有設成 `backward_compatible_choice` 時才看 `insert_deduplicate`。
4. 以這批的參數（`ReconcileParams`）執行 `RECONCILE_1M` 與 `RECONCILE_1H`：對這批碰到的、這個串流的每一格，寫入從原始列重算的值與彙總表現在的值的差，一致時什麼都不寫。這兩句帶 `deduplicate_insert_select = 'disable'`，與之前某次相同的差值不會被當成重複丟掉。
5. 寫完成紀錄，再回確認。

- 第 5 步之前任何時候當機都沒有完成紀錄，閘道器會重送，下一次從第 1 步重來；對帳只看兩張表的現狀，重跑無害。只寫一次就成功的批次不需要對帳（view 在同一個 INSERT 裡執行），跑了也什麼都不寫。
- 對帳只信任原始那一側，同一個串流有其他寫入正在進行時不能對帳；一個串流一個 replica、一次只有一批在途（遙測規則 2）保證這點。
- 筆數與和精確修正；最小、最大、第一筆、最後一筆是從原始那一側合併進去，不會撤回。遙測規則 2 規定重送的批次內容不變，前一次嘗試的列就是重寫那批的列，不會改變這些值。
- 對帳只往回看 30 個收到日（`RECONCILE_WINDOW_DAYS`，等於閘道器緩衝的最長保存時間）：一批的每一次嘗試都在這段時間內收到，原始資料的保存期限又比它長（至少多一天，`RawRetention`），所以比較的原始列一定還在：窗口最舊的一天最早在下一個午夜才開始失去最早的列，而一句讀的是它開始時的 part。保存期限設成最小的 31 天時，餘裕只有這一天。
- ingest 角色在兩張彙總表只有 `SELECT, INSERT`，不能刪除或改表。

#### 既有資料的 backfill

`0010` 本身不寫入任何列。migration Job 套用完所有檔案、把各表的 TTL 對齊設定之後，才把套用之前就在 `telemetry` 裡的列補進彙總表（`tools/migrate` 的 `backfill_aggregates`，語句是 `src/rollup.rs` 的 `BACKFILL_1M`、`BACKFILL_1H`），完成後在 `trustiot_migrate.applied` 記一筆 `backfill/telemetry_rollups`，之後的執行不再補。先對齊 TTL 的理由：檔案以平台值（1 分鐘 13 個月）建表，設定比較長時，補進去的舊格會在對齊之前被 merge 依 13 個月刪掉。

- **只補彙總缺的格，不改任何已有的格**：backfill view `telemetry_1m_backfill`、`telemetry_1h_backfill`（以 `trustiot_rollup_definer` 執行，參數 `first_day`、`last_day`、`raw_retention_days`）列出從原始列重算的值與現有彙總不一致的格，`aggregated` 欄標出彙總裡有沒有這一格；Job 只寫入 `aggregated = 0` 的格。所以再補一次什麼都不寫，前一次失敗後再執行只補還缺的格；`trustiot_migrate.applied` 遺失（例如 ClickHouse 從沒有 `trustiot_migrate` 的備份還原）後重新套用檔案、再補一次，也不會拿掉或改動任何一格，即使原始資料已經過期。
- **只比對原始資料一定還在的收到日**：view 一律排除 `today − raw_retention_days + 2`（UTC）之前的收到日，原始與彙總兩邊都排除。原始 TTL 在收到後 `raw_retention_days` 天刪掉一列，所以 `today − raw_retention_days + 1` 是列還完整的最舊一天，它最早的列在下一個午夜過期；多留的一天讓在午夜前開始或跨過午夜的一句也碰不到執行中過期的列。更舊的格比原始列活得久（彙總保存得比較久），這種差異是預期的，view 不會把它列成要修正的差（寫進去會抹掉只剩彙總的歷史）。`raw_retention_days` 大於 3650 時以 3650 計，避免日期運算溢位，只會多排除一些日子。
- Job 傳的 `raw_retention_days` 是這次執行之前 `telemetry` 的 TTL 與設定值中較短的一個：這次縮短保存期限時，較舊的列正在過期；這次延長時，舊的較短期限已經刪掉的列不會回來。Job 只補 `today − raw_retention_days + 3` 到今天（`backfill_day_count`），比 view 的界限少一天：每一句依它開始時伺服器的日期算界限，執行途中過了午夜（UTC）界限就往後一天，多留的這一天讓列出的每一天仍在界限以內，同一天的 1 分鐘與 1 小時兩句補的是同樣的格。界限一個午夜只動一天，Job 至少每兩個請求逾時就往後補一天，之後的午夜追不上它。
- **平常沒有帳號讀得到 backfill view**：row policy `migration_account` 不點名任何帳號（`TO NONE`），管理帳號也讀到 0 列（`deny_by_default`）。Job 補之前把它改成點名自己（`TO CURRENT_USER`），補完改回 `TO NONE`（補失敗時也先改回再停止），所以換了管理帳號也照樣補得到。Job 在補的途中被停掉（例如超過 Job 的 `activeDeadlineSeconds`，或 pod 被刪）時來不及改回，policy 留在點名 Job 的帳號，直到下一次執行補完再改回；runbook 檢查步驟的最後一句看得出來。
- 一個收到日一句，每句 `max_execution_time = 25`，在 Job 30 秒的請求逾時之內：逾時的一句在伺服器上也停下，不會和下一次執行同時寫。原始保存期限 3650 天時最多 7,296 句（3,648 天各兩句）；大表上一天要多久還沒有實測，Laysi/trustiot-v2#91 之前也沒有持續寫入的資料。
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
| 角色 `trustiot_ingest` | `telemetry` 的 `SELECT, INSERT, ALTER DELETE`，`batch_commits` 與兩張彙總表的 `SELECT, INSERT`（彙總表給 I1 對帳）；沒有任何改欄位、TTL 或表設定的權限。ClickHouse 26.8 的 `DELETE FROM` 檢查的就是 `ALTER DELETE`（沒有另外的權限）。這個權限實際上還允許 `ALTER TABLE ... DELETE` 與分區管理語句：`DETACH PARTITION`、`DROP PARTITION`、`DROP DETACHED PARTITION`、`ATTACH PARTITION FROM`、`REPLACE PARTITION FROM`（2026-10-11 驗證報告的發現 6）。程式碼只用 `DELETE FROM`；服務帳號的密碼要當成能卸下或刪除整個分區的權限保管 |
| view `trustiot.tenant_telemetry` | `DEFINER = trustiot_tenant_view_definer SQL SECURITY DEFINER`；欄位同 `telemetry`，但不含只有 ingest 用的 `stream_id`、`batch_seq`。彙總的 `tenant_telemetry_1m`、`tenant_telemetry_1h` 同樣以這個帳號執行（上方「彙總表」） |
| 帳號 `trustiot_tenant_view_definer` | 三個租戶 view 的 DEFINER。`IDENTIFIED WITH no_password HOST NONE`：不能登入，沒有密碼要輪替；只有 `telemetry` 與兩張彙總表的 `SELECT` |
| 帳號 `trustiot_rollup_definer` | 填彙總表的 materialized view 與 backfill view 的 DEFINER。同樣 `no_password HOST NONE`；只有 `telemetry` 的 `SELECT` 與兩張彙總表的 `SELECT, INSERT`，以及它在這三張表上看得到全部列的 row policy `rollup_definer`。沒有這條 policy 時 `deny_by_default` 會讓 view 讀到 0 列：寫入照樣成功，彙總表卻什麼都沒加 |
| row policy `deny_by_default`（每張表與每個 view） | `USING 0 TO ALL`。ClickHouse 的伺服器預設 `users_without_row_policies_can_read_rows = true`：表上有 policy、但沒有 policy 適用於某帳號時，那個帳號看得到全部的列。這條讓每個帳號都至少有一條 policy，沒有被其他 policy 點名的角色或帳號一列也讀不到，不必改伺服器設定 |
| row policy `tenant_view_definer`（`telemetry` 與兩張彙總表） | `USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_view_definer`。租戶過濾真正生效的地方：DEFINER view 裡的查詢沿用呼叫者的設定，`SQL_tenant_id` 就是呼叫帳號的常數租戶 |
| row policy `tenant_isolation`（三個租戶 view、`telemetry` 與兩張彙總表） | `USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader`；沒有這個設定的帳號查詢時報錯（`UNKNOWN_SETTING`），不會拿到資料。view 上這條是第二層；表上這條在租戶角色沒有表權限時不起作用，只在萬一誤加了表權限時仍然過濾列 |
| row policy `service_all_rows`（`telemetry`、`batch_commits` 與兩張彙總表） | `USING 1 TO trustiot_ingest`；服務帳號另建的 policy |
| row policy `deny_by_default`（materialized view `telemetry_1m_mv`、`telemetry_1h_mv`） | 從 materialized view `SELECT` 時，ClickHouse 以 view 的 DEFINER 讀目標表，讀者在彙總表上的 policy 不適用：沒有這條，任何拿到整個資料庫 `SELECT` 的帳號（管理帳號也是）都能經 view 讀到每個租戶的彙總（2026-10-25 測試發現）。有了它所有帳號經 materialized view 都讀到 0 列，寫入不受影響 |
| row policy `migration_account`（backfill view） | `USING 1 TO NONE`：平常不點名任何帳號，管理帳號經 `deny_by_default` 也讀到 0 列。migration Job 補資料時、管理者依 runbook 檢查或修正時，暫時改成 `TO CURRENT_USER`，做完改回 `TO NONE`（上方「既有資料的 backfill」） |
| settings profile `trustiot_tenant_reader` | `readonly = 1` 與查詢上限（逾時 30 秒、記憶體 512 MiB、2 個 thread、同時 4 個查詢、讀取 10 億列、結果 100 萬列），全部 `CONST` |
| quota `trustiot_tenant_reader` | `KEYED BY user_name`：每分鐘 120 個查詢、60 個錯誤；每小時 3,000 個查詢、執行 1,800 秒、讀取 200 億列。沒有 `KEYED BY` 時所有租戶共用一個計數，A 用完會擋到 B |

profile 與 quota 的數字是例子，建立租戶時依方案訂實際值（Laysi/trustiot-v2#119）。

新增租戶只要兩句（Laysi/trustiot-v2#119 在建立租戶時執行；測試以同樣的語句建立租戶帳號），租戶帳號查詢三個租戶 view：

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
- **建帳號（Laysi/trustiot-v2#119）**：範本不變，仍是上面兩句，只是查詢對象改成 `trustiot.tenant_telemetry`。彙總表（Laysi/trustiot-v2#141）照這個做法，之後要給租戶讀的表也比照辦理：租戶角色只拿 DEFINER view 的 `SELECT`，租戶過濾寫成 DEFINER 帳號在表上的 row policy，migration Job 檢查租戶角色的權限正好是這些 view 的 `SELECT`（`tools/migrate` 的 `TENANT_VIEWS`）。

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

`tests/clickhouse.rs` 需要 ClickHouse：設了 `TEST_CLICKHOUSE_URL`（加上有 access management 權限的 `TEST_CLICKHOUSE_USER`、`TEST_CLICKHOUSE_PASSWORD`）就用它；否則以 podman 或 docker 起一個暫時的容器（記憶體上限 1500 MB，測完移除）；兩者都沒有時略過，但設了 `CI` 時改為失敗。映像與 `scripts/lib/stack.ts`、`deploy/chart/values.yaml` 固定同一個 digest，`bun test deploy/chart` 檢查三處一致。這個容器與 HTTP client 在 `tests/common/mod.rs`，`services/telemetry-query` 與 `tools/lab-dataset` 的測試也以 `#[path]` 引用它，改它時一併跑這兩個 crate 的測試。

測試內容：

1. migration 套用兩次（重複套用不出錯）。
2. 內省（Laysi/trustiot-v2#260）：測試列出 ClickHouse 26.8 全部 94 個 table function 的分類，伺服器有沒分類的函式、或外部來源類的函式變成唯讀帳號可用時失敗。指名表的 24 個函式，以及 `EXPLAIN indexes`、`EXPLAIN ESTIMATE`、讀取統計、`hasColumnInTable()`、`system.tables`／`system.columns` 的統計，都對每張表與三個租戶 view 各以租戶 B 的 ID 和不存在的 ID 執行：兩次的答案（含讀取列數）必須相同，對表必須被拒或沒有任何列。
3. 租戶帳號讀得到的系統表與 information_schema 表必須在 24 張的白名單內，內容不含 B 的資料；`system.tables` 只列出三個租戶 view。
4. 從伺服器讀回表設定：每張表都是 MergeTree 系列、開了 `fsync_after_insert` 與 `fsync_part_directory`、`telemetry` 以 `received_at` 分區、完成表的 TTL 天數不短於 `telemetry`；兩張彙總表是 `AggregatingMergeTree`，去重窗口同 `telemetry`，以 `received_day` 分區，TTL 是 13 與 48 個月；`trustiot_logs` 存在；MergeTree 以外只有三個租戶 view、兩個 materialized view 與兩個 backfill view。
5. 角色、row policy、profile、quota 與兩個 DEFINER 帳號全部存在 `local_directory`（以 SQL 建立）；租戶角色只有三個租戶 view 的 `SELECT`；租戶 view 的 DEFINER 只有 `telemetry` 與兩張彙總表的 `SELECT`，彙總的 DEFINER 只有 `telemetry` 的 `SELECT` 與彙總表的 `SELECT, INSERT`，兩者都不能登入；materialized view 與 backfill view 以彙總的 DEFINER 執行。
6. 租戶 A、B 經 view 各自讀到應有的列，服務帳號讀到全部；只有 `SELECT` 權限、沒有 policy 點名的角色讀到 0 列。
7. 以租戶 A 的帳號經 view 跑 2026-10-09 可行性測試的攻擊類型（兩個彙總 view 各跑一次同類的攻擊，A、B 的讀值在同一個設備、資料點與分鐘）（直接查、JOIN、子查詢、UNION、FINAL、merge()、view()、系統表）、改設定、表函式、冒用身分與寫入，以及 Laysi/trustiot-v2#59 驗證新增的攻擊；WHERE 裡的 `throwIf()` 不會碰到其他租戶的列。每一項都得到預期的答案或被拒絕，回應中從未出現 B 的租戶 ID 或 B 的資料。同一批攻擊改查 `telemetry` 時一律被拒。
8. 新帳號預設讀不到：沒有角色的帳號被拒，直接被授與 `SELECT` 的帳號、bootstrap 管理帳號、有租戶設定但不在租戶角色的帳號讀每張表、每個租戶 view 與 materialized view 都讀到 0 列或報錯（backfill view 只有套用檔案的管理帳號讀得到）；租戶帳號讀任何表、materialized view 或 backfill view 一律被拒；`merge('trustiot', '.*')` 被拒。
9. `SET ROLE NONE` 之後讀不到資料、查詢上限仍有效；沒有 `SQL_tenant_id` 設定的租戶帳號查詢時報錯。
10. 呼叫帳號的讀取列數與記憶體上限涵蓋 view 內讀的列；quota 把 view 讀的列算在呼叫帳號上。
11. ingest 帳號能寫入、以 `DELETE FROM` 刪除一批（租戶經 view 也看不到了）、寫完成紀錄並讀回高水位、讀寫彙總表；建表、改表、改 TTL、改表設定、刪表、清表、改 view、建帳號、改 policy、刪除彙總表的列一律被拒；constraint 擋下全零租戶與放錯欄位的值。
12. quota 以帳號分開計算：一個租戶帳號用完 quota 被擋下時，另一個照常查詢。
13. 日誌表：兩張都是 MergeTree，從伺服器讀回 `TTL ReceivedAt + toIntervalDay(30)` 與以 `ReceivedAt` 分區；以文件寫的兩句建立 collector 帳號，它能寫入兩張表並讀欄位（`DESC TABLE`），`ReceivedAt` 由伺服器填入；讀日誌、讀寫遙測表、建表、改 TTL、刪資料、清表、刪表、建帳號一律被拒。
14. 日誌與遙測的權限互不相通：租戶角色、ingest 角色與 view 的 DEFINER 在 `trustiot_logs` 沒有任何權限，collector 角色的權限只有兩張日誌表的 `INSERT`、`SHOW COLUMNS`（從 `system.grants` 讀回）；租戶帳號與 ingest 帳號讀、寫或查看日誌表一律被拒，collector 帳號讀 `telemetry` 與 `tenant_telemetry` 也被拒。

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

`tests/migrations.rs` 不需要伺服器：檢查檔名連號、陳述式切分、可重複執行、沒有破壞資料的語句、表只用 MergeTree 系列引擎，以及 view 一律是 `SQL SECURITY DEFINER`。
