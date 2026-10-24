# sql-check

靜態檢查：workspace 裡的 SQL 不能由字串組成，也不能在 service-kit 的租戶模組以外設定租戶（[決策 29](../../docs/architecture/decisions.md)）。以 `cargo test` 執行，不產生執行檔。

執行：`cargo test -p sql-check`（`cargo test --workspace` 也會跑到，CI 因此擋下違規）。

## 為什麼

RLS 防的是程式漏寫租戶條件，不防 SQL 注入：注入的 SQL 以服務自己的角色執行，可以改掉 `trustiot.tenant_id` 切換租戶。所以服務只用參數化查詢（sqlx 的編譯期巨集，或靜態 SQL 加 `.bind()`、QueryBuilder 的 `push_bind`），值一律綁定，不拼進 SQL 字串。sqlx 0.9 本身已經拒絕把執行期的 `String` 當 SQL；本檢查擋下繞過它的寫法，以及從 SQL 切換租戶或角色的寫法。

## 規則

| 規則 | 擋下 | 改成 |
| --- | --- | --- |
| `DynamicSql` | `sqlx::query`、`query_as`、`query_scalar`、`query_with`、`query_as_with`、`query_scalar_with`、`raw_sql` 的 SQL 不是字串字面值、`include_str!`、只由字面值組成的 `concat!`（可以再包 `concat!` 與 `stringify!`；含 `env!` 或巨集參數就不算）或常數（帶模組的 `SCREAMING_CASE` 路徑，例如 `queries::LIST`、`Self::LIST`；或檔案裡以 `const`、`static` 宣告或以 `use` 引入、而且沒有被當成變數或參數綁定過的單一 `SCREAMING_CASE` 名稱） | 靜態 SQL 加 `$1` 與 `.bind()` |
| `AssertSqlSafe` | 任何地方用到 `AssertSqlSafe`（sqlx 關掉「SQL 必須是靜態」檢查的方式） | 同上 |
| `DynamicQueryBuilder` | `QueryBuilder::new`、`with_arguments`、`push`、`push_unseparated`、`separated` 收到非靜態的文字 | 值用 `push_bind`；SQL 片段只用字面值或常數 |
| `LeakedString` | 用到 sqlx 的檔案裡有 `.leak()` 或 `Box::leak`：洩漏的 `String` 會變成 `&'static str`，繞過 sqlx 的檢查 | 同上 |
| `SqlBuiltFromStrings` | `format!`、`format_args!`、`write!`、`writeln!`、`+`、`push_str`、`insert_str` 的文字開頭像 SQL（大寫的 `SELECT`、`WHERE`、`AND` 等後接空白） | 同上 |
| `SessionSetting` | SQL 設定或重設 `trustiot.*` 設定或角色（`SET [LOCAL] trustiot.…`、`SET ROLE`、`SET SESSION AUTHORIZATION`、`RESET ALL`、`ALTER ROLE … SET trustiot.…`）、呼叫 `set_config`（第一個參數是 `SET_CONFIG_SETTINGS` 裡審查過的設定的字面值名稱時除外：`application_name`、`idle_in_transaction_session_timeout`、`lock_timeout`、`statement_timeout`；其他設定，例如 `search_path`，只有列在 `SET_CONFIG_ALLOWED` 的檔案可以；名稱是參數、運算式、`trustiot.*`、`role` 或 `session_authorization` 一律被擋），或用到 `pg_settings`（對它 `UPDATE` 等於 `SET`）。檢查的文字包括每個字串字面值、每個 `concat!` 接起來的全文、`include_str!`、`include_bytes!` 引入的檔案內容，以及同一個檔案依原始碼順序交給 QueryBuilder 的靜態文字接起來的全文（常數也代入同檔案裡的值）；QueryBuilder 的片段只要提到 `trustiot` 也報；識別字的雙引號、`.` 前後的空白與 SQL 註解（字串裡的 `--` 不算註解）不影響判斷 | 租戶只由 service-kit 的 `tenant` 模組在交易開頭設定 |
| `UncheckedInclude` | `include_str!`、`include_bytes!` 引入的檔案讀不到（路徑不是字面值，例如 `concat!(env!("OUT_DIR"), …)`，或檔案不存在），無法檢查其中的 SQL | 以相對於原始檔的字面值路徑引入 repo 裡的檔案 |

- 檢查 workspace 內所有 `.rs` 檔（含測試與 `build.rs`），略過 `target`、`node_modules`、`.` 開頭的目錄，以及本工具自己（`SKIPPED`：原始碼與範例檔本來就含有被擋的寫法）。
- 測試程式（路徑含 `tests` 目錄的檔案、`#[cfg(test)]` 模組，或 `TEST_SUPPORT` 列出的 crate）的 `DynamicSql`、`DynamicQueryBuilder` 另外接受單一名稱的變數，例如走訪字面值表格的迴圈變數；sqlx 仍要求 `&'static str`，而 `leak` 與 `AssertSqlSafe` 在測試裡一樣被擋。其他規則對測試程式不放寬。
- `TEST_SUPPORT` 目前只有 `crates/testkit`：跨租戶負向測試框架以巨集在編譯期從宣告的表名組出每個探測的 SQL（`concat!` 的字面值），再以變數交給 sqlx，所以整個 crate 算測試程式。它只能是 `[dev-dependencies]`（`tools/repo-check` 檢查），SQL 不會進到服務的執行檔。測試確認清單上的路徑是 crate。
- `SqlBuiltFromStrings` 不認 `FROM` 與 `COPY` 開頭的文字，因為 Containerfile 也這樣開頭。
- 例外清單有三個，測試確認清單上的路徑存在：
  - `SESSION_SETTING_ALLOWED`：只有 `crates/service-kit/src/tenant.rs` 可以含 `set_config('trustiot.tenant_id', …)`；`tools/migrate/src/postgres.rs`（migration Job，Laysi/trustiot-v2#60）可以以 `set_config('role', $1, true)` 切換到各服務的擁有者角色執行它的 migration，並設定交易的 `search_path`。migration Job 是以資料庫擁有者連線的部署工具，不是服務的執行期程式，也不設定租戶；引入的 `tools/migrate/sql/0001_helpers.sql` 同樣以 `set_config('role', …)` 切換角色；`tools/migrate/src/clickhouse_files.rs` 只以 `include_str!` 列出 ClickHouse 的 DDL 檔，其中 `0004_access.sql` 的註解寫到 `SET ROLE NONE`（ClickHouse，不是 Postgres）。Job 的其他部分（`src/registry.rs` 的 Postgres migration 與每租戶角色 hook）照常檢查，Postgres migration 另由 `tools/migrate/tests/registry.rs` 擋下切換角色、`set_config` 與 `pg_settings`。測試確認清單上的每個檔案拿到別處都會被報出。
  - `SET_CONFIG_ALLOWED`：檔案可以用 `set_config` 改的額外設定，每筆寫明理由與值的來源；不能是 `trustiot.*`、`role` 或 `session_authorization`。目前是 `crates/outbox/src/consumer.rs` 的 `search_path`（值是設定的產生端 schema，`ConsumerConfig::new` 以 `is_schema_name` 驗證過）與 `outbox.delivery`（交易層級的標記，值是消費端產生的 UUID，用來判斷 handler 有沒有結束交易），以及 `crates/outbox/tests/delivery.rs` 的測試用設定 `outbox_test.tenant`。
  - `EXCEPTIONS`：測試檔（路徑在 `tests` 目錄下、不在 `src` 下）可以免除列出的規則，每筆寫明理由；`UncheckedInclude` 不能免除。目前是 `crates/outbox/tests/delivery.rs`、`crates/outbox/tests/support/mod.rs`（以每次執行產生的名稱建立 schema、角色、grant 與 policy，並把這些角色名稱代入 service-kit 的 outbox policy 範本執行，DDL 的識別字不能綁定）、`crates/outbox/tests/verify_271.rs`（Laysi/trustiot-v2#271 的獨立驗證：測試用 handler 以每次執行產生的角色名稱執行 `SET ROLE`、`RESET ALL` 與設定租戶的 `set_config`，驗證這些攻擊會失敗或重現發現的問題，所以 `SessionSetting` 也免除），`services/ingest/tests/clickhouse.rs`（ClickHouse 而不是 Postgres：建立每次執行的使用者、角色與租戶，並以 `SET ROLE NONE` 驗證攻擊會失敗，所以 `SessionSetting` 也免除），`tools/migrate/tests/registry.rs`（只免除 `SessionSetting`：列出 Postgres migration 不能含的 `SET ROLE`、`set_config`、`pg_settings` 等文字，逐一比對每個 migration 檔），`tools/migrate/tests/file_escapes.rs`（只免除 `SessionSetting`：測試用的 migration 檔執行 `SET LOCAL ROLE`、`SET SESSION ROLE`，模擬寫錯的服務 migration，驗證 migration Job 停下且不記錄，Laysi/trustiot-v2#60），`services/identity/tests/adversarial.rs`（只免除 `SessionSetting`：以服務角色用 `set_config` 設定有效、不存在、空白與格式錯誤的租戶，並嘗試 `SET ROLE`、`SET SESSION AUTHORIZATION`，驗證 RLS 與資料庫都拒絕；其中的 SQL 仍是靜態的）、`crates/service-kit/tests/tenant_db.rs`（只免除 `SessionSetting`：以 `set_config(..., false)` 設定 session 層級的租戶，驗證 pool 在連線歸還時關掉這種連線，Laysi/trustiot-v2#271；並以平台角色用 `set_config` 設定租戶，驗證 RLS 仍不讓它寫入租戶的 outbox 事件、有 `platform_access` 的表沒有多給它列，並記錄它有權限而沒有 `platform_access` 的表會讓它看到它設定的租戶的列，Laysi/trustiot-v2#360；並以 `set_config('role', ...)` 切換到平台角色所屬的角色、再設定租戶，記錄連線檢查對平台角色拒絕的每種設定會讓它碰到那個租戶的哪些列，Laysi/trustiot-v2#366；並以 `set_config('role', ...)` 切換到服務或平台角色能以 `SET ROLE` 變成的角色，記錄連線檢查因那個角色拒絕的每種設定在沒有租戶時碰得到什麼，Laysi/trustiot-v2#381），`services/assistant/tests/platform_tenant/mod.rs`（只免除 `SessionSetting`：以 assistant 的清除角色用 `set_config` 設定租戶，驗證 restrictive policy `platform_expired_only` 仍不讓它看到或刪除那個租戶還沒到期的對話，Laysi/trustiot-v2#644），以及 `crates/outbox/tests/failure_paths.rs`（只免除 `SessionSetting`：測試用 handler 的 `begin` 以 `set_config` 把投遞交易改到另一個租戶，模擬寫錯的 handler，驗證消費端把事件記進死信，Laysi/trustiot-v2#350）。
- 只檢查 `.rs` 檔與它們以 `include_str!`、`include_bytes!` 引入的檔案。各服務 `migrations/` 下的 `.sql`（例如 `services/ingest` 的 ClickHouse schema）是 migration 角色執行的 DDL，不經服務的程式送出，不在本檢查範圍；`tools/repo-check` 另外擋下 `services/*/src` 裡改 schema 的 SQL（決策 14）。
- 巨集的內容會解析成運算式或陳述式再檢查（例如 `sqlx::query!`、`tokio::select!`），解析不了時檢查其中的字串字面值與 `AssertSqlSafe`。doc comment 不檢查。

## 限制

檢查讀語法、不讀型別，所以靠名稱判斷：

- 查詢函式要寫成 `sqlx::query(...)` 或從 `sqlx` import（含改名與 `use sqlx::*`）；經由其他模組轉出口的 sqlx 函式認不出來。
- QueryBuilder 要是看得到建立方式的區域變數、型別寫著 `QueryBuilder` 或 `Separated` 的參數，或 `push_values`、`push_tuples` 的 closure 參數；同一個函式裡與它同名的其他變數也會被當成 builder。
- `SqlBuiltFromStrings` 只認大寫開頭的 SQL 字；小寫的 SQL 經由前幾條規則擋下（它到不了 sqlx）。
- `Executor` 的方法（`conn.execute(sql)`）看不出參數是不是 SQL，靠 sqlx 的型別檢查加上 `AssertSqlSafe` 與 `LeakedString` 兩條規則。
- `macro_rules!` 的定義本體不是 Rust 語法，只檢查其中的字串字面值、`concat!` 與引入的檔案、`AssertSqlSafe`、直接呼叫 sqlx 查詢函式時的 SQL（例如 `sqlx::query($sql)` 會被擋），以及 QueryBuilder 的建立與 `push`、`push_unseparated`、`separated`（例如 `$builder.push($column)` 會被擋）。巨集本體看不出接收者的型別，所以在用到 sqlx 或 QueryBuilder 的檔案裡，巨集本體中任何 `.push(...)` 的參數都要是靜態文字；這類檔案的 `Vec` 巨集改用函式。呼叫巨集時傳入 QueryBuilder，其他參數也必須是靜態 SQL。更間接的包裝認不出來，所以服務不要自己定義產生 SQL 的巨集。
- SQL 文字裡再組 SQL（PL/pgSQL 的 `EXECUTE`、`PREPARE`）不展開：`EXECUTE 'SELECT set_' || 'config(…)'` 看不出來。服務的 SQL 不用 `DO` 區塊與動態 SQL；migration 是 `.sql` 檔，不在本檢查範圍，由審查與 service-kit 的連線檢查把關。
- QueryBuilder 的文字只在同一個檔案裡接起來檢查：把 `set_config` 拆成兩段、分別在不同檔案 `push` 看不出來（提到 `trustiot` 的片段仍然會被擋）。
- `include!` 引入的 Rust 程式碼不另外讀；在 repo 裡的 `.rs` 檔本來就會被檢查，`OUT_DIR` 產生的程式碼則不會。

誤判時改寫程式（例如換變數名稱），不要加例外；確實需要例外時在 `src/lib.rs` 加入清單並在 PR 說明理由。

## 測試

- `src/lib.rs` 的單元測試：每條規則的觸發與不觸發的寫法。
- `tests/check.rs`：`tests/fixtures/concatenated.rs` 每個以行尾註解標示規則的行都被報出、而且只有這些行；`tests/fixtures/parameterized.rs` 不報任何問題（兩者引入的 `schema.sql`、`sets-tenant.sql` 也在 `tests/fixtures/`）；整個 workspace 沒有違規；例外清單與略過清單的路徑存在，`EXCEPTIONS` 只列測試檔。
