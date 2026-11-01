# assistant：元件模式的寫入與頁面選填的 migration（2026-11-02）

Laysi/trustiot-v2#934。這份報告記錄頁面選填（`0004_dashboard_pages_optional.sql`）、只有元件的版本的寫入與讀取，以及沒有檢查服務時 `dashboard` 工具的行為的測試證據。

## 環境

- **程式碼**：分支 `feat/934-dashboard-component-writes`，以 `origin/main` 的 `32393ddc` 為基礎，加上這次的變更（報告與變更同一個 commit）。
- **機器**：建置主機（arm64 Linux VM，rootless podman），經工作佇列執行，設 `CI=1`：容器測試沒有容器執行環境時失敗而不是略過。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 鎖定的版本。
- **資料庫**：測試自己啟動的拋棄式 Postgres（TLS），schema 由遷移 Job（`tools/migrate`）套用。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p contract
CI=1 cargo test --locked -p assistant
CI=1 cargo test --locked -p testkit
cargo run --locked -p openapi -- --check
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend lint
bun run --cwd frontend test
# 驗收測試：先建檢查服務的映像檔，再以它跑
bun install --cwd scripts --frozen-lockfile
bun test deploy/images/dashboard-check
DASHBOARD_CHECK_IMAGE=localhost/trustiot-e2e/dashboard-check:image-test \
  cargo test --locked -p assistant --test dashboard_acceptance -- --ignored
cargo test --locked --workspace --exclude agent
bun test deploy/chart
```

`api/openapi.json` 以 `cargo run -p openapi` 重新產生，`frontend/src/api/schema.gen.ts` 以 `bun run --cwd frontend api:generate` 重新產生。

## 結果

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -D warnings` | 通過 |
| `cargo test -p contract` | 全部通過（含 `fixtures_match_the_types`：`dashboard-view.json` 沒有變，新的 `dashboard-view-components.json` 沒有 `page` 與 `runtime`） |
| `CI=1 cargo test -p assistant` | 全部通過：lib 140、`dashboard_tool` 20、`dashboards_api` 25、`dashboards_store` 11、`dashboards_migration` 1、`startup` 1、`cross_tenant` 3，其他檔案照舊；`dashboard_acceptance` 2 個預設忽略（另跑，見下） |
| `CI=1 cargo test -p testkit` | 全部通過 |
| `cargo run -p openapi -- --check` | 通過（重新產生後） |
| frontend `api:check`、`typecheck`、`lint`、`test` | 通過；`test` 686 pass、0 fail |
| 驗收測試（真的檢查服務映像檔） | 通過：2 passed（`a_page_that_does_not_match_its_data_is_refused_and_the_user_keeps_the_previous_version`、`the_numbers_on_a_dashboard_equal_clickhouses`）。建映像檔的 `bun test deploy/images/dashboard-check` 75 pass、1 fail：`Chromium refuses to start without --no-sandbox under the engine's default seccomp profile` 在這台 VM 上沒有如預期失敗；這個測試只看映像檔與容器引擎，這次沒有改 `services/dashboard-check` 與它的映像檔，見結論的備註 |
| `cargo test --workspace --exclude agent`（`--no-fail-fast`） | 通過：2,396 passed、0 failed、9 ignored（預設忽略的驗收與 e2e 測試）。第一次執行時 `sql-check` 的 `the_workspace_has_no_string_built_sql` 抓到 `dashboards_migration.rs` 以 `Vec::leak` 建 registry（`LeakedString`：用 sqlx 的檔案不能 leak 字串）；改成 `OnceLock` 的 static 後重跑，fmt、clippy 與整個 workspace 都通過 |
| `bun test deploy/chart` | 通過：837 pass、0 fail（chart README 與 `values-onprem.yaml` 的註解有改） |

### 新的測試與它們證明的事

- `tests/dashboards_migration.rs`（`pages_become_optional_on_a_filled_database_and_no_row_changes`）：遷移 Job 先只套 assistant 到 0003，store 寫兩個租戶的草稿、發布、取草稿再發布、還原（還原的列沒有內容），共 7 列版本；0003 對沒有頁面的版本回 `23514`、`dashboard_versions_check2`。再以全部檔案執行 Job：`assistant.dashboards` 與 `assistant.dashboard_versions` 的每一列（`to_jsonb`）不變，每一版以 store 開啟的結果相同；CHECK 只少了 `_check2`、`_check7`，多了 `dashboard_versions_content`、`dashboard_versions_content_size`，其他 CHECK 的定義不變。之後 store 寫得下只有元件的版本、讀回沒有頁面與 runtime；以超級使用者插入時，有 runtime 沒頁面、有頁面沒 runtime 都被 `dashboard_versions_content` 拒絕，兩者都有或都沒有可以；沒有頁面時 spec 剛好 57,344 位元組可以、多 1 位元組被 `dashboard_versions_content_size` 拒絕，有頁面時合計多 1 位元組同樣被拒絕；再執行一次 Job 不套用任何東西、列不變。
- `tests/dashboard_tool.rs`：
  - `a_version_of_components_only_is_saved_without_the_check`：只有元件的草稿改標題，三個資料集以使用者身分執行、沒有送檢查、存成第 2 版，結果沒有 `check`；讀回沒有頁面與 runtime、元件清單不變；`read` 只有一個文字區塊、`has_page: false`；`page_edits` 回 `page_edit_not_found`，沒有執行資料集。
  - `components_only_are_checked_against_the_results`：資料集回的欄位不是元件讀的欄位時回 `invalid_spec`、`column_unknown`（`path` 在 `/spec/components/` 下），不送檢查、不存。
  - `without_the_check_service_a_custom_page_is_refused_and_components_are_saved`：沒有 checker 的工具，對新的有頁面的儀表板、改頁面、只改有頁面的草稿的標題，都只回一個問題 `{kind: invalid_spec, path: /page, problem: custom_page_disabled}`，沒有呼叫 telemetry-query、沒有送檢查、草稿仍是第 1 版；只有元件的草稿照常存下；被拒的寫入不算次數：之後同一個問題還能寫滿 6 次，第 7 次被拒。
- `tests/dashboards_api.rs`：
  - `versions_of_components_only_are_opened_published_restored_and_run`：只有元件的版本開啟與版本列表都沒有 `page` 與 `runtime`，有頁面的版本兩者都有；發布、執行資料集、還原（還原的版本也沒有頁面）與還原有頁面的版本都照常。
  - `opening_a_dashboard_and_running_all_its_datasets_calls_no_model` 加上：沒設 `dashboard_check.url` 的執行檔記一行 `the dashboard tool writes components only: ...`，沒有 `no dashboard tool`。
- `src/config.rs` 的單元測試：只有 `telemetry_query.url`（沒有檢查服務）時 `tools.max_text_bytes` 小於 64 KiB、`identity.run_lifetime` 小於 365 秒都被拒絕；只有 `assets.url` 時兩分鐘仍可以。
- `crates/contract/tests/dashboard.rs`：只有元件的版本以 `validate_version(..., None)` 合格、元件與固定樣本的結果相符、序列化沒有 `page` 與 `runtime`。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 頁面與 runtime 選填，一起有或一起沒有，56 KiB 上限沒有頁面時以 0 計；以 0003 的資料遷移後不變 | 通過（`dashboards_migration`） |
| 寫入以 `validate_version` 驗證、執行資料集、以結果檢查元件；只有自訂頁面送檢查 | 通過（`dashboard_tool` 三個新測試與既有測試） |
| 沒有檢查服務時仍有工具：元件寫得進去，自訂頁面回固定的問題類別、什麼都不存 | 通過（`dashboard_tool`、`dashboards_api` 的執行檔日誌） |
| 讀取、發布、還原、資料集執行的 API 能回沒有頁面的版本 | 通過（`dashboards_api`） |
| message key 有 zh-TW 與 en 翻譯；openapi 與前端 client 重新產生 | 通過（frontend `test`、`api:check`、`openapi --check`） |

備註：建檢查服務映像檔時 `deploy/images/dashboard-check` 的映像檔測試有一項失敗（Chromium 在引擎預設的 seccomp 下、沒有 `--no-sandbox` 也啟動了）。這一項只看映像檔與執行它的容器引擎，與這次的變更無關（沒有改 `services/dashboard-check`、它的映像檔或 chart 的模板與 values；chart 只改了 README 與一行註解）；映像檔本身建成了，驗收測試以它通過。是否是這台建置主機的引擎設定造成，留給映像檔的負責人確認，不在這張 issue 的範圍。
