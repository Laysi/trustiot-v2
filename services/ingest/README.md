# ingest（資料接收）

閘道器的遙測資料經 MQTT 寫進 ClickHouse，整批落盤後才回確認（決策 8、清單 I1）。服務本身在 Laysi/trustiot-v2#91 實作；目前這個 crate 只放 ClickHouse 的 schema 與它的測試（Laysi/trustiot-v2#59）。

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
| `0003_batch_commits.sql` | 舊的批次完成表 `trustiot.batch_commits`，`0010` 起停用（見下文） |
| `0004_access.sql` | 角色、row policy、settings profile、quota |
| `0005_tenant_view.sql` | 租戶唯一能讀的 view `trustiot.tenant_telemetry`、它的 DEFINER 帳號與 row policy（Laysi/trustiot-v2#260） |
| `0006_logs.sql` | OTel collector 寫入的日誌表 `trustiot_logs.otel_logs`、`otel_traces`，與 collector 的角色 `trustiot_otel_collector`（Laysi/trustiot-v2#73） |
| `0007_monitoring.sql` | 監控角色 `trustiot_monitoring`：只有 `system.disks` 的 `SELECT`，給磁碟水位告警（Laysi/trustiot-v2#92） |
| `0008_telemetry_query.sql` | telemetry-query 的服務帳號 `trustiot_telemetry_query`：只能建立與修改名稱以 `trustiot_tenant_account_` 開頭的帳號，並把租戶角色 `trustiot_tenant_reader` 給它們（Laysi/trustiot-v2#119；[services/telemetry-query](../telemetry-query/README.md#租戶帳號)） |
| `0009_telemetry_query_grantees.sql` | 服務帳號 `GRANTEES NONE`：它以 `ADMIN OPTION` 持有租戶角色，只能在 `CREATE USER ... DEFAULT ROLE` 時把角色給新建的帳號，不能把角色授與或收回任何既有的使用者或角色（Laysi/trustiot-v2#119 的安全審查） |
| `0010_gateway_stream_ids.sql` | `telemetry` 的 `gateway_id`、`stream_id` 改成文字（協定的 `GatewayId`、`StreamId`）並加上 ID 格式的 constraint；新的批次完成表 `trustiot.stream_commits`（以閘道器 + 串流為鍵）與它的權限、row policy；收回 ingest 對 `batch_commits` 的權限；以 `CREATE OR REPLACE VIEW` 重建租戶 view，欄位型別跟著改（Laysi/trustiot-v2#639） |

檔案格式（migration Job 與測試都依賴）：

- 檔名 `NNNN_小寫底線.sql`，從 `0001` 連號，依檔名順序套用。
- 一個檔案可以有多條陳述式；以行首 `--` 開頭的行是註解；陳述式以行尾的 `;` 結束，其他地方（包含字串與註解欄位）不能出現 `;`。這樣不必解析 SQL 就能逐條以 HTTP 送出。
- 每條陳述式都要能重複執行，而且不毀損資料。`tests/migrations.rs` 以允許清單檢查，只接受下列寫法，其他一律拒絕，訊息寫出檔名與陳述式；之後的變更寫成新的檔案：
  - `CREATE DATABASE`、`TABLE`、`VIEW`、`ROLE`、`USER`、`QUOTA`、`SETTINGS PROFILE`、`ROW POLICY`，一律 `IF NOT EXISTS`。不用 `OR REPLACE`：重建角色或 profile 會讓既有的授權失效。
  - `ALTER TABLE <表>` 只帶一個命令：`ADD COLUMN IF NOT EXISTS`、`ADD CONSTRAINT IF NOT EXISTS`、`MODIFY COLUMN`、`COMMENT COLUMN`、`MODIFY COMMENT` 或 `MODIFY TTL`；命令之後不能再出現另一個命令（例如 `DELETE`、`UPDATE`、`CLEAR`、`MOVE`、`REPLACE`、`PARTITION`、`MATERIALIZE`）、欄位 `TTL` 或 `REMOVE`。改型別的 `MODIFY COLUMN` 可能改寫或截斷已存的值，所以只接受 `tests/migrations.rs` 的 `LOSSLESS_TYPE_CHANGES` 列出的轉換（表、欄位、新型別與不會遺失資料的理由；目前是 `0010` 把 `telemetry` 的 `gateway_id`、`stream_id` 從 UUID 改成 String）；新的轉換要在那裡寫明理由，並加上讀回舊值的測試。不改型別、只改 comment、codec 或 default 的 `MODIFY COLUMN` 不受限。
  - `GRANT`、`REVOKE`、`ALTER USER`、`SET DEFAULT ROLE`。
  - 下一條所說的 `CREATE OR REPLACE VIEW`。
  - 任何陳述式在字串以外都不能有 `DROP`、`TRUNCATE`、`DETACH`、`EXCHANGE`、`RENAME`。所以 `REPLACE TABLE`、`RENAME TABLE`、`ALTER TABLE ... DELETE WHERE`、`CLEAR COLUMN`、搬移或取代 partition 都會被擋下。
- 唯一的例外：一般 view（不是 materialized view）的欄位改變用 `CREATE OR REPLACE VIEW`。只有在較早的檔案以 `CREATE VIEW IF NOT EXISTS` 建立過同名的 view，而且 `AS SELECT` 之前的文字（名稱、`DEFINER = <帳號>`、`SQL SECURITY DEFINER`）與原本的陳述式逐字相同時才放行，所以重建不能換 DEFINER，也不能「取代」一張表；comment 也照原本的寫。`CREATE VIEW IF NOT EXISTS` 不會改動已存在的 view，而 ClickHouse 26.8 拒絕對一般 view 執行 `ALTER TABLE ... MODIFY QUERY`（`NOT_IMPLEMENTED`，2026-10-24 實測；`0005_tenant_view.sql` 註解寫的做法不成立，已套用的檔案不能改）。一般 view 不存資料，授權與 row policy 依名稱掛在 view 上，重建後照樣生效；Atomic 資料庫裡重建是一次交換，查詢只會看到舊的或新的 view。這個做法假設 view 所在的資料庫是 Atomic 或 Replicated 引擎：`0001_databases.sql` 以伺服器的預設引擎建立 `trustiot`（26.8 的預設是 Atomic），舊的 Ordinary 資料庫裡重建不是原子的。
- 表只用 MergeTree 系列引擎，保留搬到 ClickHouse Cloud（SharedMergeTree）的選項。view 一律寫明 `DEFINER = <帳號> SQL SECURITY DEFINER`（`tests/migrations.rs` 檢查）：伺服器對一般 view 的預設是 `SQL SECURITY INVOKER`，讀 view 的帳號就得有底表的權限。

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
- ID：`gateway_id`、`stream_id` 在 `0002` 建表時是 UUID，`0010` 改成 String（兩欄都不在排序鍵，`MODIFY COLUMN` 原地改型別；已有的列改存 UUID 的文字形式，不遺失資料）。constraint `gateway_id_valid`、`stream_id_valid` 套用協定的 ID 規則（1–64 個 ASCII 英數字與 `.` `_` `-`，以英數字開頭），只檢查新寫入的列。`device_id` 維持 UUID：設備由資產服務以 UUID 建立，查詢端（telemetry-query、assistant）也以 UUID 解析。
- 保存期限：`TTL toDateTime(received_at) + toIntervalDay(<天數>)`，平台預設 180 天（`src/retention.rs`）。時鐘未校正的閘道器送來舊時間戳，也不會讓資料提早過期（[Laysi/trustiot#180](https://github.com/Laysi/trustiot/issues/180)）。`0002_telemetry.sql` 建表時寫的 400 天只是建表的初值（它開頭註解寫的「改天數要寫新的 migration」是這個做法之前的寫法，已套用的檔案不能改）：migration Job 每次執行都把 `telemetry` 與完成表 `stream_commits` 的 TTL 改成設定的天數（`TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS`，必須長於閘道器緩衝的最長保存時間 30 天），不另寫 migration 檔。值、限制與理由見[資料保存期限與磁碟水位](../../docs/architecture/data-retention.md)。
- 落盤：`fsync_after_insert = 1`、`fsync_part_directory = 1`，INSERT 成功代表資料已在磁碟上，不只在 page cache（2026-10-09 可行性測試：伺服器預設兩者皆為 0）。
- 去重：`non_replicated_deduplication_window = 1000`，帶相同 `insert_deduplication_token` 的重送區塊被丟棄；窗口只以區塊數計算，長時間離線的重送靠完成表 `stream_commits` 判斷。

### 批次完成表 `trustiot.stream_commits`

`0010_gateway_stream_ids.sql` 建立（Laysi/trustiot-v2#639）。一批資料全部寫進 `telemetry` 並落盤後，ingest 才寫一筆完成紀錄，紀錄也落盤後才回確認。

| 欄位 | 型別 | 說明 |
| --- | --- | --- |
| `tenant_id` | UUID | 寫入當時閘道器所屬的租戶，取自 ingest 的綁定對照表（見[閘道器的租戶歸屬](#閘道器的租戶歸屬)）；不能是全零 UUID（constraint `tenant_id_set`） |
| `binding_id` | UUID | 寫入當時生效的閘道器綁定，同樣取自綁定對照表，供追查；不能是全零 UUID（`binding_id_set`） |
| `gateway_id` | String | 協定的 `GatewayId`，取自 topic（`gateway_id_valid`） |
| `stream_id` | String | 協定的 `StreamId`，由閘道器產生（`stream_id_valid`） |
| `seq` | UInt64 | 批次序號，從 1 開始（`seq_positive`） |
| `row_count` | UInt64 | 這批寫進 `telemetry` 的筆數；重複的批次回 `duplicate` 時回報這個數 |
| `committed_at` | DateTime64(3, 'UTC') | 整批落盤的時間 |

給之後實作 ingest（Laysi/trustiot-v2#641）與部署（Laysi/trustiot-v2#642）的不變式：

- **鍵是閘道器 + 串流。** `ReplacingMergeTree`、`ORDER BY (gateway_id, stream_id, seq)`。協定的遙測規則 3：雲端以「topic 的閘道器 + 串流 ID」記錄高水位，串流 ID 由閘道器自己取，兩台閘道器可以用同一個串流 ID。高水位是 `SELECT max(seq) FROM trustiot.stream_commits WHERE gateway_id = <topic 的閘道器> AND stream_id = <串流>`，沒有紀錄時是 0（序號從 1 開始）；查詢落在排序鍵的前綴上。`gateway_id` 一律取自 topic，不取自 payload。
- **租戶與綁定只記錄、不當鍵。** 高水位屬於閘道器與它的串流，不因閘道器改綁別的租戶而重來；`tenant_id`、`binding_id` 記下這批是在哪個綁定下寫的。
- **高水位只取自這張表，不取自 `telemetry`。** 跨 partition 的 INSERT 不是原子的，`telemetry` 可能有沒完成的批次的一部分；沒有完成紀錄的批次以 `DELETE FROM trustiot.telemetry WHERE gateway_id = … AND stream_id = … AND batch_seq = …` 刪掉後重寫。
- **紀錄可能重複，讀的人不數筆數。** 同一個（閘道器、串流、序號）寫兩次時，`ReplacingMergeTree` 只在同一個 partition 內、而且要等 merge 才合併。`max(seq)` 與任一筆的 `row_count` 不受影響；不要以紀錄的筆數當成批次數。
- **落盤**：同樣開 `fsync_after_insert`、`fsync_part_directory`。
- **保存期限**：`TTL toDateTime(committed_at) + toIntervalDay(<天數>)`，天數與 `telemetry` 相同（migration Job 設定，見上一節）。`committed_at` 不早於該批的 `received_at`，所以完成紀錄不會比它描述的資料早過期，測試會比對兩張表的 TTL 天數。建表時是 3650 天（Job 接受的最長保存期限），在 Job 對齊之前也不短於 `telemetry`。
- **權限**：`trustiot_ingest` 只有 `SELECT, INSERT`，不能刪除或修改紀錄；row policy 同其他表（`deny_by_default`、`service_all_rows`）；租戶角色沒有任何權限。

**舊表 `trustiot.batch_commits` 停用。** `0003` 建的表 `ORDER BY (stream_id, seq)`，鍵裡沒有閘道器，兩台閘道器同一個串流 ID 與序號的紀錄會被合併，一台的高水位蓋掉另一台的，造成資料被當成重複而遺失；它的 `gateway_id`、`stream_id` 也是 UUID，寫不進硬體 ID。排序鍵的欄位不能改型別，migration 又不能刪表，所以改用新表：`0010` 收回 `trustiot_ingest` 對舊表的所有權限，row policy 留著（任何帳號仍然一列也讀不到），migration Job 不再管它的 TTL。到 `0010` 為止沒有任何服務寫過它，所以沒有要搬的高水位；萬一有紀錄，也只留在舊表，不再被讀取。之後要刪掉這張表時，由管理者手動執行或另訂規則，migration 檔不 `DROP`。

### 日誌表 `trustiot_logs.otel_logs`、`otel_traces`

OpenTelemetry collector（決策 22，`deploy/chart` 的 `observability.collector`）把服務遮蔽後的日誌與 trace 寫進這兩張表。欄位名稱與型別照 collector 的 ClickHouse exporter（opentelemetry-collector-contrib v0.161.0，chart 固定的版本）：exporter 關掉 `create_schema`，只送 `INSERT`，啟動時以 `DESC TABLE` 讀欄位；chart 測試比對 exporter 寫入的欄位都在表裡。升級 collector 而 exporter 多了欄位時，寫新的 migration（`ALTER TABLE ... ADD COLUMN IF NOT EXISTS`）。

- 與 exporter 預設 DDL 的差別：多一欄 `ReceivedAt`（`DEFAULT now()`，伺服器寫入的時間），分區與保存期限以它計算，發送端時鐘錯誤不會讓資料提早過期或永不過期（[Laysi/trustiot#180](https://github.com/Laysi/trustiot/issues/180)）；不建 Kubernetes resource attribute 的 materialized 欄位，也不建 trace id 查詢表與 materialized view，資料庫裡只有 MergeTree 表。
- 保存期限：`TTL ReceivedAt + INTERVAL 30 DAY`，`ttl_only_drop_parts = 1`（整個 part 過期才刪），`PARTITION BY toDate(ReceivedAt)`。改天數寫新的 migration（`ALTER TABLE ... MODIFY TTL`）。
- 角色 `trustiot_otel_collector`：兩張表的 `INSERT` 與 `SHOW COLUMNS`，沒有 `SELECT`，碰不到其他資料庫，也不能改表。collector 的帳號由 migration Job 以 chart 指定的 Secret 密碼建立，語句見 `0006_logs.sql` 的註解與 `deploy/chart/README.md`。讀日誌的帳號（Grafana）隨 Laysi/trustiot-v2#183 加入。

## 租戶隔離（row policy 與租戶 view）

帳號、角色、row policy、settings profile 與 quota 一律以 SQL 建立，存在 ClickHouse 自己的 access storage（`local_directory`），不寫伺服器設定檔（決策 14；ClickHouse Cloud 不能改設定檔）。唯一的例外是 bootstrap 管理帳號：映像依環境變數 `CLICKHOUSE_USER`、`CLICKHOUSE_PASSWORD_FILE`、`CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` 產生，相當於 ClickHouse Cloud 開好的 `default` 帳號，只給 migration Job 用。

租戶帳號只能讀 view `trustiot.tenant_telemetry`，`trustiot` 的表都沒有任何權限；理由與取捨見下一節「租戶讀取路徑」。

| 物件 | 內容 |
| --- | --- |
| 角色 `trustiot_tenant_reader` | 只有 `SELECT ON trustiot.tenant_telemetry`；每租戶唯讀帳號掛這個角色 |
| 角色 `trustiot_otel_collector` | 日誌表的 `INSERT, SHOW COLUMNS`，見上一節；在 `trustiot` 資料庫沒有任何權限 |
| 角色 `trustiot_monitoring` | 只有 `SELECT ON system.disks`，給評估磁碟水位告警的帳號（Grafana，Laysi/trustiot-v2#183）；讀不到任何 TrustIoT 資料庫或其他系統表 |
| 角色 `trustiot_ingest` | `telemetry` 的 `SELECT, INSERT, ALTER DELETE`，`stream_commits` 的 `SELECT, INSERT`，停用的 `batch_commits` 沒有任何權限（`0010` 收回）；沒有任何改欄位、TTL 或表設定的權限。ClickHouse 26.8 的 `DELETE FROM` 檢查的就是 `ALTER DELETE`（沒有另外的權限）。這個權限實際上還允許 `ALTER TABLE ... DELETE` 與分區管理語句：`DETACH PARTITION`、`DROP PARTITION`、`DROP DETACHED PARTITION`、`ATTACH PARTITION FROM`、`REPLACE PARTITION FROM`（2026-10-11 驗證報告的發現 6）。程式碼只用 `DELETE FROM`；服務帳號的密碼要當成能卸下或刪除整個分區的權限保管 |
| view `trustiot.tenant_telemetry` | `DEFINER = trustiot_tenant_view_definer SQL SECURITY DEFINER`；欄位同 `telemetry`（`gateway_id` 是 String），但不含只有 ingest 用的 `stream_id`、`batch_seq`。view 保留建立時的欄位型別，`CREATE VIEW IF NOT EXISTS` 也不改既有的 view，所以表的欄位型別改變時要以 `CREATE OR REPLACE VIEW` 重建（`0010`，見「檔案格式」）；DEFINER、row policy 與授權不變 |
| 帳號 `trustiot_tenant_view_definer` | view 的 DEFINER。`IDENTIFIED WITH no_password HOST NONE`：不能登入，沒有密碼要輪替；只有 `SELECT ON trustiot.telemetry` |
| row policy `deny_by_default`（三張表與 view） | `USING 0 TO ALL`。ClickHouse 的伺服器預設 `users_without_row_policies_can_read_rows = true`：表上有 policy、但沒有 policy 適用於某帳號時，那個帳號看得到全部的列。這條讓每個帳號都至少有一條 policy，沒有被其他 policy 點名的角色或帳號一列也讀不到，不必改伺服器設定 |
| row policy `tenant_view_definer`（`telemetry`） | `USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_view_definer`。租戶過濾真正生效的地方：DEFINER view 裡的查詢沿用呼叫者的設定，`SQL_tenant_id` 就是呼叫帳號的常數租戶 |
| row policy `tenant_isolation`（view 與 `telemetry`） | `USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader`；沒有這個設定的帳號查詢時報錯（`UNKNOWN_SETTING`），不會拿到資料。view 上這條是第二層；表上這條在租戶角色沒有表權限時不起作用，只在萬一誤加了表權限時仍然過濾列 |
| row policy `service_all_rows`（三張表） | `USING 1 TO trustiot_ingest`；服務帳號另建的 policy |
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
2. 內省（Laysi/trustiot-v2#260）：測試列出 ClickHouse 26.8 全部 94 個 table function 的分類，伺服器有沒分類的函式、或外部來源類的函式變成唯讀帳號可用時失敗。指名表的 24 個函式，以及 `EXPLAIN indexes`、`EXPLAIN ESTIMATE`、讀取統計、`hasColumnInTable()`、`system.tables`／`system.columns` 的統計，都對三張表與 view 各以租戶 B 的 ID 和不存在的 ID 執行：兩次的答案（含讀取列數）必須相同，對表必須被拒或沒有任何列。
3. 租戶帳號讀得到的系統表與 information_schema 表必須在 24 張的白名單內，內容不含 B 的資料；`system.tables` 只列出 view。
4. 從伺服器讀回表設定：`telemetry` 與 `stream_commits` 都是 MergeTree 系列、開了 `fsync_after_insert` 與 `fsync_part_directory`、`telemetry` 以 `received_at` 分區、完成表以 `(gateway_id, stream_id, seq)` 排序且 TTL 天數不短於 `telemetry`；閘道器與串流 ID 是 String、租戶與設備 ID 是 UUID；`trustiot_logs` 存在；MergeTree 以外只有租戶 view。
5. 角色、row policy、profile、quota 與 DEFINER 帳號全部存在 `local_directory`（以 SQL 建立）；租戶角色只有 view 的 `SELECT`，DEFINER 帳號只有 `telemetry` 的 `SELECT` 且不能登入；`0010` 重建 view 之後 DEFINER 與 comment 仍在，`gateway_id` 是 String，租戶經 view 讀到自己的文字閘道器 ID。
6. 租戶 A、B 經 view 各自讀到應有的列，服務帳號讀到全部；只有 `SELECT` 權限、沒有 policy 點名的角色讀到 0 列。
7. 以租戶 A 的帳號經 view 跑 2026-10-09 可行性測試的攻擊類型（直接查、JOIN、子查詢、UNION、FINAL、merge()、view()、系統表）、改設定、表函式、冒用身分與寫入，以及 Laysi/trustiot-v2#59 驗證新增的攻擊；WHERE 裡的 `throwIf()` 不會碰到其他租戶的列。每一項都得到預期的答案或被拒絕，回應中從未出現 B 的租戶 ID 或 B 的資料。同一批攻擊改查 `telemetry` 時一律被拒。
8. 新帳號預設讀不到：沒有角色的帳號被拒，直接被授與 `SELECT` 的帳號、bootstrap 管理帳號、有租戶設定但不在租戶角色的帳號都讀到 0 列或報錯；租戶帳號讀三張表一律被拒。
9. `SET ROLE NONE` 之後讀不到資料、查詢上限仍有效；沒有 `SQL_tenant_id` 設定的租戶帳號查詢時報錯。
10. 呼叫帳號的讀取列數與記憶體上限涵蓋 view 內讀的列；quota 把 view 讀的列算在呼叫帳號上。
11. ingest 帳號能寫入、以 `DELETE FROM` 依閘道器 + 串流 + 序號刪除一批（租戶經 view 也看不到了）、寫完成紀錄並讀回高水位；建表、改表、改 TTL、改表設定、刪表、清表、改 view、建帳號、改 policy、刪除或修改完成紀錄、讀寫停用的 `batch_commits` 一律被拒；constraint 擋下全零租戶、放錯欄位的值與不合協定規則的閘道器或串流 ID。ingest 角色的權限從 `system.grants` 讀回，正好是 `telemetry` 的 `SELECT, INSERT, ALTER DELETE` 與 `stream_commits` 的 `SELECT, INSERT`。
12. 完成紀錄以閘道器 + 串流為鍵（Laysi/trustiot-v2#639）：兩台閘道器用同一個串流 ID 與序號寫完成紀錄（其中一筆寫兩次），`OPTIMIZE ... FINAL` 合併之後各自的紀錄仍在、`max(seq)` 各自正確，只有同一台的重複紀錄被合併；紀錄帶著寫入時的租戶與 `binding_id`；兩台閘道器同串流同序號的遙測，刪掉其中一台的那批不影響另一台；完成紀錄的 ID、`binding_id` 與 `seq` 不合規則時被 constraint 擋下。
13. quota 以帳號分開計算：一個租戶帳號用完 quota 被擋下時，另一個照常查詢。
14. 日誌表：兩張都是 MergeTree，從伺服器讀回 `TTL ReceivedAt + toIntervalDay(30)` 與以 `ReceivedAt` 分區；以文件寫的兩句建立 collector 帳號，它能寫入兩張表並讀欄位（`DESC TABLE`），`ReceivedAt` 由伺服器填入；讀日誌、讀寫遙測表、建表、改 TTL、刪資料、清表、刪表、建帳號一律被拒。
15. 日誌與遙測的權限互不相通：租戶角色、ingest 角色與 view 的 DEFINER 在 `trustiot_logs` 沒有任何權限，collector 角色的權限只有兩張日誌表的 `INSERT`、`SHOW COLUMNS`（從 `system.grants` 讀回）；租戶帳號與 ingest 帳號讀、寫或查看日誌表一律被拒，collector 帳號讀 `telemetry`、兩張完成表與 `tenant_telemetry` 也被拒。

`tests/disk_watermark.rs`：磁碟水位告警（[`alerts/clickhouse_disk_watermark.sql`](alerts/clickhouse_disk_watermark.sql)，門檻與處理方式見[資料保存期限與磁碟水位](../../docs/architecture/data-retention.md#磁碟水位告警)）。一律以 podman 或 docker 起一個資料目錄是 256 MiB tmpfs 的暫時容器（不用 `TEST_CLICKHOUSE_URL`：共用伺服器的磁碟不是測試能填滿的），在容器裡填到 85 % 與 95 %，以監控角色的帳號評估規則，依序得到 `ok`、`warning`、`critical`，釋放後回到 `ok`；監控角色只有 `system.disks` 的 `SELECT`，讀寫遙測表、租戶 view、日誌表，改 TTL、建帳號一律被拒，沒有這個角色的帳號讀不到 `system.disks`。`cargo test -p ingest --test disk_watermark -- --nocapture` 印出每一步的磁碟用量。

`tests/migrations.rs` 的靜態檢查不需要伺服器：檢查檔名連號、陳述式切分、陳述式只用允許清單上的寫法（可重複執行、不毀損資料）、表只用 MergeTree 系列引擎，以及 view 一律是 `SQL SECURITY DEFINER`。另一個測試對 `0010_gateway_stream_ids.sql` 做升級比對，一律以 podman 或 docker 依序起兩個暫時的容器（不用 `TEST_CLICKHOUSE_URL`：要比對的是什麼都還沒套用過的伺服器）：一台全新套用全部檔案，另一台先套用 `0001`–`0009` 並以舊的形狀寫入一筆 UUID 閘道器與串流 ID 的遙測和一筆舊完成紀錄，再套用 `0010`；兩台各再套用 `0010` 一次，表、欄位、授權、角色授與與 row policy 都不變，而且兩台相同。升級的那台在背景改寫完舊的 part 之後，舊的遙測以 UUID 的文字形式讀回（也經租戶 view），ingest 角色讀不到舊完成表。
