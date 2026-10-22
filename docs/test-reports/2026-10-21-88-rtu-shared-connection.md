# 閘道器 agent 的 RTU over TCP 與共用連線（2026-10-21）

Laysi/trustiot-v2#88：`edge/agent` 的 Modbus 讀取支援 RS485 轉換器的 RTU over TCP（CRC、由功能碼與 byte 數切框、重新同步、哪個回應算數），並讓同一個端點（轉換器）後面的設備共用一條連線、依請求輪流使用。規則與失敗的對應見 `edge/agent/README.md`「RTU over TCP（RS485 轉換器）」。

## 環境

- **程式碼**：分支 `feat/88-rtu-shared-connection`，基於 main `dedd5f0`。下列結果是在 commit 前、同一份 agent 程式碼加在 `2af6405` 之上時執行的；`2af6405..dedd5f0` 之間 main 的提交沒有改任何 Rust 路徑（`git diff 2af6405 dedd5f0 --stat -- crates edge tools services Cargo.toml Cargo.lock` 沒有輸出）。
- **工具版本**：依 `rust-toolchain.toml`（Rust 1.99.0）、`.prototools`（cargo-deny 0.20.2、gitleaks 8.30.1）與 `Cargo.lock`（tokio 1.53.2）。
- **機器**：開發者工作站（x86_64）。aarch64 只做交叉建置，沒有在閘道器上執行。
- 不需要容器、叢集或 lab 的硬體：testkit 的假 Modbus 端點（假 RS485 轉換器）、手動驅動的 raw socket 與 client 都在測試行程內，只用 127.0.0.1。

## 指令

在 repo 根目錄：

```sh
cargo test -p agent                                     # 單元測試與四個整合測試
cargo test -p agent --test modbus_rtu_converter         # 對假 RS485 轉換器
cargo test -p agent --test modbus_rtu_framing           # raw socket 的 RTU 框架
cargo test --workspace
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
cargo build --locked --target aarch64-unknown-linux-gnu -p agent
cargo deny --locked check
gitleaks dir --redact edge/agent
```

Mutation 檢查：把下表的一處改壞，執行 `cargo test -p agent --no-fail-fast`，確認有測試失敗，再還原（每次只改一處；還原後與事前保留的副本比對，經 rustfmt 後相同）。

重複執行：四個 agent 整合測試執行檔，16 個並行、5 輪（共 320 次）；兩個新的執行檔（`modbus_rtu_converter`、`modbus_rtu_framing`）另外各連續執行 40 次。沒有額外的 CPU 負載。

## 結果

### `cargo test -p agent`：88 通過、0 失敗

單元測試 57 個、`tests/modbus_fake_endpoint.rs` 9 個、`tests/modbus_rtu_converter.rs` 8 個、`tests/modbus_rtu_framing.rs` 6 個、`tests/modbus_tcp_framing.rs` 8 個。整合測試都用 `#[tokio::test(start_paused = true)]` 加 `testkit::clock::manual()`，逾時只在測試推進時鐘時觸發；所有等待都經過 `testkit::clock::bounded` 或假端點的 `wait_for_*`，不依賴固定的短時間。

| 項目 | 測試 | 驗證的內容 |
| --- | --- | --- |
| CRC 與請求框架 | `rtu::tests::crc_matches_the_specification_example_and_testkit`、`rtu::tests::requests_are_rtu_frames`、`tcp::tests::the_framing_follows_the_transport`、`requests_are_rtu_frames_on_the_wire` | CRC-16 與規格的例子及 testkit 的 `crc16` 一致；請求固定 8 bytes（站號、功能碼、起始位址、筆數、CRC，例：站號 1 讀 10 個 holding register 是 `01 03 00 00 00 0A C5 CD`）；raw socket 收到的 bytes 就是這些框架；Modbus TCP 的請求仍是 MBAP |
| 由功能碼切框 | `rtu::tests::the_front_is_read_by_function_code`、`the_answer_is_taken_and_what_follows_stays` | 功能碼 1–4 由 byte 數推出長度、5／6／15／16 為 8 bytes、例外為 5 bytes；不合理的 byte 數、其他功能碼與 CRC 錯不是框架；答案之後到達的 bytes 留在緩衝 |
| 哪個回應算數 | `frames_of_other_units_are_counted_and_skipped`、`a_frame_of_the_unit_that_does_not_fit_is_skipped_and_remembered`、`an_exception_is_the_answer` | 其他站號的完整框架丟掉並計入 `unit_mismatches`；站號相同但功能碼或 byte 數對不上的框架不算回應，記下來；該功能碼的例外回應就是答案 |
| 回送、雜訊、逐 byte | `an_echo_of_the_request_is_skipped`、`noise_before_the_answer_is_skipped`、`noise_that_reads_as_the_start_of_a_long_frame_does_not_hide_the_answer`、`an_answer_arriving_byte_by_byte_is_reassembled` | 開頭與請求相同的 8 bytes 跳過（只收到前段時先等後段）；雜訊逐 byte 跳過、重新同步；像長框架開頭的雜訊不會蓋住後面已完整到達的答案；逐 byte 到達的答案湊滿才處理 |
| 逾時時回報什麼 | `a_request_without_an_answer_says_what_arrived_instead` | 什麼都沒收到是 `Timeout`；只收到該站號但對不上的框架是 `Malformed` 並說明原因；只收到湊不成框架的 bytes 是 `Malformed`，說明先寫觀察到的事實（N bytes 湊不成 RTU 框架、沒有答案），再列出可能的原因（序列埠設定不同或匯流排上的雜訊），不指定單一原因 |
| 請求之間到達的東西 | `rtu::tests::frames_between_requests_are_dropped_and_a_cut_frame_stays`、`tcp::tests::rtu_frames_between_requests_are_dropped_and_a_cut_one_too`、`mbap::tests::*`（6 個） | RTU：請求之間到達的完整框架丟掉（其他站號的計入下一個請求），送出前清空整個緩衝，被切斷的框架也丟掉；Modbus TCP 的 MBAP 切框與交易 ID、站號比對維持原行為（切一半的框架留著） |
| 共用連線的輪流 | `endpoint::tests::devices_take_turns_request_by_request`、`a_read_dropped_while_waiting_for_its_turn_gives_it_up`、`devices_at_one_endpoint_share_its_connection` | 同一端點的設備一次一個請求輪流，不是一台設備整個輪詢做完才換；等輪到時被丟掉的讀取放棄它的順位，不卡住後面的；端點以傳輸、主機（大小寫、IPv4 對應位址視為同一個）與埠為鍵，同一個鍵得到同一條連線 |
| raw socket：框架與重連 | `an_echo_another_units_answer_and_the_answer_in_one_write`、`a_frame_cut_before_the_next_request_does_not_hide_its_answer`、`a_kept_open_connection_closed_during_a_request_is_replaced`、`noise_and_frames_with_a_bad_crc_before_the_answer_are_skipped`、`what_arrives_instead_of_an_answer_decides_the_failure` | 回送、其他站號的答案與自己的答案在同一次寫入到達仍取得自己的值；請求前被切斷的框架不會藏起答案；保持中的連線在請求時被關閉，在新連線重送；雜訊與 CRC 錯的框架跳過；逾時時收到的東西決定回報的失敗（與上一列同一組規則，從線上驗證；雜訊的說明含觀察到的事實與「possible causes」） |
| 對假轉換器讀各種功能碼與型別 | `every_function_code_and_type_reads_over_rtu` | 功能碼 1–4 都經 RTU 讀取：coil 與 discrete input 的 bool，holding register 上的 i16（scale）、u32（`cdab`）、f32、字串，input register 上的 i16（scale 與 offset），值正確；假端點收到的請求都是功能碼 1–4、CRC 正確。每種型別與位元組順序的解碼與傳輸方式無關，由 Laysi/trustiot-v2#87 的 `every_type_and_byte_order_reads_from_the_fake_endpoint` 涵蓋 |
| 例外 | `an_exception_over_rtu_fails_only_its_request` | 只有該請求的點失敗（例外碼），其他照讀 |
| 驗收條件 2 | `devices_on_one_converter_share_one_connection_and_take_turns` | 站號 1–3 各兩個讀取（holding 0、input 0），三台同時輪詢兩輪：`endpoints.len()` 為 1、三個連線 `same`；每台的值都是自己的（站號 × 100 + 1、+ 2）、沒有失敗、`unit_mismatches` 為 0；假端點的請求紀錄是站號 1、2、3 輪流、兩輪重複（`(1,3,0,1)`、`(2,3,0,1)`、`(3,3,0,1)`、`(1,4,0,1)` …）；假端點只有 1 條連線；只送功能碼 1–4 |
| 驗收條件 1 | `broadcast_responses_never_land_on_another_device` | 見下 |
| 靜默的設備 | `a_silent_device_holds_the_others_up_by_one_timeout_per_request` | 站號 1 在匯流排上但不回應：每個請求拖慢後面的設備一個逾時，其他設備的值仍正確，請求順序為 1、2、3、1，仍是 1 條連線 |
| 其他設備晚到的答案 | `a_siblings_late_answer_is_a_unit_mismatch_not_a_value` | 站號 1 在請求逾時 1 秒後才回答，當時站號 2 形狀相同的請求在共用連線上等待：站號 2 讀到自己的 200，不是站號 1 晚到的 100，晚到的答案計 1 次 `unit_mismatches` |
| 形狀不同的晚到答案 | `a_late_answer_of_another_shape_is_not_taken_for_the_next_request` | 同一站號的 holding 0..=1 逾時後才到的答案（byte 數 4），不會被當成後面 holding 100 的答案（byte 數 2） |
| 重現已記載的限制：形狀相同的晚到答案 | `known_limitation_a_late_answer_of_the_same_shape_is_taken_for_the_next_request` | 重現 `edge/agent/README.md`「限制」記載的限制，呈現風險而不是期望的行為：同一站號的 holding 0 與 holding 100（各 1 個暫存器，值 100 與 200），holding 0 的答案在請求逾時 1 秒後才到，當時 holding 100 的請求在等：holding 0 逾時，holding 100 讀到的是 holding 0 的 100（不是自己的 200），沒有失敗、`unit_mismatches` 為 0。移除這個限制的人必須改這個測試 |

`broadcast_responses_never_land_on_another_device` 的設定：假轉換器注入 `Fault::broadcast().held()`，每個回應送給所有連線，且在測試釋放前不送出。電表（站號 1，讀 2 個暫存器）與驅動器（站號 2）共用一條連線；另一個主站（模擬現場的 SCADA）用第二條連線讀站號 1 的 1 個暫存器（筆數與電表不同）與站號 3。以 `wait_for_requests` 與 `release` 固定交錯的順序。檢查：

- 電表讀到自己的 11、12，`unit_mismatches` 為 0（另一個主站的站號 1 回應 byte 數不同，不被採用）。
- 驅動器讀到自己的 21，站號 3 的回應被丟掉，`unit_mismatches` 為 1。
- 另一個主站讀到自己的 11 與 31；它丟掉電表的回應，第二次讀取的 `unit_mismatches` 為 1。
- 假端點的請求紀錄（連線、站號、筆數）依序為 `(0,1,1)`、`(1,1,2)`、`(0,3,1)`、`(1,2,1)`，共 2 條連線（我方一條、另一個主站一條）；只送功能碼 1–4。

### Mutation 檢查：7 處改壞，每一處都讓至少一個整合測試與一個單元測試失敗

| 改壞的地方 |
| --- |
| RTU 在下一個請求前保留被切斷的框架（`before_request` 不清空） |
| RTU 不過濾站號 |
| 站號相同但裝不進請求的框架被當成答案 |
| 設備不共用連線（`Endpoints` 對每台設備給新連線） |
| 不完整的開頭之後不往後找答案 |
| 不跳過回送 |
| `timed_out` 一律回傳 `Timeout` |

失敗的測試名稱當時沒有逐一記下，這裡只記「每一處都有整合測試與單元測試失敗」。

### 重複執行

| 項目 | 次數 | 結果 |
| --- | --- | --- |
| 四個 agent 整合測試執行檔，16 個並行 × 5 輪 | 320 | 0 失敗 |
| `modbus_rtu_converter`、`modbus_rtu_framing` 各連續執行 | 各 40 | 0 失敗 |
| `known_limitation_a_late_answer_of_the_same_shape_is_taken_for_the_next_request`（review 後加入）連續執行 | 20 | 0 失敗 |

### 其他檢查

| 指令 | 結果 |
| --- | --- |
| `cargo test --workspace` | 875 通過、0 失敗（114 個測試執行檔） |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 沒有警告 |
| `cargo fmt --all --check` | 通過 |
| `cargo build --locked --target aarch64-unknown-linux-gnu -p agent` | 成功（只做交叉建置，沒有在閘道器上執行） |
| `cargo deny --locked check`（本分支） | bans、licenses、sources ok；advisories 失敗於 RUSTSEC-2026-0382（`rustls-webpki 0.103.15`）。main 上同樣失敗，與本變更無關，由 Laysi/trustiot-v2#485 另行修正（`cargo update -p rustls-webpki` 升到 0.103.16 後，advisories、bans、licenses、sources 都 ok） |
| `gitleaks dir --redact edge/agent` | 沒有發現洩漏 |

## 結論

逐條對照 Laysi/trustiot-v2#88 的驗收條件：

- [x] testkit 假轉換器把回應廣播給所有連線時，各設備的值不會錯置（測試）：主要證據是 `broadcast_responses_never_land_on_another_device`（電表與驅動器共用連線、另一個主站在第二條連線讀同一站號但筆數不同與站號 3；值完全正確、丟掉的回應各自計數、請求紀錄順序符合）；輔助證據是 `a_siblings_late_answer_is_a_unit_mismatch_not_a_value`、`a_late_answer_of_another_shape_is_not_taken_for_the_next_request`、raw socket 的測試與 `rtu::tests`。mutation 檢查中「不過濾站號」與「站號相同但裝不進請求的框架被當成答案」都讓測試失敗。
- [x] 同一台轉換器下多台設備都能被正確輪詢（測試）：主要證據是 `devices_on_one_converter_share_one_connection_and_take_turns`（站號 1–3、各兩個讀取、同時輪詢兩輪；一條連線、請求紀錄 1、2、3 輪流、只讀）；輔助證據是 `a_silent_device_holds_the_others_up_by_one_timeout_per_request`、`every_function_code_and_type_reads_over_rtu`、`an_exception_over_rtu_fails_only_its_request` 與 `endpoint::tests`。mutation 檢查中「設備不共用連線」讓測試失敗。

- 已記載的限制（RTU 沒有交易 ID，形狀相同的晚到答案會被當成下一個請求的）由 `known_limitation_a_late_answer_of_the_same_shape_is_taken_for_the_next_request` 重現（測試裡只有 agent 一個主站，所以單一主站也避免不了這種情況）；這個測試呈現風險，不是期望的行為。

待測：

- 在 lab 的閘道器上讀真實設備（經 RS485 轉換器）：要等 agent 的執行檔（Laysi/trustiot-v2#86），不屬於本 issue 的驗收條件。
- 額外 CPU 負載下的重複執行：這次沒有做；可用 `docs/test-reports/data/2026-10-20-87-modbus-read/repeat-under-load.ts`。
