# 連線檢查：平台角色有權限、卻只有租戶條件開放的表（2026-10-13）

對應 issue：Laysi/trustiot-v2#366

## 環境

- 建置主機（check queue）：x86_64 Linux（WSL2），20 核心，Docker；`tenant_db` 的測試共用一個用完即刪的 `postgres:18.6-alpine`（TLS，`verify-full`），每個測試一個資料庫。指令經 check queue 執行，環境變數 `CI=1`（沒有容器執行環境時測試失敗而不是略過）。
- Rust 依 `rust-toolchain.toml`（1.99.0）；sqlx 版本依 `Cargo.lock`。
- 分支 `feat/366-platform-role-policy-check`，從 main `be1cfea`（已含 Laysi/trustiot-v2#360 的 `platform_events_only` 與範本說明）開始，第一次最終檢查前 fast-forward 到 main `28862e1`。獨立驗證（見「獨立驗證」）之後修正 F1–F3，再以 merge commit 合併 main `5fbe8ea`（Laysi/trustiot-v2#373，service-kit README 的測試表衝突，兩邊都保留）。
- 「成本」的數據在 `be1cfea` 上、修正前的規則量得；「被拒的設定」「接受的設定」「突變測試」是修正後的結果；「最終檢查」在合併 `5fbe8ea` 後的 head 上跑。

## 規則

`check_session` 對平台角色（`SessionRole::Platform`，`PlatformDb` 用的規則）多一項檢查，回新的 `UnsafeSession::PlatformTenantOnly { table, role, command }`：

- 角色：session 能以 `SET ROLE` 變成的每個角色，含它自己（`pg_has_role(current_user, ..., 'SET')`，Postgres 16 以上）。只繼承、不能 `SET ROLE` 的角色的權限已算在連線角色自己身上；既不能繼承也不能 `SET ROLE` 的角色什麼都給不了它。
- 權限：對啟用 RLS 的表（`relkind` 為 `r`、`p`）能執行 `SELECT`、`INSERT`、`UPDATE`、`DELETE`（表或欄位的權限，含繼承得到的與給 `PUBLIC` 的），而且能用那個 schema。
- policy：只算 permissive policy，而且是 Postgres 真的會套用到該角色的：policy 的角色是 `PUBLIC`，或是該角色繼承得到權限的角色（`pg_has_role(..., 'USAGE')`）；指令相符（含 `FOR ALL`）。逐指令看指令實際用到的運算式：`SELECT`、`DELETE` 看 `USING`；`INSERT` 看 `WITH CHECK`，沒有時看 `USING`；`UPDATE` 兩邊都看。
- 運算式是否開放這張表：Postgres 存下的租戶條件（`TENANT_CONDITION_AS_STORED`）、任何讀 `trustiot.tenant_id` 的運算式（不論寫法）與 `false` 都不算開放；其他運算式都算。這只會多拒、不會多放：讀了租戶設定卻開放每一列的運算式（`<租戶條件> OR true`）也會被拒。
- 拒絕：這個指令用到的每一邊都有運算式，而且至少一邊沒有任何運算式開放這張表。某一邊完全沒有 policy 時這個指令碰不到任何列，不拒絕。

仍是每個新連線一次查詢（`SESSION_CHECK` 多幾個 CTE，以 `$1` 只在平台角色的連線計算）。錯誤訊息例：

```text
database role `example_platform` may SELECT on `public.private_items`, but the only permissive policies that apply to it for SELECT are the tenant condition, so a platform session that sets trustiot.tenant_id itself would reach that tenant's rows; revoke the grant or add a policy for the platform role by name (platform_access)
```

## 指令

```sh
# 主要測試（含 --nocapture 的證據輸出）
cargo test --locked -p service-kit --test tenant_db -- --nocapture

# 突變測試：在 check queue 的副本裡逐一改壞 SESSION_CHECK，確認測試抓得到（腳本見 data 目錄）
bash mutations-366.sh

# 成本：兩個暫時的測試（沒有 commit，原始碼見 data 目錄）建 300 張照 tenant-table.sql 寫的表
# （加上範本的 example_items 共 301 張），各量 30 次 check_session，並印出 EXPLAIN ANALYZE
cargo test --locked -p service-kit --test tenant_db -- tmp_366 --nocapture
```

最終檢查的指令與結果在下方「最終檢查」。

## 結果

原始輸出（已去掉 `Compiling` 行與建置主機的資訊）：修正後的 `tenant_db` 完整輸出 [tenant-db.log](data/2026-10-13-366-platform-tenant-only/tenant-db.log)、修正後的突變測試 [mutations.log](data/2026-10-13-366-platform-tenant-only/mutations.log) 與腳本 [mutations.sh.txt](data/2026-10-13-366-platform-tenant-only/mutations.sh.txt)；修正前的時間量測與突變測試 [tenant-db-timing-mutations.log](data/2026-10-13-366-platform-tenant-only/tenant-db-timing-mutations.log)（行在 200 字元處截斷）、暫時測試的原始碼 [timing-and-plan-probes.rs.txt](data/2026-10-13-366-platform-tenant-only/timing-and-plan-probes.rs.txt)。

### 被拒的設定（`platform_sessions_only_the_tenant_condition_opens_are_refused`）

每一列：owner 做出設定 → 平台角色的 `check_session` 回下表的 `PlatformTenantOnly` → 以不經連線檢查的一般連線（必要時先 `set_config('role', ...)` 切換角色）在沒設租戶、設了租戶 A 的交易各跑一次探測語句，記下碰到的列數 → 修正後 `check_session` 回 `Ok(())`。表 `private_items` 只有 `tenant_isolation`，租戶 A、B 各一列。

| 設定 | 拒絕（表、角色、指令） | 沒設租戶 | 設了租戶 A |
| --- | --- | --- | --- |
| `GRANT SELECT` | `public.private_items`、`example_platform`、`SELECT` | 0 | 1 |
| `GRANT INSERT` | 同上、`INSERT` | 0（RLS 拒絕） | 1（寫入租戶 A 的列） |
| `GRANT UPDATE` | 同上、`UPDATE` | 0 | 1 |
| `GRANT DELETE` | 同上、`DELETE` | 0 | 1 |
| 欄位權限 `GRANT SELECT (name)` | 同上、`SELECT` | 0 | 1 |
| `GRANT SELECT ... TO PUBLIC` | 同上、`example_platform`、`SELECT` | 0 | 1 |
| 群組 `example_platform_group` 有 `SELECT`（平台角色繼承） | 同上、`example_platform`、`SELECT` | 0 | 1 |
| 群組有 `example_items`（有 `platform_access`）的 `SELECT`，平台角色 `SET ROLE` 到群組後讀 | `public.example_items`、`example_platform_group`、`SELECT` | 0 | 2 |
| 只能 `SET ROLE` 的 `example_platform_switch`（`WITH INHERIT FALSE`）有 `SELECT` | `public.private_items`、`example_platform_switch`、`SELECT` | 0 | 1 |
| 以名稱給平台角色、但就是租戶條件的 `FOR ALL` policy，加 `GRANT SELECT` | `public.private_items`、`example_platform`、`SELECT` | 0 | 1 |
| 給平台角色 `FOR SELECT USING (true)`，再 `GRANT SELECT, DELETE` | 同上、`DELETE` | 0 | 1 |
| 給平台角色 `FOR UPDATE USING (true) WITH CHECK (<租戶條件>)`，`GRANT UPDATE` | 同上、`UPDATE` | 0（RLS 拒絕） | 2（租戶 B 的列被改進租戶 A） |
| 另一個 schema `other.private_items`，給 `USAGE` 與 `SELECT` | `other.private_items`、`example_platform`、`SELECT`；收回 schema 的 `USAGE` 後接受 | 0 | 1 |
| 只能 `SET ROLE` 的 `example_platform_switch` 有 `SELECT`；它以 `WITH INHERIT FALSE` 屬於 `example_switch_parent`，`USING (true)` 的 policy 給 `example_switch_parent`（驗證 F2） | `public.private_items`、`example_platform_switch`、`SELECT` | 0 | 1 |
| 給平台角色 `USING (current_setting('trustiot.tenant_id', true)::uuid = tenant_id)`，`GRANT SELECT`（驗證 F1，下五列同） | `public.private_items`、`example_platform`、`SELECT` | 0 | 1 |
| 給平台角色 `USING (tenant_id::text = current_setting('trustiot.tenant_id'))` | 同上 | 0（沒設時讀設定出錯） | 1 |
| 給平台角色 `USING (<租戶條件> AND true)` | 同上 | 0 | 1 |
| 給平台角色 `USING (<租戶條件> OR false)` | 同上 | 0 | 1 |
| 給平台角色 `USING (tenant_id IS NOT DISTINCT FROM NULLIF(...)::uuid)` | 同上 | 0 | 1 |
| 給平台角色 `USING (false)` | 同上 | 0 | 1 |
| `GRANT SELECT` 加只給平台角色的 restrictive `USING (tenant_id IS NULL)` | `public.private_items`、`example_platform`、`SELECT` | 0 | 0 |

最後一列是刻意的：restrictive policy 不列入判斷（它只會收窄，檢查不判斷它是否擋住了租戶條件多開的部分），這種權限對 `PlatformDb` 也沒有用，撤銷即可。

另一個刻意多拒的情況：給平台角色 `USING (<租戶條件> OR true)` 與 `GRANT SELECT`，平台角色不設租戶就讀得到全部 2 列（沒有因自己設租戶而多拿），檢查仍以 `PlatformTenantOnly` 拒絕，因為讀 `trustiot.tenant_id` 的運算式一律不算開放，檢查不去解析運算式。

經 kit 的 pool：同樣的 `GRANT SELECT` 讓 `PlatformDb::ping` 回 `TenantDbError::UnsafeSession(PlatformTenantOnly { ... })`（`refused the database session: database role ...`）。

### 接受的設定

| 設定 | 結果 |
| --- | --- |
| `tenant-table.sql`（`example_platform` 有 `SELECT, UPDATE` 與 `platform_access`） | `Ok(())`（`sessions_row_level_security_would_not_protect_are_refused` 與新測試開頭）；服務角色 `example_app` 以服務角色規則 `Ok(())`，新規則不影響它 |
| `outbox-event.sql`：`example_platform`（`INSERT` + `platform_events`）、`example_reader`（`SELECT` + `consumer_read`） | 都 `Ok(())`（`outbox_events_follow_the_tenant_policies`） |
| 產生端平台角色同時讀自己的事件：再給 `example_platform` `consumer_read_own`（`FOR SELECT USING (true)`）與 `SELECT`，也就是範本把 `example_reader` 換成 `example_platform` 會多出的 policy 與權限 | `example_app`、`example_platform`、`example_reader` 都 `Ok(())`；新開的 `PlatformDb`（`example_platform`）讀到全部事件（租戶 A 與全平台各一筆），同一交易寫全平台事件成功 |
| 啟用 RLS、沒有任何 policy 的表，給平台角色 `SELECT` | `Ok(())`；設不設租戶都 0 列 |
| 收回 schema `USAGE` 的 `other.private_items`（權限還在） | `Ok(())` |
| `example_platform_unusable`（`WITH INHERIT FALSE, SET FALSE`）有 `private_items` 的 `SELECT`（驗證 F3） | `Ok(())`；平台角色設了租戶 A 讀這張表 42501 `permission denied for table`，`SET ROLE` 到它 42501 `permission denied to set role` |
| `example_platform_inherited`（`WITH INHERIT TRUE, SET FALSE`）有 `example_items`（有 `platform_access`）的 `DELETE`（驗證 F3） | `Ok(())`；平台角色的 `DELETE` 不設租戶與設了租戶 A 都刪到 3 列（沒有多拿），`SET ROLE` 到它 42501 |
| `outbox` crate 的整套測試（每個測試照 `outbox-event.sql` 建 RLS，產生端與消費端的 `PlatformDb` 都經連線檢查） | 見「最終檢查」 |

`outbox` crate 的 `a_reader_without_the_read_policy_stops_the_consumer`（`tests/tenancy.rs`）原本以消費端的服務角色當讀取端（有 `outbox_event` 的 `SELECT`、只有租戶條件），預期 `ensure_subscription` 回 `CannotReadAllTenants`；新規則讓 service-kit 先拒絕這個連線，所以改成斷言 `ConsumerError::UnsafeSession(PlatformTenantOnly { table: <產生端 schema>.outbox_event, role: <消費端服務角色>, command: SELECT })`（`is_retryable()` 為真），並加一段以消費端平台角色、把 `consumer_read` 換成只開放全平台事件的 `USING (tenant_id IS NULL)`：連線檢查接受（不是租戶條件），`ensure_subscription` 仍回 `CannotReadAllTenants`，這條路徑因此仍有測試。outbox README 的對應說明一起更新。

既有的兩個 service-kit 測試因新規則調整：`the_platform_role_reads_across_tenants_only_where_allowed` 原本給平台角色 `private_items` 的 `SELECT`（沒有 `platform_access`）再斷言讀到 0 列，現在照範本不給權限，斷言讀取被拒（42501）；`the_platform_role_gains_nothing_by_setting_a_tenant` 最後一段先斷言 `check_session` 拒絕，再以不經檢查的一般連線記錄自己設租戶後讀到的列。

### 突變測試

在 check queue 的副本裡逐一改壞 `SESSION_CHECK`，再跑 `tenant_db` 中名稱含 `platform` 的測試；在合併 `5fbe8ea` 後的 head 上，每一種都讓測試失敗：

| 突變 | 失敗的測試與斷言 |
| --- | --- |
| 規則關閉（`HAVING false`） | `the_platform_role_gains_nothing_by_setting_a_tenant`、`platform_sessions_only_the_tenant_condition_opens_are_refused`：`left: Ok(())`，預期 `SELECT` 被拒 |
| F1 還原：只有字面上的租戶條件算不開放 | 新測試：預期讀租戶設定的平台 policy 被拒 |
| F2 還原：policy 以 `MEMBER` 判斷是否套用 | 新測試：預期 `example_platform_switch` 的 `SELECT` 被拒 |
| F3 還原：角色以 `MEMBER` 列出 | 新測試：`example_platform_unusable` 的權限被拒（預期接受） |
| 只看連線的角色、不看能 `SET ROLE` 的角色 | 新測試：預期 `example_platform_group` 的 `SELECT` 被拒 |
| `UPDATE` 只看 `USING` | 新測試：預期 `UPDATE` 被拒（`WITH CHECK` 是租戶條件的那列） |
| 不看欄位權限 | 新測試：預期欄位權限的 `SELECT` 被拒 |
| restrictive policy 也算進來 | 新測試：預期 restrictive `USING (tenant_id IS NULL)` 那列仍被拒 |

### 成本

301 張照範本寫的表（每張 `tenant_isolation` 與 `platform_access`，平台角色有 `SELECT, UPDATE`），平台角色屬於兩個群組角色；同一條連線連跑 30 次 `check_session`：

| 連線 | 規則 | 最小 | 中位數 | 最大 |
| --- | --- | --- | --- | --- |
| `example_app` | 服務角色 | 7.4 ms | 7.9 ms | 39.7 ms |
| `example_platform` | 平台角色 | 17.4 ms | 18.0 ms | 54.2 ms |
| `example_platform` | 服務角色 | 7.2 ms | 7.8 ms | 35.8 ms |

這是修正 F1–F3 之前的規則；修正後獨立驗證在 300 張表量到平台角色本身中位數 20.7 ms（見「獨立驗證」）。規則關閉時（上面的第一種突變）平台角色是 5.6 / 6.1 / 34.3 ms，所以新規則在 301 張表、三個角色時每個新連線多約 10 ms；服務角色的檢查不變（新的 CTE 以 `$1` 擋掉）。EXPLAIN ANALYZE 顯示多出的時間主要是以角色 oid 呼叫的權限函式（`has_table_privilege` 等，3 個角色 × 301 張表）與一次性的 `pg_get_expr`。試過讓權限檢查依角色分批（LATERAL + `OFFSET 0`）只省約 1 ms，沒有採用。連線池的連線是長期使用的，只有新連線執行這個查詢。

## 範圍外與限制

- **以名稱給平台角色、但不是全部租戶、也不讀 `trustiot.tenant_id` 的 policy**（例如 `platform_events` 的 `WITH CHECK (tenant_id IS NULL)`，或固定某個租戶的 `tenant_id = '<租戶>'`；獨立驗證的 2.6、2.7）：通過這項檢查，因為它不讀租戶設定；它與 `tenant_isolation` 以 OR 結合，設了租戶的 session 仍能多拿到那個租戶。檢查無法判斷一條任意的 policy 有沒有涵蓋租戶條件多開的部分，所以範本以 restrictive policy（`platform_events_only`）處理，README 記下這條規則。
- **restrictive policy 不列入**：它只會收窄。有權限、只靠 restrictive policy 擋住的設定也被拒（上表最後一列），修正方式是撤銷權限。
- **角色與 policy 的範圍**：檢查 session 能以 `SET ROLE` 變成的角色（`SET`），policy 只算 Postgres 會套用的（`USAGE`，即繼承得到權限的 policy 角色）。不能繼承也不能 `SET ROLE` 的成員關係不算，只繼承的角色的權限算在連線角色自己身上（獨立驗證的 F2、F3）。
- **讀租戶設定的運算式一律不算開放**：`<租戶條件> OR true` 這類其實開放每一列的寫法也被拒，寧可多拒，不解析運算式。
- **成本隨持有權限的角色數增加**：見「獨立驗證」的 F4，另案處理。
- **服務角色**：不在範圍內（issue 的「不包含」），規則只在平台角色的連線計算。
- **`TRUNCATE`**：不受 RLS 限制，既有的 `TruncateAllowed` 對所有角色檢查。
- **只能 `SET ROLE` 到的角色在其他檢查項目上的權限**（例如沒有 RLS 的租戶表）：既有的 `reachable` 只看連線角色本身的權限，這是既有檢查的範圍，不在本 issue；見 PR 的後續建議。

## 獨立驗證（2026-10-13）

head `78c7b99`（本分支第一個 commit）另由獨立驗證在 check queue 的建置主機上以 Postgres 18.6 測了 37 種情境，每種都同時看 `check_session` 的結論與一般連線（不經檢查）實際碰得到什麼。結論：規則成立，但要修正 F1–F3。原始資料在 [independent-verification/](data/2026-10-13-366-platform-tenant-only/independent-verification/)：探針原始碼 `verify_366.rs.txt`（沒有 commit），修正前的結果 `rcheck-probes.log`、修正前的 `tenant_db` 與 outbox `rcheck-baseline.log`，建議的修正 `proposed-fix.diff`，以及套用修正後重跑探針、`tenant_db` 與 outbox 的 `rcheck-proposed-fix.log`。

| 發現 | 嚴重度 | 情況 | 修正（本分支第二個 commit；SQL 的邏輯與 `proposed-fix.diff` 相同，另補註解與文件） |
| --- | --- | --- | --- |
| F1 | 中，漏放 | 只要不是字面上的租戶條件就算開放，所以以別的寫法只開放自己租戶的平台 policy 被接受：`current_setting('trustiot.tenant_id', true)::uuid = tenant_id`、`tenant_id::text = current_setting('trustiot.tenant_id')`、`<租戶條件> AND true`、`IS NOT DISTINCT FROM`、`<租戶條件> OR false`、`USING (false)`（2.1–2.5、2.9） | 讀 `trustiot.tenant_id` 的運算式與 `false` 不算開放 |
| F2 | 低，漏放 | policy 以 `MEMBER` 判斷是否套用，但 Postgres 只把 policy 套用到繼承得到 policy 角色權限的角色。1.6：`GRANT SELECT ON items TO g; GRANT g TO p WITH INHERIT FALSE, SET TRUE; GRANT h TO g WITH INHERIT FALSE; CREATE POLICY grp ON items TO h USING (true);` 被接受，`SET ROLE g` 並設租戶後讀得到那個租戶的列 | 改用 `USAGE` |
| F3 | 低，多拒 | 角色以 `MEMBER` 列出，所以既不能繼承也不能 `SET ROLE` 的角色（1.7）與只繼承的角色（1.8，表上有給 p 的 `platform_access`）讓安全的連線被拒 | 改成 session 能 `SET ROLE` 的角色（`SET`，含自己） |

修正後重跑：37 種情境中只剩 2.6（`tenant_id = '<固定租戶>'`）與 2.7（`tenant_id IS NULL`）被接受，這是上面「範圍外與限制」記下的「不是全部租戶」限制；範本（`platform_access`、`platform_events`、`consumer_read`）、`tenant_db` 與 outbox 全部照舊通過，包括 `narrow_log_read`（`USING (tenant_id IS NULL)` 的讀取 policy）仍通過連線檢查、由 `ensure_subscription` 回 `CannotReadAllTenants`。本分支另加的回歸測試見「被拒的設定」與「接受的設定」中標示「驗證 F1／F2／F3」的列，突變測試確認還原任一項修正都會讓測試失敗。

F4（成本，另案處理，不在本 issue 最佳化）：300 張表時平台角色的連線檢查中位數

| 情境 | 修正前 | 修正後 |
| --- | --- | --- |
| 服務角色 | 7.9 ms | 7.6 ms |
| 平台角色本身 | 20.6 ms | 20.7 ms |
| 加 20 個沒有權限的群組 | 27.2 ms | 24.4 ms |
| 20 個對每張表都有權限、被 `platform_access` 指名的群組 | 183 ms | 178 ms |
| 60 個這樣的群組 | 738 ms | 703 ms（最大 3.27 s） |

60 個群組時已接近 readiness 檢查的 800 ms `CHECK_TIMEOUT`。成本隨持有權限的角色數增加，service-kit README 有記錄。

## 最終檢查

在合併 main `5fbe8ea` 後的 head 上、經 check queue（`CI=1`）以腳本 [final-checks.sh.txt](data/2026-10-13-366-platform-tenant-only/final-checks.sh.txt) 一次執行：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p service-kit --no-default-features --all-targets --locked -- -D warnings
cargo test --locked -p service-kit
cargo test --locked -p service-kit --no-default-features
cargo test --locked -p outbox
cargo test --locked -p testkit
cargo test --locked -p identity
cargo test --locked -p sql-check -p repo-check
cargo run --locked -p openapi -- --check
cargo test --locked -p service-kit --test tenant_db -- --nocapture
```

全部 exit 0：fmt、兩個 clippy；service-kit（`tenant_db` 13 passed，`postgres_start` 3 passed，其餘測試檔與 doc-test 全部 ok）；service-kit 不含 `contract`；outbox（lib 10、crash 2、delivery 13、failure_paths 3、handler_panic 7、metrics 1、notify 2、producer 3、tenancy 5、verify_271 13）；testkit（lib 23、container_volumes 2、postgres_isolation 4）；identity（lib 32、adversarial 11、cross_tenant 1、ext_authz 9、token_claims 8）；sql-check 20 + 6、repo-check 17 + 9；`api/openapi.json is up to date`。第一次在合併後跑時 sql-check 的 `the_workspace_has_no_string_built_sql` 失敗：`SESSION_CHECK` 新加的 SQL 註解寫了「SET ROLE」，sql-check 連 SQL 註解一起檢查，把它當成切換角色的 SQL；改寫註解後通過，規則與例外清單沒有改。原始輸出：[final-checks.log](data/2026-10-13-366-platform-tenant-only/final-checks.log)、`tenant_db` 的完整 `--nocapture` 輸出 [tenant-db.log](data/2026-10-13-366-platform-tenant-only/tenant-db.log)。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 平台角色對啟用 RLS 的表有某個指令的權限、而套用到它與該指令的 permissive policy 只有租戶條件時，`check_session` 拒絕連線並回新的 `UnsafeSession` 變體（真 Postgres，涵蓋 SELECT 與至少一個寫入指令） | 通過 | 「被拒的設定」：`SELECT`、`INSERT`、`UPDATE`、`DELETE` 四種指令，加上欄位權限、`PUBLIC`、群組繼承、`SET ROLE`、具名的租戶條件 policy、以別的寫法只開放自己租戶的 policy、指令不符、`UPDATE` 的 `WITH CHECK`、其他 schema；每種都證明了沒設租戶時碰不到、設了租戶就碰得到；突變測試證明測試抓得到規則失效；獨立驗證的 37 種情境 |
| 套用 `tenant-table.sql` 與 `outbox-event.sql` 後，服務角色、產生端平台角色、消費端平台角色（含與產生端平台角色是同一個角色）的連線仍被接受；`cargo test -p service-kit`、`cargo test -p outbox`、`cargo test -p testkit` 通過 | 通過 | 「接受的設定」與「最終檢查」 |
| service-kit README 與決策 29 記錄新規則 | 通過 | `crates/service-kit/README.md`（角色表、範本說明、檢查項目表與判斷細節、限制、測試表）、`docs/architecture/decisions.md` 決策 29 的實作細節；另更新 `sql/tenant-table.sql` 的註解、`crates/outbox/README.md`、`tools/sql-check` 對 `tenant_db.rs` 例外的理由 |
