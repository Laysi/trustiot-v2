# 身分服務連線池在斷線後的恢復（2026-10-11）

對應 issue：Laysi/trustiot-v2#341。原始資料在 [data/2026-10-11-341-pool-recovery/](data/2026-10-11-341-pool-recovery/)。

main 的 CI run 38149049315（main `1035426`）中，`services/identity/tests/ext_authz.rs` 的 `a_slow_database_connect_never_fails_a_check_after_ready` 在 `check 2 after recovery ([503, 503, 503, 503, 200])` 失敗。本報告記錄：原因的查證、修正後的連續執行結果，以及新測試對舊程式碼確實會失敗。

## 環境

| 項目 | 本機佇列 | 建置主機（ruisi-wsl） |
| --- | --- | --- |
| 機器 | 工作站，8 核，Linux 7.2.8（Fedora 45）x86_64 | WSL2，20 核，Linux 6.18.40.1-microsoft-standard-WSL2 x86_64 |
| 容器 | podman 6.1.3 | Docker 29.8.2 |
| 負載 | 測試期間 load average 約 0.7 到 1.8（佇列一次只跑一個工作） | 測試期間另外跑 40 個 nice 19 的忙迴圈（2 倍核心數），load average 19 到 41 |
| 工具 | Rust 1.99.0（`.prototools`、`rust-toolchain.toml`），sqlx 0.9.0 | 同左 |
| 程式碼 | 分支 `fix/341-identity-pool-recovery` | 同左 |

兩邊都經檢查佇列（`rcheck.sh`）執行，`CI=1`，所以需要容器的測試在沒有容器時會失敗而不是略過。測試以每條新連線延遲 1 秒的 TCP 代理（`SlowProxy`）模擬慢網路，切斷所有連線模擬資料庫換主。

## 指令

```sh
# 舊程式碼的查證（main dddd9ed 加上暫時的連線池日誌，未提交）：重複 20 次
cargo test --locked -p identity --test ext_authz a_slow_database_connect_never_fails_a_check_after_ready -- --nocapture

# 修正後：兩個慢連線測試連跑 30 次，第一次失敗就停。libtest 接受多個過濾字串，
# 符合任一個的測試都會跑（輸出是 "running 2 tests" 與 "7 filtered out"，見 data 目錄）
cargo test --locked -p identity --test ext_authz -- --nocapture \
  a_slow_database_connect_never_fails_a_check_after_ready \
  a_keep_warm_round_replaces_every_dropped_connection

# 新測試對舊程式碼：只把 services/identity/src/session.rs 換回 main 的版本
git show origin/main:services/identity/src/session.rs > services/identity/src/session.rs

# 其他檢查
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p identity
cargo test --locked -p repo-check
cargo test --locked -p sql-check
cargo run --locked -p openapi -- --check
```

建置主機上的負載：`for _ in $(seq 1 $(( $(nproc) * 2 ))); do nice -n 19 bash -c 'while :; do :; done' & done`，等 20 秒再開始。

## 結果

### 1. 舊程式碼的行為（[baseline-main.txt](data/2026-10-11-341-pool-recovery/baseline-main.txt)）

本機 20 次全部通過，沒有重現 CI 的失敗。日誌顯示：

- 切斷之後，3 條死連線都是被檢查自己取到的（sqlx 取用前 ping 失敗，`ping on idle connection returned error`，每次檢查一條），不是保溫工作。
- 保溫工作在連線池有 3 條時只對一條做 `SELECT 1`；死連線被 sqlx 在取用時重開，這一輪仍然成功，所以 20 次裡 readiness 一次都沒有失敗（沒有任何 `cannot keep the database connections open` 日誌）。
- 檢查放棄後還在跑的查詢各自在開新連線，連線池長到 5 條。

所以舊程式碼的 readiness 在換主後根本不會變，不能代表連線已經開好。CI 那次第一個 200 之後的 503，代表那個檢查還得開新連線：可能是還有死連線排在前面，也可能是放棄後仍在跑的查詢佔著連線、閒置的不夠兩條。CI 那次沒有留下服務日誌，無法確定是哪一種。新的作法與新測試對兩種都成立（見下）。

### 2. 修正後，本機 30 次（[repeat30-local-29085e5.txt](data/2026-10-11-341-pool-recovery/repeat30-local-29085e5.txt)）

`29085e5` 是最終 commit `450abb9` 的前一版，差別只在新測試的最後一步：原本只查一次 `/readyz`，最終版改成等它回到 200（service-kit 會沿用 readiness 的結果最多 1 秒），以及 README 的一句說明。

| 項目 | 結果 |
| --- | --- |
| 通過 | 30 / 30（每次兩個測試，各約 13 到 14 秒） |
| 切斷後保溫工作換掉的連線 | 29 次剛好 3 條，1 次 4 條 |
| 代理恢復後 readiness 回到 200 | 2.015 到 2.035 秒 |

恢復時間約 2 秒，是新連線約 1.2 秒（代理延遲 1 秒加 TLS、SCRAM、session 檢查）加上 service-kit 沿用 readiness 結果的 1 秒（`RESULT_TTL`）。同一批的其他檢查全部通過：[checks-local-29085e5.txt](data/2026-10-11-341-pool-recovery/checks-local-29085e5.txt)。

### 3. 修正後，建置主機高負載 30 次（[repeat30-loaded-host-29085e5.txt](data/2026-10-11-341-pool-recovery/repeat30-loaded-host-29085e5.txt)）

30 / 30 通過，load average 19 到 41（20 核），每次約 10.5 到 13.2 秒；恢復時間 2.013 到 2.026 秒。

### 4. 最終 commit，建置主機高負載 30 次與全部檢查（[final-host.txt](data/2026-10-11-341-pool-recovery/final-host.txt)）

最終的程式碼 commit `450abb9`，2026-10-11 23:58 到 2026-10-12 00:05（台北時間）在建置主機上執行：

| 項目 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test --locked -p identity` | 通過（單元 31、`adversarial` 11、`cross_tenant` 1、`ext_authz` 9、`token_claims` 8） |
| `cargo test --locked -p repo-check`、`-p sql-check` | 通過 |
| `cargo run --locked -p openapi -- --check` | 通過 |
| 兩個慢連線測試連跑 30 次，load average 21 到 41 | 30 / 30 通過；每次切斷後保溫工作剛好換掉 3 條；恢復時間 2.013 到 2.028 秒 |

### 5. 新測試對舊程式碼（[new-tests-against-main.txt](data/2026-10-11-341-pool-recovery/new-tests-against-main.txt)）

把 `session.rs` 換回 main 的版本：

- `a_keep_warm_round_replaces_every_dropped_connection` 失敗：`the keep-warm task did not notice the dropped connections`（等了 30 秒，舊的保溫工作換掉一條就繼續回報就緒）。
- `a_slow_database_connect_never_fails_a_check_after_ready` 通過：代理暫停時舊的保溫工作在取連線逾時（5 秒）後失敗，readiness 也會轉 503，接著走補齊連線的路徑，所以這個測試本身不足以區分新舊作法，區分靠上一個測試。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 依「重現」的步驟以 `CI=1` 在佇列中重複 30 次，全部通過，結果存到 `docs/test-reports/` | 通過 | 第 2、3、4 節：本機 30 / 30、建置主機高負載 30 / 30（兩批，最後一批在最終 commit），每次都包含目標測試 |
| 測試斷言：斷線後、readiness 回到 200 之後的檢查全部回 200 | 通過 | `a_slow_database_connect_never_fails_a_check_after_ready`：代理暫停時切斷，檢查回 503 且不帶身分標頭，readiness 自己轉 503；代理恢復、readiness 回到 200 之後 20 個檢查全部 200 且代理沒有接到新連線 |
| 有測試證明保溫工作每一輪都確認 3 條連線；README 已更新 | 通過 | `a_keep_warm_round_replaces_every_dropped_connection`（第 5 節證明它對舊程式碼失敗）；`services/identity/README.md`「連線池」 |
| `cargo clippy --workspace --all-targets -- -D warnings` 與 `cargo test -p identity` 都通過 | 通過 | 第 2、4 節的檢查輸出 |
