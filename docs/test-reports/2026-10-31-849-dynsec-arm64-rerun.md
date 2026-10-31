# certs：dynsec 測試在 arm64 Linux VM 的負載下重跑 200 次，broker 重啟的埠修正之後（2026-10-31）

Laysi/trustiot-v2#849 的第一條驗收條件：修正後在 arm64 Linux VM 上，負載下至少 200 次重複執行，沒有任何一次失敗。[第一份報告](2026-10-31-849-dynsec-log-race-stress.md)（不變）的 arm64 組 200 次中有 1 次失敗，不是日誌競態，而是測試啟動的 broker 容器在 `restart` 時發佈的回送埠被別的行程佔走（`Address already in use`，`crates/testkit/src/container.rs:55`）。該失敗由 Laysi/trustiot-v2#857 修正（PR Laysi/trustiot-v2#869，main `61dd8437`：broker 重啟期間一直持有主機埠）。這份報告在修正之後，以同樣的方法與負載重跑 arm64 組。沒有改任何產品或測試程式。

**重點**

- 修正後（arm64 Linux VM，整台 VM 獨占）：**200 次中 0 次失敗**。
- 第一份報告裡 arm64 組的失敗類型（埠被佔走）與 #838 的日誌競態（`dynsec.rs:1105`）在這 200 次裡都沒有再出現。
- 依規則這是單一一組 200 次，沒有挑選或補跑。

## 環境

- **程式碼**：本分支的 merge commit `2eb8d7b25783a01e90d816d15f270746c51cbd9b`，是第一份報告的分支（`470aa8dc`）合併 main `61dd8437` 的結果，所以含 Laysi/trustiot-v2#848（日誌競態修正）與 Laysi/trustiot-v2#869（埠修正）。工作目錄乾淨，沒有任何程式碼變更。`services/certs/tests/dynsec.rs` 的 sha256 是 `892fd74cbeb3147c3ff03076c34ef0277dd925dee6be26bfed472328bf68e371`（輸出檔第一行印出；第一份報告的 arm64 組是 `819c182490fa91e69fc82e2252028cc336c53c9acea19dbe26bae335d28e5a6d`，差別就是 #869）。
- **機器**：建置主機（arm64 Linux VM），與第一份報告的 arm64 組相同。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust；測試起的容器與第一份報告相同（以 digest 釘選，見 `crates/testkit`）。
- **設定**：`CI=1`，需要容器的測試不會略過。
- **負載**：與第一份報告和 Laysi/trustiot-v2#838 相同，由 [stress.sh](data/2026-10-31-838-dynsec-log-race/stress.sh) 在同一個工作裡另外跑：一個迴圈反覆在獨立的 target 目錄從頭 `cargo build --release -p identity`，加上每個 CPU 一個忙迴圈；開始第一次執行前先等 20 秒。每次執行前後各取一次 1 分鐘 load average，記在每一行輸出裡。
- **整台 VM 獨占**：這一組是一個獨占的工作，跑的期間 VM 上沒有其他檢查工作。證據是工作自己在開始與結束時各數一次「VM 內屬於其他檢查工作的行程數」（只印數量）：開始 0、結束 0（見 [after-fix-arm64.txt](data/2026-10-31-849-dynsec-arm64-rerun/after-fix-arm64.txt)）；開始時不為 0 就以代碼 4 結束、不啟動負載，沒有發生。負載開始前的 1 分鐘 load average 是 0.85，第一次執行前（負載已開 20 秒）是 8.83。
- **建置先暖**：先用一般（非獨占）的工作跑 `cargo test --locked -p certs --test dynsec --no-run` 暖好建置，所以計時的工作不含冷建置。

## 指令

在 repo 根目錄、建置主機上執行。

```sh
# 暖建置（一般工作）
cargo test --locked -p certs --test dynsec --no-run

# 修正後，arm64 Linux VM，整台 VM 獨占的一個工作；200 次，時間預算 10800 秒
CI=1 bash docs/test-reports/data/2026-10-31-849-dynsec-log-race-stress/run.sh after-exclusive 200 10800
```

[run.sh](data/2026-10-31-849-dynsec-log-race-stress/run.sh) 是第一份報告已有的包裝腳本，沒有改；`stress.sh` 與其餘 #838 的資料也沒有改。時間預算沒有截斷這一組，200 次都開始並結束。

## 結果

| 組 | 版本 | 主機 | 次數 | 失敗 | 每次秒數（最小／中位數／最大） | 1 分鐘 load（最小–最大） |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 修正後（含 #848 與 #869，`2eb8d7b2`） | arm64 Linux VM（整台獨占） | 200 | **0** | 16／18／24 | 8.83–20.88 |

load 取自第一次執行前的值與每一行的執行前後兩個值。對照第一份報告同一組：200 次、1 次失敗、17／18／24 秒、load 6.25–21.23；負載與每次耗時一致，所以這一組和上一組的條件相同。

- 200 次都是 `test result: ok. 1 passed; 0 failed`，沒有任何 panic 行；`stress: 0 of 200 runs failed`。
- 其他檢查工作的行程數：開始 0、結束 0。

## 原始輸出

已去掉建置主機排程工具自己的行（排程、清理、工作標籤）；結尾的 `exit N at HH:MM:SS (build host)` 是工作的結束碼與時間：

- [after-fix-arm64.txt](data/2026-10-31-849-dynsec-arm64-rerun/after-fix-arm64.txt)：含兩次「其他檢查工作的行程數」、開始前的 load 與 200 行逐次結果。結束碼 0。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 修正後在 arm64 Linux VM 上，負載下至少 200 次重複執行，沒有任何一次失敗，結果記在 `docs/test-reports/` | **達成。** 修正 #838 與 #857 之後，200 次、整台 VM 獨占，0 次失敗（本報告）；上一輪 arm64 的失敗由 Laysi/trustiot-v2#857（PR Laysi/trustiot-v2#869）修正。 |
| 修正前與修正後在同樣的負載下各至少 200 次，失敗次數與 load 範圍記在 `docs/test-reports/` | 通過，見[第一份報告](2026-10-31-849-dynsec-log-race-stress.md)：修正前 200 次中 6 次失敗，修正後 200 次中 0 次失敗（x86_64 Linux）。 |

這份報告沒有再讓 x86_64 的兩組重跑：它們測的是 #838 的日誌競態，與 #869 無關，結論不變。

### 待測

無。
