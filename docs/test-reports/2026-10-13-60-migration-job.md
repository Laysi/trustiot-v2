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
| e2e 測試本身的錯誤，由 2026-10-13 在 332ed66 上的 lab 先行執行發現：兩種 shape 都是 10 通過、1 失敗 | 修正。`postgres.e2e.ts` 讀密碼 Secret 的 `managedFields` 時沒有加 `--show-managed-fields`；`kubectl get -o json` 預設不輸出這個欄位，所以 `dataManagers` 是空的。以 `--show-managed-fields` 讀同一個 Secret 時，寫 `data` 的只有 `trustiot-migrate`，`helm` 只有 `f:metadata` 與 `f:type`。現在以 `--show-managed-fields` 讀取。其他 e2e 檔與 `scripts/nightly/smoke.ts` 都沒有依賴 `managedFields` 的讀取，Flux 的 lab e2e 只是把它刪掉再比對 | 修正後的結果由下一次 lab e2e 補上 |

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
| 需要容器的 Rust 測試 | 由推送後 PR CI 的 `rust` job 執行（`cargo test --workspace`，設了 `CI` 時缺容器就失敗而不是略過），執行編號由協調者補上 |

`Cargo.toml` 與 `Cargo.lock` 沒有改，所以 `cargo deny` 不重跑。

pre-commit 的審查在這一輪擋下三次：
- `proto install` 一次只裝一個工具，退回建置時改為逐一安裝，測試涵蓋。
- 重新啟用 collector 帳號時，原本先恢復 `HOST ANY` 再檢查密碼，改為先設 Secret 的密碼。
- 審查認為 `system.role_grants` 沒有 `granted_role_name` 與 `user_name` 欄位。這兩個欄位存在：`USER_ROLES` 也讀它們，`tests/clickhouse.rs` 第 6d 步對釘選的 ClickHouse 26.8 跑過這個查詢。所以只補上說明欄位的註解。
