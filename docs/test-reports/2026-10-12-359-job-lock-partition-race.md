# job_lock 分區測試的競態（Laysi/trustiot-v2#359）

驗證 `crates/service-kit/tests/job_lock.rs` 的兩個分區測試（`a_job_that_panics_in_a_partition_gives_up_its_lock_within_the_timeout`、`a_partitioned_holder_stops_its_job_within_the_lock_timeout`）不再依賴「伺服器端看到鎖，就代表客戶端已拿到鎖」，並記錄修正前會穩定失敗的重現方式、修正後在同樣條件與高負載下的結果。只改測試，`crates/service-kit/src/job_lock.rs` 沒有動。

## 環境

| 項目 | 內容 |
| --- | --- |
| 機器 | 建置主機（WSL2 上的 Docker Desktop，20 核心），經 repo 外的檢查佇列執行；Postgres 為測試自己啟動的容器（`postgres:18.6-alpine`，每個測試一個） |
| 日期 | 2026-10-12 |
| 測試對象 | 分支 `test/359-job-lock-partition-race`，基底 `origin/main` 的 `ebbe88e`（出問題的 CI run 是 `d8f8299`，含同一個 `ebbe88e`）；最後一次全 workspace 檢查在 rebase 到較新的 `origin/main` 之後，見「最終檢查」 |
| 設定 | `CI=1`，`--locked`，debug profile |
| 負載 | 見各節；負載以 `uptime` 的 1 分鐘 load average 表示（主機 20 核心） |

## 原因（已驗證）

CI 失敗訊息（Laysi/trustiot-v2#359 的 log）依序是：測試自己的 `pg_locks` 查詢、`SELECT pg_try_advisory_lock($1)` 超過 1 秒、`job lock query timed out`（`JobLock::try_acquire` 的訊息）、`scheduled job lock failed … lost the lock`；沒有 `scheduled job panicked`。對照 `job_lock.rs` 與 `src/job_lock.rs`：

1. 伺服器授予鎖，與客戶端收到回覆之間有一段時間；測試一看到 `pg_locks` 有持有者就呼叫 `proxy.partition()`，回覆被丟掉，`try_acquire` 在 `PARTITION_TIMEOUT` 後放棄，job 從未被呼叫，所以等再久也不會有 `panicked` 事件。`a_partitioned_holder_stops_its_job_within_the_lock_timeout` 用同樣的方式同步（`lock_holder` 迴圈），同一個假設，後面會在 `expect("the job ran")` 失敗。
2. 就算 job 已經開始，`run_exclusive` 的 `tokio::select!` 不是 `biased`：測試在分區之後才設定 `panic_now`，runtime 若被餓過約 800 ms，`JobLockGuard::watch` 先判定失去鎖，job 被丟棄而不會 panic。

`taking_and_releasing_the_lock_give_up_within_the_timeout_in_a_partition` 沒有這個假設：它用 `try_acquire` 回傳的 guard 當作「客戶端已拿到鎖」的證據，之後才分區，不查伺服器端的鎖。只加了一行註解說明。

## 變更（只有 `crates/service-kit/tests/job_lock.rs`）

- 測試用 proxy 新增 `Proxy::delay_replies`：把 Postgres 對「呼叫當下已開啟的連線」的回覆延後固定時間（`REPLY_DELAY`，300 ms）再轉送，期間若分區開始，回覆被丟棄（和網路丟掉還在途中的封包一樣）。這把競態的視窗變成必然發生，修正後的兩個測試都固定開著它。之後才開的連線不延後，因為建立連線需要好幾個回覆，延後會讓重新連線超過 1 秒的鎖逾時。預設不延後，其他測試不受影響。
- `a_job_that_panics_in_a_partition_gives_up_its_lock_within_the_timeout`：不再靠 `pg_locks` 與測試本體設定 `panic_now`。job 第一次被呼叫時，在同一次 poll 內先切斷網路再 panic，因此一定是在分區時失敗，也不可能先被 `watch` 判定失去鎖，結果與 `select!` 先輪到哪個分支無關。若某一輪因機器太忙沒在鎖逾時內收到回覆，排程下一輪會再試，測試等 job 的 panic 事件出現（上限 `EVENT_WAIT`，10 秒，失敗訊息寫明在等什麼）。後續的 `released` 與「鎖失敗次數」只計算 panic 事件之後的事件，避免把 panic 之前某一輪失敗的事件誤當成 panic 後的釋放。測試最後才用 `pg_terminate_backend` 讓伺服器結束被切斷的 session，再確認另一個副本能拿到鎖。
- `a_partitioned_holder_stops_its_job_within_the_lock_timeout`：改成等 job 的第一次 tick（job 確實在鎖內執行）才分區，再用 `lock_holder` 取得 pid。若第一次嘗試因機器太忙超過鎖逾時（查詢被取消、job 沒執行），重試，直到 job 真的執行過。
- `wait_for` 與 `wait_for_event` 的等待都有上限（`EVENT_WAIT`，原本 `wait_for` 的 10 秒），失敗訊息寫明在等什麼；`wait_for_event` 回傳事件位置，可以只看某個事件之後的事件。對應 duration 的斷言維持原樣：只和測試設定的 `PARTITION_TIMEOUT` 比，沒有放寬任何數字。

## 指令

修正前測試加上注入條件（暫時的環境變數鉤子，沒有提交）：

```
rcheck.sh 359 'bash <腳本>'    # 腳本在建置主機上對舊版測試分別跑下列條件，只選兩個分區測試
  X=1                          # 不注入任何條件
  DEMO_REPLY_DELAY_MS=300      # proxy 延後轉送 Postgres 的回覆 300 ms，跑兩次
  DEMO_FREEZE_MS=1500          # 分區後讓 runtime 停住 1500 ms（std::thread::sleep），跑兩次
```

每個條件執行：

```
CI=1 cargo test --locked -p service-kit --test job_lock -- a_partitioned_holder a_job_that_panics_in_a_partition
```

修正後的測試用同樣的條件：延後回覆是內建的（每次都開著）；停住 1500 ms 的條件用暫時的鉤子放在 job 內、分區與 panic 之間（job 是被 `select!` 的同一次 poll 執行的，停住不影響結果），同樣跑兩次。高負載：在建置主機上先 `cargo test --no-run` 取得測試執行檔，用 `nice` 啟動 N 個 `yes > /dev/null` 佔滿 CPU，再每輪同時執行 M 份完整的 `job_lock` 執行檔（每份 10 個測試、各自啟動自己的 Postgres 容器），共 6 輪。

## 結果

### 修正前：能穩定重現的條件

原始輸出：[`data/2026-10-12-359-job-lock-partition-race/before-fix.txt`](data/2026-10-12-359-job-lock-partition-race/before-fix.txt)。

| 條件 | `a_partitioned_holder_…` | `a_job_that_panics_in_a_partition_…` |
| --- | --- | --- |
| 不注入（建置主機沒什麼負載） | 通過 | 通過 |
| proxy 延後回覆 300 ms（兩次） | 失敗兩次：`job_lock.rs:590` `the job ran`（job 從未執行） | 失敗兩次：`no such event within 2s`，事件列表就是 CI 上的順序（`job lock query timed out` → `lost the lock`） |
| 分區後 runtime 停住 1500 ms（兩次） | 通過 | 失敗兩次：`no such event within 2s`，事件裡只有 `lost the lock`（第二個競態：`watch` 先判定失去鎖，job 被丟棄，沒有 `panicked`） |

延後回覆的條件下，舊測試兩個測試每次都失敗，與 CI 上看到的症狀一致，證實第一個原因；停住 runtime 的條件下，panic 測試每次都失敗，證實第二個競態。

### 修正後：同樣的條件

原始輸出：[`data/2026-10-12-359-job-lock-partition-race/after-fix-same-conditions.txt`](data/2026-10-12-359-job-lock-partition-race/after-fix-same-conditions.txt)。兩個測試在延後回覆 300 ms（內建）下各通過 2 次，在 job 內分區與 panic 之間停住 1500 ms 的條件下各通過 2 次。

### 高負載下重複執行

所有跑法都是完整的 `job_lock` 測試檔（10 個測試），`CI=1`。原始輸出在 `data/2026-10-12-359-job-lock-partition-race/` 的三個 `stress-*.txt`。

| 跑法 | 同時份數 × 輪數 | 背景 CPU 負載 | load average（20 核心） | 結果 |
| --- | --- | --- | --- | --- |
| 第一次（測試的中間版本，見下） | 6 × 6 = 36 | 24 個 `yes` | 最高 53.6 | 33 通過、3 失敗：兩個是分區測試（中間版本的缺陷），一個是 Postgres 容器 60 秒沒啟動 |
| 最終版 | 4 × 6 = 24 | 20 個 `yes` | 最高 41.1 | 24 通過、0 失敗；兩個分區測試每次都通過 |
| 最終版（更重） | 6 × 6 = 36 | 24 個 `yes` | 最高 48.6 | 34 通過、2 失敗；兩個分區測試每次都通過，2 次失敗都是 `database()` 的 `Postgres did not start in time`（60 秒內容器沒啟動，發生在 `a_panicking_job_keeps_its_schedule_and_logs_no_payload` 與 `periodic_runs_on_two_replicas_never_overlap`，與本 issue 無關） |

第一次跑法找出中間版本的缺陷：我一開始讓延後套用到所有連線，一旦第一次嘗試因負載超過鎖逾時（1 秒），重連需要好幾個回覆而超過 1 秒，之後每一輪都重連失敗，測試等不到 job。因此改成只延後呼叫當下已開啟的連線，並讓 `a_partitioned_holder_…` 在 job 沒執行時重試。這一輪的輸出保留在 `stress-first-attempt.txt`，用來說明為什麼是現在的寫法。

未做：在同樣的極端負載下重跑修正前的測試以比較失敗率（修正前的測試在沒有注入條件時大多通過，注入條件的重現已經涵蓋原因）。

## 最終檢查

rebase 到 `origin/main` 的 `9107c50` 之後，在建置主機上用一個 rcheck 批次執行（主機幾乎沒有其他負載，開始前 load average 0.65）：

```
rcheck.sh 359 'cargo fmt --all --check; cargo clippy --workspace --all-targets --locked -- -D warnings; cargo clippy --package service-kit --no-default-features --all-targets --locked -- -D warnings; CI=1 cargo test --workspace --locked --no-fail-fast; cargo test --package service-kit --no-default-features --locked'
```

- `cargo fmt --all --check`：通過。
- 兩個 clippy 指令：無警告，exit 0。
- `cargo test --workspace --locked`（`CI=1`，容器型測試沒有被略過）：每個測試程式都是 `ok`，沒有失敗；`job_lock` 10 通過，耗時 7.94 秒。
- `cargo test --package service-kit --no-default-features --locked`：全部 `ok`，`job_lock` 10 通過。
- 原始輸出（已去除主機名稱與路徑，只留結果行）：[`data/2026-10-12-359-job-lock-partition-race/final-checks.txt`](data/2026-10-12-359-job-lock-partition-race/final-checks.txt)。

## 結論

- 驗收 1（讓修正前測試穩定失敗的方式；修正後同條件通過；做法與輸出存檔）：通過。延後回覆 300 ms 與停住 runtime 1500 ms 兩種條件，舊測試各兩次都失敗，新測試各兩次都通過；輸出在 `data/` 目錄。
- 驗收 2（高負載下連續多次全部通過；次數與負載寫進報告）：兩個分區測試在 24 + 36 = 60 次最終版執行、load average 最高 48.6 下全部通過；整個檔案另有 2 次 Postgres 容器啟動逾時（`database()` 的 60 秒，與本 issue 無關），見上表。若驗收要求整個檔案每次都通過，這一項要看 Postgres 容器啟動逾時如何處理，列為後續建議，不在本 PR 範圍。
- 驗收 3（同檔其他分區測試已檢查）：通過。`taking_and_releasing_the_lock_give_up_within_the_timeout_in_a_partition` 沒有這個假設，不需修改（只加註解）；`a_partitioned_holder_stops_its_job_within_the_lock_timeout` 有同樣的假設，已修正。
- 驗收 4（`cargo clippy --workspace --all-targets --locked -- -D warnings` 與 `CI=1 cargo test --workspace --locked`）：通過，見「最終檢查」。
