# outbox_aggregate 的租戶隔離（2026-10-15）

對應 issue：Laysi/trustiot-v2#413（決策 4、29）。原始資料在 [data/2026-10-15-413-outbox-aggregate-isolation/](data/2026-10-15-413-outbox-aggregate-isolation/)。做法、不採用的做法與升級行為寫在 [crates/outbox/README.md 的「聚合的租戶隔離」](../../crates/outbox/README.md#聚合的租戶隔離laysitrustiot-v24132026-10-15)。

## 1. 環境

- **受測程式碼**：分支 `fix/413-outbox-aggregate-isolation`，從 main `8e29d03` 開出；受測的樹就是加入本報告的 commit，除了本報告與 `data/` 目錄。
- **執行位置**：建置主機（x86_64 Linux，Docker），經檢查佇列，`CI=1`，所以需要容器的測試沒有容器時會失敗而不是略過。Postgres 是各 crate 測試共用的 TLS Postgres（`postgres:18.6-alpine`，以 digest 鎖定），連線一律 `verify-full`。
- **工具**：Rust 1.99.0、bun 1.4.2、helm 與 kubeconform，皆由 `.prototools` 鎖定。
- **沒有 Kubernetes**：這個變更只動資料庫的 schema 與程式庫，沒有用到叢集的測試。

## 2. 指令

在 repo 根目錄，經檢查佇列（同一個工作，依序以 `&&` 串接）：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo test --locked -p service-kit --no-default-features
bun scripts/ci/check-secret-handling.ts
bun test deploy/chart

# 負向測試與順序測試的完整輸出
cargo test --locked -p outbox --test tenancy the_producer_roles -- --nocapture
cargo test --locked -p outbox --test delivery an_aggregate_keeps_its_order -- --nocapture
cargo test --locked -p service-kit --test tenant_db outbox_aggregates -- --nocapture
cargo test --locked -p identity --test cross_tenant -- --nocapture
cargo test --locked -p migrate --test outbox_and_tenant_roles -- --nocapture
```

突變測試（之後還原）：分別從 `crates/service-kit/sql/outbox-aggregate.sql` 拿掉 `platform_aggregates_only` 與 `ENABLE ROW LEVEL SECURITY`，再跑 `cargo test --locked -p service-kit --test tenant_db outbox_aggregates -- --nocapture` 與 `cargo test --locked -p identity --test cross_tenant -- --nocapture`。

## 3. 結果

### 整套檢查

全部通過（結束碼 0，[workspace-checks.txt](data/2026-10-15-413-outbox-aggregate-isolation/workspace-checks.txt)）：fmt 與兩種 clippy 沒有警告；`cargo test --workspace` 每個測試執行檔都是 `ok`、沒有 `FAILED`，其中 `outbox` 的 `tenancy` 6 個、`delivery` 13 個、`verify_271` 13 個，`service-kit` 的 `tenant_db` 15 個，`identity` 的 `cross_tenant`、`tenants`，`assets` 的 `cross_tenant`、`tenant_created`，`migrate` 的 `outbox_and_tenant_roles`、`fresh_database`、`registry`，`testkit`、`sql-check`、`repo-check` 都通過；不含 `contract` 的 service-kit 測試通過；`check-secret-handling` 沒有違規；`bun test deploy/chart` 350 個通過、0 個失敗。

### 負向測試

[negative-tests.txt](data/2026-10-15-413-outbox-aggregate-isolation/negative-tests.txt) 是完整輸出。

| 測試 | 驗證什麼 | 結果 |
| --- | --- | --- |
| `crates/outbox/tests/tenancy.rs` 的 `the_producer_roles_reach_only_their_own_aggregates` | 產生端的服務角色經真正的 `publish`、`aggregate_version` 與直接 SQL：租戶 B 的交易只看得到 `widget/of-b`；租戶 A 與全平台的聚合讀成 `None`；`UPDATE` 改不到（0 列）；直接建立 A 或全平台的聚合、把自己的聚合搬到 A、為 A 或全平台的聚合 `publish` 都是 42501。沒有租戶時看不到任何聚合。平台角色只看得到 `catalog/shared`，為 A 的聚合寫全平台事件、寫 A 的事件都是 42501。被拒之後每個聚合的租戶、序號與排序交易都沒變，A 下一筆事件的序號是 2、`tx_order` 不減 | 通過 |
| `crates/service-kit/tests/tenant_db.rs` 的 `outbox_aggregates_follow_the_tenant_policies` | 直接執行範本：既有聚合拿到最後一筆事件的租戶（A 的 `widget/a`、B 的 `widget/b`、全平台的 `catalog/c`，沒有事件的維持 NULL）；三條 policy 的確切寫法（`tenant_isolation` 等於連線檢查認得的租戶條件）；連線檢查接受服務、平台、消費端平台角色；`publish` 的 upsert 遇到 B 或全平台的聚合時以 42501 拒絕（`USING expression`），不推進它；不帶 `tenant_id` 的舊版寫法拿到交易的租戶；平台角色沒有租戶、在 `PlatformDb` 交易裡自己設租戶、在一般 session 以交易或 session 層級設租戶，都只看得到全平台的聚合，碰 A 的聚合被拒（`platform_aggregates_only`） | 通過 |
| `services/identity/tests/cross_tenant.rs` | 產品 schema 上，`identity.outbox_aggregate` 由豁免改為 `pg_table!`：A、B 兩個方向各五種探測（讀只看到自己的 2 列、無租戶讀不到、更新只碰到自己的列、搬到對方 42501、插入對方 42501），49 個探測 0 個失敗 | 通過 |
| `tools/migrate/tests/outbox_and_tenant_roles.rs` | 第一次執行建立三條 policy；把 schema 改回範本之前的樣子並放進舊版寫的事件後再跑 Job：變更只有 `applied obx-producer.outbox/outbox-aggregate.sql`，聚合拿到最後一筆事件的租戶，RLS 開啟，連線檢查接受各登入角色，服務角色沒有租戶時看到 0 個聚合，再跑一次沒有變更、catalog 快照相同 | 通過 |
| `crates/outbox/tests/delivery.rs` 的 `an_aggregate_keeps_its_order_when_transaction_ids_are_inverted` | 聚合內有序（交易 ID 反轉時仍依寫入順序送達） | 通過 |

### 突變測試

[mutations.txt](data/2026-10-15-413-outbox-aggregate-isolation/mutations.txt) 是輸出。

| 突變 | 結果 |
| --- | --- |
| 範本拿掉 restrictive 的 `platform_aggregates_only` | `tenant_db` 的 policy 清單檢查先失敗；把那個檢查也反過來後，行為檢查失敗：平台角色在自己設了租戶 A 的交易裡看得到 A 的 `widget/a` 與 `widget/old-binary` |
| 範本拿掉 `ENABLE ROW LEVEL SECURITY` | 身分服務的跨租戶測試失敗：service-kit 拒絕 `identity_app` 的連線（`outbox_aggregate` 有 `tenant_id` 但沒開 RLS） |

兩個突變都已還原，`outbox-aggregate.sql` 的 SHA-256 與突變前相同。

## 4. 結論

| 驗收條件 | 結果 |
| --- | --- |
| 服務角色在租戶交易裡讀不到、也改不了其他租戶聚合的列，有負向測試 | 通過：上表前三個測試；跨租戶負向測試涵蓋這張表，不再豁免 |
| 聚合內有序的既有測試仍然通過，例如 `an_aggregate_keeps_its_order_when_transaction_ids_are_inverted` | 通過：該測試與整個 `outbox` 測試 |
| `crates/outbox/README.md` 的權限一節已更新 | 通過：「資料庫權限」與新的「聚合的租戶隔離」一節 |

另外確認：migration Job 在已跑過舊版的資料庫上收斂（只套用新檔案一次、已套用的檔案沒有修改、下一次執行沒有變更）；`cargo test -p repo-check` 與 `bun scripts/ci/check-secret-handling.ts` 通過；API 規格沒有變（`cargo test --workspace` 的 openapi 檢查通過）。
