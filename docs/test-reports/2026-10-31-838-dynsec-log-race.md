# certs：dynsec 測試在 broker 已套用變更、服務還沒寫出日誌時就檢查日誌（2026-10-31）

Laysi/trustiot-v2#838：`services/certs/tests/dynsec.rs` 的 `the_service_keeps_the_brokers_gateway_clients_in_line_with_postgres` 在負載下偶爾在第 1105 行失敗。這份報告記錄原因、負載下的重現、決定性的實驗，以及修正後的重複執行。修正在 `services/certs/tests/dynsec.rs`（`logged_changes`），只改測試，服務的行為不變。

## 環境

- **程式碼**：分支 `fix/838-dynsec-test-flake`，基於 main `1da444a0`。
  - 修正前：main `1da444a0`。
  - 修正後：commit `892a472d`（測試的修正）。
    - 第 1 次工作跑的是 `892a472d` 的測試，`stress.sh` 是 commit 之前的版本：負載建置迴圈結束時只停掉迴圈本身，沒有停掉它啟動的 `cargo`。其餘相同。
    - 第 2 次工作跑的是 `892a472d` 本身，跑完確認沒有留下 `cargo` 或 `rustc` 行程。
- **機器**：重現、實驗與重複執行都在建置主機（x86_64 Linux）；整個 workspace 的 fmt、clippy 與測試在建置主機（arm64 Linux）。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0；測試起的容器：`eclipse-mosquitto:2.1.2-alpine` 與 `postgres:18.6-alpine`（以 digest 釘選，見 `crates/testkit`）。
- **設定**：`CI=1`，需要容器的測試不會略過。
- **負載**：重複執行時，同一個工作裡另外跑（[stress.sh](data/2026-10-31-838-dynsec-log-race/stress.sh)）：
  - 一個迴圈：反覆在獨立的 target 目錄從頭 `cargo build --release -p identity`（`-j` 等於 CPU 數）。
  - 每個 CPU 一個忙迴圈。
  - 每次執行前後各取一次 1 分鐘 load average，記在每一行輸出裡。

## 原因

1. 服務改 broker 的閘道器權限時（`services/certs/src/dynsec.rs` 的 `Broker::apply`），先把 dynamic-security 指令送到 `$CONTROL/dynamic-security/v1`，等 broker 回覆，**拿到回覆之後**才寫 INFO 日誌 `changed a gateway's broker permissions`。
2. broker 是先套用指令、存檔，然後才回覆（實驗 B 的兩處失敗顯示：broker 的檔案與連線都已經變了，服務的日誌還沒出現）。所以從 broker 套用變更到測試讀到那一行日誌之間，還有：
   - broker 送出回覆，服務的 MQTT 連線收到並交給等待的工作；
   - 服務寫出日誌到 stdout；
   - 測試收集 stdout 的執行緒讀到這一行（`tests/support/mod.rs` 的 `Output`）。
3. 測試在第 1105 行之前等的是 broker 那一端的事：同一條連線重試 publish，直到 broker 回 PUBACK 0x00。broker 一套用 `createClient`，publish 就會被接受，不必等服務收到回覆。接著測試立刻讀日誌，負載高時那一行還沒到，`changes()` 讀到空的。
4. 同一個檔案裡還有兩處是同樣的情形：
   - 第一個測試 broker 重啟後（原本第 1186–1188 行）：等的是 broker 的 dynamic-security 檔案已經有兩台閘道器、沒有 `gw-633-ghost`，接著立刻讀三台閘道器的日誌。
   - 第二個測試 `revoking_a_gateway_disconnects_it_and_its_crl_then_refuses_it` 最後的 keyCompromise 撤銷（原本第 1662 行）：等的是 broker 結束閘道器的連線（`deleteClient` 已套用），中間只隔了幾個 HTTP 與資料庫查詢，就讀三筆日誌。
   - 其他讀 `changes(...)` 的地方都已經用 `wait_for` 等到日誌出現（例如第二台閘道器的綁定、續期後的 `["removed", "added"]`），或是檢查「沒有變更」（例如第一個副本 `is_empty()`，以及服務記下 `brought the broker's gateway permissions in line with postgres` 之後再讀，那一行一定在同一輪的變更日誌之後）。
5. 服務的行為沒有問題：日誌寫的是 broker 確認過的變更，所以一定在 broker 套用之後。這是測試對順序的假設錯了。

## 指令

在 repo 根目錄、建置主機上執行（`CI=1`）。

負載下重複執行（每次的輸出寫到 `.exp838/`；`MUTATE=1` 先跑一次實驗 A，不加負載）：

```sh
MUTATE=1 bash docs/test-reports/data/2026-10-31-838-dynsec-log-race/stress.sh 40 1500   # 修正前、修正後第 1 次
bash docs/test-reports/data/2026-10-31-838-dynsec-log-race/stress.sh 20 900             # 修正後第 2 次
```

- 實驗 A：套用 [delay-log-line.patch](data/2026-10-31-838-dynsec-log-race/delay-log-line.patch)：服務拿到 broker 的回覆之後，等 2 秒才寫日誌，等於把負載下偶爾發生的延遲放大成每次都發生。修正前只跑第一個測試；修正後跑 `tests/dynsec.rs` 的兩個測試（存進 repo 的 `stress.sh` 是修正後的版本）。
- 實驗 B：修正後的測試再套用 [unfix-later-checks.patch](data/2026-10-31-838-dynsec-log-race/unfix-later-checks.patch)，把第 2、3 處改回原本的寫法（第 1 處保留修正），加上同樣的 2 秒延遲，看另外兩處是不是也需要修正：

```sh
D=docs/test-reports/data/2026-10-31-838-dynsec-log-race
git apply $D/delay-log-line.patch && git apply $D/unfix-later-checks.patch
cargo test --locked -p certs --test dynsec
git apply -R $D/unfix-later-checks.patch && git apply -R $D/delay-log-line.patch
```

其他檢查：

```sh
cargo clippy --locked -p certs --all-targets -- -D warnings
cargo test --locked -p certs
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --exclude agent
```

## 結果

| 項目 | 版本 | 結果 |
| --- | --- | --- |
| 負載下重複執行 40 次 | 修正前 | **40 次中 1 次失敗**（第 3 次，load 20.25 → 20.55，9 秒就結束）：`panicked at services/certs/tests/dynsec.rs:1105:5`，``assertion `left == right` failed``，`left: []`，`right: ["added"]`，就是 issue 裡的那一行。其餘 39 次通過，每次 18–20 秒。load 12.75–30.90。 |
| 實驗 A（服務延後 2 秒寫日誌），只跑第一個測試 | 修正前 | 失敗，訊息同上：第 1105 行，`left: []`，`right: ["added"]` |
| 實驗 A，`tests/dynsec.rs` 兩個測試 | `892a472d` | **兩個都通過**（48.33 秒） |
| 實驗 B（第 2、3 處改回原本的寫法，同樣延後 2 秒） | `892a472d` 加 `unfix-later-checks.patch` | 兩個測試都失敗，各在改回的那一處：第一個測試 `dynsec.rs:1202`，`left: []`，`right: ["removed"]`（broker 重啟後）；第二個測試 `dynsec.rs:1677`，`left: ["removed", "added"]`，`right: ["removed", "added", "removed"]`（keyCompromise 撤銷後）。所以這兩處也需要修正。 |
| 負載下重複執行 40 次 | `892a472d`（第 1 次工作） | **40 次都通過**，每次 17–20 秒，load 14.17–31.28 |
| 負載下重複執行 20 次 | `892a472d`（第 2 次工作） | **20 次都通過**，每次 17–20 秒，load 14.78–30.38；跑完 0 個 `cargo`、0 個 `rustc` 行程，負載建置的目錄已刪除 |
| `cargo clippy --locked -p certs --all-targets -- -D warnings` | `892a472d` | 通過 |
| `cargo test --locked -p certs` | `892a472d` | 通過：lib 53 個測試，`tests/` 19 個測試執行檔都通過（`dynsec` 2 個） |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo test --locked --workspace --exclude agent` | `892a472d`，建置主機（arm64 Linux），沒有加負載 | 通過：260 個測試執行檔、2,065 個測試通過、0 失敗、1 個 ignored（[workspace-arm64.txt](data/2026-10-31-838-dynsec-log-race/workspace-arm64.txt)） |

原始輸出：

- [before-fix-x86_64.txt](data/2026-10-31-838-dynsec-log-race/before-fix-x86_64.txt)：修正前的實驗 A 與 40 次重複執行。第一行原本還印了 CPU 數，存檔時拿掉了。
- [after-fix-x86_64.txt](data/2026-10-31-838-dynsec-log-race/after-fix-x86_64.txt)：修正後的兩次工作：clippy、實驗 A、實驗 B、共 60 次重複執行與 `cargo test --locked -p certs`。

## 修正

- 新的輔助函式 `logged_changes(service, hardware_id, count)`：用測試檔原有的 `wait_for`（每 100 ms 檢查一次，上限是 `tests/support` 的 `LIMIT`，120 秒）等到這台閘道器至少有 `count` 行 `changed a gateway's broker permissions`，再回傳全部的 `action`。逾時的訊息寫明在等哪台閘道器的幾行日誌。
- 三處改用它，等到預期的行數再比對內容；內容不對時仍然是原本的 `assert_eq!` 失敗訊息：
  1. 第一台閘道器綁定後 publish 被接受：等 1 行，比對 `["added"]`。
  2. broker 重啟後：`gw-633-ghost` 等 1 行比對 `["removed"]`，兩台閘道器各等 2 行比對 `["added", "added"]`。
  3. keyCompromise 撤銷後：等 3 行，比對 `["removed", "added", "removed"]`。
- 沒有加固定的等待，也沒有放寬任何既有的時間上限。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 失敗的原因有結論，能重現時附上完整的失敗訊息 | 通過。原因見「原因」。修正前在負載下 40 次中失敗 1 次（第 3 次，load 20.25 → 20.55），失敗訊息與 issue 相同：``panicked at services/certs/tests/dynsec.rs:1105:5: assertion `left == right` failed``，`left: []`，`right: ["added"]`。實驗 A 把服務寫日誌延後 2 秒，修正前每次都以同樣的訊息失敗。 |
| 修正之後，在同樣的負載下重複執行沒有失敗，次數與結果記在 `docs/test-reports/` | 通過。修正後在同樣的負載下重複執行 60 次（40 + 20）都通過，load 14.17–31.28；修正前同樣條件 40 次中失敗 1 次。實驗 A 延後 2 秒時，修正後兩個測試都通過；實驗 B 確認另外兩處不修也會失敗。 |
| `CI=1 cargo test -p certs` 通過 | 通過（`CI=1`，建置主機）。整個 workspace 的測試（不含 agent）在 arm64 Linux 上也通過。 |

### 待測

以下是延後的進階驗證，不在這個 issue 的範圍：

- 在建置主機（arm64 Linux）上同樣在負載下重複執行。issue 回報的失敗是在那台主機上看到的；這次的重現與修正後的重複執行都在 x86_64 上，原因與平台無關（是行程間的先後順序）。
- 更多次的重複執行（例如數百次），量出修正前後失敗率的差異。修正前 40 次只失敗 1 次，修正後 60 次全過只是基本證據；實驗 A 與 B（延遲 2 秒時修正前必定失敗、修正後通過）是較強的證據。
