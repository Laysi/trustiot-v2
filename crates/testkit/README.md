# testkit

服務測試用的共用工具。主角是跨租戶負向測試框架：每個服務在 `tests/` 用它證明租戶 A 讀不到、也改不了租戶 B 的資料（[決策 29](../../docs/architecture/decisions.md)「跨租戶負向測試列入每個服務的 CI」）。另外提供測試用的內部身分 token、帶 TLS 的拋棄式 Postgres 與容器工具，閘道器採集測試用的假 Modbus 端點與故障注入（M7，見[假 Modbus 端點](#假-modbus-端點)），閘道器連線測試用的 WebSocket 前端、測試 CA 與 Mosquitto 容器（見[MQTT 測試環境](#mqtt-測試環境)），以及閘道器 TPM 測試用的軟體 TPM（見[軟體 TPM](#軟體-tpm)）。

只能當 `[dev-dependencies]` 使用：`tools/repo-check` 擋下其他用法，因為 `tools/sql-check` 把本 crate 當測試程式檢查（它把巨集組好的靜態 SQL 以變數交給 sqlx）。

| 模組 | 內容 |
| --- | --- |
| `isolation` | 跨租戶負向測試：`Suite`、`Store`、`Report`、端點探測（`isolation::api`） |
| `isolation::postgres` | Postgres 的 store：`PgStore`、`pg_table!`、`pg_view!` |
| `identity` | `TestIssuer`：以測試行程產生的 Ed25519 金鑰簽內部身分 token；`write_jwks(path)` 寫出公鑰檔，服務的 `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` 指向它（與 chart 掛進 pod 的 ConfigMap 同格式） |
| `postgres` | `tls_postgres(init)`：帶 TLS 的拋棄式 Postgres（service-kit 只接受 `verify-full`），同一個測試執行檔共用一個容器 |
| `product` | `product_postgres(logins)`：在 `tls_postgres` 上以 migration Job 本身（`migrate::run` 與 `PRODUCT`）套用整個產品的 schema，與部署得到的一樣（Laysi/trustiot-v2#65）；見[產品 schema](#產品-schema) |
| `container` | podman 或 docker 的容器，drop 時刪除；`unavailable(what)`：本機缺容器執行環境時略過測試，設了 `CI` 時改為失敗 |
| `modbus` | `FakeModbus`：測試行程內的假 Modbus TCP 與 RTU-over-TCP 端點，支援站號、故障注入（`Fault`）與請求紀錄；見[假 Modbus 端點](#假-modbus-端點) |
| `mqtt` | 閘道器到 broker 的整條路：`TestPki`、`front::WsFront`（入口與 mqtt-bridge 的替身）、`mosquitto::Mosquitto`；見[MQTT 測試環境](#mqtt-測試環境) |
| `swtpm` | `Swtpm`：拋棄式容器裡的軟體 TPM 2.0（swtpm），`session()` 像開一次 `/dev/tpmrm0`，`tools(script)` 在容器內跑 tpm2-tools；見[軟體 TPM](#軟體-tpm) |
| `clock` | 真實 socket 上的時間控制：`manual()` 讓暫停的 tokio 時鐘只在測試 `advance` 時前進；`bounded(what, future)` 以牆上時間限制等待；見[時間控制](#時間控制) |
| `chart` | chart 對服務的假設：`postgres_connection_budget(service)` 讀 `deploy/chart/templates/_postgres.tpl` 的 `trustiot.postgres.connectionBudgets`（每個副本的連線預算，Laysi/trustiot-v2#575）；`pool_size_calls(src)` 列出 crate 的 `src` 裡每個 `.max_connections(...)`。用 Postgres 的服務各有一個 `tests/chart_connections.rs`，以兩者確認程式的連線池與 chart 的預算相同，加了連線池就失敗（見 [chart 的 README](../../deploy/chart/README.md)「叢集內的 Postgres」） |

## 新服務怎麼接上

1. `Cargo.toml`：

   ```toml
   [dev-dependencies]
   testkit = { path = "../../crates/testkit" }
   ```

2. 建 `tests/cross_tenant.rs`。資料庫照服務正式的方式準備：以 migration 角色跑完全部 migration（含 service-kit 的 [`sql/database.sql`](../service-kit/sql/database.sql)），服務角色、平台角色照 [service-kit 的角色範本](../service-kit/README.md#角色與-policy-範本)。服務已登記在 `tools/migrate` 的 `PRODUCT` 時，直接用 `product_postgres`（見[產品 schema](#產品-schema)），outbox 的表與其他服務的 schema 也一起有了，例子是 `services/identity/tests/cross_tenant.rs` 與 `services/assets/tests/cross_tenant.rs`。
3. 啟動服務：以 `TestIssuer::write_jwks(path)` 寫出公鑰檔，`TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` 指向它，資料庫連線用服務角色、`verify-full` 與 `TlsPostgres::ca_pem`（以子行程啟動時把它寫成 `TRUSTIOT_POSTGRES_CA_FILE` 指向的檔案）。可以在測試行程裡以 `axum::serve` 跑服務的 router，或像 service-kit 的範例服務那樣啟動子行程。
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
    let base_url = start_assets(&postgres, &issuer).await; // 服務自己的啟動函式，以 issuer.write_jwks 寫公鑰檔

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
| `Suite::without_tenant_data("理由")` | 完全沒有租戶資料的服務：每個關聯都 `exempt`、每個 operation 都 `exempt_endpoint`，例如 `services/certs/tests/cross_tenant.rs` | 沒有它時，什麼都沒宣告的 suite 以「nothing is declared」失敗；有它時只跑涵蓋檢查，而一旦宣告了租戶表或要攻擊的端點就失敗，因為說法不再成立 |

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

## 假 Modbus 端點

M7 棄用了模擬器與測試資料產生器，自動測試改在測試行程內起最小的 Modbus 端點做故障注入（[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) M7 列）。`FakeModbus::start(transport)` 在 127.0.0.1 的臨時連接埠起一個端點，測試把被測程式指向 `address()`；端點 drop 時停掉所有工作、關掉所有 socket（包括半開的）。它不是模擬器：數值由測試設定，不會自己變化。現場的硬限制（[field-constraints](../../docs/requirements/field-constraints.md)）裡設備連著卻不回應、轉換器把回應送給所有連線，都能用它在 CI 重現；閘道器回報的讀取失敗類型（`crates/protocol` 的 `FailureKind`：connect、timeout、exception、unit_mismatch）也都做得出來。

### 用法

完整可執行的例子是 [`tests/modbus_example.rs`](tests/modbus_example.rs)：以別人寫的 Modbus client（`tokio-modbus`）讀一台 RS485 轉換器後面的兩台設備，再逐一觸發每種故障。節錄：

```rust
use std::time::Duration;

use testkit::clock;
use testkit::modbus::{Close, FakeModbus, Fault, Transport};
use tokio::time::{advance, timeout};

#[tokio::test(start_paused = true)]
async fn the_reader_survives_a_converter() {
    let _clock = clock::manual(); // 時鐘只在 advance 時前進
    let endpoint = FakeModbus::start(Transport::RtuOverTcp).await;
    endpoint.set_holding_registers(1, 0, &[2301, 2299]); // 站號 1
    endpoint.set_holding_registers(2, 0, &[500, 501]); // 站號 2，同一台轉換器

    let mut meter = client(&endpoint, 1).await; // 被測程式的 client，連 endpoint.address()

    // 不回應：client 等到逾時，連線照常可用
    endpoint.inject(Fault::no_response().once());
    let read = timeout(Duration::from_secs(3), meter.read_holding_registers(0, 2));
    let (read, ()) = tokio::join!(read, async {
        endpoint.wait_for_requests(1).await; // 請求到了才推進時鐘
        advance(Duration::from_secs(3)).await;
    });
    assert!(read.is_err());

    // 其他故障同樣先 inject 再讀：
    endpoint.inject(Fault::exception(0x04).once());
    endpoint.inject(Fault::disconnect(Close::Rst).once());
    // ……

    // 被測程式只讀：沒有任何功能碼大於 4 的請求
    assert!(endpoint.requests().iter().all(|request| (1..=4).contains(&request.function)));
}
```

`tests/modbus_endpoint.rs` 以 raw socket 逐位元組檢查回應，每種故障各有一個測試（`fault_*`）。

### 傳輸方式與站號

| | `Transport::ModbusTcp` | `Transport::RtuOverTcp` |
| --- | --- | --- |
| 代表 | 自己會講 TCP 的設備 | RS485 轉換器與它匯流排上的設備 |
| 框架 | MBAP，依長度欄切出請求；回應帶請求的交易 ID | RTU 加 CRC（低位元組在前）；TCP 不保留框架邊界，請求可能分段到達或與下一個黏在一起，所以依規格中各功能碼的請求格式決定長度（例如 1–6 是 8 bytes，15、16 是 9 加 byte count）；沒有固定格式的請求（自訂功能碼、Diagnostics 的 Return Query Data、MEI 14 以外的 43）切在第一個 CRC 正確的位置 |
| 處理順序 | 每條連線各自依序處理，一個等待中的請求只擋住同一條連線後面的請求 | 所有連線的請求依到達順序上同一條匯流排，等待中的請求擋住全部 |
| 沒有的站號 | 例外碼 0B（gateway target device failed to respond） | 不回應，就像匯流排上沒有那台設備 |
| 壞框架 | 協定 ID 不是 0 或長度不在 2–254 的 MBAP 標頭：關閉連線 | CRC 錯誤：記錄、不回應、不套用故障 |

一個端點可以有多個站號，各有自己的 coil、discrete input、holding register、input register：`set_coils`、`set_discrete_inputs`、`set_holding_registers`、`set_input_registers(unit, start, values)` 設值並建立該站號，`add_unit(unit)` 建立沒有值的站號。只服務功能碼 1–4，閘道器對現場設備唯讀；其他功能碼回例外碼 01，讀到沒設過的位址回 02，數量是 0 或超過 125 個暫存器、2000 個位元時回 03（先檢查數量再檢查位址，與規格相同）。

### 故障

`endpoint.inject(fault)` 注入，`clear_faults()` 全部移除。請求到達時，端點挑第一個符合條件、還有次數的故障，記在該請求的紀錄裡（`Request::fault`）；沒有符合的就正常回答。

| 故障 | 端點的行為 | client 看到的 | 閘道器的失敗類型 |
| --- | --- | --- | --- |
| `Fault::delay(d)` | 收到後經過 `d`（tokio 時鐘）才正常回答；後面的請求等它 | 逾時比 `d` 短時逾時，回應之後才到 | timeout |
| `Fault::hold()` | 測試呼叫 `release(index)` 才正常回答，不需要時鐘；先 release 再到達也可以 | 同上，由測試決定何時回 | timeout |
| `Fault::no_response()` | 讀進並記錄請求，永遠不回答；連線照常，後面的請求照答，TCP keepalive 偵測不到（Laysi/trustiot#159、#164）；RTU 的匯流排立刻空出來 | 逾時 | timeout |
| `Fault::unit_mismatch(u)` | 照常回答，但回應的站號是 `u`（RTU 的 CRC 照算）；本來就不回答的請求（RTU 沒有的站號）仍不回答 | 收到別台設備的回應 | unit_mismatch |
| `Fault::exception(code)` | 回例外碼 `code`，不管表裡有沒有值 | 例外回應 | exception |
| `Fault::disconnect(Close::Fin)` | 不回答，送 FIN；之後收到的請求照記錄、不回答，等 client 關閉自己那一端 | 讀到串流結尾 | 重連後回到正常 |
| `Fault::disconnect(Close::Rst)` | 不回答，以 `SO_LINGER` 0 關閉，送 RST | connection reset | 重連後回到正常 |
| `Fault::half_open()` | 忘了這條連線：不再讀寫，不送 FIN 也不送 RST，socket 留到端點 drop；新連線正常 | 寫入與 keepalive 都成功，只有回應逾時察覺得到 | timeout |
| `Fault::broadcast()` | 照常回答，回應送給所有開著的連線，就像把匯流排上的每個框架複製給所有 TCP client 的轉換器（Laysi/trustiot#183） | 另一個在等自己站號的 client 先收到別台的回應；Modbus TCP 上是別人的交易 ID | unit_mismatch |

條件與次數可以疊加，全部成立才算符合：`.unit(u)`、`.function(fc)`、`.address(a)`（讀取範圍包含 `a`，合併讀取也算）、`.connection(ConnectionId(n))`（第 n 條被接受的連線，從 0 起算，可以在 client 連線前指定）、`.nth(i)`（請求紀錄的第 i 筆，從 0 起算）；`.once()`、`.times(n)` 限制次數，預設每次都套用。`.delayed(d)`、`.held()`、`.to_all_connections()` 可以加在任何故障上，例如 `Fault::broadcast().held().unit(1)` 先暫停站號 1 的回應，放行時再廣播，讓另一個 client 的請求先排上匯流排。

### 觀察

- `requests()`：請求紀錄，每筆有 `index`、`connection`、`unit`、`function`、`range`（讀取的起始位址與數量）、`transaction_id`（Modbus TCP）、`raw`、`checksum_ok` 與套用的 `fault`。證明被測程式沒送出功能碼 5 以上，就檢查這裡。
- `connections()`：每條被接受的連線與狀態：`Open`、`FinSent`、`Forgotten`（半開）、`Closed`。
- `wait_for_requests(n)`、`wait_for_connections(n)`、`wait_for_connection(id, state)`：等到條件成立。client 的 connect 在端點接受連線之前就回傳，所以要廣播給所有連線時，先 `wait_for_connections`。等待以牆上時間計，上限是 `clock::WAIT_LIMIT`（60 秒），逾時的失敗訊息寫出在等什麼。

不要用固定的等待時間判斷「沒有回應」：用順序證明，例如在同一條連線再送一個請求，先收到的是它的回應，就表示前一個沒被回答（`fault_no_response_leaves_the_connection_healthy`）。

### 時間控制

會碰到逾時的測試，用 `#[tokio::test(start_paused = true)]` 加上 `let _clock = testkit::clock::manual();`：時鐘只在測試呼叫 `tokio::time::advance` 時前進，被測程式的逾時與端點的延遲都依到期時間的先後觸發，與機器的負載無關。

只用 `start_paused` 不夠：runtime 沒事做時 tokio 會把暫停的時鐘直接推到下一個計時器，而資料還在 socket 裡時 runtime 也算沒事做，於是 client 的逾時提早觸發。2026-10-19 實測：延遲 1 秒、逾時 3 秒的讀取做 2000 次，只用 `start_paused` 有 501 次誤判逾時，未加負載或負載平均 22 都一樣；加上 `clock::manual()` 為 0 次（[測試報告](../../docs/test-reports/2026-10-19-97-fake-modbus-endpoint.md)）。

- `manual()` 依靠 tokio 文件寫明的行為：有 `spawn_blocking` 的工作在跑時，暫停的時鐘不自動前進（`tokio::time::pause` 的「Preventing auto-advance」）。只在 current-thread runtime 有效，`start_paused` 本來就只能用在它上；守衛存在時佔用一條 blocking 執行緒。
- 先等請求到達（`wait_for_requests`），再 `advance`；被測程式的逾時要在 `advance` 之前建立（`tokio::time::timeout` 在建立時就算好到期時間）。以 `tokio::join!` 讓讀取與推進時鐘同時進行，見上面的節錄與 `fault_delay_answers_once_the_clock_reaches_the_delay`。
- 不想碰時鐘時用 `Fault::hold()` 與 `release(index)`。
- `clock::bounded(what, future)` 以看門狗執行緒計牆上時間，暫停的時鐘下也有效；testkit 的等待都經過它。

### 限制

- 只服務讀取，見[傳輸方式與站號](#傳輸方式與站號)。
- RTU-over-TCP 沒有線速與字元間隔，一個請求在匯流排上一次處理完；要模擬線速就用 `Fault::delay`。
- 沒有字元間隔，框架的邊界只能從內容判斷：沒有固定格式的請求如果 CRC 錯誤，端點要在這條連線上收滿 RTU 的上限 256 bytes 才會把它切成一個 CRC 錯誤的框架，在那之前同一條連線後面的請求都切不出來（其他連線不受影響）。有固定格式的請求（包括功能碼 1–4）CRC 錯誤時照常依長度切開。
- Modbus TCP 的連線各自處理，沒有模擬 TCP 閘道器背後的序列匯流排；轉換器的行為用 `RtuOverTcp` 測。
- 半開連線的 socket 一直留到端點 drop；client 持續寫入會填滿 kernel 的接收緩衝，之後 client 的寫入會卡住而不是失敗，與現場相同。

## MQTT 測試環境

閘道器到 broker 的整條路，給 agent 的測試用（Laysi/trustiot-v2#86）。模組 `mqtt`：

- `TestPki`：測試時產生的兩個 CA，不寫進 repo。「公開」CA 簽前端的憑證（`FRONT_HOST`，即 `localhost`），是 agent 的外層信任；broker CA 是根 CA，簽 broker 的憑證（`BROKER_NAME`，即 agent 固定的 `mqtt-broker.trustiot.internal`）與閘道器的憑證（`pki.gateway(cn)`，EKU clientAuth），是 agent 的內層信任與 broker 的用戶端 CA。另有 `Authority::new(name)` 做不相干的根 CA（測拒絕用）、`Authority::issuing(name)`（這個 CA 簽的簽發 CA，pathLen 0，像正式環境的根 → 簽發 CA → broker 與閘道器憑證）、`Authority::leaf(names, usages)`（指定 EKU 的憑證，例如只有 clientAuth）、`Authority::crl(number, revoked, next_update)`（這個 CA 簽的 PEM CRL，撤銷它簽的那些 CA；nextUpdate 可以是過去的日期）、`mtls_server_with_chain(leaf, chain, client_ca)`（在 leaf 之後送出簽發 CA）、`front_tls(leaf, versions)`、`mtls_server(leaf, client_ca)`（要求用戶端憑證的 TLS 1.3 伺服器；`mtls_server_with_versions` 可指定 TLS 版本，測拒絕用）、`Authority::client_valid(cn, not_before, not_after)`（指定效期的用戶端憑證，測過期用）、`Authority::client_for_key(cn, public_key, not_before, not_after)`（為既有的公鑰簽閘道器憑證，profile 同 certs 服務，效期精確到秒；憑證換發的測試用）與 `ScratchDir`（drop 時刪除的暫存目錄，放設定與金鑰檔）。
- `front::WsFront`：測試行程內的入口與 mqtt-bridge 替身：外層 TLS（ALPN `http/1.1`，依用戶端的套件順序選）、`/mqtt` 上 subprotocol 為 `mqtt` 的 WebSocket（路徑不對回 404、沒要求 `mqtt` 回 400），再把 binary message 原樣轉給 TCP 上游（8 KiB 緩衝，與 mqtt-bridge 相同）。內層 mTLS 由閘道器直達上游，前端只看到加密的位元組。`sessions()` 記錄每條連線協商出的 TLS 版本、套件、ALPN、SNI、是否 resumption、路徑與 subprotocol；`FrontOptions` 可改成不回 subprotocol、定時送 WebSocket ping、換外層 TLS 設定（例如只有 TLS 1.2）。`stop().await` 一次切斷所有連線、不送 WebSocket close（與轉接服務當掉相同），等 listener 關閉、每條連線都結束才回來；`restart()` 先這樣停下再在同一個 port 重新開始，所以不會因為舊的 listener 還開著而 `AddrInUse`。停下時（`stop`、`restart` 或 drop）先對 listener `shutdown` 再關閉：同一個測試行程裡別的測試啟動子行程（例如 podman）時，子行程在 fork 到 exec 之間拿著所有描述子的副本，只 drop 的 listener 會繼續 listen（重新綁定 `AddrInUse`、連線照樣成功）；`shutdown` 讓 socket 本身離開 LISTEN。`Mosquitto` 挑 port 時放掉的試探 listener 也一樣處理。
- `mosquitto::Mosquitto`：拋棄式容器裡的 Mosquitto 2.1.2（`MOSQUITTO_IMAGE`，以 multi-arch digest 釘選，與 `crates/protocol/tests/broker.rs` 相同），設定照決策 6 給閘道器的 listener：mTLS（`require_certificate`、`use_identity_as_username`、TLS 1.3）、`set_tcp_nodelay`、dynamic security 與協定的閘道器角色（`protocol::acl::gateway_role_acls`）、不允許匿名。`ciphers_tls1.3` 把 ChaCha20-Poly1305 排第一：Mosquitto 依自己的順序選套件，不設時即使閘道器把 ChaCha20 排第一也協商出 AES-256-GCM（2026-10-21 實測）。它不是 chart 的設定（`deploy/chart/files/mosquitto/mosquitto.conf`，Laysi/trustiot-v2#84）：chart 另外檢查 CRL、只接受 MQTT 5、以 username 當 client ID；`ciphers_tls1.3` 與 `set_tcp_nodelay` 兩邊相同（chart 在 2026-10-23 補上，Laysi/trustiot-v2#564）。mTLS listener 發布在主機的回送位址（`address()`）；容器內另有只用密碼的 listener，給 dynamic security 指令與雲端帳號（`CLOUD_USER`）的 `mosquitto_pub`／`mosquitto_sub` 以 `exec` 使用。憑證由環境變數傳進容器，不經過指令列。

```rust
let pki = TestPki::new();
let Some(broker) = Mosquitto::start(&pki) else { return };   // 沒有容器執行環境時略過
broker.register_gateway("gw-test-1");                          // dynsec client，綁定閘道器角色
let front = WsFront::start(FrontOptions::new(pki.front_tls(), broker.address())).await?;
let up = broker.subscribe("trustiot/v1/gw/gw-test-1/up/#");    // 雲端帳號在背景訂閱
// agent 的設定：cloud.url = wss://localhost:<front port>/mqtt，trust = pki.public_ca.pem，
// broker.ca = pki.broker_ca.pem（名稱固定是 BROKER_NAME），憑證與金鑰 = pki.gateway("gw-test-1")
let online = up.wait_for("online", limit, |message| message.topic.ends_with("/up/status"));
broker.remove_gateway_rule("publishClientSend", "trustiot/v1/gw/%u/up/log");   // 之後的 log 拿到 0x87
broker.restart();                                              // SIGTERM 後重新啟動，retained 訊息消失
```

`Mosquitto` 的方法（`publish`、`subscribe`、`restart`、`wait_for_log`、`control`）會阻塞（它們執行容器指令），在 async 測試裡以 multi-thread runtime 加 `tokio::task::block_in_place` 呼叫，agent 的 task 才不會被卡住。`start` 先挑一個空的 port 再交給容器執行環境，中間被別人拿走時換一個 port 重試（最多 5 次）；`Subscription` 被丟掉時連容器裡的 `mosquitto_sub` 一起停掉。`publish` 回傳 PUBACK 的 reason code；`Subscription::wait_for` 略過不要的訊息、逾時時列出看到的 topic。broker 的日誌（`logs()`、`wait_for_log`）含連線的來源位址，不要原樣貼進測試報告。

## 軟體 TPM

閘道器的 TPM 測試用（Laysi/trustiot-v2#86、Laysi/trustiot-v2#611），模組 `swtpm`。容器裡是 swtpm（libtpms）與 tpm2-tools，測試經 swtpm 的原始指令埠（只發佈在主機的回送位址）送 TPM 2.0 指令，tpm2-tools 在容器內用來準備與檢查 TPM。

- `Swtpm::start()`：一顆全新的 TPM（已做 TPM2_Startup，與閘道器的核心相同），等到它回應才回來。沒有 podman 或 docker 時回 `None`（本機略過，設了 `CI` 時失敗）；drop 時連同狀態刪除容器。
- `swtpm.session()`：相當於一個行程開啟 `/dev/tpmrm0`。`transact(command)` 送一個完整的指令、收回完整的回應（最多 4096 bytes）；session 被丟掉時 flush 它載入的暫時物件與 session，與核心的資源管理器在行程關閉裝置（或行程死掉）時做的一樣。swtpm 一次只服務一條連線，所以所有 session 共用一條 TCP 連線，以鎖讓每個指令完整送完；兩個執行緒各拿一個 session，就像兩個行程同時使用 TPM。agent 的測試把 session 包成自己的 `TpmChannel`（`TpmTransport::Channel`）。資源管理器另外會虛擬化 handle、在物件槽不夠時換出 context，這裡沒有：libtpms 同時只能載入 3 個暫時物件（`TPM2_PT_HR_TRANSIENT_AVAIL`）。
- `swtpm.tools(script)`：在容器內的新暫存目錄以 `sh -eu` 執行 `script`（`TPM2TOOLS_TCTI` 已指向這顆 TPM），回傳 stdout，失敗或超過 120 秒就 panic。執行前先關掉共用的連線（tpm2-tools 要自己的連線），期間持有鎖，所以不會有 session 的指令送到一半；之後的 session 指令會重新連線，TPM 的狀態不受影響。
- `create_standard_srk()`：在 `0x81000001` 放閘道器上那種標準的 ECC SRK（`tpm2_createprimary -C o -g sha256 -G ecc256:aes128cfb`）。`persistent_handles()`、`transient_handles()` 列出 handle；`snapshot()` 是 TPM 內容的文字快照（持久、暫時與 NV 的 handle，每個持久物件的 `tpm2_readpublic`，變動的屬性，包括字典攻擊計數 `TPM2_PT_LOCKOUT_COUNTER`），用來比對一個步驟前後 TPM 有沒有變。
- 映像以 `src/swtpm/Containerfile`（Debian trixie 的 swtpm 與 tpm2-tools，基底以 multi-arch digest 釘選）建置，build context 是空目錄，同一個行程只建一次，留在本機為 `localhost/testkit-swtpm:<Containerfile 的 SHA-256 前 8 bytes>`，下次直接沿用，不要時以 `podman rmi` 移除。apt 套件沒有釘版本，重建時可能拿到 Debian 的新修正版。

```rust
let Some(swtpm) = Swtpm::start() else { return };   // 沒有容器執行環境時略過
swtpm.create_standard_srk();
let mut session = swtpm.session();
let response = session.transact(&command)?;           // 一個 TPM 2.0 指令
let before = swtpm.snapshot();
// ...
assert_eq!(swtpm.snapshot(), before);
```

## 執行

```sh
cargo test -p testkit                       # 單元測試與容器測試（需要 podman 或 docker）
cargo test -p testkit -- --nocapture        # 加上每個探測的報告
cargo test -p testkit --test modbus_endpoint --test modbus_example   # 只跑假 Modbus 端點（不需要容器）
cargo test -p agent --test transport --test broker                    # MQTT 測試環境的使用者（broker 需要容器）
cargo test -p agent --test tpm_swtpm --test tpm_provision             # 軟體 TPM 的使用者（需要容器）
```

沒有 podman 或 docker 時容器測試在本機略過；設了 `CI` 時改為失敗，CI 綠燈就代表真的跑過。
