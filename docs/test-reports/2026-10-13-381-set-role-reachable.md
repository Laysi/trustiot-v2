# 連線檢查：能以 SET ROLE 變成的角色也列入檢查（2026-10-13）

對應 issue：Laysi/trustiot-v2#381

## 環境

- 建置主機（check queue）：x86_64 Linux（WSL2），20 核心，Docker；測試用一次性的 `postgres:18.6-alpine` 容器（TLS，`verify-full`，記憶體上限 512 MB），指令經 check queue 執行，環境變數 `CI=1`。**這台主機與其他 agent 共用**，下面的絕對數字只供參考；main 與分支的查詢是同一次執行、同一個資料庫交錯執行量得，負載對兩者相同。
- 量測時的 load average（`/proc/loadavg`，1／5／15 分鐘）：批次開始 0.26 0.68 1.15；計時測試開始前 1.12 1.21 1.26，每個情境前後在 0.88 到 1.12 之間，結束 0.88 1.13 1.23（見 [batch5 log](data/2026-10-13-381-set-role-reachable/batch5-tests-mutations-differential-timing.log)）。
- Rust 1.99.0（`rust-toolchain.toml`）；sqlx 依 `Cargo.lock`；Postgres 18.6。
- 分支 `feat/381-set-role-reachable`，從 main `9beb8f7`（Laysi/trustiot-v2#375 的改寫，PR Laysi/trustiot-v2#382）開始。「main」指 main `9beb8f7` 的 `SESSION_CHECK`，原文放在探針檔裡（`OLD_SESSION_CHECK`）。
- Postgres 語意的小探針（`ADMIN` 選項、`SET` 鏈）在本機一次性的 `postgres:18.6-alpine` 容器以 psql 執行，見 [admin-option-probe.txt](data/2026-10-13-381-set-role-reachable/admin-option-probe.txt)。

## 改了什麼

`crates/service-kit/src/postgres.rs` 的 `SESSION_CHECK`：

1. `session_roles`：session 能以 `SET ROLE` 變成的每個角色，`pg_has_role(current_user, oid, 'SET')`，含連線角色自己（`own`）。Postgres 沿著每一段都有 `SET` 選項的成員關係判斷，所以經一串 `SET` 成員關係、或經它繼承的群組再 `SET` 才到得了的角色也算（探針確認：中間有一段 `SET FALSE` 就不算，`SET ROLE` 也被拒）。平台規則的 `platform_roles` 改取自這裡，內容不變。
2. `checked_roles`：其中連線角色自己，以及能變成、但不繼承的角色。連線角色也繼承的角色，權限都已是連線角色的，套用到它的 policy 也都套用到連線角色，它的問題連線角色一定也有，不另外檢查（差分測試逐狀態驗證，見下方 C4、C5）。
3. `reachable` 改成（角色、關聯）：每個 `checked_roles` 角色能用的租戶資料。`unprotected_table`、`unprotected_view`、`unprotectable`、`truncate_allowed` 逐角色判斷，`TRUNCATE` 看該角色的權限。
4. `permissive_policy`：要判斷的 permissive policy（在某個角色能用、啟用 RLS 的表上，不是一字不差的租戶條件，也不是平台連線以平台角色名稱拿到的）各印一次（`session_policies`）；policy 指名的角色套用到哪些角色，每一對問一次（`session_policy_roles`）：該角色繼承得到權限的角色（`pg_has_role(角色, policy 的角色, 'USAGE')`，Postgres 的判斷），連線角色自己另外照舊算它所屬的每個角色（`MEMBER`，只會多拒）；`PUBLIC` 套用到每個角色。
5. `definer_function`、`temporary_tables` 也逐角色判斷（評估見下）。`SECURITY DEFINER` 函式的擁有者豁免改為只豁免擁有者是連線角色、它繼承的角色或它能變成的角色。
6. 新問題 `administers_role`（`UnsafeSession::AdministersRole`）：對任何角色有 `ADMIN` 選項（`pg_has_role(..., 'MEMBER WITH ADMIN OPTION')`）的 session 被拒。
7. 這些列帶上角色名稱（`quote_ident`）。排序在（rank、關聯、問題）之後先放連線角色自己的列（`later`），同一個問題連線角色自己也有時，訊息寫連線角色。
8. `UnsafeSession` 的 `UnprotectedTable`、`PermissivePolicy`、`UnprotectedView`、`Unprotectable`、`DefinerFunction`、`TemporaryTables`、`TruncateAllowed` 改為帶 `role` 的 struct variant，訊息寫出持有權限的角色。

### `temporary_tables` 與 `definer_function` 的評估

兩者都要以能變成的角色判斷，已實作：

- 暫存表：Postgres 建暫存表時以當下的角色檢查 `TEMPORARY`，所以 `SET ROLE` 成有 `TEMPORARY` 的角色就能建；暫存表留在 session 裡，`RESET ROLE` 與交易結束後都在，pool 重用連線時下一個交易可以再 `SET ROLE` 讀它，和連線角色自己有 `TEMPORARY` 一樣危險。`tenant_db` 的「temporary tables for a SET ROLE target」以 `SET ROLE` 後建立暫存表成功證明。
- `SECURITY DEFINER` 函式：`SET ROLE` 之後就能執行那個角色有 `EXECUTE` 的函式，函式以擁有者的權限與 RLS 可見範圍執行。`tenant_db` 的「a SECURITY DEFINER function a SET ROLE target may execute」以 `SET ROLE` 後讀到每個租戶的 3 列證明。
- 擁有者豁免：以前連線角色是擁有者的任何成員（`MEMBER`）就豁免。擁有者是連線角色只屬於、既不能繼承也不能 `SET` 的角色時，函式以那個角色的權限執行，而它的權限沒有被任何一項檢查，所以改為只在擁有者是連線角色、它繼承或能變成的角色時豁免。這是 commit 前 Codex 審查指出的缺口；`tenant_db` 以這種函式在沒有租戶時讀到兩個租戶的列證明。

### `ADMIN` 選項

只有 `ADMIN` 選項（不能繼承也不能 `SET`）的成員可以把那個角色授予自己（`WITH SET TRUE`）再 `SET ROLE`（[admin-option-probe.txt](data/2026-10-13-381-set-role-reachable/admin-option-probe.txt)），所以「能變成的角色」不能只看現有的 `SET` 選項。檢查直接拒絕管理任何角色的 session，而不是把這些角色算進能變成的角色：session 也可以讓自己繼承那個角色，權限混進連線角色自己的，逐角色檢查涵蓋不了。`pg_has_role(..., 'MEMBER WITH ADMIN OPTION')` 經不繼承的成員關係取得的 `ADMIN` 也算（探針中 Postgres 不讓它實際授予：`no possible grantors`），只會多拒。

### 與驗收條件「既不能繼承也不能 SET ROLE 的成員關係判斷不變」的關係

上兩節的兩條規則對「既不能繼承也不能 `SET`」的成員關係變嚴：擁有者是這種角色的 `SECURITY DEFINER` 函式、以及有 `ADMIN` 選項的成員關係，以前接受，現在拒絕。其他情況（這種角色的表、檢視、policy、暫存表、`TRUNCATE` 權限）判斷不變。兩者都只會多拒，部署中不會出現（服務與平台角色不屬於任何角色，Laysi/trustiot-v2#60 的 migration Job 發現多出的成員關係就停止）。

## 指令

探針檔沒有 commit 成測試：量測與等價證明時放到 `crates/service-kit/tests/` 執行，跑完刪除。原始碼見 [verify_381.rs.txt](data/2026-10-13-381-set-role-reachable/verify_381.rs.txt)（由 [assemble.ts.txt](data/2026-10-13-381-set-role-reachable/assemble.ts.txt) 組合：Laysi/trustiot-v2#375 探針檔裡的 Laysi/trustiot-v2#366 獨立驗證情境原文、main `9beb8f7` 的 `SESSION_CHECK` 原文，加上本次的測試）。`tenant_db` 的突變以 [mutants.ts.txt](data/2026-10-13-381-set-role-reachable/mutants.ts.txt) 產生每個突變後的 `postgres.rs`，逐一換上後跑整個 `tenant_db`。

```sh
# 探針
bun assemble.ts docs/test-reports/data/2026-10-13-375-platform-check-cost/verify_375.rs.txt <main 9beb8f7 的 postgres.rs> <body> crates/service-kit/tests/verify_381.rs
cargo test --locked -p service-kit --test verify_381 -- --nocapture differential mutations
cargo test --locked -p service-kit --test verify_381 -- --nocapture --test-threads=1 timing

# tenant_db 與它的突變（每個突變：換上 mutants-381/NN.rs，跑 tenant_db，最後 git checkout 還原）
cargo test --locked -p service-kit --test tenant_db -- --nocapture
bun mutants.ts crates/service-kit/src/postgres.rs mutants-381
cp mutants-381/NN.rs crates/service-kit/src/postgres.rs && cargo test --locked -p service-kit --test tenant_db

# 最終檢查（探針刪除後，data 目錄的 final-checks.sh.txt）
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo test --locked -p service-kit
cargo test --locked -p service-kit --no-default-features
cargo test --locked -p outbox
cargo test --locked -p testkit
cargo test --locked -p identity
cargo test --locked -p sql-check -p repo-check
cargo run --locked -p openapi -- --check
```

## 結果

### 真 Postgres 測試（`tenant_db`）

新測試 `sessions_are_checked_for_every_role_they_can_become` 在自己的資料庫裡，對服務角色 `example_app`（與一個平台角色的情況）逐一做出設定、確認 `check_session` 的結果、以一般連線 `SET ROLE` 後在**沒有租戶**時實際執行來證明風險，再修正、確認接受。服務角色的成員關係（`ROLES`）：`example_app_switch`、`example_alias` 只能 `SET`；經 `example_app_switch` 可再 `SET` 成 `example_app_switch_parent`（`SET` 鏈）；`example_app_switch` 繼承 `example_app_switch_group`；繼承 `example_app_group`，經它可 `SET` 成 `example_app_group_switch`；只繼承 `example_app_inherited`，經它到不了 `example_app_inherited_switch`；`example_app_unusable` 既不能繼承也不能 `SET`。另有登入角色 `example_administrator`，只對 `example_app_switch` 有 `ADMIN` 選項。

| 設定 | 結果 | 沒有租戶時實際碰得到 |
| --- | --- | --- |
| `SET` 目標能用沒有 RLS 的表 | 拒絕，`UnprotectedTable`，角色 `example_app_switch` | `SET ROLE` 後 2 列（兩個租戶）；連線角色自己被拒 |
| 經 `SET` 鏈到的角色能用 | 拒絕，角色 `example_app_switch_parent` | 2 列 |
| 經繼承的群組再 `SET` 到的角色能用 | 拒絕，角色 `example_app_group_switch` | 2 列 |
| `SET` 目標繼承得到的權限 | 拒絕，角色 `example_app_switch` | 2 列；直接 `SET ROLE example_app_switch_group` 被拒 |
| 權限給 `PUBLIC` | 拒絕，角色寫連線角色 `example_app` | 連線角色與 `example_alias` 都 2 列 |
| 名稱排在連線角色前面的 `SET` 目標（`example_alias`） | 拒絕，角色 `example_alias` | 2 列 |
| 給 `SET` 目標的 `USING (true)` policy（只有它能用的表） | 拒絕，`PermissivePolicy`，角色 `example_app_switch` | 2 列 |
| 給 `PUBLIC` 的 `USING (true)` policy，表只有 `SET` 目標能用 | 拒絕，角色 `example_app_switch` | 2 列；連線角色自己被拒 |
| 給 `SET` 目標所繼承角色的 policy | 拒絕，角色 `example_app_switch` | 2 列 |
| 給 `SET` 目標「屬於但不繼承」的角色的 policy | **接受**（Postgres 不套用） | 0 列；那個角色自己沒有權限，被拒 |
| `SET` 目標能用不是 `security_invoker` 的檢視 | 拒絕，`UnprotectedView` | 3 列 |
| `SET` 目標能用物化檢視 | 拒絕，`Unprotectable` | 2 列 |
| `SET` 目標有 `TRUNCATE` | 拒絕，`TruncateAllowed` | `TRUNCATE` 成功；連線角色自己被拒 |
| `SET` 目標能執行 `SECURITY DEFINER` 函式 | 拒絕，`DefinerFunction` | 3 列；連線角色自己被拒 |
| `SET` 目標有 `TEMPORARY` | 拒絕，`TemporaryTables` | 建立暫存表成功；連線角色自己被拒 |
| 平台角色的 `SET` 目標能用沒有 RLS 的表 | 拒絕，`UnprotectedTable`，角色 `example_platform_switch` | 2 列 |
| 只繼承的角色的權限（照舊拒絕） | 拒絕，角色 `example_app` | 連線角色 2 列；`SET ROLE` 被拒 |
| 既不能繼承也不能 `SET` 的角色的表、檢視、`TRUNCATE`、函式 `EXECUTE`、`TEMPORARY`（照舊接受） | 接受 | 全部被拒 |
| 只繼承的成員關係後面才有 `SET`（照舊接受） | 接受 | `SET ROLE` 被拒 |
| 給既不能繼承也不能 `SET` 的角色的 policy，表連線角色能用（照舊拒絕） | 拒絕，`PermissivePolicy`，角色 `example_app` | 0 列：policy 實際上不套用，檢查照舊多拒 |
| 擁有者是既不能繼承也不能 `SET` 的角色的 `SECURITY DEFINER` 函式（**以前接受**） | 拒絕，`DefinerFunction`，角色 `example_app` | 經函式 2 列 |
| 對另一個角色有 `ADMIN` 選項（**以前接受**） | 拒絕，`AdministersRole`，角色 `example_app_switch` | 先 `SET ROLE` 被拒；把角色授予自己後 2 列 |

既有測試的預期改成新的 variant（角色為連線角色），結果不變。整個 `tenant_db`：**14 passed**（batch5 log 的 `tenant-db.log` 一節有每個情境的輸出）。

### 突變（`tenant_db`）

每個突變換上後跑整個 `tenant_db`，都至少有一個測試失敗（[batch5 log](data/2026-10-13-381-set-role-reachable/batch5-tests-mutations-differential-timing.log)）：

| # | 突變 | 失敗的測試 |
| --- | --- | --- |
| 1 | `reachable` 只看連線角色（`sr.own AND`） | 新測試：`SET` 目標的表被接受 |
| 2 | policy 是否套用改看連線角色而不是該角色 | 新測試：`switch_all` 被接受 |
| 3 | policy 套用不比對角色（任一能變成的角色繼承就算） | 新測試：`parent_only` 被拒（應接受） |
| 4 | 每個角色都以 `MEMBER` 判斷 policy 是否套用 | 新測試：`parent_only` 被拒（應接受） |
| 5 | 拿掉連線角色照舊的 `MEMBER` 判斷 | 新測試：`unusable_all` 被接受（應照舊拒絕） |
| 6 | `SECURITY DEFINER` 函式只看連線角色的 `EXECUTE` | 新測試 |
| 7 | 暫存表只看連線角色的 `TEMPORARY` | 新測試 |
| 8 | `TRUNCATE` 只看連線角色的權限 | 新測試 |
| 9 | 能變成的角色改為「所屬的角色」（`MEMBER`） | 新測試與 `platform_sessions_only_the_tenant_condition_opens_are_refused` |
| 10 | 能變成的角色改為「繼承的角色」（`USAGE`） | 新測試與 `platform_sessions_only_the_tenant_condition_opens_are_refused` |
| 11 | 不再優先寫連線角色（排序拿掉 `later`） | 新測試（`PUBLIC` 的權限寫成 `example_alias`）與 `sessions_row_level_security_would_not_protect_are_refused` |
| 12 | 平台例外放寬到指名任一能變成的角色的 policy | `sessions_row_level_security_would_not_protect_are_refused`：`group_access` 被接受 |
| 13 | 函式擁有者豁免改回任何成員關係 | 新測試：`unusable_names()` 被接受 |
| 14 | 不拒絕 `ADMIN` 選項 | 新測試：`example_administrator` 被接受 |
| 15 | `checked_roles` 反過來（只看繼承的） | 新測試：`SET` 目標的表被接受 |

### 差分測試（main 與分支）

同一個資料庫裡，每個目錄狀態在 superuser 的交易中建好，`SET ROLE` 成連線角色後執行兩個查詢（拿掉 `LIMIT 1` 取回所有列，另外各取 `LIMIT 1` 的第一列），以平台角色（`$1` true）與服務角色（`$1` false）兩種規則；再對每個能變成的角色執行獨立寫成的單角色規則（`ORACLE`），並由 superuser `SET ROLE` 成該角色執行 main 的查詢；最後整個 rollback（含建立的角色）。狀態：

- Laysi/trustiot-v2#366 獨立驗證的 45 個情境；
- 1500 個以固定種子隨機產生的狀態：Laysi/trustiot-v2#375 的產生器（0 到 4 個群組，成員關係涵蓋 INHERIT／SET 的各種組合與群組之間的鏈，20% 的連線角色是 NOINHERIT，`pg_read_all_data`／`pg_write_all_data`（成員選項也隨機），表、欄位與 `PUBLIC` 權限，各種 permissive／restrictive policy 與運算式），另外加上本次的檢查會看的：檢視（一半 `security_invoker`）、物化檢視、`TEMPORARY` 權限（含 `PUBLIC`）、`SECURITY DEFINER` 函式（擁有者是 migration 角色或某個群組，`EXECUTE` 隨機撤銷與授予）、4% 的狀態有一個只帶 `ADMIN` 選項的成員關係。

每個狀態檢查：

- **C1**：main 的每一列都在新查詢連線角色自己的列（清掉角色欄）與平台規則的列裡；多出的只能是新規則的兩種：擁有者是連線角色只屬於的角色的 `SECURITY DEFINER` 函式，以及 `administers_role`。沒有這兩種情況時兩者完全相同。
- **C2**：沒有 SET-only 成員關係、也沒有上述兩種情況的狀態，新查詢的列去掉角色欄後**與 main 完全相同**。
- **C3**：同上的狀態，`check_session` 回報的第一列與 main 相同（角色寫連線角色）；其他狀態 main 拒絕的新查詢一定拒絕；而且連線角色自己有回報的那個問題時，寫的是連線角色。
- **C4**：每個能變成、不繼承的角色（含連線角色自己），新查詢為它找到的列與 `ORACLE` 完全相同；連線角色也繼承的角色沒有任何列，而 `ORACLE` 為它找到的問題都在連線角色自己的問題裡；沒有任何列寫出不能變成的角色。
- **C5**：只看權限的問題（沒有 RLS 的表、檢視、物化檢視、暫存表、`TRUNCATE`），每個能變成、不繼承的角色的列，與 superuser `SET ROLE` 成該角色執行 main 的查詢的結果完全相同；繼承的角色的那些結果都在連線角色自己的問題裡。
- **C6**：`administers_role` 的列正好是 session 有 `ADMIN` 選項的角色。

結果：**1545 個狀態，failures: 0**，沒有查詢失敗。401 個狀態有 SET-only 成員關係，另 64 個狀態沒有 SET-only 成員關係但有上述兩種情況；1389 個能變成的其他角色中 863 個也被繼承（略過，C4／C5 驗證略過不影響結果）。新查詢比 main 多出（問題、關聯）的狀態：兩種規則各 241 個（log 的這一行寫「SET-only states」，實際計入所有狀態；依 C2，多出的只會出現在有 SET-only 成員關係或上述兩種情況的狀態）（多出的列依問題，兩種規則合計：`truncate_allowed` 208、`permissive_policy` 147、`administers_role` 142、`temporary_tables` 72、`unprotected_table` 52、`definer_function` 52、`unprotected_view` 30、`unprotectable` 24）；main 接受而新查詢拒絕的：平台 34、服務 39 個狀態。沒有一個狀態是 main 拒絕而新查詢接受。

**差分的突變**（`mutations`）：上表 15 個突變套在新查詢上，以同一套狀態檢查，每一個都在某個狀態違反 C1 到 C6（第一次違反的狀態見 log）。

### 成本

300 張照範本寫的表（RLS、`tenant_isolation`、以名稱給 `vc_p` 的 `platform_access`），服務角色 `vc_app` 對每張表有 `SELECT, INSERT, UPDATE, DELETE`。T1、T2 再給 `vc_app` 20、60 個只能 `SET` 的群組（`WITH INHERIT FALSE, SET TRUE`），每個群組對每張表有 `SELECT, INSERT, UPDATE, DELETE`。S0 到 S3 是 Laysi/trustiot-v2#375 的平台情境（群組是一般 `GRANT`，INHERIT 與 SET 都是 TRUE）。每個情境先確認兩個查詢都沒有任何列、`check_session` 接受，再交錯執行各 25 次：「同一連線」是同一個 session 重複執行，「新連線」是每次開新連線後的第一次執行（連線池實際的情況）。

| 情境（300 張表） | main 中位數／最大 | 分支 中位數／最大 | main 新連線 中位數／最大 | 分支 新連線 中位數／最大 |
| --- | --- | --- | --- | --- |
| T0 服務角色本身 | 7.4／9.4 ms | 9.2／10.3 ms | 33.6／38.4 ms | 35.3／40.4 ms |
| T1 + 20 個只能 `SET`、有權限的群組 | 7.6／9.5 ms | 17.7／20.1 ms | 34.5／37.5 ms | 45.1／49.0 ms |
| T2 + 60 個這樣的群組 | 7.7／9.6 ms | 35.1／37.0 ms | 35.0／36.6 ms | 64.1／66.7 ms |
| S0 平台角色本身 | 12.3／13.7 ms | 11.3／14.4 ms | 41.5／42.1 ms | 42.0／44.3 ms |
| S1 + 20 個沒有權限的群組 | 19.3／21.0 ms | 18.4／21.8 ms | 57.5／63.2 ms | 58.4／59.7 ms |
| S2 20 個有權限、被 policy 指名的群組 | 20.0／23.9 ms | 20.3／24.1 ms | 51.8／55.1 ms | 52.0／57.2 ms |
| S3 60 個這樣的群組 | 58.8／62.0 ms | 58.3／62.6 ms | 89.6／90.8 ms | 91.7／93.4 ms |

- 沒有 SET-only 成員關係時（T0、S0 到 S3）與 main 相當：服務角色本身多約 1.8 ms（新連線約 1.7 ms），平台情境在量測誤差內。S0 到 S3 的群組都被繼承，不另外檢查。
- 只有 SET-only 成員關係會增加成本，大致與「能變成、不繼承、有權限的角色數 × 它們能用的表數」成正比：每多一個對 300 張表都有權限的這種角色約多 0.45 ms（60 個時同一連線 35.1 ms、新連線 64.1 ms），離 readiness 的 800 ms `CHECK_TIMEOUT` 很遠。部署的服務與平台角色不屬於任何角色，實際走的是 T0／S0。
- 第一版沒有略過繼承的角色，S3 的分支中位數是 116.7 ms（新連線 149.0 ms），是 main（59.6 ms）的兩倍：S0 到 S3 的 60 個群組也能 `SET`，每個都重算一次可用的表（[timing-first-version.log](data/2026-10-13-381-set-role-reachable/timing-first-version.log)）。這些角色的問題連線角色一定也有（C4、C5 驗證），所以改為略過，S3 回到與 main 相當。第一版沒有 commit。
- S3 的 main 數字（58.8 ms）比 Laysi/trustiot-v2#375 報告的 39.1 ms 高：本次的計時資料庫另有 60 個 `vc_sg` 群組對每張表都有權限，每張表的 ACL 多 60 筆，兩個查詢的權限判斷都變慢；比較以同一次執行的 main 為準。
- 分支在 T2 的執行計畫見 batch5 log 的 `PLAN` 行。其中以 `reachable_4.oid` 分組的那一行被 gitleaks 的 `generic-api-key` 規則當成金鑰，log 裡在這個值的前後加上方括號，其餘輸出未改動。

### 最終檢查

探針與暫存檔刪除後在 head 上執行，結果見 [final-checks.log](data/2026-10-13-381-set-role-reachable/final-checks.log)（腳本為 [final-checks.sh.txt](data/2026-10-13-381-set-role-reachable/final-checks.sh.txt)）。`tools/sql-check` 把 SQL 字串裡的 `SET ROLE` 字樣當成切換角色，所以 `SESSION_CHECK` 的 SQL 註解不寫這個字樣。

## 範圍外與限制

- 部署中服務與平台角色不屬於任何角色；檢查對成員關係的判斷是縱深防禦。
- `administers_role` 以 `pg_has_role(..., 'MEMBER WITH ADMIN OPTION')` 判斷，涵蓋 Postgres 實際允許授予的範圍以外的情況（經不繼承的成員關係取得的 `ADMIN`），只會多拒。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 以 `SET`（沒有 `INHERIT`）能變成的角色對沒有 RLS 的租戶表有權限時拒絕，訊息指出表與角色；過寬的 permissive policy、沒有 `security_invoker` 的 view、TRUNCATE 權限經這種角色取得時各有拒絕的測試 | 通過：`tenant_db` 的 `sessions_are_checked_for_every_role_they_can_become`（另含物化檢視、`SECURITY DEFINER` 函式、暫存表、`SET` 鏈、經群組的 `SET`、`PUBLIC`、平台角色），每個都以 `SET ROLE` 證明風險 |
| 只有 `INHERIT`、或既不能繼承也不能 `SET ROLE` 的成員關係判斷不變；`CI=1 cargo test -p service-kit --locked`、`-p outbox`、`-p testkit` 通過 | 通過，有兩個刻意的例外：擁有者是「既不能繼承也不能 `SET`」角色的 `SECURITY DEFINER` 函式，與 `ADMIN` 選項，現在拒絕（都只會多拒，理由見上）。其餘由新測試的「照舊」情況與差分測試 C1／C2 證明；測試通過見最終檢查 |
| 把新規則改掉時新加的測試會失敗（突變測試記錄在 `docs/test-reports/`） | 通過：15 個突變都讓 `tenant_db` 失敗，也都被差分測試抓到 |
| README 的檢查項目表與決策 29 的實作細節記錄這條規則與 `temporary_tables`、`definer_function` 的評估結論 | 通過：`crates/service-kit/README.md`（檢查項目表的 ★ 與 `AdministersRole`、表後的說明、測試表）與 `docs/architecture/decisions.md` 決策 29 |
