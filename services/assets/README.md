# assets（資產服務）

租戶的場域、設備與資料點的擁有者（決策 4；[部署與可搬移性](../../docs/architecture/deployment.md)）。

目前有：

- 自己的 schema `assets`、服務角色與平台角色、RLS（MS0 骨架，Laysi/trustiot-v2#65，2026-10-14）。
- 消費身分服務的 `identity.tenant_created`：每個新租戶在這裡都有一列（見[租戶](#租戶)）。這是第一個跨服務事件，示範其他服務怎麼從事件建立自己的租戶資料。
- 一個需要內部身分 token 的範例 API：`GET /api/assets/v1/tenant` 回呼叫者的租戶。
- 設備型號的 API 與它的事件 `assets.device_model_*`（Laysi/trustiot-v2#99，2026-10-21，見[設備型號](#設備型號)）。
- 還沒有場域與設備。

## 設備型號

`src/device_models/`（rebuild checklist C1、F5）：租戶的暫存器對應表，同型的設備共用一個型號，閘道器依它輪詢。資料點的格式就是閘道器設定用的 `protocol::config::DataPoint`（M4，只定義一次），規則也是 `crates/protocol` 的那一套（`config::validate_model_points`），所以存下的型號一定通過閘道器設定的檢查。

| 請求 | 角色 | 成功 |
| --- | --- | --- |
| `GET /api/assets/v1/device-models` | viewer 以上 | 200，`{"models": [...]}`：依名稱排序，每筆有 `model_id`、`name`、`revision`、`point_count`、`created_at`、`updated_at` |
| `POST /api/assets/v1/device-models` | editor 以上 | 201，整個型號，`Location` 指向它；寫 `assets.device_model_created` |
| `POST /api/assets/v1/device-models/import` | editor 以上 | 同上；body 是匯出的文件 |
| `GET /api/assets/v1/device-models/{model_id}` | viewer 以上 | 200，整個型號（`model_id`、`name`、`revision`、`created_at`、`updated_at`、`points`） |
| `GET /api/assets/v1/device-models/{model_id}/export` | viewer 以上 | 200，`{"format": "trustiot.device_model", "format_version": 1, "name", "points"}`，`Content-Disposition: attachment; filename="<名稱>.device-model.json"`；不含 ID、版本與時間，可以匯入到別的租戶或部署 |
| `PUT /api/assets/v1/device-models/{model_id}` | editor 以上 | 200，更新後的型號，`revision` 加一；寫 `assets.device_model_updated`。body 是 `{"points", "name"?, "revision"?}`：`points` 整批取代舊的資料點；`name` 有送就必須是原名（名稱建立後不能改）；`revision` 有送就必須是目前的版本，否則 409，避免蓋掉別人的修改 |
| `DELETE /api/assets/v1/device-models/{model_id}` | editor 以上 | 204，型號與資料點一起刪除；寫 `assets.device_model_deleted` |

**規則**：三個寫入入口（建立、更新、匯入）都先以 `document.rs` 逐欄讀 JSON，再跑 `crates/protocol` 的跨欄與跨資料點規則，全部通過才寫入；不通過就 422 `validation_failed`，每個問題一個 field error（JSON Pointer 與 `device_model.*` message key），最多列 100 個，其餘以 `details.omitted_field_errors` 計數，什麼都不寫、也不寫事件。

| 欄位 | 規則 | message key |
| --- | --- | --- |
| 未知欄位（含 `tenant_id`） | 一律拒絕；欄位名稱是 64 字以內的可見 ASCII 時才出現在 pointer 裡 | `unknown_field` |
| 必填欄位缺漏或 `null` | `name`、`points`；資料點的 `key`、`function_code`、`address`、`data_type`；匯入的 `format`、`format_version` | `field_required` |
| `name` | 小寫英數字以單一連字號連接、字母開頭、1–40 字（`protocol::ModelName`）；租戶內唯一（重複 409 `name_taken`）；建立後不能改（422 `name_immutable`） | `name_invalid`（`details.max` 40） |
| `points` | 陣列，最多 1000 個，可以是空的 | `points_invalid`、`too_many_points` |
| `key` | 1–64 個 ASCII 英數字、`.`、`_`、`-`，英數字開頭（`protocol::PointKey`）；型號內唯一 | `point_key_invalid`、`point_key_duplicate` |
| `function_code` | 只有讀取功能碼 1–4 | `function_code_invalid` |
| `address` | 0–65535 的整數；資料點的暫存器不能超過 65535，同一功能碼的位址不能重疊（四種功能碼各自一個位址空間） | `address_invalid`、`address_out_of_range`、`points_overlap` |
| `data_type` | `bool`、`u16`、`i16`、`u32`、`i32`、`u64`、`i64`、`f32`、`f64`、`string`；功能碼 1、2 只能是 `bool` | `data_type_invalid`、`function_type_mismatch` |
| `byte_order` | `abcd`（預設）、`badc`、`cdab`、`dcba`；單一位元沒有位元組順序，字串只能 `abcd` 或 `badc` | `byte_order_invalid`、`byte_order_unexpected` |
| `length` | 只有 `string` 有、而且必填：1–125 個暫存器 | `length_invalid`、`length_required`、`length_unexpected` |
| `scale`、`offset` | 有限的數字，`scale` 不為 0；只有數值型別能縮放（F5：值為 `raw * scale + offset`） | `scaling_invalid` |
| `description` | 給人與 AI 讀的說明：去掉前後空白後 1–512 字（空字串視為沒有），不能有換行與 tab 以外的控制字元、格式字元（零寬字元、雙向覆寫）或行與段落分隔字元 | `description_invalid`（`details.max` 512） |
| 匯入的 `format`、`format_version` | 必須是 `trustiot.device_model` 與 `1` | `format_unsupported`、`format_version_unsupported` |

其他錯誤（M1）：沒有有效的內部身分 token 401；沒有租戶或角色不夠 403；`model_id` 不是 UUID 或不是呼叫者租戶的型號 404；寫入的 `Content-Type` 不是 `application/json` 400（跨來源的頁面無法不經 CORS preflight 送出這個型別，所以不能借使用者的 cookie 改型號）；body 超過 2 MiB 400 `body_too_large`；body 不是 JSON 物件 400 `body_invalid`；刪除仍有設備使用的型號 409 `in_use`；租戶的 `identity.tenant_created` 還沒處理 503 `tenant_not_ready`；資料庫出錯 503。每個回應都帶 `cache-control: no-store`。每次成功的寫入記一行 INFO（`created a device model`、`updated a device model`、`deleted a device model`），只帶 `user_id`、`tenant_id`、`model_id`、`revision`，不記名稱、鍵與說明。

**仍被使用的型號不能刪**：`device_model_points` 以外，參照型號的表（設備，Laysi/trustiot-v2#100）必須以 `FOREIGN KEY (tenant_id, model_id) REFERENCES assets.device_models (tenant_id, model_id) ON DELETE RESTRICT` 參照，刪除時資料庫回 SQLSTATE 23001（`restrict_violation`；`NO ACTION` 的外鍵回 23503，兩者都對應），API 回 409 `in_use`。設備表出現前，`tests/device_models.rs` 以一張同樣參照的替身表驗證這條路。

**事件**（`contract::events::assets`，aggregate `device_model`、ID 是型號 ID，在變更的同一個租戶交易裡寫入 `assets.outbox_event`）：

| 事件 | payload |
| --- | --- |
| `assets.device_model_created` | `model_id`、`revision`（1）、`model`：`protocol::config::DeviceModel`（名稱與資料點，閘道器設定的格式，不含說明） |
| `assets.device_model_updated` | 同上，`revision` 是更新後的版本 |
| `assets.device_model_deleted` | `model_id`、`name`、`revision`（最後的版本） |

閘道器設定（Laysi/trustiot-v2#93）只靠事件就能維護自己的型號副本。它登記為消費端之前，`tools/migrate` 的 registry 讓 `assets` 暫時當自己的讀者（producer 至少要有一個讀者），沒有任何訂閱讀這些事件。

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

schema `assets`，DDL 在 `migrations/postgres/`，由 migration Job（登記在 `tools/migrate/src/registry.rs` 的 `PRODUCT`：有平台角色、消費 `identity` 的事件、發出自己的事件）以 migration 角色依檔名順序套用；執行期程式碼不建表、不改表。檔案清單在 `src/schema.rs`，Job 與服務的 readiness 編譯同一份：資料庫缺少這一版預期的檔案（或套用了別的版本）時 `/readyz` 回 503，日誌寫出檔案（Laysi/trustiot-v2#402，[service-kit 的 migration 檢查](../../crates/service-kit/README.md#migration-檢查migrations)）。

| 角色 | 權限 |
| --- | --- |
| `assets_owner`（migration 角色） | 擁有 schema 與所有表 |
| `assets_app`（服務角色） | `assets.tenants` 的 `SELECT`、`INSERT`；`device_models` 的 `SELECT`、`INSERT`、`UPDATE`、`DELETE`；`device_model_points` 的 `SELECT`、`INSERT`、`DELETE`；`outbox_event` 的 `INSERT`、`outbox_aggregate` 的 `SELECT`、`INSERT`、`UPDATE`（Job 照 outbox 範本授權，只限自己租戶的列）；消費端自己的表（Job 授權） |
| `assets_platform`（平台角色） | 讀 `identity.outbox_event`（Job 授權）；照 outbox 範本能寫平台層級（沒有租戶）的事件。在閘道器設定（Laysi/trustiot-v2#93）登記為消費端之前，`assets` 暫時是自己事件的讀者，所以它也讀得到 `assets.outbox_event`（每個租戶的型號事件，見[設備型號](#設備型號)），沒有任何訂閱讀它；讀不到 `assets` 的其他租戶資料 |

| 表 | 內容 | RLS |
| --- | --- | --- |
| `tenants` | `tenant_id`（主鍵）、`created_at` | 照 `crates/service-kit/sql/tenant-table.sql` 的 `tenant_isolation` |
| `device_models` | `tenant_id`、`model_id`（兩者是主鍵）、`name`（租戶內唯一，`CHECK` 也守命名規則）、`revision`、`created_at`、`updated_at`；`tenant_id` 參照 `tenants` | `tenant_isolation` |
| `device_model_points` | 每個資料點一列：`position`（順序）、`key`（型號內唯一）、`function_code`、`address`、`data_type`、`byte_order`、`length`、`scale`、`value_offset`、`description`，各欄有 `CHECK` 守單欄的範圍；跨欄與跨資料點的規則（重疊、型別與功能碼）只在 API 檢查；以 `(tenant_id, model_id)` 參照 `device_models`，`ON DELETE CASCADE` | `tenant_isolation` |
| `outbox_event`、`outbox_aggregate` | 這個服務發出的事件與每個 aggregate 的序號（由 Job 照 [outbox 範本](../../crates/service-kit/sql/outbox-event.sql) 建立） | 範本的政策：服務角色只寫自己租戶的事件 |
| `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version` | 消費端的記錄（由 Job 建立，見 [crates/outbox](../../crates/outbox/README.md)） | 無：不是租戶資料 |
| `trustiot_migrate_applied` | migration Job 在這個 schema 套用過的檔案（component、檔名、SHA-256），由 Job 寫入；`assets_app` 只能 `SELECT`，給 readiness 的 migration 檢查用，`assets_platform` 讀不到 | 無：不是租戶資料 |

兩個連線池：服務角色的 `TenantDb`（API 與消費端每個事件的交易，`BackgroundTenantDb` 只交給消費端），以及平台角色的 `PlatformDb`（消費端以唯讀交易讀事件，2 條連線，`EVENT_READER_CONNECTIONS`）。readiness 要兩個角色都開得出通過 service-kit session 檢查的連線，而且資料庫有這一版預期的每個 migration 檔案。

## 設定

設定由 `service_kit::config` 載入（kit README 的「設定」）。

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 平台設定：驗證內部身分 token 的公鑰檔（chart 掛進每個 pod 的 ConfigMap `<release>-internal-identity-keys` 的 `jwks.json`）；每 5 秒重讀，換公鑰不必重啟，還沒有可用的公鑰時不就緒（service-kit README「公鑰與輪替」） |
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

整合測試都需要 podman 或 docker：以 `testkit::product` 起一個帶 TLS 的暫時 Postgres，由 migration Job 的程式套用整個產品的 schema，再以 `assets_app`、`assets_platform` 啟動真的 binary。沒有容器時略過，設了 `CI` 時改為失敗。

| 測試 | 驗證 |
| --- | --- |
| `tests/tenant_created.rs` | 以身分服務自己的 `TenantStore` 建立租戶：資產服務最後有那一列，API 只回 token 租戶的資料；重播整個事件記錄、同一個事件再送一次，都不多一列、`created_at` 不變，日誌依序記 `created` 為 true、false、false，沒有 dead letter；沒有 token、錯的受眾 401，沒有租戶 403，還沒處理的租戶 404 |
| `tests/cross_tenant.rs` | [crates/testkit](../../crates/testkit/README.md) 的跨租戶負向測試：服務角色讀不到也改不了其他租戶的 `tenants`、`device_models`、`device_model_points`，寫不了其他租戶的事件與 aggregate 序號；API（租戶、型號的列表、讀取、匯出、更新、刪除、建立、匯入）不把一個租戶的資料給另一個租戶，也不寫進另一個租戶；消費端自己的表與 migration 紀錄不是租戶資料而豁免 |
| `tests/device_models.rs` | Laysi/trustiot-v2#99：建立、匯入、讀取、匯出、更新、刪除各寫出對的列與事件（`revision` 依序、事件內容就是閘道器格式的型號），匯出的文件匯入後相同；每一條命名與資料點規則經建立、更新、匯入三個入口都回 422 與對的 field error，什麼都不寫；重複的名稱 409、改名 422、過期的 `revision` 409；被參照的型號刪除 409 `in_use`；viewer 只能讀、沒有租戶 403、沒有 token 401；還沒處理到 `identity.tenant_created` 的租戶 503 |
| `tests/migrations.rs` | Laysi/trustiot-v2#402：Job 寫的紀錄就是 `src/schema.rs` 預期的清單，服務 ready 並記 INFO；以 superuser 拿掉 `0001_assets.sql` 的紀錄時，新的行程 503，ERROR 日誌的 `missing` 寫出它，已經通過的行程維持 ready；紀錄回來後同一個行程轉為 ready；SHA-256 不同時 503，`changed` 寫出它；資料庫多一個較新版本的紀錄（映像被回退）時照常 ready，INFO 的 `newer` 寫出它；`assets_app` 只讀得到自己的紀錄、不能改，讀不到 `identity` 的紀錄與 `trustiot_migrate.applied`；`assets_platform`（能進 `identity` schema 讀事件）讀不到任何一個服務的紀錄；`identity_app` 讀不到 `assets` 的紀錄 |
