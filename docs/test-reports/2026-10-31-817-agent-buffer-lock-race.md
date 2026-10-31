# agent：緩衝目錄的鎖被其他測試的子行程留住（2026-10-31）

Laysi/trustiot-v2#817：`edge/agent` 的單元測試關掉磁碟緩衝後立刻重開同一個目錄，負載高時偶爾 `WouldBlock`。這份報告記錄原因的重現、修正前後的對照，以及驗收條件要求的重複執行。修正見 commit `e03b1035`，規則寫在 `edge/agent/src/buffer/disk.rs` 的 `test_locks` 模組與 [edge/agent/README.md](../../edge/agent/README.md)「磁碟格式」。

## 環境

- **程式碼**：分支 `fix/817-agent-buffer-lock-race`，基於 main `608766cc`。
  - 修正前：main `608766cc` 加上暫時的實驗模組（沒有 commit，原始碼見下）。
  - 修正後：`e03b1035`。實驗模組另外暫時加回去跑過一次。
- **機器**：
  - 建置主機（arm64 Linux VM），有 systemd 使用者 manager，所以 `modbus::address::tests` 會真的啟動 `systemd-run`。
  - 建置主機（x86_64 Linux）。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0。
- **設定**：`CI=1`，需要容器的測試不會略過。
- **負載**：重複執行時，同一個工作裡另外跑：
  - 一個迴圈：反覆在獨立的 target 目錄從頭 `cargo build --release -p identity`（`-j` 等於 CPU 數）。
  - 每個 CPU 一個忙迴圈。

## 原因

1. 子行程建立時（`clone`，glibc 的 `posix_spawn` 也是）拿到父行程每個 fd 的複本，到它 `exec` 自己的程式時才關掉 `O_CLOEXEC` 的 fd。
2. `flock` 屬於 open file description，不屬於某一個 fd。所以只要還有一個複本，鎖就還在。
3. 測試 A 持有緩衝的期間，同一個測試行程裡的測試 B 啟動子行程。測試 A 關掉緩衝時，子行程如果還沒 `exec`，鎖還在。這時測試 A 立刻重開同一個目錄，就拿到 `WouldBlock`。
4. `agent` 的 lib 測試執行檔裡，會啟動子行程的只有 `modbus::address::tests`：`user_manager()` 與 `read_test_passes_with()` 都呼叫 `systemd-run`。
   - 在有 systemd 的機器上，`user_manager()` 一定會 spawn；要有使用者 manager，`read_test_passes_with()` 才會跑。
   - 平常子行程在幾微秒內就 `exec`。負載高時排程延遲把這段時間拉長，才會撞上。
5. 正式執行不受影響：agent 不啟動子行程；重啟時是另一個行程，舊行程結束時核心就放開鎖。

## 指令

在 repo 根目錄、建置主機上執行。

重現實驗：把 [experiment_817.rs.txt](data/2026-10-31-817-agent-buffer-lock-race/experiment_817.rs.txt) 暫時存成 `edge/agent/src/buffer/experiment_817.rs`，在 `edge/agent/src/buffer.rs` 的 `mod disk;` 後面加 `#[cfg(test)] mod experiment_817;`，跑完刪掉（沒有 commit）。修正前那次的實驗還沒有量 `longest reopen`，其餘相同。

- 實驗 A：測試用的 `pre_exec` 讓子行程停在 fork 之後、`exec` 之前，等測試放行。測試先關掉緩衝，再重開同一個目錄。
- 實驗 B：真實的路徑。一條執行緒不停 `Command::new("true").status()`（glibc `posix_spawn`）。另一條執行緒做 20,000 次「開緩衝、關掉、立刻重開」，數重開被 `WouldBlock` 拒絕幾次。

```sh
cargo test --locked -p agent --lib experiment_817 -- --nocapture --test-threads=2
EXP817_ROUNDS=20000 cargo test --locked -p agent --lib experiment_817_posix -- --nocapture
```

重複執行（[stress.sh](data/2026-10-31-817-agent-buffer-lock-race/stress.sh)，每次的輸出寫到 `.exp817/`）。存進 repo 的版本與當時執行的只差兩處：負載建置的 target 目錄改成腳本自己以 `mktemp` 建在 `.exp817/` 底下的目錄（當時是固定的 `target-load`），第一行輸出不再印 CPU 數。

```sh
mkdir -p .exp817
cp docs/test-reports/data/2026-10-31-817-agent-buffer-lock-race/stress.sh .exp817/
bash .exp817/stress.sh 20                      # 修正後
EXP=1 TEST_ARGS="--skip experiment_817" bash .exp817/stress.sh 20   # 修正前，含實驗模組
```

其他檢查：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p agent
cargo test --locked --workspace --no-fail-fast
```

## 結果

| 項目 | 在哪裡、哪個版本 | 結果 |
| --- | --- | --- |
| 實驗 A（子行程停在 `exec` 之前） | arm64 VM，修正前 | 重開被拒：`Err(Custom { kind: WouldBlock, error: ".../buffer is in use by another agent (it holds the directory's lock)" })`。子行程 `exec` 之後重開成功：`Ok(())`。沒負載與有負載各一次，結果相同。 |
| 實驗 B（真實的 `posix_spawn`） | arm64 VM，修正前 | 沒負載：20,000 次中 **802** 次重開被 `WouldBlock` 拒絕，同時 spawn 了 18,225 個子行程。有負載：20,000 次中 **76** 次，同時 spawn 了 440 個。 |
| 實驗 B | arm64 VM，修正後 | 20,000 次中 **0** 次被拒，同時 spawn 了 108,846 個子行程；最長的一次重開等了 6.7 ms。 |
| 負載下 `cargo test --locked -p agent --lib` 20 次 | arm64 VM，修正前 | **20 次中 1 次失敗**（第 17 次，load 17.88）：`buffer::tests::start_up_keeps_at_most_the_batch_limit_of_the_newest_batches`，就是 issue 裡的那個測試。其餘 19 次各 359 個測試通過。 |
| 負載下 `cargo test --locked -p agent --lib` 20 次 | arm64 VM，`e03b1035` | **20 次都通過**，每次 360 個測試，load 10.4–15.3 |
| 負載下 `cargo test --locked -p agent --lib` 20 次 | x86_64，`e03b1035` | **20 次都通過**，每次 360 個測試，load 11.5–21.6 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | arm64 VM，`e03b1035` | 通過 |
| `cargo test --locked -p agent` | arm64 VM，`e03b1035` | 通過：lib 360、`main.rs` 3、`tests/binary.rs` 15（含新的 `the_lock_check_sees_a_directorys_lock`）、其餘每個測試執行檔都通過 |
| `cargo test --locked --workspace --no-fail-fast` | x86_64，`e03b1035` | 通過：284 個測試執行檔、2,605 個測試、0 失敗 |

原始輸出：

- [before-fix-arm64.txt](data/2026-10-31-817-agent-buffer-lock-race/before-fix-arm64.txt)：修正前的兩個實驗與 20 次重複執行。
- [after-fix-experiment-arm64.txt](data/2026-10-31-817-agent-buffer-lock-race/after-fix-experiment-arm64.txt)：修正後的實驗 B。
- [after-fix-arm64.txt](data/2026-10-31-817-agent-buffer-lock-race/after-fix-arm64.txt)：修正後的 clippy、`cargo test -p agent` 與 20 次重複執行。
- [after-fix-x86_64.txt](data/2026-10-31-817-agent-buffer-lock-race/after-fix-x86_64.txt)：修正後的 workspace 測試與 20 次重複執行。

## 修正

- **lib 測試**（`cfg(test)`，只在測試建置）：
  - 每個緩衝取得目錄的鎖時，記下目錄的 device 與 inode；關掉時在 handle 關閉之前移除。
  - 開目錄時鎖被占用：
    - 鎖在這個行程的緩衝手上：照正式程式立刻回 `WouldBlock`。`a_second_store_on_the_same_directory_is_refused_while_the_first_is_open` 照樣通過。
    - 不是這個行程的緩衝持有（子行程的複本，或別的行程）：每 5 ms 再試一次，最多等 60 秒。逾時的訊息寫明在等哪個目錄的鎖、為什麼。
  - 所有開目錄的路徑（`DiskStore::open`、`open_for_appends`、`Buffer::open_disk`、停止時的開啟、上傳管線）都經過 `prepare_dir`，所以 lib 裡所有「關掉後重開」的測試都涵蓋在內，不必逐一改測試。這類測試約 30 個，例如 `disk.rs` 的 `records_survive_reopening_in_order_with_their_metadata`、`buffer.rs` 的 `reopening_orders_streams_by_age_and_each_stream_by_sequence_number`。
  - 新測試 `in_tests_a_lock_that_a_child_process_keeps_is_waited_for`：讓 `cat` 以 stdout 握住緩衝 handle 的複本，然後關掉緩衝：
    1. 等待上限 0 時，回 `WouldBlock`，訊息寫明「no store of this process holds」。
    2. `cat` 結束後重開成功。
- **`tests/binary.rs`**（整合測試，用正式建置的 lib）：
  - `a_batch_in_the_state_directory_goes_up_and_the_clouds_acknowledgement_releases_it` 先在測試行程裡用管線寫一批到緩衝目錄，再啟動 agent 執行檔開同一個目錄。同一個執行檔的其他測試會啟動 agent 子行程，所以這裡也是同樣的情形。被擋住時，agent 要等 `DISK_RETRY`（60 秒）才再試，測試就會逾時。
  - 現在啟動 agent 之前，先等到 `/proc/locks` 裡這個行程在那個目錄取得的鎖都放開。用 `/proc/locks` 檢查，自己不取鎖：取鎖來檢查的話，那把鎖一樣會被複製給子行程。新測試 `the_lock_check_sees_a_directorys_lock` 確認這個檢查看得到鎖，也看得到鎖放開。
- 其他整合測試執行檔：
  - `tests/upload.rs` 會重開同一個目錄，但沒有啟動子行程。
  - `tests/broker.rs` 會啟動子行程（podman），但每個目錄只開一次。
  - 所以兩者都不受影響。
- 正式程式的行為不變：鎖被占用時立刻回 `WouldBlock`，60 秒後（`DISK_RETRY`）再試。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| PR 說明失敗的原因（重現或論證） | 通過。實驗 A 決定性地重現機制。實驗 B 以真實的 `posix_spawn` 重現，修正前 20,000 次中 802 次被拒，修正後 0 次。修正前在負載下重複執行，issue 裡的測試 20 次中失敗 1 次。 |
| 修正後在負載下重複執行 `cargo test --locked -p agent --lib` 20 次都通過，arm64 VM 上也要跑 | 通過。arm64 VM 與 x86_64 各 20 次都通過；修正前同樣條件下 arm64 VM 20 次中失敗 1 次。 |
| `cargo test -p agent` 與 `cargo clippy --workspace --all-targets -- -D warnings` 通過 | 通過。clippy 與 `cargo test -p agent` 在 arm64 VM 通過，整個 workspace 的測試（含 agent）在 x86_64 通過。 |

### 待測

以下是延後的進階驗證，不在這個 issue 的範圍：

- 更多次的重複執行（例如數百次），量出修正前後失敗率的差異。修正前 20 次只失敗 1 次，20 次全過只是基本證據；實驗 B 的 802 比 0 是較強的證據。
- `tests/binary.rs` 的等待在真的被留住的鎖上觸發的實測：要在那個測試執行檔裡讓子行程停在 `exec` 之前，需要測試注入點，這次沒有加。

兩項都在 Laysi/trustiot-v2#820 補上，見 [2026-10-31-820-agent-lock-wait-stress.md](2026-10-31-820-agent-lock-wait-stress.md)。
