# Email 驗證碼登入與登出的測試報告（2026-10-22）

對應 issue：Laysi/trustiot-v2#103（身分服務：email 驗證碼登入、登出與伺服器端 session）。

檔名沿用開始實作的日期 2026-10-21；以下的檢查都在 2026-10-22 執行。

## 環境

- 建置主機（x86_64 Linux），經檢查佇列執行；`CI=1`，所以需要容器的測試在沒有容器時失敗、不會略過。測試用的 Postgres 由 `crates/testkit` 以 podman 啟動（以 digest 釘選的映像，TLS，`verify-full`）。
- 工具版本依 `.prototools`：Rust 1.99.0、Bun 1.4.2、Helm 4.3.0、kubeconform 0.8.0、kubectl 1.36.5、flux 2.9.6、cargo-deny 0.20.2。
- 分支 `feat/103-email-otp-login`：
  - `28a2b78`：功能（以 `cc185b9` 為基底）。
  - `2fa1e68`：合併 main（`1bda1d9`），沒有衝突。
  - `9715ffe`：修正完整檢查找到的 `tools/migrate` 測試（見「結果」）。
  - 之後只多了本報告的 commit。
- 寄信：測試自己啟動假的 SMTP relay（`services/identity/tests/support/smtp.rs`，STARTTLS 或一開始就是 TLS，憑證由測試產生的 CA 簽發），收件地址只用 `example.com` 與 `example.org`。沒有連外部郵件服務，沒有用 Kubernetes，也沒有動 lab 的機器。
- 工作站只跑輕量的檢查（工作流程政策、密鑰處理、CI 路徑規則）。

## 指令

```sh
# 建置主機（check queue）：
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
cargo run --locked -p openapi -- --check
cargo deny --locked check
bun test deploy/chart
bun test deploy/flux
bun install --cwd scripts --frozen-lockfile
bun test scripts
bun run --cwd scripts typecheck

# 只跑身分服務（含登入的整合測試）：
cargo test --locked -p identity --no-fail-fast

# 工作站：
bun scripts/ci/check-workflows.ts
bun scripts/ci/check-secret-handling.ts
bun test scripts/ci/changes.test.ts
```

## 結果

| 檢查 | 在哪個 commit | 結果 |
| --- | --- | --- |
| `cargo fmt --all --check` | `9715ffe` | 通過 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | `9715ffe` | 通過，沒有警告 |
| `cargo test --workspace --locked --no-fail-fast` | `9715ffe` | 115 個測試執行檔（含 doc test），909 通過、0 失敗、0 忽略 |
| `cargo run --locked -p openapi -- --check` | `9715ffe` | `api/openapi.json is up to date` |
| `cargo deny --locked check` | `2fa1e68` | `advisories ok, bans ok, licenses ok, sources ok`（只有重複版本的警告，不是錯誤） |
| `bun test deploy/chart` | `2fa1e68` | 461 通過、0 失敗 |
| `bun test deploy/flux` | `2fa1e68` | 73 通過、0 失敗（展示叢集的 values 在新的 `smtp.from` 規則下仍被 chart 接受；沒有私有 values 時仍被拒絕） |
| `bun install --cwd scripts --frozen-lockfile`、`bun test scripts` | `2fa1e68` | 1482 通過、4 略過（需要另外環境的測試，例如 `TRUSTIOT_DEV_E2E=1` 才跑的 podman 端對端測試）、0 失敗 |
| `bun run --cwd scripts typecheck` | `2fa1e68` | 通過 |
| `bun scripts/ci/check-workflows.ts`（工作站） | `2fa1e68` | 4 個檔案，無違規 |
| `bun scripts/ci/check-secret-handling.ts`（工作站） | `2fa1e68` | 449 個檔案，無違規 |
| `bun test scripts/ci/changes.test.ts`（工作站） | `2fa1e68` | 255 通過、0 失敗 |

`9715ffe` 只改了一個 Rust 測試檔，上表以 `2fa1e68` 執行的檢查都不讀它。

身分服務的測試（`cargo test --locked -p identity --no-fail-fast`，內容與 `28a2b78` 相同；完整檢查也再跑了一次）：

| 測試執行檔 | 通過 | 失敗 |
| --- | --- | --- |
| 單元測試（`src/`） | 59 | 0 |
| `tests/adversarial.rs` | 11 | 0 |
| `tests/cross_tenant.rs` | 1 | 0 |
| `tests/ext_authz.rs` | 11 | 0 |
| `tests/login.rs` | 11 | 0 |
| `tests/migrations.rs` | 1 | 0 |
| `tests/tenants.rs` | 1 | 0 |
| `tests/token_claims.rs` | 8 | 0 |

檢查與 commit 前的 Codex 審查找到、並在本分支修正的問題：

| 問題 | 修正與證據 |
| --- | --- |
| 測試執行檔裡的測試同時啟動時，`crates/testkit` 的 `product_postgres` 並行授予 LOGIN，Postgres 回 `tuple concurrently updated`，8 個登入測試失敗 | 授權在共用的鎖內完成；之後 `tests/login.rs` 的 11 個測試都通過 |
| serde 的 internally tagged enum 也接受 JSON 陣列當 body | `parse_request` 只讀 JSON 物件；單元測試 `the_request_names_one_step_and_nothing_else` 加上陣列與其他非物件的 body |
| `api/openapi.json` 裡登入 body 的 schema（utoipa 產生的 `allOf` 加 `additionalProperties: false`）拒絕所有合法的 body | schema 改為手寫：`oneOf` 兩個封閉的物件，各自把 `step` 列為屬性；單元測試 `the_documented_body_is_the_one_that_is_read` 以文件的 schema 檢查程式接受與拒絕的 body |
| chart schema 在設了 `smtp.host` 時，只在 `smtp.from` 存在時才檢查它 | `then` 加上 `required: ["from"]`，`if` 也要求 `host` 存在且非空；chart 測試加上「SMTP relay without a sender」（設了 relay、寄件人空白時拒絕渲染） |
| `verify_code` 的兩種拒絕（沒有可用的驗證碼、驗證碼不對）在資料庫做的事不同，先替一個地址要過驗證碼，就能從回應時間看出這個地址有沒有帳號 | 兩種 401 都至少花 `REFUSAL_FLOOR`（0.5 秒，從查驗證碼開始算，交易結束後才等、不佔連線）；`codes_are_used_once_and_die_after_five_attempts` 檢查猜錯、沒有驗證碼的使用者與沒有帳號的地址的 401 都不少於這個時間 |
| `tools/migrate` 的 `a_fresh_database_gets_every_migration_once` 仍預期 identity schema 有 7 張表（`2fa1e68` 的完整檢查 908 通過、1 失敗） | 登入的 migration 加了 `login_codes`，改成 8 張（`9715ffe`）；之後整個 workspace 909 通過、0 失敗 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 重放或過期的驗證碼被拒；同一個驗證碼猜錯達上限即失效，上限寫進文件 | 通過：`a_code_from_the_email_logs_in_once` 以同一個碼再登入一次得到 401。`codes_are_used_once_and_die_after_five_attempts`：猜錯 5 次後答對仍是 401；猜錯 4 次後答對是 200；過期的碼 401；一個使用者的碼對另一個地址無效；新寄的碼作廢舊的；同一個碼 4 個請求同時送出只有一個成功；每次 401 都至少花 0.5 秒。上限（8 位數、10 分鐘、5 次、每個地址 60 秒 1 封、每小時 5 封、每天 20 封）寫在 services/identity/README.md「登入」與 docs/architecture/secrets.md |
| 導回到別的網域被拒 | 通過：`redirects_stay_on_this_site`：`https://evil.example/`、`//evil.example`、`/\evil.example`、`javascript:`、控制字元與 CRLF、超過 2048 位元組等都回 422、不設 cookie，也不計入嘗試次數；本站路徑 `/alerts/42?x=1` 回 200。另有單元測試 `only_paths_on_this_site_are_redirect_targets` |
| 登出所有裝置後，其他 session 的下一個請求就被拒 | 通過：`logging_out_refuses_the_sessions_next_request`：登出本次 session 後它的下一個請求 401，同一使用者的其他 session 不受影響；登出所有裝置後同一使用者每個 session 的下一個請求都 401，其他使用者的 session 不受影響；沒有 cookie 或沒有內部身分時登出回 401；再登入時瀏覽器原來的 session 被撤銷 |
| 正式版本的執行檔不含假登入（編譯期檢查）；本地版本未開設定時，cookie 也開不了假登入 | 通過：任何版本都沒有假登入。`no_build_switch_can_add_a_fake_login` 以 `cargo metadata` 確認身分服務沒有 Cargo feature（含 optional dependency 帶來的）與 build script，`Cargo.toml` 沒有 `[features]`、`optional = true`、`build =`，原始碼除了 `cfg(test)` 沒有依建置設定或建置環境改變的程式（`cfg_attr`、`cfg!`、`debug_assertions`、`option_env!`、`include!`；`env!` 只讀套件版本）。`there_is_no_password_login_sign_up_or_fake_login` 以本機建置的服務確認：假登入用的 cookie 不會登入、ext_authz 也不認得它，沒有建立 session、沒有寄信 |
| 驗證碼不出現在任何網址或日誌 | 通過：`a_code_from_the_email_logs_in_once` 在 TRACE 層級跑完整個流程：驗證碼不在任何回應與日誌裡，收件地址、session cookie 與 relay 的密碼也不在日誌裡；信件標頭沒有驗證碼，內文沒有連結；網址帶 `?code=` 的請求回 400 而且不計次。單元測試 `codes_are_eight_digits_and_never_printed` 與 `the_email_holds_the_code_in_its_body_only`（驗證碼只在信件內文，不在主旨） |
| 角色變更後，同一 session 的下一個請求就套用新角色 | 通過：`a_role_change_applies_to_the_next_request`：改了成員的角色，同一個 session 的下一個請求簽出的內部身分就是新角色；移除成員資格後，同一個 session 的下一個請求不在任何租戶 |
| 身分服務沒有密碼登入與公開註冊端點 | 通過：`there_is_no_password_login_sign_up_or_fake_login`：OpenAPI 恰好列出身分服務的 4 條路徑（登入、兩種登出、建立租戶），探測的密碼登入、註冊與假登入路徑都是 404 或 405，登入請求帶 `password` 等多餘欄位回 400 |

驗收條件都是登入與 session 的安全關卡，全部在本次驗收，沒有延後的項目。

已知而且記錄的限制（不屬於本 issue 的驗收條件）：寄送與嘗試次數以地址計算，知道某個地址的人可以用掉它的額度，讓那個使用者最多一天登入不了，但無法冒充他登入。分開寄送與猜測的額度、依來源限流與警示在 Laysi/trustiot-v2#508，是 SaaS 對外開放（Laysi/trustiot-v2#434）的前提；展示環境以 Cloudflare Access 只放行受邀名單補償（docs/roadmap/milestones.md「延後的項目與展示環境的補償」）。
