# protocol crate 驗收（2026-10-10）

對應 issue：Laysi/trustiot-v2#50 protocol crate：閘道器與雲端的 MQTT 訊息格式（M4）。原始資料在 [data/2026-10-10-50-protocol-crate/](data/2026-10-10-50-protocol-crate/)。

## 環境

- 機器：本機開發機，Fedora Linux 45（kernel 7.2.8），x86_64。本 issue 不需要 lab 端到端測試，沒有啟動容器，也沒有用到 lab 主機。
- 受測版本：分支 `feat/50-protocol-crate` 的 commit `296e0cf`，基底是 `main` 的 `9212be6`；測試時除了本報告與原始資料，工作目錄沒有其他未提交的變更。之後 rebase 到 `main` 的 `a25efe9`（只改部署與文件，沒有 Rust 程式），同一個變更成為 `4b88f1c`，重跑第 1–3 節的檢查結果相同（288 個測試通過、cargo-deny 通過、aarch64 建置成功）。
- 工具：Rust 1.99.0（rustc 1.99.0 b940084d7 2026-09-28、cargo 1.99.0），由 `.prototools` 與 `rust-toolchain.toml` 鎖定；aarch64 目標以 `rustup target add aarch64-unknown-linux-gnu` 加入（與 CI 相同，不寫進 `rust-toolchain.toml`）；cargo-deny 0.20.2 由 `.prototools` 鎖定。
- 新的第三方相依：`cbor4ii` 1.2.3（MIT，開啟 `half-f16`、`serde1`、`use_std`），因而帶進 `half` 2.7.1、`cfg-if`、`zerocopy`。

## 指令

```sh
# 1. 格式、lint、測試（repo 根目錄）
cargo fmt --all --check
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets --locked -- -D warnings
CARGO_BUILD_JOBS=2 cargo test --workspace --locked
CARGO_BUILD_JOBS=2 cargo test -p protocol --locked -- --test-threads=2

# 2. 閘道器目標（aarch64）
rustup target add aarch64-unknown-linux-gnu
cargo clean -p protocol --target aarch64-unknown-linux-gnu
CARGO_BUILD_JOBS=2 cargo build --locked --target aarch64-unknown-linux-gnu -p protocol
#   從 target/aarch64-unknown-linux-gnu/debug/libprotocol.rlib 以 ar x 取出一個 .o，再用 file 看架構

# 3. 閘道器目標的相依套件與授權，以及 CI 的相依檢查
cargo tree --locked -p protocol -e normal,build --target aarch64-unknown-linux-gnu --prefix none --format "{p}: {l}" --no-dedupe
cargo deny --locked check

# 4. 證明格式變更會讓測試失敗：暫時把 TelemetryBatch.clock_synced 加上
#    #[serde(rename = "clock_ok")]，跑 golden 測試，再還原
CARGO_BUILD_JOBS=2 cargo test -p protocol --test golden
```

## 結果

| 檢查 | 結果 | 原始資料 |
| --- | --- | --- |
| `cargo fmt --all --check` | 通過 | — |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通過，無警告 | — |
| `cargo test --workspace --locked` | 33 個測試執行檔，288 個測試全部通過 | — |
| `cargo test -p protocol` | 11 個測試執行檔，98 個測試全部通過 | [cargo-test-protocol.txt](data/2026-10-10-50-protocol-crate/cargo-test-protocol.txt) |
| aarch64 建置 | 成功（exit 0），目的檔為 `ELF 64-bit LSB relocatable, ARM aarch64` | [aarch64-build.txt](data/2026-10-10-50-protocol-crate/aarch64-build.txt) |
| 閘道器目標的相依套件 | 14 個 crate（含 protocol 本身），全部是純 Rust，授權為 MIT、Apache-2.0、BSD-2-Clause 或 Unicode-3.0 | [gateway-dependencies.txt](data/2026-10-10-50-protocol-crate/gateway-dependencies.txt) |
| `cargo deny --locked check` | `advisories ok, bans ok, licenses ok, sources ok`（duplicate 只是 warn，來自既有相依） | [cargo-deny.txt](data/2026-10-10-50-protocol-crate/cargo-deny.txt) |
| 故意改格式 | `encodings_match_the_current_golden_files` 與 `frozen_v1_payloads_still_decode` 失敗，訊息指出 telemetry 的位元組、JSON 與凍結樣本都對不上 | [golden-format-change.txt](data/2026-10-10-50-protocol-crate/golden-format-change.txt) |

`protocol` 的測試分工：

| 測試檔 | 內容 |
| --- | --- |
| `src/*.rs` 單元測試 | topic 組成與解析、ACL 規則以 Mosquitto 的比對方式模擬（只能發自己的 `up/`、訂自己的 `down/`、萬用字元 CN 拿不到權限）、ID 規則、CBOR 結構檢查、版本標記 |
| `tests/golden.rs` | 每種訊息（11 個 channel、18 個樣本）的位元組與 JSON 固定樣本、往返、凍結的 v1 樣本仍能解碼 |
| `tests/compat.rs` | 新版欄位與代碼被略過或保留、缺少的選填欄位取預設值、其他主版本與較新的修訂版被拒 |
| `tests/split.rs` | 遙測拆包（序號連續、每批不超過上限、時鐘狀態改變就換批、序號用盡不繞回、隨機輸入）、日誌與即時值拆包 |
| `tests/limits.rs` | 不拆包的訊息在最大合法內容下仍不超過上限，包括 1000 台設備最壞情況的心跳 |
| `tests/robustness.rs`、`tests/allocation.rs` | 截斷、亂碼、超深巢狀、宣告超大長度、重複欄位、非有限浮點數都回錯誤而不 panic，且不會依宣告長度配置記憶體 |
| `tests/validation.rs` | 設定（型號、資料點、設備、設備數上限）、回報、心跳、即時、日誌的驗證規則與錯誤路徑 |
| `tests/readme.rs` | README 的 topic 表（topic、訊息、QoS、retain、到期、大小上限）與 ACL 表和程式一致 |
| `tests/dependencies.rs` | 閘道器目標的相依套件等於允許清單，且沒有連結原生函式庫 |

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| `cargo build -p protocol --target aarch64-unknown-linux-gnu` 成功，且相依套件不含只能在雲端編譯的 crate | 通過 | aarch64 建置成功；相依只有純 Rust crate，`tests/dependencies.rs` 以允許清單把關 |
| 每種訊息都有往返測試與固定樣本（golden）測試，格式變更會讓測試失敗 | 通過 | `every_case_round_trips`、`encodings_match_the_current_golden_files`、`every_channel_has_a_golden_case`；故意改格式時測試失敗；`frozen_v1_payloads_still_decode` 確保新版仍能解 v1 樣本 |
| 超過大小上限的批次會被拆包，且拆包後序號規則有測試 | 通過 | `tests/split.rs` 的 `oversized_input_is_split_into_consecutive_batches_within_the_limit`、`randomized_splits_keep_every_rule`、`a_stream_never_wraps_its_sequence_numbers` 等 13 項 |
| `crates/protocol/README.md` 列出所有 topic、訊息與 ACL 樣式 | 通過 | README 的 topic 表與 ACL 表；`tests/readme.rs` 檢查兩張表與程式一致 |

閘道器與雲端實際連 Mosquitto 的端到端行為（PUBACK 0x87、retained 設定、LWT）不在本 issue 範圍，由閘道器與雲端服務各自的 issue 驗證；本 crate 的 ACL 規則以 2026-10-09 可行性測試的實測結果為依據。
