# assets（資產服務）

租戶的場域、設備與資料點的擁有者（決策 4；[部署與可搬移性](../../docs/architecture/deployment.md)）。

目前是 MS0 的骨架（Laysi/trustiot-v2#65，2026-10-14）：

- 有自己的 schema `assets`、服務角色與平台角色、RLS。
- 消費身分服務的 `identity.tenant_created`：每個新租戶在這裡都有一列（見[租戶](#租戶)）。這是第一個跨服務事件，示範其他服務怎麼從事件建立自己的租戶資料。
- 一個需要內部身分 token 的範例 API：`GET /api/assets/v1/tenant` 回呼叫者的租戶。
- 還沒有場域、設備與資料點。

## 租戶

`src/tenants.rs`：

- **消費端**：訂閱名稱 `assets.identity-events`（`SUBSCRIPTION`，改名等於從頭重讀事件，所以不改），讀 `identity` schema 的 outbox（[crates/outbox](../../crates/outbox/README.md)）。收到 `identity.tenant_created` 時，在事件的租戶交易裡 `INSERT INTO assets.tenants (tenant_id) ... ON CONFLICT (tenant_id) DO NOTHING`：RLS 只放行那個租戶的列，`tenant_id` 是整個主鍵，所以重播（`outbox::Replay`）或同一個事件送兩次都不會多一列。身分服務的其他事件略過。每處理一個 `identity.tenant_created` 記一行 INFO `applied identity.tenant_created`，帶 `tenant_id`、`event_id` 與 `created`（這次是否新增了列）；不記租戶名稱（事件本身也不帶名稱）。沒有租戶的 `identity.tenant_created` 是永久錯誤，進 dead letter。
- **API**：`GET /api/assets/v1/tenant`（`TENANT_PATH`，入口不去除前綴）。

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 使用者沒有選租戶、或不是成員（token 沒有 `tenant`） | 403 `permission_denied` |
| 這個服務還沒處理到該租戶的 `identity.tenant_created` | 404 `not_found` |
| 資料庫出錯 | 503 `unavailable`（ERROR 日誌只記 `user_id` 與錯誤種類，不記資料） |
| 其他 | 200，`{"tenant_id", "created_at"}`（`created_at` 是這個服務知道這個租戶的時間），`cache-control: no-store` |

讀取在 token 租戶的 RLS 交易裡進行（`TenantDb::begin`），讀出的列不是那個租戶就當作錯誤，不回傳。

## 資料庫

schema `assets`，DDL 在 `migrations/postgres/`，由 migration Job（登記在 `tools/migrate/src/registry.rs` 的 `PRODUCT`：有平台角色、消費 `identity` 的事件）以 migration 角色依檔名順序套用；執行期程式碼不建表、不改表。

| 角色 | 權限 |
| --- | --- |
| `assets_owner`（migration 角色） | 擁有 schema 與所有表 |
| `assets_app`（服務角色） | `assets.tenants` 的 `SELECT`、`INSERT`；消費端自己的表（Job 授權） |
| `assets_platform`（平台角色） | 只讀 `identity.outbox_event`（Job 授權），讀不到 `assets` 的任何租戶資料 |

| 表 | 內容 | RLS |
| --- | --- | --- |
| `tenants` | `tenant_id`（主鍵）、`created_at` | 照 `crates/service-kit/sql/tenant-table.sql` 的 `tenant_isolation` |
| `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version` | 消費端的記錄（由 Job 建立，見 [crates/outbox](../../crates/outbox/README.md)） | 無：不是租戶資料 |

兩個連線池：服務角色的 `TenantDb`（API 與消費端每個事件的交易，`BackgroundTenantDb` 只交給消費端），以及平台角色的 `PlatformDb`（消費端以唯讀交易讀事件，2 條連線，`EVENT_READER_CONNECTIONS`）。readiness 要兩個角色都開得出通過 service-kit session 檢查的連線。

## 設定

設定由 `service_kit::config` 載入（kit README 的「設定」）。

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS` | 平台設定：驗證內部身分 token 的公鑰 |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 平台設定：Postgres 位置；只接受 `verify-full`，`_CA_FILE` 必填 |
| `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | 服務角色 `assets_app` 的帳密：密碼來自 Secret `<release>-assets-password` 的 `assets_app` key |
| `TRUSTIOT__DATABASE__PLATFORM_USERNAME`、`TRUSTIOT__DATABASE__PLATFORM_PASSWORD` | 平台角色 `assets_platform` 的帳密：密碼來自同一個 Secret 的 `assets_platform` key。chart 只對 outbox 消費端綁平台角色的密碼（`_migrate.tpl` 的 `trustiot.migrate.platformLogins`），migration Job 產生這兩個密碼並設定角色 |
| `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 服務角色連線池每個副本的上限，2–100，預設 10；平台角色另有 2 條 |

健康埠（預設 8081）只有 `/healthz` 與 `/readyz`，API 在 8080。設定不能用時以 exit code 1 結束。

## 映像

`deploy/images/assets/image.toml`：與身分服務相同，交叉編譯成 `aarch64-unknown-linux-musl` 的靜態執行檔，以 `deploy/images/rust-binary.Containerfile` 包成映像（[deploy/images/README.md](../../deploy/images/README.md)）。chart 的 `services.assets.enabled` 預設 `false`；nightly 的 smoke 以從原始碼建出的映像啟用它與身分服務，經入口建立租戶並讀它（[CI 與發佈](../../docs/architecture/ci-release.md) 的「nightly.yml」）。

## 測試

在 repo 根目錄執行：

```sh
CARGO_BUILD_JOBS=2 cargo test -p assets
```

兩個整合測試都需要 podman 或 docker：以 `testkit::product` 起一個帶 TLS 的暫時 Postgres，由 migration Job 的程式套用整個產品的 schema，再以 `assets_app`、`assets_platform` 啟動真的 binary。沒有容器時略過，設了 `CI` 時改為失敗。

| 測試 | 驗證 |
| --- | --- |
| `tests/tenant_created.rs` | 以身分服務自己的 `TenantStore` 建立租戶：資產服務最後有那一列，API 只回 token 租戶的資料；重播整個事件記錄、同一個事件再送一次，都不多一列、`created_at` 不變，日誌依序記 `created` 為 true、false、false，沒有 dead letter；沒有 token、錯的受眾 401，沒有租戶 403，還沒處理的租戶 404 |
| `tests/cross_tenant.rs` | [crates/testkit](../../crates/testkit/README.md) 的跨租戶負向測試：服務角色讀不到也改不了其他租戶的 `tenants`，API 不把一個租戶的資料給另一個租戶；消費端自己的表不是租戶資料而豁免 |
