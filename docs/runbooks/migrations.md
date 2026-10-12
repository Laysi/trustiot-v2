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

外部 Postgres（`postgres.mode: external`）由客戶的資料庫管理者建立 migration 角色：擁有資料庫、有 CREATEROLE，不是 superuser、沒有 BYPASSRLS（PostgreSQL 16 以上），帳密放在 `postgres.external.migrationSecret` 指向的 Secret。

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

OpenTelemetry collector 的 ClickHouse 密碼（`observability.collector.clickhouse.passwordSecret`）同樣由 Job 依 Secret 設定：做第 1、2 步，Job 的日誌有 `set the password of ClickHouse account <帳號>`。collector 不等 Job，也不會因 Job 換名而重啟，所以再 `kubectl -n <namespace> rollout restart deploy -l app.kubernetes.io/component=otel-collector`；重啟之前它寫不進日誌；collector 對每批日誌最多重試 5 分鐘，超過就丟掉，所以 Job 完成後立即重啟。

## Job 停止時

Job 遇到下列情況會停止，不自動修改；修好之後，Job 依 `migrate.backoffLimit` 重試，重試已用完就刪掉失敗的 Job（見上一節第一段），下一次升級或調和重建它。

| 日誌中的原因 | 意思 | 處理 |
| --- | --- | --- |
| `it is a superuser` / `it has BYPASSRLS` / `it lacks CREATEROLE` / `it does not own the database` | migration 角色不符合規則 | bundled：補 CREATEROLE（上方）；external：請客戶的資料庫管理者調整 |
| `its password file is missing` / `no service in tools/migrate's registry` | 服務要用的角色沒有密碼，或那個服務沒有登記在 tools/migrate | 補 Secret 的 key；後者是程式的錯，開 issue |
| `has SUPERUSER, CREATEROLE, CREATEDB or REPLICATION` / `has BYPASSRLS` | 有人改過服務的角色 | 先查是誰、為什麼改（稽核），再以 superuser `ALTER ROLE <角色> NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION NOBYPASSRLS` |
| `must not be a member` / `another role is a member` / `only the migration role may be a member` | 角色多了成員關係，會讓別的角色拿到它的權限 | 稽核後以 superuser `REVOKE <角色> FROM <成員>` 或 `REVOKE <其他角色> FROM <角色>` |
| `exists but the migration role cannot administer it` | 角色不是 Job 建立的 | 確認角色沒有在用之後 `DROP ROLE`，或給 migration 角色 ADMIN OPTION |
| `is owned by ..., not ...`（schema） | schema 不是由 Job 建立的 | 確認內容後改 owner 或刪掉 schema；不要讓服務在別人擁有的 schema 上執行 |
| `changed after it was applied` | 已套用的 migration 檔案被改過 | migration 只能新增檔案：把改動移到新檔案，還原舊檔案。不要改 `trustiot_migrate.applied` |
| ClickHouse：`the collector account ... has other roles or grants` | collector 的 ClickHouse 帳號被人改過 | 稽核後以管理帳號 `DROP USER <帳號>`，下一次 Job 重建它；collector 在重建前寫不進日誌 |
| `did not accept the migration role within`、`not reachable within`、`held the lock for longer than` | 資料庫還沒起來，或另一個 Job 持有鎖太久 | 看資料庫的狀態；另一個 Job 卡住時先看它的日誌 |

migration 檔案本身失敗（SQL 錯誤）時，那一個檔案的交易整個回滾，之前套用的檔案保留；修正方式同 `Changed`：新增一個修正用的檔案，不改已套用的。

## 兩個 Job 同時執行

Job 以 Postgres 的 advisory lock 互斥，第二個等第一個結束（最多 `migrate.wait.lock`），之後發現沒有事要做就結束。不需要手動處理；`helm rollback` 與升級重疊時也一樣。
