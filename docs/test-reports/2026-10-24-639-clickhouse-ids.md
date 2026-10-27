# ingest：ClickHouse 的閘道器與串流 ID，以閘道器加串流為鍵的批次完成表

對應 Laysi/trustiot-v2#639。

這份報告記錄 2026-10-24 在建置主機上，以真的 ClickHouse 容器執行 `services/ingest/migrations/clickhouse/0010_gateway_stream_ids.sql` 與相關測試的結果。這個檔案在 2026-10-27 合併 main 時改名為 `0012_gateway_stream_ids.sql`，合併後的結果見 `2026-10-27-639-merge-main.md`。

## 環境

- 程式碼：分支 `feat/639-ingest-clickhouse-ids`，基於 main `0dc8fd96`，之後合併 main 的 `1ad0ffc5`（含 Laysi/trustiot-v2#638 的租戶歸屬決策）。下方的資料檔與整個 workspace 的結果是合併之後重跑的。
- 建置主機（x86_64 Linux）：Rust 的 fmt、clippy 與測試。容器測試起暫時的 ClickHouse（映像以 digest 釘選，伺服器版本 26.8.22.13）與 Postgres；設定 `CI=1`，所以沒有容器執行環境時會失敗而不是略過。每次在工作樹的快照上執行。

## 指令

```sh
# 與這次變更相關的容器測試（輸出存成下方的資料檔）
CI=1 cargo test --locked -p ingest --test migrations -- --nocapture
CI=1 cargo test --locked -p ingest --test clickhouse -- --nocapture
CI=1 cargo test --locked -p migrate --test clickhouse -- --nocapture

# 整個 workspace
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
CI=1 cargo test --locked --workspace --no-fail-fast
```

## 結果

資料檔在 `data/2026-10-24-639-clickhouse-ids/`：

- [`cargo-test-ingest-migrations.txt`](data/2026-10-24-639-clickhouse-ids/cargo-test-ingest-migrations.txt)：`services/ingest/tests/migrations.rs` 的輸出（7 個測試通過）。
- [`cargo-test-ingest-clickhouse.txt`](data/2026-10-24-639-clickhouse-ids/cargo-test-ingest-clickhouse.txt)：`services/ingest/tests/clickhouse.rs` 的輸出（1 個測試通過，含所有權限探測）。
- [`cargo-test-migrate-clickhouse.txt`](data/2026-10-24-639-clickhouse-ids/cargo-test-migrate-clickhouse.txt)：`tools/migrate/tests/clickhouse.rs` 的輸出（1 個測試通過）。

### 驗收條件 1：全新與已套用 0001–0009 的資料庫，套用兩次結果相同

測試 `id_migration_gives_one_schema_on_fresh_and_upgraded_servers` 依序起兩個 ClickHouse 容器：

- 全新：套用 0001–0010，再套用 0010 一次，前後的結構快照（`system.tables`、`system.columns`、`trustiot_*` 角色與帳號的 `system.grants` 與 `system.role_grants`、`system.row_policies`）相同。
- 升級：只套用 0001–0009，寫入一列舊型態（UUID 的 `gateway_id`、`stream_id`）telemetry 與一筆 `batch_commits` 紀錄，再套用 0010 兩次；兩次快照相同，且與全新的快照相同。

快照的內容在 `cargo-test-ingest-migrations.txt` 第 11 行起。舊資料在升級後以 UUID 的文字讀回，型別為 `String`，經租戶 view 也一樣：

```text
legacy reading after the upgrade: c67b970c-7bcd-44ba-b2aa-20dd08516016	203f5e7b-cce6-408e-94fa-c3d2f3c5d078	String	String	42
through the tenant view: c67b970c-7bcd-44ba-b2aa-20dd08516016	String	42
```

### 驗收條件 2：兩台閘道器相同串流 ID 與 seq 的完成紀錄互不覆蓋

`completion_records_are_per_gateway_and_stream` 讓閘道器 A 在 `stream-1` 寫 seq 1–3（seq 2 寫兩次），閘道器 B 在同一個 `stream-1` 寫 seq 1–5，`OPTIMIZE ... FINAL` 之後依閘道器加串流彙總：

```text
completion records per gateway of stream stream-1 (gateway, max(seq), records, rows):
gw-a-7b1308193870	3	3	60
gw-b-7b1308193870	5	5	1500
telemetry rows of batch 6 after deleting gateway A's: gw-b-7b1308193870	4
```

兩台的 max(seq) 各自正確，A 重複寫入的 seq 2 合併成一筆而 B 的紀錄不受影響；沒有紀錄的閘道器 C 的 max(seq) 為 0。兩台都寫了 batch 6 的 telemetry 列，以閘道器限定的刪除只刪掉 A 的，B 的 4 列還在。

### 驗收條件 3：權限

`trustiot_ingest` 的權限恰好是下面 5 行（`ingest_grants_are_exact`），對 `batch_commits` 沒有任何權限，也沒有角色授權：

```text
ALTER DELETE	trustiot	telemetry
INSERT	trustiot	stream_commits
INSERT	trustiot	telemetry
SELECT	trustiot	stream_commits
SELECT	trustiot	telemetry
```

ingest 帳號對 `stream_commits` 的 DELETE、ALTER DELETE、UPDATE、MODIFY TTL、TRUNCATE，以及對 `batch_commits` 的 SELECT、INSERT、DELETE 都被拒。租戶帳號讀 `stream_commits`、`batch_commits`（含 `mergeTreeIndex()` 與 `SHOW CREATE TABLE`）都得到 `ACCESS_DENIED`；租戶 view 的欄位是 `tenant_id, gateway_id, device_id, point, ts, received_at, value_type, value_float, value_int, value_bool, value_string`，沒有 `stream_id`、`batch_seq`，租戶只讀到自己的列（`cargo-test-ingest-clickhouse.txt` 的 direct 探測）。`services/telemetry-query/tests/tenant_accounts.rs` 也加了讀 `stream_commits` 的攻擊並通過。

### 驗收條件 4：migration Job 的保存天數

`tools/migrate/tests/clickhouse.rs` 在全新的 ClickHouse 上執行 Job，`stream_commits` 從建立時的 3650 天改成設定的 180 天；改成 365 天時先改完成表再改 telemetry，改回 180 天時順序相反，兩表天數一致。Job 不再碰退役的 `batch_commits`：

```text
365 days: ["set the retention of ClickHouse table trustiot.stream_commits to 365 days (was 180)", "set the retention of ClickHouse table trustiot.telemetry to 365 days (was 180)"]
back to 180 days: ["set the retention of ClickHouse table trustiot.telemetry to 180 days (was 365)", "set the retention of ClickHouse table trustiot.stream_commits to 180 days (was 365)"]
```

### 驗收條件 5：整個 workspace

- `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings`：通過。
- 第一次 `cargo test --locked --workspace` 只有 `tools/sql-check` 的 `the_workspace_has_no_string_built_sql` 失敗：升級測試以 `format!` 組出 `CREATE USER`（密碼在執行時產生，DDL 不能綁定）與舊形狀的 `INSERT`。比照同目錄的 `services/ingest/tests/clickhouse.rs`，把 `services/ingest/tests/migrations.rs` 加入 sql-check 的例外清單，只免除 `SqlBuiltFromStrings`。
- 修正並合併 main 之後重跑 `cargo test --locked --workspace --no-fail-fast`：193 個測試套件全部通過，共 1493 個測試通過、0 個失敗，其中包括 `clickhouse_schema_and_tenant_isolation`、`id_migration_gives_one_schema_on_fresh_and_upgraded_servers`、`clickhouse_ddl_and_the_collector_account` 與 `the_workspace_has_no_string_built_sql`。

## 結論

- 驗收條件 1，通過：0010 在全新與已套用 0001–0009 的資料庫上都可以重複套用，結果一致，舊資料的 UUID 轉成文字後保留。
- 驗收條件 2，通過：完成紀錄以 `(gateway_id, stream_id, seq)` 為鍵，兩台閘道器共用串流 ID 時互不覆蓋，高水位依閘道器加串流各自計算。
- 驗收條件 3，通過：`trustiot_ingest` 對新完成表只有 SELECT、INSERT，對退役的 `batch_commits` 沒有權限；租戶仍只能讀 view 上自己的列，看不到 `stream_id`、`batch_seq`。
- 驗收條件 4，通過：migration Job 讓 telemetry 與 `stream_commits` 的 TTL 天數一致。
- 驗收條件 5，通過：fmt、clippy 與整個 workspace 的測試通過（修正 sql-check 的例外之後）。
- 與 issue 範圍的差異：ClickHouse 26.8 對一般 view 拒絕 `ALTER TABLE ... MODIFY QUERY`（`Alter of type 'MODIFY_QUERY' is not supported by storage View`，NOT_IMPLEMENTED），所以租戶 view 改以 `CREATE OR REPLACE VIEW` 重建，定義者、欄位與註解與 0005 相同；靜態檢查只對一般 view 的這個寫法放行。
