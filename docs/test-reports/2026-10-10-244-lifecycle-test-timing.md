# service-kit 整合測試時序假設修正（Laysi/trustiot-v2#244）

## 環境

- 機器：本機（Fedora, Linux 7.2.8 x86_64，8 核心，與約九個 agent 共用）。
- 工具：Rust 1.99.0（`rust-toolchain.toml`），Bun 1.4.2（只用來產生負載與跑迴圈）。
- 受測版本：修正前為 `origin/main` `71be25d` 的 `crates/service-kit/tests/`，修正後為分支 `fix/244-lifecycle-test-timing`。兩者的產品程式（`src/`）相同，只有測試程式不同。
- 負載：`burn.ts` 以 12 個 Bun worker 持續做 CPU busy loop（約佔 4 個核心），再加上其他 agent 同時進行的編譯，量測期間 load average 為 15 至 22。
- 修正前後的測試二進位都是 `CARGO_BUILD_JOBS=2 cargo test -p service-kit --test lifecycle --no-run` 的產物，由 `run.ts` 在 `crates/service-kit` 目錄下連續執行（等同 `cargo test` 的執行方式）。

## 指令

```sh
# 負載（量測期間持續執行）
bun burn.ts 12 900
# 修正前（origin/main 的測試）與修正後各跑 50 次，每次 8 個測試
bun run.ts <lifecycle 測試二進位> 50 <label>
# 其他會讀取子行程輸出的測試，修正後各跑 20 次
bun run.ts <errors|identity|logging|otlp 測試二進位> 20 <label>
# 一般驗證
cargo fmt --all --check
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test -p service-kit
```

`burn.ts` 與 `run.ts` 存在 data 目錄。

## 結果

| 版本 | 負載 | lifecycle 執行次數 | 失敗次數 |
| --- | --- | --- | --- |
| 修正前 | 有 | 50 | 9（`lifecycle.rs:77` 七次、`lifecycle.rs:222` 兩次） |
| 修正前（較早一批） | 有 | 20 | 3（`:77`、`:222`、`:202` 各一次） |
| 修正後 | 有 | 50 | 0 |

- 修正前的失敗都是 issue 描述的兩種：連線在 draining 後仍成功（斷言 `new connections are refused once draining`），以及輸出尚未讀完（`stdout.text()` 找不到設定鍵）。較早一批另外出現第三處同類問題 `missing_required_configuration_stops_startup`（行程結束後 `json_lines()` 是空的，`lifecycle.rs:202`），已一併修正。
- 修正後在同樣的負載下，另外跑 `errors`、`identity`、`logging`、`otlp` 各 20 次，全部通過。
- 無負載時 `cargo test -p service-kit` 全數通過；fmt 與 clippy 無警告。
- 摘要：[修正前 50 次](data/2026-10-10-244-lifecycle-test-timing/old-tests-50-runs.txt)、[修正前較早 20 次](data/2026-10-10-244-lifecycle-test-timing/old-tests-first-20-runs.txt)、[修正後 50 次](data/2026-10-10-244-lifecycle-test-timing/fixed-tests-50-runs.txt)。

## 結論

- 修正內容（只改測試程式）：
  - `tests/support/mod.rs`：`Output` 記錄讀取執行緒是否已讀到 EOF，新增 `wait_closed`；`Service::wait_exit` 在行程結束後等 stdout 與 stderr 讀完才回傳。
  - `tests/lifecycle.rs`：draining 後改為重試連線直到收到 `ConnectionRefused`（上限 5 秒）；以 `spawn_example` 啟動、行程結束後才讀輸出的兩個測試，改為先等讀到 EOF。
  - `tests/identity.rs`：行程結束後檢查日誌不含某些內容之前，先等讀到 EOF，避免「不含」的斷言因輸出尚未讀完而空過。
  - `tests/otlp.rs`：假的 collector 先記錄請求再回應；服務在結束時會等回應，所以行程結束後不會有尚未記錄的請求（原本是回應後才記錄，測試讀取的時間點可能早於記錄）。這一項是結構性修正，沒有單獨重現過失敗。
- 驗收條件 1（高負載連跑至少 50 次，0 次失敗）：通過，見上表。
- 驗收條件 2（回歸驗證）：通過。修正前的測試在同樣負載下 50 次中失敗 9 次，修正後 0 次。
- 驗收條件 3（仍驗證原本的行為）：通過。連線重試仍必須收到 `ConnectionRefused` 才算通過，逾時就失敗；日誌內容的斷言沒有放寬，只是改成在輸出完整之後才判斷。
- 未處理：`sigterm_fails_readiness_at_once_and_drains_requests_in_flight` 內「readiness 在 drain delay 內被檢查」（`ready_checked < 1s`）本質上是對時間的斷言，在極端負載下仍可能失敗。修正後的 50 次負載執行中沒有出現，不在這次修正範圍。
