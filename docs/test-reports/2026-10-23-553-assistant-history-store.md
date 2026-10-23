# assistant：對話歷史的 Postgres 儲存（2026-10-23）

Laysi/trustiot-v2#553：`services/assistant` 的對話與歷史存進自己的 Postgres schema（`assistant.conversations`、`assistant.messages`），只能新增、訊息以 `bytea` 原樣保存、依決策 29 啟用 RLS；migration 登錄在 `tools/migrate`。這份報告是四條驗收條件的證據，全部由下列自動化測試以 `CI=1` 實際執行（需要容器的測試不略過）。服務本身還沒有連 Postgres：資料庫設定、連線池與 readiness 檢查屬於部署工作 Laysi/trustiot-v2#552，對話 API 呼叫 `store` 是 Laysi/trustiot-v2#146。

## 環境

- **程式碼**：分支 `feat/553-assistant-history-store`，rebase 到 main `c96d059` 之後：儲存、migration 與測試的 commit，加上安全審查後的修正（`store::append` 自己核對擁有者與 preamble）。下表的結果是 rebase 之後重跑的；兩個突變檢查在 rebase 之前做，main 新增的變更沒有碰到 assistant。
- **機器**：開發者工作站（x86_64 Linux），經本機的檢查佇列執行；容器測試由 podman 起拋棄式的 TLS Postgres（`crates/testkit` 的 `product_postgres`，套用 migration Job 本身建立的整個產品 schema）。
- **工具**：Rust 1.99.0（`rust-toolchain.toml`）、bun 1.4.2（`.prototools`）。
- **設定**：`CI=1`，沒有容器執行環境時測試失敗而不是略過。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy -p assistant -p migrate --all-targets -- -D warnings
CI=1 cargo test --locked -p assistant -p migrate -p testkit --no-fail-fast -- --nocapture
cargo test --locked -p repo-check -p sql-check
cargo run --locked -q -p openapi -- --check
bun install --cwd scripts --frozen-lockfile && bun test scripts
cargo deny --locked check
```

驗收證據可以單獨重跑：`CI=1 cargo test -p assistant --lib --test history_store --test cross_tenant -- --nocapture`。

突變檢查（確認測試抓得到退化，做完就還原）：

1. 在 `services/assistant/migrations/postgres/0001_assistant.sql` 暫時加上 `GRANT UPDATE, DELETE ON assistant.messages TO assistant_app`，跑 `CI=1 cargo test -p assistant --test cross_tenant --test history_store`。
2. 在 `services/assistant/src/store.rs` 暫時停用 `append` 對擁有者與 preamble 的比對，跑 `CI=1 cargo test -p assistant --lib store::`。

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy -p assistant -p migrate --all-targets -- -D warnings` | 通過 |
| `CI=1 cargo test -p assistant ...`（assistant 部分） | 單元 18、`agent` 12、`cross_tenant` 1、`history_store` 5、`tools` 7，全部通過；輸出見 [assistant-tests.txt](data/2026-10-23-553-assistant-history-store/assistant-tests.txt) |
| 同一次執行的 migrate 與 testkit 部分 | 全部通過，含 `registry` 6、`fresh_database` 1；每個測試執行檔的結果見 [migrate-testkit-summary.txt](data/2026-10-23-553-assistant-history-store/migrate-testkit-summary.txt) |
| `cargo test -p repo-check -p sql-check` | 通過 |
| `cargo run -p openapi -- --check` | `api/openapi.json is up to date`（這次沒有新增 API） |
| `bun test scripts` | 1590 通過、0 失敗（1599 個測試，51 個檔案）；見 [repo-checks.txt](data/2026-10-23-553-assistant-history-store/repo-checks.txt) |
| `cargo deny --locked check`（rebase 之前） | bans、licenses、sources 通過；advisories 只報 `ctutils 0.4.3` 的 RUSTSEC-2026-0385（2026-10-23 公布）。這是 main 的 `Cargo.lock` 本來就有的問題，由 Laysi/trustiot-v2#586（PR Laysi/trustiot-v2#587）另外修正；這次的 `Cargo.lock` 只為 assistant 加上 `sqlx` 與 `testkit` 兩條相依，沒有新套件 |
| 突變 1（給服務角色 UPDATE、DELETE） | `the_service_role_cannot_change_or_remove_stored_history` 失敗：`assistant_app's UPDATE on assistant.messages`，left `true`；見 [mutation-grants.txt](data/2026-10-23-553-assistant-history-store/mutation-grants.txt) |
| 突變 2（停用擁有者比對） | `append_refuses_a_conversation_of_another_owner_or_preamble` 失敗：`append of another owner from 0`；見 [mutation-owner-check.txt](data/2026-10-23-553-assistant-history-store/mutation-owner-check.txt) |

跨租戶套件（`tests/cross_tenant.rs`）的 26 個探測全部通過：兩張表都已宣告，兩個租戶互相讀不到、改不到、插不進對方的列，沒有租戶的 session 看不到任何列；唯一豁免的是 migration Job 的紀錄表 `assistant.trustiot_migrate_applied`（只有檔名與雜湊）；目前沒有 HTTP API，由 Laysi/trustiot-v2#146 補上端點宣告。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 存進去的對話讀回來逐位元組相同，含 thinking 區塊與 signature | 通過 | `history_store::a_stored_conversation_reads_back_byte_for_byte_and_continues`：主 agent 跑完一個含 thinking（附 signature）、工具呼叫與工具結果的問題後存入；superuser 直接讀 `body` 與原訊息逐位元組比對，模型回合與收到的 `content` 原文相同、兩個 thinking 區塊的 signature 不變；`store::load` 讀回的歷史相同；載入後接著問，送出的請求在 `messages` 之前（工具、system、設定）與第一次的請求逐位元組相同，`messages` 以原本的歷史開頭 |
| 服務角色在資料庫層被拒絕 UPDATE 與 DELETE | 通過 | `history_store::the_service_role_cannot_change_or_remove_stored_history`：`assistant_app` 對兩張表只有 SELECT、INSERT；8 個 UPDATE、DELETE、TRUNCATE 敘述在租戶交易內與沒有租戶的 session 都回 `42501`，歷史不變；突變 1 證明測試抓得到多給的權限 |
| 跨租戶負向測試通過 | 通過 | `cross_tenant::tenants_are_isolated`，26 個探測、0 失敗 |
| `cargo test -p migrate` 在 migration 登錄後通過 | 通過 | `registry`（檔案清單、禁用敘述、chart 的 platform 登入）與 `fresh_database`（`assistant_app` 在叢集維持 NOLOGIN）等全部通過 |

其他行為也有測試：重複寫入不改變什麼，兩個寫入者分岔時後者被拒、不覆寫，空洞與沒建立的對話被拒（`appending_is_idempotent_and_never_overwrites`）；其他租戶與同租戶的其他使用者載入不到（`other_tenants_and_other_users_get_nothing`）；以已存的 ID 但不同擁有者或 preamble 建出的對話，`create` 與 `append` 都回 `Conflict`、什麼都不寫（`store::tests::append_refuses_a_conversation_of_another_owner_or_preamble`）；損壞的列讓載入失敗（`a_corrupt_history_fails_the_load`）。

待測：服務接上資料庫後在叢集裡的端到端行為（連線、readiness 的 migration 檢查），待 Laysi/trustiot-v2#552 部署時驗證；大量或很大的訊息（例如圖片）的儲存量與效能屬於進階驗證，這次沒有量測。
