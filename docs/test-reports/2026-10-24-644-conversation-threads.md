# assistant 對話串：軟刪除、30 天後清除、捨棄最後一輪、提問紀錄

對應 Laysi/trustiot-v2#644。

## 環境

- 程式碼：分支 `feat/644-assistant-conversation-store`，基於 main `0dc8fd96`；實作在 `bbf0b679`，之後合併 main `757f52df`（`c5f9e9a9`），再修正 `tools/migrate/tests/fresh_database.rs` 的角色清單（`94e9f143`）。2026-10-24 安全審查之後的修正在 `46c8d115`，之後合併 main `304de010`（`a0e315c9`）與 `509c3f1f`（`40cb2627`）；2026-10-27 在 `40cb2627` 重跑全部檢查（見「合併後的重跑」）。
- 建置主機（x86_64 Linux）：Rust 的 clippy 與測試，`CI=1`，所以容器測試在沒有容器執行環境時失敗而不是略過。Postgres 是 testkit 起的拋棄式 TLS Postgres（`testkit::product`），由 migration Job 本身套用整個產品的 schema。
- 開發者工作站（x86_64 Linux），Bun 1.4.2：`cargo fmt` 與兩個 Bun 測試檔。

## 指令

```sh
# 建置主機
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p assistant
cargo test --locked -p migrate -p sql-check -p repo-check -p testkit
cargo test --locked -p assistant --test conversation_threads --test history_store --test cross_tenant

# 工作站
bun test scripts/ci/changes.test.ts
bun test scripts/tests/product-logins.test.ts scripts/tests/migrate.test.ts
```

## 結果

| 檢查 | commit | 結果 |
| --- | --- | --- |
| `cargo fmt --all --check` | `bbf0b679` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | `bbf0b679` | 通過 |
| `cargo test -p assistant` | `bbf0b679` | 全部通過：單元測試 34、`conversation_threads` 9、`cross_tenant` 1、`history_store` 5、`startup` 1、其他照舊（[data/…/checks-before-merge.txt](data/2026-10-24-644-conversation-threads/checks-before-merge.txt)） |
| `cargo test -p migrate` | `bbf0b679` | `fresh_database` 失敗：測試列出沒有密碼的角色，少了新的 `assistant_platform`。`94e9f143` 補上；補上後的重跑見「合併後的重跑」 |
| `conversation_threads`、`history_store`、`cross_tenant`（含觸發器修正後） | `bbf0b679` 的工作樹 | 9、5、1 全部通過 |
| `bun test scripts/ci/changes.test.ts` | `bbf0b679` | 282 pass、0 fail |
| `bun test scripts/tests/product-logins.test.ts scripts/tests/migrate.test.ts` | `94e9f143` | 23 pass、0 fail |

開發過程中的兩個失敗與修正（都在提交前）：

- 一開始以服務角色造成唯一鍵衝突來測驗收條件 9，Postgres 沒有送 DETAIL：對受 RLS 約束的角色，Postgres 不在錯誤裡寫出鍵值。測試改由超級使用者造出帶 DETAIL（含租戶與對話 ID）的錯誤，再確認 `StoreError` 不含它們。
- 32 MiB 的訊息先撞上 0001 已有的 `messages_body_check`（本文要以 `{` 開頭、`}` 結尾），測試改用合格的 JSON 物件，並確認拒絕的是 `messages_body_size`。

pre-commit 的審查擋下第一次提交：服務角色可以把刪除中的對話的 `deleted_at` 一直改成新的 `now()`，讓它永遠不被清除。觸發器 `conversations_clock` 改成刪除時間在還原前不能換，`the_database_keeps_its_own_times_and_the_size_limit` 加測這一條（`42501`），重跑後提交通過審查。

## 安全審查後的修正

`18e21a06` 的安全審查找到一個 High。0001 的 `tenant_isolation` 沒有指定角色，所以也套用到清除角色 `assistant_platform`；permissive policy 之間是 OR，平台交易只要自己設定租戶，就看得到、刪得掉那個租戶的每個對話。修正在 `46c8d115`：加上只給平台角色的 restrictive policy `platform_expired_only`。

`expired_conversations_are_purged_and_nothing_else` 新增一段探測。平台交易先以 `set_config` 設定擁有者的租戶（這段寫在 `tests/platform_tenant/mod.rs`，sql-check 只對這個檔案免除 `SessionSetting`），然後：

- 對最近刪除與沒刪除的對話，`SELECT` 看到 0 筆、`DELETE` 刪掉 0 筆；
- 不帶條件的 `DELETE` 只碰到 2 個過期的對話。

最後 rollback。測試輸出：

```text
purge role with the owner's tenant set: saw 0 and deleted 0 of the recent and the live conversation; an unfiltered DELETE reached only the 2 expired ones (rolled back)
```

## 合併後的重跑

2026-10-27 在 `40cb2627`（含修正、合併 main `509c3f1f`）上跑，建置主機（x86_64 Linux），`CI=1`。輸出見 [data/…/checks-2026-10-27-merged.txt](data/2026-10-24-644-conversation-threads/checks-2026-10-27-merged.txt)。

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test --locked -p assistant` | 全部通過：單元測試 51、`conversation_threads` 9（含上面的探測）、`cross_tenant` 1、`history_store` 5，其他測試檔照舊通過 |
| `cargo test --locked -p sql-check` | 20 與 6 個測試通過 |
| `cargo test --locked -p repo-check` | 17 與 9 個測試通過 |
| `cargo test --locked -p migrate` | 全部通過（含 `fresh_database`） |
| `cargo run --locked -p openapi -- --check` | `api/openapi.json is up to date` |

## 結論

| # | 驗收條件 | 結果 | 測試 |
| --- | --- | --- | --- |
| 1 | 刪除後擁有者與同租戶其他人都當作不存在，擁有者的已刪除清單看得到；30 天內還原後全部回來、訊息逐位元組相同 | 通過 | `a_deleted_conversation_is_gone_until_its_owner_restores_it`：還原後超級使用者讀到的對話列、訊息、捨棄紀錄、提問紀錄與刪除前相同 |
| 2 | 超過 30 天不能還原；清除後訊息、捨棄紀錄、提問紀錄都不在；未過期與沒刪除的不受影響；還原與清除同時只有一個結果 | 通過 | `expired_conversations_are_purged_and_nothing_else`：兩種先後各一次，以 `pg_blocking_pids` 確認後到的一方在等鎖 |
| 3 | 清除角色只刪得掉過期的；服務角色 DELETE、UPDATE 訊息仍是 `42501` | 通過 | 同上；平台交易自己設定租戶時也一樣（「安全審查後的修正」） |
| 4 | 刪除與 `store::append` 同時只有兩種結果，刪除後沒有新訊息 | 通過 | `a_delete_and_an_append_run_one_after_the_other` |
| 5 | 捨棄最後一輪後歷史逐位元組回到之前、新一輪接在後面、重新載入順序正確；捨棄不是最後一輪的被拒 | 通過 | `discarding_the_last_round_restores_the_history_before_it` |
| 6 | 同時開始兩個提問只有一個成功；過了截止時間的在下一次開始時變成 `interrupted` | 通過 | `one_question_runs_at_a_time` |
| 7 | 同租戶其他使用者的所有操作都當作不存在、資料不變 | 通過 | `another_user_of_the_tenant_finds_nothing` |
| 8 | 指定 `created_at` 與超過上限的 `body` 被資料庫拒絕 | 通過 | `the_database_keeps_its_own_times_and_the_size_limit` |
| 9 | `StoreError` 不含 Postgres 的訊息與 DETAIL | 通過 | `store_errors_carry_no_database_text` |
| 10 | `cargo test -p assistant`、`cargo test -p migrate`、`cargo clippy --workspace` 通過 | 通過 | `40cb2627` 上全部通過（「合併後的重跑」） |

2026-10-24 合併 main 之後的重跑，因為建置主機空間不足而沒有執行；2026-10-27 在 `40cb2627` 補跑完成（「合併後的重跑」）。
