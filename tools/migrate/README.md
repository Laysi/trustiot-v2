# migrate

部署時的 migration Job（Laysi/trustiot-v2#60，決策 4、14、29，清單 M8）：套用每個服務的 Postgres migration 與 ClickHouse DDL，冪等地建立服務的資料庫角色，並保留每租戶角色的 hook。chart 怎麼執行它、服務怎麼等它，見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「migration Job」；架構層級的說明在 [docs/architecture/deployment.md](../../docs/architecture/deployment.md)；維運步驟在 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md)。

| 指令 | 作用 |
| --- | --- |
| `migrate run`（或不帶參數） | 一次 migration（chart 的 Job）。結束碼 0 完成、1 失敗、2 設定或參數不能用 |
| `migrate wait-for-job` | 服務 pod 的 init container：經 Kubernetes API 等設定裡的 Job 完成；Job 失敗時以 1 結束 |
| `migrate clickhouse-admin-password` | 叢集內 ClickHouse pod 的 init container（Laysi/trustiot-v2#403）：在 chart 宣告的空 Secret 產生 bootstrap 管理帳號的密碼（規則同下方的密碼 Secret，代是 `clickhouse.bundled.adminPasswordGeneration`），再寫成 ClickHouse 映像讀的檔案（[`src/clickhouse_admin.rs`](src/clickhouse_admin.rs)） |
| `migrate identity-signing-keys` | 身分服務 pod 的 init container（Laysi/trustiot-v2#404），以及依金鑰年齡輪替的 CronJob（Laysi/trustiot-v2#429，多一個 `TRUSTIOT__KEYS__MAX_KEY_AGE_SECONDS`：最新一把公開超過它時加一把同一代的新金鑰）：在 chart 宣告的空 Secret 產生、輪替與撤銷內部身分簽章金鑰，並把公鑰寫進 ConfigMap（規則在 `crates/identity-keys`，綁定 uid 與帶 `resourceVersion` 的 patch 與密碼 Secret 相同；代是 `internalIdentity.keyGeneration`，設定是 `TRUSTIOT__KEYS__*`）。日誌只有 kid、代與時間（[`src/signing_keys.rs`](src/signing_keys.rs)） |
| `migrate certs-ca-keys` | certs pod 的 init container（Laysi/trustiot-v2#78）：在 chart 宣告的空 Secret 產生與移除簽發 CA 的 P-256 金鑰，並把每把金鑰的 CSR 寫進 ConfigMap，給離線根 CA 簽發（規則在 `crates/ca-keys`，綁定 uid 與帶 `resourceVersion` 的 patch 與密碼 Secret 相同；代是 `certificateAuthority.keyGeneration`，設定是 `TRUSTIOT__KEYS__*`）。日誌只有金鑰 id、代與 CSR 的檔名（[`src/ca_keys.rs`](src/ca_keys.rs)，操作在 [services/certs/README.md](../../services/certs/README.md)） |
| `migrate mqtt-broker-accounts` | MQTT broker pod 的 init container（Laysi/trustiot-v2#84）：在 chart 宣告的空 Secret 為每個有 broker 帳號的服務產生密碼（規則同下方的密碼 Secret，代是 `mqttBroker.passwordGeneration`），再寫出 broker 的 dynamic security 檔：這個 release 的角色、預設拒絕，服務帳號只放 PBKDF2-SHA512 雜湊，保留 broker 存的閘道器 client；保留的角色、client、群組先以 Mosquitto 讀檔的規則檢查，它會略過或讀錯的項目讓啟動停止（[`src/mqtt_accounts.rs`](src/mqtt_accounts.rs)） |
| `migrate mqtt-broker-trust` | MQTT broker pod 的 init container（`--once`）與 sidecar（Laysi/trustiot-v2#84）：從 certs 服務取閘道器憑證的 CA 與 CRL、檢查後原子地換上，沒有可用的就裝不信任任何閘道器的佔位 CA；確認伺服器憑證與私鑰是一對；有變化時對 broker 送 SIGHUP（[`src/mqtt_trust.rs`](src/mqtt_trust.rs)，做法在 [deploy/chart/README.md](../../deploy/chart/README.md) 的「MQTT broker」） |
| `migrate bootstrap-platform-admin` | 操作者在叢集內執行一次（chart 的 CronJob `<release>-platform-admin-bootstrap`，Laysi/trustiot-v2#582）：讓 stdin 第一行的電子郵件地址的使用者成為這個部署的第一位平台管理員，一個部署只成功一次（下方「第一位平台管理員」，步驟在 [docs/runbooks/platform-admin.md](../../docs/runbooks/platform-admin.md)）。結束碼 0 建立了、1 失敗（什麼都沒改）、2 設定、參數或 stdin 不能用、3 之前已經建立過（什麼都沒改）（[`src/platform_admin.rs`](src/platform_admin.rs)） |
| `cargo test -p migrate` | 單元測試與整合測試（下方「測試」） |

映像是 `deploy/images/migrate`（`image.toml`），與服務一樣由 release 流程建置與簽章。

## 一次執行做什麼

依序，全部在一個 Postgres session 層級的 advisory lock 內（key `(0x7472_7573, 0x6d69_6772)`）；另一次執行持有鎖時，等它結束（`TRUSTIOT__WAIT__LOCK`）：

1. 檢查設定：`TRUSTIOT__PASSWORDS__POSTGRES` 列的每個登入角色（chart 填入部署中用 Postgres 的服務的 `<服務>_app`，以及 outbox 消費端的 `<服務>_platform`）都要在服務清單裡，否則什麼都不改就停止。
2. 以 migration 角色連 Postgres（只接受 `verify-full`），資料庫還沒起來時等（`TRUSTIOT__WAIT__DATABASES`）。migration 角色必須擁有資料庫、有 CREATEROLE，不能是 superuser、不能有 BYPASSRLS，也不能擁有其他資料庫：角色是叢集層級的，角色名稱不帶實例名稱，所以一個 Postgres 叢集只放一個 TrustIoT 資料庫（runbook 的「一個 Postgres 叢集只放一個 TrustIoT 資料庫」）。
   取得鎖、確認 migration 角色之後，填密碼 Secret（[`src/passwords.rs`](src/passwords.rs)）：經 Kubernetes API 讀 `TRUSTIOT__PASSWORDS__*` 列的每個 Secret，缺的 key、不能用的值、Secret 不是 Job 填過的那個物件（annotation `trustiot.io/password-secret-uid` 不等於它的 `metadata.uid`，例如有人刪掉後自己建了同名的 Secret，或從備份還原），或 `TRUSTIOT__PASSWORDS__GENERATION` 大於 annotation `trustiot.io/password-generation` 記的代（沒記、或記的不是數字也算）時，以作業系統的 CSPRNG 產生 256 位元的密碼（64 個十六進位字元），以帶 `resourceVersion` 的 JSON merge patch（field manager `trustiot-migrate`）寫回，同一個 patch 記下 Secret 的 uid 與代；代以數字比較、只往上走：要求的代較小（rollback 渲染出舊值）時保留現有的密碼、記的代不變，所以之後升級回來也不會再換一次；Secret 在讀與寫之間被改過時 API 回 409，Job 重讀再決定，連續 5 次就停止。Secret 不存在、API 拒絕存取時停止並寫出處理方式；API 沒回應或回 429、5xx 時在 `TRUSTIOT__WAIT__DATABASES` 內重試。日誌與錯誤只寫角色、帳號與 Secret 的名稱，不寫密碼、hash 或 Secret 的資料。
3. `sql/bootstrap.sql`（Job 自己的紀錄表 `trustiot_migrate.applied`）與 `sql/` 的編號檔案（`0001_helpers.sql`：下方「SQL 怎麼組」的函式），再套用 `crates/service-kit/sql/database.sql`。
4. 每個服務（[`src/registry.rs`](src/registry.rs) 的 `PRODUCT`，依序）：
   - 角色：`<schema>_owner`（NOLOGIN NOINHERIT，擁有 schema 與每個物件；migration 角色以 PostgreSQL 16 的 `createrole_self_grant = 'set'` 取得 SET 權限，但不繼承它的權限）、`<schema>_app`（NOINHERIT，只在綁定了 Secret 時 LOGIN）、需要時 `<schema>_platform`（決策 29 的平台角色）。登入角色建立時帶 Job 的標記（`COMMENT ON ROLE ... IS 'trustiot-migrate:login-role'`；這之前建立的在下一次執行補上）。不存在就建立；存在就檢查，偏離規則就停止（見 runbook 的表）：屬性、成員關係、不是這個 migration 角色建立的、在叢集的其他資料庫擁有物件或權限、有這個資料庫專屬的設定；登入角色另外不能有 `search_path` 以外的角色設定，也不能在自己的 schema 建立物件或建立暫存表。
   - schema `<schema>`，owner 是 `<schema>_owner`；登入角色的 `search_path` 是自己的 schema。
   - schema 裡的 migration 紀錄 `<schema>.trustiot_migrate_applied`（Laysi/trustiot-v2#402）：以 owner 角色執行 `crates/service-kit/sql/migration-records.sql`（表屬於 owner，只有 `<schema>_app` 能 `SELECT`），再把它調成與 `trustiot_migrate.applied` 裡這個服務的紀錄（component `<服務>` 與 `<服務>.outbox`）相同：缺的補上、hash 不同的改回、多的刪掉，同一個交易。之後套用在這個 schema 的每個檔案（服務的檔案與 outbox 範本），兩份紀錄都在套用它的那個交易裡寫，檔案失敗時兩份都不留。服務的 readiness 讀這份紀錄（service-kit 的 `migrations`），`trustiot_migrate` 仍然只有 migration 角色能用。
   - 登入角色的密碼：取上一步 Secret 裡的值，先以它實際登入，登不進去才 `ALTER ROLE ... PASSWORD`，送出的是 Job 自己算的 SCRAM-SHA-256 verifier，明文不進 SQL；登入角色只在綁定了 Secret 時能登入：綁定了但是 NOLOGIN（剛建立、或服務重新開啟）就先設 Secret 的密碼再 `ALTER ROLE ... LOGIN`，所以不會以舊密碼登入；沒有綁定 Secret（例如這次部署沒開的服務、chart 沒有綁定的平台角色）就 `NOLOGIN`，記在結果的 `roles_without_password`；日誌只在把能登入的角色改成 `NOLOGIN` 時記 WARN，本來就不能登入的（例如 chart 不綁的平台角色、一直沒開的服務）每次執行記一行 INFO。不 `DROP`：角色可能擁有物件，`NOLOGIN` 也能在服務重新開啟時還原。
   - outbox（`crates/outbox`）：依 `Outbox` 的設定建立 producer 或 consumer 的表，producer 的 `crates/service-kit/sql/outbox-event.sql` 與 `outbox-aggregate.sql`（`outbox_aggregate` 的租戶欄位、RLS 與權限，Laysi/trustiot-v2#413）各套用一次，consumer 的平台角色加進 producer 的讀者。`outbox-aggregate.sql` 是 2026-10-15 新增的檔案：之前已跑過 Job 的資料庫在下一次執行時套用它一次，既有的聚合拿到它最後一筆事件的租戶（見 [crates/outbox README 的聚合的租戶隔離](../../crates/outbox/README.md#聚合的租戶隔離laysitrustiot-v24132026-10-15)）；已套用的檔案都沒改。
   - 以 `<schema>_owner` 依序套用 `migrations/postgres/` 的每個檔案，一個檔案一個交易，`search_path` 是該 schema；已套用的檔案若內容改了就停止。檔案執行完後比對交易 ID、`current_user` 與 `role` 設定，與執行前不同（檔案結束了交易或切換了角色）就停止，不記錄這個檔案（下方「檔案的規則」）。
   - 所有服務之後：帶 Job 標記、migration 角色有 ADMIN OPTION、還能登入、但服務清單裡已經沒有任何服務用它的角色（服務從清單移除或改名）一律 `NOLOGIN`。挑選依標記與 ADMIN OPTION，不依名稱，所以 CloudNativePG 的 superuser、客戶自己的帳號、另一個實例的角色都不會被碰。
   - 接著檢查叢集的其他資料庫（`datallowconn`、不是這個資料庫）：服務清單裡的登入角色（不論能不能登入）有 `CONNECT` 時，每個資料庫記一行警告（日誌的 WARN，結果的 `warnings`），寫出資料庫、角色（依名稱排序）與能不能建立暫存表，不停止（Laysi/trustiot-v2#400）。migration 角色不擁有那些資料庫，收不回權限；`bundled` 的 Cluster 在 bootstrap 時已收回 `postgres` 與 `template1` 的，外部 Postgres 由資料庫管理者收回（runbook 的「`postgres` 與 `template1`：收回 PUBLIC 的 CONNECT 與 TEMPORARY」）。只警告，因為客戶的應用也許需要 `PUBLIC` 連得上某個資料庫，這是管理者的決定。
5. 每租戶角色的 hook（`TenantRoleHook`）：以服務的 owner 角色讀租戶表，每個租戶的角色不存在就以 NOLOGIN NOINHERIT 建立，再執行 hook 的範本。角色是叢集層級的物件，不在資料庫備份裡，所以還原的資料庫在下一次執行時取回它們。目前沒有 hook，AGE 的每租戶角色在 Laysi/trustiot-v2#176 接上。
6. ClickHouse（有設定 `TRUSTIOT_CLICKHOUSE_URL` 與 `TRUSTIOT__CLICKHOUSE__*` 時）：以有 access management 的帳號（叢集內 ClickHouse 時密碼經 Kubernetes API 從它的 init container 填的 Secret 讀取：Secret 還沒填好時等待，ClickHouse 拒絕時重讀再試，只採用 init container 填過的物件裡的值）依序套用 `services/ingest/migrations/clickhouse/`，記在 ClickHouse 的 `trustiot_migrate.applied`；collector 的帳號（`TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR`，密碼也來自它的 Secret）不存在就建立，只有預設角色 `trustiot_otel_collector`；存在但多了角色、權限或設定（含 settings profile）就停止，不自動刪除（`DROP USER` 是管理者的決定）；collector 關閉或帳號改名之後，其他拿到 `trustiot_otel_collector` 角色的帳號（只有 Job 的 `CREATE USER ... DEFAULT ROLE` 會給；Job 自己執行時用的帳號 `currentUser()` 與設定檔定義的帳號除外）改成 `HOST NONE`，不能再登入但不刪除，collector 再開啟時先設 Secret 的密碼、再恢復成 `HOST ANY`；`trustiot_otel_collector` 角色本身多了 `0006_logs.sql` 以外的權限或角色時也停止；密碼登不進去才重設。telemetry-query 的服務帳號 `trustiot_telemetry_query`（`0008_telemetry_query.sql` 以 `no_password HOST NONE` 建立）：有 `TRUSTIOT__PASSWORDS__CLICKHOUSE_TELEMETRY_QUERY` 時設 Secret 的密碼（SHA-256 hash）並改成 `HOST ANY`，之後只在登不進去時重設；沒有時改回 `HOST NONE`；帳號或租戶角色 `trustiot_tenant_reader` 多了 `0008_telemetry_query.sql`、`0004_access.sql`、`0005_tenant_view.sql` 以外的權限、角色或設定，或租戶角色少了 `SELECT ON trustiot.tenant_telemetry` 時停止；租戶角色除了服務帳號（有 `ADMIN OPTION`）與 `trustiot_tenant_account_*` 帳號（沒有 `ADMIN OPTION`）之外還有別的持有者（使用者或角色），或服務帳號的 `GRANTEES` 不再是 `NONE` 時也停止，因為 ClickHouse 26.8 不限制有 `ADMIN OPTION` 的帳號把角色給誰（Laysi/trustiot-v2#119）；這些情況下 Job 在停止之前先把服務帳號改成 `HOST NONE`，修好之後的下一次執行再設密碼、恢復 `HOST ANY`。套用完檔案後，把 `trustiot.telemetry` 與完成表 `trustiot.stream_commits` 的 TTL 對齊原始資料的保存期限（`TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS`，不設是 180 天；延長時先改完成表、縮短時先改 `telemetry`，完成表任何時刻都不短於原始資料；`0010_gateway_stream_ids.sql` 停用的舊完成表 `trustiot.batch_commits` 不再管）；TTL 已經是設定的天數就不改，不是 Job 管理的形式時停止、不覆蓋（[資料保存期限與磁碟水位](../../docs/architecture/data-retention.md)）。外部 ClickHouse 的管理帳號也要能授出 `system.disks` 的 `SELECT`（`0007_monitoring.sql` 給監控角色），否則 Job 停在這個檔案。`https` 的 ClickHouse 只信任 `TRUSTIOT_CLICKHOUSE_CA_FILE`，不用系統的根憑證。

每一步都先比對再改，所以設定沒變時再跑一次什麼都不變；每一項變更寫成一行日誌，結果的 `changes` 也列出它們，測試據此與 catalog 的快照確認第二次執行沒有變更。

## 新增服務的 migration

1. 檔案放在 `services/<服務>/migrations/postgres/NNNN_<名稱>.sql`。
2. 在服務的 `src/schema.rs` 宣告 `pub const SCHEMA: service_kit::migrations::Schema`：服務名稱、是不是 outbox producer、每個檔案（`include_str!`，依檔名順序）；之後的新檔案加在最後。檔案只能用 `service_kit`，因為 Job 也編譯它：在 `src/registry.rs` 以 `#[path = "../../../services/<服務>/src/schema.rs"]` 引入，`PRODUCT` 的 `Service` 從 `SCHEMA` 取名稱、producer 與檔案。服務在啟動時以同一個 `SCHEMA` 註冊 readiness 檢查（`MigrationCheck::new(&schema::SCHEMA, <服務角色的 pool>).register(context.health())`，見 [service-kit 的 README](../../crates/service-kit/README.md#migration-檢查migrations)），所以 Job 套用的與服務預期的是同一份清單（Laysi/trustiot-v2#402）。`tests/registry.rs` 在少列、順序錯、重複或內容不同時失敗。consumer 讀的必須是清單裡的 outbox producer，排在它之前或之後都可以：Job 先建立所有服務的角色，再設定 outbox，所以排在後面的 producer 也能把 consumer 的平台角色列為讀者（2026-10-22 起，身分服務的事件紀錄讀排在它後面的 `assets`，Laysi/trustiot-v2#130）。
3. 服務在 chart 的 `mesh.calls` 列出 `postgres`；chart 為它宣告密碼 Secret `<release>-<服務>-password`，Job 產生 `<schema>_app` 的密碼（chart README「migration Job」）。服務以平台角色連線時（outbox 的消費端以它讀 producer 的事件），把服務加進 `deploy/chart/templates/_migrate.tpl` 的 `trustiot.migrate.platformLogins`：同一個 Secret 多一個 `<schema>_platform` key，服務多拿到 `TRUSTIOT__DATABASE__PLATFORM_USERNAME`、`_PASSWORD`。`tests/registry.rs` 檢查這份清單只列有平台角色的服務，而且每個 consumer 都在裡面；producer 的平台角色沒有程式用它連線時不列，維持 `NOLOGIN`，並寫進同一個測試的 `UNBOUND_PLATFORM_ROLES`（附理由；2026-10-21 起是空的，原本的 `identity` 因為事件層要以平台角色讀事件而移進 chart 的清單，Laysi/trustiot-v2#130）。有平台角色的服務一定在 chart 的清單或 `UNBOUND_PLATFORM_ROLES` 其中之一，不能兩邊都在，所以不會有平台角色在沒人注意下維持 `NOLOGIN`。

目前 `PRODUCT` 有兩個服務（2026-10-14，Laysi/trustiot-v2#65）：`identity`（`0001_identity.sql`、`0002_tenants.sql`、`0003_event_log.sql`、`0004_login.sql`（Laysi/trustiot-v2#103）、`0005_member_roles.sql`（Laysi/trustiot-v2#583）、`0006_platform_admin_bootstrap.sql`（Laysi/trustiot-v2#582）；outbox producer，也是事件層的消費端：讀 `src/schema.rs` 的 `CONSUMES` 列出的 producer（目前是它自己與 `assets`）的事件，所以 `identity_platform` 自 2026-10-21 起由 chart 綁定密碼，Laysi/trustiot-v2#130）與 `assets`（`0001_assets.sql`、`0002_device_models.sql`、`0003_gateway_pool.sql`（Laysi/trustiot-v2#101）；消費 `identity` 的事件，2026-10-23 起也消費 `certs` 的事件（出廠登錄的 `certs.gateway_registered` 讓閘道器進入設備池），`assets_platform` 由 chart 綁定密碼）。2026-10-21 起 `assets` 也是 outbox producer（設備型號的 `assets.device_model_*`，Laysi/trustiot-v2#99）：producer 至少要有一個讀者，而會消費這些事件的閘道器設定（Laysi/trustiot-v2#93）還不存在，所以 `assets` 暫時把自己列為讀者（`consumes: &["identity", "certs", "assets"]`），沒有任何訂閱讀它；真正的消費端登記後拿掉 `"assets"`，Job 每次執行都同步讀者，會收回 `assets_platform` 的讀取權。

檔案的規則（`tests/registry.rs` 檢查，不分大小寫，連續的空白、tab 與換行當成一個空白）：

- 不建立 schema、角色，不改角色，不切換角色或 session 設定（`CREATE SCHEMA`、`CREATE ROLE`、`ALTER ROLE`、`CREATE USER`、`ALTER USER`、`CREATE GROUP`、`ALTER GROUP`、`SET ROLE`、`SET LOCAL ROLE`、`SET SESSION ROLE`、`RESET ROLE`、`RESET ALL`、`SESSION AUTHORIZATION`、`set_config`、`pg_settings`）：這些都由 Job 做，檔案以 owner 角色在自己的 schema 裡執行。
- 不寫交易控制（`BEGIN;`、`START TRANSACTION`、`COMMIT;`、`COMMIT AND`、`ROLLBACK;`、`ABORT;`）也不用 `CONCURRENTLY`：Job 把每個檔案包在一個交易裡。`END;` 無法以文字擋下（plpgsql 的區塊也以它結束），由下一段的執行期檢查擋。
- 不建 `SECURITY DEFINER` 函式：它會以 owner 的權限執行，繞過決策 29。
- 已套用的檔案不再修改（Job 以 SHA-256 比對）；修正以新檔案進行。
- 給服務角色的權限寫在檔案裡（`GRANT ... TO <schema>_app`），table 的 RLS 與 policy 照 `crates/service-kit/README.md` 的「角色與 policy 範本」。

文字檢查之外，Job 在每個檔案執行完後比對交易 ID、`current_user` 與 `role` 設定：檔案結束了交易（`END`、`COMMIT AND CHAIN` 等）或把角色留在別處（`SET LOCAL ROLE`、`SET SESSION ROLE`）時停止，這個檔案不記錄（`tests/file_escapes.rs`）。留在交易內的角色切換隨交易回滾；結束過交易的檔案，結束之前與之後在交易外執行的部分已經提交，下一次執行會再套用它而失敗，所以 Job 一直停著，直到修正後的檔案取代它。

這兩層檢查擋的是寫錯的檔案，不是服務之間的隔離：檔案能以 `SET LOCAL ROLE <另一個服務>_owner` 切到別的服務、做完再切回來，結束時角色與交易都和開始時相同（2026-10-13 的獨立驗證確認這樣會通過）。migration 檔案是本 repo 的程式碼，編進 Job 的映像，邊界是 code review：審查新的 migration 時確認它只動自己的 schema。

## SQL 怎麼組

`tools/sql-check` 不允許以字串組 SQL。角色、schema 與權限的語句需要識別字，而 Postgres 不能以參數傳識別字，所以 Job 把每個這種語句寫成常數範本（`src/postgres.rs` 的 `CREATE_LOGIN_ROLE` 等，名稱以 `example_<名稱>` 佔位，與 `crates/service-kit/sql/` 的範本同一種寫法），把範本與名稱當參數交給 `sql/0001_helpers.sql` 的 `trustiot_migrate.apply_template(run_as, template, replacements)`：

- 每個名稱以 `quote_ident` 加引號，而且必須是小寫的一般識別字（`^[a-z_][a-z0-9_]{0,62}$`，不能以 `pg_` 或 `example_` 開頭）；字串值（verifier、租戶 ID）以 `quote_literal` 加引號，只接受固定的字元集合。
- 替換以字詞邊界比對，範本裡還有沒替換的 `example_` 時失敗。
- `run_as` 不是 NULL 時，以 `SET LOCAL ROLE` 的語意在該角色下執行，函式結束前切回。
- 函式是 SECURITY INVOKER，只有擁有 `trustiot_migrate` 的 migration 角色能呼叫。

`src/postgres.rs` 會設定 session（`set_config('role'|'search_path', ...)`，以參數傳值），在 `tools/sql-check` 的 `SESSION_SETTING_ALLOWED` 裡；清單上的 `src/clickhouse_files.rs` 只以 `include_str!` 列出 ClickHouse 的 DDL 檔，因為其中一個檔案的註解寫到 ClickHouse 的 `SET ROLE NONE`。`src/registry.rs`（Postgres migration 與每租戶角色 hook）照常檢查。`tests/registry.rs`（列出禁止的文字）與 `tests/file_escapes.rs`（測試用的檔案切換角色）列在它的例外清單，只免除 `SessionSetting`。

## 設定

平台設定（chart 的 `<release>-platform` ConfigMap，與服務相同）：`TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`（只接受 `verify-full`）、`_CA_FILE`，`TRUSTIOT_CLICKHOUSE_URL`、`TRUSTIOT_CLICKHOUSE_CA_FILE`。

Job 自己的設定：

| 變數 | 內容 |
| --- | --- |
| `TRUSTIOT__POSTGRES__USERNAME`、`__PASSWORD` | migration 角色 |
| `TRUSTIOT__CLICKHOUSE__USERNAME`，與 `__PASSWORD` 或 `__PASSWORD_SECRET` 其中之一 | 有 access management 的 ClickHouse 帳號；`__PASSWORD_SECRET` 是放它密碼的 Secret 名稱（叢集內 ClickHouse 的 `<release>-clickhouse-admin-password`，key 是帳號名稱），經 `TRUSTIOT__PASSWORDS__*` 的 API 設定讀取，所以兩者要一起設 |
| `TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS` | 原始資料（與批次完成表）的保存期限，天；不設是平台的 180 天。必須大於閘道器緩衝的最長保存時間 30 天、不超過 3650 天，否則 Job 在連任何資料庫之前以結束碼 2 停止。chart 目前不設，地端的 values key 由 Laysi/trustiot-v2#446 加入 |
| `TRUSTIOT__PASSWORDS__API_URL`、`__NAMESPACE`、`__TOKEN_FILE`、`__CA_FILE` | 密碼 Secret 所在的 Kubernetes API（`https://`；`http://` 只接受 127.0.0.1，給測試用）、namespace、projected token（每次請求都重讀）與唯一的信任根 |
| `TRUSTIOT__PASSWORDS__GENERATION` | 密碼的代（chart 的 `migrate.passwordGeneration`，1 到 9 位數字）；比 Secret 記的大才全部重新產生，較小時保留 |
| `TRUSTIOT__PASSWORDS__POSTGRES` | 逗號分隔的 `角色=Secret`；key 是角色名稱 |
| `TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR` | `帳號=Secret`：collector 的 ClickHouse 帳號與放它密碼的 Secret；不設就不管 collector 的帳號。帳號不能是 `trustiot_telemetry_query`，也不能以 `trustiot_tenant_account_` 開頭（telemetry-query 的帳號） |
| `TRUSTIOT__PASSWORDS__CLICKHOUSE_TELEMETRY_QUERY` | `trustiot_telemetry_query=Secret`：telemetry-query 的 ClickHouse 服務帳號（`0008_telemetry_query.sql` 定義）與放它密碼的 Secret（chart 是 `<release>-telemetry-query-password`）；Job 設定這個密碼並讓帳號能登入，不設時讓它不能登入 |
| `TRUSTIOT__WAIT__DATABASES`、`__LOCK` | 等資料庫、等另一次執行的上限（例如 `10m`） |

`wait-for-job` 的設定：`TRUSTIOT__KUBE__JOB`、`__NAMESPACE`、`__API_URL`（`https://`）、`__TOKEN_FILE`（每次請求都重讀）、`__CA_FILE`（唯一的信任根）。

`clickhouse-admin-password` 的設定：`TRUSTIOT__ADMIN__API_URL`、`__NAMESPACE`、`__TOKEN_FILE`、`__CA_FILE`（同上）、`__SECRET`（chart 宣告的空 Secret）、`__ACCOUNT`（管理帳號，也是 Secret 的 key）、`__GENERATION`（1 到 9 位數字）、`__PASSWORD_FILE`（絕對路徑，寫成只有擁有者能讀的檔案，先寫暫存檔再改名）、`__WAIT`（API 不回應時重試的上限，預設 10 分鐘）。

`certs-ca-keys` 的設定：`TRUSTIOT__KEYS__API_URL`、`__NAMESPACE`、`__TOKEN_FILE`、`__CA_FILE`（同上）、`__SECRET`（chart 宣告的空 Secret `<release>-certs-ca-keys`）、`__CONFIG_MAP`（CSR 的 ConfigMap `<release>-certs-ca-requests`）、`__GENERATION`（`certificateAuthority.keyGeneration`，1 到 9 位數字）、`__REMOVE_BEFORE_GENERATION`（`certificateAuthority.removeKeysBeforeGeneration`，預設 `0`）、`__WAIT`（API 不回應時重試的上限，預設 10 分鐘）。

`mqtt-broker-accounts` 的設定：`TRUSTIOT__ACCOUNTS__API_URL`、`__NAMESPACE`、`__TOKEN_FILE`、`__CA_FILE`（同上）、`__SECRET`（chart 宣告的空 Secret `<release>-mqtt-broker-accounts`）、`__SERVICES`（有帳號的服務，逗號分隔，可以是空的）、`__GENERATION`（`mqttBroker.passwordGeneration`，1 到 9 位數字）、`__DYNSEC_FILE`（broker 磁碟區上的 dynamic security 檔）、`__WAIT`（API 不回應時重試的上限，預設 10 分鐘）。

`mqtt-broker-trust` 的設定：`TRUSTIOT__TRUST__CERTS_URL`（certs 服務的 `http://<主機>:<埠>`，mesh 內由 ztunnel 加密；沒有 certs 服務時是空的）、`__DIR`（broker 磁碟區上放信任檔的目錄，broker 讀 `<dir>/current/`）、`__TLS_DIR`（伺服器憑證 Secret 的掛載）、`__INTERVAL`（sidecar 的間隔，預設 30 秒，1 秒到 1 小時）。

`bootstrap-platform-admin` 的設定：平台設定的 `TRUSTIOT_POSTGRES_*`、`TRUSTIOT__POSTGRES__USERNAME`、`__PASSWORD`（migration 角色，與 Job 相同），`TRUSTIOT__WAIT__DATABASES`（等 Postgres 的上限，預設 10 分鐘）；不讀其他 Job 的設定。地址不是設定：只從 stdin 讀，指令不接受任何參數（帶了就以 2 結束）。

`scripts/dev.ts up` 在本機執行同一個 binary（Laysi/trustiot-v2#336），不改這個 crate：`up` 以 superuser 建立 migration 角色 `trustiot_owner`（擁有資料庫、有 CREATEROLE、不是 superuser，`tests/support/mod.rs` 的 `MIGRATOR_INIT` 是同一種形狀），以 `cargo build --package migrate` 建出 binary，用平台設定與上表的變數（不帶參數）執行。密碼 Secret 沒有 Kubernetes：`scripts/lib/secret-api.ts` 在 127.0.0.1 的隨機埠起一個只回答 `GET` 與帶 `resourceVersion` 的 merge `PATCH` 的替身，宣告一個空的 Secret，`TRUSTIOT__PASSWORDS__API_URL` 指向它（`http://` 只接受 127.0.0.1，正是為這種用途保留的），所以 `identity_app` 的密碼走叢集裡一樣的程式路徑產生並設定，登入角色不是 NOLOGIN。替身只在 Job 執行期間存在，Secret 在記憶體裡；選擇它而不是讓這個 crate 接受另一個密碼來源的理由見 [scripts/README.md](../../scripts/README.md) 的「密碼的決定」。本機的 Job 不處理 ClickHouse，也不管 `example-service` 的角色（它不在 `PRODUCT` 裡，由 `up` 自己建立）。

## 第一位平台管理員

`migrate bootstrap-platform-admin`（Laysi/trustiot-v2#582，清單 A2）：

1. 讀 stdin 的第一行（最多 1024 bytes，可以以 `\r\n` 結束），去掉前後空白、轉成 ASCII 小寫，以身分服務的 `services/identity/src/email.rs`（這個 crate 以 `#[path]` 編譯同一個檔案）檢查是不是地址；不是時以 2 結束，訊息不引用輸入。
2. 以 migration 角色用 verify-full 連上 Postgres（等 `TRUSTIOT__WAIT__DATABASES`），照 `run` 的規則檢查 migration 角色，再開一個交易，在交易內切換到 `identity_owner`、`search_path` 設為 `identity`（`set_config(..., true)`）（不取 migration 的 advisory lock：唯一性由下面的主鍵保證，與正在執行的 migration Job 也不互等）。
3. 交易裡依序：確認 `identity.platform_admin_bootstrap` 存在（不存在表示這一版的 migration Job 還沒跑，以 1 結束）；依地址找使用者（`FOR UPDATE`）；`INSERT ... ON CONFLICT (singleton) DO NOTHING` 寫下唯一的一列。已有一列時讀出它的使用者 ID 與時間、回復交易，以 3 結束；同時執行的另一次在這裡等前一個交易 commit，之後同樣以 3 結束。寫入成功時，已有的使用者設 `platform_admin = true`，沒有時以新的 UUIDv7 建立（`email` 是正規化的地址）；以 outbox 寫 `identity.platform_admin_granted`（aggregate `user`，沒有租戶、沒有 `actor`，payload `{"user_id", "via": "bootstrap"}`）；commit，以 0 結束。
4. 日誌（JSON 行，stdout）只寫 `user_id`、`existing_user`、`created_at`；資料庫的錯誤只寫步驟、SQLSTATE 與 constraint，不寫 Postgres 的訊息（它可能引用地址）。地址不在日誌、事件與錯誤訊息裡。

標記不跟著使用者刪除或取消平台管理員而消失，所以「只有一次」在所有平台管理員都被移除之後仍然成立；要再建立一位，以 superuser 刪除這一列（runbook 的「復原：再建立一次」）。

## 已知限制

- 一個 Postgres 叢集只放一個 TrustIoT 資料庫（上方第 2 步）。要讓多個實例共用叢集，角色名稱得帶實例的前綴，目前沒有做。
- 服務角色能改自己的密碼與自己的角色設定（Postgres 允許每個角色這樣做）。設定在下一次執行時被發現並停止 Job；密碼要等 Job 再跑才會設回 Secret 的值，在那之前同一個服務的其他 pod 登不進去（runbook 的「密碼 Secret 的資料遺失」最後一段）。
- 叢集的其他資料庫（`postgres`、`template1` 與客戶自己的）的權限不歸 Job 管：Job 不擁有它們，只在服務角色連得上時警告（上方第 4 步）。`bundled` 由 chart 在 Cluster bootstrap 時收回 `postgres` 與 `template1` 的；Laysi/trustiot-v2#400 之前 bootstrap 的 Cluster 與外部 Postgres 要照 runbook 收回一次，在那之前服務角色能連過去並在 `postgres` 建立暫存表。
- 檔案之間的隔離靠 code review（上方「檔案的規則」最後一段）。

## 測試

需要 podman 或 docker：Postgres 用 `crates/service-kit` 測試共用的 TLS Postgres，ClickHouse 用拋棄式容器。密碼 Secret 放在一個假的 Kubernetes API（`tests/support/secrets.rs`，記憶體裡的 Secret，`GET` 與帶 `resourceVersion` 前提的 merge `PATCH`，只記錄請求行，不記錄內容）。沒有容器執行環境時這些測試略過，設了 `CI` 時失敗。角色是叢集層級的物件，所以跑 `PRODUCT` 的每個測試檔只有一個測試、各自一個 Postgres。

| 檔案 | 驗證 |
| --- | --- |
| `tests/fresh_database.rs` | 全新資料庫跑完全部 migration，服務角色的密碼由 Job 產生進空的 Secret（只有一次 `GET` 與一次 `PATCH`）；pg_roles 與 pg_tables 符合決策 29；`identity_platform` 在 `assets` schema 只有 `assets.outbox_event` 的 `SELECT`（事件紀錄讀排在後面的 `assets`），讀取 policy 只列 `assets_platform` 與 `identity_platform`；第二次執行不改 catalog；新的密碼世代換掉密碼、舊的登不進去，再跑一次不改 catalog；key 被刪掉時重新產生，別人寫進的值成為角色的密碼；綁定的角色不在服務清單時什麼都不改（Secret 也沒碰）；拒絕 superuser、沒有 CREATEROLE 的角色、改過的檔案、被改過的服務角色（BYPASSRLS、成員關係、服務角色自己設定的資料庫專屬 `search_path`、角色層級的其他設定、schema 的 CREATE、還給 PUBLIC 的 TEMPORARY），修好後再跑沒有變更 |
| `tests/system_databases.rs` | 叢集的其他資料庫（Laysi/trustiot-v2#400）：收回之前，PostgreSQL 18 讓 Job 建立的服務角色連得上 `postgres` 與 `template1`、能在 `postgres` 建立暫存表（`template1` 不能），兩者的 `public` 都不能建立物件；Job 對兩個資料庫各記一行警告、寫出每個登入角色，照常完成。以 superuser 依 chart 的切法逐句執行 `deploy/chart/files/postgres/system-databases.sql` 兩次（冪等）：兩個資料庫的 ACL 只剩 superuser，`postgres` 另外只有 `streaming_replica` 與 `cnpg_metrics_exporter` 的 `CONNECT`，兩個角色的屬性與 operator 建立的相同；之後從 `template1` 建立的資料庫仍讓服務角色連線。Job 再跑一次沒有警告也沒有變更；每個登入角色與 migration 角色在兩個資料庫都沒有 `CONNECT`、`TEMPORARY`，服務角色連線被拒（42501），TrustIoT 的資料庫照常可用。另有一個不需要資料庫的測試確認切法與 chart 相同（空白行分隔、`--` 開頭的行是註解） |
| `tests/one_instance_per_cluster.rs` | 同一個 migration 角色擁有第二個資料庫時停止；第二個 migration 角色以 ADMIN OPTION 拿到第一個實例的角色時也停止；兩種情況第一個實例的 catalog 都沒變，服務仍以原密碼登入，被拒絕的執行沒有讀寫 Secret |
| `tests/file_escapes.rs` | 檔案留下 `SET LOCAL ROLE NONE`、切到別的服務的 owner 角色、`SET SESSION ROLE` 時停止，交易回滾、不記錄，再試一次不改 catalog；`END` 停止且不記錄，之前的部分已提交，下一次執行在它上面失敗；`COMMIT AND CHAIN` 後再取回角色也停止 |
| `tests/concurrent_runs.rs` | 鎖被持有時執行會等，到上限就放棄且什麼都沒改；同時開始的兩次執行都成功，只有一次套用變更 |
| `tests/outbox_and_tenant_roles.rs` | outbox 的表、policy 與讀者的增減；`outbox-aggregate.sql` 之前的資料庫（`outbox_aggregate` 沒有租戶欄位與 RLS、已有舊版寫的聚合）再跑一次只套用這個檔案、每個聚合拿到最後一筆事件的租戶、服務角色在沒有租戶時看不到任何聚合，下一次執行沒有變更（Laysi/trustiot-v2#413）；每租戶角色 hook 為新租戶建立角色，還原的資料庫少了角色時補回；從服務清單移除的服務的登入角色變成 NOLOGIN、舊密碼登不進去；部署關掉的服務（沒有綁定 Secret）變成 NOLOGIN，重新開啟時先設密碼再 LOGIN；沒有 Job 標記、或 migration 角色沒有 ADMIN OPTION 的登入角色不受影響 |
| `tests/clickhouse.rs` | ClickHouse DDL 套用一次、改過的檔案被拒；collector 帳號以 Job 產生的密碼建立、寫入，新的密碼世代輪替，多了權限、設定或角色不是預設角色時停止；collector 角色多了權限或角色時停止；collector 關閉或帳號改名時舊帳號不能再登入（`HOST NONE`），再開啟時恢復；原始資料的保存期限（Laysi/trustiot-v2#92）：兩張表的 TTL 從建表的 400 天改成 180 天、再跑一次不改；帶三年前時間戳但剛收到的列與完成紀錄在強制 merge 後仍在，收到時間超過保存期限的被清掉；延長與縮短時兩張表的變更順序；30 天與 7 天的設定被拒且什麼都沒改；手動改過的 TTL 讓 Job 停止並寫出表名，改回可辨認的形式後 Job 把它對齊 |
| `tests/clickhouse_telemetry_query.rs` | telemetry-query 的 ClickHouse 服務帳號（Laysi/trustiot-v2#119）：`0008_telemetry_query.sql` 建立時不能登入；服務部署時以 Job 產生的密碼登入，只能建立與修改 `trustiot_tenant_account_*` 帳號；新的密碼世代輪替；帳號多了權限、角色或設定，租戶角色多了權限或角色，或租戶角色有服務帳號與租戶帳號以外的持有者，或服務帳號的 `GRANTEES` 被改時，讓服務帳號不能登入並停止，撤銷後再跑設定密碼、恢復登入；`GRANTEES NONE` 下服務帳號仍能以 `CREATE USER ... DEFAULT ROLE` 把租戶角色給新帳號，但不能把它授與或收回既有的使用者或角色，也改不了自己的 `GRANTEES`；服務關閉時 `HOST NONE`，再開啟時恢復 |
| `tests/passwords.rs` | 密碼 Secret 對假的 API：第一次填入每個缺的 key（256 位元十六進位，各不相同），第二次只讀不寫；新的世代換掉每個受管的 key、不動其他 key、只換一次；較小的世代（rollback）不換、之後升級回來也不換；不是 Job 填過的 Secret 物件（uid 不同，含帶著同代 annotation 與自選密碼的）每個 key 都重新產生，別人只改 annotation 時也一樣，綁定時拿掉白名單以外的 annotation、標籤、ownerReferences 與 finalizers（報告只寫 key），已綁定的不再動；遺失或不能用的 key 單獨重新產生；Secret 不存在時停止並寫出處理方式；讀寫之間被改過時重讀，一直被改時停止而不覆蓋；拒絕的 token 與 403、API 暫時 503 的重試與上限；報告、錯誤與 `Debug` 都不含密碼或它的 base64；沒有綁定時不發請求（不需要容器） |
| `tests/clickhouse_admin.rs` | `clickhouse-admin-password` 對假的 API：第一次填入並寫出 0400 的檔案、第二次只讀不寫，留下的暫存檔不影響；新的代輪替一次，rollback 不換；別人建立的 Secret（uid 不同）不採用；Secret 不存在或被拒時停止並寫出處理方式。對拋棄式 ClickHouse：Job 只用 init container 寫入的密碼，被拒時重讀 Secret，等到上限時的訊息寫出 Secret 名稱；未綁定的 Secret 裡的值不使用；全新安裝時 Job 等 Secret 填好才連線 |
| `tests/signing_keys.rs` | `identity-signing-keys` 對假的 API（Secret 與 ConfigMap，以假的時鐘）：全新安裝一把立即簽章；輪替先寫 Secret、再公開、最後記下時間，舊金鑰在它的 token 都過期後才移除；撤銷立即換掉每一把較舊的；中途停掉由下一次完成；同時執行只產生一把；較舊的一次執行不會撤下較新的公開，Secret 被另一次執行改過時也不把還顯示自己那組公鑰的 ConfigMap 當成完成；不是它填過的 Secret（uid 不同）不採用；不能用的內容換掉；人建立的私鑰匯入後被換掉；物件不存在或被拒時停止並寫出處理方式；不超過 8 把；日誌與 API 回應以外的地方沒有私鑰；輪替 CronJob 的設定（Laysi/trustiot-v2#429）在到期前不寫入、到期時照手動輪替的排程換掉金鑰並如期移除舊的，之後手動輪替與撤銷照常（撤銷也移除自動產生的金鑰），與要求新一代的 init container 同時執行時讓出 |
| `tests/ca_keys.rs` | `certs-ca-keys` 對假的 API（Secret 與 ConfigMap）：全新安裝填入一把金鑰並公開它的 CSR，再啟動一次只讀不寫；較大的代加一把並保留舊的，rollback 不變，`__REMOVE_BEFORE_GENERATION` 移除較舊的金鑰與 CSR；中途停掉由下一次完成；同時執行只產生一把；別人建立或還原的 Secret（uid 不同）不採用、外來的 metadata 拿掉；物件不存在、被拒、寫入時被改過都停止並寫出處理方式；不超過 4 把；報告、錯誤與 `Debug` 沒有私鑰，ConfigMap 只有 CSR |
| `tests/mqtt_accounts.rs` | `mqtt-broker-accounts` 對假的 API：密碼進 Secret、broker 的檔案只有雜湊；沒有帳號持有者時不碰 API；讀不了的 dynamic security 檔或不存在的 Secret 讓啟動停止並寫出處理方式 |
| `tests/mqtt_trust.rs` | `mqtt-broker-trust` 對假的 certs 服務與假的 broker 行程（處理 SIGHUP 的 shell）：init container 裝上 certs 的檔案或佔位 CA、拒絕不是一對的伺服器私鑰；sidecar 等 broker 處理 SIGHUP 才送，每次能載入的變化之後送一次 |
| `tests/mqtt_broker.rs` | chart 的 Mosquitto 設定（`deploy/chart/files/mosquitto/mosquitto.conf`）在固定版本的映像裡，搭配上面兩個指令寫的檔案、以 kubelet 的方式（`..data` symlink）掛的伺服器憑證、唯讀根目錄：沒有憑證、別的 CA、過期、沒有 clientAuth、已撤銷的閘道器被拒，佔位 CA 不信任任何閘道器，新的 CRL 與伺服器憑證在 SIGHUP 後生效且已連線的 session 不斷；服務以自己的帳號登入、以 shared subscription 收自己的頻道、只能送自己的頻道，只有 certs 能用 dynamic security 的控制 topic，broker 在 certs 的變更後自己存的檔通過 init container 的檢查；1 MiB 通過、超過上限的封包被拒；retained 訊息、session 與排隊的訊息在 SIGKILL 後還在；探針不留錯誤、警告或 OpenSSL 訊息。`-- --nocapture` 印出測試報告引用的證據行 |
| `tests/platform_admin.rs` | `bootstrap-platform-admin` 以真的 binary（Laysi/trustiot-v2#582）：migration 之前以 1 結束、什麼都沒建；不是地址、空行、含空白的地址、帶了參數都以 2 結束、什麼都沒改；兩個同時執行（以 superuser 鎖住標記表，確認兩個都在等鎖之後才放開）一個 0、一個 3，3 的那個寫出 0 的使用者；只有一位平台管理員、一筆 `identity.platform_admin_granted`（payload 只有 `user_id` 與 `via`、沒有租戶），後來的執行與取消所有平台管理員之後仍然以 3 結束、什麼都沒改；照 runbook 刪除標記之後，大小寫與空白不同的地址讓已有的使用者成為平台管理員（ID 不變），下一次又是 3；每一次的 stdout 與 stderr 都沒有地址 |
| `tests/wait_for_job.rs` | `wait-for-job` 對一個假的 API server：等到完成、失敗與拒絕的 token、不信任其他 CA；Job 不存在時繼續等，訊息寫出 Job 名稱、原因與處理方式 |
| `tests/service_records.rs` | 每個服務 schema 裡的 migration 紀錄（Laysi/trustiot-v2#402），以測試用的服務清單與 service-kit 的 `MigrationCheck`（服務角色、帶連線檢查的 pool）：紀錄與 `trustiot_migrate.applied` 相同、表屬於 owner 而只有服務角色能讀；預期下一版檔案的映像不 ready 並寫出缺的檔案，下一版的 Job 跑完後同一個檢查通過，上一版的映像在較新的資料庫上照常通過；失敗的檔案兩份紀錄都不留；第二次執行沒有變更；這一版之前的資料庫（沒有紀錄表）讀不到紀錄而不 ready，下一次執行建立並補齊、再下一次沒有變更；手動改過的紀錄被改回；服務角色不能改自己的紀錄、讀不到 `trustiot_migrate.applied`，其他服務的服務角色與能進這個 schema 的消費端平台角色都讀不到；每個角色仍通過 service-kit 的連線檢查 |
| `tests/registry.rs` | `PRODUCT` 列出每個檔案、檔案符合規則（大小寫與空白的變化也擋得到）、不一致的清單被拒絕（消費不存在或不寫事件的服務等），consumer 排在 producer 之前、兩個服務互讀事件則接受；chart 綁定密碼的登入角色（`_migrate.tpl` 的兩條 Postgres 綁定與 `trustiot.migrate.platformLogins`）與 `PRODUCT` 的角色一致（不需要資料庫） |
