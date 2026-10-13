# migrate

部署時的 migration Job（Laysi/trustiot-v2#60，決策 4、14、29，清單 M8）：套用每個服務的 Postgres migration 與 ClickHouse DDL，冪等地建立服務的資料庫角色，並保留每租戶角色的 hook。chart 怎麼執行它、服務怎麼等它，見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「migration Job」；架構層級的說明在 [docs/architecture/deployment.md](../../docs/architecture/deployment.md)；維運步驟在 [docs/runbooks/migrations.md](../../docs/runbooks/migrations.md)。

| 指令 | 作用 |
| --- | --- |
| `migrate run`（或不帶參數） | 一次 migration（chart 的 Job）。結束碼 0 完成、1 失敗、2 設定或參數不能用 |
| `migrate wait-for-job` | 服務 pod 的 init container：經 Kubernetes API 等設定裡的 Job 完成；Job 失敗時以 1 結束 |
| `cargo test -p migrate` | 單元測試與整合測試（下方「測試」） |

映像是 `deploy/images/migrate`（`image.toml`），與服務一樣由 release 流程建置與簽章。

## 一次執行做什麼

依序，全部在一個 Postgres session 層級的 advisory lock 內（key `(0x7472_7573, 0x6d69_6772)`）；另一次執行持有鎖時，等它結束（`TRUSTIOT__WAIT__LOCK`）：

1. 檢查設定：`TRUSTIOT__PASSWORDS__POSTGRES` 列的每個登入角色（chart 填入部署中用 Postgres 的服務的 `<服務>_app`）都要在服務清單裡，否則什麼都不改就停止。
2. 以 migration 角色連 Postgres（只接受 `verify-full`），資料庫還沒起來時等（`TRUSTIOT__WAIT__DATABASES`）。migration 角色必須擁有資料庫、有 CREATEROLE，不能是 superuser、不能有 BYPASSRLS，也不能擁有其他資料庫：角色是叢集層級的，角色名稱不帶實例名稱，所以一個 Postgres 叢集只放一個 TrustIoT 資料庫（runbook 的「一個 Postgres 叢集只放一個 TrustIoT 資料庫」）。
   取得鎖、確認 migration 角色之後，填密碼 Secret（[`src/passwords.rs`](src/passwords.rs)）：經 Kubernetes API 讀 `TRUSTIOT__PASSWORDS__*` 列的每個 Secret，缺的 key、不能用的值、或 annotation `trustiot.io/password-generation` 不等於 `TRUSTIOT__PASSWORDS__GENERATION` 時，以作業系統的 CSPRNG 產生 256 位元的密碼（64 個十六進位字元），以帶 `resourceVersion` 的 JSON merge patch（field manager `trustiot-migrate`）寫回並更新 annotation；Secret 在讀與寫之間被改過時 API 回 409，Job 重讀再決定，連續 5 次就停止。Secret 不存在、API 拒絕存取時停止並寫出處理方式；API 沒回應或回 429、5xx 時在 `TRUSTIOT__WAIT__DATABASES` 內重試。日誌與錯誤只寫角色、帳號與 Secret 的名稱，不寫密碼、hash 或 Secret 的資料。
3. `sql/bootstrap.sql`（Job 自己的紀錄表 `trustiot_migrate.applied`）與 `sql/` 的編號檔案（`0001_helpers.sql`：下方「SQL 怎麼組」的函式），再套用 `crates/service-kit/sql/database.sql`。
4. 每個服務（[`src/registry.rs`](src/registry.rs) 的 `PRODUCT`，依序）：
   - 角色：`<schema>_owner`（NOLOGIN NOINHERIT，擁有 schema 與每個物件；migration 角色以 PostgreSQL 16 的 `createrole_self_grant = 'set'` 取得 SET 權限，但不繼承它的權限）、`<schema>_app`（LOGIN NOINHERIT）、需要時 `<schema>_platform`（決策 29 的平台角色）。不存在就建立；存在就檢查，偏離規則就停止（見 runbook 的表）：屬性、成員關係、不是這個 migration 角色建立的、在叢集的其他資料庫擁有物件或權限、有這個資料庫專屬的設定；登入角色另外不能有 `search_path` 以外的角色設定，也不能在自己的 schema 建立物件或建立暫存表。
   - schema `<schema>`，owner 是 `<schema>_owner`；登入角色的 `search_path` 是自己的 schema。
   - 登入角色的密碼：取上一步 Secret 裡的值，先以它實際登入，登不進去才 `ALTER ROLE ... PASSWORD`，送出的是 Job 自己算的 SCRAM-SHA-256 verifier，明文不進 SQL；沒有綁定 Secret 的登入角色（例如這次部署沒開的服務、平台角色）記在結果裡，不設密碼。
   - outbox（`crates/outbox`）：依 `Outbox` 的設定建立 producer 或 consumer 的表，producer 的 `crates/service-kit/sql/outbox-event.sql` 只套用一次，consumer 的平台角色加進 producer 的讀者。
   - 以 `<schema>_owner` 依序套用 `migrations/postgres/` 的每個檔案，一個檔案一個交易，`search_path` 是該 schema；已套用的檔案若內容改了就停止。檔案執行完後比對交易 ID、`current_user` 與 `role` 設定，與執行前不同（檔案結束了交易或切換了角色）就停止，不記錄這個檔案（下方「檔案的規則」）。
5. 每租戶角色的 hook（`TenantRoleHook`）：以服務的 owner 角色讀租戶表，每個租戶的角色不存在就以 NOLOGIN NOINHERIT 建立，再執行 hook 的範本。角色是叢集層級的物件，不在資料庫備份裡，所以還原的資料庫在下一次執行時取回它們。目前沒有 hook，AGE 的每租戶角色在 Laysi/trustiot-v2#176 接上。
6. ClickHouse（有設定 `TRUSTIOT_CLICKHOUSE_URL` 與 `TRUSTIOT__CLICKHOUSE__*` 時）：以有 access management 的帳號依序套用 `services/ingest/migrations/clickhouse/`，記在 ClickHouse 的 `trustiot_migrate.applied`；collector 的帳號（`TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR`，密碼也來自它的 Secret）不存在就建立，只有預設角色 `trustiot_otel_collector`；存在但多了角色、權限或設定（含 settings profile）就停止，不自動刪除（`DROP USER` 是管理者的決定）；`trustiot_otel_collector` 角色本身多了 `0006_logs.sql` 以外的權限或角色時也停止；密碼登不進去才重設。`https` 的 ClickHouse 只信任 `TRUSTIOT_CLICKHOUSE_CA_FILE`，不用系統的根憑證。

每一步都先比對再改，所以設定沒變時再跑一次什麼都不變；每一項變更寫成一行日誌，結果的 `changes` 也列出它們，測試據此與 catalog 的快照確認第二次執行沒有變更。

## 新增服務的 migration

1. 檔案放在 `services/<服務>/migrations/postgres/NNNN_<名稱>.sql`。
2. 在 `src/registry.rs` 的 `PRODUCT` 加上這個服務與每個檔案（`include_str!`，依檔名順序）；之後的新檔案加在最後。`tests/registry.rs` 在少列、順序錯、重複或內容不同時失敗。consumer 要排在它讀的 producer 之後。
3. 服務在 chart 的 `mesh.calls` 列出 `postgres`；chart 為它宣告密碼 Secret `<release>-<服務>-password`，Job 產生 `<schema>_app` 的密碼（chart README「migration Job」）。

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
| `TRUSTIOT__CLICKHOUSE__USERNAME`、`__PASSWORD` | 有 access management 的 ClickHouse 帳號 |
| `TRUSTIOT__PASSWORDS__API_URL`、`__NAMESPACE`、`__TOKEN_FILE`、`__CA_FILE` | 密碼 Secret 所在的 Kubernetes API（`https://`；`http://` 只接受 127.0.0.1，給測試用）、namespace、projected token（每次請求都重讀）與唯一的信任根 |
| `TRUSTIOT__PASSWORDS__GENERATION` | 密碼的代（chart 的 `migrate.passwordGeneration`）；Secret 記的不同就全部重新產生 |
| `TRUSTIOT__PASSWORDS__POSTGRES` | 逗號分隔的 `角色=Secret`；key 是角色名稱 |
| `TRUSTIOT__PASSWORDS__CLICKHOUSE_COLLECTOR` | `帳號=Secret`：collector 的 ClickHouse 帳號與放它密碼的 Secret；不設就不管 collector 的帳號 |
| `TRUSTIOT__WAIT__DATABASES`、`__LOCK` | 等資料庫、等另一次執行的上限（例如 `10m`） |

`wait-for-job` 的設定：`TRUSTIOT__KUBE__JOB`、`__NAMESPACE`、`__API_URL`（`https://`）、`__TOKEN_FILE`（每次請求都重讀）、`__CA_FILE`（唯一的信任根）。

`scripts/dev.ts` 的本機環境還沒有接上這個 Job：它自己建立範例服務的角色，不跑服務的 migration。接上之前，本機要套用 migration 時，以 `bun scripts/dev.ts env` 的連線設定加上上表的變數執行 `cargo run -p migrate`，migration 角色（擁有資料庫、有 CREATEROLE、不是 superuser）要先由 superuser 建立，`tests/support/mod.rs` 的 `MIGRATOR_INIT` 是一個例子。

## 已知限制

- 一個 Postgres 叢集只放一個 TrustIoT 資料庫（上方第 2 步）。要讓多個實例共用叢集，角色名稱得帶實例的前綴，目前沒有做。
- 服務角色能改自己的密碼與自己的角色設定（Postgres 允許每個角色這樣做）。設定在下一次執行時被發現並停止 Job；密碼要等 Job 再跑才會設回 Secret 的值，在那之前同一個服務的其他 pod 登不進去（runbook 的「密碼 Secret 的資料遺失」最後一段）。
- 服務角色能連 `postgres` 等其他資料庫，並在那裡建立暫存表：`database.sql` 只收回 TrustIoT 資料庫的 TEMPORARY，其他資料庫的權限由資料庫管理者（`bundled` 是 CloudNativePG）決定。
- 檔案之間的隔離靠 code review（上方「檔案的規則」最後一段）。

## 測試

需要 podman 或 docker：Postgres 用 `crates/service-kit` 測試共用的 TLS Postgres，ClickHouse 用拋棄式容器。密碼 Secret 放在一個假的 Kubernetes API（`tests/support/secrets.rs`，記憶體裡的 Secret，`GET` 與帶 `resourceVersion` 前提的 merge `PATCH`，只記錄請求行，不記錄內容）。沒有容器執行環境時這些測試略過，設了 `CI` 時失敗。角色是叢集層級的物件，所以跑 `PRODUCT` 的每個測試檔只有一個測試、各自一個 Postgres。

| 檔案 | 驗證 |
| --- | --- |
| `tests/fresh_database.rs` | 全新資料庫跑完全部 migration，服務角色的密碼由 Job 產生進空的 Secret（只有一次 `GET` 與一次 `PATCH`）；pg_roles 與 pg_tables 符合決策 29；第二次執行不改 catalog；新的密碼世代換掉密碼、舊的登不進去，再跑一次不改 catalog；key 被刪掉時重新產生，別人寫進的值成為角色的密碼；綁定的角色不在服務清單時什麼都不改（Secret 也沒碰）；拒絕 superuser、沒有 CREATEROLE 的角色、改過的檔案、被改過的服務角色（BYPASSRLS、成員關係、服務角色自己設定的資料庫專屬 `search_path`、角色層級的其他設定、schema 的 CREATE、還給 PUBLIC 的 TEMPORARY），修好後再跑沒有變更 |
| `tests/one_instance_per_cluster.rs` | 同一個 migration 角色擁有第二個資料庫時停止；第二個 migration 角色以 ADMIN OPTION 拿到第一個實例的角色時也停止；兩種情況第一個實例的 catalog 都沒變，服務仍以原密碼登入，被拒絕的執行沒有讀寫 Secret |
| `tests/file_escapes.rs` | 檔案留下 `SET LOCAL ROLE NONE`、切到別的服務的 owner 角色、`SET SESSION ROLE` 時停止，交易回滾、不記錄，再試一次不改 catalog；`END` 停止且不記錄，之前的部分已提交，下一次執行在它上面失敗；`COMMIT AND CHAIN` 後再取回角色也停止 |
| `tests/concurrent_runs.rs` | 鎖被持有時執行會等，到上限就放棄且什麼都沒改；同時開始的兩次執行都成功，只有一次套用變更 |
| `tests/outbox_and_tenant_roles.rs` | outbox 的表、policy 與讀者的增減；每租戶角色 hook 為新租戶建立角色，還原的資料庫少了角色時補回 |
| `tests/clickhouse.rs` | ClickHouse DDL 套用一次、改過的檔案被拒；collector 帳號以 Job 產生的密碼建立、寫入，新的密碼世代輪替，多了權限、設定或角色不是預設角色時停止；collector 角色多了權限或角色時停止 |
| `tests/passwords.rs` | 密碼 Secret 對假的 API：第一次填入每個缺的 key（256 位元十六進位，各不相同），第二次只讀不寫；新的世代換掉每個受管的 key、不動其他 key、只換一次；遺失或不能用的 key 單獨重新產生；Secret 不存在時停止並寫出處理方式；讀寫之間被改過時重讀，一直被改時停止而不覆蓋；拒絕的 token 與 403、API 暫時 503 的重試與上限；報告、錯誤與 `Debug` 都不含密碼或它的 base64；沒有綁定時不發請求（不需要容器） |
| `tests/wait_for_job.rs` | `wait-for-job` 對一個假的 API server：等到完成、失敗與拒絕的 token、不信任其他 CA；Job 不存在時繼續等，訊息寫出 Job 名稱、原因與處理方式 |
| `tests/registry.rs` | `PRODUCT` 列出每個檔案、檔案符合規則（大小寫與空白的變化也擋得到）、不一致的清單被拒絕（不需要資料庫） |
