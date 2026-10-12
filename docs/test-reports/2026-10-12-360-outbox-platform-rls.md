# outbox_event 範本：產生端平台角色只寫得進全平台事件（2026-10-12）

對應 issue：Laysi/trustiot-v2#360

## 環境

- 建置主機（check queue）：x86_64 Linux（WSL2），20 核心，Docker；每個測試起一個用完即刪的 `postgres:18.6-alpine`（TLS，`verify-full`）容器。指令經 check queue 執行，環境變數 `CI=1`（沒有容器執行環境時測試失敗而不是略過）。
- Rust 依 `rust-toolchain.toml`（1.99.0）；sqlx 版本依 `Cargo.lock`。
- 分支 `fix/360-outbox-platform-role-rls`，基於 main `b1a4ad3`；之後 rebase 到 main `b2b9232`（無衝突），最終檢查是在 rebase 後重跑的結果。「舊範本」指 `b1a4ad3` 的 `crates/service-kit/sql/outbox-event.sql`（三條 permissive policy，`b2b9232` 也相同），搭配本分支的新測試。

## 指令

```sh
# 舊範本 + 新測試（預期失敗，證明測試抓得到問題）：在 check queue 的副本裡換回舊檔
cp old-outbox-event.sql crates/service-kit/sql/outbox-event.sql   # 內容為 git show b1a4ad3:crates/service-kit/sql/outbox-event.sql
cargo test --locked -p service-kit --test tenant_db -- \
    outbox_events_follow_the_tenant_policies the_platform_role_gains_nothing_by_setting_a_tenant --nocapture

# 新範本
cargo test --locked -p service-kit --test tenant_db -- --nocapture
cargo test --locked -p outbox

# 最終檢查（一次批次）
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy --package service-kit --no-default-features --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo test --package service-kit --no-default-features --locked
cargo run --locked -p openapi -- --check
```

## 結果

原始輸出（已去掉 `Compiling` 行與建置主機的路徑）：[old-vs-new.log](data/2026-10-12-360-outbox-platform-rls/old-vs-new.log)、[final-checks.log](data/2026-10-12-360-outbox-platform-rls/final-checks.log)。

### 平台角色自己設定租戶後寫入租戶事件

| 情況 | 舊範本 | 新範本 |
| --- | --- | --- |
| `PlatformDb` 交易內 `set_config('trustiot.tenant_id', <租戶 A>, true)`，INSERT 租戶 A 的事件 | **寫入成功**（`rows_affected: 1`，測試 FAILED） | 42501：`new row violates row-level security policy "platform_events_only"` |
| 同上，租戶 B | （測試在上一步已停止） | 42501 |
| 一般連線（不經 service-kit），交易層級 `set_config(..., true)` | 同上 | 42501 |
| 一般連線，session 層級 `set_config(..., false)` | 同上 | 42501 |
| 設了租戶後寫全平台事件（`tenant_id` NULL） | — | 可以（照舊） |

`outbox_events_follow_the_tenant_policies` 的其他部分在新範本下照舊：服務角色寫目前租戶的事件成功、寫別的租戶與全平台事件 42501；平台角色不設租戶寫全平台事件成功、寫租戶事件 42501；消費端平台角色讀到所有事件（租戶 A 與全平台各一筆，被拒的寫入一筆都沒有進去）；`check_session` 對 `example_app`（服務角色規則）、`example_platform` 與 `example_reader`（平台角色規則）都回 `Ok(())`；消費端平台角色以服務角色規則檢查仍被拒（`PermissivePolicy("public.outbox_event policy consumer_read")`）。

### `tenant-table.sql` 的 `platform_access`

`the_platform_role_gains_nothing_by_setting_a_tenant`（新舊範本都通過：`tenant-table.sql` 的 policy 沒有改，測試記錄的是檢查結論）：

| 平台角色在 `PlatformDb` 交易內設了租戶 A 之後 | 結果 |
| --- | --- |
| 讀有 `platform_access` 的 `example_items` | `a-1, a-2, b-1`，與沒設租戶時相同（沒有多看到列） |
| INSERT、DELETE `example_items`（範本不給這兩個權限） | 42501 `permission denied for table example_items` |
| 讀沒有 `platform_access`、也沒給平台角色權限的表 | 42501 `permission denied for table private_items` |
| 給了平台角色 SELECT、卻沒有 `platform_access` 的表 | 沒設租戶時 0 列，設了租戶 A 後看到租戶 A 的列：範本因此改寫成「不加 `platform_access` 就不給平台角色任何權限」 |

### 全部測試與檢查

- 新範本：`tenant_db` 12 passed；`cargo test -p outbox` 全部通過（lib 10、crash 2、delivery 13、failure_paths 3、handler_panic 7、metrics 1、notify 2、producer 3、tenancy 5、verify_271 13）。outbox 的每個測試都以新範本建立 `outbox_event` 的 RLS，消費端的 `ensure_subscription` 讀取檢查照舊接受。
- 第一次批次中 `cargo test -p sql-check` 的 `the_workspace_has_no_string_built_sql` 失敗：兩份範本新加的註解寫了「set `trustiot.tenant_id`」，sql-check 檢查 `include_str!` 引入的檔案全文（含 SQL 註解），把它當成設定租戶的 SQL（`crates/outbox/tests/support/mod.rs` 與 `crates/testkit/tests/postgres_isolation.rs` 引入這兩份範本）。改寫註解的用字後通過，規則與例外清單沒有改。
- 最終檢查（rebase 到 `b2b9232` 後）：六項全部通過（每項 exit 0）：fmt、兩個 clippy、`cargo test --workspace`（79 個測試執行檔與 doc-test 全部 ok，含 `tenant_db` 12 passed、outbox 全部、`sql-check` 20 + 6、`repo-check`）、`cargo test -p service-kit --no-default-features`、openapi `api/openapi.json is up to date`。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 產生端的平台角色即使設定了 `trustiot.tenant_id`，INSERT 租戶事件也會被拒（真 Postgres） | 通過 | 上方第一張表；舊範本上同一個測試失敗 |
| 平台角色寫全平台事件、服務角色寫目前租戶的事件、消費端平台角色讀取所有事件仍然可行，連線檢查仍接受套用範本的連線 | 通過 | `outbox_events_follow_the_tenant_policies`、`cargo test -p outbox` |
| `tenant-table.sql` 的 `platform_access` 已檢查 | 通過 | 上方第二張表；結論：有 `platform_access` 時沒有多拿到權限；只有「給權限卻沒有 `platform_access`」時會多拿到，範本與 README 改寫為不給權限 |
| 範本與 README 已更新；`cargo test -p service-kit`、`cargo test -p outbox` 通過 | 通過 | 最終檢查 |

## 獨立驗證（2026-10-13）

PR Laysi/trustiot-v2#367（head `a094ab9`）另由獨立驗證在 check queue 的建置主機上以真的 Postgres 18.6 攻擊新範本。原始資料：[independent-verification/](data/2026-10-12-360-outbox-platform-rls/independent-verification/)。其中兩份探針原始碼 `verify_360.rs.txt`（service-kit）與 `outbox-verify_360.rs.txt`（outbox）是驗證者在執行後依實際執行的內容重建的（探針沒有 commit，原始檔已刪除），rustfmt 的排版可能與執行時略有不同，檔頭有註明；`run1-rcheck.txt`、`run2-rcheck.txt`、`run3-rcheck.txt` 是三次執行的輸出（run2 的 1 個失敗是探針本身重複 `PREPARE` 的錯誤，run3 修正後 3 passed）。

### 攻擊了什麼、結果

- **產生端平台角色寫租戶事件**（交易層級與 session 層級各自設定租戶 A 後）：單列與多列 VALUES、`INSERT ... SELECT`、data-modifying CTE、`PREPARE`/`EXECUTE`、extended protocol 綁定參數、`ON CONFLICT DO NOTHING`、`MERGE ... WHEN NOT MATCHED THEN INSERT`、plpgsql `DO` 區塊、`RESET ROLE`／`SET ROLE NONE` 之後再 INSERT，全部 42501 `platform_events_only`。`ON CONFLICT DO UPDATE`、帶 `WHEN MATCHED` 的 MERGE、`INSERT ... RETURNING`、UPDATE、DELETE、TRUNCATE、`CREATE TRIGGER` 因為沒有權限而 42501；`SET LOCAL row_security = off` 42501；`COPY FROM STDIN` 0A000；建立 view、函式、schema、暫存表、rule，關閉 RLS 或改 policy 全部被拒。擁有者在前後數到的租戶事件數相同，沒有任何一筆寫進去。
- **服務消費自己的事件**（`example_reader` 與 `example_platform` 是同一個角色）：寫租戶事件全部 42501（這時平台角色多了 SELECT，`INSERT ... RETURNING` 也由 `platform_events_only` 擋下）；寫全平台事件、讀取全部事件、`READS_ALL_TENANTS` 為 true、`check_session` 接受都照舊。用 outbox 的真 `Consumer` 跑：`outbox::publish(None)` 成功，自己設定租戶後 `publish(Some(..))` 42501，`ensure_subscription` 成功，drain 結果 delivered 3、dead_lettered 0。
- **切換角色**：`SET ROLE`、`SET LOCAL ROLE`、`SET SESSION AUTHORIZATION`、`set_config('role' | 'session_authorization', ...)` 全部 42501；`RESET ALL` 後 INSERT 被 RLS 擋下；唯一能切換的 `example_platform_group` 沒有任何權限。
- **合法的寫入與讀取**：服務角色寫目前租戶的事件、平台角色寫全平台事件、`example_reader` 讀全部事件、三個角色的 `check_session` 都照舊；`tenant-table.sql` 的 `platform_access` 下，平台角色設租戶前後 UPDATE 影響的列數相同（3）。
- 同時跑的套件：`tenant_db` 12 passed、`cargo test -p outbox` 全部 ok、`cargo test -p testkit` 全部 ok。

結論：修正成立，沒有找到能讓產生端平台角色寫進租戶事件的方法。

### 發現與修正（都是低嚴重度的文件用字，在同一個 PR 修正）

| 發現 | 修正 |
| --- | --- |
| `outbox-event.sql` 第 5 點註解說 INSERT 是平台角色「唯一的指令」；服務消費自己的事件時平台角色也有 SELECT | 改成「唯一的寫入指令」，並註明消費自己的事件時它也有 SELECT |
| `tools/sql-check` 對 `tenant_db.rs` 例外的理由（`src/lib.rs` 與 README）說平台角色「也沒有多拿到其他表的列」，但同一個測試記錄了有權限、沒有 `platform_access` 的表會讓它看到它設定的租戶的列 | 改成：驗證 RLS 不讓它寫租戶的 outbox 事件、有 `platform_access` 的表沒有多給它列，並記錄它有權限而沒有 `platform_access` 的表會讓它看到它設定的租戶的列（中英文同義） |
| `crates/outbox/README.md`「資料庫權限」最後一段說消費端的平台角色不能寫產生端的事件；服務消費自己的事件時它就是產生端的平台角色，有 INSERT | 補上這個例外：它有 INSERT，但 `platform_events_only` 只讓它寫全平台事件（`tenant_id IS NULL`） |

### 既有資料庫

目前沒有服務套用 `outbox-event.sql`（`services/identity` 的 migration 沒有 outbox，`services/ingest` 用 ClickHouse），所以不需要 migration。範本不是冪等的：已套用舊版範本的資料庫重跑整份範本會在 `CREATE POLICY tenant_isolation` 失敗，所以將來若有資料庫已套用舊版，要另寫只含 `CREATE POLICY platform_events_only` 的 migration。

### 修正後的檢查

合併 main `0bd0ab4` 並修正上述文件後，經 check queue 執行：

```sh
cargo fmt --all --check
cargo clippy --locked -p sql-check -p service-kit -p outbox --all-targets -- -D warnings
cargo test --locked -p sql-check
cargo test --locked -p repo-check
cargo test --locked -p service-kit --test tenant_db
cargo test --locked -p outbox
```

結果：六項全部 exit 0（`sql-check` 20 + 6、`repo-check` 15 + 7、`tenant_db` 12 passed、`cargo test -p outbox` lib 10、crash 2、delivery 13、failure_paths 3、handler_panic 7、metrics 1、notify 2、producer 3、tenancy 5、verify_271 13）。原始輸出：[after-verification-fixes.log](data/2026-10-12-360-outbox-platform-rls/after-verification-fixes.log)。
