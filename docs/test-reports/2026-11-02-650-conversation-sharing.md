# AI 助理：把對話分享給整個租戶與複製（2026-11-02）

Laysi/trustiot-v2#650：擁有者分享與取消分享對話；同租戶的其他成員在「分享給我的」清單看到它、唯讀看問題與回答、複製成自己的新對話；分享的對話的儀表板草稿，讀者唯讀看得到，資料集以讀者自己的身分執行（Laysi/trustiot-v2#150 在 2026-10-26 的決定）。這份報告是驗收條件的證據：全部由下列自動化測試以 `CI=1` 實際執行，需要容器的測試沒有略過。

## 環境

- **程式碼**：分支 `feat/650-conversation-sharing`，起點 main `5d7bcd55`，之後依序合併 main `2e882e56`、`9ab0e4d2`、`2b29ffdd` 與 `e3bce49d`（最後一次的合併 commit 是 `c0fb5039`）。合併 `9ab0e4d2` 時 migration 改名為 `0005_conversation_sharing.sql`（main 先用了 `0004`）。「結果」表的數字取自 `c0fb5039` 的樹：cargo 的檢查在它合併前的樹 `cb6e1442` 跑（兩者只差 `frontend/src/api/conversations.ts` 的 `shared_at`，見下），各 crate 的測試數在 `c0fb5039` 重跑；本機的檢查在 `c0fb5039` 的樹。之後只改了這份報告。合併前在 `d2ecdadd` 的樹上跑過一次同樣的檢查：`cargo test --workspace` 唯一的失敗是 `tools/sql-check` 的 `the_workspace_has_no_string_built_sql`（`store/sharing.rs` 把 SQL 經變數交給 sqlx），在 `c3960dbe` 修正。合併 `e3bce49d` 後前端 `typecheck` 失敗一次：Laysi/trustiot-v2#1061 的對話頁把 zod schema 綁在產生的契約型別上，這個分支的 `Conversation` 與 `ConversationDetail` 多了 `shared_at`，所以在合併 commit 補上這個可省略的欄位。
- **機器**：
  - 建置主機（Linux aarch64 VM，12 核）：所有 cargo 的建置、clippy 與測試。容器測試以 podman 起拋棄式 TLS Postgres，以 migration Job 套用整個產品的 schema（`testkit::product`）；`tests/api.rs`、`tests/cross_tenant.rs`、`tests/permissions.rs` 與 `tests/conversation_sharing.rs` 的部分測試啟動 `assistant` 執行檔本身當子行程。
  - 本機：`cargo fmt`、前端、`scripts`、chart 的檢查與 gitleaks。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的版本：Rust 1.99.0、Bun 1.4.2；測試的 Postgres 是 `postgres:18.6-alpine`（以 digest 釘選，`crates/service-kit/tests/support/start.rs`）。
- **設定**：`CI=1`，沒有容器執行環境時測試會失敗、不會略過。不連真正的 AI 供應商：複製後的提問以 loopback 上的 AI 閘道替身回答；身分服務的 token 交換與 telemetry-query 也是 loopback 上的替身（`tests/dashboard_world`），記下收到的 token。
- **負載**：建置主機同時跑著其他工作，測試都等條件成立、上限放寬，不等固定的時間。

## 指令

建置主機上，在 repo 根目錄（`CI=1`）：

```sh
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p assistant --no-fail-fast
cargo test --locked -p migrate
cargo test --locked -p openapi
cargo run --locked -p openapi -- --check
cargo test --locked -p repo-check
cargo test --locked --workspace --no-fail-fast
cargo deny --locked check
```

本機，在 repo 根目錄：

```sh
cargo fmt --all --check
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun run --cwd frontend build
bun run --cwd frontend check:dist
bun install --cwd scripts --frozen-lockfile
bun test scripts
bun run --cwd scripts typecheck
bun test deploy/chart
gitleaks git --redact --log-opts="origin/main..HEAD" .
git archive HEAD | tar -x -C <空目錄> && gitleaks dir --redact <空目錄>
```

單獨重跑本 issue 的測試：`cargo test -p assistant --test conversation_sharing --test shared_dashboards --test cross_tenant --test permissions -- --nocapture`。

## 結果

| 指令 | 機器 | 結果 |
| --- | --- | --- |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 建置主機 | 通過，沒有警告 |
| `cargo test --locked -p assistant --no-fail-fast` | 建置主機 | 294 通過、0 失敗、3 略過（`tests/dashboard_acceptance.rs` 的 2 個需要檢查服務的映像，預設 `#[ignore]`；1 個 doc test） |
| `cargo test --locked -p migrate` | 建置主機 | 175 通過、0 失敗 |
| `cargo test --locked -p openapi` | 建置主機 | 17 通過、0 失敗 |
| `cargo run --locked -p openapi -- --check` | 建置主機 | `api/openapi.json is up to date` |
| `cargo test --locked -p repo-check` | 建置主機 | 37 通過、0 失敗 |
| `cargo test --locked --workspace --no-fail-fast` | 建置主機 | 3101 通過、0 失敗、11 略過（預設 `#[ignore]` 的測試與 doc test） |
| `cargo deny --locked check` | 建置主機 | advisories、bans、licenses、sources 都是 ok |
| `cargo fmt --all --check` | 本機 | 通過 |
| `bun run --cwd frontend api:check`、`typecheck`、`lint`、`build`、`check:dist` | 本機 | 都通過 |
| `bun run --cwd frontend test` | 本機 | 955 通過、0 失敗 |
| `bun test scripts` | 本機 | 2024 通過、9 略過、0 失敗 |
| `bun run --cwd scripts typecheck` | 本機 | 通過 |
| `bun test deploy/chart` | 本機 | 845 通過、0 失敗 |
| gitleaks（分支的 commit 與 `git archive HEAD` 的樹） | 本機 | 兩者都沒有發現 |

`cargo test -p assistant` 裡與本 issue 有關的測試執行檔：

| 測試執行檔 | 通過 |
| --- | --- |
| 單元測試（`src/`） | 148 |
| `tests/conversation_sharing.rs` | 8 |
| `tests/shared_dashboards.rs` | 3 |
| `tests/cross_tenant.rs` | 4 |
| `tests/permissions.rs` | 2 |
| `tests/api.rs` | 10 |
| `tests/dashboards_api.rs` | 25 |
| `tests/conversation_threads.rs` | 13 |
| `tests/dashboards_migration.rs` | 1 |

### 「分享給我的」清單的查詢計畫

`0005_conversation_sharing.sql` 的部分索引 `conversations_shared` 是給清單用的。2026-11-03 在建置主機的拋棄式 Postgres 以一個暫時的測試量測（沒有留在 repo）：兩個租戶各 20,000 個對話，其中每 50 個分享 1 個，以服務角色在租戶交易裡對 `store::list_shared` 的兩個查詢執行 `EXPLAIN (ANALYZE, BUFFERS, COSTS OFF, TIMING OFF, SUMMARY OFF)`。第一頁與後面的一頁都是 `conversations_shared` 的 Index Scan，沒有排序；後面的一頁的 cursor 是一個列比較，是索引條件的一部分：

```text
== first page
Limit (actual rows=51.00 loops=1)
  Buffers: shared hit=42
  ->  Result (actual rows=51.00 loops=1)
        One-Time Filter: ((NULLIF(current_setting('trustiot.tenant_id'::text, true), ''::text))::uuid = '<租戶>'::uuid)
        ->  Index Scan using conversations_shared on conversations (actual rows=51.00 loops=1)
              Index Cond: (tenant_id = '<租戶>'::uuid)
              Filter: (user_id <> '<呼叫者>'::uuid)
              Index Searches: 1
              Buffers: shared hit=42
== a later page
Limit (actual rows=51.00 loops=1)
  Buffers: shared hit=42
  ->  Result (actual rows=51.00 loops=1)
        One-Time Filter: ((NULLIF(current_setting('trustiot.tenant_id'::text, true), ''::text))::uuid = '<租戶>'::uuid)
        ->  Index Scan using conversations_shared on conversations (actual rows=51.00 loops=1)
              Index Cond: ((tenant_id = '<租戶>'::uuid) AND (ROW(last_activity_at, conversation_id) < ROW('<cursor 的時間>'::timestamp with time zone, '<cursor 的 ID>'::uuid)))
              Filter: (user_id <> '<呼叫者>'::uuid)
              Index Searches: 1
              Buffers: shared hit=42
```

兩頁讀的 buffer 一樣多：後面的頁從 cursor 指的位置開始讀，不再先掃過前面的列。2026-10-30 的第一次量測，cursor 寫成 `last_activity_at < $3 OR (last_activity_at = $3 AND conversation_id < $4)`，是索引掃描之後的 filter（`Rows Removed by Filter: 99`）；安全審查的 I6 之後分成第一頁與 cursor 之後的兩個查詢（`0c9d12e1`），分頁的行為由 `the_shared_list_pages_in_activity_order` 驗證。再更早，`ORDER BY` 寫的是輸出欄位 `conversation_id`（`conversation_id::text` 的別名），Postgres 依文字排序，計畫多一個 Incremental Sort；改成表格的欄位 `conversations.conversation_id` 之後就沒有排序。大量分享時的量測是另開的 P2 驗證（見 Laysi/trustiot-v2#650 的「不包含」）。

## 安全審查的修正與測試

| 項目 | 修正 | 測試 |
| --- | --- | --- |
| M1：上一版的服務在滾動更新時刪除已分享的對話，會被 CHECK 擋下（`23514`） | `db863677`：`BEFORE UPDATE OF deleted_at` 觸發器 `conversations_unshare_deleted` 在設定 `deleted_at` 時清掉 `shared_at`；CHECK 保留；`store::delete` 仍自己清 `shared_at` | `the_database_holds_the_sharing_rules`：以服務角色執行上一版（main `5d7bcd55`）一字不差的刪除語句，對話被刪除且取消分享；直接分享已刪除的對話仍回 `23514` |
| L1：副本把別人的問答當成複製者自己的回合交給模型 | `76ec8fd9`：最後一個問題與它的回答之間加一則對話中的 system 訊息（`prompt::COPIED_ROUNDS`，存成 note），說明前面的回合是從別的成員的對話複製、以對方的權限回答、只供參考 | `a_copy_is_the_readers_own_and_carries_only_the_texts`：存下的副本與複製者下一次的模型請求都有這則訊息，位置在問題之後、回答之前；聊天畫面與讀者看不到它；thinking、工具呼叫與結果都沒有帶過去 |
| L3：讀者被拒絕的寫入先等擁有者對草稿列的鎖 | `23c9c075`：發布、還原與封存先以不鎖的查詢判斷可見性與權限，拒絕時不鎖任何列 | `tests/shared_dashboards.rs::a_readers_refused_write_never_waits_for_the_drafts_row`：另一個交易以 `FOR UPDATE` 持有草稿列時，讀者的還原與發布立即回 `OwnerRequired`（API 是 403），擁有者的還原則等這個鎖 |
| I2：`MAX_HISTORY_BYTES` 的說明 | `0c9d12e1`：寫明這個分支只用它擋複製，提問的上限屬於 Laysi/trustiot-v2#646 | 文件 |
| I6：清單的 cursor 不是索引條件 | `0c9d12e1`：cursor 改成列比較，見上方的查詢計畫 | `the_shared_list_pages_in_activity_order`：七個分享的對話、最後活動時間有相同的，以每頁 2 個走完，每個只出現一次、順序正確，不含自己的、沒分享的與別的租戶的 |

## 結論

| 驗收條件 | 測試 | 結果 |
| --- | --- | --- |
| 1. 沒分享時，同租戶的 B（含租戶管理員）在分享清單看不到 A 的對話，唯讀檢視與複製都回 404 | `tests/conversation_sharing.rs::a_conversation_not_shared_is_nobody_elses` | 通過 |
| 2. A 分享後，B 在清單看到它（分頁見 `the_shared_list_pages_in_activity_order`），唯讀檢視只有問答（以含 thinking、工具呼叫與結果的對話測）；B 呼叫擁有者的路由都是 404、資料不變；別的租戶一律 404 | `tests/conversation_sharing.rs::a_shared_conversation_shows_its_readers_only_the_questions_and_answers`、`the_shared_list_pages_in_activity_order`、`tests/cross_tenant.rs::shared_conversations_stay_in_their_tenant` | 通過；提問與重新產生的端點還不存在（Laysi/trustiot-v2#646 尚未合併），測的是讀完整對話、改名、封存、刪除、還原、分享與取消分享 |
| 3. 取消分享後讀不到；刪除後讀不到（上一版的刪除語句也是），30 天內還原後沒有分享 | `tests/conversation_sharing.rs::unsharing_and_deleting_take_a_conversation_from_its_readers`、`the_database_holds_the_sharing_rules` | 通過 |
| 4. B 的副本屬於 B；B 提問時閘道替身只收到問答的文字與標示複製的 system 訊息，run 委派以 B 的身分開始；A 取消分享、刪除並清除原對話後副本仍完整 | `tests/conversation_sharing.rs::a_copy_is_the_readers_own_and_carries_only_the_texts` | 通過；提問的 API 還沒有（Laysi/trustiot-v2#646），測試以服務同樣的 `Agent` 與 `QuestionRuns` 在新對話提問 |
| 5. 同租戶的 B（viewer、editor、admin）看得到草稿、以自己的身分執行資料集（替身 telemetry-query 看到的是 B），發布與還原回 403 且資料不變、不等草稿列的鎖；取消分享或刪除後每個路由與不存在的相同（404）；別的租戶一律 404 | `tests/shared_dashboards.rs::readers_of_a_shared_conversation_see_its_drafts_read_only_and_run_them_as_themselves`、`unsharing_and_deleting_hide_the_drafts_from_readers_as_if_they_did_not_exist`、`a_readers_refused_write_never_waits_for_the_drafts_row` | 通過 |
| 6. 分享、取消分享、複製的日誌不含標題、問題與回答 | `tests/conversation_sharing.rs::sharing_logs_carry_no_content` | 通過 |
| 7. 指定的檢查都通過 | 上方「結果」 | 通過 |

另外驗證：複製超過 16 MiB 時回 409 `conversation.copy_too_large`、在超過處就停止讀取、什麼都不存，超過每日上限回 429（`a_copy_over_a_limit_stores_nothing`）；權限矩陣的 21 個 operation（`tests/permissions.rs`）；跨租戶的 suite 宣告分享、取消分享與複製（`tests/cross_tenant.rs::tenants_are_isolated`）。
