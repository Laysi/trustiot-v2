# AI 儀表板：assistant 一側的強化（檢查判定、寫入與檢查的名額、版本上限、發布的基準、記憶體）

對應 issue：Laysi/trustiot-v2#150（AI 儀表板）。測試日期 2026-10-27（Asia/Taipei）。

這份報告是 #150 安全審查之後 assistant 一側的修正證據。每一項修正都先加一個在修正前失敗的測試，修正後通過；下表列的是修正後在本報告 commit 上的結果。驗收條件的總表仍是 [2026-10-27-150-dashboards.md](2026-10-27-150-dashboards.md)，這份不改寫它，只補上強化後重跑的跨服務驗收。沒有原始資料目錄：輸出含本機路徑，摘要直接寫在「結果」。

## 環境

- **受測程式碼**：本報告所在的 commit（分支 `feat/150-x-hardening-assistant`，從 `e397c9d` 開始）。
- **執行的地方**：開發工作站。容器引擎是 Docker，平台 `linux/arm64`；起容器的測試一次只跑一個。沒有設 `CI`。
- **工具版本**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2。
- **檢查服務**：在 `e397c9d` 建出的 `localhost/trustiot-e2e/dashboard-check:e397c9d`，映像 ID `sha256:6ef0a921b483af748c6beb98012047727a972f5bbee0fd213764a19dfa77a056`（與 2026-10-26 映像報告的映像相同）。驗收測試照 chart 的設定執行它，方式與 [2026-10-27-150-dashboards.md](2026-10-27-150-dashboards.md) 的「環境」相同。
- **Postgres、ClickHouse、telemetry-query、AI 閘道與資料**：與 [2026-10-27-150-dashboards.md](2026-10-27-150-dashboards.md) 的「環境」相同（testkit 的 TLS Postgres 與 migration Job 套用的整個產品 schema、拋棄式 ClickHouse、測試程序裡的 telemetry-query 或它的替身、照腳本回答的 AI 閘道替身）。

## 指令

在 repo 根目錄：

```sh
# assistant 的全部測試（驗收測試在這裡是 ignored）
cargo test -p assistant

# 跨服務的驗收 1 與 2
DASHBOARD_CHECK_IMAGE=localhost/trustiot-e2e/dashboard-check:e397c9d \
  cargo test --locked -p assistant --test dashboard_acceptance -- --ignored

# 其餘檢查
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p contract
cargo test -p sql-check
cargo test -p repo-check
cargo test -p testkit
cargo test -p migrate --test registry --test fresh_database
cargo run -p openapi -- --check
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend test
bun test deploy/chart
```

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo test -p assistant` | 全部通過：單元測試 111、`agent` 10、`agent_logs` 2、`answer_guard` 3、`chart_connections` 1、`cross_tenant` 1、`dashboard_acceptance` 2 ignored、`dashboard_tool` 12、`dashboards_api` 16、`dashboards_store` 8（1 個等 Laysi/trustiot-v2#644 的 ignored）、`delegated_tools` 13、`history_store` 5、`startup` 1、`telemetry_sql` 2、`tools` 7 |
| `cargo test --locked -p assistant --test dashboard_acceptance -- --ignored` | 2 個通過，10.8 秒（`the_numbers_on_a_dashboard_equal_clickhouses`、`a_page_that_does_not_match_its_data_is_refused_and_the_user_keeps_the_previous_version`） |
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p contract`、`-p sql-check`、`-p repo-check`、`-p testkit` | 全部通過（`testkit` 67 個） |
| `cargo test -p migrate --test registry --test fresh_database` | 通過（6 與 1 個） |
| `cargo run -p openapi -- --check`、`bun run --cwd frontend api:check` | 與程式一致 |
| `bun run --cwd frontend typecheck`、`test` | 通過（`test` 583 個；含新訊息鍵的 zh-TW 與 en 翻譯） |
| `bun test deploy/chart` | 691 個通過 |
| 測試結束後的 `docker ps -a` | 沒有留下測試的容器 |

### 每項修正的測試

每個測試在修正前失敗、修正後通過。路徑相對於 `services/assistant/`，`sql-check` 的測試在 `tools/sql-check/tests/check.rs`。

| 修正 | 測試 |
| --- | --- |
| 模型從檢查判定只拿到固定集合的值與 assistant 自己的資料；判定報告的值與自己的資料不符時不存 | `tests/dashboard_tool.rs`：`the_model_learns_only_closed_values_and_its_own_data_from_a_verdict` |
| 每個租戶在每個副本同時只有一次寫入在檢查；所有租戶共用檢查的名額 | `tests/dashboard_tool.rs`：`a_tenant_checks_one_write_at_a_time_and_other_tenants_go_on`、`the_checks_of_all_tenants_share_the_places` |
| 發布與回到舊版每分鐘的版本數依使用者與租戶限制；內容與最新版相同的回到舊版被拒；草稿與已發布的儀表板各自計算版本；版本數到上限時說明原因 | `tests/dashboards_api.rs`：`versions_added_by_publishing_and_restoring_are_limited_per_user_and_tenant_in_a_minute`、`a_restore_that_changes_nothing_is_refused_and_a_draft_numbers_its_own_versions`、`a_dashboard_with_the_most_versions_takes_no_more_and_says_why` |
| 草稿的每個版本記下它根據的已發布版本，發布與回到舊版用的是該版本的基準；工具只接受這個問題讀過的已發布版本做 `rebase_saved_version` | `tests/dashboards_api.rs`：`publishing_an_older_draft_version_compares_the_base_it_was_written_on`、`restoring_a_draft_version_carries_the_base_it_was_written_on`、`versions_written_before_the_first_publish_are_based_on_no_saved_version`；`tests/dashboard_tool.rs`：`a_rebase_names_a_saved_version_this_question_read` |
| 記憶體有上界：所有租戶的資料集執行與寫入共用每個副本的名額；結果快取以實際的位元組計算，每個租戶最多四分之一；SQL 回答只讀到結果允許的範圍 | `src/dashboards/caps.rs`：`the_runs_of_all_tenants_share_the_places_in_flight`；`src/tools/dashboard.rs`：`one_tenant_cannot_take_the_cache_from_the_others`（修正前以同一情境的暫時測試在舊程式上失敗）；`tests/dashboard_tool.rs`：`the_writes_of_all_tenants_share_the_places_in_flight`；另有新增的 `a_sql_answer_reads_like_the_contracts_type`、`a_sql_answer_is_read_only_within_what_a_result_may_hold`、`the_result_cache_keeps_a_questions_results_within_its_budget`、`json_len_is_the_length_of_the_json` |
| 資料庫要求草稿的建立者是它所屬對話的擁有者 | `tests/dashboards_store.rs`：`the_service_role_cannot_change_what_was_written` |
| 回答過濾的長 SQL 測試輸入改以字串串接產生，不被 `sql-check` 誤判 | `tools/sql-check` 的 `the_workspace_has_no_string_built_sql` |

## 結論

1. **檢查判定只把固定集合的值與 assistant 自己的資料交給模型**：通過，見上表。
2. **檢查的名額依租戶公平分配、每個副本有上界**：通過。
3. **發布與回到舊版的版本數有每分鐘上限，無變化的回到舊版被拒，草稿與已發布的版本分開計算**：通過。
4. **發布舊的草稿版本時，比對的是該版本自己的基準；工具的 rebase 只接受讀過的已發布版本**：通過。
5. **assistant 每個副本的記憶體有上界，chart 依此設定 request 與 limit**：上界的行為通過（見上表）；chart 的值由 `bun test deploy/chart` 確認。實際的記憶體用量量測**待測**：數字的推算見 `services/assistant/README.md`「記憶體」，以量測校正上限屬於 Laysi/trustiot-v2#694 的調整。
6. **草稿的建立者與對話擁有者一致由資料庫保證**：通過。
7. **跨服務的驗收 1 與 2 在強化後仍通過**：通過。
