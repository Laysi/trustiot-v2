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
