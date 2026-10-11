# Laysi/trustiot-v2#269 protocol 解碼器強化

issue：Laysi/trustiot-v2#269。來源是 Laysi/trustiot-v2#50 的獨立驗證報告 [`2026-10-11-50-verify.md`](2026-10-11-50-verify.md) 的 F1、F2、F4 與資訊項。

## 環境

- 本機 Linux 7.2（Fedora，x86_64），8 核與其他 agent 共用；`CARGO_BUILD_JOBS=2`，cargo target 在 worktree 內。
- Rust 1.99.0（`rust-toolchain.toml`）；aarch64 交叉建置用 rustup 的 `aarch64-unknown-linux-gnu` 標準函式庫。
- 分支 `fix/269-protocol-decoder-hardening`，從 `main` 的 `0c08a66`（含 PR Laysi/trustiot-v2#266 的驗證測試）開出。修正前的探測在 `0c08a66` 上跑。
- 沒有新增相依；`tests/dependencies.rs` 的允許清單不變。

## 指令

```
cargo fmt --all --check
cargo clippy -p protocol --all-targets --locked -- -D warnings
CI=1 cargo test -p protocol --locked
RUSTDOCFLAGS="-D warnings" cargo doc -p protocol --no-deps --locked
cargo build --locked --target aarch64-unknown-linux-gnu -p protocol
# 長時間 fuzz（release）：
FUZZ_ITERS=50000 FUZZ_SEED=4 cargo test --release --locked -p protocol --test fuzz -- --nocapture --test-threads=1
```

修正前後的探測（F4 與驗證報告的 hex 重現）用暫時的測試檔 `tests/probe_*.rs` 執行，`cargo test -p protocol --locked --test <probe> -- --nocapture`，輸出存進資料目錄後刪除，沒有進 commit。

## 結果

### F4：推測的 `Host::canonical` 不一致是真的

修正前（`0c08a66`）的探測，[`f4-probe-before-fix.txt`](data/2026-10-11-269-decoder-hardening/f4-probe-before-fix.txt)：

- `::ffff:0.0.0.0`、`::ffff:224.0.0.1`、`::ffff:255.255.255.255` 都被 `Host::new` 接受。
- 共用的轉換器 `192.168.10.30:4196`：一台設備用 `rtu_over_tcp` 寫 `192.168.10.30`，另一台用 `modbus_tcp` 寫 `::ffff:192.168.10.30`（或反過來，寫成 `::ffff:c0a8:a1e`），`DesiredConfig::validate` 沒有回報 `mixed_transport`。所以驗證報告推測的繞過確實存在：同一個端點只要換一種寫法，就能混用兩種傳輸方式。

修正：`Host::new` 先用 `IpAddr::to_canonical` 把 IPv4-mapped 位址換成 IPv4 再套用規則；`Host::canonical`（改為公開）同樣正規化。修正後的探測 [`probe-after-fix.txt`](data/2026-10-11-269-decoder-hardening/probe-after-fix.txt)：三個位址被拒絕，`::ffff:1.2.3.4` 的 canonical 是 `1.2.3.4`，兩種順序都回報 `mixed_transport`。

測試：`tests/validation.rs` 的 `ipv4_mapped_hosts_follow_the_ipv4_rules`（9 個 IPv4 位址乘以 4 種 mapped 寫法：小寫、大寫、完整零、十六進位，接受結果與 canonical 都與 IPv4 寫法相同）、`devices_follow_the_connection_rules` 新增的兩個 `mixed_transport` 負向案例。

### F1：資料 map 的重複鍵

驗證報告的重現（`F1a-log-fields-repeated-key.cbor.hex`、`F1b-heartbeat-exception-codes-repeated-key.cbor.hex`）修正後都回 `Malformed("a map key appears twice")`（[`probe-after-fix.txt`](data/2026-10-11-269-decoder-hardening/probe-after-fix.txt)）。做法：`LogEntry.fields` 與 `DeviceStats.exception_codes` 改用 `codec::unique_map` 反序列化，重複鍵即失敗。

測試：`tests/adversarial.rs` 的 `a_repeated_key_in_a_data_map_is_refused`（兩種順序、新鍵仍接受、整數鍵以 `18 02` 與 `02` 兩種寬度編碼也算重複）。`tests/fuzz.rs` 的已知例外 `KNOWN_LAST_WINS_MAPS` 已刪除，重複鍵檢查涵蓋所有 map。

### F2：解碼配置

選擇收緊預留策略，並改寫 README 的說法。所有 `Vec` 欄位改用 `codec::seq` 反序列化：項目解出之前最多預留 `SEQ_PREALLOC_BYTES`（4 KiB），不再依宣告的項目數預留。

| payload | 大小 | 修正前配置高峰 | 修正後配置高峰 |
| --- | --- | --- | --- |
| 心跳，`devices` 宣告 1,000 項 | 1,017 bytes | 184,000 | 4,048 |
| 心跳，`devices` 宣告 4,200 項 | 4,217 bytes | 772,800 | 4,048 |
| 遙測，`series` 宣告 4,200 項 | 4,244 bytes | 302,401 | 4,033 |
| 五種訊息，宣告 20,000–60,000 項 | 20–60 KB | 約 1 MiB | 4,032–4,080 |

修正後的數字見 [`alloc-hostile-array-count-after-fix.txt`](data/2026-10-11-269-decoder-hardening/alloc-hostile-array-count-after-fix.txt)；修正前的數字取自驗證報告。`a_hostile_array_count_reserves_at_most_a_bounded_amount` 的上限從 1.2 MB 收緊為 `2 × SEQ_PREALLOC_BYTES`（8 KiB）。

release fuzz（種子 4，90 萬個輸入，[`fuzz-run-seed4-900k.log`](data/2026-10-11-269-decoder-hardening/fuzz-run-seed4-900k.log)）：0 個不變條件被違反、0 panic；配置最多是 payload 的 56.2 倍（驗證時約 180 倍），最壞的是 `LiveControl`：以 `op` 區分種類的 enum 由 serde 先緩衝整個 map；絕對高峰 13.2 MB 來自 1 MiB 的設定（合法內容的大小）；最慢的單次輸入 1.24 秒，同樣是 1 MiB 的設定，量測包含重複鍵與再編碼的檢查。

### 資訊項

| 項目 | 決定 | 測試或負責者 |
| --- | --- | --- |
| 文字中的 NUL | 協定層拒絕：遙測與即時文字值、日誌 target、message、欄位名稱與值、心跳 `last_failure.detail`、`TelemetryAck` 的拒絕訊息、`ConfigReport` issue 的 path 與 message。`LogEntry::new`、`with_field`、`Issue::new` 把 NUL 換成 U+FFFD | `nul_is_refused_in_every_free_text`（`tests/adversarial.rs`）、`a_refused_reading_leaves_the_builder_unchanged`（`tests/split.rs`）、`invalid_values_are_reported`（`src/value.rs`） |
| 其他控制字元 | 交給下游：協定接受；寫進資料庫不需處理，寫進服務日誌要當結構化欄位，前端以純文字呈現 | Laysi/trustiot-v2#91、Laysi/trustiot-v2#93、Laysi/trustiot-v2#118、Laysi/trustiot-v2#185；同一個測試確認 tab、換行、ESC、DEL、U+202E 被接受 |
| 心跳交叉檢查 | 交給下游：拒收心跳會讓閘道器看起來離線 | 雲端 Laysi/trustiot-v2#118（成功率除以 0、夾在 0–1、可用量大於總量）、閘道器 Laysi/trustiot-v2#117（同時取樣計數器） |
| `peek_header` 大小上限 | 協定層拒絕：超過 `MAX_PEEK_BYTES`（1 MiB + 8 KiB）回 `TooLarge`；不能用 channel 上限，因為超過上限的批次也要能回 `rejected` | `a_header_peek_reads_at_most_max_peek_bytes`（`tests/adversarial.rs`） |

決定與理由寫在 `crates/protocol/README.md` 的「解碼規則與下游責任」，接收範圍的變更列在「v1 接收範圍的變更紀錄」。

### 相容性與建置

- `tests/golden/current/` 與 `tests/golden/v1-frozen/` 沒有變更；`golden.rs`（6 項）與 `compat.rs`（10 項，含 `frozen_v1_payloads_still_decode`）通過，`golden_payloads_survive_a_decode_and_encode_byte_for_byte` 通過。
- `CI=1 cargo test -p protocol --locked`：121 項全部通過、0 失敗（[`cargo-test-protocol.txt`](data/2026-10-11-269-decoder-hardening/cargo-test-protocol.txt)）。
- `cargo build --locked --target aarch64-unknown-linux-gnu -p protocol` 成功，產物 `libprotocol.rlib`；`tests/dependencies.rs` 通過。
- clippy、fmt、`cargo doc`（`-D warnings`）都沒有警告。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| F1：重複鍵被拒（測試）；fuzz 測試的已知例外已移除 | 通過 | `a_repeated_key_in_a_data_map_is_refused`；`KNOWN_LAST_WINS_MAPS` 已刪除；驗證報告的兩個重現被拒 |
| F2：README 的配置說法與實測一致，或預留策略已收緊；有測試釘住上限 | 通過（兩者都做） | `SEQ_PREALLOC_BYTES`；README「解碼配置上限」；上限測試收緊為 8 KiB；fuzz 實測 |
| F4：兩種寫法得到相同接受結果與 canonical 形式（測試）；混合傳輸檢查有負向測試 | 通過；推測的繞過確認是真的 | `ipv4_mapped_hosts_follow_the_ipv4_rules`、兩個 `mixed_transport` 案例；修正前後的探測 |
| 資訊項逐項有決定並寫進 README；選擇拒絕的項目有測試 | 通過 | 上表；README「解碼規則與下游責任」 |
| aarch64 建置成功，golden 測試仍通過 | 通過 | 見「相容性與建置」 |
| 測試證據存到 `docs/test-reports/` | 通過 | 本報告與資料目錄 |

未涵蓋：沒有在 aarch64 上執行測試（只驗證建置）；沒有 coverage-guided fuzzing（不在範圍內）；下游的處理（Laysi/trustiot-v2#87、Laysi/trustiot-v2#91、Laysi/trustiot-v2#93、Laysi/trustiot-v2#117、Laysi/trustiot-v2#118、Laysi/trustiot-v2#185）在各自的 issue 實作時驗證。
