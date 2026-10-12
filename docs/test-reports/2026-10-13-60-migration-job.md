# migration Job（2026-10-13）

對應 issue：Laysi/trustiot-v2#60（決策 4、14、29，清單 M8）。原始資料在 [data/2026-10-13-60-migration-job/](data/2026-10-13-60-migration-job/)。

## 環境

- **受測程式碼**：分支 `feat/60-migration-job`（基於 main `be1cfea`）的工作樹，含 `e4595dd` 之後尚未提交的變更（HTTP client 改用 hyper、文件、chart 測試）；測試之後只改了文件。
- **Rust 測試**：共用建置主機（x86_64 Linux，Docker），`CI=1`，所以需要容器的測試在沒有容器執行環境時會失敗而不是略過。Postgres 是 `crates/service-kit` 測試共用的 TLS Postgres（PostgreSQL 18），ClickHouse 是 `clickhouse/clickhouse-server` 26.8（以 digest 釘選，與 `services/ingest` 的測試相同）。Rust 1.99.0、zig 0.16.0，皆由 `.prototools` 鎖定。
- **chart 與腳本測試**：開發用工作站（x86_64 Linux），bun 1.4.2、helm 4.3.0，由 `.prototools` 鎖定。
- **沒有 Kubernetes**：本 repo 規定工作站不跑 Kubernetes；k3d 上的驗證由 nightly 的 smoke 執行（見「結論」）。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo test --locked --workspace --no-fail-fast
cargo test --locked -p migrate -p sql-check
cargo run --locked -q -p openapi -- --check
cargo deny --locked check
CARGO_ZIGBUILD_ZIG_PATH=$(proto bin zig) cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p migrate

bun test deploy/chart
bun test deploy/flux
bun test scripts
bun run --cwd scripts typecheck
bun scripts/ci/check-workflows.ts
bun scripts/ci/lint-workflows.ts
bun scripts/ci/check-secret-handling.ts
```

## 結果

| 項目 | 結果 |
| --- | --- |
| `cargo fmt`、兩次 clippy | 通過，沒有警告 |
| `cargo test --workspace` | 87 個測試執行檔、641 個測試全部通過（[workspace-checks.txt](data/2026-10-13-60-migration-job/workspace-checks.txt)） |
| `cargo test -p migrate -p sql-check` | 全部通過，逐項名稱見 [migrate-and-sql-check-tests.txt](data/2026-10-13-60-migration-job/migrate-and-sql-check-tests.txt) |
| OpenAPI | `api/openapi.json is up to date` |
| `cargo deny` | advisories、bans、licenses、sources 都 ok |
| aarch64 musl 交叉編譯 | `migrate` 以釘選的 zig 建置成功（release profile） |
| `bun test deploy/chart` | 304 通過 |
| `bun test deploy/flux` | 19 通過 |
| `bun test scripts` | 620 通過、1 略過（podman e2e，需 `TRUSTIOT_DEV_E2E=1`），0 失敗 |
| typecheck、workflow 政策、actionlint、secret handling | 通過，沒有違規 |

migrate 的整合測試涵蓋的情境：

- `tests/fresh_database.rs`：必要的角色沒有密碼檔、或不在服務清單裡時，Job 什麼都沒建立就停止；全新資料庫第一次執行套用全部檔案（`database.sql`、identity 的角色、schema、`search_path`、密碼、`0001_identity.sql`）；`identity_app` 在 `pg_roles` 是 `(rolsuper, rolbypassrls, rolcanlogin, rolcreaterole, rolcreatedb, rolinherit) = (f, f, t, f, f, f)`，`identity_owner` 不能登入；`pg_tables` 中 schema `identity` 的 3 張表都由 `identity_owner` 擁有，`identity_app` 不擁有任何表；`identity_app` 沒有 TEMPORARY；migration 角色讀不到 `identity.sessions`（不繼承 owner 的權限）；`identity_app` 以密碼檔登入後通過 service-kit 的 session 檢查；第二次執行 `changes` 為空，catalog 快照完全相同；換密碼檔後只產生 `set the password of identity_app`，舊密碼登不進去、新密碼可以，之後再跑一次也沒有變更；改過的已套用檔案、superuser、沒有 CREATEROLE 的角色、得到 BYPASSRLS、加入 owner 角色或多了其他成員的服務角色，都讓 Job 停止。
- `tests/concurrent_runs.rs`：另一個 session 持有鎖時，執行等到上限就放棄且沒有改任何東西；鎖持有期間開始的執行在鎖釋放後才完成；同時開始的兩次執行都成功，只有一次套用變更。
- `tests/outbox_and_tenant_roles.rs`：producer 與 consumer 的表、policy 與讀者隨 consumer 的增減更新；每租戶角色 hook 為新租戶建立 NOLOGIN NOINHERIT 的角色，還原的資料庫少了角色時下一次執行補回。
- `tests/clickhouse.rs`：全新 ClickHouse 套用每個檔案一次並建立 collector 帳號；collector 能寫日誌、只持有 `trustiot_otel_collector`；第二次執行沒有變更；輪替後舊密碼失效；帳號多了權限、或角色不是預設角色時 Job 停止且不改動它；改過的已套用檔案讓 Job 停止。
- `tests/wait_for_job.rs`（假的 Kubernetes API，HTTPS，測試 CA 簽發）：Job 不存在、執行中時繼續等，`Complete` 時結束；Job 失敗、token 被拒時以錯誤結束；其他 CA 簽的 API 憑證一律不信任。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| helm install：Job 完成後才啟動服務（k3d 測試） | 待測：k3d 上由 nightly 的 smoke 檢查「the migration Job completed before any service started」驗證（`scripts/nightly/manifests.ts` 的 `migrationOrderProblems`，單元測試已通過）；這個分支的 PR 加上 `smoke` label 時第一次在 k3d 上執行。chart 測試已確認每個服務 pod 都有等這個 Job 的 init container，而且只有它掛 Kubernetes token |
| 第二次執行不產生變更 | 通過（`tests/fresh_database.rs` 步驟 4、5，`tests/clickhouse.rs` 步驟 3） |
| 服務角色沒有 BYPASSRLS、不是表的 owner（pg_roles、pg_tables） | 通過（`tests/fresh_database.rs` 步驟 2） |
| CI 在全新資料庫跑完全部 migration | 通過：`tests/fresh_database.rs` 隨 `cargo test --workspace` 執行，CI 的 `rust` job 設有 `CI`，沒有容器就失敗 |
| 兩個 Job 同時跑只有一個執行 | 通過（`tests/concurrent_runs.rs`） |
