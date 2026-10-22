# telemetry-query：每租戶 ClickHouse 帳號、查詢上限與歷史查詢 API

對應 issue：Laysi/trustiot-v2#119。測試日期 2026-10-22（Asia/Taipei）。原始輸出在 [data/2026-10-22-119-telemetry-query/](data/2026-10-22-119-telemetry-query/)。

## 環境

- **受測程式碼**：分支 `feat/119-telemetry-query`。功能 commit `98ce1f1`，合併 main `1bda1d9` 的 commit `e78df61`，安全審查的修正 `1b7f9f6` 與 `a753fec`；本報告的 commit 只改文件與測試資料。
- **執行的地方**：經檢查佇列在建置主機（x86_64 Linux，rootless podman）跑，`CI=1`，所以需要容器的測試在沒有容器時失敗而不是略過。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun、helm、kubeconform、cargo-deny。
- **測試用的伺服器**：整合測試自己起拋棄式容器。
  - ClickHouse：`docker.io/clickhouse/clickhouse-server` 26.8，與 chart 相同的映像 digest（`services/ingest/tests/common`），套用 `tools/migrate` 的 `PRODUCT` 所有 ClickHouse migration（含新的 `0007_telemetry_query.sql`、`0008_telemetry_query_grantees.sql`）。
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
cargo test --locked --workspace --no-fail-fast
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

`services/telemetry-query/tests/tenant_accounts.rs` 的 `each_tenant_account_sees_its_own_readings_only`，通過（39.65 s）：

- 建立兩個租戶 A、B 之後，服務從事件建好兩個帳號，API 才開始回應這兩個租戶。管理帳號看到帳號的 default role 只有 `trustiot_tenant_reader`（沒有 `ADMIN OPTION`）、`HOST ANY`、`sha256_password`、沒有自己的 grant；以帳號登入看到 `SQL_tenant_id` 是自己的租戶、`readonly = 1` 與 profile 的上限。
- A、B 與第三個只存在 ClickHouse 的租戶各寫 40 筆讀數；A、B 的帳號各自只讀到自己的 40 筆。
- 以 A 的帳號跑 2026-10-09 PoC 的攻擊類型共 40 種（33 種查詢與 7 個系統表），全部被拒或回不出 B 與第三個租戶的讀數，之後那些讀數沒有被改。逐項結果在 [tenant-accounts.txt](data/2026-10-22-119-telemetry-query/tenant-accounts.txt)：基底表、JOIN、子查詢、UNION、CTE、`merge()`、`view()`、`remote()`、`url()`、`file()`、系統表、寫入、建表、建帳號、改服務帳號、grant、刪 row policy 都是 `ACCESS_DENIED` 一類；系統表 `query_log`、`text_log`、`processes`、`parts`、`users`、`role_grants`、`settings_profile_elements` 在以管理帳號 `SYSTEM FLUSH LOGS` 建出日誌表之後，測試要求每個都是 `ACCESS_DENIED`（安全審查之前 `system.query_log` 是因為表還不存在而以 `UNKNOWN_TABLE` 被拒）；在 SQL、URL 或 `SET` 改 `SQL_tenant_id` 與上限都是 `READONLY`；以 URL 啟用 ingest 的角色是 `SET_NON_GRANTED_ROLE`；在 view 上過濾別的租戶、恆真條件與 `FINAL` 回應了但沒有別的租戶的讀數。
- 帳號的密碼被管理者改掉之後，下一個 API 呼叫自動重設並回出 A 的讀數。
- **本次新增**：帳號的查詢上限在帳號上被改寬（`ALTER USER ... SETTINGS PROFILE 'trustiot_tenant_reader', SQL_tenant_id = '<A>' CONST, max_execution_time = 3600`，`readonly` 不變；以帳號登入確認 `max_execution_time` 真的變成 3600）時，API 回 503、不執行查詢，日誌有說明；改回之後恢復回應。帳號被改成別的租戶時，API 回 503、回應裡沒有任何租戶的讀數；重播事件時那個事件進 dead letter，B 的帳號列仍只有一筆。
- **本次新增**：每個 API 呼叫只算帳號 quota 的一個查詢。以管理帳號讀 `system.quotas_usage` 的每小時區間，3 次 API 呼叫前後的 `queries` 差是 3：查詢前的帳號檢查不讀表，ClickHouse 26.8 不計入 quota（與 [2026-10-11 的驗證](2026-10-11-59-verify.md) F3 相同）。
- 服務的 stdout、stderr 沒有服務帳號或任何租戶帳號的密碼。

`services/telemetry-query/tests/cross_tenant.rs`（crates/testkit 的跨租戶負向測試）通過：服務角色讀不到也改不了別的租戶的 `telemetry_query.tenant_accounts`。

服務帳號 `trustiot_telemetry_query` 的權限由 `tools/migrate/tests/clickhouse_telemetry_query.rs` 的 `telemetry_query_account` 驗證，通過：

- `0007_telemetry_query.sql` 建立時不能登入（`HOST NONE`）；服務部署時 migration Job 產生密碼寫進 Secret、設定並讓它登入；第二次執行沒有變更；新的密碼世代讓舊密碼失效；服務關閉時又變回 `HOST NONE`，再開啟時恢復。
- 只能建立與修改 `trustiot_tenant_account_*` 帳號；建立別的名稱（含 `trustiot_tenant_accountx`）、修改 collector、view 的 definer 或自己、刪帳號、grant 別的角色或表、建角色、讀任何表，都被拒。
- 帳號多了權限、角色、設定或 default role，`GRANTEES` 不再是 `NONE`，或租戶角色多了權限或角色、少了 `SELECT ON trustiot.tenant_telemetry`、有服務帳號與租戶帳號以外的持有者（不相干的使用者、`trustiot_ingest` 角色、拿到 `ADMIN OPTION` 的租戶帳號）時，Job 讓服務帳號不能登入（`HOST NONE`）並停止，訊息說明原因；撤銷後再跑設定密碼、恢復登入。租戶帳號沒有 `ADMIN OPTION` 地持有角色則允許。
- `GRANTEES NONE`：服務帳號啟用租戶角色後仍能以 `CREATE USER ... DEFAULT ROLE trustiot_tenant_reader` 建立帶角色的帳號；把角色授與不相干的使用者或 `trustiot_ingest`、從租戶帳號（既有的與剛建立的）收回，都被拒（`is not allowed as grantee`）；`ALTER USER trustiot_telemetry_query GRANTEES ANY` 也被拒。

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

在安全審查的修正之後的 `a753fec` 上，建置主機，2026-10-22 15:27 到 15:37（Asia/Taipei），全部通過（合併 main 之後的 `e78df61` 上也曾全部通過，數字相同）：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `cargo test --locked --workspace --no-fail-fast` | 通過：120 個測試執行檔、893 個測試，0 失敗（`CI=1`，需要容器的測試都有執行） |
| `cargo run --locked -q -p openapi -- --check` | `api/openapi.json is up to date` |
| `cargo deny --locked check` | `advisories ok, bans ok, licenses ok, sources ok` |
| `bun test deploy/chart` | 461 pass、0 fail（含啟用 `services.telemetry-query` 時的渲染、密碼 Secret 只給這個服務、自訂 `env` 不能覆寫或讀它；預設仍是停用） |
| `bun test scripts` | 1482 pass、4 skip、0 fail |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun scripts/ci/check-secret-handling.ts` | 449 個檔案，沒有違規 |
| `bun scripts/ci/check-workflows.ts` | 4 個檔案，沒有違規 |

## 2026-10-22 安全審查

審查沒有 Critical 或 High。依 2026-10-21 的決定，Medium 與 Low 的修正不另加測試、不再審查，但既有測試都要通過（上方的整合測試與完整檢查是修正之後的結果）。

| 項目 | 處理 |
| --- | --- |
| Medium 1：服務帳號以 `ADMIN OPTION` 持有租戶角色，能把它授與或收回任何使用者或角色（審查時把它給了 `trustiot_ingest`，ingest 的帳號因此變成 `readonly = 1`、每個租戶都寫不進去；也能從租戶帳號收回） | 已修。新的 `0008_telemetry_query_grantees.sql` 把服務帳號設成 `GRANTEES NONE`（`0007` 是這個分支新增的檔案，但為了不改已記錄 checksum 的檔案，另開一個檔案）。服務只以 `CREATE USER ... DEFAULT ROLE` 給角色，不以 `GRANT` 給，所以沒有程式路徑受影響。Job 每次執行檢查 `grantees_any = 0` 且 `grantees_list` 是空的；這一項或服務帳號、租戶角色的任何其他檢查不符時，Job 先把服務帳號改成 `HOST NONE` 再停止，修好之後的下一次執行恢復登入。`tools/migrate/tests/clickhouse_telemetry_query.rs` 改成驗證：服務帳號啟用租戶角色後仍能建立帶角色的帳號；把角色授與不相干的使用者或 `trustiot_ingest`、從既有或剛建立的租戶帳號收回都被拒（`is not allowed as grantee`）；改自己的 `GRANTEES` 被拒；每一種竄改都讓 Job 停止且服務帳號不能登入，修好之後恢復 |
| Medium 2：只輪替服務帳號的密碼時，租戶帳號在該租戶下一次查詢之前保留舊的推導密碼；關掉服務只鎖服務帳號 | runbook 與註解已修：`docs/runbooks/migrations.md` 不再說租戶密碼跟著改變，並新增「懷疑時序查詢的服務帳號外洩」：輪替、以管理帳號刪掉每個 `trustiot_tenant_account_<32 個十六進位數字>` 帳號（服務在該租戶下一次查詢時以新的推導密碼重建）、在 `system.users` 找出 Job 不認得的帳號。這幾步先在拋棄式 ClickHouse 26.8 容器實測：刪除語句只刪符合名稱的帳號，刪掉之後以舊密碼登入得到 `AUTHENTICATION_FAILED`（服務以這個錯誤判斷要重建），服務帳號以 `CREATE USER IF NOT EXISTS ... DEFAULT ROLE` 重建後帳號有租戶角色、`SQL_tenant_id` 與 `readonly = 1`；被服務帳號改名的帳號出現在不認得的清單裡。`accounts.rs` 的註解改成：服務帳號（`GRANTEES NONE`）不能授與或收回角色，但仍能 `ALTER USER` 前綴下的帳號，包括 `RENAME` 與 `ADD IDENTIFIED`。其餘部分（`system.users` 的稽核、輪替時讓租戶密碼失效、收編既有帳號的檢查）記在 Laysi/trustiot-v2#528 |
| Low 3：租戶帳號能 `SET DEFAULT ROLE NONE` | 記在 Laysi/trustiot-v2#143 |
| Low 4：修復路徑沒有退避 | 記在 Laysi/trustiot-v2#528 |
| Info 5：`system.query_log` 的攻擊是因為表不存在（`UNKNOWN_TABLE`）才被拒 | 已修。測試先以管理帳號 `SYSTEM FLUSH LOGS`，再要求 `system.query_log`、`text_log`、`processes`、`parts`、`users`、`role_grants`、`settings_profile_elements` 都是 `ACCESS_DENIED`，[tenant-accounts.txt](data/2026-10-22-119-telemetry-query/tenant-accounts.txt) 已更新 |
| Info 6：ClickHouse 的 HTTP handler 在 `trace` 等級把請求 URI（帶加鹽 hash 的參數）寫進伺服器日誌 | 已寫進文件（`services/telemetry-query/README.md`、`docs/architecture/secrets.md`）：外部 ClickHouse 不可以用 `trace` 等級；叢集內的 ClickHouse 是 `information`。在拋棄式容器確認：只有 `trace` 的 `Request URI` 那一行帶參數，`debug` 的 `executeQuery` 與 `system.query_log` 都遮住 hash |
| Info 7 | 已知悉，不需要變更 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 建立租戶後該租戶帳號存在且只看得到自己的資料（以 2026-10-09 PoC 的攻擊類型測試） | 通過 |
| 超過查詢上限的重查詢被中止（測試） | 通過 |
| 超出可查期間的查詢被拒或截斷（測試） | 通過 |
| 設備圖表的歷史查詢 API 依指定的時間範圍與時區回傳降採樣後的序列，與直接查 ClickHouse 算出的結果一致（測試） | 通過 |

待測（不屬於本 issue 的驗收條件，另開 issue）：多租戶同時重查詢時資料接收不受影響的負載測試，與依部署規模的伺服器層級記憶體上限（本 issue 2026-10-10 的留言）；chart 啟用 `services.telemetry-query` 時在叢集上的實際部署（目前只有 `bun test deploy/chart` 的渲染與規則檢查，nightly 還沒建置這個映像）。
