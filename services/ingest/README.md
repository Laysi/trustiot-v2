# ingest（資料接收）

閘道器的遙測資料經 MQTT 寫進 ClickHouse，整批落盤後才回確認（決策 8、清單 I1）。服務本身在 Laysi/trustiot-v2#91 實作；目前這個 crate 只放 ClickHouse 的 schema 與它的測試（Laysi/trustiot-v2#59）。

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

檔案格式（migration Job 與測試都依賴）：

- 檔名 `NNNN_小寫底線.sql`，從 `0001` 連號，依檔名順序套用。
- 一個檔案可以有多條陳述式；以行首 `--` 開頭的行是註解；陳述式以行尾的 `;` 結束，其他地方（包含字串與註解欄位）不能出現 `;`。這樣不必解析 SQL 就能逐條以 HTTP 送出。
- 每條陳述式都要能重複執行：`CREATE` 一律 `IF NOT EXISTS`；不用 `OR REPLACE`（重建角色或 profile 會讓既有的授權失效）、`DROP`、`TRUNCATE`、`DETACH`、`EXCHANGE`。之後的變更寫成新的檔案，例如 `ALTER TABLE ... MODIFY TTL`；view 的欄位改變用 `ALTER TABLE <view> MODIFY QUERY`，因為 `CREATE VIEW IF NOT EXISTS` 不會改動已存在的 view。
- 表只用 MergeTree 系列引擎，保留搬到 ClickHouse Cloud（SharedMergeTree）的選項。view 一律寫明 `DEFINER = <帳號> SQL SECURITY DEFINER`（`tests/migrations.rs` 檢查）：伺服器對一般 view 的預設是 `SQL SECURITY INVOKER`，讀 view 的帳號就得有底表的權限。

### 遙測窄表 `trustiot.telemetry`

一筆讀值一列，欄位固定：

| 欄位 | 型別 | 說明 |
| --- | --- | --- |
| `tenant_id` | UUID | 租戶，取自閘道器的連線憑證，不採信資料內容；不能是全零 UUID（constraint） |
| `gateway_id` | UUID | 送出這批的閘道器 |
| `stream_id`、`batch_seq` | UUID、UInt64 | 這列來自哪個串流的哪一批；沒有完成紀錄的批次以這兩欄刪掉已寫入的列後重寫（I1） |
| `device_id` | UUID | 設備 |
| `point` | LowCardinality(String) | 資料點在設備型號中的 key（C1），1–128 字元 |
| `ts` | DateTime64(3, 'UTC') | 閘道器的讀值時間，毫秒，UTC |
| `received_at` | DateTime64(3, 'UTC') | 伺服器收到的時間，由 ingest 填入；分區與保存期限都以它計算 |
| `value_type` | Enum8 | `float`、`int`、`bool`、`string`，指出值在哪一欄 |
| `value_float`、`value_int`、`value_bool`、`value_string` | Float64、Int64、Bool、String | 各型別的值；其他欄維持預設值（constraint `value_matches_type`） |

- `ORDER BY (tenant_id, device_id, point, ts)`，`PARTITION BY toYYYYMM(received_at)`。
- 保存期限：`TTL toDateTime(received_at) + INTERVAL 400 DAY`。時鐘未校正的閘道器送來舊時間戳，也不會讓資料提早過期（[Laysi/trustiot#180](https://github.com/Laysi/trustiot/issues/180)）。400 天是暫定值，實際天數由 Laysi/trustiot-v2#92 訂；改值寫成新的 migration，`batch_commits` 一起改。
- 落盤：`fsync_after_insert = 1`、`fsync_part_directory = 1`，INSERT 成功代表資料已在磁碟上，不只在 page cache（2026-10-09 可行性測試：伺服器預設兩者皆為 0）。
- 去重：`non_replicated_deduplication_window = 1000`，帶相同 `insert_deduplication_token` 的重送區塊被丟棄；窗口只以區塊數計算，長時間離線的重送靠 `batch_commits` 判斷。

### 批次完成表 `trustiot.batch_commits`

`tenant_id`、`gateway_id`、`stream_id`、`seq`、`row_count`（筆數）、`committed_at`（完成時間）。一批資料全部寫進 `telemetry` 並落盤後，ingest 才寫一筆完成紀錄，紀錄也落盤後才回確認；串流的高水位只取 `max(seq)`，不取自 `telemetry`（跨 partition 的 INSERT 不是原子的）。`ReplacingMergeTree`、`ORDER BY (stream_id, seq)`；同樣開 fsync；`TTL toDateTime(committed_at) + INTERVAL 400 DAY`，`committed_at` 不早於該批的 `received_at`，所以完成紀錄的保存期限不短於原始資料，測試會比對兩張表的 TTL 天數。

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

`tests/migrations.rs` 不需要伺服器：檢查檔名連號、陳述式切分、可重複執行、沒有破壞資料的語句、表只用 MergeTree 系列引擎，以及 view 一律是 `SQL SECURITY DEFINER`。
