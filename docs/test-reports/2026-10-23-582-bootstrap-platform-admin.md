# 第一位平台管理員：`migrate bootstrap-platform-admin`

Laysi/trustiot-v2#582

## 環境

- 開發者工作站（x86_64 Linux），podman 6.1.4（測試的 TLS Postgres 容器），Rust 1.99.0（`rust-toolchain.toml`），Bun 1.4.2，Helm v4.3.0（`.prototools` 的版本）。
- 分支 `feat/582-bootstrap-platform-admin`。第一輪在 origin/main 7ae93229 上；最後一輪 rebase 到 origin/main 7371f15b（含 Laysi/trustiot-v2#587 的 ctutils 0.4.4 與 Laysi/trustiot-v2#572 的出廠登錄）。
- 沒有 Kubernetes：chart 只以 `helm template` 檢查；叢集上的流程由 nightly 在帶 `smoke` 標籤的 PR 上執行（見「結論」）。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy -p contract -p identity -p migrate --all-targets -- -D warnings
cargo run -p openapi -- --check
CI=1 cargo test -p contract
CI=1 cargo test -p migrate --no-fail-fast
CI=1 cargo test -p identity --no-fail-fast
CI=1 cargo test -p sql-check
CI=1 cargo test -p repo-check
CI=1 cargo test -p testkit
cargo deny --locked check
bun install --cwd scripts --frozen-lockfile
bun test scripts
bun run --cwd scripts typecheck
bun test deploy/chart
```

`CI=1` 讓需要容器的測試在沒有容器時失敗而不是略過。

## 結果

兩次執行的輸出摘錄（只有指令、測試名稱與結果行）：

- [scoped-checks.txt](data/2026-10-23-582-bootstrap-platform-admin/scoped-checks.txt)：第一輪（7ae93229 上）。`cargo fmt` 有格式差異（已以 `cargo fmt --all` 修正）；`tests/fresh_database.rs` 預期 identity schema 有 14 張表，新增的 `platform_admin_bootstrap` 讓它變成 15（已更新測試）；`cargo deny` 只回報 RUSTSEC-2026-0385（ctutils 0.4.3，main 由 Laysi/trustiot-v2#587 修正）。其他都通過：`bun test scripts` 1608 個、`bun test deploy/chart` 627 個，contract、identity、sql-check、repo-check、testkit 全部通過，`openapi --check` 通過。
- [after-rebase.txt](data/2026-10-23-582-bootstrap-platform-admin/after-rebase.txt)：修正並 rebase 到 7371f15b 之後，上面「指令」的每一項再跑一次，全部通過：`cargo fmt --check`、clippy、`openapi --check`；contract、migrate（每個測試檔，含 `tests/platform_admin.rs` 與 `tests/fresh_database.rs`）、identity（含 `tests/platform_admin.rs`）、sql-check、repo-check、testkit；`cargo deny --locked check`（advisories ok）；`bun test scripts` 1608 個、`bun run --cwd scripts typecheck`、`bun test deploy/chart` 627 個。

| 測試 | 內容 | 結果 |
| --- | --- | --- |
| `tools/migrate/tests/platform_admin.rs` `the_first_platform_admin_is_created_once` | 真的 binary：migration 之前以 1 結束、什麼都沒建；不是地址、空行、含空白的地址、帶參數都以 2 結束、什麼都沒改；以 superuser 鎖住標記表、確認兩次執行都在等鎖後才放開，結束碼是 0 與 3，3 的那次寫出 0 的使用者；只有一位平台管理員、一筆 `identity.platform_admin_granted`（payload 只有 `user_id` 與 `via`，沒有租戶、沒有 `@`）；之後的執行、以及取消所有平台管理員之後的執行都是 3、什麼都沒改；照 runbook 刪掉標記後，大小寫與空白不同的地址讓已有的使用者成為平台管理員（ID 不變，第二筆事件），下一次又是 3；每次的 stdout、stderr 都沒有地址 | 通過 |
| `services/identity/tests/platform_admin.rs` `the_first_platform_admin_logs_in_and_creates_a_tenant` | 真的身分服務、產品 schema 與假 SMTP relay：bootstrap 建立的使用者，`platform_event_log` 剛好一筆 grant、沒有成員資格；以驗證碼登入、經 ext_authz 拿到 token，`POST /api/identity/v1/tenants` 201；再執行回報同一位、什麼都沒改；服務日誌沒有地址 | 通過 |
| `crates/contract/tests/events.rs` `platform_admin_granted_decodes_every_version` | 事件的 v1 fixture 與不認得的 `via` | 通過 |
| `tools/migrate/src/platform_admin.rs` 單元測試 | 只讀第一行（CRLF、沒有換行）、拒絕的輸入不引用內容、資料庫錯誤只寫步驟與 SQLSTATE、owner 角色與服務清單一致 | 通過 |
| `deploy/chart/tests/chart.test.ts` 「first platform administrator」 | 兩種型態：暫停的 CronJob、一次嘗試、stdin 一次、migration Job 的映像、ServiceAccount（沒有 token）、安全設定與 CA；環境變數只有 migration 角色與等待；每個 NetworkPolicy 與 AuthorizationPolicy 對它與 migration Job 的判斷相同；只在 migration Job 與身分服務都啟用時渲染；名稱最多 52 字元；保留名稱 | 通過 |
| `scripts/nightly/api.test.ts`、`platform-admin.test.ts` | nightly 先以 CronJob 建立管理員（0）、再跑一次（3、同一位），再等操作紀錄剛好一筆；結束碼與訊息和 Rust 原始碼一致；兩種型態渲染出的 CronJob 通過 nightly 的檢查；失敗時只列出固定訊息 | 通過 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 第一次建立並以 0 結束；之後的執行與同時執行的輸家以專用結束碼（3）結束、資料不變（測試） | 通過：`tools/migrate/tests/platform_admin.rs` |
| `identity.platform_event_log` 剛好一筆 `identity.platform_admin_granted`；payload、stdout、stderr 與日誌都沒有地址（測試） | 通過：上面兩個整合測試 |
| 建立的使用者以 email 驗證碼登入，`POST /api/identity/v1/tenants` 回 201（測試） | 通過：`services/identity/tests/platform_admin.rs` |
| 沒有新的 HTTP 路由：`cargo run -p openapi -- --check` 通過、`api/openapi.json` 不變 | 通過 |
| 帶 `smoke` 標籤的 PR 上，nightly 以 chart 的方式建立 API 檢查的平台管理員，兩個 namespace 都通過 | 待測：要在這個 PR 掛上 `smoke` 標籤後的 nightly 執行確認（工作站不跑 Kubernetes）；nightly 的單元測試與 chart 渲染已通過 |
| runbook 與 `tools/migrate/README.md` 寫明步驟、結束碼與復原 SQL | 完成：`docs/runbooks/platform-admin.md`、`tools/migrate/README.md` |

## 合併 main 之後（2026-10-23）

main 到了 b7da9509，多了 Laysi/trustiot-v2#583 的 `0005_member_roles.sql` 與事件 `identity.member_role_changed`，以及 Laysi/trustiot-v2#517 的 nightly 憑證服務檢查。這個分支以合併 commit 70dbd741 併入 main：

- migration 改名為 `0006_platform_admin_bootstrap.sql`（`git mv`），`src/schema.rs` 先列 main 的 `0005_member_roles.sql` 再列它，身分服務與 migrate 的 README、`tools/migrate` 的註解一起更新。舊名只在這個未合併的 PR 分支上出現過，沒有任何 release 或部署的資料庫記過它。`0005_member_roles.sql` 沒有新增資料表，`tools/migrate/tests/fresh_database.rs` 預期的 identity 表數仍是 15。
- 兩個事件、各自的 fixture 與解碼測試、README 的說明都保留；展示環境 runbook 第 8 步先照第一位平台管理員 runbook 建立平台管理員，再建立租戶、設定成員角色。
- nightly：憑證服務的檢查不再以 SQL 新增平台管理員，改用 API 檢查以 bootstrap 建立的那一位（`runApiCheck` 回傳他的使用者 ID，`smoke.ts` 交給 `checkCertsWaiting`），只為他再建一個 session；API 檢查原有的 0 與 3 兩次執行、操作紀錄的檢查都不變。

在開發者工作站（x86_64 Linux）上於 70dbd741 重跑，全部通過（[after-merge.txt](data/2026-10-23-582-bootstrap-platform-admin/after-merge.txt)）：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo run -p openapi -- --check`；`CI=1 cargo test -p identity -p migrate -p contract -p repo-check -p sql-check --no-fail-fast`（identity 含 `tests/members.rs` 與 `tests/platform_admin.rs`，migrate 含 `tests/platform_admin.rs` 與 `tests/fresh_database.rs`，contract 的 `tests/events.rs` 15 個）；`bun test scripts` 1659 個通過、9 個略過（dev 環境的 podman e2e、aarch64 binary 與測試 pod 映像的測試，這次沒有啟用）、`bun run --cwd scripts typecheck`；`bun test deploy/chart` 627 個；`bun run --cwd frontend api:check` 與 `bun run --cwd frontend test` 131 個。帶 `smoke` 標籤的 nightly 仍待這個 PR 上的執行確認。
