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

## 服務角色與 collector 的密碼

不用準備：在 `mesh.calls` 列出 `postgres` 的每個服務、以及 OpenTelemetry collector，各有一個 chart 宣告的空 Secret（`<release>-<服務>-password`、`<release>-otel-collector-password`），Job 第一次執行時產生密碼寫進去、再以它設定角色與帳號（[密鑰與權限](../architecture/secrets.md) 的「migration Job 產生的密碼」）。沒有人需要看到或複製這些密碼；要確認時只看 key 是否存在，不要印出值：`kubectl -n <namespace> get secret <release>-identity-password -o jsonpath='{.data}' | jq 'keys'`。

## 輪替服務角色與 collector 的密碼

1. 把 `migrate.passwordGeneration` 調大一（SaaS：在覆寫檔改，經 PR 合併後由 Flux 套用；地端：`helm upgrade --reuse-values --set migrate.passwordGeneration=<新值>` 或改 values 檔）。Job 名稱跟著變，所以 Job 重跑；它為每個密碼 Secret 產生新密碼、更新 annotation `trustiot.io/password-generation`，再設定角色與帳號。服務與 collector 的 init container 改等新的 Job，所以它們跟著滾動更新，以新密碼連線。
2. 確認：新 Job 的日誌對每個 Secret 有 `generated the password of <角色或帳號> in Secret <名稱> (password generation <新值>)`，接著有 `set the password of <角色>`（collector 是 `set the password of ClickHouse account <帳號>`）；`kubectl -n <namespace> rollout status deploy/<release>-<服務>` 與 collector 的 rollout 完成，服務健康。
3. 舊密碼在 Job 設定新密碼的那一刻失效。Postgres 已建立的連線不受影響，直到服務的舊 pod 被換掉；舊 pod 在這段時間開新連線會失敗（readiness 讓它退出負載平衡）。舊的 collector 寫 ClickHouse 會失敗並重試（每批最多 5 分鐘），新的 collector 就緒後恢復。Postgres 一個角色只有一個密碼，這段時間無法省掉；需要時在低流量時段輪替。懷疑外洩時，再以 superuser 中止舊連線：`kubectl -n <namespace> exec -i <cluster>-1 -c postgres -- psql -d trustiot -c "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = '<角色>'"`。

**輪替到一半 Job 失敗時**：Job 先把新密碼寫進 Secret，再 `ALTER ROLE`（collector 是 `ALTER USER`）。在這兩步之間失敗（例如資料庫斷線、Job 被中止），Secret 已是新密碼、角色還是舊密碼：已經在跑的 pod 不受影響，但這段時間重啟或新起的 pod 以 Secret 的新密碼登不進去（服務停在未就緒、collector 寫入重試），直到 Job 重試成功把角色設成 Secret 的值。這個順序是刻意的：反過來先改角色、再寫 Secret 時，失敗會留下一個沒有任何 Secret 記得密碼的角色；現在的順序任何時候 Secret 都有一個角色會接受（或下一次 Job 會設定）的密碼。處理：看 Job 的日誌、修好停止的原因，讓 Job 依 `migrate.backoffLimit` 重試（重試用完時刪掉失敗的 Job，下一次升級或調和重建它）。不要手動把 Secret 改回舊值。

同一個值再跑一次（例如改 `migrate.revision`）不會再換密碼：annotation 已經記著這一代。代以數字比較、只往上走：`helm rollback`（或 Flux 的 HelmRelease 升級失敗後以 `strategy: rollback` 回復）渲染出較小的值時，Job 保留現有的密碼，記的代不變，日誌沒有 `generated the password`；之後再升級回較大的值也不換，因為 Secret 記的已經是它。要真的換，就調到比 Secret 記的更大的值（`kubectl -n <namespace> get secret <release>-identity-password -o jsonpath='{.metadata.annotations.trustiot\.io/password-generation}'`）。

Job 只信任它自己填過的 Secret 物件：寫入時在 annotation `trustiot.io/password-secret-uid` 記下 Secret 的 uid。Secret 被刪掉後重建（包括有人自己建一個同名、帶 Helm ownership metadata 的 Secret 讓 `helm install` 接手）或從備份還原時 uid 變了，下一次 Job 把每個 key 都重新產生並設定，日誌寫 `(the Secret is not the one the Job filled)`；同一次寫入也拿掉 chart 與 Job 不設定的 annotation、標籤、`ownerReferences` 與 `finalizers`，日誌有 `removed from Secret <名稱> what neither the chart nor the Job sets: <key 清單>`（只有 key，沒有值）；服務隨新的 Job 名稱滾動更新。

## 密碼 Secret 的資料遺失

有人刪了密碼 Secret 的 key 或整個 Secret，或以 `helm upgrade --force-replace` 換掉了它（換掉時不帶資料，見下方「不要用 `helm upgrade --force-replace`」）：已在執行的 pod 不受影響（環境變數在啟動時讀好），但之後重啟的 pod 讀不到 key 會停在 `CreateContainerConfigError`。處理：

1. 把 `migrate.revision` 改成新的值再升級（SaaS 經 PR，地端 `helm upgrade`）。Helm 升級時重建被刪掉的 Secret（空的），Job 重跑時為缺的 key 產生新密碼並設定角色，日誌有 `generated the password of <角色> in Secret <名稱> (it had none)`。
2. 服務與 collector 隨新的 Job 名稱滾動更新，以新密碼連線。

Job 不能建立 Secret（它的 Role 沒有 `create`）：Secret 不存在時它停止，日誌寫出 `Secret <名稱>: does not exist` 與上面的處理方式。一般的 `helm upgrade` 與 `helm rollback`、Flux 的升級與 rollback 都不動這些 Secret 的資料（chart 渲染出的 Secret 沒有 `data`）。

服務角色可以自己 `ALTER ROLE ... PASSWORD` 改掉自己的密碼（Postgres 允許每個角色改自己的密碼，Job 擋不住）。服務程式不會這樣做；若發生（例如服務被入侵），同一個服務的其他 pod 以 Secret 的密碼登不進去，直到 Job 再跑一次把密碼設回 Secret 的值：改 `migrate.revision` 讓 Job 重跑，並調查原因；懷疑外洩時改做上一節的輪替。

Postgres 若開著 `log_statement = 'all'`（或 `log_min_duration_statement` 記下每一句），伺服器的日誌會記下 Job 設定密碼的那一句，裡面是 SCRAM-SHA-256 的 verifier（伺服器本來就存的值，不是密碼；2026-10-13 的驗證確認日誌裡沒有明文密碼）。verifier 仍可用來離線猜密碼，所以 Postgres 的伺服器日誌要當成敏感資料保管。這個設定只有 superuser 能改，migration 角色關不掉它；需要時請資料庫管理者調整。

### 不要用 `helm upgrade --force-replace`

`--force-replace` 以刪除再建立取代更新。對這個 chart 它會（2026-10-13 在 lab 叢集上確認）：

1. 把兩種密碼 Secret 換成 chart 渲染的空 Secret：`data` 與 Job 記的 annotation（代與 uid）都不見了，雖然它們帶 `helm.sh/resource-policy: keep`。
2. 接著取代同名的 migration Job 時失敗（`spec.selector ... field is immutable`），升級以 FAILED 結束，Job 不會再跑。

所以操作者看到的是「升級失敗」，但密碼已經沒了：之後任何重啟的服務或 collector pod 都會停在 `CreateContainerConfigError`。看到這種失敗要立刻做上一段的處理（改 `migrate.revision` 再一般地升級，不帶 `--force-replace`），Job 會產生新密碼並設定。這個 chart 沒有需要 `--force-replace` 的情況；升級卡住時看 Job 與服務的日誌，不要用它。

### 升級失敗與 `--cleanup-on-fail`

`helm upgrade --cleanup-on-fail`（Flux 的 HelmRelease 設了 `upgrade.cleanupOnFail: true`，見 `deploy/flux/clusters/saas/trustiot/helm-release.yaml`）在升級失敗時刪掉這次升級新建立的物件，包括這次才宣告的密碼 Secret（例如同一次升級新開的服務），`helm.sh/resource-policy: keep` 擋不住（2026-10-13 在 lab 叢集上確認）。Job 可能已經以它設定了角色的密碼。不需要手動處理：下一次成功的升級重建空的 Secret，Job 為它產生新密碼並設定（日誌有 `(it had none)`），叢集自己恢復。Flux 失敗後的 rollback（`strategy: rollback`）渲染出較舊的 `migrate.passwordGeneration` 時也不換密碼（上方「輪替服務角色與 collector 的密碼」）。

## 停用服務之後留下的 Secret

在 values 關掉一個服務（或把它的 `mesh.calls` 拿掉 `postgres`）之後，下一次 Job 把它的登入角色改成 `NOLOGIN`，日誌有 `revoked the login of role <服務>_app: no Secret is bound to it`；服務從 tools/migrate 的服務清單移除或改名時是 `... no service in tools/migrate's registry has it`。關掉 collector 或改它的帳號名稱時，舊的 ClickHouse 帳號改成 `HOST NONE`，日誌有 `revoked the login of ClickHouse account <帳號>: the collector no longer uses it`。角色與帳號不刪除（可能擁有物件；重新啟用時 Job 先設 Secret 的密碼再恢復登入）。只有 Job 標記過、migration 角色有 ADMIN OPTION 的角色會被改，superuser、客戶的帳號、其他實例的角色不會。ClickHouse 沒有可以標記帳號的地方，所以 Job 把拿到 `trustiot_otel_collector` 角色、但不是目前 collector 帳號的帳號都當成它的：外部 ClickHouse 上不要把這個角色給自己的帳號，給了的帳號會被改成 `HOST NONE`（以管理帳號 `ALTER USER <帳號> HOST ANY` 並 `REVOKE trustiot_otel_collector FROM <帳號>` 復原）。Job 執行時用的 migration 帳號本身不會被改。`NOLOGIN` 不中斷已經開著的連線；被關掉的服務的 pod 隨升級刪除，懷疑還有連線時以 superuser 執行 `SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = '<角色>'`。

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
| `Secret <名稱>: the API refused to get it` / `refused to patch it` | Job 的 Role 不在或被改過 | 確認 `<release>-migrate` 的 Role 與 RoleBinding 存在且與 chart 渲染的相同（`helm get manifest`）；不要放寬它 |
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
| `did not accept the migration role within`、`not reachable within`、`held the lock for longer than` | 資料庫還沒起來，或另一個 Job 持有鎖太久 | 看資料庫的狀態；另一個 Job 卡住時先看它的日誌 |

migration 檔案本身失敗（SQL 錯誤）時，那一個檔案的交易整個回滾，之前套用的檔案保留；修正方式同 `Changed`：新增一個修正用的檔案，不改已套用的。

## 服務的 pod 等不到 Job

`wait-for-migrations` 的日誌寫 `the migration Job <名稱> does not exist` 時，這個 pod 等的 Job 已經不在：

- 剛 `helm install` 或 `helm upgrade`：Job 幾秒內就會出現，等一下即可。
- 升級或 `helm rollback` 換掉了 Job，而這個 pod 屬於另一個 release revision：讓升級完成（新 pod 會取代它），或以 `helm rollback` 回到有這個 Job 的 revision。
- 要刻意讓 Job 再跑一次（例如資料庫從備份還原、改接另一個資料庫之後，角色與密碼要補回）：改 `migrate.revision`，所有服務會改等新的 Job。

## 兩個 Job 同時執行

Job 以 Postgres 的 advisory lock 互斥，第二個等第一個結束（最多 `migrate.wait.lock`），之後發現沒有事要做就結束。不需要手動處理；`helm rollback` 與升級重疊時也一樣。
