# agent：緩衝目錄鎖的等待，arm64 上的大量重複執行與注入測試在負載下的重複執行（2026-10-31）

Laysi/trustiot-v2#834：Laysi/trustiot-v2#820（PR Laysi/trustiot-v2#833）把兩項進階驗證延後到這張 issue，本報告補上：

1. 修正後在建置主機的 arm64 Linux VM 上，負載下重複執行 `cargo test --locked -p agent --lib` 200 次。
2. 注入測試 `the_agent_starts_only_once_a_childs_copy_of_the_buffer_lock_is_gone` 在負載下於 x86_64 與 arm64 各重複執行 50 次。

原因與修正見 [Laysi/trustiot-v2#817 的報告](2026-10-31-817-agent-buffer-lock-race.md)；修正前後在 x86_64 的 200 次比較與注入測試的設計見 [Laysi/trustiot-v2#820 的報告](2026-10-31-820-agent-lock-wait-stress.md)。這兩份報告與它們的原始資料沒有更動。

## 環境

- **程式碼**：main `46ced0bf155751c79b97826297d67a1117afa438`（含 Laysi/trustiot-v2#817 的修正 `55aa6349` 與 Laysi/trustiot-v2#820 的注入測試）。這次沒有改任何產品程式碼或 agent 的測試；新增的只有本報告與 `data/` 裡的腳本，腳本在建置主機上以未 commit 的檔案執行（`injection-stress.sh` 之後只改了結束時的清理，見「結果」最後一點）。輸出的 `fix present: 1` 表示 `edge/agent/src/buffer/disk.rs` 有 `test_locks` 模組。
- **機器**：
  - arm64 的兩組（lib 200 次、注入 50 次）：建置主機（arm64 Linux VM），兩組放在同一個工作裡，這個工作以獨占模式執行，佔住整台 VM：沒有其他工作時才啟動，執行期間不會有其他工作在這台啟動。
  - x86_64 的注入 50 次：建置主機（x86_64 Linux），這台一次只跑一個工作。
  - 兩台都有 systemd 使用者 manager（輸出的 `systemd user manager: yes`，lib 那組有印），所以 `modbus::address::tests` 會真的啟動 `systemd-run` 子行程，也就是 Laysi/trustiot-v2#817 的競爭來源。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0。
- **設定**：`CI=1`，需要容器的測試不會略過。
- **負載**：與 Laysi/trustiot-v2#817 相同的 [stress.sh](data/2026-10-31-817-agent-buffer-lock-race/stress.sh)（lib 那組）與複製了它的負載的 [injection-stress.sh](data/2026-10-31-834-agent-lock-wait-arm64-stress/injection-stress.sh)（注入那組）。兩者都先建好測試執行檔，再在同一個工作裡另外跑：
  - 一個迴圈：反覆在獨立的 target 目錄從頭 `cargo build --release -p identity`（`-j` 等於 CPU 數）。
  - 每個 CPU 一個忙迴圈。
  - 啟動負載後先等 20 秒才開始第一次。
- **arm64 VM 只跑這一個工作**：[exclusive-job.sh](data/2026-10-31-834-agent-lock-wait-arm64-stress/exclusive-job.sh) 在負載啟動前與全部結束後各印一次「其他檢查工作的行程數」（只印數量），負載啟動前不是 0 就以結束碼 4 結束、不啟動負載。這個工作印出的兩個數都是 **0**，負載啟動前的 1 分鐘 load average 是 0.67。這個工作之前還啟動過兩次，都在負載啟動前看到其他工作的行程（4 個與 5 個，是不遵守獨占保留的較早工作的行程），照規則以結束碼 4 結束、沒有啟動負載，隔了約 10 分鐘再啟動；這兩次沒有任何測試資料，不算進結果。

## 指令

在 repo 根目錄、建置主機上執行（`CI=1`）。

arm64（lib 200 次與注入 50 次，同一個工作，以獨占模式在建置主機的 arm64 VM 上執行）：

```sh
bash docs/test-reports/data/2026-10-31-834-agent-lock-wait-arm64-stress/exclusive-job.sh
```

它依序執行：

```sh
bash docs/test-reports/data/2026-10-31-820-agent-lock-wait-stress/stress-job.sh \
  docs/test-reports/data/2026-10-31-817-agent-buffer-lock-race/stress.sh 200
bash docs/test-reports/data/2026-10-31-834-agent-lock-wait-arm64-stress/injection-stress.sh 50
```

x86_64（注入 50 次，在建置主機的 x86_64 Linux 上執行）：

```sh
bash docs/test-reports/data/2026-10-31-834-agent-lock-wait-arm64-stress/injection-stress.sh 50
```

`injection-stress.sh` 每次執行：

```sh
cargo test --locked -p agent --test binary -- --nocapture --exact \
  the_agent_starts_only_once_a_childs_copy_of_the_buffer_lock_is_gone
```

執行前先用 `-- --list --exact <名稱>` 確認這個名稱剛好符合一個測試（輸出的 `tests matching the name: 1`）。一次執行只有在測試通過、輸出有 `test result: ok. 1 passed`、而且有 `the wait found the lock held N times` 且 N 大於等於 2 時才算通過；每次印出結果、秒數、當時的 1 分鐘 load 與 N，最後印失敗次數與 N 的分布，有任何失敗就以非 0 結束。

## 結果

| 組別 | 機器 | 次數 | 失敗 | 每次的測試數 | 每次秒數（最小／中位數／最大） | load（1 分鐘） |
| --- | --- | --- | --- | --- | --- | --- |
| lib 測試 | arm64 VM | 200 | **0** | 362 個（360 個執行、2 個 ignored） | 2／2／3 | 8.71–21.47 |
| 注入測試 | arm64 VM | 50 | **0** | 1 個 | 0.200／0.334／0.432 | 21.06–21.22 |
| 注入測試 | x86_64 | 50 | **0** | 1 個 | 0.392／0.488／0.609 | 11.44–17.69 |

- 三組都沒有任何一次失敗：lib 那組沒有任何一次 `WouldBlock`，也沒有 `open_disk` 回 `None`；注入那組每次測試都通過。
- lib 那組 200 次的結果行都是 `test result: ok. 360 passed; 0 failed; 2 ignored`。lib 那組每次的秒數只記到整數秒（`stress.sh` 的格式）。
- 注入測試的 N（等待看到鎖被持有的次數）分布：

  | 機器 | N=2 | N=3 | N=4 |
  | --- | --- | --- | --- |
  | arm64 VM | 48 | 1（第 35 次） | 1（第 47 次） |
  | x86_64 | 48 | 2（第 3、15 次） | 0 |

  這 100 次裡有 4 次 N 大於 2。測試在等待第二次看到鎖被持有時就放行子行程；負載下等待的下一次檢查可能在子行程 `exec`（鎖在這時放開）之前又發生一次，所以 N 可以是 3 或更多。測試自己的條件是「至少兩次」，而且等待在子行程被放行之前結束的話斷言會失敗，所以 N 大於 2 不是失敗，只表示等待在鎖仍被持有時多檢查了一次。issue 範圍裡寫的「兩次」在這裡讀成「至少兩次」。
- 兩個工作結束時各有一行 `rm: cannot remove '<host copy>/src/.exp817/load.…/release/deps': Directory not empty`（注入那組是 `.exp834`）：腳本結束時只殺了負載迴圈的 subshell，它底下正在跑的 cargo 還在寫檔，清理目錄時就刪不乾淨。這只影響負載的暫存目錄，發生在所有次數都跑完之後。`stress.sh`（lib 那組）沿用 Laysi/trustiot-v2#817 的版本，沒有改。`injection-stress.sh` 在這次執行之後改了清理：負載迴圈改在自己的行程群組裡執行，結束時殺整個群組，工作目錄改成每次新建的 `.exp834.XXXXXX`、結束時只刪它（commit 的版本）；測量的部分（建置、20 秒暖機、每次執行與判定）沒有動。改過的清理另以一個假的 `cargo` 與 `nproc` 在本機檢查過：2 次執行都通過，結束後沒有留下行程，暫存目錄已清除；兩個原始輸出檔是改之前的版本產生的，所以含有上述 `rm` 訊息。

原始輸出（已去掉建置主機排程自己的行）：

- [arm64-exclusive-job.txt](data/2026-10-31-834-agent-lock-wait-arm64-stress/arm64-exclusive-job.txt)：arm64 的整個工作：行程數、load、lib 200 次、注入 50 次與分布。
- [injection-x86_64.txt](data/2026-10-31-834-agent-lock-wait-arm64-stress/injection-x86_64.txt)：x86_64 的注入 50 次與分布。
- 腳本：[injection-stress.sh](data/2026-10-31-834-agent-lock-wait-arm64-stress/injection-stress.sh)、[exclusive-job.sh](data/2026-10-31-834-agent-lock-wait-arm64-stress/exclusive-job.sh)。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 修正後在 arm64 Linux VM 上，負載下至少 200 次重複執行 `cargo test --locked -p agent --lib`，沒有任何一次失敗，結果記在 `docs/test-reports/` | 通過。200 次、0 次失敗，沒有 `WouldBlock`，也沒有 `open_disk` 回 `None`；整個工作獨占 VM，前後的其他工作行程數都是 0。 |
| 注入測試在負載下於 x86_64 與 arm64 各重複執行至少 50 次，每次都通過，結果記在 `docs/test-reports/` | 通過。x86_64 50 次、arm64 50 次，0 次失敗；等待每次都至少看到鎖被持有兩次（N=2 共 96 次，N=3 共 3 次，N=4 共 1 次）。 |

### 待測

沒有。
