# migration Job（2026-10-13）

對應 issue：Laysi/trustiot-v2#60（決策 4、14、29，清單 M8）。原始資料在 [data/2026-10-13-60-migration-job/](data/2026-10-13-60-migration-job/)。

## 環境

- **受測程式碼**：分支 `feat/60-migration-job` rebase 到 main `041f76c` 之後的工作樹（含發佈流程為 `migrate` 映像寫入 digest 的變更與 service-kit 新的測試容器啟動邏輯）。第一輪的數字已由第二輪取代：`workspace-checks.txt` 與 `migrate-and-sql-check-tests.txt` 都是第二輪修正後的工作樹（merge main `e2e476b` 之後）的結果。
- **Rust 測試**：共用建置主機（x86_64 Linux，Docker），`CI=1`，所以需要容器的測試在沒有容器執行環境時會失敗而不是略過。Postgres 是 `crates/service-kit` 測試共用的 TLS Postgres（PostgreSQL 18），ClickHouse 是 `clickhouse/clickhouse-server` 26.8（以 digest 釘選，與 `services/ingest` 的測試相同）。Rust 1.99.0、zig 0.16.0，皆由 `.prototools` 鎖定。
- **chart 與腳本測試**：開發用工作站（x86_64 Linux），bun 1.4.2、helm 4.3.0，由 `.prototools` 鎖定。
- **沒有 Kubernetes**：本 repo 規定工作站不跑 Kubernetes；k3d 上的驗證由 nightly 的 smoke 執行（見「結論」）。
- **nightly smoke**：GitHub-hosted runner 上的 k3d（`rancher/k3s` v1.36.5-k3s1），PR Laysi/trustiot-v2#379 帶 `smoke` label 時執行，run 38306699376、job 115002384911（2026-10-12 UTC），checkout 的是 PR head `e290c4d` 合併進 main `4edbe05` 的 merge commit；migration Job 的映像從原始碼建置。
- **第二輪修正（2026-10-13）**：獨立驗證找到的問題修正之後（見「第二輪：獨立驗證的發現與修正」），在 merge main `e2e476b` 之後的工作樹上重跑全部檢查。
- **第三輪（2026-10-13）**：依使用者意見改設計（服務角色與 collector 的密碼由 Job 產生、nightly 的 Rust 映像依內容重用），在 merge main `e6caf8e` 之後的工作樹上重跑全部檢查，數字以這一輪為準（見文末「第三輪：Job 產生的密碼與 nightly 的映像快取」）。前面兩輪描述的「密碼檔」「人建立的服務角色 Secret」已被取代。

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
| `cargo test --workspace` | 第二輪：93 個測試執行檔、660 個測試全部通過（[workspace-checks.txt](data/2026-10-13-60-migration-job/workspace-checks.txt)） |
| `cargo test -p migrate -p sql-check` | 全部通過，逐項名稱見 [migrate-and-sql-check-tests.txt](data/2026-10-13-60-migration-job/migrate-and-sql-check-tests.txt) |
| OpenAPI | `api/openapi.json is up to date` |
| `cargo deny` | advisories、bans、licenses、sources 都 ok |
| aarch64 musl 交叉編譯 | `migrate` 以釘選的 zig 建置成功（release profile；第一輪，第二輪沒有改依賴，nightly 的 smoke 也以同一個指令建置） |
| `bun test deploy/chart` | 305 通過 |
| `bun test deploy/flux` | 19 通過 |
| `bun test scripts`（含 `scripts/release`，`migrate` 映像的 digest 寫入發佈的 chart） | 649 通過、2 略過（需要 podman 的 e2e），0 失敗（第一次執行有 1 個失敗：`scripts/ci/changes.test.ts` 把測試裡的 runbook 路徑字串當成 Rust 測試讀的檔案；改字串後重跑通過，見 workspace-checks.txt 最後一段） |
| typecheck、workflow 政策、actionlint、secret handling | 通過，沒有違規 |

migrate 的整合測試涵蓋的情境：

- 第二輪新增 `tests/one_instance_per_cluster.rs`：A 資料庫套用完後，同一個 migration 角色擁有的 B 資料庫上的執行以 `it owns more than one database` 停止，B 沒有 `trustiot_migrate`；另一個 migration 角色（以 superuser 給它第一個實例角色的 ADMIN OPTION）擁有的 C 資料庫上的執行以 `owns objects or holds privileges in another database` 停止；兩次之後 A 的快照不變，`identity_app` 仍以 A 的密碼登入。
- 第二輪新增 `tests/file_escapes.rs`：留下 `SET LOCAL ROLE NONE`、`SET LOCAL ROLE identity_owner`（在 `identity` 建表）、`SET SESSION ROLE identity_owner` 的檔案都以 `LeftItsContext` 停止，表被回滾、檔案沒有記錄，再試一次 catalog 不變；`END` 的檔案停止且沒有記錄，`END` 之前建的表已提交，下一次執行以 `already exists` 失敗；`COMMIT AND CHAIN` 後再 `SET LOCAL ROLE escape_owner` 的檔案只靠交易 ID 抓到，也停止。
- `tests/fresh_database.rs`：必要的角色沒有密碼檔、或不在服務清單裡時，Job 什麼都沒建立就停止；全新資料庫第一次執行套用全部檔案（`database.sql`、identity 的角色、schema、`search_path`、密碼、`0001_identity.sql`）；`identity_app` 在 `pg_roles` 是 `(rolsuper, rolbypassrls, rolcanlogin, rolcreaterole, rolcreatedb, rolinherit) = (f, f, t, f, f, f)`，`identity_owner` 不能登入；`pg_tables` 中 schema `identity` 的 3 張表都由 `identity_owner` 擁有，`identity_app` 不擁有任何表；`identity_app` 沒有 TEMPORARY；migration 角色讀不到 `identity.sessions`（不繼承 owner 的權限）；`identity_app` 以密碼檔登入後通過 service-kit 的 session 檢查；第二次執行 `changes` 為空，catalog 快照完全相同；換密碼檔後只產生 `set the password of identity_app`，舊密碼登不進去、新密碼可以，之後再跑一次也沒有變更；改過的已套用檔案、superuser、沒有 CREATEROLE 的角色、得到 BYPASSRLS、加入 owner 角色或多了其他成員的服務角色，都讓 Job 停止；第二輪加上：`identity_app` 自己設定的資料庫專屬 `search_path`、角色層級的 `statement_timeout`、schema `identity` 的 CREATE、還給 PUBLIC 的 TEMPORARY，各自以預期的原因停止，修好後再跑沒有變更。
- `tests/concurrent_runs.rs`：另一個 session 持有鎖時，執行等到上限就放棄且沒有改任何東西；鎖持有期間開始的執行在鎖釋放後才完成；同時開始的兩次執行都成功，只有一次套用變更。
- `tests/outbox_and_tenant_roles.rs`：producer 與 consumer 的表、policy 與讀者隨 consumer 的增減更新；每租戶角色 hook 為新租戶建立 NOLOGIN NOINHERIT 的角色，還原的資料庫少了角色時下一次執行補回。
- `tests/clickhouse.rs`：全新 ClickHouse 套用每個檔案一次並建立 collector 帳號；collector 能寫日誌、只持有 `trustiot_otel_collector`；第二次執行沒有變更；輪替後舊密碼失效；帳號多了權限、或角色不是預設角色時 Job 停止且不改動它；第二輪加上：`trustiot_otel_collector` 角色多了 `SELECT ON trustiot_logs.*`、或被授予另一個角色時停止，撤銷後再跑沒有變更；帳號有 `SETTINGS readonly = 0` 時停止，`DROP USER` 後重建；改過的已套用檔案讓 Job 停止。
- `tests/wait_for_job.rs`（假的 Kubernetes API，HTTPS，測試 CA 簽發）：Job 不存在、執行中時繼續等，`Complete` 時結束；Job 不存在時的訊息寫出 Job 名稱、`another release`、`helm rollback`、`migrate.revision` 與 runbook；Job 失敗、token 被拒時以錯誤結束；其他 CA 簽的 API 憑證一律不信任。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| helm install：Job 完成後才啟動服務（k3d 測試） | 通過：nightly smoke（run 38306699376，PR head `e290c4d`）在 k3d 上兩種型態都是 `job/trustiot-migrate-… completed at …; every service container started after it`，之後的 ingress e2e 各 61 通過（[nightly-smoke.txt](data/2026-10-13-60-migration-job/nightly-smoke.txt)）。chart 測試確認每個服務 pod 都有等這個 Job 的 init container，而且只有它掛 Kubernetes token。第二輪修正推送後，PR 的 `smoke` label 會讓 smoke 再跑一次 |
| 第二次執行不產生變更 | 通過（`tests/fresh_database.rs` 步驟 4、5，`tests/clickhouse.rs` 步驟 3） |
| 服務角色沒有 BYPASSRLS、不是表的 owner（pg_roles、pg_tables） | 通過（`tests/fresh_database.rs` 步驟 2） |
| CI 在全新資料庫跑完全部 migration | 通過：`tests/fresh_database.rs` 隨 `cargo test --workspace` 執行，CI 的 `rust` job 設有 `CI`，沒有容器就失敗 |
| 兩個 Job 同時跑只有一個執行 | 通過（`tests/concurrent_runs.rs`） |

## 第二輪：獨立驗證的發現與修正

2026-10-13 另一個 agent 獨立驗證（以 SQL 與 helm 探測程式嘗試繞過權限、Secret 與 RBAC）：沒有找到服務能直接越權的方法，找到下列問題。修正後每項都有回歸測試；拿掉任一項新檢查（10 個變異）都讓對應的測試失敗（[round2-mutations.txt](data/2026-10-13-60-migration-job/round2-mutations.txt)）。驗證者的探測程式對修正後的分支重跑一次（只在那次執行時放進 `tools/migrate/tests/`，之後移除），結果在 [independent-verification/](data/2026-10-13-60-migration-job/independent-verification/)。

| 發現 | 修正後的行為 | 回歸測試 | 探測重跑 |
| --- | --- | --- | --- |
| F1 外部 Postgres 同一叢集放兩個實例時共用服務角色與密碼 | migration 角色擁有超過一個資料庫時停止；角色在其他資料庫擁有物件或權限時停止 | `tests/one_instance_per_cluster.rs` | E1、E5：`it owns more than one database` |
| F2 migration 檔案以 `END`、`SET LOCAL ROLE`、`SET SESSION ROLE` 離開 Job 的交易或角色；文字檢查被空白變化繞過 | 檔案後比對交易 ID、`current_user`、`role` 設定，不同就停止且不記錄；lint 正規化空白、加上更多字 | `tests/file_escapes.rs`；`tests/registry.rs` 的 `the_lint_sees_through_whitespace_and_case` | G1：`END`、`SET LOCAL ROLE NONE`、切到另一個 owner、`SET SESSION ROLE` 都是 `LeftItsContext`；切過去再切回來（vsfour）仍通過，是已知限制 |
| F3 服務映像與 Job 的 tag 不同時，服務在別的 schema 上啟動 | chart 渲染失敗；用 Postgres 的服務的映像參照（含 digest）也算進 Job 名稱的 hash，只換 digest 也讓 Job 再跑、服務等它 | chart 測試「a service that uses Postgres must have the migration Job's tag」與「the Job's name changes …」（拿掉這個 annotation 時後者失敗） | chart-tag-rule.txt：不同 tag 被拒、相同 tag 與不用 Postgres 的服務照常渲染 |
| F4 漏查資料庫專屬設定、角色層級的其他設定、schema 的 CREATE、PUBLIC 的 TEMPORARY | 都讓 Job 停止 | `tests/fresh_database.rs` 步驟 8 | B5、B6、C5、C6 都停止 |
| F5 漏查 collector 角色本身的權限與角色、帳號的設定 | 都讓 Job 停止 | `tests/clickhouse.rs` 步驟 6b、6c | K2、K3 都停止 |
| F6 `log_statement = all` 會記下 SCRAM verifier | runbook 寫明 | — | F2、F3：日誌沒有明文密碼，只有 verifier |
| F7 sql-check 允許整個 `src/registry.rs` | 只允許 `src/clickhouse_files.rs` | `cargo test -p sql-check` | — |
| F8 Job 不存在時 init container 默默等 | 每 30 秒寫出 Job 名稱、原因與處理方式 | `tests/wait_for_job.rs` 的 `a_missing_job_names_the_cause_and_the_way_out` | — |
| F9 `config.rs` 的 API URL 註解不對 | 改正 | — | — |

探測的 R1 自帶一份第二輪之前的 lint，所以它的 `NOT CAUGHT` 描述的是舊的 lint；新的 lint 由上表的測試檢查。已知限制（`tools/migrate/README.md`「已知限制」）：檔案能切到另一個服務的 owner 角色再切回來；服務角色能改自己的密碼；服務角色能連 `postgres` 資料庫並在那裡建暫存表（B8）；多個實例共用一個 Postgres 叢集需要角色名稱帶前綴。

## 第三輪：Job 產生的密碼與 nightly 的映像快取

2026-10-13 使用者否決兩個決定：服務角色的密碼由人建立的 Secret 提供（需要人介入，是維運風險），以及 nightly 每天從原始碼建 Rust 映像（CI 額度撐不住）。改為：

- **密碼由 Job 產生**：chart 為每個用 Postgres 的服務宣告空的 `<release>-<服務>-password`、為 collector 宣告 `<release>-otel-collector-password`（`helm.sh/resource-policy: keep`），Job 以自己的 ServiceAccount 經 Kubernetes API 讀取，缺的 key、不能用的值或 `migrate.passwordGeneration` 換了時以 CSPRNG 產生 256 位元的密碼，以帶 `resourceVersion` 的 merge patch 寫回（field manager `trustiot-migrate`），API 回傳的值與寫入的不同就停止，再設定角色與 ClickHouse 帳號。Job 的 Role 只有 `get`、`patch`，以 `resourceNames` 限定這些 Secret；chart 的其他 Role 都不碰 Secret。
- **nightly 依內容重用映像**：key 是映像每個輸入的 git 物件 hash 的 SHA-256，命中時載入 `docker save` 的 tarball、不裝 Rust 工具、不建置；只有 main 上的排程與手動執行儲存，PR 只還原。

### 指令與結果

| 項目 | 結果 |
| --- | --- |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `cargo test --locked --workspace` | 94 個測試執行檔、673 個測試全部通過（加上下面 M3 的新測試之前；之後 `cargo test -p migrate` 再跑一次，全部通過） |
| `cargo test --locked -p migrate` | 全部通過：單元 16，`tests/passwords.rs` 7，其餘整合測試各自通過，逐項名稱見 [round3-generated-passwords.txt](data/2026-10-13-60-migration-job/round3-generated-passwords.txt) |
| `cargo run -p openapi -- --check` | `api/openapi.json is up to date` |
| `bun test deploy/chart` | 309 通過 |
| `bun test deploy/flux` | 19 通過 |
| `bun run --cwd scripts test`（同 `bun test scripts`） | 672 通過、2 略過（需要 podman 的 e2e），0 失敗 |
| `bun run --cwd scripts typecheck` | 通過（第一次執行抓到 `chart.test.ts` 的型別錯誤，修正後通過） |
| `check-workflows`、`lint-workflows`（actionlint）、`check-secret-handling` | 通過，沒有違規 |
| 變異測試 | 6 個變異；5 個第一次就被抓到，1 個（API 存的值與寫入的不同時不停止）沒被抓到，補上 `a_value_the_api_did_not_store_as_written_stops_the_run` 後被抓到（[round3-mutations.txt](data/2026-10-13-60-migration-job/round3-mutations.txt)） |

`cargo deny` 沒有重跑：這一輪沒有改 `Cargo.toml` 與 `Cargo.lock`。

提交 nightly 快取的 commit 時，pre-commit 的審查擋下三次，都已修正：第一次儲存時快取目錄還不存在（`saveImageForCache` 先建立目錄，測試涵蓋）；PR 的觸發路徑少了映像的輸入（加上 workspace、工具鏈、Containerfile 與 `tools/migrate` 經 path 相依用到的 crate，`image-cache.test.ts` 檢查清單完整）；key 少了建置腳本 import 的檔案（改為整個 `scripts/`）。另外加上 tarball 載入失敗時寫出要刪哪個快取項目。修正後 `bun run --cwd scripts test` 672 通過、2 略過，typecheck、`check-workflows`、actionlint 通過；在建置主機的乾淨快照上 `bun scripts/nightly/image-cache.ts key migrate` 印出 key。[round3-generated-passwords.txt](data/2026-10-13-60-migration-job/round3-generated-passwords.txt) 的 nightly 與 ci 測試清單是修正前的（214 個）。

### 測試涵蓋的情境

- `tests/passwords.rs`（假的 Kubernetes API，`tests/support/secrets.rs`，不需要容器）：第一次執行每個 Secret 一次 `GET`、一次 `PATCH`，填入每個缺的 key（64 個十六進位字元、各不相同），annotation 記下密碼世代；第二次執行只讀不寫；新的世代換掉每個受管的 key、不動其他 key、只換一次；遺失或不能用的 key 單獨重新產生；Secret 在讀寫之間被改過時 API 回 409，重讀後再決定，一直被改時停止而不覆蓋；API 存下的值與寫入的不同時停止，不回報產生；token 被拒、403、API 暫時 503 的重試與上限；Secret 不存在時寫出 `helm upgrade` 與 `migrate.revision` 的處理方式；報告、錯誤與 `Debug` 都不含任何密碼或它的 base64；沒有綁定時不發請求。
- `tests/fresh_database.rs`：綁定的角色不在服務清單時什麼都不改、Secret 也沒被讀寫；第一次執行由 Job 產生 `identity_app` 的密碼並設定，服務以 Secret 裡的值登入；`passwordGeneration` 改成 2 後換掉密碼、舊的登不進去，再跑一次沒有變更；key 被刪掉時重新產生；別人寫進 Secret 的合格值成為角色的密碼。
- `tests/clickhouse.rs`：collector 帳號以 Job 產生的密碼建立、寫入，新的世代輪替後舊密碼失效。
- chart 測試：每個用 Postgres 的服務與 collector 各一個空的密碼 Secret（只有 `type` 與標籤、annotation，沒有 `data`），Job 關閉時也宣告；Job 的 Role 只有 `get`、`patch` 與排序後的 `resourceNames`，chart 其他 Role 都不碰 Secret；服務的 `env` 不能覆寫 `TRUSTIOT__DATABASE__USERNAME`／`PASSWORD`；collector 等 Job 完成才啟動；`passwordGeneration` 只接受數字；移除的欄位（`serviceRolesSecret`、collector 的 `passwordSecret`）被 schema 拒絕。
- `scripts/ci/check-secret-handling`：template 裡的 Secret 只能是帶 `trustiot.io/filled-by: migrate` 的空 Secret，帶 `data` 或 `stringData` 的仍被拒。
- nightly：smoke 在第一次安裝後檢查每個密碼 Secret 的 key 都在、是 Job 產生的格式、有世代 annotation（只讀名稱，Secret 的 JSON 不進日誌），uninstall 後密碼 Secret 與資料庫一起保留再刪除；映像快取的 key、載入、儲存與 workflow 條件都有單元測試（`scripts/nightly/image-cache.test.ts`、`service-image.test.ts`、`scripts/ci/workflow-conditions.test.ts`）。

### 沒有執行的

- **真叢集上的 Job 產生密碼**：`deploy/chart/tests/e2e/postgres.e2e.ts` 已改寫成一次安裝（CloudNativePG、叢集內 ClickHouse、collector、migration Job、由 example-service 扮演的 identity），檢查第一次安裝產生 key、服務與 collector Ready，同樣 values 的 `helm upgrade` 與 `helm rollback` 後密碼不變，`migrate.passwordGeneration=2` 後改變且服務與 collector 再次 Ready，刪掉 key 後以 `migrate.revision=2` 重新產生，uninstall 保留密碼 Secret；比較只在測試行程內以 digest 做，輸出只有 `unchanged`／`changed`。這個 e2e 需要 lab 的叢集、lab 鎖與 `e2e-60` namespace，以及 arm64 的 migrate 與 example-service 映像（不能在 lab 的 Mac 上建 Rust），這一輪沒有執行，由協調者執行後補上結果。
- **nightly 的快取命中**：要等 main 上的排程執行第一次儲存之後才看得到命中；命中時省下的時間待量測。
- **PR 的 smoke**：這一輪的變更推送後，PR 的 `smoke` label 會讓它再跑一次（Job 產生 collector 的密碼、第一次安裝的 key 檢查）。

## 第四輪：CI 修正與第三輪獨立驗證的發現

2026-10-13 先合併 `origin/main`（merge commit，未 rebase）。main 新增的 Renovate 規則（Laysi/trustiot-v2#394）在 ci.yml 的 `workflow-policy` 擋下 `tools/migrate/tests/support/mod.rs` 的 ClickHouse 映像：只有 `name@sha256:…`。改成 `name:tag@sha256:…`，tag 與 digest 同 `services/ingest/tests/common/mod.rs`；`renovate.test.ts` 的同一映像份數由 3 改為 4。分支沒有其他裸 digest。

### 發現與處理

| 發現 | 處理 | 證據 |
| --- | --- | --- |
| R1（中）`check-secret-handling` 的空 Secret 例外可以繞過：值來自 values、`randAlphaNum` 或 `lookup` 的 annotation 與標籤，以及同一行在無輸出動作之後的 `data:`／`stringData:` | 修正。Secret 樣板裡含 `{{` 的行只能逐字等於三行已知的行（名稱、chart 標準標籤的 include、`{{- end }}`）。另加全 repo 規則：任何 Helm 樣板與 helper 都不能用 `lookup`、`rand*`、`uuidv*`、`shuffle`、`gen*` 憑證與金鑰函式、`derivePassword`、`htpasswd`、`bcrypt`、AES 函式、`getHostByName`，以會略過字串與註解的 tokenizer 判斷。規則放在每個 PR 都跑的 `check-secret-handling.ts`，不放在只有 chart 變動時才跑的 chart 測試。chart 測試改成逐欄比對渲染出的密碼 Secret，包括標籤與 annotation 的集合與值 | `scripts/ci/check-secret-handling.test.ts` 的「a template may declare only an empty Secret that the migration Job fills」與「no template may make or read secret material」：驗證者的 8 個 probe 與另外 3 個變形都必須被拒，chart 自己的形狀是通過的對照組（60 通過） |
| R2（低）預先建立的 Secret 會被 Helm 接手，Job 保留裡面自選的密碼 | 修正。Job 寫入時在同一個 merge patch 記下 `trustiot.io/password-secret-uid: <metadata.uid>`，只有在它等於 Secret 目前的 uid 時才信任資料，否則每個 key 都重新產生。chart 不渲染這個 annotation，R1 的逐欄比對涵蓋這一點。剩下的風險寫進文件：能 patch 這些 Secret 的人本來就讀得到；從備份還原會改變 uid，下一次 Job 會重新產生並收斂 | `tests/passwords.rs` 的 `a_secret_someone_else_created_never_keeps_its_passwords`（重建的 Secret 帶同代 annotation 與自選密碼、偽造的 uid annotation），單元測試 `data_is_trusted_only_in_the_object_the_job_filled`；e2e 的第一次安裝檢查兩個 Secret 都是 `bound: true` |
| R3（低）服務停用、改名，或 collector 換帳號之後，舊的登入仍然有效 | 修正。Postgres：登入角色只在綁定了 Secret 時 LOGIN，沒有綁定就 `NOLOGIN`，重新綁定時先設 Secret 的密碼再 LOGIN；服務清單已經沒有的角色也改成 `NOLOGIN`。只挑 Job 建立時以 `COMMENT ON ROLE ... IS 'trustiot-migrate:login-role'` 標記、且 migration 角色有 ADMIN OPTION 的角色，不依名稱挑選，也不 `DROP`。ClickHouse：拿到 `trustiot_otel_collector` 角色、但不是目前 collector 帳號的帳號改成 `HOST NONE`（不刪除），collector 再開啟時恢復成 `HOST ANY`。留下的 Secret 的刪除步驟寫進 runbook 的「停用服務之後留下的 Secret」 | `tests/outbox_and_tenant_roles.rs` 第 5 到 7 步：移出服務清單、部署中關掉再打開，以及沒有標記或沒有 ADMIN OPTION 的角色不受影響。`tests/clickhouse.rs` 第 6d 步：關掉、再打開、改名 |
| R4（低）Secret 已經 patch、`ALTER ROLE` 還沒執行時失敗，重啟的 pod 會讀到角色還沒有的密碼 | 接受，寫進 runbook 的「輪替到一半 Job 失敗時」：這個順序不會留下一個沒有任何 Secret 記得密碼的角色，Job 重試成功後就收斂 | 文件 |
| R5（低）Flux 的 rollback remediation 渲染較舊的 `passwordGeneration`，字串比較讓每次 rollback 與再升級都輪替一次 | 修正。代以數字比較（1 到 9 位數字，與 chart 的 schema 一致），只往上走：要求的代較小時保留現有的密碼與記的代。在較小的代之下補填遺失的 key 時，記的是較大的那一代，所以之後升級回來也不會再輪替。`--cleanup-on-fail` 會刪掉那次升級新建的密碼 Secret（即使有 `keep`），下一次成功的 Job 會重新產生，這點寫進 runbook | `tests/passwords.rs` 的 `a_rollback_keeps_the_passwords_and_so_does_the_re_upgrade`，單元測試 `generations_only_go_up`、`generations_are_decimal_digits`；e2e 新增「rollback 到較舊的代不換密碼，之後升級回來也不換」 |
| R6（低）nightly 映像快取的 key 少了輸入；不完整或損壞的 tarball 讓 nightly 失敗 | 修正。key 加上 `.dockerignore`、`nightly.yml`、`.github/actions/setup-proto`，以及 workspace 成員 `edge/`、`factory/`，測試檢查每個成員目錄都在輸入裡；`.dockerignore` 也加進 nightly 的 PR 路徑。tarball 無法載入、沒有預期的 tag 或平台不對時，改為寫一行說明並發出 GitHub warning，以 proto 安裝命中時略過的建置工具，再從原始碼建置 | `scripts/nightly/image-cache.test.ts` 與 `service-image.test.ts` 的新案例 |
| `helm upgrade --force-replace`（lab 確認） | runbook 寫明不要用，以及原因：它清空兩種密碼 Secret 的資料與 annotation，接著取代同名 Job 時失敗（`spec.selector … immutable`），升級停在 FAILED，Job 不會再跑。看到這種失敗要立刻改 `migrate.revision` 再一般地升級 | 文件 |
| e2e 測試本身的錯誤，由 2026-10-13 在 332ed66 上的 lab 先行執行發現：兩種 shape 都是 10 通過、1 失敗 | 修正。`postgres.e2e.ts` 讀密碼 Secret 的 `managedFields` 時沒有加 `--show-managed-fields`；`kubectl get -o json` 預設不輸出這個欄位，所以 `dataManagers` 是空的。以 `--show-managed-fields` 讀同一個 Secret 時，寫 `data` 的只有 `trustiot-migrate`，`helm` 只有 `f:metadata` 與 `f:type`。現在以 `--show-managed-fields` 讀取。其他 e2e 檔與 `scripts/nightly/smoke.ts` 都沒有依賴 `managedFields` 的讀取，Flux 的 lab e2e 只是把它刪掉再比對 | 修正後的結果見「第四輪 lab e2e」：兩種 shape 都是 12 通過 |

### 指令與結果

| 項目 | 結果 |
| --- | --- |
| `bun test scripts/nightly`（合併 main 並修正映像寫法之後） | 136 通過 |
| `bun test scripts`（R6 之後） | 全部通過，2 略過（需要 podman 的 e2e），0 失敗 |
| `bun test scripts/nightly`（R6 之後） | 139 通過 |
| `bun test scripts/ci` | 300 通過 |
| `bun run --cwd scripts typecheck` | 通過，包括 `deploy/chart/tests/e2e/postgres.e2e.ts` |
| `bun test deploy/chart` | 309 通過 |
| `bun test deploy/flux` | 19 通過 |
| `check-workflows`、`lint-workflows`（actionlint）、`check-secret-handling` | 通過，沒有違規 |
| `cargo clippy -p migrate`、`cargo test -p migrate`（R2、R5 之後） | 通過：單元 19 個、`tests/passwords.rs` 9 個，其餘整合測試也通過 |
| `cargo test --locked -p migrate --test clickhouse`（R3 的 ClickHouse 部分） | 1 通過。這是 pre-commit 審查要求「重新啟用帳號時先設 Secret 的密碼、再恢復 `HOST ANY`」之前的版本；修改後的版本由 PR CI 執行 |
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`（2026-10-14，在建置主機上的 R3 commit 之後，經檢查佇列） | 通過，沒有警告 |
| `cargo test --locked --workspace`、`cargo run --locked -p openapi -- --check`（同上） | 全部通過，0 失敗；`api/openapi.json is up to date`。這次建置主機上沒有 docker 或 podman，所以在不設 `CI` 的情況下執行，需要容器的測試（`tools/migrate` 的 Postgres 與 ClickHouse 整合測試，包括 R3 在 `tests/outbox_and_tenant_roles.rs` 的第 5 到 7 步與 `tests/clickhouse.rs` 的第 6d 步）都略過。設了 `CI` 的同一批檢查在第一個容器測試就停住（`tests/clickhouse.rs`：`podman or docker is required when CI is set`）；`migrate` 的 19 個單元測試在那之前通過 |
| 需要容器的 Rust 測試 | 由推送後 PR CI 的 `rust` job 執行（`cargo test --workspace`，設了 `CI` 時缺容器就失敗而不是略過）。33dd568 的 run 38516149613 在 `tests/outbox_and_tenant_roles.rs` 第 6 步失敗：測試以 `db.bind` 重新宣告了空的 Secret，卻沒有預期 Job 先產生密碼；產品的順序是對的。`c433820` 修正預期，並補上 Helm 保留後重新接手的 Secret 這條路徑（不產生密碼，先設密碼再 LOGIN）。c433820 的 run 38517891393 跑過 `migrate` 的每個容器測試執行檔，全部通過；20e3a9a 的 CI run 38519171335 與 smoke run 38519171296 都通過 |

`Cargo.toml` 與 `Cargo.lock` 沒有改，所以 `cargo deny` 不重跑。

pre-commit 的審查在這一輪擋下三次：
- `proto install` 一次只裝一個工具，退回建置時改為逐一安裝，測試涵蓋。
- 重新啟用 collector 帳號時，原本先恢復 `HOST ANY` 再檢查密碼，改為先設 Secret 的密碼。
- 審查認為 `system.role_grants` 沒有 `granted_role_name` 與 `user_name` 欄位。這兩個欄位存在：`USER_ROLES` 也讀它們，`tests/clickhouse.rs` 第 6d 步對釘選的 ClickHouse 26.8 跑過這個查詢。所以只補上說明欄位的註解。

### 第四輪 lab e2e

2026-10-14（台北時間）在 lab 的 Mac mini M1 上，以 `deploy/chart/tests/e2e/postgres.e2e.ts` 對第四輪的程式碼各跑一次 `saas` 與 `onprem`：兩種 shape 都是 12 個測試全部通過、82 個斷言，0 失敗。

- **受測程式碼**：`feat/60-migration-job` 的 `33dd568`（工作樹乾淨，建置主機的副本取自這個狀態）。之後的 `c433820` 只改 `tools/migrate/tests` 的整合測試，不改產品程式碼與 chart，所以這次的映像與它的產品程式碼相同。
- **環境**：k3s v1.36.5+k3s1、Istio 1.30.5 ambient、CloudNativePG 1.30.1（2026-10-13 經使用者授權暫時安裝，這次沒有動它）、namespace `e2e-60`（Pod Security `restricted`）。執行身分是該 namespace 的 ServiceAccount `e2e-60-runner`：綁定內建的 `admin`，另加一個 Role 給 `postgresql.cnpg.io` 的 clusters、imagecatalogs、databases、databaseroles，`gateway.networking.k8s.io` 的 gateways、httproutes 與 `security.istio.io` 的 authorizationpolicies（[round4-lab-e2e-runner-rbac.yaml](data/2026-10-13-60-migration-job/round4-lab-e2e-runner-rbac.yaml)）。kubeconfig 只放在本機暫存目錄，API server 經 SSH 轉送，測完刪除。
- **二進位與映像**：在建置主機以檢查佇列執行 `cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p service-kit --bin example-service -p migrate --bin migrate`（zig 0.16.0、cargo-zigbuild 0.23.4，由 `.prototools` 鎖定；44.4 秒）。SHA-256：example-service `e7d3cb23…387d`（與 332ed66 的建置相同）、migrate `4d73fbe2…bbe2`。映像在本機以 podman 打包成 arm64，匯入節點的 k3s containerd：

  | 映像 | digest |
  | --- | --- |
  | `localhost/trustiot-e2e/example-service:60` | `sha256:d092547e29ea956c06faab885993dc3e99a38ff900c87c70d6643a8960e23a67` |
  | `localhost/trustiot-e2e/migrate:60` | `sha256:d23b43f5718534ce1c0ed9df07e7a873e624cd5291abd256d613bb422c97274f` |
  | `localhost/trustiot-e2e/postgres:60`（發佈的 `v0.0.0-rc.6` 映像，匿名拉取後加 tag；`deploy/images/postgres` 自該版的來源 commit 之後沒有變更） | `sha256:a69113bc9ed44cc810941248f3a0874371df11b5cd252c417ac793d26e83157b` |

  完整的二進位 SHA-256 與映像 ID 在 [round4-lab-e2e-binaries-and-images.txt](data/2026-10-13-60-migration-job/round4-lab-e2e-binaries-and-images.txt)，建置指令在 [round4-lab-e2e-build.sh](data/2026-10-13-60-migration-job/round4-lab-e2e-build.sh)。
- **指令**（在工作樹根目錄，兩種 shape 各一次，一次一個）：[round4-lab-e2e-run-shape.sh](data/2026-10-13-60-migration-job/round4-lab-e2e-run-shape.sh) 以 `E2E_NAMESPACE=e2e-60 E2E_SHAPE=<shape> E2E_POSTGRES_IMAGE=localhost/trustiot-e2e/postgres:60 E2E_SERVICE_IMAGE=localhost/trustiot-e2e/example-service:60 E2E_MIGRATE_IMAGE=localhost/trustiot-e2e/migrate:60 E2E_IMAGE_PULL_POLICY=Never E2E_REPORT_DIR=<目錄> bun test --timeout 900000 --reporter=junit ./deploy/chart/tests/e2e/postgres.e2e.ts` 執行（資料目錄裡的兩個腳本比實際執行的多兩處：建置命令明寫 `pipefail`，其實 rcheck 本來就以 `pipefail` 執行；`run-shape.sh` 結尾以測試的結束碼離開，實際執行時結束碼只記在輸出檔最後一行的 `rc=`）。
- **時間**：saas 05:44:24Z 到 05:47:20Z（整個檔案 175.7 秒），onprem 05:47:33Z 到 05:50:28Z（175.1 秒）。

| 檢查 | saas | onprem |
| --- | --- | --- |
| Cluster 健康、一個 instance、在 mesh 外、符合 restricted | 通過，0.36 秒 | 通過，0.35 秒 |
| 伺服器 TLS、AGE、內建 locale、沒有 superuser 密碼、owner 沒有 superuser | 通過，1.89 秒 | 通過，1.85 秒 |
| AGE 的 `cypher()` | 通過，0.73 秒 | 通過，0.72 秒 |
| 沒有 TLS 的 TCP 連線被拒 | 通過，0.36 秒 | 通過，0.38 秒 |
| Job 把密碼產生進 chart 宣告的空 Secret：只有該有的 key、世代 `1`、`keep`、寫 `data` 的只有 `trustiot-migrate`、兩個 Secret 都是 `bound: true`（uid 綁定） | 通過，0.79 秒 | 通過，0.79 秒 |
| 服務以 Job 建立的角色與 verify-full 連線 | 通過，0.64 秒 | 通過，0.62 秒 |
| 另一個 CA、不在憑證裡的主機名稱、沒有放行的 label 都被拒 | 通過，5.24 秒 | 通過，5.18 秒 |
| 同樣 values 的 `helm upgrade` 與 `helm rollback` 後密碼 `unchanged` | 通過，54.8 秒 | 通過，54.6 秒 |
| `migrate.passwordGeneration=2`：`changed`、新的 Job、服務與 collector 再次 Ready | 通過，11.4 秒 | 通過，11.5 秒 |
| 新增：rollback 到世代 1 後密碼 `unchanged`，升級回世代 2 後也 `unchanged` | 通過，20.9 秒 | 通過，20.5 秒 |
| 刪掉 key 後以 `migrate.revision=2` 重跑：`changed`，服務 Ready | 通過，11.0 秒 | 通過，11.4 秒 |
| `helm uninstall` 後 Cluster、ImageCatalog、PVC 與兩個密碼 Secret 保留 | 通過，3.2 秒 | 通過，3.1 秒 |

第一次安裝：`helm install --wait` 返回 saas 33.3 秒、onprem 29.8 秒，Cluster 在返回時已健康，整組 Ready 約 46 秒。輸出裡的密碼比較只有 `unchanged` 與 `changed`（[saas](data/2026-10-13-60-migration-job/round4-lab-e2e-saas-output.txt)、[onprem](data/2026-10-13-60-migration-job/round4-lab-e2e-onprem-output.txt)），伺服器設定、角色、Secret 的形狀（名稱、key、世代、`bound`、寫入者）與拒絕訊息在 [saas](data/2026-10-13-60-migration-job/round4-lab-e2e-saas.json) 與 [onprem](data/2026-10-13-60-migration-job/round4-lab-e2e-onprem.json)，JUnit 在同目錄的 `round4-lab-e2e-*.junit.xml`。這些檔案沒有密碼值、密碼的雜湊或 Secret 的 `data`。

**環境問題**：saas 的第一次執行（約 05:30Z）在第 9 個測試時失敗，原因是這台工作站到 lab 的 SSH 轉送（API server 的連線）中斷，`helm` 與 `kubectl` 回 `TLS handshake timeout`、`connection refused`，之後的測試與清理跟著失敗（8 通過、5 失敗，節錄在 [round4-lab-e2e-attempt1-saas-tunnel-lost.txt](data/2026-10-13-60-migration-job/round4-lab-e2e-attempt1-saas-tunnel-lost.txt)），沒有產品的錯誤。改用會自動重連的轉送後，刪除 namespace 重建，重跑 saas 與 onprem 都通過。

**2026-10-13 在 332ed66 的先行執行**：兩種 shape 各 10 通過、1 失敗；失敗的是測試本身的錯誤（讀 Secret 時沒有 `--show-managed-fields`，已在 `064a325` 修正）。那次的原始檔隨工作站的 /tmp 清除而遺失，上面的結果是修正後在 `33dd568` 重跑的。

**清理**：兩次執行之後刪除 namespace `e2e-60`；`kubectl get clusters.postgresql.cnpg.io -A` 沒有任何 Cluster、沒有屬於這次執行的 PV。三個 `:60` 映像留在節點上給獨立驗證重用；CloudNativePG 與 lab 鎖由協調者處理。建置主機上的副本、本機的暫存檔與 kubeconfig 已刪除。

## 第五輪：第四輪的獨立複驗

2026-10-14 獨立驗證以 33dd568 的程式碼與 lab 映像複驗第四輪。結論如下：

- R1 到 R6 都成立，第三輪的 R1 繞過方式全部被拒。
- 在 lab 上以真的 Job 確認了以下各點：
  - 預先建立的 Secret 不會被採用。
  - 兩個同時執行、真的遇到 409 的 Job 會收斂。
  - `log_statement=all` 的日誌裡只有 SCRAM verifier。
  - 洩漏掃描沒有找到任何值。
  - R3 的停用與重新啟用順序正確。
  - 服務角色無法偽造標記。

新的發現與處理：

| 發現 | 處理 | 證據 |
| --- | --- | --- |
| N1（低到中，第四輪改成數字比較後才出現）values 檔裡的整數 `passwordGeneration` 到 1000000 以上時，`toString` 會渲染成科學記號（`1e+06`、`2.0261014e+07`）。schema 接受這個值，Job 拒絕它，所以每次執行都失敗，升級進入 remediation。`--set` 給的是字串，不受影響 | 修正。新增 `trustiot.decimal`：整數以 `%d` 渲染、字串照原樣，其他值讓渲染失敗。`passwordGeneration` 經過它之後，不是 1 到 9 位數字也讓渲染失敗；`migrate.revision` 的 annotation 也改用它（它只進 hash，原本不會壞，但同樣會變成科學記號）。chart 渲染進程式會解析的設定的其他數字只有 port 與 `max_connections`，schema 把它們限制在遠低於 1000000 的範圍 | chart 測試「passwordGeneration and revision from a values file render as exact decimals…」：values 檔的整數 `20261014`、`1000000`、`999999999`、`0`、加引號的字串、`--set`，以及必須失敗的 10 位數（整數與字串） |
| N3（低）預先建立、被 Helm 接手的 Secret 保留建立者放的 annotation、標籤、ownerReferences 與 finalizers，Job 再把密碼寫進去：replicator 或 reflector 可能把密碼複製到建立者讀得到的 namespace，owner 可能讓 Secret 被垃圾回收 | 修正。綁定 Secret 的同一個 merge patch（第一次填入、別人建立的物件、從備份還原）把白名單以外的每個 annotation 與標籤、`ownerReferences`、`finalizers` 設成 null。白名單是 chart 的、Helm 的 ownership annotation、Flux helm-controller 的兩個標籤，以及 Job 的兩個 annotation。報告只寫 key。已綁定的 Secret 不再處理。pre-commit 審查另外指出兩點並已修正：（一）文件原本說「能 patch 就讀得到」並不精確，RBAC 分開授予 `get` 與 `patch`，但沒有 `get` 的 patch 也能寫入自選的密碼，所以文件改為「patch 權限等同能取得密碼，只能給受信任的主體」；（二）正在刪除（有 `deletionTimestamp`）的 Secret 一律在 patch 之前停止，否則拿掉 finalizer 會讓 Secret 消失，角色卻已經設成它的密碼 | 單元測試 `binding_a_secret_removes_metadata_neither_the_chart_nor_the_job_sets`（patch 內容），`tests/passwords.rs` 的 `binding_a_foreign_secret_removes_metadata_neither_the_chart_nor_the_job_sets` 與 `a_secret_being_deleted_is_never_filled` |
| N4（低）ClickHouse 停用規則會鎖住任何拿到 `trustiot_otel_collector` 的帳號（lab 上手動建立的帳號也被鎖） | 修正與文件。查詢排除 `currentUser()`，也就是 Job 自己執行時用的帳號。ClickHouse 沒有可以標記帳號的地方，所以其他拿到這個角色的帳號仍然算是 Job 的：runbook、`secrets.md` 與 README 寫明外部 ClickHouse 上不要把這個角色給自己的帳號，以及怎麼復原。`currentUser()` 的測試沒有加：測試用 ClickHouse 的管理帳號是設定檔定義的，不能授予角色，要另建一個 SQL 帳號來當 migration 帳號，不便宜 | 程式與文件；實際行為由 PR CI 的 `tests/clickhouse.rs` 第 6d 步涵蓋（預期沒變） |
| N2（既有的弱點）`check-secret-handling` 不認得 `kind` 的其他寫法，也不認得由樣板動作組出來的 Secret | 不在這個 PR 處理，由協調者開後續 issue | — |
| 文件：`--force-replace` 的機制 | 修正。它不是刪除再建立，而是以 chart 渲染的內容整個取代物件（PUT replace）：Secret 的 uid 不變，但資料與 annotation 被清空；同名 Job 的取代因 `spec.selector … immutable` 失敗。結論不變 | runbook |
| 選做：chart 只綁定 `<schema>_app`，有平台角色的服務會讓平台角色停在 NOLOGIN | 加上防護。`tests/registry.rs` 的 `the_chart_binds_every_login_role_of_the_product` 固定 chart 的 Postgres 綁定是每個服務一個 `<schema>_app`，而且產品的服務清單裡不能有平台角色；Laysi/trustiot-v2#65 加入第一個平台角色時要一併修改 chart 與這個測試。`scripts/ci/changes.ts` 把這個測試讀的 `_migrate.tpl` 列進 rust job 的輸入 | 測試 |

### 指令與結果

| 項目 | 結果 |
| --- | --- |
| `bun test deploy/chart` | 310 通過 |
| `bun test scripts` | 693 通過，2 略過（需要 podman 的 e2e），0 失敗 |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun scripts/ci/check-secret-handling.ts` | 285 個檔案，沒有違規 |
| `bun test scripts/ci/changes.test.ts` | 170 通過 |
| 經檢查佇列：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings` | 通過 |
| 經檢查佇列：`env -u CI cargo test --locked -p migrate` | 全部通過：單元 20 個、`tests/passwords.rs` 11 個、`tests/registry.rs` 5 個、`tests/wait_for_job.rs` 4 個。建置主機沒有容器執行環境，所以不設 `CI` 執行，需要容器的 6 個測試執行檔（`clickhouse`、`concurrent_runs`、`file_escapes`、`fresh_database`、`one_instance_per_cluster`、`outbox_and_tenant_roles`）都略過 |
| 經檢查佇列：`cargo run --locked -p openapi -- --check` | `api/openapi.json is up to date` |
| 需要容器的 Rust 測試 | 由推送後 PR CI 的 `rust` job 執行，執行編號由協調者補上。這一輪改到的部分（N3 的 metadata 移除、N4 的帳號查詢）逐步對照了這些測試的預期：fake 宣告的空 Secret、chart 渲染的 Secret 都只有白名單內的 metadata，不會多出報告行；第 6d 步沒有把角色給 migration 帳號，預期不變 |

pre-commit 審查在這一輪擋下三次，都已修正：
- N3：「patch 就讀得到」的說法不精確。
- N3：正在刪除的 Secret。
- 平台角色的防護：原本只檢查 `_platform` 字樣，改為逐項比對 chart 的 Postgres 綁定。
- N1、N4 與 `--force-replace` 的文件一次就通過。

## 第六輪：第五輪的程式層複驗

2026-10-14 獨立驗證在程式層複驗第五輪（head 94b3614）。N1、N3、N4 與服務清單的防護都成立：
- N1：values 檔與 `--set` 共 23 種寫法都渲染成正確的十進位或讓渲染失敗。
- N3：managedFields 留不住外來的 key，Helm 與 Flux 的 ownership 在拿掉外來 metadata 之後仍在。

剩下三個低風險項目：

| 發現 | 處理 | 證據 |
| --- | --- | --- |
| L1（低）`tools/migrate/src/passwords.rs` 的 `KEPT_LABELS`、`KEPT_ANNOTATIONS` 跟 chart 之間只靠一行註解對應 | 修正。chart 測試讀這兩個陣列（Job 的 annotation 常數也解析出值），在四種渲染設定下檢查兩種密碼 Secret 的每個標籤與 annotation、以及 Helm 的兩個 ownership annotation 都在白名單裡。這兩種 Secret 是服務的與 collector 的；四種設定是 saas、onprem、全部服務、Job 關閉。`scripts/ci/changes.ts` 把 `passwords.rs` 列為 chart 測試讀的檔案 | chart 測試「every label and annotation the chart and Helm put on the password Secrets is one the Job keeps…」。從白名單拿掉 `app.kubernetes.io/part-of` 時這個測試失敗，並寫出 Secret 與 key |
| L3（低）在 ClickHouse 設定檔（`users_xml`）定義的帳號若拿到 `trustiot_otel_collector`，`ALTER USER … HOST NONE` 改不了它，Job 每次都停 | 修正。查詢只選 `system.users` 的 storage 不是 `users_xml` 的帳號，`currentUser()` 仍然排除。runbook 與 README 一併更新。沒有加測試：測試用 ClickHouse 只有設定檔定義的管理帳號，要做出「設定檔帳號拿到角色」得改容器的設定，不便宜 | 程式與文件；PR CI 的 `tests/clickhouse.rs` 第 6d 步會跑這個查詢 |
| L2（低）叢集政策（Kyverno、Gatekeeper）若要求 Secret 一定要帶某個只有 post-renderer 或 Flux `spec.commonMetadata` 才會加的 key，會拒絕 Job 綁定時拿掉 metadata 的 patch，Job 每次都停 | 只寫進文件，不加新的設定。runbook 新增「要求 Secret 帶某個標籤或 annotation 的叢集政策」：症狀是 `refused to patch it` 加上指名這個 Secret 的 admission 拒絕；處理是讓政策豁免兩種密碼 Secret，或改用會自動加上那個 key 的 mutating 政策；也寫明第一次填入時會拿掉一次 post-renderer 或 `commonMetadata` 加的 key。SaaS 的 HelmRelease 兩者都沒用 | runbook |

### 指令與結果

| 項目 | 結果 |
| --- | --- |
| `bun test deploy/chart` | 311 通過 |
| `bun test scripts` | 693 通過，2 略過（需要 podman 的 e2e），0 失敗 |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun scripts/ci/check-secret-handling.ts` | 285 個檔案，沒有違規 |
| 經檢查佇列：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings` | 通過 |
| 經檢查佇列：`env -u CI cargo test --locked -p migrate` | 全部通過：單元 20 個、`tests/passwords.rs` 11 個、`tests/registry.rs` 5 個、`tests/wait_for_job.rs` 4 個。建置主機沒有容器執行環境，需要容器的 6 個測試執行檔都略過，由推送後的 PR CI 執行 |

這一輪的 pre-commit 審查每個 commit 都一次通過。
