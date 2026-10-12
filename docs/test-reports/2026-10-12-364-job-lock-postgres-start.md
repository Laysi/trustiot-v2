# service-kit 的 Postgres 容器啟動（Laysi/trustiot-v2#364）

驗證 `crates/service-kit` 的 `job_lock`、`verify_job_lock` 測試不再因 `Postgres did not start in time` 偶發失敗：先量測這個等待在等什麼、要多久，找出失敗的真正原因，再記錄修正前後在同樣高負載下的結果。只改測試與測試輔助程式，`crates/service-kit/src` 沒有動。

## 環境

| 項目 | 內容 |
| --- | --- |
| 機器 | 建置主機（WSL2 上的 Docker Desktop 29.8.2，20 核心，約 15.5 GiB），經 repo 外的檢查佇列執行；容器是 `postgres:18.6-alpine`（測試固定的 digest） |
| 日期 | 量測與重現：2026-10-12 與 2026-10-13；最終負載測試：2026-10-13（01:45 至 02:05，Asia/Taipei） |
| 測試對象 | 分支 `test/364-job-lock-postgres-start`，基底 `origin/main` 的 `b2b9232`（含 Laysi/trustiot-v2#363）；修正前的對照是同一份 `b2b9232` 的 `job_lock.rs` 與 `verify_job_lock.rs`，以另一個名稱當作額外的測試程式編進同一次建置 |
| 設定 | `CI=1`，`--locked`，debug profile |
| 負載 | 以 `yes` 佔滿 CPU（`nice -n 10`，數量見各表）加上同時執行多份測試檔；以 `/proc/loadavg` 的 1 分鐘 load average 表示（20 核心） |

## 這個等待在等什麼

舊的 `database()` 啟動一個 `postgres` 容器，每 250 ms 以 `PgPool::connect` 加 `SELECT 1` 嘗試，最多 60 秒（TLS 版本 90 秒）。映像的啟動順序是：建立 cluster（`initdb`）、起一個不開網路的暫時伺服器跑初始化、停掉、再起真正的伺服器。容器從第一刻起就是 running，且 Docker 發佈的連接埠要等到伺服器真的在聽才會有人回應，所以唯一的證據是登入成功。

### 啟動時間的分布（`docker run` 到第一次登入成功）

量測腳本：[`measure-start.ts.txt`](data/2026-10-12-364-job-lock-postgres-start/measure-start.ts.txt)，每個容器記錄 `docker run` 回傳時間、`initdb` 開始與完成、伺服器最後一次 `ready to accept connections` 的日誌時間（容器自己的時鐘），以及主機端第一次登入成功的時間。一次同時啟動 N 個容器，與測試一樣。原始輸出：[`start-measurements-first-run.txt`](data/2026-10-12-364-job-lock-postgres-start/start-measurements-first-run.txt)（逐容器一行）、[`final-run-start-and-stress.txt`](data/2026-10-12-364-job-lock-postgres-start/final-run-start-and-stress.txt)。

| 同時啟動 | 背景負載 | 組數 | 第一次登入成功（最小至最大，秒） | 中位數（各組，秒） |
| --- | --- | --- | --- | --- |
| 1 | 無 | 3 | 1.5 至 1.7 | 1.5 至 1.7 |
| 10 | 無 | 3 | 1.9 至 3.0 | 2.8 至 2.9 |
| 10 | 24 個 `yes`（load 19 至 22） | 3 | 3.0 至 4.7 | 3.3 至 4.4 |
| 60 | 無 | 3 + 4 | 4.8 至 16.5 | 9.9 至 15.0 |
| 60 | 24 個 `yes`（load 23 至 42） | 3 | 5.2 至 20.5 | 12.8 至 19.1 |

- 啟動時間幾乎都花在 `initdb` 與初始化（伺服器的 `ready` 日誌在容器啟動後：單獨啟動 1.0 至 1.3 秒，60 個一起時 4 至 16 秒），主機端第一次登入緊跟在 `ready` 之後：最後一次 `ready` 日誌到第一次登入成功只隔 0.03 至 1.1 秒（四組各 60 個，原始輸出的 `readyToFirstLoginMs`）。
- 同時啟動的 60 個是 6 份 `job_lock` 同時執行的量（每份 10 個測試各一個容器）。這樣的啟動最慢 20.5 秒，離 60 秒的上限很遠。**慢不是失敗的原因。**

### 失敗的真正原因：發佈的連接埠沒有在主機端建立

第一次量測時，60 個一起啟動的容器裡有 1 個 300 秒都沒有登入成功（`attempts` 1171 次，`PostgresError: Failed to connect`），但它的伺服器日誌在 13.2 秒就顯示 ready。為了查原因，腳本在放棄時（60 秒）收集診斷，再連續重現 20 組、每組 60 個容器（原始輸出：[`dead-port-repro-20x60.txt`](data/2026-10-12-364-job-lock-postgres-start/dead-port-repro-20x60.txt)）：

- 1,200 個容器裡有 5 個放棄（第 9 組 2 個、第 14 組 3 個），其他組全部在 20.1 秒內成功。
- 5 個的診斷完全相同：容器 `running`、沒有 OOM；`docker port` 與 `docker inspect` 都有 `127.0.0.1:<埠>` 的對應；在容器**裡面**執行 `pg_isready -h 127.0.0.1` 回報 `accepting connections`；但主機上 `ss -ltn` **沒有**那個埠的監聽，原始 TCP 連線直接被拒（`connect error`），再試一次登入仍然失敗。
- 總計：60 個一組的啟動共 1,800 個容器（第一次量測 360、重現 1,200、最終 240），6 個如此，約 300 個裡 1 個。

也就是 Docker Desktop 偶爾沒有建立發佈連接埠的主機端，伺服器本身沒問題。單純放寬等待時間不會有用：這個容器等多久都不會有人聽。#359 報告的 36 次執行裡的 2 次失敗（每次執行 10 個容器，共 360 個）與這個機率相符。舊測試沒有記錄容器狀態，所以那兩次沒有被直接診斷，這是依機率與症狀（60 秒、容器本身正常）的推論；此報告修正前的對照執行（下節）也只留下 `did not start in time`。

## 變更

- `tests/support/postgres.rs`：
  - `test_database()`：一個測試程式（binary）只啟動一個 Postgres 容器，同一個程式裡同時執行的測試共用；每個測試取得自己的資料庫（`CREATE DATABASE` 語句取自 `TEST_DATABASES` 表的字面值，因為 `tools/sql-check` 不接受字串組成的 SQL，資料庫名稱又不能當參數；一個程式裡用到資料庫的測試超過 32 個時會明確 panic，請加表項）。advisory lock 屬於資料庫，所以測試之間互不可見，隔離與以前一樣；`TEST_DATABASE_URL` 的行為不變（直接用該 URL）。最後一個測試放掉時容器被移除；`--test-threads=1` 時各測試不重疊，會各自重啟一次容器，與以前的成本相同。
  - `start_postgres()`：`job_lock`、`verify_job_lock` 與 TLS 版本共用的啟動與等待。`POSTGRES_START_TIMEOUT`（60 秒，原本 60 與 90 兩種）、`HOST_REACH_GRACE`（10 秒）、`POSTGRES_START_ATTEMPTS`（3 個容器）三個具名常數的註解寫明在等什麼與依據的量測值。
  - 容器內的伺服器已經接受連線（容器內 `pg_isready`）、主機端卻連續 10 秒登入不了，視為發佈的連接埠已壞：移除該容器，再啟動一個，最多 3 個。伺服器還沒好的慢啟動不會觸發，只受 60 秒上限約束。
  - 失敗訊息寫明：在等什麼、等了多久、嘗試次數、最後一個錯誤，以及容器狀態（status、exit code、OOM、啟動時間）與日誌最後 20 行；容器的環境變數（含 TLS 私鑰與密碼）不會被印出。多個容器都失敗時，每個容器各一段。
  - 啟動失敗會被記住：同一個程式裡其他測試立刻以同樣的訊息失敗，不會各自再等一輪。
- 共用容器以 `max_connections=400` 啟動（預設 100）：十個測試各開數個最多 4 條連線的連線池。這是留餘裕，沒有量測過尖峰連線數。
- `tests/job_lock.rs`、`tests/verify_job_lock.rs`：刪掉各自的 `database()`、`Database` 與 `POSTGRES_IMAGE`，改用共用輔助程式；`pg_locks` 的三個查詢（`lock_holder`、`holders`、`any_advisory_locks`）加上只看自己資料庫的條件。
- `tests/postgres_start.rs`（新）：3 個測試。伺服器永遠不起來時的錯誤訊息內容與容器被移除；第一個容器的連接埠「壞掉」（主機端登入被改成永遠失敗，伺服器本身正常）時被換成第二個容器、舊容器被移除；全部容器都壞時每個容器各一段訊息。
- `tests/support/mod.rs`：`Container::diagnostics()`（狀態與日誌尾端，不會 panic）與 `Container::accepting_connections()`。

### 每個測試是否需要自己的容器

不需要。隔離靠資料庫（advisory lock 與 `pg_terminate_backend` 的對象都限於自己的資料庫與 pid），不靠伺服器。共用後，一份 `job_lock` 從 10 個容器變成 1 個，同時啟動的負載少一個數量級；啟動成本只付一次。

## 指令

量測與負載都在建置主機上以 `rcheck.sh` 執行。腳本：[`measure-start.ts.txt`](data/2026-10-12-364-job-lock-postgres-start/measure-start.ts.txt)（`bun measure-start.ts <容器數> <標籤>`）與 [`stress.ts.txt`](data/2026-10-12-364-job-lock-postgres-start/stress.ts.txt)（`bun stress.ts <標籤> <yes 數> <份數> <輪數> <測試執行檔前綴>...`：每輪同時啟動「份數」份每個測試執行檔，每份是完整的測試檔，`CI=1`）。

最終的一次執行（單一 rcheck，輸出在 `final-run-start-and-stress.txt`）：

```
cargo fmt --all --check
cargo clippy --locked -p service-kit --all-targets -- -D warnings
cargo test --locked -p service-kit --test job_lock --test verify_job_lock --test postgres_start
bun measure-start.ts 60 idle-60-<n>                      # 4 次
bun stress.ts before 24 6 10 before_job_lock before_verify_job_lock   # 修正前，24 個 yes，6 份 × 2 個檔 × 10 輪
bun stress.ts after 24 6 10 job_lock verify_job_lock                  # 修正後，同樣的負載
bun stress.ts after-heavy 24 12 10 job_lock verify_job_lock           # 修正後，份數加倍
```

最終程式的重跑只有後兩個 `stress.ts` 指令（先 `cargo test --no-run --test job_lock --test verify_job_lock`，再同樣的 `bun stress.ts`）。

`before_*` 是 `b2b9232` 的 `job_lock.rs` 與 `verify_job_lock.rs` 原封不動，暫時放在 `tests/` 以額外的測試程式編譯，沒有提交（`git show b2b9232:crates/service-kit/tests/job_lock.rs`）。原始輸出裡失敗訊息的行號（`before_job_lock.rs:76`）就是舊 `database()` 裡的 `assert!`。

## 結果

### 修正前後，同樣的高負載

| 跑法 | 每輪同時執行 | 輪數 | 執行次數 | 通過 | 失敗 | load average（輪末，最高） |
| --- | --- | --- | --- | --- | --- | --- |
| 修正前 | 6 份 `job_lock` + 6 份 `verify_job_lock`（84 個容器） | 10 | 120 | 114 | 6 | 62.8 |
| 修正後（共用輔助程式的第一版） | 同上（12 個容器） | 10 | 120 | 120 | 0 | 46.6 |
| 修正後（第一版），份數加倍 | 12 份 + 12 份（24 個容器） | 10 | 240 | 240 | 0 | 53.6 |
| 修正後（最終程式，rebase 與 sql-check 修改之後重跑） | 6 份 + 6 份（12 個容器） | 10 | 120 | 120 | 0 | 35.6 |
| 修正後（最終程式），份數加倍 | 12 份 + 12 份（24 個容器） | 10 | 240 | 240 | 0 | 47.1 |

全部的背景負載都是 24 個 `yes`（20 核心）。修正前與第一版修正後在同一次執行中連續跑，所以第一版的負載從修正前留下的 44 開始；最終程式的重跑是從 11 開始升到 36（份數加倍時 31 至 47），負載比較低，但同樣是 24 個 `yes`。原始輸出：[`final-run-start-and-stress.txt`](data/2026-10-12-364-job-lock-postgres-start/final-run-start-and-stress.txt)（修正前與第一版）、[`stress-after-final-code.txt`](data/2026-10-12-364-job-lock-postgres-start/stress-after-final-code.txt)（最終程式）。

- 修正前的 6 次失敗全部是 `Postgres did not start in time`（`job_lock` 5 次、`verify_job_lock` 1 次；其中一次有 3 個測試同時失敗），執行時間 65 至 79 秒，符合 60 秒等待加上其餘測試。整份測試檔的中位數 37 秒（`job_lock`）與 34 秒（`verify_job_lock`）。
- 修正後 720 次執行（四組）全部通過；`job_lock` 的中位數 11.5 至 14.3 秒（份數加倍時 16.5 至 19.3 秒），`verify_job_lock` 7.7 至 10.0 秒（份數加倍時 13.2 至 15.0 秒）。修正後的 load 較低，部分原因是測試本身的負載少了。
- 修正後的負載測試沒有碰到壞掉的連接埠（那個機率約 300 個容器 1 個，修正後每組只有 12 或 24 個容器），所以「換容器」的路徑由 `postgres_start` 的測試驗證，不是由負載測試。

### 單元測試與本機檢查

`cargo test --locked -p service-kit --test job_lock --test verify_job_lock --test postgres_start`（`CI=1`，建置主機）：`job_lock` 10 通過、`postgres_start` 3 通過（7.9 秒）、`verify_job_lock` 4 通過；`cargo clippy -p service-kit --all-targets -D warnings` 無警告。

## 最終檢查

在合併 `origin/main` 的 `28862e1` 之後，以一個 rcheck 批次執行（第一次跑出兩個問題，已修正：`services/identity` 的測試以 `#[path]` 共用 `tests/support/postgres.rs` 與自己的 `Container`，所以診斷函式改放在 `postgres.rs`；`tools/sql-check` 不接受 `format!` 組成的 `CREATE DATABASE`，改用字面值的表）： `cargo fmt --all --check`、兩個 `cargo clippy`（預設與 `--no-default-features`）、`cargo test --workspace --locked` 與 `cargo test --package service-kit --no-default-features --locked`；輸出：[`final-checks.txt`](data/2026-10-12-364-job-lock-postgres-start/final-checks.txt)。

## 結論

- 驗收 1（量測啟動時間的分布；修正後同樣高負載多次執行兩個測試檔全部通過，次數與負載寫進報告）：通過。啟動時間分布見上表；修正前 120 次執行失敗 6 次，修正後 720 次（兩種份數、兩個版本）全部通過，負載與次數見結果表。
- 驗收 2（啟動上限的依據寫在常數註解或報告；失敗訊息寫明在等什麼）：通過。三個常數的註解引用本報告的量測值；訊息內容由 `postgres_start` 的測試斷言。
- 驗收 3（`cargo clippy --workspace --all-targets --locked -- -D warnings` 與 `CI=1 cargo test -p service-kit --locked`）：通過，見「最終檢查」。
- 限制：只在 Docker Desktop 上量測與驗證。podman 的路徑（`container_runtime()` 的另一個選擇）沒有跑過；`exec`、`inspect --format`、`logs --tail`、`ps --filter` 在 podman 上同樣存在，但沒有實測。
- 限制：壞掉的連接埠只在 Docker Desktop 上看到，原因是推論（Docker 沒有建立主機端監聽），沒有查 Docker 的日誌。若 CI 的 runner（Linux 上的 Docker）從不發生，換容器的路徑只是多一道保險。

## 後續建議（不在本 issue 範圍）

- `crates/outbox/tests/support` 與 `crates/testkit` 各有自己的容器啟動與等待（60 秒或固定的上限，沒有診斷訊息，也沒有壞連接埠的處理），可能有同樣的偶發失敗；可以改用同一份啟動邏輯，或把它搬到一個共用的測試 crate。
