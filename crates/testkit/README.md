# testkit

服務測試用的共用工具。主角是跨租戶負向測試框架：每個服務在 `tests/` 用它證明租戶 A 讀不到、也改不了租戶 B 的資料（[決策 29](../../docs/architecture/decisions.md)「跨租戶負向測試列入每個服務的 CI」）。另外提供測試用的內部身分 token、帶 TLS 的拋棄式 Postgres 與容器工具。

只能當 `[dev-dependencies]` 使用：`tools/repo-check` 擋下其他用法，因為 `tools/sql-check` 把本 crate 當測試程式檢查（它把巨集組好的靜態 SQL 以變數交給 sqlx）。

| 模組 | 內容 |
| --- | --- |
| `isolation` | 跨租戶負向測試：`Suite`、`Store`、`Report`、端點探測（`isolation::api`） |
| `isolation::postgres` | Postgres 的 store：`PgStore`、`pg_table!`、`pg_view!` |
| `identity` | `TestIssuer`：以測試行程產生的 Ed25519 金鑰簽內部身分 token，`jwks()` 就是服務的 `TRUSTIOT_INTERNAL_IDENTITY_KEYS` |
| `postgres` | `tls_postgres(init)`：帶 TLS 的拋棄式 Postgres（service-kit 只接受 `verify-full`），同一個測試執行檔共用一個容器 |
| `product` | `product_postgres(logins)`：在 `tls_postgres` 上以 migration Job 本身（`migrate::run` 與 `PRODUCT`）套用整個產品的 schema，與部署得到的一樣（Laysi/trustiot-v2#65）；見[產品 schema](#產品-schema) |
| `container` | podman 或 docker 的容器，drop 時刪除；`unavailable(what)`：本機缺容器執行環境時略過測試，設了 `CI` 時改為失敗 |

## 新服務怎麼接上

1. `Cargo.toml`：

   ```toml
   [dev-dependencies]
   testkit = { path = "../../crates/testkit" }
   ```

2. 建 `tests/cross_tenant.rs`。資料庫照服務正式的方式準備：以 migration 角色跑完全部 migration（含 service-kit 的 [`sql/database.sql`](../service-kit/sql/database.sql)），服務角色、平台角色照 [service-kit 的角色範本](../service-kit/README.md#角色與-policy-範本)。服務已登記在 `tools/migrate` 的 `PRODUCT` 時，直接用 `product_postgres`（見[產品 schema](#產品-schema)），outbox 的表與其他服務的 schema 也一起有了，例子是 `services/identity/tests/cross_tenant.rs` 與 `services/assets/tests/cross_tenant.rs`。
3. 啟動服務：用 `TestIssuer::jwks()` 當 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`，資料庫連線用服務角色、`verify-full` 與 `TlsPostgres::ca_pem`（以子行程啟動時把它寫成 `TRUSTIOT_POSTGRES_CA_FILE` 指向的檔案）。可以在測試行程裡以 `axum::serve` 跑服務的 router，或像 service-kit 的範例服務那樣啟動子行程。
4. 宣告每張表、每個端點，跑 suite：

```rust
use contract::ServiceName;
use serde_json::json;
use testkit::identity::TestIssuer;
use testkit::isolation::Suite;
use testkit::isolation::api::{Endpoint, Method};
use testkit::isolation::postgres::PgStore;
use testkit::postgres::tls_postgres;
use testkit::{pg_table, pg_view};

#[tokio::test]
async fn tenants_are_isolated() {
    let Some(postgres) = tls_postgres(ROLES).await else { return }; // 沒有容器：本機略過，CI 失敗
    // ……建資料庫、以 assets_owner 跑 migration……
    let issuer = TestIssuer::new();
    let base_url = start_assets(&postgres, &issuer.jwks()).await; // 服務自己的啟動函式

    let store = PgStore::new(
        postgres.options("assets_owner", "assets"), // seeder：不受 RLS 限制的 migration 角色
        postgres.options("assets_app", "assets"),   // 被攻擊的服務角色
    )
    .table(pg_table!(
        "devices",
        seed = "INSERT INTO devices (tenant_id, id, name) VALUES ($1, $2, $3)"
    ))
    .table(pg_view!("device_summaries"))
    .exempt("public.device_models", "全平台共用的型號目錄，沒有租戶資料");

    Suite::new(ServiceName::Assets, &issuer, store)
        .openapi(serde_json::to_value(assets::openapi()).unwrap())
        .endpoint(Endpoint::list("/v1/devices", "devices"))
        .endpoint(Endpoint::get("/v1/devices/{id}", "devices"))
        .endpoint(
            Endpoint::item(Method::PATCH, "/v1/devices/{id}", "devices")
                .body(json!({"name": "{canary}"})),
        )
        .endpoint(
            Endpoint::create(Method::POST, "/v1/devices", "devices")
                .body(json!({"name": "{canary}", "tenant_id": "{tenant}"})),
        )
        .exempt_endpoint("GET /v1/device-models", "沒有租戶資料")
        .run(Some(&base_url))
        .await
        .assert_isolated();
}
```

完整可執行的例子是 [`tests/postgres_isolation.rs`](tests/postgres_isolation.rs)：service-kit 的表範本 `example_items` 加上一個以 `TenantDb` 寫成的小服務。

## 宣告

新表、新端點各是一行宣告；忘了宣告的會讓測試失敗，不會被略過。

| 宣告 | 用在 | 說明 |
| --- | --- | --- |
| `pg_table!("name", seed = "…")` | 有 `tenant_id` 的表 | 名稱是字串字面值，`table`（在 `public`）或 `schema.table`，小寫、不加引號。seed 是 seeder 執行的靜態 SQL，插入一列：`$1` 租戶 id（uuid）、`$2` 新的列 id（uuid）、`$3` canary（text）；`$2` 或 `$3` 至少存一個在讀得到的欄位。需要父列的表用一個語句一起插：`WITH parent AS (INSERT INTO devices (tenant_id, id, name) VALUES ($1, gen_random_uuid(), $3) RETURNING tenant_id, id) INSERT INTO points (tenant_id, id, device_id, name) SELECT tenant_id, $2, id, $3 FROM parent` |
| `pg_table!("name", seed = "…", one_per_tenant)` | 每個租戶最多一列的表（`tenant_id` 就是主鍵，例如 `identity.tenants`、`assets.tenants`） | 每個租戶只種一列，不管端點要幾列；那一列的 id（`$2`）就是租戶 id，所以讀得到它的 `tenant_id` 就算讀得到那一列。Postgres 要每個參數都有型別，seed 沒存 `$2` 或 `$3` 時仍要提到它們，例如 `INSERT INTO assets.tenants (tenant_id) SELECT $1 WHERE $2::uuid IS NOT NULL AND $3::text IS NOT NULL` |
| `pg_view!("name")` | 建在租戶表上的 `security_invoker` 檢視 | 只讀；攻擊者至少要看到一列（正向對照） |
| `PgStore::exempt("schema.name", "理由")` | 服務角色用得到、但沒有租戶資料的關聯，例如 migration Job 在每個服務 schema 寫的 `trustiot_migrate_applied`（服務角色只能讀，Laysi/trustiot-v2#402） | 有 `tenant_id` 欄的不能豁免 |
| `Endpoint::list(path, table)` | 列出呼叫者租戶資料的 `GET` | 路徑沒有參數 |
| `Endpoint::get(path, table)`、`Endpoint::item(method, path, table)` | 針對單一列的請求 | 路徑剛好一個參數，例如 `/v1/devices/{id}`，代入列 id |
| `Endpoint::create(method, path, table)` | 建立資料的請求 | 路徑沒有參數；body 裡能指定租戶的地方寫 `{tenant}` |
| `.body(json)` | 需要 body 的端點 | 字串裡的 `{id}` 換成該請求的列 id、`{tenant}` 換成該請求的目標租戶（攻擊時是 B，正向對照時是 A）、`{canary}` 換成新的 canary |
| `Suite::openapi(doc)` | 有 HTTP API 的服務 | 服務的 OpenAPI 文件（`serde_json::to_value(openapi())`）；每個 operation 都要宣告或豁免 |
| `Suite::exempt_endpoint("METHOD /path", "理由")` | 沒有租戶資料的 operation，或另有手寫測試的巢狀路徑 | |
| `Suite::without_api("理由")` | 沒有 HTTP API 的服務（例如只消費事件） | 兩者都沒呼叫時測試失敗 |

## 怎麼測

`Suite::run` 每次產生兩個新租戶，所以共用的資料庫也能重跑。每張表為每個租戶種幾列（讀取共用一列，每個針對單一列的寫入端點在每一輪各用一列，讓 `DELETE` 等寫入的正向對照不影響其他探測；至少兩列），每列有隨機的 id 與 canary（`canary-a-…`、`canary-b-…`）；一個租戶的 marker 是它的租戶 id 與它所有列的 id、canary（id 以帶連字號與不帶連字號兩種寫法、不分大小寫比對）。任何回應或查詢結果出現對方的 marker 就算外洩；請求本身帶的 id（路徑或 body 的列 id、body 的 `{tenant}`）被回應原樣帶回不算，canary 與其他 id 照算。

1. **覆蓋**：查 `pg_class`，服務角色有任何權限（直接、經所屬角色或 `PUBLIC`）的表、partitioned table、檢視、物化檢視、foreign table（系統 schema 與 extension 的除外）都要宣告或豁免；宣告了卻不存在或沒授權、`pg_table!` 宣告的表沒有 `tenant_id`、豁免的表有 `tenant_id`、同一個名稱宣告兩次，都失敗。OpenAPI 的每個 operation 也一樣。
2. **種資料**：seeder（migration 角色或 superuser，不受 RLS 限制）執行 seed，再讀回每一列；讀不回 id 或 canary 就失敗並停止，避免拿空表做出假的通過。
3. **資料庫探測**：以 A 攻擊 B、再以 B 攻擊 A，全部經 service-kit 的 `TenantDb`（與 handler 同一條路，連線檢查也會跑）：

   | 探測 | 通過條件 |
   | --- | --- |
   | session | kit 接受服務角色的連線；RLS 關掉、policy 放寬等由 kit 的連線檢查拒絕，報告寫出原因 |
   | read | 看不到對方的 marker，而且看得到自己種的每一列（檢視：至少一列）；或被拒（`42501`） |
   | read without a tenant | 交易外看不到任何列，或被拒 |
   | update own rows | `UPDATE … SET tenant_id = tenant_id RETURNING` 回傳的列沒有對方的 marker，或被拒 |
   | move own rows | `UPDATE … SET tenant_id = <對方>` 被 `WITH CHECK` 拒絕（`42501`）；其他錯誤代表沒有證明 RLS 會擋，也算失敗 |
   | insert | 以對方的租戶執行 seed 被拒（`42501`） |

   寫入探測都在各自的交易裡，結束時 rollback。
4. **端點探測**：先以租戶 A 的 admin（權限最大的租戶使用者）攻擊 B，再以 B 的 admin 攻擊 A，每一輪都呼叫每個端點、讀取先於寫入（下表以 A 攻擊 B 為例）：

   | 端點 | 攻擊 | 沒有租戶的使用者 | 正向對照 |
   | --- | --- | --- | --- |
   | list | 回應沒有 B 的 marker | 回應沒有任何租戶的 marker | 2xx，A 種的每一列都在 |
   | get、item | 路徑帶 B 的列 id：403 或 404，回應沒有 B 的其他 marker | 帶 A 的列：不是 2xx、沒有 marker | 帶 A 的列：2xx（`GET` 要看得到該列） |
   | create | body 指向 B（`{tenant}`）：回應沒有 B 的 marker；有沒有寫進 B 由下一步的快照判斷 | 不是 2xx、沒有 marker | body 指向 A：2xx（建立本身能用，攻擊才有意義） |

   每一輪前後各讀一次被攻擊租戶的所有列（seeder 讀），有任何差異就失敗，所以任何端點寫到對方都抓得到。

每個探測都記在 `Report`；`assert_isolated()` 印出整份報告（`result | subject | probe | outcome`，不含 token），有失敗或一個探測都沒跑就讓測試失敗。以 `--nocapture` 執行可以看到報告，當作驗收證據。

## 證明測得出漏洞

`tests/postgres_isolation.rs` 以同一份宣告測試前四種狀況，另以一張每個租戶一列的表測 `one_per_tenant`，`cargo test -p testkit` 每次都跑：

| 測試 | 狀況 | 預期 |
| --- | --- | --- |
| `the_example_fails_with_rls_off_and_passes_with_it_on` | `example_items` 的 RLS 開著、關掉、再打開 | 通過、失敗（kit 拒絕連線並寫出沒開 RLS 的表，端點也失敗）、通過 |
| `a_forgotten_table_fails` | 多一張服務角色讀得到、但沒宣告的租戶表 | 只有覆蓋檢查失敗 |
| `an_endpoint_that_reads_across_tenants_fails` | `GET /v1/items/{id}` 改用 migration 角色讀（RLS 管不到） | 端點探測抓到 B 的資料；資料庫探測全部通過 |
| `an_endpoint_that_writes_across_tenants_fails` | `PUT /v1/items/{id}` 改用 migration 角色寫 | 回 200 而不是 404，而且 B 的列被改 |
| `a_table_with_one_row_per_tenant_is_attacked_too` | 一張 `one_per_tenant` 的表，RLS 開著、再關掉 | 開著時通過；關掉時失敗，失敗寫出那張表 |

## 擴充：ClickHouse 與 AGE

`isolation::Store` 是資料庫的介面：`check_coverage`（列出服務碰得到的物件、比對宣告）、`seed`（以不受隔離限制的身分種資料並讀回）、`attack`（以攻擊者的身分、經服務實際的路徑攻擊）、`snapshot`（讀出某租戶的所有列，比對端點探測前後）。ClickHouse 的 row policy（Laysi/trustiot-v2#59，攻擊清單見 `services/ingest/tests/clickhouse.rs`）與 AGE 的每租戶角色（MS4）各實作一個 store，沿用同一套 `World`、marker、`Report` 與端點探測。目前一次 `Suite::run` 測一個 store；同時用 Postgres 與 ClickHouse 的服務各跑一次。

## 限制

- 有參數的巢狀路徑（例如 `/v1/devices/{device_id}/points/{point_id}`）、列表路徑帶參數的端點還不支援：宣告時就失敗，請寫手動測試並以 `exempt_endpoint` 說明。
- 平台角色（`PlatformDb`）依設計可以跨租戶，不在這裡測；它的 policy 由 service-kit 的連線檢查與審查把關。
- 端點探測只用 admin 身分；租戶內角色（viewer、editor）的權限檢查是各服務自己的測試。
- 外洩以 marker 判斷：回應若把 id 轉成別的格式（例如 base64），就看不出來，這種端點要另外寫測試。
- 豁免的關聯不種資料也不攻擊，豁免理由要經審查。

## 產品 schema

`product::product_postgres(&["identity_app", ...])` 起一個 `tls_postgres`（`init` 是 `PRODUCT_INIT`：migration 角色 `trustiot_owner`，與 chart 的 bundled Postgres 一樣有 `CREATEROLE`、不是 superuser），建資料庫 `trustiot`，以 `migrate::run(&config, &PRODUCT)` 套用每個服務的角色、schema、outbox 的表與 policy、migration，與部署時 migration Job 做的完全相同。Job 不會讓沒有綁 Secret 的登入角色登入（維持 `NOLOGIN`），所以參數列出測試要用來登入的角色，只有它們以 `TEST_PASSWORD` 取得 `LOGIN`，就像 chart 的 Secret 給它們密碼。回傳的 `ProductPostgres` 有 `platform_env()`（服務子行程要的 `TRUSTIOT_POSTGRES_*`，CA 寫成暫存檔，drop 時刪除）與 `superuser()`（不受 RLS 限制的連線，給種資料與檢查用）。

角色是整個叢集共用的，Job 也拒絕擁有兩個資料庫的 migration 角色，所以一個容器只套用一次產品；同一個測試執行檔的測試共用它與資料庫 `trustiot`：每個測試用自己的資料（新的租戶、新的使用者），不要假設表是空的。需要 podman 或 docker，沒有時略過，設了 `CI` 時失敗。

## 其他工具

- `TestIssuer::new()`：每次產生新的金鑰；`tenant_token(audience, tenant, role)`、`token(&claims)` 簽 token，`user(audience, &claims)` 取得 handler 會拿到的 `UserIdentity`。
- `tls_postgres(init)`：image 與 service-kit 的測試相同（以 digest 鎖定），記憶體上限 512 MB；`init` 是以 superuser 在 `postgres` 資料庫執行一次的靜態 SQL（例如建立角色，密碼用 `TEST_PASSWORD`）。每個測試用自己的資料庫。容器經 `postgres::start`（與 service-kit、outbox、identity 的測試是同一個檔案，`crates/service-kit/tests/support/start.rs`）啟動：等待有上限（具名常數與量測依據見 [`docs/test-reports/2026-10-12-364-job-lock-postgres-start.md`](../../docs/test-reports/2026-10-12-364-job-lock-postgres-start.md)）、發佈的連接埠失效時換容器、失敗訊息寫明在等什麼並附容器狀態與日誌最後 20 行（不含環境變數）。`start_tls_postgres_within(limits, runtime, kind, published_port_dead)` 是 `tls_postgres` 的啟動部分，`tests/postgres_start.rs` 用它測這些行為。
- `Container`、`container_runtime()`、`unavailable(what)`：自己起其他容器時用。

## 執行

```sh
cargo test -p testkit                       # 單元測試與容器測試（需要 podman 或 docker）
cargo test -p testkit -- --nocapture        # 加上每個探測的報告
```

沒有 podman 或 docker 時容器測試在本機略過；設了 `CI` 時改為失敗，CI 綠燈就代表真的跑過。
