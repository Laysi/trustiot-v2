# migration Job

migration Job（[tools/migrate](../../tools/migrate/README.md)，Laysi/trustiot-v2#60）的維運步驟：補給既有 Cluster 的權限、輪替服務的資料庫密碼、Job 失敗時怎麼處理。Job 做什麼、何時執行，見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「migration Job」。

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

## 一個 Postgres 叢集只放一個 TrustIoT 資料庫

Postgres 的角色屬於整個叢集，不屬於某個資料庫；Job 建立的角色名稱（`identity_owner`、`identity_app` 等）不帶實例名稱。同一個叢集上的第二個 TrustIoT 資料庫會用到同名的角色：第一個實例的服務帳號與密碼就能連第二個實例的資料庫，反之亦然。所以：

- `bundled`：每個 release 有自己的 CloudNativePG Cluster，不會發生。
- `external`：每個 TrustIoT 實例（例如正式與測試環境）各用一個 Postgres 叢集。不要讓同一個 migration 角色擁有兩個資料庫，也不要以 `GRANT <角色> TO <另一個 migration 角色> WITH ADMIN OPTION` 把一個實例的角色分給另一個。

Job 遇到這種部署會停止，不改任何角色：migration 角色擁有超過一個資料庫時寫出 `it owns more than one database`；角色在叢集的其他資料庫擁有物件或權限時寫出 `owns objects or holds privileges in another database`；角色不是這個 migration 角色建立的時寫出 `exists but the migration role did not create it`。處理方式是把第二個實例搬到自己的 Postgres 叢集（新叢集上第一次執行 Job 就會建立它自己的角色），再刪掉舊叢集上那個多出來的資料庫。

## 安裝前：服務角色的密碼

在 `mesh.calls` 列出 `postgres` 的每個服務，`postgres.serviceRolesSecret` 都要有一個 `<服務>_app` 的 key（例如 `identity_app`）；少一個時 Job 什麼都不改就停止。

- SaaS：在 `deploy/flux/` 的 SOPS 檔案加上這個 key（`sops edit`，值以 `openssl rand -hex 32` 產生，不放在指令列，見[密鑰輪替](secrets.md)的「準備」）。
- 地端：值經管線傳入，不寫成檔案、不出現在命令列（[密鑰與權限](../architecture/secrets.md) 的規則 3）：`openssl rand -hex 32 | tr -d "\n" | kubectl -n <namespace> create secret generic trustiot-postgres-roles --from-file=identity_app=/dev/stdin`（`tr` 去掉結尾的換行）。之後的服務各加一個 key：以 `kubectl -n <namespace> edit secret trustiot-postgres-roles` 在編輯器裡加上 base64 編碼的值。

密碼要有 16 到 512 個字元，只能是可列印的 ASCII、不含空白，結尾也不能有換行（服務從同一個 key 讀到的密碼會含那個換行）；不符時 Job 停止，訊息不含密碼本身。

## 輪替服務角色的密碼

Job 每次執行都先以 Secret 的密碼登入一次，登不進去才設定新密碼，所以只要讓 Job 再跑一次：

1. 換掉 Secret 裡該角色的值（SaaS：`sops edit`；地端：上一節的方式）。
2. 把 `migrate.revision` 改成新的值（例如日期 `"2026-10-13"`）。chart 看不到 Secret 的內容，改 revision 才會產生新名稱的 Job；服務的 init container 改等新的 Job，所以服務跟著滾動更新，以新密碼連線。SaaS 在同一個 PR 改 SOPS 檔案與覆寫檔的 `migrate.revision`。
3. 確認：新 Job 的日誌有 `set the password of <角色>`，`kubectl -n <namespace> rollout status deploy/<release>-<服務>` 完成，服務健康。
4. 舊密碼在 Job 設定新密碼的那一刻失效，已建立的連線不受影響，直到服務的舊 pod 被換掉。懷疑外洩時，再以 superuser 中止舊連線：`kubectl -n <namespace> exec -i <cluster>-1 -c postgres -- psql -d trustiot -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = '<角色>'"`。

滾動更新期間，舊版 pod 若重新連線會用到舊密碼而失敗；這段時間很短，服務以 readiness 退出負載平衡。

服務角色可以自己 `ALTER ROLE ... PASSWORD` 改掉自己的密碼（Postgres 允許每個角色改自己的密碼，Job 擋不住）。服務程式不會這樣做；若發生（例如服務被入侵），同一個服務的其他 pod 以 Secret 的密碼登不進去，直到 Job 再跑一次把密碼設回 Secret 的值：改 `migrate.revision` 讓 Job 重跑，並調查原因。

Postgres 若開著 `log_statement = 'all'`（或 `log_min_duration_statement` 記下每一句），伺服器的日誌會記下 Job 設定密碼的那一句，裡面是 SCRAM-SHA-256 的 verifier（伺服器本來就存的值，不是密碼；2026-10-13 的驗證確認日誌裡沒有明文密碼）。verifier 仍可用來離線猜密碼，所以 Postgres 的伺服器日誌要當成敏感資料保管。這個設定只有 superuser 能改，migration 角色關不掉它；需要時請資料庫管理者調整。

OpenTelemetry collector 的 ClickHouse 密碼（`observability.collector.clickhouse.passwordSecret`）同樣由 Job 依 Secret 設定：做第 1、2 步，Job 的日誌有 `set the password of ClickHouse account <帳號>`。collector 不等 Job，也不會因 Job 換名而重啟，所以再 `kubectl -n <namespace> rollout restart deploy -l app.kubernetes.io/component=otel-collector`；重啟之前它寫不進日誌；collector 對每批日誌最多重試 5 分鐘，超過就丟掉，所以 Job 完成後立即重啟。

## Job 停止時

Job 遇到下列情況會停止，不自動修改；修好之後，Job 依 `migrate.backoffLimit` 重試，重試已用完就刪掉失敗的 Job（見上一節第一段），下一次升級或調和重建它。

| 日誌中的原因 | 意思 | 處理 |
| --- | --- | --- |
| `it is a superuser` / `it has BYPASSRLS` / `it lacks CREATEROLE` / `it does not own the database` | migration 角色不符合規則 | bundled：補 CREATEROLE（上方）；external：請客戶的資料庫管理者調整 |
| `its password file is missing` / `no service in tools/migrate's registry` | 服務要用的角色沒有密碼，或那個服務沒有登記在 tools/migrate | 補 Secret 的 key；後者是程式的錯，開 issue |
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
| `did not accept the migration role within`、`not reachable within`、`held the lock for longer than` | 資料庫還沒起來，或另一個 Job 持有鎖太久 | 看資料庫的狀態；另一個 Job 卡住時先看它的日誌 |

migration 檔案本身失敗（SQL 錯誤）時，那一個檔案的交易整個回滾，之前套用的檔案保留；修正方式同 `Changed`：新增一個修正用的檔案，不改已套用的。

## 服務的 pod 等不到 Job

`wait-for-migrations` 的日誌寫 `the migration Job <名稱> does not exist` 時，這個 pod 等的 Job 已經不在：

- 剛 `helm install` 或 `helm upgrade`：Job 幾秒內就會出現，等一下即可。
- 升級或 `helm rollback` 換掉了 Job，而這個 pod 屬於另一個 release revision：讓升級完成（新 pod 會取代它），或以 `helm rollback` 回到有這個 Job 的 revision。
- 要刻意讓 Job 再跑一次（例如資料庫從備份還原、改接另一個資料庫之後，角色與密碼要補回）：改 `migrate.revision`，所有服務會改等新的 Job。

## 兩個 Job 同時執行

Job 以 Postgres 的 advisory lock 互斥，第二個等第一個結束（最多 `migrate.wait.lock`），之後發現沒有事要做就結束。不需要手動處理；`helm rollback` 與升級重疊時也一樣。
