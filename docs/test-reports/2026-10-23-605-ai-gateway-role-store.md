# ai-gateway：模型角色存進 Postgres、每個請求確認設定世代（2026-10-23）

Laysi/trustiot-v2#605：ai-gateway 把模型角色的模型清單覆寫存進 Postgres 的 schema `ai_gateway`（`role_models`、`settings_generation`、append-only 的 `role_model_changes`），每個轉送請求讀一次設定世代、世代變了才重載；讀不到設定時以 503 拒絕而不呼叫供應商。chart 的 Postgres 接線改成依服務的設定前綴產生，ai-gateway 拿到 `AI_GATEWAY_POSTGRES_*` 與 `AI_GATEWAY__DATABASE__*`。這份報告是驗收條件的證據：除了最後一條（nightly），全部由下列自動化測試以 `CI=1` 實際執行（需要容器的測試不略過）。

## 環境

- **程式碼**：分支 `feat/605-ai-gateway-role-store`，rebase 到 main `74216494`（Laysi/trustiot-v2#604）之後的 commit。下表除了 `bun test scripts`（rebase 之後那棵樹的結果），都是 2026-10-24 安全審查的修正、再 merge main `d1269a0d`（Laysi/trustiot-v2#609 的 Postgres 連線預算，含 Laysi/trustiot-v2#622 的 tokio 1.53.3）之後在同一棵樹上重跑的結果（見「安全審查之後的修正」與「連線預算」）。
- **機器**：建置主機（x86_64 Linux），經檢查佇列執行；容器測試由 podman 起 service-kit 的拋棄式 TLS Postgres（`crates/service-kit/tests/support/postgres.rs`，每個測試自己的資料庫，以 `ai_gateway_owner` 跑 `crates/service-kit/sql/database.sql`、建立 schema、跑 `0001_model_roles.sql` 與 migration 紀錄，與 migration Job 的順序相同）。
- **工具**：Rust 1.99.0（`rust-toolchain.toml`）、bun 1.4.2（`.prototools`）。
- **設定**：`CI=1`，沒有容器執行環境時測試失敗而不是略過。不連真正的供應商：供應商是 loopback 上的腳本化 HTTP/1.1 伺服器。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked --no-fail-fast -p ai-gateway
CI=1 cargo test --locked --no-fail-fast -p migrate -p sql-check -p repo-check -p testkit
cargo run --locked -p openapi -- --check
cargo deny --locked check
cargo tree -p ai-gateway --prefix none | grep -c '^contract '   # 0
bun test deploy/chart
bun test deploy/flux
bun test scripts/ci
bun install --cwd scripts --frozen-lockfile && bun test scripts
```

驗收證據可以單獨重跑：`CI=1 cargo test -p ai-gateway --test roles_store --test process`。

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p ai-gateway` | 單元 23、`chart_connections` 1、`decisions` 4、`fallback` 9、`messages` 7、`no_contract` 2、`process` 6、`roles_store` 9，共 61 個全部通過；每個測試的名稱見 [ai-gateway-tests.txt](data/2026-10-23-605-ai-gateway-role-store/ai-gateway-tests.txt) |
| `cargo test -p migrate -p sql-check -p repo-check -p testkit` | 全部通過，含 `registry` 6（ai-gateway 的 migration 登記與目錄一致）、`fresh_database` 1（叢集裡沒綁密碼的 `ai_gateway_app` 維持 NOLOGIN）、sql-check 26（`store.rs` 的 SQL 全是靜態字串） |
| `cargo tree -p ai-gateway` 的 `contract` | 0 行 |
| `cargo run -p openapi -- --check` | 通過 |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都通過 |
| `bun test deploy/chart` | 646 通過、0 失敗 |
| `bun test deploy/flux` | 77 通過、0 失敗 |
| `bun test scripts/ci` | 488 通過、0 失敗 |
| `bun test scripts` | 1659 通過、9 略過（需要 podman 或 arm64 的端對端測試，與這次變更無關）、0 失敗 |

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 兩個行程共用一個資料庫，A 的更換在 B 的下一個請求生效 | 通過 | `process::a_change_reaches_every_process_for_its_next_request`：兩個真正的程式連同一個資料庫，各送一次請求（都已讀入設定）後經 `store.rs` 更換，`set_models` 回傳之後 B 的下一個請求、接著 A 的下一個請求，供應商收到的 `model` 都是新模型；兩個程式的日誌都有 `model role uses stored models` |
| 重啟之後用最後一次更換 | 通過 | `process::a_restarted_process_uses_the_last_change`：兩次更換之後啟動、再重新啟動一次，兩次的請求都用第二次更換的模型，第一次更換的供應商沒收到請求 |
| 換成別種協定或沒設定的供應商被拒、什麼都沒寫；不成立的覆寫被忽略並記 ERROR | 通過 | `roles_store::refused_changes_write_nothing_and_each_change_writes_one_record`：8 種被拒的更換（別種協定、沒設定的供應商、9 個、重複、格式錯誤、模型 ID 含逗號、沒設定的角色、空的操作者）之後世代、角色列與紀錄都是 0；`process::a_stored_list_the_configuration_no_longer_allows_is_ignored_with_an_error`：存進去之後設定拿掉那個供應商再啟動，請求用設定的模型，日誌有 ERROR `ignored the stored models of a model role` |
| 重設之後再更換版本比重設前大；帶重設前版本的更換是衝突 | 通過 | `roles_store::versions_keep_rising_after_a_reset`：重設的版本大於重設前，重設後角色的列保留重設的版本、模型是 NULL；帶重設前版本與帶 0（更換之前讀到的版本）的更換都回衝突；之後的更換版本大於兩者 |
| 資料庫連不上時回 503、沒有供應商呼叫 | 通過 | `roles_store::no_provider_is_called_while_the_database_cannot_be_read`：經 TCP 代理連資料庫，切斷代理之後 Messages 與 Decisions 都回 503（各自的錯誤格式），供應商的請求數是 0；從啟動起就連不上資料庫時也一樣 |
| 進行中的串流在更換之後仍用它開始時的模型 | 通過 | `roles_store::a_stream_keeps_its_model_after_a_change`：供應商送出串流的前半（到 `content_block_stop` 之前）後暫停，此時更換角色的模型並以另一個請求確認新模型生效，再讓串流跑完：交回的串流與錄製的樣本逐位元組相同，供應商收到的 `model` 是原來的模型 |
| 重載之後沒變的模型的並發計數沒有歸零 | 通過 | `roles_store::a_reload_keeps_the_concurrency_count_of_a_model_that_stays`：`max_concurrent` 1，一個請求占住模型（供應商不回應），更換清單但保留這個模型並重載之後，新請求在這個模型仍是滿的、改用下一個模型 |
| 服務角色對 `role_model_changes` 只能 SELECT、INSERT；每次更換一筆紀錄、被拒的沒有 | 通過 | `roles_store::the_service_role_cannot_rewrite_or_remove_the_change_log`：`ai_gateway_app` 的 UPDATE、DELETE、TRUNCATE 都回 `42501`（`role_models` 的 DELETE、TRUNCATE 也是），紀錄不變；`refused_changes_write_nothing_and_each_change_writes_one_record` 逐筆核對紀錄（版本、角色、`set`/`reset`、模型、操作者） |
| `cargo tree -p ai-gateway` 不含 `contract` | 通過 | `no_contract` 2 通過，`cargo tree` 計數 0 |
| `cargo test -p ai-gateway`、`-p migrate`、`-p sql-check`、`bun test deploy/chart`、workspace clippy 通過 | 通過 | 上表 |
| 帶 `smoke` label 的 PR 的 nightly 部署啟用資料庫的 ai-gateway，pod 就緒 | 待測 | PR 推上去並加上 `smoke` label 之後由 nightly 執行；結果補在下一節 |

chart 的接線另由 `deploy/chart/tests/chart.test.ts` 檢查（`AI gateway settings`）：bundled 與 external 兩種 Postgres，閘道的 `AI_GATEWAY_POSTGRES_{HOST,PORT,DATABASE,SSLMODE}` 與平台 ConfigMap 的 `TRUSTIOT_POSTGRES_*` 相同、`AI_GATEWAY_POSTGRES_CA_FILE` 與 CA 掛載、`AI_GATEWAY__DATABASE__USERNAME=ai_gateway_app`、密碼來自 `<release>-ai-gateway-password`、沒有 `TRUSTIOT__DATABASE__*`、migration Job 綁定 `ai_gateway_app`；自訂 `env` 設 `AI_GATEWAY_POSTGRES_HOST`、`AI_GATEWAY__DATABASE__USERNAME` 或（小寫、經 `valueFrom`）`AI_GATEWAY__DATABASE__PASSWORD` 時渲染失敗；`mesh.calls.ai-gateway` 拿掉 `postgres` 時渲染失敗（saas 與 onprem 兩種形狀，訊息 `mesh.calls.ai-gateway must list postgres`）。

## 安全審查之後的修正（2026-10-24）

協調者的安全審查沒有 Critical 或 High；以下修正在同一個分支的新 commit，結果見上方「結果」。

| 項目 | 修正 | 證據 |
| --- | --- | --- |
| 資料庫的世代倒退（例如從備份還原）時，副本原本留著記憶體裡較新、已經不在資料庫的設定 | 資料庫是來源：世代與記憶體不同就重載，較舊時也採用並記 WARN（`loaded`、`stored`） | `process::settings_restored_from_an_older_backup_are_adopted_with_a_warning`：兩次更換之後以 owner 把三張表還原成第一次更換的內容（世代 1），下一個請求用第一次更換的模型，日誌有 WARN，`loaded` 2、`stored` 1 |
| 交易沒有伺服器端的上限 | 每個交易開始後設 `lock_timeout` 與 `statement_timeout`（`set_config(..., true)`）：轉送請求的讀取設成到這次設定確認的期限為止剩下的時間（不超過 `database.timeout`，重載的每個查詢之前重設），更換的每個敘述是 `database.timeout` | `roles_store::the_server_stops_a_blocked_read_at_the_calls_deadline`：owner 以 `ACCESS EXCLUSIVE` 鎖住 `settings_generation`，`database.timeout` 設 60 秒、期限 500 ms 的世代讀取與重載都由伺服器結束（SQLSTATE 55P03 或 57014），在 50 秒內回來，客戶端沒有取消它們；鎖放開之後讀得到；其他 `roles_store` 與 `process` 測試都經過新的交易路徑；`sql-check` 通過 |
| 啟動時的時間預算檢查不含 `database.timeout` | 不加入；在 `services/ai-gateway/README.md`「時間上限」寫明檢查保證的範圍與理由 | 文件 |
| chart 允許啟用閘道卻沒有 `postgres`，pod 會一直重啟 | 渲染失敗並說明原因 | `chart.test.ts` 的 `the templates refuse a gateway without postgres under mesh.calls, without schema validation` 與 `... an on-premises gateway without postgres under mesh.calls ...` |
| 世代與版本只升不降只靠程式碼 | migration：`role_models` 只給 `models`、`version`、`updated_at`、`updated_by` 欄的 UPDATE；兩個 trigger 拒絕不讓世代或版本上升、或改角色名稱的 UPDATE（42501），owner 也一樣 | `roles_store::the_generation_and_the_versions_only_rise_for_every_role`：`ai_gateway_app` 與 `ai_gateway_owner` 的 5 種不上升的 UPDATE 與改名都回 42501，資料不變，之後 `set_models` 照常成功且版本上升 |

## 連線預算（merge Laysi/trustiot-v2#609 之後）

Laysi/trustiot-v2#609 讓 chart 檢查整版滾動時所有服務的連線池加總。閘道加入預算：`trustiot.postgres.connectionBudgets` 的 `ai-gateway` 是 `maxConnections` 2 到 32、`otherPools` 0（設定的連線池是它唯一的連線池），`services.ai-gateway.database.maxConnections` 預設 4，chart 以 `AI_GATEWAY__DATABASE__MAX_CONNECTIONS` 傳入、自訂 `env` 不能設定。五個用 Postgres 的服務各一個副本時需要 140 + 18 = 158 條，`max_connections` 預設 160。

- `chart_connections::the_chart_budgets_every_pool_the_service_opens`：程式裡唯一的 `.max_connections(...)` 是 `store.rs` 的 `config.database.max_connections`，chart 的範圍等於 `MIN_DATABASE_CONNECTIONS`、`MAX_DATABASE_CONNECTIONS`，`otherPools` 是 0。testkit 依賴 `contract`，不能進閘道的依賴樹（`no_contract`），所以這個測試自己讀同一段 JSON。
- `chart.test.ts` 的 `Postgres connection budget`：預設的五個服務、每副本 4、加總 158；剛好 158 時通過、157 時失敗並列出 `ai-gateway 4 x 2`；閘道拿到 `AI_GATEWAY__DATABASE__MAX_CONNECTIONS=4`（設 32 時是 32），33 與自訂 `env` 都渲染失敗。

### nightly（待補）

待 PR 帶 `smoke` label 的 nightly run 完成後補上：run 的連結、ai-gateway pod 就緒、readiness 的 `migrations` 與 `postgres` 檢查。
