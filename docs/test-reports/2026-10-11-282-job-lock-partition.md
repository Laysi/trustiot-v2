# job lock 在網路分割下的時間界限（Laysi/trustiot-v2#282）

修正 Laysi/trustiot-v2#51 獨立驗證（`2026-10-11-51-verify.md` 的 F-2）發現的問題：鎖持有者與 Postgres 之間封包被丟棄、連線沒有被重設時，heartbeat 的 `ping()` 沒有逾時；server 端結束該 session 後鎖被釋放，另一個副本取得鎖，兩個副本同時執行同一個工作。

修正後，持有者在 session 超過鎖逾時（`JobLock::with_timeout`，預設 10 秒，範圍 100 ms 到 60 秒）沒有確認還活著時判定 `JobLockError::Lost` 並取消工作；`try_acquire` 與 `release` 等資料庫也最多等鎖逾時。設計與保證寫在 `crates/service-kit/README.md` 的「排程工作（`job_lock`）」。

## 1. 環境

| 項目 | 內容 |
| --- | --- |
| 主機 | 建置主機 `ruisi-wsl`：Ubuntu 24.04.3 LTS on WSL2（Linux 6.18.33.2-microsoft-standard-WSL2，x86_64），20 核 |
| 工具 | rustc 1.99.0，Docker 29.7.2，`CI=1`（沒有容器執行環境時測試失敗而不是略過） |
| Postgres | `postgres:18.6-alpine@sha256:77f58511…`，每個測試一個用完即刪的容器 |
| 修正前 | `e8f180b`（main），只加上新的兩個分割測試；因為修正前沒有 `with_timeout`，執行時把測試裡的 `.with_timeout(PARTITION_TIMEOUT)` 拿掉，其餘不變 |
| 修正後 | 第一次在 `03006a1`（以 `e8f180b` 為基底，並放入 PR Laysi/trustiot-v2#280 的 `tests/verify_job_lock.rs`，與後來併入 main 的檔案 sha256 相同）；完整測試與 10 次重複在 `94c0b6f`（同一修正 rebase 到 main `938ea0e`，已含 PR Laysi/trustiot-v2#280）；最後把同一修正 rebase 到 main `37d81aa`（已含 Laysi/trustiot-v2#283 的時間上限），在 `310249e` 重跑一次完整檢查 |

分割的做法（`crates/service-kit/tests/job_lock.rs` 的 `Proxy`）：副本 A 經本機 TCP 代理連到 Postgres。代理改成黑洞後，雙向的位元組都讀進來丟掉，兩邊的連線都不關閉，也不送 FIN 或 RST；對端在 server 關閉後送來的 EOF 也不轉送。兩端看到的就是封包被丟棄的網路分割。

測試的鎖逾時 1 秒、heartbeat 200 ms；斷言另加 150 ms 給計時器與排程延遲。

## 2. 指令

從 worktree 根目錄，在建置主機上執行（`rcheck.sh` 會把 worktree 複製過去，設 `CI=1`）：

```sh
# 修正前（e8f180b + 新測試，去掉 .with_timeout）與修正後的分割測試
cargo test --locked -p service-kit --test job_lock partition -- --nocapture --test-threads=1

# 修正後：既有的 job lock 測試、Laysi/trustiot-v2#51 驗證的 job lock 測試（含 8 副本壓力測試）
cargo test --locked -p service-kit --lib job_lock
cargo test --locked -p service-kit --test job_lock --test verify_job_lock -- --nocapture

# 修正後：完整檢查與 10 次重複
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo test --locked -p service-kit
cargo test --locked -p service-kit --no-default-features
cargo test --locked -p repo-check
for i in $(seq 1 10); do cargo test --locked -p service-kit --test job_lock partition -- --nocapture; done
```

## 3. 結果

### 修正前

原始輸出：[before-fix-partition-tests.txt](data/2026-10-11-282-job-lock-partition/before-fix-partition-tests.txt)。

| 測試 | 結果 |
| --- | --- |
| `a_partitioned_holder_stops_its_job_within_the_lock_timeout` | **失敗**：副本 B 取得鎖 5 秒後，A 的工作仍在執行（共 236 個 20 ms 的 tick），`run_exclusive` 沒有結束，兩個副本同時執行 |
| `taking_and_releasing_the_lock_give_up_within_the_timeout_in_a_partition` | **失敗**：分割 5 秒後，`release`、`try_acquire`、經會先 ping 的 pool 的 `try_acquire` 都還在等資料庫（`Elapsed`） |

與 F-2 的探測一致（該次 10 秒內 449 個 tick）。

### 修正後

原始輸出：[after-fix-job-lock-and-verify.txt](data/2026-10-11-282-job-lock-partition/after-fix-job-lock-and-verify.txt)、[after-fix-full-suite-and-10-runs.txt](data/2026-10-11-282-job-lock-partition/after-fix-full-suite-and-10-runs.txt)、rebase 到 `37d81aa` 後的重跑 [after-rebase-full-suite.txt](data/2026-10-11-282-job-lock-partition/after-rebase-full-suite.txt)（全部通過；分割測試同時執行 964.4 ms、A 在分割後 999.4 ms 停止，都在下表範圍內）。下表是前兩份的 11 次分割測試（第一次加 10 次重複），全部通過：

| 量測（11 次） | 最小 | 最大 |
| --- | --- | --- |
| 分割後副本 B 取得鎖 | 25.2 ms | 27.3 ms |
| 分割後副本 A 停止工作（回 `Lost`） | 998.4 ms | 1000.3 ms |
| 兩個副本同時執行同一個工作的時間（A 最後一個 tick 減 B 取得鎖的時間） | 952.1 ms | 970.6 ms |
| 分割後 `release` 放棄（`Lost`） | 1000.5 ms | 1002.9 ms |
| 分割後 `try_acquire` 放棄（鎖的查詢沒有回應，`Lost`） | 1000.5 ms | 1002.6 ms |
| 分割後經會先 ping 的 pool 的 `try_acquire` 放棄（`Database`，`PoolTimedOut`） | 1000.5 ms | 1002.9 ms |

- 同時執行的時間都在 1 秒的鎖逾時內。A 最後一次確認發生在分割之前，設計上它最晚在分割後一個逾時加上計時器與排程延遲時停止；實測最大 1000.3 ms，多出的 0.3 ms 來自 tokio 計時器的毫秒解析度。
- 修正後 A 停止時，工作的計數器不再增加（測試再等 200 ms 確認）。

既有行為不變：

| 檢查 | 結果 |
| --- | --- |
| `tests/job_lock.rs` 既有 5 個測試（兩副本互斥、週期執行不重疊、session 被終止時取消工作、guard 與 panic 釋放鎖、連線回到 pool） | 通過 |
| `tests/verify_job_lock.rs`（Laysi/trustiot-v2#51 驗證）4 個測試 | 通過；8 副本 × 60 輪：50 次執行、430 次略過，同時執行數最大 1；session 被終止後 193.5 ms 偵測到 `Lost`（heartbeat 200 ms），連線重設不必等逾時 |
| `job_lock` 單元測試（key 固定、逾時上下限、heartbeat 不能是 0） | 6 個通過 |
| `cargo test -p service-kit`（預設 feature 與 `--no-default-features`）、`cargo test -p repo-check` | 全部通過 |
| `cargo fmt --check`、workspace clippy、service-kit `--no-default-features` clippy | 無警告 |

### 與 Laysi/trustiot-v2#284 的組合（工作 panic 後排程繼續）

PR Laysi/trustiot-v2#295（main `dc39e7d`）讓 `run_periodically` 攔下工作的 panic、記一行 ERROR 後繼續排程。合併後新增 `a_job_that_panics_in_a_partition_gives_up_its_lock_within_the_timeout` 測試，情境如下：

1. 分割並結束持有者的 session；
2. 讓工作 panic。

測試檢查：

- panic 的 ERROR 有記下；
- 之後的釋放在鎖逾時內放棄，並記下 `scheduled job lock failed`；
- 排程繼續，之後的回合在逾時內取鎖失敗，工作沒有在無鎖時再跑；
- 另一副本取得鎖；
- 服務停止時排程結束。

- 合併後、修正前：**失敗**，見 [panic-in-partition-before-logging-fix.txt](data/2026-10-11-282-job-lock-partition/panic-in-partition-before-logging-fix.txt)。
  - `run_periodically` 原本在 `run_exclusive` 回傳後才記工作結果；釋放逾時回 `Lost` 時，工作的結果被丟掉，panic 的 ERROR 沒有記下。
  - 這一行是 Laysi/trustiot-v2#284 告警的依據，所以會漏告警。
- 修正：`run_periodically` 改在鎖住的回合內、釋放之前就記下工作結果（完成、失敗、panic）。
- 修正後：job lock 全部 10 個測試跑 5 次都通過，見 [panic-in-partition-after-logging-fix.txt](data/2026-10-11-282-job-lock-partition/panic-in-partition-after-logging-fix.txt)。panic 後的釋放在 997.3–1005.4 ms 放棄（鎖逾時 1 秒）。

## 4. 結論

| 驗收條件 | 結果 |
| --- | --- |
| 黑洞代理情境下，持有者在設定的逾時內判定 `Lost` 並停止工作；另一個副本取得鎖後，兩者同時執行的時間不超過這個逾時 | 通過（容許計時器與排程延遲）：鎖逾時 1 秒，11 次中同時執行 952.1–970.6 ms，都在逾時內；A 在分割後 998.4–1000.3 ms 回 `Lost`，最大值超出 1 秒 0.3 ms，來自 tokio 計時器的毫秒解析度，測試對這兩項的斷言是「逾時加 150 ms」。修正前 A 在 B 取得鎖 5 秒後仍在執行 |
| 乾淨關閉與連線重設的既有行為不變，既有的 job lock 測試與 8 副本壓力測試都通過 | 通過 |
| README 寫明鎖的時間界限、分割時的行為與冪等前提 | 通過：`crates/service-kit/README.md`「鎖的保證有時間界限」與規則 1、2 |
| 測試報告含修正前後的數字 | 本報告 |

沒有測的：真實網路設備造成的分割與真實 failover（Kubernetes 上的 Postgres 尚未部署）。代理丟棄位元組、不關連線，對兩端的效果與封包被丟棄相同，所以本機測試涵蓋了這個情境。
