# 連線檢查：平台 session 檢查的成本（2026-10-13）

對應 issue：Laysi/trustiot-v2#375

## 環境

- 建置主機（check queue）：x86_64 Linux（WSL2），20 核心，Docker；測試用一次性的 `postgres:18.6-alpine` 容器（TLS，`verify-full`，記憶體上限 512 MB），指令經 check queue 執行，環境變數 `CI=1`。**這台主機與其他 agent 共用**，下面的絕對數字只供參考；改寫前後的比較是同一次執行、同一個資料庫、兩個查詢交錯執行量得，負載對兩者相同。
- 量測時的 load average（`/proc/loadavg`，1／5／15 分鐘）：執行開始 0.22 0.65 0.82；計時測試每個情境前後在 0.76 到 0.94 之間；整批結束 2.02 1.10 0.96（計時之後的 `tenant_db` 等測試造成）。main 的基準執行（只跑 Laysi/trustiot-v2#366 的探針檔）開始時 1.25 1.04 0.95、結束時 2.35 1.52 1.12（見 [rcheck-main.log](data/2026-10-13-375-platform-check-cost/rcheck-main.log)；log 第二行的 commit 是 check queue 副本的快照 commit，副本來自 main `e2e476b` 的 clone）。
- Rust 1.99.0（`rust-toolchain.toml`）；sqlx 依 `Cargo.lock`；Postgres 18.6。
- 分支 `perf/375-platform-check-cost`，從 main `e2e476b`（Laysi/trustiot-v2#366 的規則，PR Laysi/trustiot-v2#380）開始。「改寫前」是 main `e2e476b` 的 `SESSION_CHECK`，原文放在探針檔裡（`OLD_SESSION_CHECK`）。

## 改了什麼

只改 `crates/service-kit/src/postgres.rs` 的 `SESSION_CHECK` 平台部分；服務角色的部分、`CHECK_TIMEOUT`、檢查何時執行都不變，仍是每個新連線一次目錄查詢，只在平台角色的連線計算平台部分。

改寫前的成本來源：`platform_sides` 對每個（角色、表、5 個 side）列，對每條 policy 展開 `polroles` 並呼叫 `pg_has_role`，次數約是「角色數 × 表數 × 5 × 每表 policy 數 × 每條 policy 的角色數」；60 個群組時約 1100 萬次。改寫後：

1. 每條 permissive policy 只印出、判斷一次（`platform_policy_exprs`、`platform_policies`），把五個 side 編成位元：`has` 是這條 policy 在哪些 side 有運算式，`wider` 是哪些 side 的運算式開放超過 session 的租戶。判斷運算式的條件（租戶條件、`trustiot.tenant_id`、`current_setting(`、`false`、`pg_depend` 的 opaque 規則）原樣保留。
2. `PUBLIC` 的 policy 每張表只彙總一次（`platform_public`），所有角色共用。
3. 平台角色繼承哪個被 policy 指名的角色，每一對只問一次 `pg_has_role(..., 'USAGE')`（`platform_inherits`），再以 hash join 對上 policy 的角色、依（角色、表）彙總（`platform_named`）。
4. 每個（角色、表）把兩邊的位元 OR 起來，逐指令判斷：指令用到的 side 都有運算式，而且不是每一邊都有開放的運算式。這與改寫前「每一邊的運算式數 > 0，而且至少一邊開放的運算式數 = 0」相同：計數只用來判斷是否為 0，所以用存在與否（位元）即可，同一條 policy 經多個角色重複算到也不影響。
5. 角色名稱與表名只在最後對有問題的列查一次；中間的彙總只用 oid。

工作量因此約與「平台角色能用的角色數 × 它們能用的表數」加上「policy 數 × 每條 policy 的角色數 × 繼承它的角色數」成正比，見下方 S0 到 S3 的數字。

## 指令

探針檔沒有 commit 成測試：量測與等價證明時複製到 `crates/service-kit/tests/` 執行，跑完刪除。原始碼見 data 目錄的 [verify_375.rs.txt](data/2026-10-13-375-platform-check-cost/verify_375.rs.txt)（由 [assemble.ts.txt](data/2026-10-13-375-platform-check-cost/assemble.ts.txt) 組合：Laysi/trustiot-v2#366 獨立驗證的 45 個情境原文、main 的 `SESSION_CHECK` 原文，加上本次的測試）。

```sh
# 探針放進測試目錄（Laysi/trustiot-v2#366 的兩個探針檔原樣複製）
cp docs/test-reports/data/2026-10-13-375-platform-check-cost/verify_375.rs.txt crates/service-kit/tests/verify_375.rs
cp docs/test-reports/data/2026-10-13-366-platform-tenant-only/independent-verification/verify_366.rs.txt crates/service-kit/tests/verify_366.rs
cp docs/test-reports/data/2026-10-13-366-platform-tenant-only/independent-verification/verify_366-followup.rs.txt crates/service-kit/tests/verify_366_followup.rs

# 差分與突變（新舊查詢比對每一列）
cargo test --locked -p service-kit --test verify_375 -- --nocapture differential mutations
# 計時（單執行緒，避免與其他測試同時跑）
cargo test --locked -p service-kit --test verify_375 -- --nocapture --test-threads=1 timing
# Laysi/trustiot-v2#366 的獨立驗證情境（分支與 main e2e476b 各跑一次）
cargo test --locked -p service-kit --test verify_366 -- --nocapture
cargo test --locked -p service-kit --test verify_366_followup -- --nocapture
# 比對兩次的情境結果
bun outcomes.ts <log> <out>   # data 目錄的 outcomes.ts.txt；兩份輸出 diff 為空

# 最終檢查（探針刪除後）
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo test --locked -p service-kit
cargo test --locked -p outbox
cargo test --locked -p testkit
cargo test --locked -p repo-check
cargo run --locked -q -p openapi -- --check
```

## 結果

### 成本

與 Laysi/trustiot-v2#366 獨立驗證的成本測試同樣的資料庫與規模：300 張照範本寫的表（RLS、`tenant_isolation`、以名稱給 `vc_p` 的 `platform_access`），平台角色 `vc_p` 對每張表有 `SELECT, UPDATE`。S1 再加 20 個沒有權限的群組（一般 `GRANT`，INHERIT 與 SET 都是預設的 TRUE）；S2 讓這 20 個群組對每張表有 `SELECT, INSERT, UPDATE, DELETE`，並列在每張表的 `platform_access`；S3 增為 60 個這樣的群組。每個情境先確認新舊查詢不帶 `LIMIT 1` 的結果完全相同，且 `check_session` 接受；再交錯執行新舊查詢各 25 次：「同一連線」是同一個 session 預備好的查詢重複執行（與 Laysi/trustiot-v2#366 的量法相同），「新連線」是每次開新連線後的第一次執行（連線池實際執行檢查的情況，含 Postgres 的目錄快取暖機）。原始輸出見 [rcheck-branch.log](data/2026-10-13-375-platform-check-cost/rcheck-branch.log)。

| 情境（300 張表） | 改寫前 中位數／最大 | 改寫後 中位數／最大 | 改寫前 新連線 中位數／最大 | 改寫後 新連線 中位數／最大 |
| --- | --- | --- | --- | --- |
| S0 平台角色本身 | 18.1／19.9 ms | 12.6／14.2 ms | 45.1／46.3 ms | 38.8／40.9 ms |
| S1 + 20 個沒有權限的群組 | 22.5／24.9 ms | 16.2／17.6 ms | 58.2／61.4 ms | 51.0／53.5 ms |
| S2 20 個有權限、被 policy 指名的群組 | 166.1／170.6 ms | 22.4／24.6 ms | 196.5／203.9 ms | 51.0／55.4 ms |
| S3 60 個這樣的群組 | 679.5／721.9 ms | **39.1／42.6 ms** | 712.6／721.8 ms | **67.0／72.4 ms** |

- 60 個群組時中位數 39.1 ms（新連線 67.0 ms），低於 100 ms 的驗收門檻，離 readiness 的 800 ms `CHECK_TIMEOUT` 很遠；改寫前在同一次執行是 680 ms／713 ms。
- 隨角色數近乎線性：S0（1 個角色）12.6 ms、S2（21 個）22.4 ms、S3（61 個）39.1 ms，每多一個有權限的角色約多 0.4 ms。S0、S1 也比改寫前快，因為 `PUBLIC` 的 policy 不再逐角色比對。
- Laysi/trustiot-v2#366 探針檔自己的成本測試（每情境 15 次，`check_session`）在分支上兩次得到 S3 中位數 38.9 ms（最大 42.5、45.5 ms）；main 上同樣的測試是 694.5 ms（最大 3334.7 ms）與 719.5 ms（最大 739.3 ms），偶發的秒級最大值與 Laysi/trustiot-v2#366 報告記錄的 3.27 s 相同。
- 服務角色不受影響：同一份成本測試中 7.6 到 7.9 ms（main 上 10.3 到 10.5 ms，主機負載不同）。
- S3 的執行計畫（`EXPLAIN (ANALYZE, COSTS OFF)`）見 [session-check-plan-60-groups.txt](data/2026-10-13-375-platform-check-cost/session-check-plan-60-groups.txt)。

第一版改寫（policy 判斷一次、`pg_has_role` 每對一次，但仍以（角色、policy）去重複後依含名稱的欄位排序彙總）在同樣的量測得到 S3 中位數 134.6 ms，沒有達到門檻；執行計畫顯示成本在 `PUBLIC` policy 的逐角色比對、去重複與以文字欄位排序。上面的版本改成位元彙總、`PUBLIC` 每表一次、只用 oid 彙總後達到 39.1 ms。第一版沒有 commit。

### 判斷不變

**差分測試**（`differential`）：在同一個資料庫裡，每個目錄狀態在 superuser 的交易中建好，`SET ROLE` 成平台角色後執行新舊查詢（拿掉 `LIMIT 1`，取回所有問題列並排序比對），再整個 rollback（含建立的角色）。`$1` 為 true（平台角色）與 false（服務角色）都比。狀態包括：

- Laysi/trustiot-v2#366 獨立驗證的 45 個情境（第 1 到第 4 節，含 INHERIT／SET 的各種組合、NOINHERIT 角色、改寫租戶條件、`pg_read_all_data`／`pg_write_all_data`、`PUBLIC` 權限、欄位權限、其他 schema、分割表、`FOR UPDATE`／`ON CONFLICT`／`MERGE`、只有 WITH CHECK 的 UPDATE policy、經自訂函式、運算子與 view 讀租戶的 policy）；
- 1500 個以固定種子隨機產生的狀態：0 到 4 個群組，平台角色與群組間、群組之間的成員關係（預設、INHERIT TRUE／FALSE × SET TRUE／FALSE），20% 的平台角色是 NOINHERIT；偶爾加入 `pg_read_all_data`、`pg_write_all_data`；一半在 `public`、一半在另一個 schema（USAGE 隨機給）的 1 到 3 張表，部分是分割表，85% 啟用 RLS；表與欄位權限隨機給平台角色、群組或 `PUBLIC`；每張表 0 到 4 條 policy，20% 是 restrictive，指令為 ALL、SELECT、INSERT、UPDATE、DELETE，角色為 `PUBLIC` 或平台角色、群組、無關角色的組合，運算式從租戶條件、`true`、`false`、`tenant_id IS NULL`、改寫的租戶條件、內建函式、自訂函式（讀租戶與不讀租戶的）、`<租戶條件> OR true`、經 view 讀租戶等隨機取，USING 與 WITH CHECK 各自有無。

結果：1545 個狀態，**新舊查詢每一列都相同（mismatches: 0）**，舊查詢沒有失敗的；其中 728 個狀態有 `platform_tenant_only` 的列（共 2242 列）。以平台角色看，`check_session` 會回報的第一個問題分布：接受 317、`platform_tenant_only` 617、`permissive_policy` 260、`unprotected_table` 220、`truncate_allowed` 131，所以狀態涵蓋接受與各種拒絕。

**突變測試**（`mutations`）：把新查詢故意改壞，以同一套狀態比對舊查詢，每一個都被抓到：

| 突變 | 第一次不同的狀態 |
| --- | --- |
| 平台角色改為「是成員的角色」（`SET` → `MEMBER`） | 第 7 個（情境 1.7） |
| 計入 restrictive policy（拿掉 `polpermissive`） | 第 48 個（種子 3） |
| 拿掉 `pg_depend` 的 opaque 規則 | 第 38 個（情境 4.1） |
| policy 角色以成員關係比對（`USAGE` → `MEMBER`） | 第 1 個（情境 1.1） |
| 拿掉 `PUBLIC` 的 policy | 第 1 個（情境 1.1） |
| 拿掉以名稱指定角色的 policy | 第 3 個（情境 1.3） |
| UPDATE 要兩邊都只有租戶條件才拒絕 | 第 36 個（情境 3.16） |
| `FOR ALL` 不算進 INSERT | 第 4 個（情境 1.4） |
| WITH CHECK 的運算式一律不算開放 | 第 33 個（情境 3.13） |
| 沒有 USING 時也當成有運算式 | 第 47 個（種子 2） |

**Laysi/trustiot-v2#366 的探針檔**：`verify_366.rs.txt` 與 `verify_366-followup.rs.txt` 原樣在分支與 main `e2e476b` 上各跑一次，以真實登入的連線跑 82 個情境（37＋45）。抽出每個情境的檢查結果、各探針碰得到的列數與判定（[probe-outcomes.txt](data/2026-10-13-375-platform-check-cost/probe-outcomes.txt)，由 [outcomes.ts.txt](data/2026-10-13-375-platform-check-cost/outcomes.ts.txt) 從兩份 log 產生）後 diff：**完全相同**。兩邊列出的 mismatches 也相同，都是 Laysi/trustiot-v2#366 已記錄的限制：2.6、2.7（以名稱給平台角色、不讀租戶也不是全部租戶的 policy 通過檢查，service-kit README 有說明，需搭配 restrictive policy）與 4.7、4.8（自訂函式一律當成可能讀租戶而拒絕，fail closed）。

**既有測試**：`tenant_db`（13 個測試，含 Laysi/trustiot-v2#366 的拒絕與接受案例）在改寫後通過；最終檢查見下節。

### 最終檢查

探針刪除後在 head 上執行，結果見 [final-checks.log](data/2026-10-13-375-platform-check-cost/final-checks.log)：fmt、workspace clippy、service-kit 不含預設 feature 的 clippy、`cargo test -p service-kit`（含 `tenant_db`）、`-p outbox`、`-p testkit`、`-p repo-check`、`openapi --check` 全部通過。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 300 張表、60 個群組時的中位數與最大值寫進報告並與優化前比較；中位數低於 100 ms | 通過：改寫後 39.1／42.6 ms（新連線 67.0／72.4 ms），改寫前同一次執行 679.5／721.9 ms（新連線 712.6／721.8 ms） |
| Laysi/trustiot-v2#366 的測試與獨立驗證情境（`verify_366.rs.txt`、`verify_366-followup.rs.txt`）結果不變；`CI=1 cargo test -p service-kit --locked` 與 `-p outbox` 通過 | 通過：82 個情境的結果與 main 逐行相同；差分測試 1545 個狀態每列相同；service-kit、outbox、testkit 測試通過 |
