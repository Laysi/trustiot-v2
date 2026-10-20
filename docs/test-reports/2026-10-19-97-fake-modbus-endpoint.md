# testkit 的假 Modbus 端點與故障注入（2026-10-19）

Laysi/trustiot-v2#97：`crates/testkit` 新增 `modbus` 模組（`FakeModbus`：測試行程內的假 Modbus TCP 與 RTU-over-TCP 端點，支援站號、故障注入與請求紀錄）與 `clock` 模組（`manual()` 讓暫停的 tokio 時鐘只在測試 `advance` 時前進，`bounded()` 以牆上時間限制等待）。用法與每種故障的定義見 `crates/testkit/README.md`「假 Modbus 端點」。

## 環境

- **程式碼**：分支 `feat/97-testkit-fake-modbus`，一個 commit，基於 main `22343f1`。下方「負載下重複執行」與「其他檢查」是最終的程式碼（含 [Codex 審查後的修正](#codex-審查後的修正drop-與-accept-的競爭) 與 [pre-commit 關卡的修正](#pre-commit-關卡的修正rtu-框架的邊界)）在這個基底上的結果。過程：初版在基於 main `6ba62a4` 時量測，之後 rebase 到 main `a29a473`（Laysi/trustiot-v2#451，只改 CI 腳本與文件，沒有 Rust 程式碼），重跑 `cargo test -p repo-check -p testkit`（全部通過）與 `bun test scripts/ci/changes.test.ts`（255 通過），`scripts/ci/changes.ts` 判定這次的變更會跑 `rust` 與 `cargo-deny`；Codex 審查後的修正在 `a29a473` 上量測；2026-10-20 分支整理成一個 commit 並 rebase 到 main `22343f1`（Laysi/trustiot-v2#460、Laysi/trustiot-v2#461，只改 `docs/` 與 `CLAUDE.md`），再加上 pre-commit 關卡的修正。自動前進的量測在初版與 Codex 審查後的版本上執行，之後的修正沒有改到時鐘。
- **工具版本**：依 `rust-toolchain.toml`（Rust 1.99.0）、`.prototools`（bun 1.4.2，只用來產生 CPU 負載）與 `Cargo.lock`（tokio 1.53.2、tokio-modbus 0.17.0）。
- **負載**：「負載下」是以 [cpu-hog.ts](data/2026-10-19-97-fake-modbus-endpoint/cpu-hog.ts) 開 32 個忙碌迴圈（多於工作站的 CPU 核心數），等 45 秒讓負載平均升上去之後才開始；測試期間的負載平均見各節。
- 不需要容器、叢集或 lab 的硬體：端點與 client 都在測試行程內，只用 127.0.0.1。

## 指令

在 repo 根目錄：

```sh
cargo test -p testkit --test modbus_endpoint --test modbus_example   # 假端點的測試
cargo test -p testkit                                                   # 含 frame 的單元測試與既有的容器測試
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo deny --locked check
cargo run -p openapi -- --check
```

負載下重複執行（測試執行檔路徑取自 `cargo test --no-run --message-format=json`，單元測試執行檔加上篩選 `modbus`）：

```sh
bun docs/test-reports/data/2026-10-19-97-fake-modbus-endpoint/cpu-hog.ts 32 &
sleep 45
for i in $(seq 1 30); do <modbus_endpoint 執行檔> -q; <modbus_example 執行檔> -q; <testkit 單元測試執行檔> -q modbus; done
kill %1
```

自動前進的量測：把 [auto_advance_probe.rs](data/2026-10-19-97-fake-modbus-endpoint/auto_advance_probe.rs) 複製到 `crates/testkit/tests/`，執行 `cargo test -p testkit --test auto_advance_probe -- --nocapture`（量完刪除，它不在測試套件裡）。

## 結果

### 假端點的測試：30 通過、0 失敗

`tests/modbus_endpoint.rs` 20 個、`tests/modbus_example.rs` 1 個，另有 `src/modbus/frame.rs` 的 7 個單元測試（CRC 對照規格的例子、位元打包、例外碼的檢查順序、MBAP 與 RTU 的切框、被 TCP 拆開或相連的 RTU 請求、沒有正確 CRC 的位元組）與 `src/modbus.rs` 的 2 個（見[Codex 審查後的修正](#codex-審查後的修正drop-與-accept-的競爭)）。

| 故障 | 測試 | 兩種傳輸方式 | 驗證的內容 |
| --- | --- | --- | --- |
| 延遲 | `fault_delay_answers_once_the_clock_reaches_the_delay` | 是 | 延遲 2 秒、逾時 3 秒：時鐘推進 2 秒後收到回應；延遲 5 秒：推進 3 秒時 client 逾時，再推進 2 秒才收到晚到的回應 |
| 暫停 | `fault_hold_answers_when_the_test_releases_it`、`release_before_the_hold_lets_the_answer_through` | 是 | `release` 才回答；Modbus TCP 的其他連線照常回答，RTU-over-TCP 的匯流排被擋住；先 release 再到達也會回答 |
| 不回應 | `fault_no_response_leaves_the_connection_healthy` | 是 | 同一條連線上第一個回來的框架是第二個請求的回應；連線仍是 `Open` |
| 站號不符 | `fault_unit_mismatch_answers_with_another_unit` | 是 | 站號 1 的回應帶站號 7（RTU 的 CRC 正確） |
| 例外碼 | `fault_exception_answers_with_the_chosen_code` | 是 | 回 `0x83 0x04`；`.once()` 之後正常 |
| 斷線（FIN） | `fault_disconnect_fin_ends_the_stream` | 是 | client 讀到串流結尾；之後送的請求被記錄、不回答；client 關閉後是 `Closed`；新連線正常 |
| 斷線（RST） | `fault_disconnect_rst_resets_the_connection` | 是 | client 讀到 `ConnectionReset`；只影響指定的連線 |
| 半開連線 | `fault_half_open_keeps_the_socket_silent` | 是 | 連線是 `Forgotten`；client 的寫入成功；新連線取得回應之後，舊連線上仍沒有任何資料、FIN 或 RST（以系統呼叫 `peek` 檢查）；之後送的請求沒有被讀取 |
| 廣播 | `fault_broadcast_sends_the_answer_to_every_connection` | 是 | 另一個 client 先收到站號 1 的回應（Modbus TCP 上帶的是發問 client 的交易 ID），然後才是自己的 |

其他行為：功能碼 1–4 的回應逐位元組比對（`function_codes_1_to_4_are_served_on_both_transports`）、每個站號有自己的表、例外碼 01／02／03、沒有的站號（RTU 不回應、TCP 回 0B）、CRC 錯誤的框架被記錄但不回應也不套用故障、請求紀錄的每個欄位、故障條件（站號、位址範圍含合併讀取、連線、第 n 筆、次數）、不合法的 MBAP 標頭關閉連線、RTU 請求被拆成兩次寫入或與下一個請求在同一次寫入時照樣切對並回答（`rtu_requests_are_answered_however_tcp_splits_or_joins_them`，見[pre-commit 關卡的修正](#pre-commit-關卡的修正rtu-框架的邊界)）、multi-thread runtime 上一邊有 client 連線一邊 drop 端點時每條連線都會結束（`dropping_the_endpoint_closes_every_socket_on_a_multi_thread_runtime`）。

範例測試 `read_the_fake_endpoint_and_trigger_each_fault` 以 `tokio-modbus` 的 RTU client 經 TCP 讀同一台轉換器後面的站號 1 與 2，依序觸發例外碼（`ServerDeviceFailure`）、站號不符（`HeaderMismatch`）、延遲（及時收到）、不回應（逾時，之後同一條連線照常讀到）、廣播（站號 2 的 client 收到站號 1 的回應，`HeaderMismatch`）、RST（`ConnectionReset`）、FIN（`BrokenPipe`）、半開連線（逾時，連線是 `Forgotten`，新連線照常讀到），最後確認請求紀錄裡沒有功能碼 5 以上。

### 負載下重複執行：90 通過、0 失敗

最終的程式碼，負載平均 18.1 到 32.0：`modbus_endpoint`（含 multi-thread 的壓力測試）、`modbus_example` 與 testkit 單元測試中的 `modbus`（9 個）三個執行檔各 30 次。之前的版本也都全部通過：Codex 審查後的版本在負載平均 17.6 到 27.7 下 90 次，初版在 18.0 到 22.2 下 60 次（前兩個執行檔各 30 次）。

### 只用 `start_paused` 的自動前進會誤判逾時

延遲 1 秒、逾時 3 秒的 Modbus TCP 讀取做 2000 次（每次逾時後重新連線）：

| 時鐘 | 未加負載（負載平均 1.5） | 負載下（18.0–22.2） |
| --- | --- | --- |
| 只有 `#[tokio::test(start_paused = true)]` | 501 次逾時 | 501 次逾時 |
| 加上 `testkit::clock::manual()` | 0 次 | 0 次 |

兩種負載下次數相同，所以不是負載造成的競爭：資料還在 socket 裡時 runtime 沒有可執行的工作，tokio 就把時鐘推到下一個計時器（client 的 3 秒逾時），排在端點讀到請求之前。`manual()` 以一個 `spawn_blocking` 工作擋住自動前進（tokio 1.53.2 `time::pause` 文件的「Preventing auto-advance」，原始碼 `runtime/blocking/schedule.rs` 的 `inhibit_auto_advance`），時間只在測試 `advance` 時移動。

### Codex 審查後的修正：drop 與 accept 的競爭

2026-10-19 以 `codex exec review --base origin/main` 審查初版，提出一項（P2）：在 multi-thread runtime 上，`FakeModbus` 被 drop 時，另一條 worker thread 上的 accept 工作可能剛接受一條連線，在 drop 清空工作清單之後才登記它的工作；那些工作不會被 abort，socket 在 drop 之後仍開著。accept 工作從 `accept()` 回來到下一個 `.await` 之間沒有讓出點，drop 的 `abort()` 擋不住這一段，所以這項成立。

修正：`State` 加上 `closed`，drop 在同一把鎖內設定它。之後 `Shared::spawn` 立刻 abort 新的工作，accept 迴圈不再登記連線並結束（socket 隨之關閉），半開連線的 socket 也不再保留。

| 測試 | 有修正 | 拿掉 `spawn` 與 accept 的 `closed` 檢查 |
| --- | --- | --- |
| `modbus::tests::a_task_started_after_the_drop_is_aborted` | 通過 | 失敗：`timed out after 60s waiting for the task started after the drop to be aborted` |
| `modbus::tests::a_connection_accepted_after_the_drop_is_closed` | 通過 | 失敗：`timed out after 60s waiting for the accept loop to stop after the drop` |
| `dropping_the_endpoint_closes_every_socket_on_a_multi_thread_runtime`（200 輪 × 兩種傳輸方式 × 8 個 client，4 條 worker thread） | 10 次全部通過 | 10 次全部通過 |

壓力測試觸發不到這個競爭（時間窗太窄），所以競爭本身由兩個單元測試以決定性的方式驗證：它們把端點放在 drop 之後的狀態，再執行 accept 工作在那之後才會做的步驟。

撰寫壓力測試時的發現：listener 在三向交握途中關閉時，Linux 可能不送 RST 就丟掉 server 端，client 停在 ESTABLISHED（`ss` 只看得到 client 端的 socket），直到它送出資料才被 reset。第一版測試只讀不寫，所以修正前後都會卡住；最終版讓每個 client 在 drop 之後先送一個請求再讀。這也是現場半開連線的一種成因。

### pre-commit 關卡的修正：RTU 框架的邊界

2026-10-20 這個 clone 啟用了 repo 的 Codex pre-commit 關卡（`.githooks/pre-commit`），把分支整理成一個 commit 時，關卡擋下 commit 並提出一項：RTU-over-TCP 對功能碼 1–6、15、16 以外的請求，把「目前收到的全部 bytes」當成一個框架；TCP 不保留框架邊界，請求被拆成兩次讀取時，前一段會被當成 CRC 錯誤的框架而不回答，README 承諾的例外碼 01 不會出現；兩個請求在同一次讀取到達時，後一個會被吞掉。這項成立。功能碼仍會記在請求紀錄裡，所以「證明沒送出功能碼 5 以上」的用法不受影響，受影響的是對其他功能碼斷言例外碼 01 的測試。

修正：依 Modbus application protocol 1.1b3 第 6 節各公開功能碼的請求格式決定長度（7、11、12、17 只有功能碼；8 是子功能碼加兩個 bytes；20、21、23 讀 byte count；22、24 與 43 的 MEI 14 是固定長度）。沒有固定格式的請求（自訂功能碼、Diagnostics 的 Return Query Data、其他 MEI）切在最短的、結尾兩個 bytes 是前面內容 CRC 的位置；一直沒有正確 CRC 的位元組，收滿 RTU 的上限 256 bytes 時切成一個 CRC 錯誤的框架（README「限制」）。

| 測試 | 有修正 | 換回舊的切法 |
| --- | --- | --- |
| `modbus::frame::tests::rtu_requests_are_cut_however_tcp_splits_or_joins_them`（8 種請求，每個拆分位置，後面接一個讀取請求） | 通過 | 失敗：Read Exception Status 與後面的讀取被切成同一個框架 |
| `modbus::frame::tests::rtu_bytes_that_never_end_with_a_valid_crc_are_cut_at_the_rtu_maximum` | 通過 | 失敗：4 bytes 就被切成框架 |
| `rtu_requests_are_answered_however_tcp_splits_or_joins_them`（Diagnostics 分兩次寫入；自訂功能碼與讀取請求在同一次寫入） | 通過 | 失敗：`timed out after 60s waiting for a RtuOverTcp response` |

端點測試的「分兩次寫入」不保證端點分兩次讀到，拆分本身由 frame 的單元測試逐一驗證；同一次寫入的兩個請求在 loopback 上通常一次讀到，舊的切法就把它們併成一個框架。

### 其他檢查

| 指令 | 結果 |
| --- | --- |
| `cargo test --workspace` | 788 通過、0 失敗（初版是 782；Codex 審查後的修正與 pre-commit 關卡的修正各新增 3 個測試）；容器測試真的起了 podman 容器（例如 testkit 的 `postgres_isolation` 26 秒），沒有略過 |
| `cargo test -p testkit` | 62 通過、0 失敗（單元 32、`container_volumes` 2、`modbus_endpoint` 20、`modbus_example` 1、`postgres_isolation` 5、`postgres_start` 2），另有 1 個 doc test 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 沒有警告 |
| `cargo fmt --all --check` | 通過 |
| `cargo deny --locked check`（cargo-deny 0.20.2） | advisories ok、bans ok、licenses ok、sources ok；`Cargo.lock` 只多了 `tokio-modbus`（MIT OR Apache-2.0），它的相依都已在 lock 裡 |
| `cargo run -p openapi -- --check` | `api/openapi.json is up to date` |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 每種故障情境都有自己的測試 | 通過：延遲、不回應、站號不符、斷線（FIN 與 RST 各一）、半開連線、廣播、例外碼各有一個測試，兩種傳輸方式都測（上方第一張表）；另加暫停（`hold`）與故障條件的測試 |
| `crates/testkit/README.md` 說明用法，並附一個以 Modbus client 讀取假端點、逐一觸發每種故障的範例測試 | 通過：README「假 Modbus 端點」說明用法、傳輸方式、每種故障的行為、觀察與時間控制；範例測試是 `tests/modbus_example.rs`（`tokio-modbus` 的 client），README 節錄它 |
| 可控制的時間，讓測試可重現（範圍） | 通過：延遲依 tokio 時鐘，`clock::manual()` 下只在 `advance` 時前進，2000 次 0 次誤判；`Fault::hold` 不需要時鐘；負載下 90 次全部通過 |
