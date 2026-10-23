# ai-gateway：模型角色存進 Postgres、每個請求確認設定世代（2026-10-23）

Laysi/trustiot-v2#605：ai-gateway 把模型角色的模型清單覆寫存進 Postgres 的 schema `ai_gateway`（`role_models`、`settings_generation`、append-only 的 `role_model_changes`），每個轉送請求讀一次設定世代、世代變了才重載；讀不到設定時以 503 拒絕而不呼叫供應商。chart 的 Postgres 接線改成依服務的設定前綴產生，ai-gateway 拿到 `AI_GATEWAY_POSTGRES_*` 與 `AI_GATEWAY__DATABASE__*`。這份報告是驗收條件的證據：除了最後一條（nightly），全部由下列自動化測試以 `CI=1` 實際執行（需要容器的測試不略過）。

## 環境

- **程式碼**：分支 `feat/605-ai-gateway-role-store`，rebase 到 main `74216494`（Laysi/trustiot-v2#604）之後的 commit。下表是 rebase 之後在同一棵樹上跑的結果。
- **機器**：建置主機（x86_64 Linux），經檢查佇列執行；容器測試由 podman 起 service-kit 的拋棄式 TLS Postgres（`crates/service-kit/tests/support/postgres.rs`，每個測試自己的資料庫，以 `ai_gateway_owner` 跑 `crates/service-kit/sql/database.sql`、建立 schema、跑 `0001_model_roles.sql` 與 migration 紀錄，與 migration Job 的順序相同）。
- **工具**：Rust 1.99.0（`rust-toolchain.toml`）、bun 1.4.2（`.prototools`）。
- **設定**：`CI=1`，沒有容器執行環境時測試失敗而不是略過。不連真正的供應商：供應商是 loopback 上的腳本化 HTTP/1.1 伺服器。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked --no-fail-fast -p ai-gateway
CI=1 cargo test --locked --no-fail-fast -p migrate -p sql-check -p repo-check
cargo tree -p ai-gateway --prefix none | grep -c '^contract '   # 0
bun test deploy/chart
bun install --cwd scripts --frozen-lockfile && bun test scripts
```

驗收證據可以單獨重跑：`CI=1 cargo test -p ai-gateway --test roles_store --test process`。

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p ai-gateway` | 單元 23、`decisions` 4、`fallback` 9、`messages` 7、`no_contract` 2、`process` 5、`roles_store` 7，全部通過；每個測試的名稱見 [ai-gateway-tests.txt](data/2026-10-23-605-ai-gateway-role-store/ai-gateway-tests.txt) |
| `cargo test -p migrate -p sql-check -p repo-check` | 全部通過，含 `registry` 6（ai-gateway 的 migration 登記與目錄一致）、`fresh_database` 1（叢集裡沒綁密碼的 `ai_gateway_app` 維持 NOLOGIN）、sql-check 26（`store.rs` 的 SQL 全是靜態字串） |
| `cargo tree -p ai-gateway` 的 `contract` | 0 行 |
| `bun test deploy/chart` | 636 通過、0 失敗 |
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

chart 的接線另由 `deploy/chart/tests/chart.test.ts` 檢查（`AI gateway settings`）：bundled 與 external 兩種 Postgres，閘道的 `AI_GATEWAY_POSTGRES_{HOST,PORT,DATABASE,SSLMODE}` 與平台 ConfigMap 的 `TRUSTIOT_POSTGRES_*` 相同、`AI_GATEWAY_POSTGRES_CA_FILE` 與 CA 掛載、`AI_GATEWAY__DATABASE__USERNAME=ai_gateway_app`、密碼來自 `<release>-ai-gateway-password`、沒有 `TRUSTIOT__DATABASE__*`、migration Job 綁定 `ai_gateway_app`；自訂 `env` 設 `AI_GATEWAY_POSTGRES_HOST`、`AI_GATEWAY__DATABASE__USERNAME` 或（小寫、經 `valueFrom`）`AI_GATEWAY__DATABASE__PASSWORD` 時渲染失敗；`mesh.calls.ai-gateway` 拿掉 `postgres` 時沒有這些變數。

### nightly（待補）

待 PR 帶 `smoke` label 的 nightly run 完成後補上：run 的連結、ai-gateway pod 就緒、readiness 的 `migrations` 與 `postgres` 檢查。
