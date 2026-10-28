# AI 助理：對話串 API 與排程清除（2026-10-28）

Laysi/trustiot-v2#645：AI 助理的 API 骨架（`/api/assistant/v1`）、對話串的七個端點（清單、開始、讀取、改名與封存、刪除、已刪除清單、還原），以及刪除滿 30 天的對話的排程清除（每個副本都排程，同一時間一個副本以平台角色 `assistant_platform` 分批刪除）。這份報告是驗收條件的證據：全部由下列自動化測試以 `CI=1` 實際執行，需要容器的測試沒有略過。

## 環境

- **程式碼**：分支 `feat/645-assistant-chat-api`，從 main `5dad042` 分出；結果取自 commit `01d1518` 的樹。本報告的 commit 只加這份文件。
- **機器**：本機（x86_64 Linux 工作站）。容器測試以 podman 起拋棄式 TLS Postgres，以 migration Job 套用整個產品的 schema（`testkit::product`，每個測試執行檔一個資料庫）。API 與清除的測試啟動 `assistant` 執行檔本身當子行程（`services/assistant/tests/service/mod.rs`），清除的測試同時起兩個行程。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的版本：Rust 1.99.0、Bun 1.4.2、Helm 4.3.0、kubeconform 0.8.0、cargo-deny 0.20.2、gitleaks 8.30.1。
- **設定**：`CI=1`，所以沒有容器執行環境時測試會失敗，不會略過。不連真正的 AI 供應商：主 agent 的模型是 loopback 上回放 `tests/fixtures/` 的替身，服務設定的 AI 閘道位址沒有東西在聽（這些測試都不發問）。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
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
bun test deploy/chart
bun test deploy/flux
bun install --cwd scripts --frozen-lockfile
bun test scripts
bun run --cwd scripts typecheck
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
gitleaks git --redact --log-opts="origin/main..HEAD" .
git archive HEAD | tar -x -C <空目錄> && gitleaks dir --redact <空目錄>
```

驗收證據可以單獨重跑：`CI=1 cargo test -p assistant --test api --test cross_tenant --test purge_job --test conversation_threads --test startup -- --nocapture`。

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test --workspace` | 2058 通過、0 失敗、1 忽略（`services/assistant/src/delegation.rs` 原本就標成 `ignore` 的文件範例）；含 `tools/sql-check` 的「SQL 不由字串組成」檢查與 `cargo test -p openapi`、`-p migrate`、`-p repo-check` |
| `cargo test -p assistant` | 單元 57、`agent` 10、`agent_logs` 2、`api` 7、`chart_connections` 1、`conversation_threads` 11、`cross_tenant` 2、`delegated_tools` 13、`history_store` 5、`purge_job` 1、`startup` 1、`telemetry_sql` 2、`tools` 7，共 119 個全部通過 |
| `cargo test -p testkit` | 單元 42、`container_volumes` 2、`modbus_endpoint` 20、`modbus_example` 1、`postgres_isolation` 6、`postgres_start` 2，全部通過（單元測試含新的 `every_token_of_a_run_names_its_user` 與 `owned` 的巨集檢查）；其他服務的跨租戶測試（identity、assets、telemetry-query、ingest、certs）在 testkit 改了每個探測的使用者之後也都通過 |
| `cargo run -p openapi -- --check` | 通過（`api/openapi.json` 多了 assistant 的 7 個 operation 與它們的 schema） |
| `cargo deny --locked check` | advisories ok, bans ok, licenses ok, sources ok |
| 前端 `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist` | 全部通過；`test` 218 通過、0 失敗（含新訊息鍵的 zh-TW 與 en 翻譯） |
| `bun test deploy/chart` | 659 通過、0 失敗 |
| `bun test deploy/flux` | 78 通過、0 失敗（它以這個分支的 chart 渲染，連線預算的改變也在內） |
| `bun test scripts` | 1704 通過、9 略過、0 失敗（略過的是要 `TRUSTIOT_DEV_E2E` 的 podman 端對端測試） |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun scripts/ci/check-secret-handling.ts` | 沒有違規 |
| `bun scripts/ci/check-workflows.ts` | 沒有違規 |
| gitleaks（分支的 commit、`git archive HEAD` 的樹） | 都沒有發現 |

### 跨租戶測試的報告

`cargo test -p assistant --test cross_tenant -- --nocapture`：`tenants_are_isolated` 79 個探測、0 個失敗。四張表在兩個租戶各種 5 列，對話的擁有者是同一個人（testkit 的 `owned`）。每個探測的 token 也是這個人，他是兩個租戶的 admin，所以兩個租戶的對話只靠租戶分開。

- **覆蓋檢查**：服務角色碰得到的每個關聯、API 的每個 operation 都有宣告。
- **資料庫探測**：每張表的讀取、沒有租戶的讀取、改自己的列、把列搬到對方、插入對方，結果是只看得到自己的列，或被拒（`42501`）。
- **端點探測**：清單、讀取、PATCH（改名加封存）、DELETE、開始對話。
  - 以對方的 ID 請求都回 404，回應裡沒有對方的任何 marker。
  - 沒有租戶的請求都回 403。
  - 正向對照：自己的清單列出全部 5 個；讀取、PATCH、DELETE 回 200；開始對話回 201。
  - 每一輪探測前後，對方的列都沒有改變（40 列與 42 列）。
- **已刪除清單與還原**：suite 種不出已刪除的對話，所以由 `deleted_conversations_stay_in_their_tenant` 手動探測，同樣是同一個人在兩個租戶：
  - 在租戶 A，已刪除清單只有 A 的對話，還原 B 的對話與還原不存在的 ID 逐位元組相同（404）。
  - 沒有租戶的請求都回 403。
  - 在租戶 B 列得出、也還原得了 B 的對話。

### 清除只在一個副本執行

`cargo test -p assistant --test purge_job -- --nocapture`（`one_replica_purges_at_a_time`）：

1. 測試以 superuser 加了一個觸發器，讓第一個行程的清除停在 DELETE 裡（`pg_sleep` 等測試開閘，不靠固定時間）。這時唯一的 advisory lock 在 `assistant_app` 的連線上。
2. 第二個行程記下 DEBUG `scheduled job skipped; another replica holds its lock`（`job: purge`），沒有開任何平台交易。
3. 開閘後，第一個行程刪掉過期的對話連同訊息，記下 INFO `purged expired conversations`，`purged` 為 1。日誌沒有對話 ID 與租戶。
4. 兩個行程的 readiness 都回 200。

### 資料庫錯誤

`cargo test -p assistant --test api a_database_failure -- --nocapture`：測試加的唯一索引只對一個固定的測試使用者生效，讓他第二次開始對話時以 `23505` 失敗。

- 測試先確認 Postgres 自己對這個錯誤的訊息與 DETAIL 含有 `duplicate key`、`violates unique constraint`、`already exists`、`Key (` 與索引名稱。
- API 回 `503 {"code":"unavailable","message_key":"error.unavailable"}`，回應與服務的日誌（stdout、stderr）都沒有上述任何一段。
- ERROR `conversation request failed` 的 `error` 欄位只有 `database error (SQLSTATE 23505)`。
- 沒有多存任何東西。

## 結論

| 驗收條件 | 證據 | 結果 |
| --- | --- | --- |
| 同租戶的使用者 B（含租戶管理員）列不到 A 的對話；以 ID 讀、改名、封存、刪除、還原都回 404，與不存在的 ID 相同，資料不變（真的服務與拋棄式 Postgres） | `tests/api.rs` 的 `another_user_of_the_tenant_gets_what_a_missing_id_gets`：viewer、editor、admin 的三個清單都是空的。對擁有者的一般、封存、已刪除對話，每個以 ID 的請求都與隨機 UUID 的回應逐位元組相同（404），每一列與請求前相同。store 層另有 `tests/conversation_threads.rs` 的 `another_user_of_the_tenant_finds_nothing` | 通過 |
| 跨租戶負向測試宣告每個新端點並通過 | `tests/cross_tenant.rs` 的 `tenants_are_isolated`（79 個探測、0 個失敗，見上）與 `deleted_conversations_stay_in_their_tenant` | 通過 |
| `GET` 只有提問與回答的文字（以主 agent 跑過工具的對話測） | `tests/api.rs` 的 `a_conversation_shows_only_its_questions_and_answers`：真的主 agent 經替身模型思考、呼叫工具、拿到結果再回答。回應只有問題與回答的文字；thinking、工具輸入、工具結果、工具定義、租戶脈絡與 system prompt 的標記都不在回應裡，但都確實在資料庫裡 | 通過 |
| 改名、封存、取消封存與分頁符合 README | `tests/api.rs` 的 `rename_archive_and_the_pages_of_the_list`：去空白、400 與 422 的各種情況、`archived=true` 與預設的清單；12 個對話（其中 5 個最後活動時間相同）以每頁 5 個走完 3 頁，不重複也不遺漏；錯的 `limit`、別的清單或改寫過的 cursor 回 400。另有 `src/api.rs` 與 `src/api/conversations.rs` 的單元測試 | 通過 |
| 刪除、已刪除清單、30 天內還原、超過 30 天回 404、清除後到期的對話與訊息都不在資料庫 | `tests/api.rs` 的 `delete_the_deleted_list_and_restore`：刪除後離開清單、讀取回 404、出現在已刪除清單（`restorable_until` 是 30 天後）；還原後讀取與每一列都和刪除前相同；有提問進行中時刪除回 409、資料不變；`deleted_at` 改到 31 天前的對話不在已刪除清單、還原回 404；一輪清除後它連同訊息都不在，最近刪除的與沒刪除的都還在。store 層另有 `tests/conversation_threads.rs` 的 `expired_conversations_are_purged_and_nothing_else`、`a_running_question_keeps_its_conversation`、`a_question_start_and_a_delete_run_one_after_the_other`（兩種順序各一次，各只有一個結果） | 通過 |
| 兩個服務行程共用一個資料庫時，清除同時只有一個在執行 | `tests/purge_job.rs` 的 `one_replica_purges_at_a_time`（兩個 OS 行程，見上） | 通過 |
| 沒有身分標頭、簽章錯誤、受眾不是 assistant 的 token 被拒 | `tests/api.rs` 的 `a_request_needs_a_token_for_the_assistant_and_a_tenant`：每個端點在沒有 token、別的簽發者、給 assets 的 token 時都回 401，沒有租戶時回 403，都帶 `cache-control: no-store`，什麼都沒建立 | 通過 |
| 資料庫錯誤的回應與日誌不含 Postgres 的錯誤文字 | `tests/api.rs` 的 `a_database_failure_shows_nothing_of_the_database`（唯一鍵衝突，見上） | 通過 |
| `cargo run -p openapi -- --check`、`bun run --cwd frontend api:check`、`bun run --cwd frontend test`、`cargo test -p assistant`、`cargo test -p openapi`、`bun test deploy/chart`、`cargo clippy --workspace --all-targets -- -D warnings` 通過 | 上方「結果」表 | 通過 |
