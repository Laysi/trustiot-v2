# AI 儀表板：儲存、公開 API 與以檢視者身分執行資料集

對應 issue：Laysi/trustiot-v2#150（AI 儀表板）。測試日期 2026-10-26（Asia/Taipei）。這份報告只涵蓋 assistant 服務的後端：儀表板與版本的 Postgres 儲存（`migrations/postgres/0002_dashboards.sql`、`src/dashboards/store.rs`）、公開 API `/api/assistant/v1`（`src/api.rs`、`src/api/dashboards.rs`）與資料集的執行（`src/dashboards/runner.rs`、`caps.rs`、`flatten.rs`）。`dashboard` 工具、發布前的檢查服務、沙箱 runtime 與回答過濾不在這裡。

原始輸出在 [data/2026-10-26-150-dashboard-api/](data/2026-10-26-150-dashboard-api/)。

## 環境

- **受測程式碼**：本報告所在的 commit（分支 `feat/150-s1-backend`，由 Laysi/trustiot-v2#150 共用契約的 `3779f3b` 開出）。`cargo test -p assistant` 與 `--test cross_tenant -- --nocapture` 在這份程式與測試上跑出；其餘指令在它之前一版的工作樹上跑出，兩版之間只改了 `src/api.rs` 讀請求本文的方式（同一個欄位寫兩次時拒絕）與 `tests/dashboards_api.rs` 對應的案例，`clippy -p assistant` 與 `fmt` 在改後重跑。新的案例先在改之前的程式上跑過、確認會失敗。
- **執行的地方**：開發工作站。容器由測試自行啟動（測試會偵測 podman 或 docker），一次只跑一個起容器的測試程式；沒有設 `TEST_DATABASE_URL`，也沒有設 `CI`。起容器的測試確實執行而沒有略過：跨租戶測試印出每個探測（`cross-tenant.txt`）；其他起容器的測試程式（`dashboards_api`、`dashboards_store`、`history_store`、`startup`）各花 2 到 4 秒，略過時不起容器、幾乎立刻結束。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2。
- **測試用的伺服器**：
  - Postgres：`crates/service-kit` 與 `crates/testkit` 共用的 TLS Postgres，`docker.io/library/postgres:18.6-alpine@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873`；schema 由 migration Job（`tools/migrate`）套用整個產品的 migration，含 `0002_dashboards.sql`。
  - assistant：`tests/dashboards_api.rs` 的大部分測試在測試行程裡起公開 API 的路由；`tests/cross_tenant.rs`、`tests/startup.rs` 與 `opening_a_dashboard_and_running_all_its_datasets_calls_no_model` 啟動真正的執行檔，設定與 chart 相同（服務角色 `assistant_app`、公鑰檔、所有被呼叫端的位址）。
  - 身分服務的 token 交換與 telemetry-query：loopback 上的替身（`tests/dashboard_world/`），記下每個呼叫的路徑、本文與 token 的 claims；token 交換照身分服務的規則簽發（受眾是 telemetry-query、`act` 是 assistant、效期不超過原 token）；其他路徑（包括 AI 閘道會用的任何路徑）都記成意外的呼叫。
  - 內部身分 token：`testkit::identity::TestIssuer` 以測試的 Ed25519 金鑰簽發，每次請求都是新的 60 秒 token。

## 指令

在 repo 根目錄：

```sh
# assistant 的全部測試（儲存、API、跨租戶、啟動、既有的 agent 與工具測試）
cargo test -p assistant

# 跨租戶負向測試，印出每個探測
cargo test -p assistant --test cross_tenant -- --nocapture

# 其他受影響的 crate 與 repo 規則
cargo test -p testkit
cargo test -p contract
cargo test -p openapi
cargo test -p repo-check
cargo run -p openapi -- --check
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

# 前端（API client 與 i18n 的 message key）與 chart（values 的註解與 README 的欄位）
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
bun install --cwd scripts --frozen-lockfile
bun test deploy/chart
```

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo test -p assistant` | 全部通過：單元測試 61、`dashboards_api` 10、`dashboards_store` 8（1 個等 Laysi/trustiot-v2#644 的 `#[ignore]`）、`cross_tenant` 1、`startup` 1，以及既有的 `agent` 10、`agent_logs` 2、`chart_connections` 1、`delegated_tools` 13、`history_store` 5、`telemetry_sql` 2、`tools` 7（[cargo-test-assistant.txt](data/2026-10-26-150-dashboard-api/cargo-test-assistant.txt)） |
| `cargo test -p assistant --test cross_tenant -- --nocapture` | 91 個探測，0 個失敗（[cross-tenant.txt](data/2026-10-26-150-dashboard-api/cross-tenant.txt)） |
| `cargo test -p testkit`、`-p contract`、`-p openapi`、`-p repo-check` | 通過 |
| `cargo run -p openapi -- --check` | `api/openapi.json` 與程式產生的一致 |
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | 沒有差異、沒有警告 |
| 前端 `api:check`、`typecheck`、`lint`、`test` | 通過；`test` 289 個 |
| `bun test deploy/chart` | 655 個通過 |

跨租戶測試的內容：

- 四張表（`assistant.conversations`、`assistant.messages`、`assistant.dashboards`、`assistant.dashboard_versions`）各種 9 列給兩個租戶。以服務角色讀只看得到自己租戶的列，沒有租戶時一列都看不到；改寫、搬到另一個租戶、插入另一個租戶都被資料庫拒絕（`42501`）。
- 執行檔的公開 API：列表、讀取、版本列表、還原、封存、取消封存、執行資料集，各以另一個租戶的 admin 探測對方的儀表板，都回 404 而且沒有顯示任何東西；沒有租戶的 token 回 403；以自己的列做的正向對照回 200 或 201。所有探測後對方租戶的列都沒有改變。
- 列表以租戶 B 讀取時只有 B 的 9 個儀表板。
- 執行資料集的正向對照經替身的 token 交換與 telemetry-query 真的執行；整個測試期間沒有任何意外的呼叫。
- 兩個端點豁免，原因寫在豁免裡：對話的草稿列表（路徑參數是對話，不是表的列）與發布（只有草稿的擁有者看得到，admin 的正向對照不會成功）。這兩個由 `tests/dashboards_api.rs` 的 `someone_elses_draft_is_not_found_inside_the_tenant_or_outside_it` 手寫測試：同租戶的另一位 admin 與另一個租戶的 admin 對別人的草稿、別人對話的草稿列表都回 404，本文與不存在的相同，什麼都沒寫、也沒有執行任何資料集。

## 結論

只對照本報告涵蓋的部分；其餘驗收條件屬於 Laysi/trustiot-v2#150 的其他部分（檢查服務、`dashboard` 工具、回答過濾、合成 fixture 的數字比對）。

1. **以權限較少的成員或另一個租戶的身分打開儀表板時，資料集以該身分重新執行**：通過。`datasets_run_as_the_viewer_who_opened_the_dashboard`：替身記下的 token 的使用者、租戶與角色是打開儀表板的 viewer，不是作者（editor）；受眾是 telemetry-query、`act` 是 assistant；SQL 送 `max_rows` 2000，語意查詢的本文與存下的逐位元組相同；editor 自己執行時 token 是 editor 的。跨租戶負向測試見上方（91 個探測）。
2. **修改後產生新版本，舊版仍可讀回**：通過（儲存與 API 兩層）。`versions_are_added_and_old_ones_stay_readable`、`versions_are_added_kept_restored_and_publishing_over_a_change_conflicts`：每次寫入加一版，舊版以 `?version=` 讀回內容不變；還原加一版（`origin` 是 `restore`、`restored_from` 是來源）；以舊版本為基礎的還原回 409 `dashboard.version_conflict`，已儲存的儀表板在草稿的基礎版本之後被改過時發布回 409 `dashboard.saved_changed`，兩者都附 `latest`。服務角色改不了已寫的版本（`the_service_role_cannot_change_what_was_written`，`42501`）。
3. **打開與重新整理儀表板不呼叫模型**：通過。`opening_a_dashboard_and_running_all_its_datasets_calls_no_model`：對真正的執行檔列出、讀取、讀版本列表並把三個資料集各執行兩次，替身只收到 6 個 telemetry-query 的呼叫，全部是 viewer 的 token，沒有任何其他呼叫（AI 閘道的位址指向同一個替身，所以有呼叫一定會被記下）；日誌裡沒有標題、SQL 或頁面。
4. **viewer 發布、回到舊版、封存都回 403；同租戶的其他使用者讀不到別人的草稿，回 404，與不存在相同**：通過。`the_requests_role_decides_who_publishes_restores_and_archives`：viewer 發布自己的草稿、還原已儲存的儀表板、封存與取消封存都回 403 `dashboard.editor_required`；viewer 可以還原自己的草稿（201，負責人 2026-10-26 的角色表）；同一個使用者的 token 升為 editor 後立即可以發布，降回 viewer 後又被拒；`permissions` 物件與每種角色的檢查一致。404 見上方的手寫測試。
5. **公開 API 不接受 SQL 與頁面內容**：通過。`requests_are_strict_and_no_answer_is_stored`：發布、還原、執行資料集的本文多了 `spec`、`page`、`sql`、`source`、`request` 等欄位、寫成陣列（`[1, "daily"]`）、`null`、字串、同一個欄位寫兩次（`{"version": 2, …, "version": 1}`）、物件後面還有文字、超過 4 KiB、不是 JSON 的 content type、沒有本文或 JSON 不完整時都回 400 `invalid_request`，資料庫裡什麼都沒寫（物件前的 JSON 空白照常接受）；沒有 token、偽造的 token、沒有租戶時每個路由都拒絕；成功的回應都是 `cache-control: no-store`。
6. **超過 2,000 列的資料集被拒並說明怎麼縮小；同一租戶一分鐘內超過 60 次資料集執行時回 429**：通過。`more_rows_than_the_dataset_allows_are_refused_with_the_limit`：telemetry-query 拒絕超過列數、或回了 2,001 列時都回 422 `dashboard.dataset_too_many_rows`、`details.max` 2000（前端的翻譯請使用者縮短時間範圍或改用較長的間隔）；剛好 2,000 列回 200。`the_caps_refuse_the_61st_run_in_a_minute_and_a_third_at_a_time_per_tenant`（以測試控制的時鐘，不等真的一分鐘）：第 61 次回 429 `dashboard.runs_limited`、`Retry-After: 60`、`details.retry_after` 60，被拒的執行沒有呼叫任何服務；同時另一個租戶的執行照常進行；時鐘前進 60 秒後再次可以執行；第三個同時執行回 429（`Retry-After: 1`）；`dashboard` 工具的 60 次執行之後，瀏覽器的執行也被拒（共用上限）。telemetry-query 的錯誤只以封閉的 key 回給使用者（`telemetry_querys_refusals_reach_the_user_only_as_closed_keys`，9 種回答，替身塞進本文的文字沒有出現在回應裡）。
7. **對話軟刪除時它的草稿讀不到，還原後回來；清除對話時草稿與版本一起刪除；已發布的儀表板不受影響**：部分通過。清除的部分通過：`purging_a_conversation_removes_its_drafts_and_keeps_what_was_published`。軟刪除與還原**待測**：需要 Laysi/trustiot-v2#644 的 `assistant.conversations.deleted_at`，那時還沒合併；測試已寫好並以 `#[ignore]` 標記原因，Laysi/trustiot-v2#644 合併後在 `store::conversation_access` 的查詢加一行條件、拿掉 `#[ignore]` 即可。
8. **`cargo run -p openapi -- --check`、`bun run --cwd frontend api:check`、`bun run --cwd frontend test`、`bun test deploy/chart` 通過**：通過（見「結果」）。`cargo test --workspace` 與 `bun test scripts` 在這台工作站上沒有跑，由 CI 與整合時的檢查執行。
