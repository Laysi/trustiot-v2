# 跨租戶負向測試框架（testkit）驗收

對應 issue：Laysi/trustiot-v2#56。

## 環境

- 機器：共用建置主機 `ruisi-wsl`（Ubuntu on WSL2，x86_64，Docker），以 `rcheck.sh` 執行，設 `CI=1`（缺容器執行環境時測試失敗而不是略過）。
- 工具：`.prototools` 與 `rust-toolchain.toml` 鎖定的 Rust 1.99.0。
- Postgres：`docker.io/library/postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873`，TLS（測試行程產生的 CA），連線一律 `verify-full`，容器記憶體上限 512 MB。
- 分支：`feat/56-cross-tenant-testkit`（基於 `e8f180b`）。
- 範例：service-kit 的 `sql/database.sql` 與 `sql/tenant-table.sql`（表 `example_items`，角色 `example_owner`、`example_app`、`example_platform`），加上 `crates/testkit/tests/postgres_isolation.rs` 裡以 `TenantDb` 寫成的小服務（`GET/POST /v1/items`、`GET/PUT/DELETE /v1/items/{id}`）。

## 指令

```sh
bash rcheck.sh 56 'cargo clippy --locked -p testkit --all-targets -- -D warnings 2>&1 | tail -3; cargo test --locked -p testkit -- --nocapture --test-threads=1 2>&1 | grep -v "^\s*Compiling"'
bash rcheck.sh 56 'set -e; cargo fmt --all --check; cargo clippy --workspace --all-targets --locked -- -D warnings 2>&1 | tail -15; cargo test --workspace --locked 2>&1 | grep -E "^test result|FAILED|panicked|^error|Running" | tail -120; cargo deny --locked check 2>&1 | tail -8'
```

`rcheck.sh` 在建置主機上設 `CI=1`。本機重跑：`CI=1 cargo test -p testkit -- --nocapture`（需要 podman 或 docker）。

## 結果

`cargo test -p testkit`：單元測試 22 個、容器測試 4 個、doctest 1 個，全部通過，clippy 無警告。原始輸出：[cargo-test-testkit.txt](data/2026-10-11-56-cross-tenant-testkit/cargo-test-testkit.txt)（id 與 canary 每次隨機）。

| 容器測試 | 狀況 | suite 的結果 |
| --- | --- | --- |
| `the_example_fails_with_rls_off_and_passes_with_it_on` | RLS 開著 | 43 個探測全部通過 |
| 同上 | `ALTER TABLE example_items DISABLE ROW LEVEL SECURITY` | 34 個探測 12 個失敗：kit 拒絕服務角色的新連線（`table public.example_items has a tenant_id column but row-level security is not enabled`，A、B 各一）；服務 pool 裡 RLS 關掉前就通過檢查的連線照常查詢，所以端點真的外洩，A 攻擊 B、B 攻擊 A 兩輪各 5 個失敗：`GET /v1/items` 回 200 並含對方的 id 與 canary、`GET /v1/items/{id}` 回對方的列、`DELETE /v1/items/{id}` 刪掉對方的列（204）、`PUT /v1/items/{id}` 改到對方的列（200）、對方的列快照前後不同 |
| 同上 | 重新 `ENABLE ROW LEVEL SECURITY` | 43 個探測全部通過 |
| `a_forgotten_table_fails` | 多一張有 RLS、服務角色讀得到、但沒宣告的 `example_notes`（只測資料庫，`without_api`） | 15 個探測只有覆蓋檢查 1 個失敗：`public.example_notes | coverage | the service role can use this table but the suite does not declare it` |
| `an_endpoint_that_reads_across_tenants_fails` | `GET /v1/items/{id}` 改用 migration 角色讀 | 43 個探測 4 個失敗：兩輪各有攻擊者讀到對方的列（200 含對方的 canary）、沒有租戶的使用者讀到列；資料庫探測全部通過 |
| `an_endpoint_that_writes_across_tenants_fails` | `PUT /v1/items/{id}` 改用 migration 角色寫 | 43 個探測 6 個失敗：兩輪各有改對方的列回 200（應為 403/404）、對方的列在該輪後被改（快照差異）、沒有租戶的使用者也改得到 |

RLS 開著時的 43 個探測（完整見原始輸出）：覆蓋 1、seed 1（每租戶 5 列：讀取共用 1 列，`DELETE` 與 `PUT` 在 A、B 兩輪各用 1 列）、資料庫探測 11（A→B 與 B→A 各 session、read、update own、move、insert，加一次 read without a tenant）、端點探測 28（每輪 14）、被攻擊租戶的列快照 2（每輪 1）。

全 workspace（`cargo fmt --check`、`cargo clippy --workspace --all-targets -D warnings`、`cargo test --workspace`、`cargo deny --locked check`）：全部通過，`tools/sql-check` 的整個 workspace 檢查與 `tools/repo-check` 新增的 `dev_only_crates_are_only_dev_dependencies` 也在內；cargo deny 為 `advisories ok, bans ok, licenses ok, sources ok`。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 框架附範例：關掉某張表的 RLS 時範例測試失敗、開啟時通過（證明測得出漏洞） | 通過：`the_example_fails_with_rls_off_and_passes_with_it_on` 同一份宣告在 RLS 開、關、再開三個階段分別通過（43/43）、失敗（12 個）、通過（43/43）；失敗時報告寫出沒開 RLS 的表，並由端點探測抓到實際外洩的 B 資料。另有漏宣告的表、跨租戶讀寫的端點三種漏洞也都被抓到 |
| `crates/testkit/README.md` 說明新服務如何接上 | 通過：README 的「新服務怎麼接上」與「宣告」兩節 |

觀察（不在本 issue 範圍）：service-kit 只在建立新連線時檢查 RLS 設定，執行中關掉 RLS 時，pool 裡既有的連線仍可使用；本次端點外洩即由此而來。migration 不應關掉 RLS，若要在執行期也擋下，需要 service-kit 定期重新檢查。
