# certs：dynsec 測試等日誌的修正，在負載下重複執行與修正前後的失敗率（2026-10-31）

Laysi/trustiot-v2#849（延後自 Laysi/trustiot-v2#838，原因、修正與基本驗證見 [2026-10-31-838-dynsec-log-race.md](2026-10-31-838-dynsec-log-race.md)，該報告不變）：用 #838 的負載（[stress.sh](data/2026-10-31-838-dynsec-log-race/stress.sh)）重複執行 `services/certs/tests/dynsec.rs` 的 `the_service_keeps_the_brokers_gateway_clients_in_line_with_postgres`，三組各 200 次：修正後在 arm64 Linux VM、修正前與修正後在 x86_64 Linux。沒有改任何產品或測試程式。

**重點**

- 修正前（x86_64）：**200 次中 6 次失敗（3.0%）**，6 次都是 issue 裡的訊息（`dynsec.rs:1105:5`，`left: []`，`right: ["added"]`）。
- 修正後（x86_64）：200 次都通過。
- 修正後（arm64 Linux VM）：**200 次中 1 次失敗（第 5 次）**。失敗的不是日誌競態（第 1105 行沒有失敗），而是測試啟動的 broker 容器在 `restart` 時，主機上固定的回送埠被別的行程佔走：`Listen failed for HOST TCP port 127.0.0.1/42977: Address already in use`（`crates/testkit/src/container.rs:55`）。這是修正後出現的失敗，依規則不重跑補數，列為發現，見「修正後 arm64 的 1 次失敗」。

## 環境

- **程式碼**：main `33cea741`（含 Laysi/trustiot-v2#848 的修正；工作目錄乾淨，沒有任何程式碼變更）。
  - 修正後三組之一（arm64、x86_64）：`33cea741` 的 `services/certs/tests/dynsec.rs`（sha256 `819c182490fa91e69fc82e2252028cc336c53c9acea19dbe26bae335d28e5a6d`，每個輸出檔第一行印出）。
  - 修正前：同一棵樹，只把 `services/certs/tests/dynsec.rs` 換成 main `1da444a0` 的版本（`git show 1da444a0:services/certs/tests/dynsec.rs`，sha256 `d0c87401322e0b821476f2622c9f092bcbd6ad6dedfd59c78219d519b1fcf595`，輸出檔第一行印出）。`1da444a0` 到 `33cea741` 之間，`services/certs`、`crates/testkit`、`crates/service-kit` 只有這一個檔案不同，所以換檔後可以編譯。換檔只發生在建置主機上該次工作自己的副本裡（[run.sh](data/2026-10-31-849-dynsec-log-race-stress/run.sh) 的 `before` 模式從工作副本裡未追蹤的 `.exp849-before/dynsec.rs` 複製過去），這個檔案沒有 commit，也不在這個 PR 裡。
- **機器**：
  - arm64 組：建置主機（arm64 Linux VM）。
  - x86_64 兩組：建置主機（x86_64 Linux）。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust；測試起的容器與 #838 相同（`eclipse-mosquitto:2.1.2-alpine`、`postgres:18.6-alpine`，以 digest 釘選，見 `crates/testkit`）。
- **設定**：`CI=1`，需要容器的測試不會略過。
- **負載**：與 #838 相同，由 [stress.sh](data/2026-10-31-838-dynsec-log-race/stress.sh) 在同一個工作裡另外跑：一個迴圈反覆在獨立的 target 目錄從頭 `cargo build --release -p identity`，加上每個 CPU 一個忙迴圈；開始第一次執行前先等 20 秒。每次執行前後各取一次 1 分鐘 load average，記在每一行輸出裡。
- **arm64 組整台 VM 獨占**：這一組是一個獨占的工作，跑的期間 VM 上沒有其他檢查工作（負載會拖慢別人的測試）。證據是工作自己在開始與結束時各數一次「VM 內屬於其他檢查工作的行程數」（只印數量）：開始 0、結束 0（見 [after-fix-arm64.txt](data/2026-10-31-849-dynsec-log-race-stress/after-fix-arm64.txt)）；開始時的檢查通過才啟動負載（不為 0 就以代碼 4 結束，沒有發生）。負載開始前的 1 分鐘 load average 是 0.08，第一次執行前（負載已開 20 秒）是 6.25。
- **建置先暖**：兩台主機都先用一般（非獨占）的工作跑 `cargo test --locked -p certs --test dynsec --no-run` 暖好 dynsec 測試的建置，所以計時的工作不含冷建置。（`stress.sh` 自己還會再做一次 `--no-run`，已暖，幾乎立刻結束。）
- **x86_64 組拆成兩個工作**：該主機一次只跑一個工作，所以每組拆成兩個 100 次的工作（時間預算 2100 秒，沒有被預算截斷，每個工作 100 次都開始了），原始輸出檔以 `part1`、`part2` 編號。

## 指令

在 repo 根目錄、建置主機上執行（`CI=1`）。`stress.sh` 的第一個參數是次數，第二個是時間預算（秒，超過就不再開始新的一次）。

```sh
# 暖建置（一般工作）
cargo test --locked -p certs --test dynsec --no-run

# 修正後，arm64 Linux VM，整台 VM 獨占的一個工作
CI=1 bash docs/test-reports/data/2026-10-31-849-dynsec-log-race-stress/run.sh after-exclusive 200 10800

# 修正前，x86_64 Linux，兩個工作（先在工作副本放好修正前的測試檔，不 commit）
mkdir .exp849-before && git show 1da444a0:services/certs/tests/dynsec.rs > .exp849-before/dynsec.rs
CI=1 bash docs/test-reports/data/2026-10-31-849-dynsec-log-race-stress/run.sh before 100 2100   # part1
CI=1 bash docs/test-reports/data/2026-10-31-849-dynsec-log-race-stress/run.sh before 100 2100   # part2

# 修正後，x86_64 Linux，兩個工作
CI=1 bash docs/test-reports/data/2026-10-31-849-dynsec-log-race-stress/run.sh after 100 2100    # part1
CI=1 bash docs/test-reports/data/2026-10-31-849-dynsec-log-race-stress/run.sh after 100 2100    # part2
```

[run.sh](data/2026-10-31-849-dynsec-log-race-stress/run.sh) 只是把 `stress.sh` 包起來：`before` 先換測試檔並印出 sha256，`after-exclusive` 在前後數其他工作的行程並在開始時不為 0 就拒絕啟動負載。`stress.sh` 與其餘 #838 的資料沒有改。

## 結果

| 組 | 版本 | 主機 | 次數 | 失敗 | 每次秒數（最小／中位數／最大） | 1 分鐘 load（最小–最大） |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | 修正後 `33cea741` | arm64 Linux VM（整台獨占） | 200 | **1**（第 5 次，不是日誌競態） | 17／18／24 | 6.25–21.23 |
| 2 | 修正前（`1da444a0` 的測試檔） | x86_64 Linux | 200（100＋100） | **6**（3.0%） | 9／18／20 | 11.94–30.59 |
| 3 | 修正後 `33cea741` | x86_64 Linux | 200（100＋100） | **0** | 17／19／20 | 12.85–30.42 |

load 取自每一行的執行前後兩個值。第 2 組失敗的 6 次各只跑 9–11 秒就結束（測試在第 1105 行失敗，沒有跑完），其餘通過的每次 17–20 秒。

### 修正前（x86_64）

| 工作 | 次數 | 失敗 | 失敗的執行 | load |
| --- | --- | --- | --- | --- |
| part1（[before-fix-x86_64-part1.txt](data/2026-10-31-849-dynsec-log-race-stress/before-fix-x86_64-part1.txt)） | 100 | 0 | — | 11.94–30.59 |
| part2（[before-fix-x86_64-part2.txt](data/2026-10-31-849-dynsec-log-race-stress/before-fix-x86_64-part2.txt)） | 100 | 6 | 第 2、11、14、63、88、96 次 | 21.68–30.13 |

- 6 次失敗的訊息完全相同，與 issue 和 #838 的重現一致：``panicked at services/certs/tests/dynsec.rs:1105:5:``，``assertion `left == right` failed``，`left: []`，`right: ["added"]`。每次失敗的完整 panic 行都在 part2 檔裡。
- 失敗率 6／200＝3.0%；#838 的 40 次裡是 1 次（2.5%），落在同一個量級。兩個 100 次的工作差很多（0 與 6），單次工作的 100 次不足以量出失敗率，這也是延後到 200 次以上的原因。
- 失敗集中在 part2，part1 的 load 範圍涵蓋同樣的高 load（最高 30.59）卻沒有失敗；這份資料沒有證據說明失敗與 load 的高低成比例，只能說在這種負載下會發生。

### 修正後（x86_64）

| 工作 | 次數 | 失敗 | load |
| --- | --- | --- | --- |
| part1（[after-fix-x86_64-part1.txt](data/2026-10-31-849-dynsec-log-race-stress/after-fix-x86_64-part1.txt)） | 100 | 0 | 22.20–30.14 |
| part2（[after-fix-x86_64-part2.txt](data/2026-10-31-849-dynsec-log-race-stress/after-fix-x86_64-part2.txt)） | 100 | 0 | 12.85–30.42 |

### 修正前後的差異

同樣的負載、同樣的主機（x86_64 Linux）、各 200 次：修正前 6 次失敗、修正後 0 次。以單尾 Fisher 精確檢定（修正前後共 6 次失敗，全部落在修正前的機率 C(200,6)／C(400,6)）約 0.015；在 #838 的實驗 A（延遲 2 秒）已經證明因果，這裡只是自然負載下的次數對照。

### 修正後 arm64 的 1 次失敗

[after-fix-arm64.txt](data/2026-10-31-849-dynsec-log-race-stress/after-fix-arm64.txt) 第 5 次（16 秒，load 12.72 → 14.38）：

```text
thread 'the_service_keeps_the_brokers_gateway_clients_in_line_with_postgres' (1503278) panicked at crates/testkit/src/container.rs:55:5:
podman ["restart", "--time", "10", "testkit-certs-dynsec-broker-1503277-1"] failed: Error: pasta failed with exit code 1:
Listen failed for HOST TCP port 127.0.0.1/42977: Address already in use
Couldn't listen on requested TCP ports
```

- 這不是 #838 修的那個失敗：沒有 `dynsec.rs:1105`，也沒有 `left`／`right` 的比對失敗。測試已經走過第 1105 行，在 broker 重啟那一步（`broker.restart()`，約第 1188 行）因為容器啟動失敗而 panic。其餘 199 次通過，其中包含同一個重啟步驟。
- 依 `services/certs/tests/dynsec.rs` 的 `free_port`（約第 187 行），broker 容器發佈的兩個回送埠是測試先綁 `127.0.0.1:0` 取得、放掉之後才交給容器的固定埠；容器重啟時要重新監聽同一個埠，負載下（負載的 `cargo build` 與其他行程大量建立連線）這個埠在放掉到重啟之間可能被別的行程拿走。上面的訊息與這個機制吻合，但這份報告沒有再重現，所以是推論，不是已證明的原因。
- 依規則沒有重跑這一組補數，也沒有因此換一個「乾淨」的 200 次。這個失敗該怎麼處理（修測試的埠配置，或視為環境雜訊）要另開 issue 判斷，不在 #849 的範圍（#849「不包含」：改服務或測試的行為；若發現新的失敗，記錄完整訊息並另開 Bug）。

## 原始輸出

都已去掉建置主機排程工具自己的行（排程、清理、工作標籤），結尾的 `exit N at HH:MM:SS (build host)` 是工作的結束碼與時間：

- [after-fix-arm64.txt](data/2026-10-31-849-dynsec-log-race-stress/after-fix-arm64.txt)：第 1 組，含兩次「其他檢查工作的行程數」與開始前的 load。結束碼 1（`stress.sh` 回傳失敗次數）。
- [before-fix-x86_64-part1.txt](data/2026-10-31-849-dynsec-log-race-stress/before-fix-x86_64-part1.txt)、[before-fix-x86_64-part2.txt](data/2026-10-31-849-dynsec-log-race-stress/before-fix-x86_64-part2.txt)：第 2 組（結束碼 0 與 6）。
- [after-fix-x86_64-part1.txt](data/2026-10-31-849-dynsec-log-race-stress/after-fix-x86_64-part1.txt)、[after-fix-x86_64-part2.txt](data/2026-10-31-849-dynsec-log-race-stress/after-fix-x86_64-part2.txt)：第 3 組（結束碼都是 0）。
- [run.sh](data/2026-10-31-849-dynsec-log-race-stress/run.sh)：包裝腳本。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 修正後在 arm64 Linux VM 上，負載下至少 200 次重複執行，沒有任何一次失敗，結果記在 `docs/test-reports/` | **未達成，需要判斷。** 200 次、整台 VM 獨占，199 次通過，1 次失敗（第 5 次），失敗原因不是日誌競態（沒有第 1105 行的比對失敗）而是 broker 容器重啟時回送埠被佔走，見「修正後 arm64 的 1 次失敗」。#838 修的那個失敗在 arm64 的 200 次裡沒有再出現，但「沒有任何一次失敗」這個字面條件沒有滿足，依規則不重跑補數。 |
| 修正前與修正後在同樣的負載下各至少 200 次，失敗次數與 load 範圍記在 `docs/test-reports/` | 通過。修正前 200 次中 6 次失敗（load 11.94–30.59），修正後 200 次中 0 次失敗（load 12.85–30.42），都在 x86_64 Linux，同樣的負載。 |

### 待測

- arm64 組上那 1 次失敗的處理：要不要修測試的回送埠配置（例如發佈埠改由容器執行環境指定、再讀回），或只當環境雜訊，另開 issue 判斷。修好之後才有意義再在 arm64 上跑一組 200 次；這份報告不重跑。
