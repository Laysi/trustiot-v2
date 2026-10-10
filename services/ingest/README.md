# ingest（資料接收）

閘道器的遙測資料經 MQTT 寫進 ClickHouse，整批落盤後才回確認（決策 8、清單 I1）。服務本身在 Laysi/trustiot-v2#91 實作；目前這個 crate 只放 ClickHouse 的 schema 與它的測試（Laysi/trustiot-v2#59）。

## ClickHouse schema

DDL 是 `migrations/clickhouse/` 的 SQL 檔，部署時由 migration Job（Laysi/trustiot-v2#60）以 bootstrap 管理帳號依檔名順序套用。執行期程式碼不建表、不改表（決策 14、[Laysi/trustiot#103](https://github.com/Laysi/trustiot/issues/103)）：`tools/repo-check` 掃描 `services/*/src`，出現 `CREATE`／`ALTER`／`DROP`／`RENAME`／`TRUNCATE`／`ATTACH`／`DETACH`／`EXCHANGE` 加 `TABLE`／`DATABASE`／`VIEW`／`DICTIONARY` 的陳述式就失敗；資料庫端也只給服務帳號讀寫與刪列的權限，改 schema 的語句一律被拒。刪除一批不完整的資料用 `DELETE FROM`（lightweight delete），不用 `ALTER TABLE ... DELETE`。

| 檔案 | 內容 |
| --- | --- |
| `0001_databases.sql` | `trustiot`（遙測與批次完成表）、`trustiot_logs`（服務與閘道器日誌，給 OTel collector，Laysi/trustiot-v2#73；獨立資料庫，可以分開決定保存與搬遷） |
| `0002_telemetry.sql` | 遙測窄表 `trustiot.telemetry` |
| `0003_batch_commits.sql` | 批次完成表 `trustiot.batch_commits` |
| `0004_access.sql` | 角色、row policy、settings profile、quota |

檔案格式（migration Job 與測試都依賴）：

- 檔名 `NNNN_小寫底線.sql`，從 `0001` 連號，依檔名順序套用。
- 一個檔案可以有多條陳述式；以行首 `--` 開頭的行是註解；陳述式以行尾的 `;` 結束，其他地方（包含字串與註解欄位）不能出現 `;`。這樣不必解析 SQL 就能逐條以 HTTP 送出。
- 每條陳述式都要能重複執行：`CREATE` 一律 `IF NOT EXISTS`；不用 `OR REPLACE`（重建角色或 profile 會讓既有的授權失效）、`DROP`、`TRUNCATE`、`DETACH`、`EXCHANGE`。之後的變更寫成新的檔案，例如 `ALTER TABLE ... MODIFY TTL`。
- 只用 MergeTree 系列引擎，保留搬到 ClickHouse Cloud（SharedMergeTree）的選項。

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

## 租戶隔離（row policy）

帳號、角色、row policy、settings profile 與 quota 一律以 SQL 建立，存在 ClickHouse 自己的 access storage（`local_directory`），不寫伺服器設定檔（決策 14；ClickHouse Cloud 不能改設定檔）。唯一的例外是 bootstrap 管理帳號：映像依環境變數 `CLICKHOUSE_USER`、`CLICKHOUSE_PASSWORD_FILE`、`CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1` 產生，相當於 ClickHouse Cloud 開好的 `default` 帳號，只給 migration Job 用。

| 物件 | 內容 |
| --- | --- |
| 角色 `trustiot_tenant_reader` | `SELECT ON trustiot.telemetry`；每租戶唯讀帳號掛這個角色 |
| 角色 `trustiot_ingest` | `telemetry` 的 `SELECT, INSERT, ALTER DELETE`，`batch_commits` 的 `SELECT, INSERT`；沒有任何改欄位、TTL 或表設定的權限。ClickHouse 26.8 的 `DELETE FROM` 檢查的就是 `ALTER DELETE`（沒有另外的權限），它也允許 `ALTER TABLE ... DELETE`；程式碼只用 `DELETE FROM`，由 `tools/repo-check` 把關 |
| row policy `deny_by_default`（兩張表） | `USING 0 TO ALL`。ClickHouse 的伺服器預設 `users_without_row_policies_can_read_rows = true`：表上有 policy、但沒有 policy 適用於某帳號時，那個帳號看得到全部的列。這條讓每個帳號都至少有一條 policy，沒有被其他 policy 點名的角色一列也讀不到，不必改伺服器設定 |
| row policy `tenant_isolation`（`telemetry`） | `USING tenant_id = toUUID(getSetting('SQL_tenant_id')) TO trustiot_tenant_reader`；沒有這個設定的帳號查詢時報錯（`UNKNOWN_SETTING`），不會拿到資料 |
| row policy `service_all_rows`（兩張表） | `USING 1 TO trustiot_ingest`；服務帳號另建的 policy |
| settings profile `trustiot_tenant_reader` | `readonly = 1` 與查詢上限（逾時 30 秒、記憶體 512 MiB、2 個 thread、同時 4 個查詢、讀取 10 億列、結果 100 萬列），全部 `CONST` |
| quota `trustiot_tenant_reader` | `KEYED BY user_name`：每分鐘 120 個查詢、60 個錯誤；每小時 3,000 個查詢、執行 1,800 秒、讀取 200 億列。沒有 `KEYED BY` 時所有租戶共用一個計數，A 用完會擋到 B |

profile 與 quota 的數字是例子，建立租戶時依方案訂實際值（Laysi/trustiot-v2#119）。

新增租戶只要兩句（Laysi/trustiot-v2#119 在建立租戶時執行；測試以同樣的語句建立租戶帳號）：

```sql
CREATE USER <name> IDENTIFIED WITH sha256_password BY '<password>'
    SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '<tenant UUID>' CONST;
GRANT trustiot_tenant_reader TO <name>;
```

profile 同時掛在帳號與角色上：帳號在 session 裡 `SET ROLE NONE` 會失去 `SELECT` 權限，查詢上限仍然有效。quota 掛在角色上，`SET ROLE NONE` 之後不再計數，但這時帳號已讀不到任何表，查詢上限也仍然限制它能用的資源。

已知的限制：

- 有 `SELECT` 權限的租戶帳號從 `system.tables` 的 `total_rows` 與 `EXPLAIN ESTIMATE` 看得到整張表（所有租戶）的總筆數，看不到任何一列的內容。
- ClickHouse 26.8 的 quota 不計算沒有讀表的常數查詢（例如 `SELECT 1`）。

## 測試

在 repo 根目錄執行：

```sh
# 全部：靜態檢查 migration 檔，再對真的 ClickHouse 套用並跑租戶隔離測試
CARGO_BUILD_JOBS=2 cargo test -p ingest

# 印出每個攻擊與結果
CARGO_BUILD_JOBS=2 cargo test -p ingest --test clickhouse -- --nocapture
```

`tests/clickhouse.rs` 需要 ClickHouse：設了 `TEST_CLICKHOUSE_URL`（加上有 access management 權限的 `TEST_CLICKHOUSE_USER`、`TEST_CLICKHOUSE_PASSWORD`）就用它；否則以 podman 或 docker 起一個暫時的容器（記憶體上限 1500 MB，測完移除）；兩者都沒有時略過，但設了 `CI` 時改為失敗。映像與 `scripts/lib/stack.ts`、`deploy/chart/values.yaml` 固定同一個 digest，`bun test deploy/chart` 檢查三處一致。

測試內容：

1. migration 套用兩次（重複套用不出錯）。
2. 從伺服器讀回表設定：兩張表都是 MergeTree 系列、開了 `fsync_after_insert` 與 `fsync_part_directory`、`telemetry` 以 `received_at` 分區、完成表的 TTL 天數不短於 `telemetry`；`trustiot_logs` 存在。
3. 角色、row policy、profile、quota 全部存在 `local_directory`（以 SQL 建立）。
4. 租戶 A、B 與服務帳號各自讀到應有的列；只有 `SELECT` 權限、沒有 policy 點名的角色讀到 0 列。
5. 以租戶 A 的帳號跑 2026-10-09 可行性測試的攻擊類型（直接查、JOIN、子查詢、UNION、FINAL、merge()、view()、系統表），再加上改設定、表函式、冒用身分與寫入：每一項都得到預期的答案或被拒絕，回應中從未出現 B 的租戶 ID 或 B 的資料。
6. `SET ROLE NONE` 之後讀不到資料、查詢上限仍有效。
7. 沒有 `SQL_tenant_id` 設定的租戶帳號查詢時報錯。
8. ingest 帳號能寫入、以 `DELETE FROM` 刪除一批、寫完成紀錄並讀回高水位；建表、改表、改 TTL、改表設定、刪表、清表、建帳號、改 policy 一律被拒；constraint 擋下全零租戶與放錯欄位的值。
9. quota 以帳號分開計算：一個租戶帳號用完 quota 被擋下時，另一個照常查詢。

`tests/migrations.rs` 不需要伺服器：檢查檔名連號、陳述式切分、可重複執行、沒有破壞資料的語句，以及只用 MergeTree 系列引擎。
