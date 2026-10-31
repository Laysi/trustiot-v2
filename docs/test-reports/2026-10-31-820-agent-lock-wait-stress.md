# agent：緩衝目錄鎖的等待，大量重複執行與 binary 測試的注入驗證（2026-10-31）

Laysi/trustiot-v2#820：Laysi/trustiot-v2#817（PR Laysi/trustiot-v2#819）修正了 `edge/agent` 測試在負載下偶爾出現的 `WouldBlock`，當時延後了兩項進階驗證，本報告補上：

1. 在負載下大量重複執行 `cargo test --locked -p agent --lib`，修正前後各至少 200 次，量出失敗率的差異。
2. 讓 `tests/binary.rs` 的 `/proc/locks` 等待（`locked`、`unlocked`）在一把真的被子行程留住的鎖上觸發，確認 agent 執行檔在鎖放開之後才啟動。

原因與修正見 [Laysi/trustiot-v2#817 的報告](2026-10-31-817-agent-buffer-lock-race.md)。

## 環境

- **程式碼**：
  - 修正前：main `608766cc`（PR Laysi/trustiot-v2#819 的前一個 commit），另外放進 Laysi/trustiot-v2#817 報告的 `stress.sh` 與本報告的 `stress-job.sh`（沒有 commit）。輸出的 `fix present: 0` 表示 `edge/agent/src/buffer/disk.rs` 沒有 `test_locks` 模組。
  - 修正後：分支 `test/820-agent-lock-wait-stress`，基於 main `1da444a0`。重複執行只跑 lib 測試，這個分支沒有改 lib 的任何檔案（只改 `tests/binary.rs` 與本報告），所以 lib 測試與 main `1da444a0` 相同；輸出的 `fix present: 1`。
  - 注入測試：分支 `test/820-agent-lock-wait-stress` 的 commit `cf49fff1`。
- **機器**：
  - 重複執行：建置主機（x86_64 Linux），兩組都在這台，一次只跑一個工作。
  - 注入測試：兩台都跑；突變只在建置主機（arm64 Linux VM）。
  - 最後的 fmt、clippy、`cargo test -p agent` 與整個 workspace 的測試：建置主機（x86_64 Linux）。
  - 修正前另有一組只跑了 103 次的資料（arm64 Linux VM，見「結果」）。
  - 兩台都有 systemd 使用者 manager（輸出的 `systemd user manager: yes`），所以 `modbus::address::tests` 會真的啟動 `systemd-run` 子行程，也就是 Laysi/trustiot-v2#817 的競爭來源。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0。
- **設定**：`CI=1`，需要容器的測試不會略過。
- **負載**：與 Laysi/trustiot-v2#817 相同的 [stress.sh](data/2026-10-31-817-agent-buffer-lock-race/stress.sh)。重複執行的同時，在同一個工作裡另外跑：
  - 一個迴圈：反覆在獨立的 target 目錄從頭 `cargo build --release -p identity`（`-j` 等於 CPU 數）。
  - 每個 CPU 一個忙迴圈。

## 指令

在 repo 根目錄、建置主機上執行。[stress-job.sh](data/2026-10-31-820-agent-lock-wait-stress/stress-job.sh) 先印出修正在不在、有沒有 systemd 使用者 manager，再執行 `stress.sh`，最後列出每次失敗的 panic 訊息（`stress.sh` 本身只列 panic 的位置，`assert_eq!` 的左右值在下一行）。

修正後（這個分支）：

```sh
bash docs/test-reports/data/2026-10-31-820-agent-lock-wait-stress/stress-job.sh \
  docs/test-reports/data/2026-10-31-817-agent-buffer-lock-race/stress.sh 200
```

修正前（main `608766cc` 的另一個 worktree，兩個腳本複製到它的根目錄）：

```sh
git worktree add --detach ../before-820 608766cc
cp docs/test-reports/data/2026-10-31-817-agent-buffer-lock-race/stress.sh ../before-820/stress-817.sh
cp docs/test-reports/data/2026-10-31-820-agent-lock-wait-stress/stress-job.sh ../before-820/stress-job-820.sh
cd ../before-820
bash stress-job-820.sh stress-817.sh 200
```

注入測試、clippy 與兩個突變（突變只在建置主機的副本裡以 `sed` 改，沒有 commit）：

```sh
cargo clippy --locked -p agent --all-targets -- -D warnings
for i in 1 2 3; do
  cargo test --locked -p agent --test binary -- --nocapture the_agent_starts_only_once
done
# A: the wait checks once and does not loop
sed -i "s/            while locked(dir) {/            if locked(dir) {/" edge/agent/tests/binary.rs
cargo test --locked -p agent --test binary -- --nocapture the_agent_starts_only_once
# B: no wait at all
sed -i "s/            if locked(dir) {/            while false \&\& locked(dir) {/" edge/agent/tests/binary.rs
cargo test --locked -p agent --test binary -- --nocapture the_agent_starts_only_once
```

其他檢查（在 x86_64 跑在最後的樹上）：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p agent
cargo test --locked --workspace --exclude agent --no-fail-fast
```

## 結果

### 負載下重複執行 `cargo test --locked -p agent --lib`

| 組別 | 機器 | 次數 | 失敗的次數 | load（1 分鐘） |
| --- | --- | --- | --- | --- |
| 修正前（`608766cc`） | x86_64 | 200 | **33**（16.5%） | 13.6–34.1 |
| 修正後（這個分支） | x86_64 | 200 | **0** | 13.0–35.1 |
| 修正前（`608766cc`），只跑到第 103 次 | arm64 VM | 103 | **7** | 12.1–23.1 |

- 修正前每次 361 個測試（359 個執行、2 個 ignored），修正後 362 個（360 個執行，多了 Laysi/trustiot-v2#817 的 `in_tests_a_lock_that_a_child_process_keeps_is_waited_for`）。
- 修正前 x86_64 的 33 次失敗中，30 次有 1 個測試失敗、2 次有 2 個、1 次有 3 個，一共 37 個測試失敗，全都是重開剛關掉的緩衝目錄：
  - **8 個直接是 `WouldBlock`**：`called Result::unwrap() on an Err value: Custom { kind: WouldBlock, error: "/tmp/testkit-agent-disk-…/buffer is in use by another agent (it holds the directory's lock)" }`。`buffer::disk::tests::recovery_keeps_at_most_its_limit_of_the_newest_records` 7 個、`recovery_keeps_only_the_stream_ids_of_the_records_it_keeps` 1 個。
  - **29 個是 `open_disk` 回 `None`**（`left: None`，預期 `Some(...)`）。`Buffer::open_disk` 在目錄開不了（`DiskStore::open_until` 回錯誤，記成 `DiskOpenFailed`）時回 `None`。這些測試重開前剛在同一個新建的暫存目錄寫過，目錄開不了的原因就是鎖還在，與 Laysi/trustiot-v2#817 報告的修正前那次失敗相同。`buffer::tests::start_up_keeps_at_most_the_batch_limit_of_the_newest_batches` 27 個、`a_torn_record_still_raises_the_floor_to_its_formation_time` 1 個、`reopening_orders_streams_by_age_and_each_stream_by_sequence_number` 1 個。
  - 沒有其他種類的失敗。
- 修正後 200 次全部通過，沒有任何一次 `WouldBlock`，也沒有其他失敗。
- arm64 VM 那組：建置主機的 arm64 Linux VM 同時跑兩個工作，負載會拖慢同一台上其他工作的測試，所以跑到第 103 次時手動停止，重複執行改到只跑一個工作的 x86_64。這 103 次中 7 次失敗：3 個 `WouldBlock`（`recovery_keeps_at_most_its_limit_of_the_newest_records`），5 個 `open_disk` 回 `None`（`start_up_keeps_at_most_the_batch_limit_of_the_newest_batches`），第 19 次兩個都有。這組不算進驗收的 200 次，只是補充：arm64 上修正前同樣會失敗。
- 修正前的失敗率比 Laysi/trustiot-v2#817 時（20 次中 1 次）高，因為這次負載更重（load 到 34，Laysi/trustiot-v2#817 時最高 17.9）。

### `tests/binary.rs` 的等待在真的被留住的鎖上觸發

新測試 `the_agent_starts_only_once_a_childs_copy_of_the_buffer_lock_is_gone`（`tests/binary.rs`）：

1. 測試行程用 lib 的上傳管線寫一批到緩衝目錄（`leave_a_batch`），和原本的 `a_batch_in_the_state_directory_goes_up_and_the_clouds_acknowledgement_releases_it` 相同。管線啟動時開目錄、取得鎖；等 `/proc/locks` 看到這把鎖之後，建立一個 `HeldChild`。
2. `HeldChild` 是注入點，只在這個測試執行檔裡：以 `std::process::Command` 啟動 `true`，`pre_exec` 掛勾在子行程 fork 之後、`exec` 之前執行：
   1. 關掉自己那份「放行」管道的寫入端。
   2. 寫一個 byte 到「已建立」管道，通知測試。
   3. 在「放行」管道上 `read`，等測試放行。

   掛勾只呼叫 `close(2)`、`write(2)`、`read(2)` 與讀 errno，都是 async-signal-safe，不配置記憶體、不取鎖，並有 SAFETY 註解。這個子行程握有測試行程在 fork 那一刻所有 fd 的複本，包括管線的緩衝目錄 handle，和 Laysi/trustiot-v2#817 裡其他測試的子行程一樣，只是停在 `exec` 之前多久由測試決定，不靠負載。
3. 管線停止、關掉它的緩衝之後，測試確認鎖還在（`locked` 為真）：這時持有的只有子行程的複本。
4. 測試照正常路徑等鎖放開（`unlocked` 的同一個迴圈，另外計數找到鎖被持有幾次）。另一個 task 等到等待已經兩次看到鎖被持有，才放行子行程；等待結束時檢查子行程已經被放行。子行程 `exec` 之後鎖才會放開，所以等待在放行之前結束就表示它沒有真的等。
5. 啟動 agent 執行檔之前再確認鎖已經放開，然後照原本的測試走完：agent 連上 broker、送出那一批、收到 acknowledgement 後刪掉 segment、SIGTERM 後正常結束。另外確認 agent 的日誌沒有「is in use by another agent」：鎖被占用的話，agent 要等 60 秒（`DISK_RETRY`）才再試。
6. 最後確認子行程正常結束（`true` 的結束碼 0）。

原本的測試改成與新測試共用同一個函式（`a_buffered_batch_goes_up_and_is_released`），不建立 `HeldChild`，其餘照舊。

| 項目 | 機器 | 結果 |
| --- | --- | --- |
| `cargo clippy --locked -p agent --all-targets -- -D warnings` | arm64 VM | 通過 |
| 新測試 3 次 | arm64 VM | 3 次都通過，每次 `the wait found the lock held 2 times` |
| 突變 A：等待只檢查一次、不重複 | arm64 VM | 失敗：`the wait ended while the held child still kept the lock` |
| 突變 B：完全不等 | arm64 VM | 失敗：`the wait ended while the held child still kept the lock` |
| `tests/binary.rs` 全部（17 個） | arm64 VM | 通過，含新測試與改寫後的原測試 |
| 新測試 3 次 | x86_64 | 3 次都通過，每次 `the wait found the lock held 2 times` |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings` | x86_64 | 通過 |
| `cargo test --locked -p agent` | x86_64 | 通過：lib 360、`main.rs` 3、`tests/binary.rs` 17，其餘每個測試執行檔都通過 |
| `cargo test --locked --workspace --exclude agent --no-fail-fast` | x86_64 | 通過：260 個測試執行檔、2,065 個測試、0 失敗 |

突變 A 是第一版測試抓不到的情形：那一版只要求等待看到鎖被持有一次就放行。等待在 5 ms 的間隔裡沒有再檢查，子行程這時剛好已經 `exec`，所以測試照樣通過（1 次，建置主機的 arm64 VM）。現在要求看到兩次，等待必須在鎖仍被持有時再檢查一次，突變 A 就會失敗。

原始輸出：

- [before-fix-x86_64.txt](data/2026-10-31-820-agent-lock-wait-stress/before-fix-x86_64.txt)：修正前 200 次，最後是每次失敗的 panic 訊息。
- [after-fix-x86_64.txt](data/2026-10-31-820-agent-lock-wait-stress/after-fix-x86_64.txt)：修正後 200 次。
- [before-fix-arm64-partial.txt](data/2026-10-31-820-agent-lock-wait-stress/before-fix-arm64-partial.txt)：修正前在 arm64 VM 跑到第 103 次的輸出，失敗訊息是停止後從那次工作的日誌讀出來的。
- [injection-arm64.txt](data/2026-10-31-820-agent-lock-wait-stress/injection-arm64.txt)：clippy、新測試 3 次與兩個突變，以及第一版測試在突變 A 下仍通過的那次。
- [final-checks-x86_64.txt](data/2026-10-31-820-agent-lock-wait-stress/final-checks-x86_64.txt)：x86_64 上的 fmt、clippy、新測試 3 次、`cargo test -p agent` 與整個 workspace 的測試。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 修正前後各至少 200 次負載下的重複執行，結果記在 `docs/test-reports/`：修正後沒有任何一次 `WouldBlock` | 通過。x86_64 上修正前 200 次中 33 次失敗（8 個 `WouldBlock`、29 個因鎖開不了目錄），修正後 200 次全部通過，沒有任何一次 `WouldBlock`。另外修正前在 arm64 VM 103 次中 7 次失敗。 |
| `tests/binary.rs` 的等待在一把真的被子行程留住的鎖上觸發過一次，agent 執行檔在鎖放開後才啟動、測試通過，結果記在 `docs/test-reports/` | 通過。新測試讓子行程停在 `exec` 之前留住鎖。等待兩次看到鎖被持有，子行程被放行之後等待才結束，agent 在鎖放開後啟動並送出那一批，日誌沒有鎖被占用的訊息，測試通過。等待不重複檢查或完全不等時，測試失敗。 |

### 待測

延後的進階驗證，不在這個 issue 的範圍：

- 修正後在 arm64 VM 上的大量重複執行。這次重複執行只在 x86_64 跑，Laysi/trustiot-v2#817 已在 arm64 VM 跑過修正後 20 次全部通過。
- 新的注入測試在負載下的大量重複執行。目前只跑了幾次。它的順序由管道決定，不靠時間，所以預期不受負載影響。
