# telemetry-query：每租戶 ClickHouse 帳號、查詢上限與歷史查詢 API

對應 issue：Laysi/trustiot-v2#119。測試日期 2026-10-22（Asia/Taipei）。原始輸出在 [data/2026-10-22-119-telemetry-query/](data/2026-10-22-119-telemetry-query/)。

## 環境

- **受測程式碼**：分支 `feat/119-telemetry-query`。功能 commit `98ce1f1`，合併 main `1bda1d9` 的 commit `e78df61`；本報告是之後的 commit，只改文件。
- **執行的地方**：建置主機（x86_64 Linux，rootless podman），經共用的檢查佇列執行；佇列把工作樹複製到 `<host copy>`，設 `CI=1`，所以需要容器的測試在沒有容器時失敗而不是略過。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun、helm、kubeconform、cargo-deny。
- **測試用的伺服器**：整合測試自己起拋棄式容器。
  - ClickHouse：`docker.io/clickhouse/clickhouse-server` 26.8，與 chart 相同的映像 digest（`services/ingest/tests/common`），套用 `tools/migrate` 的 `PRODUCT` 所有 ClickHouse migration（含新的 `0007_telemetry_query.sql`）。
  - Postgres：`testkit::product` 起的 TLS Postgres，套用產品 schema（含新的 `telemetry_query` schema）。
  - 租戶由身分服務自己的程式碼建立，telemetry-query 以真正的執行檔啟動，從 `identity.tenant_created` 建立帳號。

## 指令

在 repo 根目錄，經檢查佇列在建置主機（x86_64 Linux）跑，`CI=1`。

驗收條件的整合測試（`--nocapture` 印出每個攻擊與每個序列的比對）：

```sh
cargo test --locked -p telemetry-query -- --nocapture
cargo test --locked -p migrate --test clickhouse_telemetry_query --test clickhouse --test fresh_database
```

最後 head 上的完整檢查（CLAUDE.md「Build, lint, and test」對本分支碰到的路徑要求的全部）：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo run --locked -q -p openapi -- --check
cargo deny --locked check
bun install --cwd scripts --frozen-lockfile
bun test deploy/chart
bun test scripts
bun run --cwd scripts typecheck
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
```

## 結果

### 驗收 1：建立租戶後帳號存在，只看得到自己的資料

`services/telemetry-query/tests/tenant_accounts.rs` 的 `each_tenant_account_sees_its_own_readings_only`，通過（38.78 s）：

- 建立兩個租戶 A、B 之後，服務從事件建好兩個帳號，API 才開始回應這兩個租戶。管理帳號看到帳號的 default role 只有 `trustiot_tenant_reader`（沒有 `ADMIN OPTION`）、`HOST ANY`、`sha256_password`、沒有自己的 grant；以帳號登入看到 `SQL_tenant_id` 是自己的租戶、`readonly = 1` 與 profile 的上限。
- A、B 與第三個只存在 ClickHouse 的租戶各寫 40 筆讀數；A、B 的帳號各自只讀到自己的 40 筆。
- 以 A 的帳號跑 2026-10-09 PoC 的攻擊類型共 36 種，全部被拒或回不出 B 與第三個租戶的讀數，之後那些讀數沒有被改。逐項結果在 [tenant-accounts.txt](data/2026-10-22-119-telemetry-query/tenant-accounts.txt)：基底表、JOIN、子查詢、UNION、CTE、`merge()`、`view()`、`remote()`、`url()`、`file()`、系統表、寫入、建表、建帳號、改服務帳號、grant、刪 row policy 都是 `ACCESS_DENIED` 一類；在 SQL、URL 或 `SET` 改 `SQL_tenant_id` 與上限都是 `READONLY`；以 URL 啟用 ingest 的角色是 `SET_NON_GRANTED_ROLE`；在 view 上過濾別的租戶、恆真條件與 `FINAL` 回應了但沒有別的租戶的讀數。
- 帳號的密碼被管理者改掉之後，下一個 API 呼叫自動重設並回出 A 的讀數。
- **本次新增**：帳號的查詢上限在帳號上被改寬（`ALTER USER ... SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '<A>' CONST, max_execution_time = 3600`，`readonly` 不變；以帳號登入確認 `max_execution_time` 真的變成 3600）時，API 回 503、不執行查詢，日誌有說明；改回之後恢復回應。帳號被改成別的租戶時，API 回 503、回應裡沒有任何租戶的讀數；重播事件時那個事件進 dead letter，B 的帳號列仍只有一筆。
- **本次新增**：每個 API 呼叫只算帳號 quota 的一個查詢。以管理帳號讀 `system.quotas_usage` 的每小時區間，3 次 API 呼叫前後的 `queries` 差是 3：查詢前的帳號檢查不讀表，ClickHouse 26.8 不計入 quota（與 [2026-10-11 的驗證](2026-10-11-59-verify.md) F3 相同）。
- 服務的 stdout、stderr 沒有服務帳號或任何租戶帳號的密碼。

`services/telemetry-query/tests/cross_tenant.rs`（crates/testkit 的跨租戶負向測試）通過：服務角色讀不到也改不了別的租戶的 `telemetry_query.tenant_accounts`。

服務帳號 `trustiot_telemetry_query` 的權限由 `tools/migrate/tests/clickhouse_telemetry_query.rs` 的 `telemetry_query_account` 驗證，通過：

- `0007_telemetry_query.sql` 建立時不能登入（`HOST NONE`）；服務部署時 migration Job 產生密碼寫進 Secret、設定並讓它登入；第二次執行沒有變更；新的密碼世代讓舊密碼失效；服務關閉時又變回 `HOST NONE`，再開啟時恢復。
- 只能建立與修改 `trustiot_tenant_account_*` 帳號；建立別的名稱（含 `trustiot_tenant_accountx`）、修改 collector、view 的 definer 或自己、刪帳號、grant 別的角色或表、建角色、讀任何表，都被拒。
- 帳號多了權限、角色、設定或 default role，或租戶角色多了權限或角色，Job 停止且不改它；撤銷後再跑沒有變更。
- **本次新增**：租戶角色少了 `SELECT ON trustiot.tenant_telemetry` 時 Job 停止。ClickHouse 26.8 讓持有 `ADMIN OPTION` 的帳號把角色給任何使用者或角色：測試以服務帳號啟用租戶角色後成功把它給了一個不相干的使用者，下一次 Job 停止並列出那個使用者；把角色給 `trustiot_ingest` 角色、或給某個租戶帳號 `ADMIN OPTION`，Job 也停止；租戶帳號沒有 `ADMIN OPTION` 地持有角色則允許。

### 驗收 2：超過查詢上限的重查詢被中止

同一個 `tenant_accounts.rs` 測試，以 A 的帳號：

| 重查詢 | 結果 |
| --- | --- |
| 讀超過 `max_rows_to_read`（10 億列） | `TOO_MANY_ROWS` |
| 超過 `max_memory_usage`（512 MiB） | `MEMORY_LIMIT_EXCEEDED` |
| 跑超過 `max_execution_time`（30 秒） | `TIMEOUT_EXCEEDED` |

`tests/history.rs` 另外確認 API 對這類錯誤回 422 `telemetry.query_too_heavy`，quota 用完時回 429 `rate_limited`（同一分鐘內第 100 個以上的呼叫才被限制，所以每個呼叫只算一次）。

### 驗收 3：超出可查期間的查詢被拒或截斷

`tests/history.rs` 的 `history_matches_clickhouse_within_the_queryable_period`，通過：原始資料往前超過 7 天的範圍截到期間開始（`truncated: true`、`earliest`），整段早於期間時回 403 `quota_exceeded`、`telemetry.outside_queryable_period`；聚合資料以 3 個日曆月做同樣的截斷與拒絕，120 天前的讀數不出現。單元測試 `history::tests::ranges_are_cut_to_the_queryable_period` 覆蓋邊界。

### 驗收 4：降採樣的序列與直接查 ClickHouse 的結果一致

同一個測試，399 筆讀數，以另一組 ClickHouse 函式（`toStartOfHour`、`toStartOfDay`、`toStartOfFifteenMinutes`）直接查表算出參考值，逐區間比較 `count`、`min`、`max`、`avg`、`last` 與區間開始時間（[history-series.txt](data/2026-10-22-119-telemetry-query/history-series.txt)）：

| 時區 | 區間 | 區間數 | 結果 |
| --- | --- | --- | --- |
| Asia/Taipei | 1h | 49 | 相同 |
| Asia/Kolkata | 3h | 17 | 相同 |
| Asia/Taipei | 1d | 3 | 相同 |
| UTC | 1d | 3 | 相同 |
| Asia/Kolkata | 15m | 193 | 相同 |
| Asia/Taipei | 自動（選到 5m） | 399 | 相同 |

另外：整數、bool、文字資料點的形狀；`raw` 與 `latest` 等於儲存的讀數；另一個租戶同一個設備 ID 的讀數不出現；401、403、404、400、422 的各種拒絕都不帶 ClickHouse 的錯誤內文。

### 完整檢查（最後 head）

在合併 main 之後的 `e78df61` 上，建置主機，2026-10-22 14:04 到 14:19（Asia/Taipei），全部通過：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `cargo test --locked --workspace` | 通過：120 個測試執行檔、893 個測試，0 失敗（`CI=1`，需要容器的測試都有執行） |
| `cargo run --locked -q -p openapi -- --check` | `api/openapi.json is up to date` |
| `cargo deny --locked check` | `advisories ok, bans ok, licenses ok, sources ok` |
| `bun test deploy/chart` | 461 pass、0 fail（含啟用 `services.telemetry-query` 時的渲染、密碼 Secret 只給這個服務、自訂 `env` 不能覆寫或讀它；預設仍是停用） |
| `bun test scripts` | 1482 pass、4 skip、0 fail |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun scripts/ci/check-secret-handling.ts` | 449 個檔案，沒有違規 |
| `bun scripts/ci/check-workflows.ts` | 4 個檔案，沒有違規 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 建立租戶後該租戶帳號存在且只看得到自己的資料（以 2026-10-09 PoC 的攻擊類型測試） | 通過 |
| 超過查詢上限的重查詢被中止（測試） | 通過 |
| 超出可查期間的查詢被拒或截斷（測試） | 通過 |
| 設備圖表的歷史查詢 API 依指定的時間範圍與時區回傳降採樣後的序列，與直接查 ClickHouse 算出的結果一致（測試） | 通過 |

待測（不屬於本 issue 的驗收條件，另開 issue）：多租戶同時重查詢時資料接收不受影響的負載測試，與依部署規模的伺服器層級記憶體上限（本 issue 2026-10-10 的留言）；chart 啟用 `services.telemetry-query` 時在叢集上的實際部署（目前只有 `bun test deploy/chart` 的渲染與規則檢查，nightly 還沒建置這個映像）。
