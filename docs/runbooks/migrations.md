# migration Job

migration Job（[tools/migrate](../../tools/migrate/README.md)，Laysi/trustiot-v2#60）的維運步驟：補給既有 Cluster 的權限、收回服務角色在叢集其他資料庫的權限、輪替服務的資料庫密碼、Job 失敗時怎麼處理。Job 做什麼、何時執行，見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「migration Job」。

下文的 `<namespace>` 是 TrustIoT 的 namespace，`<release>` 是 Helm release 名稱，`<cluster>` 是叢集內 Postgres 的 Cluster 名稱（通常是 `<release>-postgres`）。SaaS 在主機上以 `sudo k3s kubectl` 執行；地端由客戶的維運人員執行。

## 找到 Job 與它的日誌

```sh
kubectl -n <namespace> get jobs -l app.kubernetes.io/component=migrate
kubectl -n <namespace> logs job/<release>-migrate-<hash>
```

日誌是 JSON 行，只寫 stdout：每一項變更（`created role ...`、`applied ...`、`set the password of ...`）一行，停止時最後一行寫出原因。日誌不含密碼與 verifier。服務的 pod 停在 `Init:0/1` 或 `Init:Error` 表示它在等這個 Job，`kubectl -n <namespace> logs <pod> -c wait-for-migrations` 寫出它在等哪個 Job。

## 2026-10-13 之前建立的叢集內 Postgres：給 owner CREATEROLE

chart 在 Cluster bootstrap 時以 `postInitApplicationSQL` 給 `trustiot_owner` CREATEROLE；bootstrap 只執行一次，所以這之前建立的 Cluster 沒有。Job 會停止並寫出 `it lacks CREATEROLE`。升級到含 migration Job 的版本之前（或看到這個錯誤之後），以 superuser 經 pod 內的 local socket 補一次：

```sh
kubectl -n <namespace> exec -i <cluster>-1 -c postgres -- psql -d trustiot -c 'ALTER ROLE trustiot_owner CREATEROLE'
kubectl -n <namespace> exec -i <cluster>-1 -c postgres -- psql -d trustiot -tAc "SELECT rolcreaterole, rolsuper, rolbypassrls FROM pg_roles WHERE rolname = 'trustiot_owner'"
```

第二個指令印出 `t|f|f` 即完成。Job 失敗之後才補的，Job 會依 `migrate.backoffLimit` 重試；重試已用完時，刪掉失敗的 Job 讓下一次 `helm upgrade`（SaaS 是下一次 Flux 調和）重建它：`kubectl -n <namespace> delete job <release>-migrate-<hash>`。

外部 Postgres（`postgres.mode: external`）由客戶的資料庫管理者建立 migration 角色：擁有資料庫、有 CREATEROLE，不是 superuser、沒有 BYPASSRLS（PostgreSQL 16 以上），不擁有其他資料庫，帳密放在 `postgres.external.migrationSecret` 指向的 Secret。

## `postgres` 與 `template1`：收回 PUBLIC 的 CONNECT 與 TEMPORARY

Postgres 在 initdb 之後讓 `PUBLIC` 能連 `postgres` 與 `template1`，並能在 `postgres` 建立暫存表與暫存函式（`template1` 只給 `PUBLIC` CONNECT；PostgreSQL 15 起 `PUBLIC` 在兩者的 `public` schema 都不能建立物件，2026-10-15 在 PostgreSQL 18.6 上確認）。所以 Job 建立的服務角色也能連過去、在 `postgres` 佔用暫存空間，在 `template1` 開著的 session 還會讓 `CREATE DATABASE` 失敗（`source database "template1" is being accessed by other users`）。服務不需要這兩個資料庫，要收回 `PUBLIC` 的 `CONNECT` 與 `TEMPORARY`（Laysi/trustiot-v2#400）。migration 角色不擁有它們，Job 收不回來：

- Job 每次執行都檢查服務的登入角色能連哪些其他資料庫（`datallowconn` 的資料庫），每個資料庫記一行 WARN，例如 `database postgres: service roles assets_app, assets_platform, identity_app, identity_platform may connect to it and create temporary tables there; revoke CONNECT and TEMPORARY on it from PUBLIC as a superuser`。這是警告，Job 照常完成。
- 收回之後，服務角色連這兩個資料庫時被拒（`permission denied for database "postgres"`、`User does not have CONNECT privilege`）；服務自己的資料庫不受影響：新資料庫不複製 `template1` 的權限。

### 叢集內 Postgres：Laysi/trustiot-v2#400 之前 bootstrap 的 Cluster

之後 bootstrap 的 Cluster 由 chart 的 `bootstrap.initdb.postInitSQL` 做好，不用處理（[deploy/chart/README.md](../../deploy/chart/README.md) 的「叢集內的 Postgres」）。bootstrap 只執行一次，所以之前建立的 Cluster 要以 superuser 經 primary pod 內的 local socket 執行一次同一個檔案。檔案在 chart 裡：repo 的 `deploy/chart/files/postgres/system-databases.sql`，或打包的 chart 解開後的 `trustiot/files/postgres/system-databases.sql`。它是冪等的，再執行一次不會改變任何東西，不需要重啟，也不中斷複寫：`streaming_replica` 與 `cnpg_metrics_exporter` 在同一個檔案裡先依名稱拿到 `postgres` 的 `CONNECT`。

1. 找出 primary：`kubectl -n <namespace> get cluster <cluster> -o jsonpath='{.status.currentPrimary}'`，下面寫成 `<primary>`。
2. 在 repo 根目錄（或解開的 chart 旁）執行檔案：

   ```sh
   kubectl -n <namespace> exec -i <primary> -c postgres -- psql -d postgres -v ON_ERROR_STOP=1 < deploy/chart/files/postgres/system-databases.sql
   ```

   輸出 `DO`、`GRANT`、`REVOKE`、`REVOKE`。
3. 確認：

   ```sh
   kubectl -n <namespace> exec -i <primary> -c postgres -- psql -d postgres -tAc "SELECT datname, datacl FROM pg_database WHERE datname IN ('postgres', 'template1') ORDER BY 1"
   ```

   印出下面兩行即完成；Cluster 有 Pooler 時 `postgres` 多一個 `cnpg_pooler_pgbouncer=c/postgres`（operator 建立這個角色時自己給的），也是正常的：

   ```text
   postgres|{postgres=CTc/postgres,streaming_replica=c/postgres,cnpg_metrics_exporter=c/postgres}
   template1|{postgres=CTc/postgres}
   ```

4. 收回不會中斷已經開著的連線。看有沒有 TrustIoT 的角色連在這兩個資料庫：`... psql -d postgres -tAc "SELECT usename, datname, count(*) FROM pg_stat_activity WHERE datname IN ('postgres', 'template1') GROUP BY 1, 2"`。服務角色（`<服務>_app`、`<服務>_platform`）或 `trustiot_owner` 出現時，先查它為什麼連過去（服務不會這樣做），再以 `SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname IN ('postgres', 'template1') AND usename = '<角色>'` 中止。`postgres`、`streaming_replica`、`cnpg_metrics_exporter` 是 operator 自己的連線，不要中止。
5. 下一次 Job 執行時日誌沒有 `may connect to it` 的警告。

### 外部 Postgres

由客戶的資料庫管理者以 superuser（託管服務以擁有這兩個資料庫的管理帳號）執行。不要執行 chart 的檔案：它另外建立 CloudNativePG 的角色，外部 Postgres 用不到。

1. 先找出需要連 `postgres` 或 `template1` 的其他角色，依名稱給它們 `CONNECT`，例如監控代理、備份工具、連線池的驗證帳號，以及 `pg_rewind` 用的複寫帳號（它連 `postgres`）：`GRANT CONNECT ON DATABASE postgres TO <角色>;`。superuser 不受影響。
2. 收回：

   ```sql
   REVOKE CONNECT, TEMPORARY ON DATABASE postgres FROM PUBLIC;
   REVOKE CONNECT, TEMPORARY ON DATABASE template1 FROM PUBLIC;
   ```

   叢集還有其他 `datallowconn` 的資料庫、而 TrustIoT 的角色不該連過去時，對它們做同樣的收回（一個 Postgres 叢集只放一個 TrustIoT 資料庫，見下一節）。
3. 確認：`SELECT datname, datacl FROM pg_database ORDER BY 1` 裡，這兩個資料庫的 `datacl` 不再有以 `=` 開頭（`PUBLIC`）的項目；並照上一節第 4 步處理已經開著的連線。下一次 Job 執行時日誌沒有 `may connect to it` 的警告。

客戶自己的應用需要 `PUBLIC` 連得上某個資料庫時，那是管理者的決定：Job 只警告、不停止，每次執行都會記下這個例外。

## 一個 Postgres 叢集只放一個 TrustIoT 資料庫

Postgres 的角色屬於整個叢集，不屬於某個資料庫；Job 建立的角色名稱（`identity_owner`、`identity_app` 等）不帶實例名稱。同一個叢集上的第二個 TrustIoT 資料庫會用到同名的角色：第一個實例的服務帳號與密碼就能連第二個實例的資料庫，反之亦然。所以：

- `bundled`：每個 release 有自己的 CloudNativePG Cluster，不會發生。
- `external`：每個 TrustIoT 實例（例如正式與測試環境）各用一個 Postgres 叢集。不要讓同一個 migration 角色擁有兩個資料庫，也不要以 `GRANT <角色> TO <另一個 migration 角色> WITH ADMIN OPTION` 把一個實例的角色分給另一個。

Job 遇到這種部署會停止，不改任何角色：migration 角色擁有超過一個資料庫時寫出 `it owns more than one database`；角色在叢集的其他資料庫擁有物件或權限時寫出 `owns objects or holds privileges in another database`；角色不是這個 migration 角色建立的時寫出 `exists but the migration role did not create it`。處理方式是把第二個實例搬到自己的 Postgres 叢集（新叢集上第一次執行 Job 就會建立它自己的角色），再刪掉舊叢集上那個多出來的資料庫。

## 服務角色與 collector 的密碼

不用準備：在 `mesh.calls` 列出 `postgres` 的每個服務、以及 OpenTelemetry collector，各有一個 chart 宣告的空 Secret（`<release>-<服務>-password`、`<release>-otel-collector-password`），Job 第一次執行時產生密碼寫進去、再以它設定角色與帳號（[密鑰與權限](../architecture/secrets.md) 的「migration Job 產生的密碼」）。沒有人需要看到或複製這些密碼；要確認時只看 key 是否存在，不要印出值：`kubectl -n <namespace> get secret <release>-identity-password -o jsonpath='{.data}' | jq 'keys'`。時序查詢服務（`services.telemetry-query`）啟用時，它的 Secret 另有 ClickHouse 服務帳號 `trustiot_telemetry_query` 的密碼（同名 key），同樣由 Job 產生並設定。

## 輪替服務角色與 collector 的密碼

1. 把 `migrate.passwordGeneration` 調大一（SaaS：在覆寫檔改，經 PR 合併後由 Flux 套用；地端：`helm upgrade --reuse-values --set migrate.passwordGeneration=<新值>` 或改 values 檔）。Job 名稱跟著變，所以 Job 重跑；它為每個密碼 Secret 產生新密碼、更新 annotation `trustiot.io/password-generation`，再設定角色與帳號。服務與 collector 的 init container 改等新的 Job，所以它們跟著滾動更新，以新密碼連線。
2. 確認：新 Job 的日誌對每個 Secret 有 `generated the password of <角色或帳號> in Secret <名稱> (password generation <新值>)`，接著有 `set the password of <角色>`（collector 是 `set the password of ClickHouse account <帳號>`）；`kubectl -n <namespace> rollout status deploy/<release>-<服務>` 與 collector 的 rollout 完成，服務健康。
3. 舊密碼在 Job 設定新密碼的那一刻失效。Postgres 已建立的連線不受影響，直到服務的舊 pod 被換掉；舊 pod 在這段時間開新連線會失敗（readiness 讓它退出負載平衡）。舊的 collector 寫 ClickHouse 會失敗並重試（每批最多 5 分鐘），新的 collector 就緒後恢復。Postgres 一個角色只有一個密碼，這段時間無法省掉；需要時在低流量時段輪替。懷疑外洩時，再以 superuser 中止舊連線：`kubectl -n <namespace> exec -i <cluster>-1 -c postgres -- psql -d trustiot -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = '<角色>'"`。時序查詢的租戶 ClickHouse 帳號（`trustiot_tenant_account_*`）的密碼由服務帳號的密碼推導，但 Job 不改它們：每個租戶帳號保留舊的推導密碼，直到該租戶下一次查詢時服務以新的推導密碼登入失敗、重設它的密碼再查（`services/telemetry-query/README.md`）；一直沒有查詢的租戶的帳號一直是舊密碼。平常的輪替不用處理；懷疑服務帳號的密碼外洩時，舊密碼推導得出每個租戶帳號的舊密碼，照下方「懷疑時序查詢的服務帳號外洩」處理。

**輪替到一半 Job 失敗時**：Job 先把新密碼寫進 Secret，再 `ALTER ROLE`（collector 是 `ALTER USER`）。在這兩步之間失敗（例如資料庫斷線、Job 被中止），Secret 已是新密碼、角色還是舊密碼：已經在跑的 pod 不受影響，但這段時間重啟或新起的 pod 以 Secret 的新密碼登不進去（服務停在未就緒、collector 寫入重試），直到 Job 重試成功把角色設成 Secret 的值。這個順序是刻意的：反過來先改角色、再寫 Secret 時，失敗會留下一個沒有任何 Secret 記得密碼的角色；現在的順序任何時候 Secret 都有一個角色會接受（或下一次 Job 會設定）的密碼。處理：看 Job 的日誌、修好停止的原因，讓 Job 依 `migrate.backoffLimit` 重試（重試用完時刪掉失敗的 Job，下一次升級或調和重建它）。不要手動把 Secret 改回舊值。

同一個值再跑一次（例如改 `migrate.revision`）不會再換密碼：annotation 已經記著這一代。代以數字比較、只往上走：`helm rollback`（或 Flux 的 HelmRelease 升級失敗後以 `strategy: rollback` 回復）渲染出較小的值時，Job 保留現有的密碼，記的代不變，日誌沒有 `generated the password`；之後再升級回較大的值也不換，因為 Secret 記的已經是它。要真的換，就調到比 Secret 記的更大的值（`kubectl -n <namespace> get secret <release>-identity-password -o jsonpath='{.metadata.annotations.trustiot\.io/password-generation}'`）。

Job 只信任它自己填過的 Secret 物件：寫入時在 annotation `trustiot.io/password-secret-uid` 記下 Secret 的 uid。Secret 被刪掉後重建（包括有人自己建一個同名、帶 Helm ownership metadata 的 Secret 讓 `helm install` 接手）或從備份還原時 uid 變了，下一次 Job 把每個 key 都重新產生並設定，日誌寫 `(the Secret is not the one the Job filled)`；同一次寫入也拿掉 chart 與 Job 不設定的 annotation、標籤、`ownerReferences` 與 `finalizers`，日誌有 `removed from Secret <名稱> what neither the chart nor the Job sets: <key 清單>`（只有 key，沒有值）；服務隨新的 Job 名稱滾動更新。

## 懷疑時序查詢的服務帳號外洩

時序查詢的服務帳號 `trustiot_telemetry_query` 能建立與修改每個 `trustiot_tenant_account_*` 帳號（包括改名與加上別的密碼），它的密碼也推導得出每個租戶帳號的密碼。只輪替它的密碼不夠：租戶帳號在該租戶下一次查詢之前保留舊的推導密碼。關掉服務（`services.telemetry-query.enabled: false`）也不夠：Job 只讓服務帳號不能登入（`HOST NONE`），租戶帳號照樣能以推導密碼登入。

1. 依上一節輪替密碼（調大 `migrate.passwordGeneration`），確認新 Job 的日誌有 `set the password of ClickHouse account trustiot_telemetry_query`。
2. 以管理帳號刪掉服務建立的每個租戶帳號。叢集內 ClickHouse 照 [密鑰的 runbook](secrets.md) 在 pod 內執行，管理帳號的密碼不離開 pod（外部 ClickHouse 以客戶的管理帳號執行同樣的兩個查詢）：

   ```bash
   kubectl -n <namespace> exec -i <release>-clickhouse-0 -c clickhouse -- bash -s <<'EOF'
   c() { clickhouse-client --user "$CLICKHOUSE_USER" --password "$(cat "$CLICKHOUSE_PASSWORD_FILE")" "$@"; }
   c --query "SELECT concat('DROP USER IF EXISTS ', name, ';') FROM system.users
     WHERE match(name, '^trustiot_tenant_account_[0-9a-f]{32}\$') ORDER BY name FORMAT TSVRaw" | c --multiquery
   EOF
   ```

   只挑服務會用的名稱（前綴加 32 個十六進位數字），所以產生的 SQL 裡只有這種名稱；其他名稱在下一步處理。服務在每個租戶下一次查詢時以新的推導密碼重建它的帳號，不用重啟服務；在那之前該租戶的帳號不存在，沒有人能以它登入。
3. 找出 Job 與服務都不認得的帳號（服務帳號能以 `RENAME` 把租戶帳號改成別的名稱）：在同一個 pod 內執行 `c --query "SELECT name, storage FROM system.users WHERE NOT match(name, '^trustiot_tenant_account_[0-9a-f]{32}\$') ORDER BY name"`。認得的只有管理帳號（`users_xml`）、`trustiot_collector`（或 values 設定的 collector 帳號）、`trustiot_telemetry_query`、`trustiot_tenant_view_definer`、`trustiot_rollup_definer`（彙總表的 DEFINER，Laysi/trustiot-v2#141），以及外部 ClickHouse 上客戶自己的帳號。其他帳號稽核後以 `DROP USER` 刪掉。
4. 確認下一次 Job 沒有下方表格裡 `trustiot_telemetry_query` 或 `trustiot_tenant_reader` 的停止訊息。

Job 還不稽核租戶帳號本身（多出的驗證方式、設定或角色、改名）；這些記在 Laysi/trustiot-v2#528。

## 密碼 Secret 的資料遺失

有人刪了密碼 Secret 的 key 或整個 Secret，或以 `helm upgrade --force-replace` 換掉了它（換掉時不帶資料，見下方「不要用 `helm upgrade --force-replace`」）：已在執行的 pod 不受影響（環境變數在啟動時讀好），但之後重啟的 pod 讀不到 key 會停在 `CreateContainerConfigError`。處理：

1. 把 `migrate.revision` 改成新的值再升級（SaaS 經 PR，地端 `helm upgrade`）。Helm 升級時重建被刪掉的 Secret（空的），Job 重跑時為缺的 key 產生新密碼並設定角色，日誌有 `generated the password of <角色> in Secret <名稱> (it had none)`。
2. 服務與 collector 隨新的 Job 名稱滾動更新，以新密碼連線。

Job 不能建立 Secret（它的 Role 沒有 `create`）：Secret 不存在時它停止，日誌寫出 `Secret <名稱>: does not exist` 與上面的處理方式。一般的 `helm upgrade` 與 `helm rollback`、Flux 的升級與 rollback 都不動這些 Secret 的資料（chart 渲染出的 Secret 沒有 `data`）。

服務角色可以自己 `ALTER ROLE ... PASSWORD` 改掉自己的密碼（Postgres 允許每個角色改自己的密碼，Job 擋不住）。服務程式不會這樣做；若發生（例如服務被入侵），同一個服務的其他 pod 以 Secret 的密碼登不進去，直到 Job 再跑一次把密碼設回 Secret 的值：改 `migrate.revision` 讓 Job 重跑，並調查原因；懷疑外洩時改做上一節的輪替。

Postgres 若開著 `log_statement = 'all'`（或 `log_min_duration_statement` 記下每一句），伺服器的日誌會記下 Job 設定密碼的那一句，裡面是 SCRAM-SHA-256 的 verifier（伺服器本來就存的值，不是密碼；2026-10-13 的驗證確認日誌裡沒有明文密碼）。verifier 仍可用來離線猜密碼，所以 Postgres 的伺服器日誌要當成敏感資料保管。這個設定只有 superuser 能改，migration 角色關不掉它；需要時請資料庫管理者調整。

### 不要用 `helm upgrade --force-replace`

`--force-replace` 不以 patch 更新，而是以 chart 渲染的內容整個取代每個物件（API 的 PUT replace）。對這個 chart 它會（2026-10-13 在 lab 叢集上確認，2026-10-14 的獨立驗證更正了機制）：

1. 把兩種密碼 Secret 取代成 chart 渲染的樣子：物件還是同一個（uid 不變），但 `data` 與 Job 記的 annotation（代與 uid）都被清空，雖然它們帶 `helm.sh/resource-policy: keep`。
2. 接著取代同名的 migration Job 時失敗（`spec.selector ... field is immutable`），升級以 FAILED 結束，Job 不會再跑。

所以操作者看到的是「升級失敗」，但密碼已經沒了：之後任何重啟的服務或 collector pod 都會停在 `CreateContainerConfigError`。看到這種失敗要立刻做上一段的處理（改 `migrate.revision` 再一般地升級，不帶 `--force-replace`），Job 會產生新密碼並設定。這個 chart 沒有需要 `--force-replace` 的情況；升級卡住時看 Job 與服務的日誌，不要用它。

### 升級失敗與 `--cleanup-on-fail`

`helm upgrade --cleanup-on-fail`（Flux 的 HelmRelease 設了 `upgrade.cleanupOnFail: true`，見 `deploy/flux/clusters/saas/trustiot/helm-release.yaml`）在升級失敗時刪掉這次升級新建立的物件，包括這次才宣告的密碼 Secret（例如同一次升級新開的服務），`helm.sh/resource-policy: keep` 擋不住（2026-10-13 在 lab 叢集上確認）。Job 可能已經以它設定了角色的密碼。不需要手動處理：下一次成功的升級重建空的 Secret，Job 為它產生新密碼並設定（日誌有 `(it had none)`），叢集自己恢復。Flux 失敗後的 rollback（`strategy: rollback`）渲染出較舊的 `migrate.passwordGeneration` 時也不換密碼（上方「輪替服務角色與 collector 的密碼」）。

### 要求 Secret 帶某個標籤或 annotation 的叢集政策

Job 第一次填入一個密碼 Secret 時（以及 Secret 被別人重建、或從備份還原之後），會在同一個 patch 拿掉 chart 與 Job 都不設定的標籤與 annotation（[密鑰與權限](../architecture/secrets.md) 的「只信任 Job 填過的物件」）。所以由 Helm post-renderer 或 Flux HelmRelease 的 `spec.commonMetadata` 加上的 key，在第一次填入時會被拿掉一次；之後的升級會把它們加回來，Job 不再動已綁定的 Secret。SaaS 的 HelmRelease 兩者都沒用。

叢集若有政策（例如 Kyverno 或 Gatekeeper）要求 Secret 一定要帶某個只有 post-renderer 或 `commonMetadata` 會加的標籤或 annotation，admission 會拒絕 Job 的這個 patch，Job 每次執行都停在同一個 Secret：

- 症狀：Job 的日誌寫 `Secret <名稱>: the API refused to patch it`（HTTP 400 或 403，依政策引擎而定），叢集的事件或政策報告寫出拒絕的是哪個政策；這時 Job 的 Role 本身沒有問題，不要放寬它。
- 處理，二選一：讓那個政策豁免兩種密碼 Secret（`<release>-<服務>-password`、`<release>-otel-collector-password`，標籤 `trustiot.io/filled-by=migrate`）；或改用會自動加上那個 key 的 mutating 政策，它也會在 Job 的 patch 上把 key 加回去，patch 就會通過。改好之後 Job 依 `migrate.backoffLimit` 重試，重試用完時刪掉失敗的 Job，讓下一次升級或調和重建它。

### 安裝或升級時密碼 Secret 的 apply conflict

症狀：`helm install` 或 `helm upgrade` 失敗，訊息有 `conflict occurred while applying object <namespace>/<release>-…-password` 與 `conflicts with "<field manager>"`（2026-10-14 在 lab 叢集上以 helm 4.3.0 重現）。

原因：有人在 chart 之前先建立了這個密碼 Secret，而且在 chart 也會設定的 key 上（例如標籤 `app.kubernetes.io/component`、annotation `trustiot.io/description`）放了不同的值。helm 4 預設以 server-side apply 安裝，所以拒絕接手別的 field manager 擁有、值又不同的欄位。

先當成安全事件處理。錯誤訊息裡的 field manager 只是用戶端自己填的名稱（例如 `kubectl-create`），只能當線索，不能證明是誰；實際建立者要從 API server 的 audit log 確認，前提是叢集有開 audit log 而且紀錄還在。Job 若已經跑過（安裝失敗時 Job 照樣會執行），它已經拿掉白名單以外的每個 key、換掉 Secret 裡的密碼（日誌有 `removed from Secret …` 與 `(the Secret is not the one the Job filled)`）；衝突的那兩個 key 在白名單裡，所以還留著對方的值。這不會洩漏密碼，只會擋住安裝與升級。

處理，二選一：

- 直接用 Helm 時：`helm upgrade … --force-conflicts`，以 chart 的值接手這些欄位，Job 綁定的密碼不變（lab 已確認）。之後確認 `trustiot.io/password-secret-uid` 等於 Secret 自己的 uid：`kubectl -n <namespace> get secret <名稱> -o jsonpath='{.metadata.uid} {.metadata.annotations.trustiot\.io/password-secret-uid}'`。
- 任何部署工具都適用：刪掉這個 Secret，再依上方「密碼 Secret 的資料遺失」改 `migrate.revision` 升級。Helm 重建空的 Secret，Job 產生新密碼，所以這個服務的密碼會輪替一次。

Flux（helm-controller v1.6.5，隨 `.prototools` 釘選的 flux 2.9.6 安裝；以下依 `deploy/flux/clusters/saas/flux-system/gotk-components.yaml` 裡 HelmRelease CRD 的欄位說明，**沒有在叢集上驗證**）：

- `spec.install.serverSideApply` 預設開啟。`spec.upgrade.serverSideApply` 預設 `auto`，沿用 release 之前的方式。CRD 說 server-side apply「always forces conflicts with other field managers」，所以這個衝突應該不會擋住 Flux 的安裝或升級，helm-controller 會像 `--force-conflicts` 一樣接手。
- 不要為了它設 `spec.upgrade.force`。CRD 說它在 server-side apply 下沒有作用；在 client-side apply 下則是「replacement strategy」，也就是上方禁止的 `--force-replace`。
- 衝突沒有消失時，改用刪除 Secret 再改 `migrate.revision` 的做法。

## 停用服務之後留下的 Secret

在 values 關掉一個服務（或把它的 `mesh.calls` 拿掉 `postgres`）之後，下一次 Job 把它的登入角色改成 `NOLOGIN`，日誌有 `revoked the login of role <服務>_app: no Secret is bound to it`；服務從 tools/migrate 的服務清單移除或改名時是 `... no service in tools/migrate's registry has it`。關掉 collector 或改它的帳號名稱時，舊的 ClickHouse 帳號改成 `HOST NONE`，日誌有 `revoked the login of ClickHouse account <帳號>: the collector no longer uses it`；時序查詢服務沒有啟用時，它的服務帳號同樣是 `HOST NONE`，日誌有 `revoked the login of ClickHouse account trustiot_telemetry_query: telemetry-query is not deployed`；只有服務帳號被鎖，已建立的租戶帳號不受影響，仍能以推導密碼登入（懷疑外洩時見上方「懷疑時序查詢的服務帳號外洩」）。角色與帳號不刪除（可能擁有物件；重新啟用時 Job 先設 Secret 的密碼再恢復登入）。只有 Job 標記過、migration 角色有 ADMIN OPTION 的角色會被改，superuser、客戶的帳號、其他實例的角色不會。ClickHouse 沒有可以標記帳號的地方，所以 Job 把拿到 `trustiot_otel_collector` 角色、但不是目前 collector 帳號的帳號都當成它的：外部 ClickHouse 上不要把這個角色給自己的帳號，給了的帳號會被改成 `HOST NONE`（以管理帳號 `ALTER USER <帳號> HOST ANY` 並 `REVOKE trustiot_otel_collector FROM <帳號>` 復原）。Job 執行時用的 migration 帳號本身不會被改，在 ClickHouse 設定檔（`users.xml` 或 `users.d/`）裡定義的帳號也不會（`ALTER USER` 改不了它們，Job 也從不建立這種帳號）。`NOLOGIN` 不中斷已經開著的連線；被關掉的服務的 pod 隨升級刪除，懷疑還有連線時以 superuser 執行 `SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = '<角色>'`。

它的密碼 Secret `<release>-<服務>-password` 因為 `keep` 留在 namespace 裡，chart 與 Job 的 Role 都不再列它。裡面的密碼已經登不進去，可以刪除：

1. 確認服務真的關了：`helm -n <namespace> get manifest <release> | grep '<release>-<服務>-password'` 沒有輸出，且 Job 的日誌有上面的 `revoked the login of role <服務>_app`（或角色是 `NOLOGIN`：`... psql -d trustiot -tAc "SELECT rolcanlogin FROM pg_roles WHERE rolname = '<服務>_app'"` 印出 `f`）。
2. 刪除：`kubectl -n <namespace> delete secret <release>-<服務>-password`。

`helm uninstall` 之後不再重裝時也一樣，每個 `trustiot.io/filled-by=migrate` 的 Secret 都可以刪：`kubectl -n <namespace> get secret -l trustiot.io/filled-by=migrate`（例如升級失敗、再 uninstall 之後留下的、只有那次升級才宣告的 Secret）。之後重新啟用或重裝時 chart 重新宣告空的 Secret，Job 產生新密碼。

## Job 停止時

Job 遇到下列情況會停止，不自動修改；修好之後，Job 依 `migrate.backoffLimit` 重試，重試已用完就刪掉失敗的 Job（見上一節第一段），下一次升級或調和重建它。

| 日誌中的原因 | 意思 | 處理 |
| --- | --- | --- |
| `it is a superuser` / `it has BYPASSRLS` / `it lacks CREATEROLE` / `it does not own the database` | migration 角色不符合規則 | bundled：補 CREATEROLE（上方）；external：請客戶的資料庫管理者調整 |
| `no service in tools/migrate's registry` | 服務要用的角色沒有登記在 tools/migrate | 程式的錯，開 issue |
| `Secret <名稱>: does not exist` | 密碼 Secret 被刪掉了 | 見上方「密碼 Secret 的資料遺失」 |
| `Secret <名稱>: is being deleted` | 密碼 Secret 正在刪除（有 `deletionTimestamp`，可能被 finalizer 擋著）；Job 不把密碼寫進一個即將消失的 Secret | 找出擋著它的 finalizer 與原因（`kubectl get secret <名稱> -o jsonpath='{.metadata.finalizers}'`），讓刪除完成，再依「密碼 Secret 的資料遺失」處理 |
| `Secret <名稱>: the API refused to get it` / `refused to patch it` | 只有 `refused to patch it`、而且是 admission 政策拒絕時，見上方「要求 Secret 帶某個標籤或 annotation 的叢集政策」。其他情況：Job 的 Role 不在或被改過 | 確認 `<release>-migrate` 的 Role 與 RoleBinding 存在且與 chart 渲染的相同（`helm get manifest`）；不要放寬它 |
| `Secret <名稱>: changed 5 times while the run filled it` | 有別的程式在同時寫這個 Secret | 找出寫入者（`kubectl get secret -o yaml` 的 `managedFields`）並停掉它；只有 `trustiot-migrate` 該寫 `data` |
| `has SUPERUSER, CREATEROLE, CREATEDB or REPLICATION` / `has BYPASSRLS` | 有人改過服務的角色 | 先查是誰、為什麼改（稽核），再以 superuser `ALTER ROLE <角色> NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION NOBYPASSRLS` |
| `must not be a member` / `another role is a member` / `only the migration role may be a member` | 角色多了成員關係，會讓別的角色拿到它的權限 | 稽核後以 superuser `REVOKE <角色> FROM <成員>` 或 `REVOKE <其他角色> FROM <角色>` |
| `exists but the migration role did not create it` | 角色不是這個 migration 角色建立的：同一個 Postgres 叢集上有另一個 TrustIoT 實例，或有人手動建了同名角色 | 另一個實例：見上方「一個 Postgres 叢集只放一個 TrustIoT 資料庫」。手動建立的：確認沒有在用之後以 superuser `DROP ROLE`，下一次 Job 重建它。不要給 migration 角色 ADMIN OPTION 來繞過 |
| `it owns more than one database` / `owns objects or holds privileges in another database` | 同一個 Postgres 叢集上有兩個 TrustIoT 資料庫 | 見上方「一個 Postgres 叢集只放一個 TrustIoT 資料庫」 |
| `has settings for this database` / `has role settings other than its search_path` | 角色有 `ALTER ROLE ... [IN DATABASE ...] SET` 的設定，會蓋過 Job 設定的 `search_path` 或改變服務的連線（服務角色也能替自己設定） | 稽核後以 superuser `ALTER ROLE <角色> IN DATABASE <資料庫> RESET ALL` 或 `ALTER ROLE <角色> RESET <設定>` |
| `may create objects in its schema or temporary tables` | 服務角色能在自己的 schema 建立物件，或又能建立暫存表（決策 29 不給） | 稽核後 `REVOKE CREATE ON SCHEMA <schema> FROM <角色>`；暫存表是 `REVOKE TEMPORARY ON DATABASE <資料庫> FROM PUBLIC`（以及 `FROM <角色>`） |
| `left its transaction or its role` | 某個 migration 檔案結束了 Job 的交易，或切換了角色（程式的錯，`tests/registry.rs` 的檢查沒擋到） | 開 issue，改掉那個檔案（它沒有被記為已套用，可以直接改）。檔案若結束過交易，結束之前的部分已經提交，下一次執行會在它上面失敗：修正版要能接受已存在的物件，或在稽核後移除它們 |
| `is owned by ..., not ...`（schema） | schema 不是由 Job 建立的 | 確認內容後改 owner 或刪掉 schema；不要讓服務在別人擁有的 schema 上執行 |
| `changed after it was applied` | 已套用的 migration 檔案被改過 | migration 只能新增檔案：把改動移到新檔案，還原舊檔案。不要改 `trustiot_migrate.applied` |
| ClickHouse：`the collector account ... has other roles, grants or settings` | collector 的 ClickHouse 帳號被人改過（多了角色、權限、設定或 settings profile） | 稽核後以管理帳號 `DROP USER <帳號>`，下一次 Job 重建它；collector 在重建前寫不進日誌 |
| ClickHouse：`the role trustiot_otel_collector has grants or roles beyond 0006_logs.sql` | collector 的角色被加了權限或角色，拿到這個角色的帳號都會得到它們 | 稽核後以管理帳號 `REVOKE <權限> ON <對象> FROM trustiot_otel_collector` 或 `REVOKE <角色> FROM trustiot_otel_collector` |
| ClickHouse：`the ClickHouse account trustiot_telemetry_query may grant the role trustiot_tenant_reader to other users or roles` | 服務帳號的 `GRANTEES` 不再是 `NONE`（`0009_telemetry_query_grantees.sql` 設定的）；它以 `ADMIN OPTION` 持有租戶角色，於是能把角色給任何使用者或角色、或從租戶帳號收回 | 以管理帳號 `ALTER USER trustiot_telemetry_query GRANTEES NONE`，再查 `system.role_grants` 裡誰拿到 `trustiot_tenant_reader`；懷疑服務帳號外洩時照「懷疑時序查詢的服務帳號外洩」處理 |
| ClickHouse：`the ClickHouse account trustiot_telemetry_query has other roles, grants or settings than 0008_telemetry_query.sql` | 時序查詢的服務帳號被人改過；它能建立與修改租戶帳號，多出的權限會落到每個租戶 | 稽核後以管理帳號 `REVOKE` 多出的權限、角色或 `ALTER USER trustiot_telemetry_query SETTINGS NONE` |
| ClickHouse：`the role trustiot_tenant_reader is held by ... grantee(s) besides trustiot_telemetry_query` | 租戶角色被給了服務帳號與 `trustiot_tenant_account_*` 帳號以外的使用者或角色，或某個租戶帳號拿到 `ADMIN OPTION`；持有者都讀得到租戶 view。服務帳號有 `GRANTEES NONE`，給不了別人，所以是有權授與的管理帳號做的 | 稽核後以管理帳號 `REVOKE trustiot_tenant_reader FROM <使用者或角色>`，租戶帳號是 `REVOKE ADMIN OPTION FOR trustiot_tenant_reader FROM <帳號>`；若同時出現上一列的 `GRANTEES` 訊息，照「懷疑時序查詢的服務帳號外洩」處理 |
| ClickHouse：`the role trustiot_tenant_reader lacks SELECT on trustiot.<view>` | 租戶角色對某個租戶 view（`tenant_telemetry` 由 `0005_tenant_view.sql` 給，`tenant_telemetry_1m`、`tenant_telemetry_1h` 由 `0010_telemetry_rollups.sql` 給）的 `SELECT` 被收回，所有租戶帳號都讀不到那個 view | 稽核後以管理帳號 `GRANT SELECT ON trustiot.<view> TO trustiot_tenant_reader` |
| ClickHouse：`the role trustiot_tenant_reader has grants or roles beyond SELECT on the tenant views` | 租戶角色被加了三個租戶 view 的 `SELECT` 以外的權限（包括表的權限與 `GRANT OPTION`）或角色，每個租戶帳號都會得到它們 | 稽核後以管理帳號 `REVOKE <權限> ON <對象> FROM trustiot_tenant_reader` 或 `REVOKE <角色> FROM trustiot_tenant_reader`；`GRANT OPTION` 是 `REVOKE GRANT OPTION FOR SELECT ON trustiot.<view> FROM trustiot_tenant_reader` |
| `did not accept the migration role within`、`not reachable within`、`held the lock for longer than` | 資料庫還沒起來，或另一個 Job 持有鎖太久 | 看資料庫的狀態；另一個 Job 卡住時先看它的日誌 |

上面五個關於 `trustiot_telemetry_query` 與 `trustiot_tenant_reader` 的停止訊息出現時，Job 在停止之前先讓服務帳號不能登入（`HOST NONE`），訊息後面接著 `the run revoked the login of trustiot_telemetry_query (HOST NONE)`：修好之前服務建立不了新的租戶帳號，也重設不了租戶帳號的密碼，已建立的租戶帳號照常查詢。修好之後，服務啟用時的下一次 Job 設定密碼並恢復登入，日誌有 `let ClickHouse account trustiot_telemetry_query log in`。

migration 檔案本身失敗（SQL 錯誤）時，那一個檔案的交易整個回滾，之前套用的檔案保留；修正方式同 `Changed`：新增一個修正用的檔案，不改已套用的。

## 服務的 pod 等不到 Job

`wait-for-migrations` 的日誌寫 `the migration Job <名稱> does not exist` 時，這個 pod 等的 Job 已經不在：

- 剛 `helm install` 或 `helm upgrade`：Job 幾秒內就會出現，等一下即可。
- 升級或 `helm rollback` 換掉了 Job，而這個 pod 屬於另一個 release revision：讓升級完成（新 pod 會取代它），或以 `helm rollback` 回到有這個 Job 的 revision。
- 要刻意讓 Job 再跑一次（例如資料庫從備份還原、改接另一個資料庫之後，角色與密碼要補回）：改 `migrate.revision`，所有服務會改等新的 Job。

## 服務因為 migration 不 ready

每個用 Postgres 的服務確認資料庫已套用它的映像預期的每個 migration 檔案，缺少或內容不同時 `/readyz` 回 503（Laysi/trustiot-v2#402，[部署與可搬移性](../architecture/deployment.md)的「服務自己確認 migration 已套用」）。服務的日誌有這一行 ERROR：

```text
the database lacks migrations this image expects, so the service stays unready; run the migration Job of this release (tools/migrate, docs/runbooks/migrations.md)
```

欄位 `missing` 是缺少的檔案、`changed` 是資料庫套用了別的版本的檔案（附映像與資料庫的 SHA-256），都寫成 `<component>/<檔名>`，例如 `identity/0002_tenants.sql`、`identity.outbox/outbox-aggregate.sql`；`no_records` 為 `true` 表示服務讀不到紀錄表 `<schema>.trustiot_migrate_applied`（這一版之前的 Job 沒有建立它）。處理：

1. 確認服務的映像與 migration Job 是同一個版本（`kubectl -n <namespace> get deploy,job -o wide` 的 image tag；chart 要求兩者相同）。只以 digest 修補某個服務、或手動改過 Deployment 時，改回與 Job 相同的版本，或以 `helm upgrade` 讓這一版的 Job 跑一次。
2. `migrate.enabled=false`（由外部流程套用 migration）時，外部流程要以這一版的 `migrate` 執行一次，否則服務一直不 ready。
3. Job 跑完後服務自己轉為 ready，不必重啟。
4. `changed` 不是空的：同名檔案在資料庫裡是另一個版本，不是少跑 Job。已套用的檔案不能改（見上方「Job 停止時」的 `changed after it was applied`），開 issue 查是哪個映像帶著改過的檔案；不要手動改 `trustiot_migrate.applied` 或服務的紀錄表，Job 下一次執行會把紀錄表改回與 `trustiot_migrate.applied` 相同。

資料庫比映像新（`helm rollback` 或只回退某個服務的映像之後）時服務照常 ready：migration 只能往前相容，舊的程式要能在新的 schema 上執行。

服務通過這項檢查之後，在行程存活期間不再檢查。資料庫從較舊的備份還原（還原點早於某些 migration）之後，先讓 Job 再跑一次（上一節：改 `migrate.revision`），再重新啟動用 Postgres 的服務（`kubectl -n <namespace> rollout restart deploy`），讓它們重新確認。

## 兩個 Job 同時執行

Job 以 Postgres 的 advisory lock 互斥，第二個等第一個結束（最多 `migrate.wait.lock`），之後發現沒有事要做就結束。不需要手動處理；`helm rollback` 與升級重疊時也一樣。

## 原始資料的保存期限

Job 每次執行都把 ClickHouse 的 `trustiot.telemetry` 與 `trustiot.batch_commits` 的 TTL 對齊 `TRUSTIOT__CLICKHOUSE__RAW_RETENTION_DAYS`（不設是 180 天；值與理由見[資料保存期限與磁碟水位](../architecture/data-retention.md)）。

- **設定被拒（結束碼 2）**：訊息寫出 `clickhouse.raw_retention_days` 與閘道器緩衝的最長保存時間 30 天。設定必須是 31 到 3650 的整數；不長於緩衝時，閘道器斷線後重送的批次找不到已過期的完成紀錄，資料會重複。Job 在連任何資料庫之前就停止，什麼都沒改。
- **「its TTL ... is not one the migration Job manages」**：有人手動改了表的 TTL（多了條件、換了欄位或運算式）。Job 不覆蓋，因為改法可能是刻意的。以管理帳號查目前的設定：`SELECT engine_full FROM system.tables WHERE database = 'trustiot' AND name = '<表名>'`，確認改動的原因後改回 Job 管理的形式，下一次執行就把天數對齊設定：

  ```sql
  ALTER TABLE trustiot.<表名> MODIFY TTL toDateTime(<received_at 或 committed_at>) + toIntervalDay(<天數>)
  ```

  `telemetry` 用 `received_at`、`batch_commits` 用 `committed_at`；不要改用資料本身的時間戳 `ts`，時鐘錯的閘道器會讓資料與高水位提早被清掉。
- **ClickHouse 升級之後出現上面的訊息**：Job 認得的形式是 `toIntervalDay(<天數>)`，以及 ClickHouse 26.8 存它自己帶參數寫入的 `toIntervalDay(_CAST(<天數>, 'UInt32'))`。新版 ClickHouse 若換了存法，Job 會認不出自己寫的 TTL 而停止，沒有資料被刪。照上面的指令以整數天數重寫一次，再回報 issue 讓 Job 認得新的存法。
- **改天數的代價**：改 TTL 後 ClickHouse 在背景執行 `MATERIALIZE TTL`，重寫每個 part，占用 I/O 與暫時的磁碟空間；縮短時要等它跑完才釋放空間。在 `system.mutations` 看進度（`WHERE database = 'trustiot' AND NOT is_done`）。
- **縮短是產品決定**：縮短會永久刪掉超過新天數的資料，不是解決磁碟壓力的手段；磁碟快滿時擴充磁碟（[資料保存期限與磁碟水位](../architecture/data-retention.md)「磁碟水位告警」）。手動改的天數在下一次部署會被 Job 改回設定值。

## 彙總資料的保存期限

Job 每次執行也把 `trustiot.telemetry_1m`、`trustiot.telemetry_1h` 的 TTL 對齊 `TRUSTIOT__CLICKHOUSE__AGGREGATE_1M_RETENTION_MONTHS`、`_1H_RETENTION_MONTHS`（不設是 13 與 48 個月，Laysi/trustiot-v2#141；值與理由見[資料保存期限與磁碟水位](../architecture/data-retention.md)「彙總資料的保存期限」）。它先讀完四張表的 TTL 再改，所以一張表的 TTL 認不出來時，四張表都沒有改。

- **設定被拒（結束碼 2）**：訊息寫出 `clickhouse.aggregate_1m_retention_months` 或 `clickhouse.aggregate_1h_retention_months` 與違反的規則：1 分鐘 13 到 120 個月且至少涵蓋原始資料的天數，1 小時不短於 1 分鐘、不超過 120 個月。超過 120 多半是把天數當成月數。Job 在連任何資料庫之前就停止，什麼都沒改。
- **「its TTL ... is not one the migration Job manages」**（彙總表）：以管理帳號查 `engine_full`（同上一節），確認改動的原因後改回 Job 管理的形式，下一次執行就把月數對齊設定：

  ```sql
  ALTER TABLE trustiot.<telemetry_1m 或 telemetry_1h> MODIFY TTL received_day + toIntervalMonth(<月數>) + toIntervalDay(1)
  ```

  一律用 `received_day`（收到日），不要改用 `bucket`（讀值時間），時鐘錯的閘道器會讓彙總提早被清掉。ClickHouse 26.8 存成 `(received_day + toIntervalMonth(<月數>)) + toIntervalDay(1)`，Job 自己寫入時月數是 `_CAST(<月數>, 'UInt32')`，兩種 Job 都認得；升級 ClickHouse 後認不出時，處理同上一節。
- 改 TTL 的代價與「縮短是產品決定」同上一節。縮短彙總的保存期限刪掉的年份，原始資料早已過期，無法重算。

## 彙總表的 backfill 與一致性檢查

Job 第一次套用 `0010_telemetry_rollups.sql` 的那次執行，在套用完檔案、對齊 TTL 之後，把 `telemetry` 裡已有的列補進兩張彙總表，記在 `trustiot_migrate.applied` 的 `backfill/telemetry_rollups`，日誌有 `backfilled the one-minute and one-hour aggregates from the raw rows received from <日期> to <日期> ...`（[services/ingest](../../services/ingest/README.md)「既有資料的 backfill」）。它只補彙總缺的格、不改已有的格，所以重跑不會重複計算，也不會拿掉任何一格。

**界限**：backfill view 只比對 `today − raw_retention_days + 2`（UTC）以後的收到日。`raw_retention_days` 一律傳原始資料實際保存的天數，也就是 `telemetry` 現在的 TTL（上方「原始資料的保存期限」的 `engine_full` 裡 `toIntervalDay(<天數>)` 的天數）；最近延長過原始資料的保存期限時，在延長之後的「新天數 − 舊天數」天內改傳舊的天數，因為舊的較短期限已經刪掉的列不會回來。界限以前（原始保存期限的最後兩天與更早）的收到日，彙總與原始資料不一致是預期的：原始列已經過期，只剩彙總。**絕不能「修正」這些日子**：修正會抹掉只剩彙總的歷史，無法復原。

- **Job 停在 backfill，原因是 `TIMEOUT_EXCEEDED`**：訊息寫出表與收到日（`backfill trustiot.telemetry_1m for the receive day <日期>`）。那一天的一句在 25 秒（`max_execution_time`，低於 Job 30 秒的請求逾時）內沒有做完，在伺服器上也已停止；已補的格保留，下一次執行只補還缺的格。Laysi/trustiot-v2#91 之前沒有程式持續寫入 `telemetry`（只有 `tools/lab-dataset` 載入的測試資料），所以預期不會發生；大表上的 backfill 時間還沒有實測。重試仍停在同一天時，不要拿掉時間上限（Job 放棄等待之後，一句還在跑的 backfill 會和下一次執行同時寫，重複計算）；開 issue，附上原始資料的列數（`SELECT total_rows FROM system.tables WHERE database = 'trustiot' AND name = 'telemetry'`）。Job 在 backfill 途中被 Kubernetes 停掉（超過 `activeDeadlineSeconds`）時沒有機會把 policy 改回 `TO NONE`，Job 的帳號仍讀得到每個租戶的彙總：做一次下方檢查步驟的最後兩句（改回 `TO NONE` 並確認）。
- **讓 Job 再補一次**（例如彙總表少了一段收到日的格）：刪掉紀錄 `ALTER TABLE trustiot_migrate.applied DELETE WHERE name = 'backfill/telemetry_rollups' SETTINGS mutations_sync = 2`，下一次執行以那時的保存期限補回界限以內缺的格，已有的格不變。同樣只在沒有程式寫入 `telemetry` 時做（見下方「修正」）。
- **檢查彙總是否與原始資料一致**：backfill view 平常不給任何帳號看（row policy `migration_account` 是 `TO NONE`）。檢查的帳號（叢集內 ClickHouse 是管理帳號，外部 ClickHouse 是給 Job 的帳號）先點名自己，做完一定改回 `TO NONE`。叢集內 ClickHouse 在 pod 內執行（`c` 的定義同上方「懷疑時序查詢的服務帳號外洩」，`<天數>` 依上面的界限）：

  ```bash
  kubectl -n <namespace> exec -i <release>-clickhouse-0 -c clickhouse -- bash -s <<'EOF'
  c() { clickhouse-client --user "$CLICKHOUSE_USER" --password "$(cat "$CLICKHOUSE_PASSWORD_FILE")" "$@"; }
  c --query "ALTER ROW POLICY migration_account ON trustiot.telemetry_1m_backfill, trustiot.telemetry_1h_backfill TO CURRENT_USER"
  for table in telemetry_1m telemetry_1h; do
    c --query "SELECT '$table', received_day, aggregated, count(), sum(readings) FROM trustiot.${table}_backfill(first_day = '<YYYY-MM-DD>', last_day = '<YYYY-MM-DD>', raw_retention_days = <天數>) GROUP BY received_day, aggregated ORDER BY received_day, aggregated"
  done
  c --query "ALTER ROW POLICY migration_account ON trustiot.telemetry_1m_backfill, trustiot.telemetry_1h_backfill TO NONE"
  c --query "SELECT table, apply_to_list FROM system.row_policies WHERE short_name = 'migration_account'"
  EOF
  ```

  沒有任何列表示這段收到日裡、界限以內的每一格都一致；`first_day` 早於界限時，界限以前的日子不會列出，不代表一致。`aggregated = 0` 是彙總缺的格；`1` 是彙總有、但筆數或和不同的格，`sum(readings)` 是負的表示彙總多算（例如一批被刪掉而沒有對帳）。今天（UTC）的格與 ingest 正在重寫的批次可能暫時不一致。最後一句應該印出兩列 `[]`：還點名某個帳號時，那個帳號讀得到每個租戶的彙總，要改回 `TO NONE`。
- **修正不一致的收到日**：只在沒有任何程式寫入 `telemetry` 時做（Laysi/trustiot-v2#91 之前；之後先把 ingest 縮到 0 個 replica），否則一個寫到一半的區塊會被算兩次。`raw_retention_days` 與檢查時相同，絕不能比上面的界限長。同一個 pod 內，像檢查一樣先點名自己，對兩張表各執行一次下面的語句（`<table>` 換成 `telemetry_1m` 或 `telemetry_1h`；就是 `services/ingest/src/rollup.rs` 的 `REPAIR_1M`、`REPAIR_1H`），最後改回 `TO NONE` 並確認：

  ```sql
  INSERT INTO trustiot.<table> (tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day, readings, numeric_readings, value_sum, value_min, value_max, first, last)
  SETTINGS deduplicate_insert_select = 'disable'
  SELECT tenant_id, device_id, point, bucket, gateway_id, stream_id, received_day, readings, numeric_readings, value_sum, value_min, value_max, first, last
  FROM trustiot.<table>_backfill(first_day = '<YYYY-MM-DD>', last_day = '<YYYY-MM-DD>', raw_retention_days = <天數>)
  ```

  寫入的是差，重跑無害；再做一次上面的檢查應該沒有任何列。`deduplicate_insert_select = 'disable'` 不能省：與之前某次相同的差值會被當成重複丟掉。只補缺的格、不改已有的格時（Job 的做法，`BACKFILL_1M`、`BACKFILL_1H`），在最後加上 `WHERE aggregated = 0`。

## 回到 Laysi/trustiot-v2#141 之前的版本

`0010_telemetry_rollups.sql` 給了租戶角色 `trustiot_tenant_reader` 兩個彙總 view 的 `SELECT`。這之前的版本的 Job 只認得 `tenant_telemetry`，會把這兩個權限當成多出的權限，停在 `the role trustiot_tenant_reader has grants or roles beyond SELECT on trustiot.tenant_telemetry`，並讓 `trustiot_telemetry_query` 不能登入（上方「Job 停止時」）；服務的 pod 也會等不到 Job。所以 `helm rollback`（或 Flux 失敗後的 rollback）回到這之前的版本時：

1. 以管理帳號收回兩個權限：`REVOKE SELECT ON trustiot.tenant_telemetry_1m FROM trustiot_tenant_reader`、`REVOKE SELECT ON trustiot.tenant_telemetry_1h FROM trustiot_tenant_reader`。舊版不讀彙總表，租戶照常讀原始資料。
2. 讓舊版的 Job 再跑一次（刪掉失敗的 Job 或改 `migrate.revision`）。

表、view 與 DEFINER 帳號都留著（舊版不管它們），materialized view 照常填入彙總表。之後再升級回這一版時，Job 停在 `lacks SELECT on trustiot.tenant_telemetry_1m`：照「Job 停止時」的表把兩個權限加回去。
