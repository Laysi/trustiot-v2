# service-kit 容器測試 draining 斷言的時序修正（Laysi/trustiot-v2#259）

## 環境

- 機器：本機（Fedora, Linux 7.2.8 x86_64，8 核心，與其他 agent 共用）；podman 6.1.3，沒有 docker。
- 工具：Rust 1.99.0（`rust-toolchain.toml`），Bun（只用來產生負載與跑迴圈）。
- 受測版本：修正前為 `origin/main` 的 `crates/service-kit/tests/container.rs`（`4d129f2`，與 `a4bfe2b` 在這個檔案上相同）；修正後為分支 `fix/259-container-drain-flake`。產品程式（`src/`）兩者相同，只有這個測試檔不同。
- 修正前的測試以 `tests/container_old.rs` 的名稱另外編譯，只在本機驗證時存在，沒有提交；兩個二進位用同一份 `tests/support`，在 `crates/service-kit` 目錄下由 `run.ts` 連續執行（等同 `cargo test` 的執行方式）。
- 負載：`burn.ts` 以 12 個 Bun worker 做 CPU busy loop；前半段另有一個 `CARGO_BUILD_JOBS=4` 的 `cargo build --workspace`（獨立 target 目錄）。量測期間 load average 約 15 至 30（8 核心）；執行期間 MemAvailable 一直在 7 GB 以上。

## 原因驗證

issue 的推測成立，並且有兩處補充：

1. 服務在 SIGTERM 之後只有 `drain_delay`（測試設 1 秒）的時間回 503，之後關閉 listener。舊測試的輪詢期限是從 `podman stop` **啟動前** 起算的 1 秒（`stop_started.elapsed() < 1s`），所以 `podman stop` 啟動與送出訊號花掉的時間直接吃掉 503 的時間窗。
2. 在 `podman` 前面放一個只對 `stop` 睡 1 秒再執行真正 podman 的包裝（`data/.../podman-slow-stop.sh`），舊測試 5 次全部失敗在 `container_old.rs:160`（即 `readiness fails within the drain delay`），與 issue 描述的行號相同；延遲 0 秒時舊測試通過。
3. 補充：舊測試的 in-flight 請求是 3 秒，從 `stop` 之前 0.5 秒開始。訊號延遲超過約 2.5 秒時，請求會在訊號之前結束，測試就不再驗證「draining 期間仍服務 in-flight 請求」。同一檔案的 `stop_took < GRACE_PERIOD - 5s`（grace 15 秒）也沒有給慢速 `stop` 留餘裕。

### 與 #51 驗證者的測量對照（`podman stop` 送出 SIGTERM 要多久）

協調者轉述：#51 的獨立驗證者（PR Laysi/trustiot-v2#280）在這台機器量到 `podman stop` 呼叫後約 3.5 秒才送出 SIGTERM，並在其 `verify_container.rs` 改用 `podman kill --signal TERM`。這與上面的原因是同一件事：訊號比 1 秒的輪詢期限晚到。確認與取捨如下。

- 我這邊的量測（`data/.../signal-latency.txt`，`latency.ts`）：用日誌估計「`stop` 開始到服務記錄 SIGTERM」的時間，修正後高負載 40 次為 1.7 至 4.4 秒（中位數 2.7 秒），最終程式高負載 10 次為 0.8 至 2.8 秒。這個估計是上界：`podman stop` 在服務結束之後還要拆容器，才會返回，這段時間也被算進去。修正前在負載下通過的 17 次估計值為 0.2 至 2.7 秒，但它們必然在 1 秒內收到訊號，所以多餘的部分（最多約 1.7 秒以上）就是這個拆除時間，不能把估計當成精確延遲。結論是：訊號延遲在高負載下可超過 1 秒，數秒是可信的，與驗證者的 3.5 秒一致。
- 用 alpine 容器加相同旗標（`--read-only`、`--cap-drop ALL`、`--publish` 等）直接量 `podman stop` 到 TERM trap 的時間（`stoplat.sh`），在 load 11 至 21 時只有 0.0 至 0.6 秒（量測誤差約 0.5 秒），量不到數秒的延遲；所以數秒級的延遲不是每次都有，視負載與當時 podman 的狀態而定，這也是它「偶發」的原因。
- 取捨：不改用 `podman kill --signal TERM`。`podman kill` 可以去掉訊號延遲，但這個測試要證明的是「kubelet 的停止方式」：SIGTERM，然後 grace period 到了才 SIGKILL，以及行程在 grace period 內以 0 結束；`podman stop --time` 才有這個語意。把測試寫成不依賴訊號何時到達，一樣穩定，又保留了這個語意。修正後容許的訊號延遲約 8 秒（見下），高於觀察到的任何數值。

## 修正（只改 `crates/service-kit/tests/container.rs`）

- 輪詢從 `stop` 啟動起算，直到看到第一個 503，期限 `READINESS_FAILS_BY = 30s`。503 只存在於「SIGTERM 到 listener 關閉」之間，所以看到 503 就已經證明 readiness 在 drain delay 內失敗；訊號多晚到都不影響。連線失敗（訊號之前，或 listener 關閉之後）不算答案，重試到期限，逾時時印出最後一次結果與容器日誌。
- `DRAIN_DELAY` 由 1 秒改為 3 秒（以 `TRUSTIOT__SHUTDOWN__DRAIN_DELAY` 傳入），輪詢卡住幾百毫秒時仍有時間看到 503。
- in-flight 請求由 3 秒改為 9 秒（`/v1/slow` 上限 10 秒）。它必須在 listener 關閉（SIGTERM 後 `DRAIN_DELAY`）之後仍在執行，訊號可晚到約 8 秒；看到 503 時另外斷言請求尚未結束（`!in_flight.is_finished()`），否則測試失敗並說明 draining 沒有被驗證到。
- `GRACE_PERIOD` 由 15 秒改為 40 秒，`stop_took < GRACE_PERIOD - 5s` 的斷言保留（35 秒）。`stop` 要等 in-flight 請求，修正後正常約 9 至 12 秒；會撞到 grace period 的只有忽略 SIGTERM 的 PID 1（40 秒後被 SIGKILL，exit code 137、耗時超過 35 秒），測試照樣失敗。
- 不變的斷言：503 的 body `status == "draining"`、in-flight 請求回 200 且 `waited_ms` 等於請求值、`podman stop` 成功、exit code 為 0（不是 137）、日誌有 `shutdown requested; readiness now fails`（signal 為 SIGTERM）與 `shutdown complete`。

## 指令

```sh
# 負載（跑測試期間持續）
bun burn.ts 12 3000
CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR=<worktree>/target-load cargo build --workspace

# 測試二進位
CARGO_BUILD_JOBS=4 cargo test -p service-kit --test container --test container_old --no-run

# 修正後，高負載下連跑 40 次
bun run.ts <container 測試二進位> 40 fixed-natural-load
# 注入延遲：podman-slow-stop.sh 放在 PATH 最前面並命名為 podman，STOP_DELAY 為秒數
STOP_DELAY=1 bun run.ts <二進位> 5 <label>    # 舊、新各一批；STOP_DELAY=3 同
# 修正前，同樣負載下 20 次
bun run.ts <container_old 測試二進位> 20 old-natural-load

# 一般驗證
cargo fmt --all --check
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test -p service-kit
```

`regress.sh`、`final.sh` 是實際用過的批次腳本（路徑為當時的 worktree 與 scratchpad）。

## 結果

| 測試 | 條件 | 次數 | 失敗 |
| --- | --- | --- | --- |
| 修正後 | 高負載（12 個 busy worker 加 cargo build，load 約 21 至 30） | 40 | 0 |
| 修正後（最終提交的程式，重新驗證） | 高負載（12 個 busy worker，load 約 15 至 30） | 10 | 0 |
| 修正前 | 高負載（12 個 busy worker，load 約 15 至 30） | 20 | 3（run 02、03、07，皆為 `container_old.rs:160`） |
| 修正前 | `podman stop` 前注入 1 秒 | 5 | 5（`:160`） |
| 修正前 | `podman stop` 前注入 3 秒 | 5 | 5（`:160`） |
| 修正後 | 注入 1 秒 | 5 | 0 |
| 修正後 | 注入 3 秒 | 5 | 0 |
| 修正後（最終提交的程式） | 注入 3 秒 | 2 | 0 |

- 修正後 40 次高負載執行的 `podman stop` 總耗時（含 9 秒 in-flight 請求）：最短 9.3 秒、中位數約 10.2 秒、最長 11.7 秒，全部遠低於 35 秒的上限，exit code 皆為 0。
- 修正後在前期（含 4 工作的 cargo build 同時進行）與 40 次執行期間，沒有一次失敗；修正前在負載下 20 次中失敗 3 次，與 issue 的現象相同。
- 最終提交的程式在修正前驗證之後只有微調（以 `IN_FLIGHT_MS: u64` 常數取代 `Duration` 轉型以通過 clippy，以及註解換行），行為相同，因此另外在高負載跑了 10 次與注入 3 秒 2 次。另外 `cargo test -p service-kit` 全部通過（`container` 1 個測試通過）。
- 日誌與摘要：[summary.txt](data/2026-10-11-259-container-drain-flake/summary.txt)、[修正前失敗範例](data/2026-10-11-259-container-drain-flake/old-test-failure-example.log)、[修正後注入 3 秒範例](data/2026-10-11-259-container-drain-flake/fixed-test-stop-delay-3s-example.log)；[signal-latency.txt](data/2026-10-11-259-container-drain-flake/signal-latency.txt)；腳本 `burn.ts`、`run.ts`、`latency.ts`、`stoplat.sh`、`podman-slow-stop.sh`、`regress.sh`、`final.sh` 也在該目錄。

## 結論

- 驗收條件 1（高負載連跑至少 30 次，0 次失敗）：通過，40 次 0 失敗，另有最終程式 10 次 0 失敗。
- 驗收條件 2（回歸驗證）：通過。同樣的負載下，修正前 20 次失敗 3 次；在 `podman stop` 前注入 1 秒或 3 秒延遲，修正前 10 次全部失敗在原斷言，修正後同樣條件 12 次全部通過。
- 驗收條件 3（仍驗證原本的行為）：通過。503 加 `draining`、in-flight 請求仍完成並回 200、exit code 0、收尾時間低於 grace period 的斷言都在；503 改成必須在 in-flight 請求尚未結束時看到，比舊測試更嚴格。放寬的只有「等 503 的期限」，其餘（drain delay、in-flight 長度、grace period）是為了讓時間窗不依賴訊號何時到達而加大。
- 驗收條件 4（證據存到 `docs/test-reports/`）：本報告與 data 目錄。
- 產品程式沒有發現問題：`shutdown.rs` 的行為與其文件一致（SIGTERM 後立即 503，`drain_delay` 後停止接受連線，等 in-flight 請求完成）。
- 未處理：測試仍有一個依賴時間的假設，即 in-flight 請求要在訊號之後約 8 秒內被送達，超過時測試會以明確訊息失敗（不是誤判通過）。實測沒有接近這個值。
