# 使用者偏好、目前的 session 與切換租戶

對應 issue：Laysi/trustiot-v2#497。

## 環境

- 開發工作站（arm64），Rust 1.99.0（`rust-toolchain.toml`），Bun 1.4.2（`.prototools`）。容器測試以 Docker 起暫時的 TLS Postgres（`testkit::product`：migration Job 套用整個產品的 schema，含 `0005_preferences_and_tenant_switch.sql`）。
- 每個容器測試都在 `CI=1` 下執行：沒有容器執行環境時失敗而不是略過，所以下面的「通過」都是真的跑了 Postgres。
- 分支 `feat/497-preferences-tenant-switch`，基於 `c4d045a`，之後合併了 `origin/main` 的 `34f777e`；測試的是本報告所在 commit 的內容。整個 workspace 的測試（`workspace-summary.txt`）是第一個 commit（安全審查之前）跑的，審查後的修正只動身分服務與 `crates/service-kit` 的文件註解，重跑的是下面「安全審查後的修正」列的檢查。

## 指令

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
CI=1 cargo test -p identity --lib -- preferences:: current_session:: tests::
CI=1 cargo test -p identity --test account
CI=1 cargo test -p identity --test cross_tenant -- --nocapture
CI=1 cargo test -p identity
CI=1 cargo test --workspace --no-fail-fast
cargo run -p openapi -- --check
cargo deny --locked check
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
```

## 結果

原始輸出：[跨租戶負向測試的每個探測](data/2026-10-23-497-preferences-tenant-switch/cross-tenant.txt)、[tests/account.rs 與身分服務的單元測試](data/2026-10-23-497-preferences-tenant-switch/account.txt)、[身分服務全部的測試摘要](data/2026-10-23-497-preferences-tenant-switch/identity-summary.txt)、[安全審查的突變測試](data/2026-10-23-497-preferences-tenant-switch/mutations.txt)、[整個 workspace 的測試摘要](data/2026-10-23-497-preferences-tenant-switch/workspace-summary.txt)（第一個 commit）。

| 指令 | 結果 |
| --- | --- |
| `cargo test -p identity --lib -- preferences:: current_session:: tests::` | 身分服務的 108 個單元測試都通過，與本 issue 有關的 13 個：允許清單（大小寫、`Factory`、路徑形狀）、tzdb 每個名稱都通過 migration 的 CHECK（598 個名稱，`Factory` 以外都接受）、預設值與清單和 migration 一致、`PATCH` 與切換的 body（`null`、未知欄位、陣列、重複欄位被拒）、每個不合的欄位各一個錯誤、回應的欄位、API 文件把 session 與租戶的每個欄位列為 `required`、讀成員資格的上限加消費端不超過共用的平台角色連線池、README 寫的每個副本連線數與連線池表和程式的常數相同 |
| `cargo test -p identity --test account` | 4 個通過，連跑 3 次都通過（見下） |
| `cargo test -p identity --test cross_tenant -- --nocapture` | 86 個探測、0 個失敗；`GET /api/identity/v1/sessions/current` 以租戶 A、B 的 token 各自只顯示自己租戶的名稱（正向對照），沒有租戶時什麼都不顯示；`identity.preferences` 與三個新端點的豁免有理由 |
| `cargo test -p identity` | 單元測試 108 個與 10 個整合測試檔全部通過（`account` 4、`adversarial` 11、`cross_tenant` 1、`event_log` 1、`ext_authz` 14、`login` 12、`migrations` 1、`tenants` 1、`token_claims` 8）。同一天的另一次完整執行裡 `tests/login.rs` 的 `there_is_no_password_login_sign_up_or_fake_login` 失敗一次（探測不存在的路徑時連線被重置：服務沒讀 body 就回 404 並關閉連線），單獨重跑 6 次與再一次完整執行都通過；這個探測迴圈在 `origin/main` 上就是這樣寫，本變更沒有動它 |
| `cargo test --workspace --no-fail-fast` | 155 組測試（各 crate 的單元、整合與 doc-test）通過（含 `tools/migrate` 的 `fresh_database`：identity schema 的表從 14 張變 15 張，`repo-check`、`sql-check` 與 openapi 檢查），4 個失敗，都和本變更無關：在同一台工作站上對 `origin/main`（`c4d045a`）未修改的內容以同樣的指令也以同樣的方式失敗（見[摘要](data/2026-10-23-497-preferences-tenant-switch/workspace-summary.txt)的最後一段）。失敗的是 `ai-clients` 的 `an_unanswered_connection_attempt_is_a_connect_timeout`（接受佇列在這個作業系統上沒有填滿）、`service-kit` 的兩個 PID 1 容器測試（容器沒有發佈 8080 埠）與 `tools/migrate` 的 `mqtt_broker::gateways_need_a_current_certificate_of_our_ca`（broker 重新載入時讀不到 CRL 與 CA 檔）；本變更沒有碰這三個 crate 的程式與 Mosquitto 的設定，它們在 CI 的 Linux runner 上執行 |
| `cargo clippy --workspace --all-targets -- -D warnings`、`cargo fmt --all --check` | 通過 |
| `cargo run -p openapi -- --check` | 通過（`api/openapi.json` 已重新產生） |
| `cargo deny --locked check` | `advisories ok, bans ok, licenses ok, sources ok`（新增的 `jiff-tzdb` 0.1.9 是 `Unlicense OR MIT`、沒有相依） |
| 前端的 7 個指令 | 全部通過；`test` 115 pass、0 fail |

`tests/account.rs` 的四個測試：

| 測試 | 驗證的內容 |
| --- | --- |
| `a_session_switches_only_to_the_users_own_tenants` | 目前的 session 顯示使用者、email、租戶 A 與 `editor`、依名稱排序的兩個所屬租戶，回應不含別人的 ID、email 與非所屬租戶的 ID、名稱；切到 B：回應、資料庫的 `active_tenant_id`、下一個請求給資產服務的 token（`tenant` 是 B 與 `viewer`）與再讀一次的 session 都是 B；B 的角色改成 `admin` 時下一個 token 立即是 `admin`；同一使用者的另一個 session 與別人的 session 不變；切到存在但不屬於自己的 C 與不存在的租戶都是 `{"code": "permission_denied", "message_key": "session.tenant_not_member"}`，回應不含 C 的名稱，session 仍是 B；token 與 cookie 不是同一個 session、別人的 cookie 配自己的 token、別人的 token 配自己的 cookie、沒有 cookie 共 5 種組合都改不到任何 session；cookie 檢查之後才過期（資料庫的時鐘，沒有撤銷）的另一個 session 切到所屬的 B 回 401，仍在 A；切回 A 是 `editor`；切到 B 之後移除 B 的成員資格，下一個 token 沒有 `tenant`，session 的 `active_tenant` 是 `null`、`tenants` 只剩 A，再切 B 回 403；cookie 檢查之後才被撤銷的 session 回 401 且不變；日誌有成功與拒絕的原因，沒有租戶名稱、email 與 cookie |
| `account_writes_need_the_users_own_same_origin_json_request` | 切換與修改偏好兩個寫入：cookie 檢查對 `Sec-Fetch-Site: cross-site`、`same-site` 與別的 `Origin` 回 403 `security.cross_site_request` 且不簽 token；帶著有效 token 直接送到處理常式，跨站（含 `Sec-Fetch-Site: none` 與重複的標頭）也是同樣的 403；沒有或不是 JSON 的 `Content-Type`（含表單、`multipart`、`text/plain`）400；大小上限以補了空白的合法 body 測：剛好 1024 bytes 回 200（切到原本的租戶、`{}`），1025 bytes 與多 1100 bytes 的 400（或連線在讀完之前被關掉），每次之後 session 與偏好都沒變；別的服務換來的 token（`act`）不論有沒有 cookie 都 403；沒有 token 或給資產服務的 token 401；沒有 `sid` 的 token 不能切換（401）；以上都沒改到 session 與偏好；換來的 token 讀 session 與偏好也是 403（有沒有 cookie 都一樣），回應沒有租戶名稱與 email，使用者自己的請求讀得到；同源的 JSON 請求（`Origin` 相符或 `Sec-Fetch-Site: same-origin`，`application/json; charset=utf-8`）成功 |
| `users_change_only_their_own_preferences_within_the_allow_lists` | 預設值（沒有存任何列）、`Cache-Control: no-store`；另一個使用者（Bob）先經 API 存了自己的偏好；Alice 改兩個欄位後回應、資料庫與再讀一次一致，之後 Alice 的每次修改後 Bob 的列與他讀到的都不變；沒給的欄位維持原值、`{}` 不改；16 種不在允許清單的值（`fr`、`zh-tw`、`ZH-TW`、`zh-Hant`、空字串、`ja`、`Auto`、`Mars/Olympus_Mons`、`asia/taipei`、`Factory`、`../../etc/passwd`、`Asia/Taipei `、`+08:00`、過長的名稱、`blue`、`Dark`）都是 422 並指出欄位，同一個請求裡合法的欄位也不套用；`null`、數字、陣列、未知欄位、想指定別的使用者、陣列與字串的 body 都 400；使用者在 cookie 檢查之後被刪除時 401 且沒有留下列；日誌沒有偏好的值與 email |
| `the_roles_get_only_the_rights_these_apis_use` | `identity_app` 能改 `sessions.active_tenant_id`、新增只帶 `user_id` 的偏好列、改四個偏好欄位；不能改 session 的使用者與 `token_hash`、改租戶名稱、新增時指定偏好值、改偏好列的使用者、刪除或 `TRUNCATE` 偏好（42501）；CHECK 擋下 `fr`、`zh-tw`、`blue`、空的、路徑形狀的、帶空白的、`Asia//Taipei` 與 65 bytes 的時區（23514）；沒有租戶時仍看不到成員資格與租戶（0 列）。`identity_platform` 看得到兩個租戶的成員資格與名稱（跨租戶），但新增、修改、刪除成員資格與租戶，讀或改 session 與偏好都是 42501 |

## 安全審查後的修正

2026-10-23 的獨立安全審查沒有 Critical、High、Medium，提出的 Low 與 Info 在同一個分支修正。每一項都有回歸測試，並把修正拿掉（突變）確認測試會失敗：每個突變套用後以 `CI=1` 跑那個測試，跑完還原檔案（還原前後的 SHA-1 相同），[輸出](data/2026-10-23-497-preferences-tenant-switch/mutations.txt)。

| 項目 | 修正 | 回歸測試 | 突變與結果 |
| --- | --- | --- | --- |
| F1 大小上限沒有被證明 | 測試改用補了空白的合法 body | `account_writes_need_the_users_own_same_origin_json_request`：剛好 1024 bytes 200，1025 bytes 與多 1100 bytes 400，session 與偏好不變 | 兩個 `MAX_BODY_BYTES` 改成 `1 << 20`：失敗（1025 bytes 回 200） |
| F2 PATCH 漏掉使用者條件抓不到 | 測試裡 Bob 先存一列，Alice 每次修改後比對；程式另確認 `UPDATE` 剛好改到一列，否則不提交、回 503 | `users_change_only_their_own_preferences_within_the_allow_lists` | `WHERE` 換成 `$1::uuid IS NOT NULL`：失敗（503，一列的檢查擋下）；同時拿掉一列的檢查：失敗（Bob 讀到 Alice 的值） |
| F3 每個副本 34 條連線 | 使用者自己的帳號不另開連線池：服務角色用租戶 API 的，讀成員資格用事件紀錄的平台角色連線池、同時最多 2 個；每個副本回到 30 條（26 + 4） | `tests::the_readme_states_the_connections_per_replica`（從常數算出的數字與 README 的句子、連線池表每一列相同，不超過 30）；`membership_reads_leave_every_consumer_a_connection` | 共用的服務角色連線池改成 3 條：失敗；共用的平台角色連線池改成 `CONSUMES` 加 3：失敗 |
| F5 平台角色進了使用者請求的路徑 | 決策 29 與 service-kit 的 README、`PlatformReader` 的說明寫明條件（查詢條件只來自已驗證的 token）；讀成員資格的交易另記 DEBUG，帶 `user_id` | 文件 | 不適用 |
| F6 換來的 token 讀得到 session 與 email | `GET` 兩個端點也拒絕帶 `act` 的 token（403） | `account_writes_need_the_users_own_same_origin_json_request` 第 7 步 | 拿掉 session 的判斷：失敗；拿掉偏好的判斷：失敗 |
| F7 一定出現的欄位不在 `required` | `email`、`active_tenant`、租戶的 `name` 標成 `required`；`GET` 的 503 說明補上平台角色沒有帳密與讀成員資格等太久；重新產生 `api/openapi.json` 與 `frontend/src/api/schema.gen.ts` | `every_field_of_the_session_is_required_in_the_api_document` | 拿掉三個標註：失敗 |
| F8 `expires_at > now()` 沒有測試等 | 加上過期 session 的測試；跨租戶測試的豁免理由與 `src/login.rs` 的註解改成現況 | `a_session_switches_only_to_the_users_own_tenants` 第 4b 步 | 拿掉 `SWITCH_TENANT` 的 `expires_at > now()`：失敗（回 200） |

F4（已登入的使用者大量請求時，同一副本的這組 API 排隊後回 503）屬於限流（Laysi/trustiot-v2#113），不在這裡修。F3 的作法讓它的影響多了兩處：大量讀取目前 session 或偏好的請求可能讓平台管理者建立租戶等連線、逾時回 503，後台讀事件紀錄也和讀成員資格共用平台角色連線池的那 2 條；事件紀錄的消費端不受影響（讀成員資格同時最多 2 個）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 已登入的使用者可讀取目前的 session（使用者、目前的租戶與角色）；切換租戶後，下一次讀到的是新租戶與該租戶的角色 | 通過：`a_session_switches_only_to_the_users_own_tenants` 第 1、2 步 |
| 使用者可讀取與修改自己的偏好；不在允許清單內的語言、時區與主題被拒 | 通過：`users_change_only_their_own_preferences_within_the_allow_lists`，資料庫的 CHECK 另由 `the_roles_get_only_the_rights_these_apis_use` 與單元測試涵蓋 |
| 切換到自己所屬的租戶後，同一 session 的下一個請求以新租戶與使用者在該租戶的角色執行；切到不屬於自己的租戶被拒，session 維持原本的租戶 | 通過：`a_session_switches_only_to_the_users_own_tenants` 第 2 到 7 步（下一個請求的 token、別的 session 不變、非所屬與不存在的租戶同樣被拒、成員資格移除後不在任何租戶） |
| 跨租戶負向測試涵蓋新端點（crates/testkit） | 通過：`tests/cross_tenant.rs` 宣告 `GET /api/identity/v1/sessions/current`（`Endpoint::list`，對 `identity.tenants`，正向對照與兩個租戶互相探測都通過），切換與偏好的三個端點以理由豁免（偏好不是租戶資料；切換只回呼叫者自己所屬的租戶，與 `GET` 相同，只動呼叫者自己的 session，探測的 token 指不到它，由 `tests/account.rs` 涵蓋），`identity.preferences` 以理由豁免 |
