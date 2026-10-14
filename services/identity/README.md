# identity（身分服務）

使用者、租戶成員與 server-side session 的擁有者，也是入口 gateway 的 cookie 檢查（ext_authz）：把 session cookie 換成一個短效、簽過章的內部身分 token，交給請求要去的服務（決策 13、27；[部署與可搬移性](../../docs/architecture/deployment.md)「身分與租戶」）。

目前是 MS0 的骨架（Laysi/trustiot-v2#61）：

- 有自己的 schema、角色與 RLS，session 存在伺服器端。
- 有 ext_authz 端點與內部身分簽發。
- **沒有登入**：服務本身無法建立 session，資料庫角色也沒有 session 的寫入權限；MS0 只有測試工具建立 session（見[測試工具怎麼建立 session](#測試工具怎麼建立-session)），不做假登入。登入、登出與 session 寫入在 Laysi/trustiot-v2#103。
- 一個對外的 HTTP API：平台管理者建立租戶，同一個交易寫出 `identity.tenant_created` 事件（見[租戶](#租戶)，Laysi/trustiot-v2#65，2026-10-14）；規格在 `api/openapi.json`（`tools/openapi`）。
- 映像見下方[映像](#映像)；chart 的 `services.identity.enabled` 預設仍是 `false`。nightly 的 smoke 從 Laysi/trustiot-v2#65 起啟用它與資產服務，經入口建立租戶；其餘啟用工作在 Laysi/trustiot-v2#77。

## 與入口的約定（ext_authz）

Istio 的 ext_authz provider（`deploy/istio/istiod-values.yaml`）對每個需要 cookie 的請求，以原始的方法、Host 與 `Cookie` 呼叫 `/internal/ext-authz<原始路徑>`。`/internal` 不會被入口路由，用戶端到不了這個端點。服務只讀 `Cookie`、`Host` 與路徑；請求帶的其他標頭（包含任何寫法的 `x-internal-identity`）都不影響結果。

| 回應 | 情況 | 入口 |
| --- | --- | --- |
| 200，剛好一個 `x-internal-identity`，`cache-control: no-store` | session 有效，原始請求的主機與路徑對得到一條 cookie 路由 | 以這個值覆寫用戶端帶來的同名標頭，轉給後端 |
| 401 `unauthenticated` | 沒有 session cookie、有兩個以上、格式不對；session 不存在、過期、已撤銷或使用者已刪除 | 原樣交給用戶端 |
| 403 `permission_denied` | session 有效，但路由表對不到這個請求 | 原樣交給用戶端 |
| 503 `unavailable` | 資料庫出錯、0.4 秒內查不完，或同時在查的請求太多（見[連線池](#連線池)） | 把 5xx 當成錯誤，回自己的 503（`statusOnError`） |

- 200 只能由剛簽好的 token 組成（程式裡的 `Decision::Allow` 沒有 token 就建不出來）。原因：ext_authz 回 200 卻沒帶 `x-internal-identity` 時，入口會把用戶端自己帶的同名標頭轉給後端（[2026-10-10 PoC](../../docs/feasibility/2026-10-10-edge-poc.md)）。
- 拒絕的回應一律不帶身分標頭，body 是 `contract::ErrorBody`，也帶 `cache-control: no-store`。
- 401 與 403 以 INFO 記一行 `denied the ext_authz check`，`reason` 是 `no_session_cookie`、`repeated_session_cookie`、`malformed_session_cookie`、`no_active_session` 或 `no_route`；日誌不寫 cookie、session 雜湊或 token。
- 每個檢查最多等 0.4 秒（`CHECK_DEADLINE`），比入口的 0.5 秒短，所以入口一定拿得到服務自己的回答；到了就回 503（fail closed）。查詢本身在背景跑完，開到一半的資料庫連線留給下一個檢查（見[連線池](#連線池)）。

### token

JWS compact serialization，格式與驗證規則在 [`crates/service-kit` 的「內部身分」](../../crates/service-kit/README.md)：

- 保護標頭剛好是 `{"alg":"EdDSA","typ":"internal-identity+jwt","kid":"<kid>"}`，Ed25519 簽章。
- payload 是 `contract::InternalClaims`：`iss` 一律 `identity`；`sub` 是使用者；`aud` 是請求要去的服務；`iat` 是簽發時間，`exp = iat + 60`；`tenant` 是 session 目前的租戶與使用者在那裡的角色（沒有選租戶、或已不是成員時省略）；`platform_admin` 是平台管理者（A2）。
- 效期 60 秒（`TOKEN_LIFETIME_SECS`）：token 在效期內是受眾服務的 bearer credential，所以要短；輪替也要等驗證端接受的最長效期（300 秒）。

角色每個請求都從 `memberships` 重新讀：改角色或移除成員，下一個請求就生效，不必等 session 過期。

### 受眾（`aud`）

token 只給請求要去的服務。chart 把 `ingress.routes` 換成實際主機名稱，以 `TRUSTIOT_INGRESS_ROUTES` 交給身分服務（JSON 陣列 `[{host, path, match, service}]`，最多 512 條）；身分服務以 Gateway API 的規則比對原始請求：

- 主機不分大小寫，忽略埠號與結尾的 `.`。
- 先找 `Exact`，再找最長、以路徑段為界的 `PathPrefix`（`/api` 對得到 `/api` 與 `/api/x`，對不到 `/apix`）；`/` 對得到所有路徑。
- 對不到就回 403。路由表與入口不一致時只會多拒絕，不會放寬權限，因為 token 只有它的受眾接受。
- `ai-gateway` 不驗證內部身分（決策 28），chart 不允許它有 cookie 路由，設定裡出現它，服務拒絕啟動。

## session

- cookie 名稱 `__Host-trustiot-session`（`__Host-` 前綴：瀏覽器只接受 `Secure`、`Path=/`、沒有 `Domain` 的這個 cookie，子網域種不進來）。值是 32 bytes 的隨機數，base64url 不補 `=`，43 個字元；只接受唯一的標準寫法。
- 資料庫只存值的 SHA-256（`sessions.token_hash`）：資料表外洩也拿不到可用的 cookie。cookie 缺少、重複或格式不對時直接回 401，不查資料庫。
- 有效的 session：`token_hash` 相符、`revoked_at` 是 NULL、`expires_at > now()`（以資料庫的時鐘判斷），而且使用者還在。

## 租戶

身分服務擁有租戶清單（`identity.tenants`）。平台管理者（A2）以 `POST /api/identity/v1/tenants` 建立租戶（`src/tenants.rs`）：

| 請求 | 回應 |
| --- | --- |
| 沒有有效的內部身分 token | 401 `unauthenticated` |
| 不是平台管理者（租戶的 `admin` 也不是） | 403 `permission_denied` |
| `Content-Type` 不是 `application/json`（可帶 `; charset=utf-8` 之類的參數），或沒有 `Content-Type` | 400 `invalid_request` |
| body 不是帶字串 `name` 的 JSON 物件 | 400 `invalid_request` |
| `name` 去掉前後空白後是空的、超過 100 個字元，或含 Unicode 一般類別 Cc（控制字元）、Cf（格式字元：零寬字元、雙向文字的覆寫與隔離、BOM）、Zl、Zp（行與段落分隔符）的字元 | 422 `validation_failed`，欄位 `/name`，訊息 key `tenant.name_invalid`，`{"max": 100}` |
| 資料庫出錯或拒絕交易 | 503 `unavailable`，什麼都沒有建立 |
| 建立了 | 201，`{"tenant_id", "name", "created_at"}` |

- 呼叫路徑和其他 API 一樣經過入口：cookie 由這個服務自己的 ext_authz 換成內部身分 token，路由 `/api/identity` 的受眾是 `identity`；平台管理者不必屬於任何租戶，所以 token 沒有 `tenant` 也可以建立。給其他服務的 token（受眾不是 `identity`）回 401。
- **以 cookie 認證的寫入一律要求 JSON 的 `Content-Type`**（防 CSRF）：入口把 cookie 換成呼叫者的身分，而別的 origin 的網頁也能讓瀏覽器帶著 cookie 送出 POST，但沒有 CORS preflight 時只能用表單（`application/x-www-form-urlencoded`、`multipart/form-data`）或 `text/plain`。要求 `application/json` 會讓跨 origin 的請求必須先經 preflight，而入口與服務都沒有 CORS 設定、不允許任何 preflight，所以這種網頁無法用管理者的 cookie 建立租戶。日後若加 CORS，不可對外部 origin 允許帶 cookie 的請求。之後每個以 cookie 認證的寫入 API 都照這個規則；Laysi/trustiot-v2#103 的登入、登出與其他 session 寫入也必須遵守（或改用同等的防護），並在 cookie 上設 `SameSite`。
- 新租戶的 ID 由服務產生（UUIDv7），租戶的列與 `identity.tenant_created`（`crates/contract` 的 `events::identity::TenantCreated`，aggregate `tenant`/租戶 ID，事件帶租戶）在**同一個新租戶的 RLS 交易**裡寫入（`outbox::publish`），一起 commit 或一起放棄：有租戶就一定有事件，沒有租戶就沒有事件（`tests/tenants.rs` 撤銷寫 outbox 的權限後證明 503 且沒有留下列）。其他服務從這個事件建立自己的租戶資料，第一個是[資產服務](../assets/README.md)。
- 成功時記一行 INFO `created a tenant`，帶 `user_id` 與 `tenant_id`；租戶名稱可能是公司名，不寫日誌，事件也不帶名稱（要名稱的服務以後經 API 取得）。

## 資料庫

schema `identity`，DDL 在 `migrations/postgres/`，由 migration Job（Laysi/trustiot-v2#60）以 migration 角色依檔名順序套用；執行期程式碼不建表、不改表（`tools/repo-check` 檢查）。角色照 [`crates/service-kit` 的角色與 policy 範本](../../crates/service-kit/README.md)：

| 角色 | 權限 |
| --- | --- |
| `identity_owner`（migration 角色） | 擁有 schema 與所有表 |
| `identity_app`（服務角色） | `identity.users`、`identity.memberships`、`identity.sessions` 的 `SELECT`；`identity.tenants` 的 `SELECT`、`INSERT`；outbox 的寫入權限（`outbox_event` 的 `INSERT`、`outbox_aggregate` 的 `SELECT`、`INSERT`、`UPDATE`，由 migration Job 授權，見 [crates/outbox](../../crates/outbox/README.md)） |
| `identity_platform`（平台角色） | 身分服務是 outbox 的產生端，outbox 的 policy 要有平台角色（全平台事件用），所以 Job 建立它；目前沒有程式以它連線，chart 不綁它的密碼，它維持 `NOLOGIN` |

| 表 | 內容 | RLS |
| --- | --- | --- |
| `users` | 使用者 ID、平台管理者旗標 | 無：使用者跨租戶，不是租戶資料 |
| `memberships` | `(tenant_id, user_id)` 與角色 `tenant_role`（`viewer`、`editor`、`admin`；不叫 `role`，因為 `tools/sql-check` 把 `UPDATE` 這個名稱的欄位當成切換資料庫角色而拒絕） | 照 `crates/service-kit/sql/tenant-table.sql` 的 `tenant_isolation` |
| `sessions` | `token_hash`、使用者、`active_tenant_id`、建立、到期與撤銷時間 | 無 |
| `tenants` | 租戶 ID（整個主鍵）、名稱（1–100 個字元）、建立時間 | 照 `tenant-table.sql` 的 `tenant_isolation` |
| `outbox_event`、`outbox_aggregate` | 這個服務寫出的事件（由 Job 建立，見 [crates/outbox](../../crates/outbox/README.md)） | outbox 範本的 policy |

為什麼 `sessions` 不是租戶資料：查 session 時還不知道租戶（只有 cookie），使用者也可以切換目前的租戶，所以 session 屬於使用者，沒有 `tenant_id`。它記的租戶叫 `active_tenant_id`，不叫 `tenant_id`，連線檢查與 `tools/sql-check` 才不會把它當成租戶欄位。角色一律在那個租戶的 RLS 交易裡從 `memberships` 讀。

讀角色用 `BackgroundTenantDb`，這是 kit 規則的例外：kit 只把它交給背景工作，handler 只拿 `TenantDb`。這裡的租戶來自剛從資料庫讀出的 session 列，不來自請求，所以由 `PgSessionStore` 私下持有 `BackgroundTenantDb`，只在 `lookup` 裡使用，ext_authz handler 拿不到它。

建立租戶是第二個例外：新租戶的交易沒有請求可以提供租戶（token 是平台管理者的，沒有那個租戶）。`TenantStore` 有自己的連線池（服務角色 2 條連線，`POOL_CONNECTIONS`，和 session 查詢的連線池分開，大量建立租戶不會佔走 ext_authz 等著用的連線），私下持有它的 `BackgroundTenantDb`；唯一的方法 `create` 自己產生租戶 ID，所以請求無法選擇交易的租戶，handler 也拿不到 `BackgroundTenantDb`。

## 連線池

開一條新的資料庫連線（TCP、TLS、SCRAM、service-kit 的 session 檢查）在負載高的節點或較慢的網路上可能超過檢查的 0.4 秒。2026-10-11 的獨立驗證（[報告](../../docs/test-reports/2026-10-11-61-verify.md)的 F1）發現：原本取得連線的逾時也是 0.4 秒，開到一半的連線跟著檢查一起被取消，這種環境下連線池永遠補不到連線；readiness 又只要一條連線，就緒之後的第一批檢查仍會 503。現在的作法（`src/session.rs`）：

- **檢查不等新連線**：連線池常駐 3 條連線（`WARM_CONNECTIONS`）。一個檢查同時最多用兩條（session 查詢的連線還在歸還時，成員資格的交易已經在另一條開始），保溫工作再用一條，所以一連串的檢查不必開新連線。
- **readiness 跟著連線池**：背景的保溫工作（`PgSessionStore::keep_warm`）每 2 秒一輪，每輪確認 3 條不同的連線都還能用：
  1. 先把這一輪開始時閒置的每條連線取出來，跑一次 `SELECT pg_backend_pid()` 再還回去。取的方式（sqlx 的 `try_acquire`）不先 ping，所以斷掉的連線不會被悄悄重開，而是由這一輪發現。連線池的閒置連線先進先出，還回去的排到最後，所以開始時閒置的每一條，不是被這一輪、就是被檢查（sqlx 取用前會先 ping）取到過。
  2. 再一條一條取，直到 3 條不同的連線（以 backend PID 區分）都回答過：連線池不到 3 條就同時開缺的條數，連線都在檢查手上或還在開，就每 50 ms 再看一次。

  保溫工作同時只拿一條，而且還回去之後才拿下一條，所以檢查要用的兩條一直空著。一輪確認了 3 條，`/readyz` 才回 200，所以入口只把檢查送給連線已經開好的副本。發現斷掉的連線時，readiness 當下回 503，那條連線關掉，由連線池的 `min_connections` 在背景補開，等這一輪確認了 3 條新的才回 200。資料庫連不上、帳密錯、session 檢查拒絕，或一輪超過 10 秒（`WORK_LIMIT`）時，這一輪失敗，readiness 回 503。readiness 改變時記一行日誌（開好 INFO；失敗或發現斷線 WARN 附錯誤），不含密鑰。
- **保溫的負擔**：每個副本每輪對每條閒置連線做一次 `SELECT pg_backend_pid()`，sqlx 還連線時再 ping 一次（一來一回，不執行查詢）。連線池是常駐的 3 條時，每個副本每秒約 1.5 個查詢加 1.5 次 ping（原本每輪各 1 次），連線池因尖峰多開的連線也每輪各一次，直到 30 分鐘的壽命到期。這些都是不讀表的查詢，對資料庫可以忽略。保溫工作不多開連線：同時最多拿一條，只在連線池不到 3 條時才開新的。
- **開連線不受檢查期限限制**：取得連線（含開新連線）的逾時是 5 秒（`CONNECT_TIMEOUT`）。每次查詢在自己的 task 裡跑：檢查到 0.4 秒就回 503，但 task 不被取消，開到一半的連線會開完，留給下一個檢查。同時在跑的查詢每條連線最多 4 個（`IN_FLIGHT_PER_CONNECTION`，預設 10 條連線就是 40 個），超過就直接回 503；每個查詢與每輪保溫最多 10 秒（`WORK_LIMIT`），資料庫卡住時 task 與連線不會一直被佔住。
- **連線的壽命**：沿用 sqlx 的預設：存活超過 30 分鐘的連線在歸還時關閉，閒置超過 10 分鐘的由連線池的定期清理關閉；替補的連線重新經過 service-kit 的 session 檢查，由連線池的 `min_connections`（3）在背景開，不在檢查裡開。不把壽命設成無限，是為了讓資料庫角色或 policy 改壞時，最多 30 分鐘內就會被 session 檢查擋下。

剩下的情況：資料庫換主或斷線時舊連線全部失效。從斷線到下一輪保溫（最多 2 秒），readiness 還是 200，這段期間遇到死連線的檢查要重開連線，新連線超過 0.4 秒時回 503（不帶身分標頭）。保溫工作一發現斷線就讓 readiness 回 503，3 條新連線開好、確認過才回 200（service-kit 會沿用 readiness 的結果最多 1 秒，所以 `/readyz` 看到的改變最多晚 1 秒）。檢查放棄後還在跑的查詢會跑到自己的連線開好、查完為止，換主時湧入的檢查因此會讓連線池暫時多開幾條；這些查詢跑完之前，新的檢查仍可能要等連線。

`tests/ext_authz.rs` 以每條新連線延遲 1 秒的代理重現（Laysi/trustiot-v2#341）：

- `a_slow_database_connect_never_fails_a_check_after_ready`：就緒後的檢查全部 200、沒有開新連線。切斷所有連線並讓代理暫停放行新連線：檢查回 503 且不帶身分標頭，不再送檢查，readiness 也自己轉為 503。代理恢復後 readiness 回到 200，之後的檢查全部 200、沒有開新連線。
- `a_keep_warm_round_replaces_every_dropped_connection`：切斷所有連線後不送任何檢查，保溫工作自己發現斷線（WARN），換掉全部 3 條（代理多接到至少 3 條連線），再記 INFO、readiness 回 200；之後的檢查全部 200、沒有開新連線。原本一輪只 ping 一條連線的作法換掉一條就繼續回報就緒，readiness 不會失敗，通不過這個測試。

## 簽章金鑰

- 私鑰是 PKCS#8 PEM 檔（`openssl genpkey -algorithm ed25519 -out signing-key.pem`），放在 Kubernetes Secret，chart 的 `internalIdentity.signingKeySecret` 以唯讀 volume 只掛進身分服務，路徑以 `TRUSTIOT_IDENTITY_SIGNING_KEY_FILE` 傳入。私鑰不放進 values、映像或 repo。
- 公鑰由 chart 的 `internalIdentity.publicKeys` 組成 JWK Set，放進平台設定 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`，每個服務以它驗證（見 [chart README](../../deploy/chart/README.md) 的「內部身分」）。
- `kid` 不另外設定：啟動時用私鑰對 `TRUSTIOT_INTERNAL_IDENTITY_KEYS` 的每個 `kid` 簽一個測試 token，再以 service-kit 的驗證器驗證，驗得過的那個就是自己的 `kid`。沒有任何一個驗得過（公鑰沒部署），或有兩個以上（同一把公鑰列了兩次），服務就拒絕啟動，所以身分服務不會簽出服務不接受的 token。啟動時以 INFO 記下使用的 `kid`。
- 私鑰只在啟動時讀一次，檔案超過 16 KiB、不是 Ed25519 PKCS#8 PEM 都拒絕啟動；錯誤訊息與 `Debug` 輸出不含檔案內容，讀進來的文字用後清零。

產生公鑰的 `x`（`publicKeys[].x`，32 bytes 公鑰的 base64url）：

```sh
openssl pkey -in signing-key.pem -pubout -outform DER | tail -c 32 | basenc --base64url | tr -d '='
```

### 金鑰輪替

步驟與理由在 [`crates/service-kit` 的「公鑰與輪替」](../../crates/service-kit/README.md)；對身分服務來說是：

1. **加新公鑰**：產生新的金鑰對，把新公鑰以新的 `kid`（例如 `2027-04-a`）加進 `internalIdentity.publicKeys`（新舊並存）並部署，等每個服務都滾動完成。
2. **換簽章私鑰**：把 Secret 的內容換成新私鑰，然後 `kubectl rollout restart deployment/<release>-identity`（Secret 改變不會自動重啟 pod）。新的 pod 自己找出新 `kid`；步驟 1 沒做完就換，新 pod 拒絕啟動，舊 pod 繼續服務。
3. **移除舊公鑰**：身分服務滾動完成後至少等 305 秒（驗證端接受的最長效期 300 秒加 5 秒時鐘誤差），再從 `publicKeys` 移除舊公鑰並部署，最後刪除舊私鑰。

私鑰外洩時三步同時做，不等舊 token 過期。SaaS（SOPS、每步一個 PR、以 `trustiot.io/secret-rotated` annotation 重啟）與地端的實際操作見 [密鑰 runbook](../../docs/runbooks/secrets.md) 的「內部身分簽章金鑰」。

## 設定

設定由 `service_kit::config` 載入，`TRUSTIOT_` 後接英數字的平台設定只由部署決定（kit README 的「設定」）。

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS` | 平台設定：每個服務共用的公鑰 JWK Set；簽章私鑰的公鑰必須在裡面 |
| `TRUSTIOT_IDENTITY_SIGNING_KEY_FILE` | 只給身分服務：掛載的私鑰路徑 |
| `TRUSTIOT_INGRESS_ROUTES` | 只給身分服務：cookie 路由，決定受眾 |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 平台設定：Postgres 位置；只接受 `verify-full`，`_CA_FILE` 必填，是唯一信任的根憑證 |
| `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | 服務角色 `identity_app` 的帳密：chart 以 `identity_app` 設定帳號，密碼來自 Secret `<release>-identity-password` 的 `identity_app` key；migration Job 產生這個密碼並以它設定角色 |
| `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 每個副本的連線數上限，3–100（至少放得下常駐的 3 條），預設 10 |

啟動後由保溫工作在背景開連線；readiness（健康埠與 API 埠的 `/readyz`）在常駐的 3 條連線開好之後才通過（見[連線池](#連線池)），資料庫連不上時 ext_authz 回 503、readiness 回 503。健康埠（預設 8081）只有 `/healthz` 與 `/readyz`，ext_authz 端點只在 API 埠（8080）。設定、金鑰或路由不能用時以 exit code 1 結束，不開始服務。

## 映像

`deploy/images/identity/image.toml` 宣告這個服務的映像（2026-10-11，Laysi/trustiot-v2#308）：release.yml 把 `identity` 交叉編譯成 `aarch64-unknown-linux-musl` 的靜態執行檔，放進共用的 `deploy/images/rust-binary.Containerfile`（distroless static、UID 65532、`ENTRYPOINT` 是 `/usr/local/bin/identity`），發佈為 `ghcr.io/laysi/trustiot-v2/identity:<版本>`，只有 linux/arm64（[deploy/images/README.md](../../deploy/images/README.md)）。

- 映像裡沒有 shell、私鑰與設定：私鑰由 chart 掛載，其餘都是環境變數（見上方[設定](#設定)）。
- 啟動不需要資料庫：設定、金鑰與路由可用時就開始服務，健康埠的 `/healthz` 回 200，`/readyz` 等連線池開好才回 200。2026-10-11 在 arm64（lab Mac M1 的 podman）以發佈流程建出的映像、當場產生的金鑰與連不到的資料庫驗證過：`/healthz` 200、`/readyz` 503、沒有 cookie 的 ext_authz 檢查 401（[測試報告](../../docs/test-reports/2026-10-11-308-identity-image.md)）。
- nightly 的 k3d 以 `scripts/nightly/service-image.ts` 的 `loadServiceImage(platform, "identity")` 建出同樣的映像並匯入節點（輸入沒變時從快取載入）；smoke 以它部署身分服務，當作入口的 ext_authz，並經入口建立租戶（Laysi/trustiot-v2#65，`scripts/nightly/api.ts`）。

本機試建：`bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --output plan.json`，再 `bun scripts/release/build.ts --plan plan.json --out <目錄>`（需要 podman、skopeo 與 `proto install` 裝好的工具，見 [scripts/README.md](../../scripts/README.md)）。

## 本機啟動

本機開發環境（`bun scripts/dev.ts`，[scripts/README.md](../../scripts/README.md) 的「身分服務」）備好了身分服務要的東西（Laysi/trustiot-v2#336）：

- `up`：以 migration Job（`tools/migrate`，不改程式）建立 `identity_owner`、`identity_app` 與 `identity` schema 並套用 `migrations/postgres/`（Job 套用整個產品，`identity_platform` 與資產服務的角色也會建立，沒綁密碼的維持 `NOLOGIN`）；產生 Ed25519 簽章私鑰（`<狀態目錄>/identity/signing-key.pem`，0600，沿用已有的）。
- `seed`：建立開發使用者、`admin` 成員資格與一筆 session（寫法照下一節），印出 cookie。
- `env`：印出 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`（公鑰）、`TRUSTIOT_IDENTITY_SIGNING_KEY_FILE`（路徑）、`TRUSTIOT_INGRESS_ROUTES`（`localhost` 的每個路徑給 `assets`）與 `TRUSTIOT_DEV_IDENTITY_DATABASE_{USERNAME,PASSWORD}`。

```sh
bun scripts/dev.ts up && bun scripts/dev.ts seed
set -a; eval "$(bun scripts/dev.ts env)"; set +a
TRUSTIOT__DATABASE__USERNAME="$TRUSTIOT_DEV_IDENTITY_DATABASE_USERNAME" \
TRUSTIOT__DATABASE__PASSWORD="$TRUSTIOT_DEV_IDENTITY_DATABASE_PASSWORD" \
  cargo run -p identity
curl -i -H 'Host: localhost' -H 'Cookie: __Host-trustiot-session=<seed 印出的值>' \
  http://127.0.0.1:8080/internal/ext-authz/api/anything
```

`/readyz`（健康埠 8081）回 200 之後，有效的 cookie 回 200 與一個 `x-internal-identity`，無效的 cookie 回 401，不在路由表的主機回 403。本機沒有入口 gateway，所以呼叫的人自己帶 `Host` 與 `Cookie`。`TRUSTIOT_DEV_IDENTITY_DATABASE_PASSWORD` 是 Job 為 `identity_app` 產生的密碼，每次 `up` 重新產生。

## 測試工具怎麼建立 session

MS0 沒有登入，測試以 migration 角色直接寫表（`tests/ext_authz.rs` 的 `add_session`；nightly 的 smoke 以資料庫的 superuser 做同樣的事，`scripts/nightly/api.ts`）：

1. 產生 32 bytes 的隨機數，base64url（不補 `=`）就是 cookie `__Host-trustiot-session` 的值。
2. 寫入 `identity.sessions`：`token_hash` 是那 32 bytes（不是 base64 文字）的 SHA-256，另填 `user_id`、`active_tenant_id`、`expires_at`。

服務角色沒有寫入權限，所以這只能由擁有者角色做；正式的建立流程隨登入在 Laysi/trustiot-v2#103。

## 測試

在 repo 根目錄執行：

```sh
CARGO_BUILD_JOBS=2 cargo test -p identity
```

`tests/ext_authz.rs` 需要 podman 或 docker：起一個帶 TLS 的暫時 Postgres（與 `crates/service-kit` 的測試共用同一份支援程式），每個測試建自己的資料庫、套用 migration、以 `identity_app` 啟動真的 binary。`tests/tenants.rs` 與 `tests/cross_tenant.rs`（[crates/testkit](../../crates/testkit/README.md) 的跨租戶負向測試）用 testkit 的 TLS Postgres，schema 由 migration Job 的程式套用（`testkit::product`）。沒有容器時都略過，設了 `CI` 時改為失敗。

| 測試 | 驗證 |
| --- | --- |
| `src/session.rs` | cookie 的解析（混在其他 cookie 與多個 `Cookie` 標頭裡、缺少、重複、非標準寫法），`Debug` 不顯示 secret 與雜湊 |
| `src/issuer.rs` | token 的標頭與 claims 剛好是約定的格式、只有受眾驗得過、過期後被拒；`kid` 的判斷；公鑰沒部署時拒絕；錯誤與 `Debug` 不含私鑰 |
| `src/audience.rs` | `Exact` 優先與最長 `PathPrefix`、主機比對；不能用的路由設定拒絕啟動且不引用設定值 |
| `src/ext_authz/tests.rs` | 以假的 session store 驗證回應表的每一列、偽造的身分標頭不影響 200、store 失敗或卡住時 0.4 秒內回 503、各種方法與路徑形狀、只服務這個端點 |
| `tests/ext_authz.rs` | 真的服務與 Postgres：有效 session 拿到一個通過 service-kit 驗證的 token，偽造的同名標頭不會出現在回應；401 的各種情況（含過期、撤銷、使用者刪除）；改角色與移除成員在下一個請求生效；服務角色不能寫 session、RLS 擋住其他租戶的成員資料；私鑰、session secret 與 token 不出現在日誌與回應；健康埠只有 `/healthz`、`/readyz`；資料庫連不上時回 503；開新連線比檢查期限慢時，就緒後的檢查不失敗、斷線後自動恢復；金鑰與設定不能用時拒絕啟動 |
| `tests/adversarial.rs` | 驗證者另寫的攻擊測試（真的服務與 Postgres）：cookie 解析的各種變形、以資料庫時鐘判斷到期與撤銷、資料庫被鎖、連線被殺、帳密錯誤時回 503 而不放行、成員資料的租戶隔離、各種日誌等級都不含密鑰、金鑰載入、健康埠、跨重啟的金鑰輪替 |
| `tests/token_claims.rs` | 驗證者另寫：竄改、改簽、改受眾、改時間的 token 都過不了 service-kit 的驗證器，輪替期間舊 token 在效期內仍有效（不需要資料庫） |
| `src/tenants.rs` | 租戶名稱的規則（去空白、長度以字元計、控制字元、格式字元與行／段落分隔符）；只接受 JSON 的 `Content-Type`（可帶參數，表單、`text/plain`、相近的型別與兩個 `Content-Type` 都拒絕） |
| `tests/tenants.rs` | 真的服務與 migration Job 套用的產品 schema：平台管理者的 session 經這個服務的 ext_authz 換成 token 後建立租戶得到 201，列與 `identity.tenant_created` 在同一個交易（同一個 `xmin`）；撤銷寫 outbox 的權限時回 503 且沒有留下列；不是平台管理者 403（租戶的 `admin` 也是）、沒有 token 401、平台管理者給資產服務的 token 401、沒有 `Content-Type` 或是表單與 `text/plain` 400、壞的 body 400、壞的名稱 422，都不建立租戶；`application/json; charset=utf-8` 201；日誌不含租戶名稱 |
| `tests/cross_tenant.rs` | 產品 schema（`testkit::product`）上，服務角色讀不到也改不了其他租戶的 `memberships`、`tenants`（每個租戶一列）與 `outbox_event`（讀、無租戶讀、更新、搬移、插入五種探測），服務角色碰得到的每個關聯都已宣告：`users`、`sessions` 不是租戶資料、`outbox_aggregate` 是 outbox 的記錄而豁免；建立租戶的 API 不讀租戶資料而豁免 |
