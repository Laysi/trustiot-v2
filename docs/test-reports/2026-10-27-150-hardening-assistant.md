# AI 儀表板：assistant 一側的強化（檢查判定、寫入與檢查的名額、版本上限、發布的基準、記憶體、還原）

對應 issue：Laysi/trustiot-v2#150（AI 儀表板）。測試日期：第一輪 2026-10-27，第二輪 2026-10-27 至 2026-10-30，第三輪 2026-10-31（Asia/Taipei）。

這份報告是 Laysi/trustiot-v2#150 安全審查之後 assistant 一側的修正證據，分三輪：第一輪是審查的修正，第二輪是複審之後的修正（結果記憶體改成一個以位元組計的預算、還原改存內容的參照，見「結果」的「第二輪」），第三輪是再次複審之後的小修正（見「結果」的「第三輪」）。每一項修正都先加一個在修正前失敗的測試，修正後通過；第二輪新加的上界在修正前沒有對應的程式，表中寫明。「結果」列的是修正後的結果。驗收條件的總表仍是 [2026-10-27-150-dashboards.md](2026-10-27-150-dashboards.md)，這份不改寫它，只補上強化後重跑的跨服務驗收。沒有原始資料目錄：輸出含本機路徑，摘要直接寫在「結果」。

## 環境

- **受測程式碼**：分支 `feat/150-x-hardening-assistant`（從 `e397c9d` 開始）。第一輪的結果在第一輪最後一個 commit 上取得，第二輪的結果在本報告所在的 commit 上取得；「修正前」是同一個測試在修正的前一個 commit 上的結果。
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

# 修正前的證據：在修正的前一個 commit 上加入同一個測試後執行
cargo test -p assistant --test <測試檔> <測試名稱>
```

## 結果

### 第一輪（2026-10-27）

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

#### 第一輪每項修正的測試

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

第二輪以結果記憶體的預算取代了執行與寫入的名額：上表記憶體一列的 `the_runs_of_all_tenants_share_the_places_in_flight` 與 `the_writes_of_all_tenants_share_the_places_in_flight` 隨名額移除，由第二輪的測試取代。

### 第二輪（2026-10-27 至 2026-10-30）

複審確認了三件事，這一輪修正：別的租戶很慢的資料集執行或 AI 寫入佔滿副本的名額時，這個租戶的執行被拒、寫入回 `check_busy`；執行的名額在回應本文寫出之前就放回，本文不在任何上限內；還原為每個新版本複製一份內容。另外寫明一件設計：同一個租戶在每個副本同時只有一次 AI 寫入，不分成員，所以一位成員慢的寫入讓同租戶其他成員的 AI 寫入等待。之後再補三件事：執行的回應本文有時限（60 秒），連線沒在時限內取走整個本文時放回記憶體、中斷回應並關閉連線；有 `dashboard` 工具時，問題的委派效期至少放得下一次完整的工具呼叫；工具的說明告訴模型頁面文字要讀得清楚的規則。

| 指令 | 結果 |
| --- | --- |
| `cargo test -p assistant` | 全部通過：單元測試 129、`agent` 10、`agent_logs` 2、`answer_guard` 3、`chart_connections` 1、`cross_tenant` 1、`dashboard_acceptance` 2 ignored、`dashboard_tool` 13、`dashboards_api` 20、`dashboards_store` 9（1 個等 Laysi/trustiot-v2#644 的 ignored）、`delegated_tools` 13、`history_store` 5、`startup` 1、`telemetry_sql` 2、`tools` 8 |
| `cargo test --locked -p assistant --test dashboard_acceptance -- --ignored` | 2 個通過，10.7 秒（同第一輪的兩個測試） |
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p contract`、`-p sql-check`、`-p repo-check`、`-p testkit` | 全部通過（`testkit` 67 個） |
| `cargo test -p migrate --test registry --test fresh_database` | 通過（6 與 1 個；這一輪改了 `0002_dashboards.sql` 的還原儲存） |
| `cargo run -p openapi -- --check`、`bun run --cwd frontend api:check` | 與程式一致（執行資料集的 429 說明加上記憶體等不到的情況） |
| `bun run --cwd frontend typecheck`、`test` | 通過（`test` 598 個；這一輪沒有新的訊息鍵） |
| `bun test deploy/chart` | 691 個通過 |
| 測試結束後的 `docker ps -a` | 沒有留下測試的容器 |

#### 第二輪每項修正的測試

路徑相對於 `services/assistant/`。

| 修正 | 測試 | 修正前 |
| --- | --- | --- |
| 還原只加一列指向內容所在的版本，不複製內容；每個版本讀回的內容不變 | `tests/dashboards_store.rs`：`restores_add_rows_that_point_at_the_content_and_every_version_reads_it_back` | 失敗：寫入 2 個版本再還原 6 次後，存著內容的版本是第 1 到 8 版、共 7,720 bytes（應只有寫入的第 1、2 版、1,930 bytes） |
| 別的租戶的執行在等很慢的 telemetry-query 時，這個租戶的執行照常完成 | `tests/dashboards_api.rs`：`slow_answers_of_other_tenants_do_not_stop_a_run` | 失敗：回 429 `dashboard.runs_limited`（應為 200） |
| 別的租戶的寫入在等很慢的 telemetry-query 時，這個租戶的寫入照常檢查並存下 | `tests/dashboard_tool.rs`：`slow_writes_of_other_tenants_do_not_keep_a_write_busy` | 失敗：回 `check_busy`、沒有存下 |
| 執行的回應本文在寫出或丟棄之前都佔著結果的記憶體 | `tests/dashboards_api.rs`：`a_run_answer_holds_its_memory_until_its_body_is_written_or_dropped` | 舊程式沒有可觀察的預留；以暫時的測試確認執行的名額在回應本文寫出之前就放回 |
| 一個租戶最多佔預算的一半，其他租戶用其餘的；因記憶體被拒的執行回 429 與 `Retry-After: 1`、算進每分鐘的次數 | `src/dashboards/memory.rs`：`one_tenant_holds_at_most_its_share_and_others_use_the_rest`；`tests/dashboards_api.rs`：`a_tenant_holds_at_most_its_share_and_a_run_refused_for_memory_is_counted` | 新的上界 |
| AI 寫入在租戶的份額內取得記憶體：執行任何東西之前等不到時不算次數，資料集執行之後等不到時算一次；寫入結束後不佔記憶體 | `tests/dashboard_tool.rs`：`a_write_takes_its_memory_within_its_tenants_share` | 新的上界（取代第一輪的 `the_writes_of_all_tenants_share_the_places_in_flight`） |
| 最大的一次寫入加上租戶同時的 2 次最大執行放得進一個租戶的份額，所以閒置的副本做得完最大的寫入 | `src/tools/dashboard.rs`：`a_worst_case_write_fits_a_tenants_share`、`a_results_values_are_reserved_from_its_json_and_rows` | 新的上界 |
| 預留的等待有上界，等不到時什麼都不佔；放回的與縮小後多出的給等待中的預留 | `src/dashboards/memory.rs`：`a_reservation_that_does_not_fit_in_its_wait_is_refused_and_holds_nothing`、`a_waiting_reservation_gets_what_is_released_and_shrinking_releases_the_rest`、`a_read_holds_the_declared_length_or_twice_the_limit` | 新的上界（取代第一輪的 `the_runs_of_all_tenants_share_the_places_in_flight`） |
| 讀與解析的用量取決於上限，而不是回答或判定的內容：沒宣告長度的本文以倍增成長、不超過上限；統計與用量的表格組出前以 `max_rows` 為界；有型別的解析不超過 `PARSE_FACTOR`；判定解析前先數值 | `src/callee.rs`：`a_body_without_a_declared_length_grows_by_doubling_up_to_the_limit`；`src/dashboards/flatten.rs`：`a_table_stops_at_the_runs_max_rows`、`typed_answers_take_at_most_the_parse_factor`；`src/dashboards/checker.rs`：`a_verdict_is_parsed_only_within_its_values`、`verdict_values_fit_their_share`、`a_check_holds_its_body_and_the_largest_verdict` | 新的上界 |
| 執行的回應本文要在 60 秒（`HELD_BODY_TIMEOUT`）內由連線取走（連線寫出一段才拿下一段），否則放回記憶體、中斷回應並關閉連線：讀取的讀者逐位元組拿到整個本文；停止讀取的讀者一直不讀，連線仍在時限後關閉、記憶體已放回，之後只拿到不完整的本文；沒讀就丟棄時立即放回 | `tests/dashboards_api.rs`：`a_reader_that_stops_loses_the_answer_and_the_connection_at_the_bound`（約 1.5 MB 的本文、小的 socket 緩衝、時限設成 1 秒）；`src/api/held_body.rs`：`a_held_body_hands_the_whole_json_over_in_pieces_then_lets_it_go`、`at_most_two_pieces_wait_to_be_written_and_a_written_piece_brings_the_next`、`at_the_bound_the_result_goes_the_waiting_connection_wakes_and_the_body_fails`、`a_body_never_asked_for_lets_the_result_go_at_the_bound`、`a_body_dropped_unread_lets_the_result_go_at_once`、`answers_whose_readers_stopped_leave_another_tenant_its_share` | 失敗：以修正前的本文（整個結果一次交給連線）跑這個整合測試的前一版，停止讀取的讀者的記憶體在 120 秒的等待結束時仍未放回；只在時限放回記憶體、不限制連線緩衝裡段數的本文，連線在 120 秒的等待結束時仍未關閉 |
| 有 `dashboard` 工具時，問題的委派效期至少 365 秒（一次完整的工具呼叫 335 秒加結束前的 30 秒）；問題的截止時間取消進行中的呼叫，被取消的呼叫不再送出請求 | `src/config.rs`：`a_dashboard_check_needs_a_run_lifetime_with_room_for_one_dashboard_call`；`tests/dashboard_tool.rs`：`datasets_still_running_at_the_bound_are_problems_and_the_rest_is_kept`（服務的呼叫時限等於 `CALL_LIMIT`）；`tests/tools.rs`：`dropping_a_step_cancels_its_calls_before_they_send` | 新的設定檢查（修正前 2 分鐘的委派也被接受）；取消是既有的行為，新測試確認它 |

在一次量到的執行裡，伺服器在送出請求後約 1.14 秒關閉停止讀取的讀者的連線（時限設成 1 秒），讀者之後拿到 1,517,271 bytes 中的 327,542 bytes（socket 緩衝裡的部分），遠低於回應的長度，所以這個測試不是剛好過。

記憶體的上界（由上限推得的估計，不是實測；推導見 `services/assistant/README.md`「記憶體」）：

| 項目 | 每個副本 |
| --- | --- |
| 結果記憶體預算（`RESULT_MEMORY`）與一個租戶的份額 | 576 MiB，每個租戶 288 MiB |
| 一次資料集執行，讀與解析時 | SQL 約 26 MiB，統計與用量約 54 MiB |
| 一次 AI 寫入 | 約 164 MiB |
| 一個租戶同時最多（最大的寫入加 2 次最大的執行） | 約 272 MiB |
| 讀者都不讀時，一個租戶的回應本文最多佔 | 124 MiB（60 秒內最多 62 個 2 MiB 的本文）；第三輪起以每個租戶的回應空間 36 MiB 為上限（見「第三輪」） |
| 合計（預算、結果快取 64 MiB、其他約 64 MiB） | 約 704 MiB；chart 的 requests 128Mi、limits 768Mi 不變 |

### 第三輪（2026-10-31）

再次複審之後的修正。受測程式碼是分支 `feat/150-dashboards` 上這一輪的最後一個 commit；「修正前」是同一個測試在修正的前一個 commit 的程式上的結果。

| 修正 | 測試 | 修正前 |
| --- | --- | --- |
| 每個租戶的回應空間：API 的執行開始前先從租戶的回應空間（`ANSWER_MEMORY_PER_TENANT`，預算的十六分之一，36 MiB）取最大結果的 2 MiB，回應持有 JSON 長度的空間，直到與結果的記憶體一起放回；空間不夠時只有這個租戶新的執行被拒（429 `dashboard.runs_limited`、`Retry-After: 1`），不呼叫 telemetry-query、不算進每分鐘的次數，別的租戶的執行照常；讀者離開後這個租戶再執行 | `tests/dashboards_api.rs`：`a_tenants_unread_answers_refuse_only_its_own_runs_past_its_room`（回應空間設成一個最大結果）；`src/dashboards/memory.rs`：`a_tenants_answers_hold_at_most_their_room_and_other_tenants_are_not_held_up`；`src/api/held_body.rs`：`answers_whose_readers_stopped_hold_at_most_their_tenants_room` | 新的上界 |
| AI 寫入在判定變成標記或問題時就放回檢查請求本文與判定的記憶體，存檔時只佔值的記憶體 | `tests/dashboard_tool.rs`：`a_write_gives_back_its_checks_memory_before_it_saves`（測試佔住連線池唯一的連線，存檔等連線時看租戶佔的記憶體） | 失敗：存檔等連線時仍佔著檢查時的 20,811,824 bytes，寫入結束時才一次放回 |
| 工具的存檔交易設與 API 相同的 `lock_timeout`（`REQUEST_LOCK_TIMEOUT`，2 秒），`55P03` 是原本的存檔失敗 | `tests/dashboard_tool.rs`：`a_save_waits_for_the_conversations_row_only_the_requests_lock_timeout`（另一個工作階段以 `FOR UPDATE` 鎖住對話的列） | 失敗：存檔等到 `SAVE_TIMEOUT`（30 秒）才結束 |
| 對話擁有者查詢的 `FOR SHARE` 版本只回擁有者的列：非擁有者發布或還原別人的草稿不等、也不鎖對話的列，回應與不存在的儀表板逐位元組相同（404） | `tests/dashboards_api.rs`：`someone_elses_draft_is_not_found_without_waiting_for_its_conversation`（另一個工作階段鎖住對話的列；擁有者自己的發布等那一列、回 503，是對照） | 失敗：非擁有者的發布等那一列，在請求的鎖等待上限後回 503（應為 404） |
| 文件：別的租戶在等 telemetry-query 的慢資料集執行不讓寫入忙碌，檢查名額與記憶體預算仍共用（`docs/architecture/ai-assistant.md` 第 9 點、`services/assistant/README.md` 寫入的第 3 步、本報告結論 9）；清除與草稿列的鎖（README「排程清除」）；合併報告的一格措辭 | — | — |

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | `edge/agent/tests/binary.rs` 編譯失敗，是已知的本機失敗，在 CI 通過；`--exclude agent` 與 `-p agent --lib --bins` 都通過 |
| `cargo test -p assistant --no-fail-fast` | 全部通過：單元測試 139、`agent` 10、`agent_logs` 2、`answer_guard` 3、`api` 10、`chart_connections` 1、`conversation_threads` 12、`cross_tenant` 3、`dashboard_acceptance` 2 ignored、`dashboard_tool` 16、`dashboards_api` 23、`dashboards_store` 11、`delegated_tools` 13、`history_store` 5、`permissions` 2、`purge_job` 1、`purge_locks` 2、`startup` 1、`telemetry_sql` 2、`tools` 8 |
| `cargo run -p openapi -- --check` | 與程式一致（這一輪沒有改 API 的型別或說明） |
| `cargo test -p repo-check` | 全部通過 |

## 結論

1. **檢查判定只把固定集合的值與 assistant 自己的資料交給模型**：通過，見上表。
2. **檢查的名額依租戶公平分配、每個副本有上界**：通過。
3. **發布與回到舊版的版本數有每分鐘上限，無變化的回到舊版被拒，草稿與已發布的版本分開計算**：通過。
4. **發布舊的草稿版本時，比對的是該版本自己的基準；工具的 rebase 只接受讀過的已發布版本**：通過。
5. **assistant 每個副本的記憶體有上界，chart 依此設定 request 與 limit**：上界的行為通過（見上表）；chart 的值由 `bun test deploy/chart` 確認。實際的記憶體用量量測**待測**：數字的推算見 `services/assistant/README.md`「記憶體」，以量測校正上限屬於 Laysi/trustiot-v2#694 的調整。
6. **草稿的建立者與對話擁有者一致由資料庫保證**：通過。
7. **跨服務的驗收 1 與 2 在強化後仍通過**：通過。
8. **還原不複製內容，每個版本讀回的內容不變**：通過（第二輪）。
9. **結果記憶體是每個副本一個以位元組計的預算，執行在 telemetry-query 回答之後才預留，預留隨結果進入回應本文與寫入，一個租戶最多一半；別的租戶在等 telemetry-query 的慢資料集執行，不讓這個租戶的執行被拒、也不讓它的寫入忙碌（檢查名額與記憶體預算仍由所有租戶共用）**：行為通過（第二輪）。實際的記憶體用量量測**待測**：以量測校正預算與 chart 的值屬於 Laysi/trustiot-v2#694 的調整。
10. **同一個租戶的 AI 寫入一次一個、不分成員**：設計如此，`services/assistant/README.md` 與 `docs/architecture/ai-assistant.md` 寫明一位成員慢的寫入讓同租戶其他成員的 AI 寫入等待。
11. **跨服務的驗收 1 與 2 在第二輪之後仍通過**：通過。
12. **執行的回應本文最多 60 秒：讀者停止讀取時，記憶體放回、回應中斷、連線在時限關閉；讀取的讀者仍拿到整個本文**：通過（第二輪）。
13. **有 `dashboard` 工具時，問題的委派效期放得下一次完整的工具呼叫；截止時間取消進行中的呼叫**：通過（第二輪）。
14. **工具的說明告訴模型頁面文字要讀得清楚**（標籤水平或最多斜 45°、不鏡像不拉伸、字級與對比讀得到、一般的文字元素）：已寫進工具的說明與 `services/assistant/README.md`；檢查與 runtime 拒絕這些頁面的證據不在這份報告。
15. **一個租戶還沒被讀者取走的回應最多佔它的回應空間（36 MiB），超過時只拒絕這個租戶自己新的執行，不影響別的租戶的執行與寫入**：通過（第三輪）。
16. **工具的存檔等列鎖最多與 API 的請求一樣久；非擁有者的發布與還原不等、也不鎖別人對話的列，回應不變**：通過（第三輪）。
