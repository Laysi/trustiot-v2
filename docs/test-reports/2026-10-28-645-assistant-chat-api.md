# AI 助理：對話串 API 與排程清除（2026-10-28）

Laysi/trustiot-v2#645：AI 助理的 API 骨架（`/api/assistant/v1`）、對話串的七個端點（清單、開始、讀取、改名與封存、刪除、已刪除清單、還原），以及刪除滿 30 天的對話的排程清除（每個副本都排程，同一時間一個副本以平台角色 `assistant_platform` 分批刪除）。這份報告是驗收條件的證據：全部由下列自動化測試以 `CI=1` 實際執行，需要容器的測試沒有略過。

2026-10-28 第一次執行之後，獨立安全審查提出的修正（下方「審查後的修正」，F1 到 F7）在 2026-10-29 加入，所有指令都在修正後的樹上重跑；下面的數字都是重跑的結果。複審之後又修了清除的兩個 Low 項目（下方「複審後的修正」，R1、R2），`cargo test -p assistant` 等相關的檢查在那個 commit 上重跑。

## 環境

- **程式碼**：分支 `feat/645-assistant-chat-api`，從 main `5dad042` 分出；完整的數字取自 commit `095e552` 的樹。之後的 `2f53f8f` 與這份報告最後的修改只改文件（這份報告、`docs/architecture/data-retention.md` 的一個標點、`deploy/chart/README.md` 的一個數字）；合併 commit `c262825` 帶進 main，在它的樹上重跑的檢查見「結果」表下方。R1、R2 的修正是 `dcbaefe`，在它的樹上重跑的檢查也在那裡。
- **機器**：本機（x86_64 Linux 工作站）。容器測試以 podman 起拋棄式 TLS Postgres，以 migration Job 套用整個產品的 schema（`testkit::product`，每個測試執行檔一個資料庫）。API 與清除的測試啟動 `assistant` 執行檔本身當子行程（`services/assistant/tests/service/mod.rs`），清除的測試同時起兩個行程。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的版本：Rust 1.99.0、Bun 1.4.2、Helm 4.3.0、kubeconform 0.8.0、cargo-deny 0.20.2、gitleaks 8.30.1。
- **設定**：`CI=1`，所以沒有容器執行環境時測試會失敗，不會略過。不連真正的 AI 供應商：主 agent 的模型是 loopback 上回放 `tests/fixtures/` 的替身，服務設定的 AI 閘道位址沒有東西在聽（這些測試都不發問）。
- **負載**：機器同時跑著其他工作，負載很高，所以測試都等條件成立、上限放寬，不等固定的時間。

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
| `CI=1 cargo test --workspace --no-fail-fast` | 242 個測試執行檔與文件測試，2062 通過、0 失敗、1 忽略（`services/assistant/src/delegation.rs` 原本就標成 `ignore` 的文件範例），1837 秒；含 `tools/sql-check` 的「SQL 不由字串組成」檢查與 `cargo test -p openapi`、`-p migrate`、`-p repo-check` |
| `cargo test -p assistant`（同一次執行） | 單元 57、`agent` 10、`agent_logs` 2、`api` 10、`chart_connections` 1、`conversation_threads` 12、`cross_tenant` 2、`delegated_tools` 13、`history_store` 5、`purge_job` 1、`startup` 1、`telemetry_sql` 2、`tools` 7，共 123 個全部通過；文件測試 1 個忽略（同上） |
| `cargo test -p testkit`（同一次執行） | 單元 42、`container_volumes` 2、`modbus_endpoint` 20、`modbus_example` 1、`postgres_isolation` 6、`postgres_start` 2，全部通過；其他服務的跨租戶測試（identity、assets、telemetry-query、ingest、certs）也都通過 |
| `cargo run -p openapi -- --check` | 通過（`api/openapi.json` 有 assistant 的 7 個 operation 與它們的 schema；刪除是 204，開始對話有 429 與 `retry-after` 標頭） |
| `cargo deny --locked check` | advisories ok, bans ok, licenses ok, sources ok |
| 前端 `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist` | 全部通過；`test` 218 通過、0 失敗（含七個新訊息鍵的 zh-TW 與 en 翻譯） |
| `bun test deploy/chart` | 659 通過、0 失敗 |
| `bun test deploy/flux` | 78 通過、0 失敗 |
| `bun test scripts` | 1704 通過、9 略過、0 失敗（略過的是要 `TRUSTIOT_DEV_E2E` 的 podman 端對端測試） |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun scripts/ci/check-secret-handling.ts` | 沒有違規（537 個檔案、7 個容器建置檔） |
| `bun scripts/ci/check-workflows.ts` | 沒有違規 |
| gitleaks（分支的 commit、`git archive HEAD` 的樹） | 都沒有發現 |

**合併 main 後重跑**：合併 commit `c262825`（main 的 `fdf7bd0`，帶進 Laysi/trustiot-v2#722 的 edge/agent 與 Laysi/trustiot-v2#725 的 AI 閘道管理埠 Service 與 mesh 政策）的樹上，重跑 main 的變更可能影響的檢查：`cargo fmt --all --check` 與 `cargo clippy --workspace --all-targets -- -D warnings` 通過，`cargo run -p openapi -- --check` 通過，`CI=1 cargo test -p repo-check` 28 通過，`bun test deploy/flux` 78 通過，`bun test deploy/chart` 680 通過（多出的是 #725 的 chart 測試），都是 0 失敗。上表其他的數字仍是 `095e552` 的。

**R1、R2 的修正後重跑**（`dcbaefe` 的樹）：`cargo fmt --all --check` 與 `cargo clippy --workspace --all-targets -- -D warnings` 通過；`CI=1 cargo test -p assistant --no-fail-fast` 單元 57、`agent` 10、`agent_logs` 2、`api` 10、`chart_connections` 1、`conversation_threads` 12、`cross_tenant` 2、`delegated_tools` 13、`history_store` 5、`purge_job` 1、`purge_locks` 2（新的）、`startup` 1、`telemetry_sql` 2、`tools` 7，共 125 個全部通過，文件測試 1 個忽略；跑了 179 秒。

### 新的常數

| 常數 | 值 | 用途 |
| --- | --- | --- |
| `store::PURGE_ROW_LOCK_TIMEOUT` | 1 秒 | 一批清除等不到列鎖、改成一次刪一個時，每個對話等鎖的上限（F2） |
| `api::REQUEST_LOCK_TIMEOUT` | 2 秒 | 每個 API 請求的交易設的 `lock_timeout`（F5） |
| `store::MAX_STARTS_PER_DAY` | 100 | 每個使用者在每個租戶，一段時間內最多開始的對話數（F3） |
| `store::START_WINDOW_HOURS` | 24 | 上一列的時間範圍，以資料庫的時鐘算（F3） |

原本的 `store::PURGE_LOCK_TIMEOUT`（一批清除等鎖 5 秒）不變。

### 跨租戶測試的報告

`CI=1 cargo test -p assistant --test cross_tenant -- --nocapture`（在 `095e552` 的樹上另外跑一次）：`tenants_are_isolated` 79 個探測、0 個失敗。四張表在兩個租戶各種 5 列，對話的擁有者是同一個人（testkit 的 `owned`）。每個探測的 token 也是這個人，他是兩個租戶的 admin，所以兩個租戶的對話只靠租戶分開。

- **覆蓋檢查**：服務角色碰得到的每個關聯、API 的每個 operation 都有宣告。
- **資料庫探測**：每張表的讀取、沒有租戶的讀取、改自己的列、把列搬到對方、插入對方，結果是只看得到自己的列，或被拒（`42501`）。
- **端點探測**：清單、讀取、PATCH（改名加封存）、DELETE、開始對話。
  - 以對方的 ID 請求都回 404，回應裡沒有對方的任何 marker。
  - 沒有租戶的請求都回 403。
  - 正向對照：自己的清單列出全部 5 個；讀取與 PATCH 回 200，DELETE 回 204；開始對話回 201。
  - 每一輪探測前後，對方的列都沒有改變（40 列與 42 列）。
- **已刪除清單與還原**：suite 種不出已刪除的對話，所以由 `deleted_conversations_stay_in_their_tenant` 手動探測，同樣是同一個人在兩個租戶：
  - 在租戶 A，已刪除清單只有 A 的對話，還原 B 的對話與還原不存在的 ID 逐位元組相同（404）。
  - 沒有租戶的請求都回 403。
  - 在租戶 B 列得出、也還原得了 B 的對話。

### 清除只在一個副本執行

`cargo test -p assistant --test purge_job -- --nocapture`（`one_replica_purges_at_a_time`）：

1. 測試以 superuser 加了一個觸發器，讓第一個行程的清除停在 DELETE 裡（`pg_sleep` 等測試開閘，不靠固定時間）。這時唯一的 advisory lock 在 `assistant_app` 的連線上。
2. 第二個行程記下 DEBUG `scheduled job skipped; another replica holds its lock`（`job: purge`），沒有開任何平台交易。
3. 開閘後，第一個行程刪掉過期的對話連同訊息，記下 INFO `purged expired conversations`，`purged` 為 1、`skipped` 為 0。
4. 這個對話有一個帶標記的標題（`PURGED-TITLE-MARKER-7c41`，測試先確認它確實存進去）；兩個行程的 stdout 與 stderr 都沒有它的 ID、租戶與標題。
5. 兩個行程的 readiness 都回 200。

### 資料庫錯誤

`cargo test -p assistant --test api a_database_failure -- --nocapture`：測試加的唯一索引只對一個固定的測試使用者生效，讓他第二次開始對話時以 `23505` 失敗。

- 測試先確認 Postgres 自己對這個錯誤的訊息與 DETAIL 含有 `duplicate key`、`violates unique constraint`、`already exists`、`Key (` 與索引名稱。
- API 回 `503 {"code":"unavailable","message_key":"error.unavailable"}`，回應與服務的日誌（stdout、stderr）都沒有上述任何一段。
- ERROR `conversation request failed` 的 `error` 欄位只有 `database error (SQLSTATE 23505)`。
- 沒有多存任何東西。
- 索引以 `CONCURRENTLY` 建立與刪除，見下方 F6。

## 審查後的修正

每一項都有測試，沒有那項修正時測試會失敗。F3 與 F6 另外把修正拿掉、實際跑過，結果寫在該項。

### F1：刪除回 204

`DELETE /conversations/{id}` 回 204、沒有本文、沒有 `content-type`，仍帶 `cache-control: no-store`，INFO `conversation deleted` 不變。可以還原到什麼時候改由已刪除清單的 `restorable_until` 提供。

- `tests/api.rs` 的 `delete_the_deleted_list_and_restore`：刪除回 204、本文是空的，已刪除清單的 `restorable_until` 是刪除後 30 天；其他測試的每個刪除也都預期 204。
- `tests/cross_tenant.rs` 的正向對照：自己的列刪除回 204（見上）。

### F2：一筆被鎖住的過期對話不讓清除停擺

`tests/conversation_threads.rs` 的 `expired_conversations_are_purged_and_nothing_else`：兩個過期的對話（刪除於 32 天前與 31 天前），較舊的那個整輪都被另一個工作階段 `FOR UPDATE` 鎖住。

- 整批刪除在 `PURGE_LOCK_TIMEOUT`（5 秒）之後以 `55P03` 失敗、什麼都沒刪。
- `purge::round` 改成一次刪一個，每個等鎖最多 `PURGE_ROW_LOCK_TIMEOUT`（1 秒）：較新的被刪、較舊的留著，回傳 `Round { purged: 1, skipped: 1 }`。日誌一行 WARN `purged expired conversations; some stayed locked and wait for the next round`，欄位只有 `job`、`purged`、`skipped`，沒有 ID 與租戶。
- 略過清單讓之後的候選不再選到它。
- 鎖放開後，下一輪刪掉它，回傳 `Round { purged: 1, skipped: 0 }`，記 INFO。
- 平台角色的 `SKIP LOCKED` 仍被拒（`42501`），權限沒有改。

### F3：開始對話的上限

每個使用者在每個租戶 24 小時內最多開始 100 個對話（`MAX_STARTS_PER_DAY`、`START_WINDOW_HOURS`），已刪除的也算。

`tests/api.rs` 的 `starting_conversations_is_limited_per_user_and_day`：先種一小時前開始的 99 個（其中 40 個已刪除），再開始 1 個。

- 再開始回 `429 {"code":"rate_limited","message_key":"conversation.start_limited","details":{"max":100}}`。
- 標頭：`cache-control: no-store`、`content-type: application/json`、沒有 `Location`、`retry-after: 82800`（最舊的一小時前開始，23 小時後離開 24 小時）。
- 什麼都沒存。
- 日誌一行 WARN `conversation start refused: the user reached the daily limit`，欄位 `user_id`、`tenant_id`、`action`、`max`（100）、`retry_after_secs`；每次被拒一行。
- 刪除一個之後仍是 429。
- 同租戶的其他使用者、同一個使用者在別的租戶都能開始。
- 把一個的開始時間移到 24 小時又 1 分鐘前，就能再開始一個，接著又是 429。

`tests/api.rs` 的 `concurrent_starts_stop_at_the_limit`：已有 95 個時，24 個請求同時開始。

- 結果是 `{201: 5, 429: 19}`，存了剛好 100 個。
- 把 `create` 的 advisory lock 拿掉、其他不變時，同一個測試連跑三次都存了 101 個（`{201: 6, 429: 18}`），測試失敗。

`tests/conversation_threads.rs` 的 `starts_stop_at_the_daily_limit_but_a_retry_is_not_a_start`：

- store 層到上限時，`create` 回 `StartLimited`（`retry_after_secs` 在 22 到 23 小時之間），什麼都不存。
- 重試已存的同一個對話不算新的，照樣成功。把「已存的不算」拿掉時，這個測試失敗（`its retry: the user started too many conversations today`）。

**查詢計畫**：在測試資料庫，以服務角色的租戶交易對計數的查詢跑 `EXPLAIN (ANALYZE, BUFFERS)`。資料是兩個租戶各四萬列、各兩百個使用者，跑過 `ANALYZE`。

- 沒刪除的與已刪除的兩個計數，分別走 `conversations_by_activity` 與 `conversations_deleted` 的 Index Scan。
- `Index Cond` 都是 `tenant_id` 與 `user_id`；`created_at` 是 Filter，篩掉這個使用者 24 小時前的 120 列與 60 列。
- 共讀 209 個 shared buffer，沒有讀到別的使用者的列。

所以沒有加索引；計數的成本只隨這個使用者自己存著的對話數增加。這是一次性的量測，沒有放進測試。

### F4：標題規則

只有一份規則 `store::title_is_valid`，API 與 `set_title` 共用。`src/api/conversations.rs` 的 `updates_need_a_field_and_a_valid_title` 與 `tests/conversation_threads.rs` 的 `titles_archive_and_the_list`（使用者、模型、暫定三種來源）都測：

- 拒絕只有不顯示字元的標題：U+200B、U+FEFF、U+00AD、U+2060、U+200E、U+061C 或它們的組合，補充變體選擇字元（U+E0100，U+E01EF 加 U+FE0F），點字空白 U+2800，U+1D173、U+FFF9、U+0600。
- 拒絕含 U+202A、U+202E、U+2066 到 U+2069、U+206A、U+2028、U+2029 或控制字元的標題。
- 接受：含 ZWJ 的 emoji 序列、含 ZWNJ 的波斯文、看得見的文字旁的 RLM 與 LRM。
- 資料庫的 CHECK 只管長度，沒有改。

### F5：請求等鎖的上限

`tests/api.rs` 的 `a_request_that_waits_for_a_held_conversation_gives_up`：另一個工作階段以 `FOR UPDATE` 鎖住對話那一列。

- PATCH 與 DELETE 都等了至少 `REQUEST_LOCK_TIMEOUT`（2 秒），之後回 `503 unavailable` 並帶 `no-store`。
- 對話的每一列都沒變。
- 日誌兩行 ERROR `conversation request failed`，`error` 只有 `database error (SQLSTATE 55P03)`。
- 鎖放開後，同樣的 PATCH 回 200、DELETE 回 204。

### F6：資料庫失敗測試不卡住其他測試

`a_database_failure_shows_nothing_of_the_database` 的唯一索引改用 `CREATE UNIQUE INDEX CONCURRENTLY` 與 `DROP INDEX CONCURRENTLY` 建立與刪除。這兩個指令拿 SHARE UPDATE EXCLUSIVE，與任何讀寫都不衝突。

- 測試開著一個讀過這張表的交易，等到 `pg_blocking_pids` 顯示刪除在等它。
- 這時同租戶另一個使用者的讀取回 200、開始對話回 201，兩個請求共約 47 毫秒。
- 交易結束後，刪除完成。
- 把刪除改回一般的 `DROP INDEX`、其他不變時，同一個讀取在 lock timeout 後回 503，測試失敗。

### F7：404 逐位元組相同，清除的日誌沒有標題

下列每個請求的回應，都與同一個請求對隨機 UUID 的回應比對狀態、本文、`content-type` 與 `cache-control`，全部相同：

- `tests/api.rs` 的 `rename_archive_and_the_pages_of_the_list`：路徑 ID 不是 UUID 時的讀取、改名、封存、刪除與還原。
- `tests/api.rs` 的 `delete_the_deleted_list_and_restore`：已刪除對話的讀取、改名、封存與刪除；還原沒刪除的對話；還原刪除超過 30 天的對話。

清除的日誌沒有標題：見上方「清除只在一個副本執行」第 4 點。

## 複審後的修正

複審（`61f93ec..095e552`）沒有 Medium 以上的項目，清除的一次刪一個有兩個 Low 項目，修正是 `dcbaefe`。測試在新的測試執行檔 `tests/purge_locks.rs`，有自己的 Postgres：表鎖會擋住共用資料庫的其他測試，或讓 `conversation_threads.rs` 裡清除測試自己的 5 秒逾時失敗。兩個測試以一個 mutex 輪流執行，先開好平台連線池唯一的那條連線再鎖表（新連線的 session 檢查會以 `pg_get_expr` 讀表的 policy，ACCESS EXCLUSIVE 下會等鎖；這裡測的是一輪自己的敘述）。

### R2：整張表被鎖住時一輪提早結束

`a_table_lock_ends_the_round_after_one_fallback_batch`（單獨執行 14.0 秒）：三個過期的對話，superuser 以 `LOCK TABLE assistant.conversations IN SHARE MODE` 擋住所有 DELETE（讀取不擋）。

- 一輪回傳 `Round { purged: 0, skipped: 3 }`，什麼都沒刪。
- 這一輪開的平台交易是 2 + 3 個（批次、讀鍵、每個對話一次刪除），以 `platform database access` 的行數計算，不靠時間。拿掉修正（改回「有略過就算有進展」）時同一個測試數到 7 個（多了第二批與第二次讀鍵），測試失敗。
- 一行 WARN `purge round stopped early: a lock held up its deletions; the next round tries again`（`purge::STOPPED_EARLY`），欄位只有 `job`、`purged`、`skipped`，沒有租戶與 ID。
- 鎖放開後，下一輪回傳 `Round { purged: 3, skipped: 0 }`，記 INFO `purged expired conversations`。

### R1：讀候選也有等鎖上限

`an_exclusive_table_lock_does_not_hold_the_round`（單獨執行 17.5 秒）：兩個過期的對話，superuser 以 `LOCK TABLE assistant.conversations IN ACCESS EXCLUSIVE MODE` 連讀取都擋。

- 一輪在鎖還在的時候就回傳 `Round { purged: 0, skipped: 0 }`：批次與讀鍵各等 `PURGE_LOCK_TIMEOUT`（5 秒）後放棄，平台交易只有這 2 個。
- 一行 `STOPPED_EARLY` 的 WARN，只有計數。
- 鎖放開後兩個都還在；下一輪回傳 `Round { purged: 2, skipped: 0 }`，記 INFO。
- 拿掉修正（讀鍵不設 `lock_timeout`）時，一輪一直等到測試的上限 120 秒（`ROUND_LIMIT`），測試以「the round did not end while the table stayed exclusively locked」失敗。

兩個測試在同一個執行檔裡輪流跑，共 28.2 秒（`cargo test -p assistant` 那一次）。

## 結論

| 驗收條件 | 證據 | 結果 |
| --- | --- | --- |
| 同租戶的使用者 B（含租戶管理員）列不到 A 的對話；以 ID 讀、改名、封存、刪除、還原都回 404，與不存在的 ID 相同，資料不變（真的服務與拋棄式 Postgres） | `tests/api.rs` 的 `another_user_of_the_tenant_gets_what_a_missing_id_gets`：viewer、editor、admin 的三個清單都是空的。對擁有者的一般、封存、已刪除對話，每個以 ID 的請求都與隨機 UUID 的回應逐位元組相同（404），每一列與請求前相同。store 層另有 `tests/conversation_threads.rs` 的 `another_user_of_the_tenant_finds_nothing` | 通過 |
| 不存在、已刪除、別的租戶或別的使用者：一律同一個 404，回應內容相同 | 上一列，加上 F7 的比對（擁有者自己的非 UUID 路徑 ID、已刪除的對話、還原沒刪除的與刪除超過 30 天的），以及跨租戶的 `deleted_conversations_stay_in_their_tenant` | 通過 |
| 跨租戶負向測試宣告每個新端點並通過 | `tests/cross_tenant.rs` 的 `tenants_are_isolated`（79 個探測、0 個失敗，見上）與 `deleted_conversations_stay_in_their_tenant` | 通過 |
| `GET` 只有提問與回答的文字（以主 agent 跑過工具的對話測） | `tests/api.rs` 的 `a_conversation_shows_only_its_questions_and_answers`：真的主 agent 經替身模型思考、呼叫工具、拿到結果再回答。回應只有問題與回答的文字；thinking、工具輸入、工具結果、工具定義、租戶脈絡與 system prompt 的標記都不在回應裡，但都確實在資料庫裡 | 通過 |
| 改名、封存、取消封存與分頁符合 README | `tests/api.rs` 的 `rename_archive_and_the_pages_of_the_list`：去空白、400 與 422 的各種情況、`archived=true` 與預設的清單；12 個對話（其中 5 個最後活動時間相同）以每頁 5 個走完 3 頁，不重複也不遺漏；錯的 `limit`、別的清單或改寫過的 cursor 回 400。另有 `src/api.rs` 與 `src/api/conversations.rs` 的單元測試 | 通過 |
| 刪除（204）、已刪除清單、30 天內還原、超過 30 天回 404、清除後到期的對話與訊息都不在資料庫 | `tests/api.rs` 的 `delete_the_deleted_list_and_restore`：刪除回 204、沒有本文，離開清單、讀取回 404、出現在已刪除清單（`restorable_until` 是 30 天後）；還原後讀取與每一列都和刪除前相同；有提問進行中時刪除回 409、資料不變；`deleted_at` 改到 31 天前的對話不在已刪除清單、還原回 404；一輪清除後它連同訊息都不在，最近刪除的與沒刪除的都還在。store 層另有 `tests/conversation_threads.rs` 的 `expired_conversations_are_purged_and_nothing_else`、`a_running_question_keeps_its_conversation`、`a_question_start_and_a_delete_run_one_after_the_other`（兩種順序各一次，各只有一個結果） | 通過 |
| 兩個服務行程共用一個資料庫時，清除同時只有一個在執行 | `tests/purge_job.rs` 的 `one_replica_purges_at_a_time`（兩個 OS 行程，見上） | 通過 |
| 沒有身分標頭、簽章錯誤、受眾不是 assistant 的 token 被拒 | `tests/api.rs` 的 `a_request_needs_a_token_for_the_assistant_and_a_tenant`：每個端點在沒有 token、別的簽發者、給 assets 的 token 時都回 401，沒有租戶時回 403，都帶 `cache-control: no-store`，什麼都沒建立 | 通過 |
| 資料庫錯誤的回應與日誌不含 Postgres 的錯誤文字 | `tests/api.rs` 的 `a_database_failure_shows_nothing_of_the_database`（唯一鍵衝突，見上） | 通過 |
| 審查 F2 到 F6 的修正 | 上方「審查後的修正」各節 | 通過 |
| 複審 R1、R2 的修正 | 上方「複審後的修正」各節 | 通過 |
| `cargo run -p openapi -- --check`、`bun run --cwd frontend api:check`、`bun run --cwd frontend test`、`cargo test -p assistant`、`cargo test -p openapi`、`bun test deploy/chart`、`cargo clippy --workspace --all-targets -- -D warnings` 通過 | 上方「結果」表 | 通過 |
