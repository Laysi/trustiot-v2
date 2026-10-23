# 第一位平台管理員

部署之後的第一位平台管理員（清單 A2）由操作者在叢集內執行一次 `migrate bootstrap-platform-admin` 建立（Laysi/trustiot-v2#582）。沒有任何 HTTP 路由能建立平台管理員；其他使用者與成員邀請在 Laysi/trustiot-v2#105。指令本身見 [tools/migrate](../../tools/migrate/README.md)，chart 的 CronJob 見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「第一位平台管理員」。

下文的 `<namespace>` 是 TrustIoT 的 namespace，`<release>` 是 Helm release 名稱，`<cluster>` 是叢集內 Postgres 的 Cluster 名稱（通常是 `<release>-postgres`）。SaaS 在主機上以 `sudo k3s kubectl` 執行（展示環境同樣在展示機上執行）；地端由客戶的維運人員執行。

## 規則

- **一個部署只成功一次**：成功時在 `identity.platform_admin_bootstrap` 寫下唯一的一列（使用者 ID 與時間），之後的每一次執行都以結束碼 3 結束、什麼都不改，即使後來所有平台管理員都被移除。同時執行兩次時只有一次成功，另一次也是 3。要再建立一位，照下方「復原：再建立一次」。資料庫重建（例如展示環境的資料重設）之後這一列也不在了，所以可以再執行一次。
- **電子郵件地址只從 stdin 讀**：不寫進 values、Secret、ConfigMap、Job 的參數或環境變數，也不出現在日誌、操作紀錄與測試報告。不要以 `echo` 或 `printf` 帶地址（會留在 shell 的歷史紀錄），在 attach 之後直接輸入。
- **地址的比對與登入相同**：去掉頭尾空白、ASCII 轉小寫。已經有這個地址的使用者時，把它設為平台管理員（ID 不變）；沒有時建立新使用者。
- **不給租戶權限**：平台管理員不會自動成為任何租戶的成員。登入之後以 `POST /api/identity/v1/tenants` 建立租戶；建立租戶也不會讓他成為那個租戶的成員（[services/identity/README.md](../../services/identity/README.md) 的「租戶」）。
- **操作紀錄**：成功時 `identity.platform_event_log` 多一筆 `identity.platform_admin_granted`，payload 只有 `{"user_id": "<使用者 ID>", "via": "bootstrap"}`，沒有地址，也沒有 actor（執行的是操作者，不是平台上的使用者或服務）。

## 前提

- 身分服務已啟用（`services.identity.enabled`），而且這個版本的 migration Job 已完成：`kubectl -n <namespace> get jobs -l app.kubernetes.io/component=migrate` 有完成的 `<release>-migrate-<hash>`。
- CronJob 存在：`kubectl -n <namespace> get cronjob <release>-platform-admin-bootstrap`，`SUSPEND` 是 `True`。它不會自己執行。
- 收得到登入驗證碼的郵件設定（[services/identity/README.md](../../services/identity/README.md) 的寄信設定），否則建好的管理員登不進來。

## 步驟

1. 從 CronJob 建立一個 Job（名稱自選，下面用 `platform-admin-bootstrap`）：

   ```sh
   kubectl -n <namespace> create job --from=cronjob/<release>-platform-admin-bootstrap platform-admin-bootstrap
   kubectl -n <namespace> get pod -l job-name=platform-admin-bootstrap    # 重複到 STATUS 是 Running
   ```

   pod 在等 stdin：15 分鐘內（`activeDeadlineSeconds: 900`）沒有人寫入，Job 以 `DeadlineExceeded` 失敗，什麼都不改；刪掉 Job 從這一步重來。

2. attach 到容器的 stdin，輸入地址，按 Enter：

   ```sh
   kubectl -n <namespace> attach -i job/platform-admin-bootstrap -c bootstrap-platform-admin
   ```

   沒有提示字元，日誌的第一行 `reading the first platform administrator's email address from stdin` 可能已經寫完。只讀第一行，Enter 之後指令就執行，結束時 attach 也結束；這段時間 attach 顯示容器的日誌（JSON 行）。不要把地址先寫進檔案再導入：檔案留存要靠人記得刪。

3. 看結束碼與日誌：

   ```sh
   kubectl -n <namespace> get pod -l job-name=platform-admin-bootstrap -o jsonpath='{.items[0].status.containerStatuses[0].state.terminated.exitCode}'
   kubectl -n <namespace> logs job/platform-admin-bootstrap
   ```

   結束碼的意思見下表。記下日誌裡的 `user_id`。

4. 刪除 Job（它的 pod 有日誌；不刪時一天後自動刪除）：

   ```sh
   kubectl -n <namespace> delete job platform-admin-bootstrap
   ```

5. 確認：以這個地址在 app 登入（電子郵件驗證碼），建立第一個租戶。操作紀錄也可以確認（[services/identity/README.md](../../services/identity/README.md) 的平台操作紀錄 API，或以 superuser 查 `identity.platform_event_log`）。

## 結束碼

| 結束碼 | 日誌 | 意思與處理 |
| --- | --- | --- |
| 0 | INFO `created the first platform administrator`，欄位 `user_id`、`existing_user`（`true` 表示這個地址原本就有使用者） | 完成。 |
| 3 | WARN `the first platform administrator was created before; nothing changed (docs/runbooks/platform-admin.md)`，欄位 `user_id`、`created_at`（UTC） | 這個部署已經建立過第一位平台管理員，這次什麼都沒改。`user_id` 是當時那一位。確實要再建立一位時照「復原：再建立一次」。 |
| 2 | ERROR `no email address on stdin`（原因：不是一個地址、不是 UTF-8、第一行超過 1024 bytes、讀不到 stdin），或設定不能用、帶了參數 | 什麼都沒改。原因不引用輸入的內容。刪掉 Job，從步驟 1 重來。 |
| 1 | ERROR `the first platform administrator was not created`，欄位 `error` | 什麼都沒改（整個交易回復）。`error` 只寫哪一步、SQLSTATE 與 constraint，不寫資料：`identity.platform_admin_bootstrap does not exist` 表示這個版本的 migration Job 還沒跑完；連不上 Postgres 時先等 `migrate.wait.databases`（預設 10 分鐘）；migration 角色不符時與 migration Job 停止的原因相同（[migration Job runbook](migrations.md)）。修好之後刪掉 Job，從步驟 1 重來。 |

kubectl 的 `attach` 不回傳容器的結束碼，所以一律以步驟 3 的 pod 狀態為準。

## 復原：再建立一次

只在第一位平台管理員已經無法使用（帳號遺失、所有平台管理員都被移除），而且確認要再建立一位時執行。這會讓下一次 `bootstrap-platform-admin` 再成功一次；之前的操作紀錄保留，下一次成功時多一筆 `identity.platform_admin_granted`。

1. 舊帳號可能外洩時，先以 superuser 收回它的平台管理員並結束它的 session（`<user_id>` 是步驟 3 或結束碼 3 的日誌記的那一個）。下一次請求起生效，已經簽出的內部 token 最多再用 60 秒：

   ```sql
   UPDATE identity.users SET platform_admin = false WHERE id = '<user_id>';
   UPDATE identity.sessions SET revoked_at = now() WHERE user_id = '<user_id>' AND revoked_at IS NULL;
   ```

2. 刪除標記：

   ```sql
   DELETE FROM identity.platform_admin_bootstrap;
   ```

   叢集內的 Postgres 以 superuser 經 primary 的 local socket 執行。先找出 primary：`kubectl -n <namespace> get cluster <cluster> -o jsonpath='{.status.currentPrimary}'`，下面寫成 `<primary>`：

   ```sh
   kubectl -n <namespace> exec -i <primary> -c postgres -- psql -d trustiot -v ON_ERROR_STOP=1 -c 'DELETE FROM identity.platform_admin_bootstrap'
   ```

   外部 Postgres 由資料庫管理者以 superuser 執行同一個 `DELETE`（或以 migration 角色先 `SET ROLE identity_owner`）。

3. 照「步驟」再執行一次，結束碼應為 0。

這幾個操作直接改資料庫，不經身分服務，所以操作紀錄只有之後那一筆 `identity.platform_admin_granted`；誰在何時刪除了標記，要記在變更紀錄或事件的處理紀錄裡。
