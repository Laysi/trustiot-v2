# 閘道器 agent 的離線緩衝與端對端確認（2026-10-27）

Laysi/trustiot-v2#90：`edge/agent` 新增 `upload`（收集視窗、逐系列組批、串流與序號、一次一批的上傳與 `TelemetryAck` 的處理）、`buffer`（記憶體與磁碟的單一 FIFO、上限與丟棄、磁碟上的 segment 檔與復原）、`clock`（`GatewayClock` 與 `AgeClock`）、`dispatch`（訂閱 `down/telemetry-ack` 並分派）；`main.rs` 接上分派、上傳、磁碟緩衝的復原與 SIGTERM 時寫入磁碟，心跳帶 `buffer` 與 `clock_synced`，unit 加上 `MALLOC_MMAP_THRESHOLD_=65536`。規則見 `edge/agent/README.md`「遙測上傳與離線緩衝」，設計決定見本 issue 2026-10-27 的留言「開始時的設計決定」。2026-10-29 與先合併進 main 的 Laysi/trustiot-v2#94（設定套用與輪詢）整合：執行檔輪詢的讀值經 `upload::Readings` 進入上傳，見「與設定套用（Laysi/trustiot-v2#94）的整合」。

## 環境

- **程式碼**：分支 `feat/90-offline-buffer`。開發與各輪審查基於 main `509c3f1`；第一次準備提交時移到 main `5dad042`（其間 main 加了 Modbus 的連線位址限制等，只有 `edge/agent/README.md` 測試一節的兩處文字需要合併）。之後 Laysi/trustiot-v2#94（PR Laysi/trustiot-v2#722，`9d879e9`）先合併進 main：它把設定套用與每台設備的輪詢接進執行檔，有自己的訂閱與分派，並在 `main.rs` 留下兩個等本 issue 取代的暫代品（`SystemClock`：讀值一律 `clock_synced = false`；`UntilBuffer`：輪詢結果記 debug 日誌後丟掉）。所以本分支改以 main `fdf7bd0` 為基底重新套用並整合（見「與設定套用（Laysi/trustiot-v2#94）的整合」）。之後 main 又加了 TPM 金鑰佈建、`provision` 子命令與憑證換發，本分支最後以 main `371572a` 為基底（見同一節的「移到 main `371572a`」）；「結果」的數字是移過去之後重跑的。
- **工具版本**：依 `rust-toolchain.toml`（Rust 1.99.0）、`.prototools`（bun 1.4.2，只用來執行 mutation 檢查與改回修正的腳本；zig 0.16.0、cargo-zigbuild 0.23.4）與 `Cargo.lock`（tokio 1.53.3）。沒有新增相依：checksum 用既有的 `ring`，亂數用 link 既有的 `fill_random`。
- **機器**：開發與各輪審查在建置主機（x86_64 Linux）上建置與測試，以 podman 跑容器。移到 main `371572a` 之後的結果（「結果」一節）在建置主機的 Linux VM（aarch64）上執行，容器由 VM 裡的 podman 執行。沒有在閘道器上執行（見「待測」）。
- **容器**：`tests/broker.rs` 的 Mosquitto 是 `testkit::mqtt::mosquitto` 釘選的 `eclipse-mosquitto:2.1.2-alpine`（multi-arch digest）；`tests/tpm_swtpm.rs` 的 swtpm 映像沿用。測試結束後容器都已移除。
- 遙測的長時間情境（斷網一小時、30 天的年齡）在行程內以停住的 tokio 時鐘（`start_paused`）跑，緩衝的檔案是 scratch 目錄裡真的檔案；測試的牆上時鐘跟著 tokio 的時鐘走，可以往回或往前調。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
CI=1 cargo test -p agent                       # 設了 CI，容器測試不能略過
cargo test -p protocol
cargo test -p repo-check
cargo test --workspace
proto install zig && proto install cargo-zigbuild
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
```

`cargo test -p agent --test upload` 與 `cargo test -p agent --lib` 在第一輪「獨立攻擊審查」的修正之後各自另外連續跑了三次，結果都一樣（15 個與 198 個通過）。

兩個記憶體的量測（平常略過，要單獨在 release build 跑）：

```sh
cargo test --release -p agent --lib -- --ignored --test-threads=1 --nocapture index_memory
cargo test --release -p agent --lib -- --ignored --test-threads=1 --nocapture distinct_streams
```

最後一次的檢查在這個 worktree 自己的 `target/` 裡建置：共用的 target 目錄同時有別的 worktree 在建置，同名的執行檔可能在建好與執行之間被換掉（這次確實發生過：共用目錄裡的 `repo-check` 跑出 main 的 18 與 10 個測試，這個分支是 17 與 9 個；`ai_gateway` 的 doc test 也曾因此找不到相依的 crate）。

Mutation 檢查：把下表的一處程式改壞，執行 `cargo test -p agent --test upload --no-fail-fast`，記下失敗的測試，再還原（一次只改一處；以腳本比對雜湊確認已還原）。

## 與設定套用（Laysi/trustiot-v2#94）的整合

2026-10-29。Laysi/trustiot-v2#94 合併之後，兩邊都改了 `main.rs`、`lib.rs`、`tests/binary.rs`、`tests/support/wss.rs` 與 `edge/agent/README.md`；其餘的檔案（`buffer`、`upload`、`clock`、`dispatch`、`config`、`heartbeat`、`link`、`setup`、`tests/upload.rs`、`tests/broker.rs` 與文件）直接套得上。整合的做法：

- **一個訂閱清單、一個分派**：`dispatch::subscriptions()` 是 `config` 與 `telemetry-ack`（同一個 SUBSCRIBE），`dispatch::run` 把 `config` 原樣交給設定套用的 `Inbox`、`telemetry-ack` 解碼後交給上傳（解不出來的照舊記警告、每 60 秒最多一筆）。`main.rs` 原本的 `dispatch` 函式與它自己的訂閱拿掉；之後的頻道（log-request Laysi/trustiot-v2#185、live-control Laysi/trustiot-v2#121）照舊加在 `dispatch.rs`。
- **緩衝的目錄跟著 Laysi/trustiot-v2#94 的規則**：狀態目錄由 systemd 的 `STATE_DIRECTORY` 給（`desired::store::state_directory`，預設 `/var/lib/trustiot-agent`），緩衝在它底下的 `buffer/`（`upload::BUFFER_DIRECTORY`）。理由同 Laysi/trustiot-v2#94：unit 的沙箱只讓 `StateDirectory` 可寫，設定檔指到別處就寫不了。所以 `[buffer] directory` 這個鍵拿掉了（`config.rs`、`agent.example.toml`、README，連同它的檢查與測試；寫了這個鍵現在是未知的鍵而拒絕啟動，`buffer_settings_are_bounded` 驗證）。其他 `[buffer]` 的鍵不變。測試改以 `STATE_DIRECTORY`（執行檔）或明確的路徑（library）指定目錄。
- **真的 Sink**：`UntilBuffer` 拿掉，`upload::Readings` 實作 `desired::fleet::Sink`：`polled` 把 `poll.readings` 交給 `Readings::push`（從不等待），失敗的資料點與錯過的輪詢照舊記 debug 日誌（`polled`，留給 Laysi/trustiot-v2#117）；`removed` 是 debug 日誌。
- **同一個時鐘**：`SystemClock` 拿掉，`Fleet` 用上傳的同一個 `GatewayClock` 蓋時間戳，所以讀值的 `clock_synced` 就是上傳從雲端確認學到的狀態。
- **停止**：Laysi/trustiot-v2#90 的保證都留著（SIGTERM 時在 `STOP_LIMIT` 之內、`FLUSH_LIMIT` 之內把記憶體裡的批次寫進磁碟；連線的執行緒或 pipeline 意外結束時走同樣的停止、結束碼 1）。Laysi/trustiot-v2#94 的 `desired::run` 原本只隨 `shutdown` 停下、沒有人等它；現在 `serve` 也等它：它意外結束時同樣走停止、結束碼 1；停止時先停輪詢（等每台設備正在進行的讀取結束，最多 `POLLING_STOP_LIMIT` 2 秒），之後才叫 pipeline 停下關閉最後的視窗，這時結束的輪詢的讀值因此也寫進磁碟。2 秒、`FLUSH_LIMIT` 5 秒與最後一次 fsync 都在 `STOP_LIMIT` 10 秒內（`main.rs` 以編譯期的 assert 檢查）。原本兩者同時停下時，停止期間才完成的輪詢的讀值進了一個沒有人會關閉的視窗，無聲遺失，README 說「正常的重啟與更新不遺失資料」就不成立。

新的測試：

| 測試 | 驗證的內容 |
| --- | --- |
| `desired_config::polled_readings_go_up_as_a_batch_and_the_clouds_acknowledgement_releases_it` | 照 `main.rs` 的接法（`dispatch::subscriptions()` 與 `dispatch::run`、`Readings` 當 `Sink`、同一個 `GatewayClock`；只有連線在記憶體裡、假端點的位址以 `exempt_for_tests` 放行、`Sink` 外面多包一層告訴測試每次輪詢）對 testkit 的假 Modbus 端點：腳本化的雲端送來 retained 設定（兩台設備），回報 `applied`；第一輪的兩個讀值在一個上傳間隔後成為 `up/telemetry` 的序號 1（QoS 1、非 retained，值是兩台設備的暫存器值，`clock_synced = false`）；確認之前緩衝裡有 1 批；雲端的 `TelemetryAck`（`committed`）之後緩衝清空、時鐘算已校正；下一輪的讀值（暫存器改過的值）是同一個串流的序號 2，`clock_synced = true`，序號 1 沒有再送。停住的時鐘（`testkit::clock::manual`） |
| `binary::a_batch_in_the_state_directory_goes_up_and_the_clouds_acknowledgement_releases_it` | 執行檔：先以 library 的 pipeline 在 `STATE_DIRECTORY` 的 `buffer/` 留下一批 3 筆（像上一個行程停止時寫進磁碟的）；執行檔連上後訂閱 `config` 與 `telemetry-ack`，在 `up/telemetry` 原樣送出這一批（序號 1，讀值與時間戳不變）；broker 送 `TelemetryAck` 之後日誌出現「the cloud has the batch」，SIGTERM 照樣 `offline`／`shutdown`、DISCONNECT 0x00、結束碼 0，segment 檔已刪除 |
| `binary::connected_the_agent_applies_its_configuration_reports_it_and_polls_it_after_a_restart`（Laysi/trustiot-v2#94 的測試，加上斷言） | 連上後的一個 SUBSCRIBE 是 `config` 與 `telemetry-ack` 兩個；結束後 `STATE_DIRECTORY` 底下有 0700 的 `buffer/`，日誌沒有開不了緩衝目錄的錯誤；重開後以存下的設定輪詢、日誌的 `polled` 帶 `failed=1`（現在由 `Readings` 的 `Sink` 記） |
| `main.rs` 的 `tests::a_stop_lets_the_polls_in_flight_end_and_writes_their_readings_to_disk` | 一台設備的第一次輪詢的回應被假端點扣住；`serve` 開始停止之後才放行。檢查放行的那一刻 pipeline 還沒被要求停下，而且 `serve` 回傳時這筆讀值在一批裡（`pending_batches` 1、`pending_records` 1）、磁碟上有帶紀錄的 segment。停住的時鐘，所以請求的逾時與 `POLLING_STOP_LIMIT` 都不會在機器很慢時提早結束等待 |
| `main.rs` 的 `tests::an_unexpected_end_of_the_connection_still_writes_the_batches_in_memory_to_disk`（第七次 Codex 審查的測試） | 改用與 `run` 相同的接法（含設定套用），照樣通過 |

讀值由執行檔自己輪詢、送上雲端的端對端測試做不到：執行檔走正式的位址限制（Laysi/trustiot-v2#653），loopback 與執行機器自己的位址都被拒絕，測試機器上的假端點因此連不到（Laysi/trustiot-v2#94 的 `binary` 測試也因此以被拒絕的 127.0.0.1 驗證輪詢）；為測試在執行檔開放例外會削弱正式的防護，沒有做。所以拆成兩半：輪詢的讀值到確認，以與 `main.rs` 相同的接法在行程內驗證；執行檔的上傳、分派與狀態目錄，以留在緩衝裡的一批驗證。

改壞檢查（以腳本改一處、跑對應的測試、改回並以雜湊確認已還原）：

| 改壞的地方 | 結果 |
| --- | --- |
| 停止時 pipeline 與輪詢同時停下（`shutdown` 之後立刻 `pipeline_stop.cancel()`） | `a_stop_lets_the_polls_in_flight_end_and_writes_their_readings_to_disk` 連續 5 次都失敗（「the pipeline was asked to stop while a poll was still in flight」）。只看讀值是否進了批次的第一版斷言是競態（同樣的改壞 10 次只失敗 2 次），所以加上放行時 pipeline 還沒被要求停下的檢查 |
| 分派不把解得開的 `telemetry-ack` 交給上傳 | `a_batch_in_the_state_directory_goes_up_...` 與 `polled_readings_go_up_...` 都失敗（等不到確認的效果） |

整合之後的提交前審查（Codex，`git add -A` 後執行 `sh .githooks/pre-commit`），每一點都先寫回歸測試、在還沒修的程式上確認失敗，修好之後再以腳本改回修正一次（以雜湊確認已還原）：

| 輪次 | 問題 | 修正 | 測試 | 修正前／改回修正時 |
| --- | --- | --- | --- | --- |
| 整合後第一次 | `Collector::form` 把視窗裡和第一筆讀值同一個時鐘狀態的讀值都先放：狀態在一個視窗裡改變又改回（未校正、已校正、又未校正）時，第二段未校正的讀值進了第一批，同一個 series 較晚的讀值比較早的先送出，第一批也混了兩段不相連的未校正期間（這段程式在整合前就有；整合之後執行檔真的有輪詢的讀值進來） | 視窗依到達的順序記下時鐘狀態的段落（`Window::runs`：狀態和前一筆不同就開新的一段，每筆讀值記它的段落），形成批次時一段一段放、段內一個 series 一個 series；相鄰的段落狀態不同，`BatchBuilder` 在每段換批，所以一批只有一段、每個 series 照讀到的順序。同一類檢查：形成批次只有這一處重排讀值；緩衝與上傳都照 FIFO 的順序 | `upload::tests::a_clock_state_that_changes_back_within_a_window_keeps_every_series_in_order`（兩台設備、狀態 未校正→已校正→未校正：三批，序號 1、2、3，狀態 false、true、false，各 series 的時間依序） | 修正前與改回修正時都失敗：只有兩批，第一批是 d1 的 1、5 與 d2 的 2；`upload` 的其他 9 個單元測試照樣通過 |
| 整合後第二次 | 時鐘比對的下限（這一批在這條連線上第一次被 broker 接受的送出）在 `permit.publish` 之前讀系統時鐘，而 `publish` 可能先等一段時間封包才到 broker；`server_time` 因此和較早的本機時間比，等得夠久時，快了的時鐘也落在範圍內而被當成已校正。審查建議改在 link 真的寫出 PUBLISH 時記時間 | 沒有照建議改 link（會改動 Laysi/trustiot-v2#86 的 `Published` 公開介面），而是限制整個來回：佇列的等待只是 `server_time` 與本機時間之間的延遲之一，在慢的上行寫完 256 KiB、broker、雲端的提交與回程也都在範圍裡，比對的誤差是整個來回加 `SYNC_MARGIN`。新的常數 `SYNC_ROUND_TRIP_LIMIT`（10 秒，`clock.rs`）：確認要在那次被接受的送出之後 10 秒內到達才比對，以系統時鐘（`arrived - published` 在 0 到 10 秒之間，與範圍用的同一個時鐘）與單調時鐘（`FirstPublish::sent` 到確認處理時的 `elapsed`，10 秒以內）兩者量；超過的回傳 `Comparison::NotCompared`、狀態不變、記 debug 日誌。所以算成已校正的時鐘偏差最多 12 秒（不計雲端自己時鐘的誤差）。系統時鐘在來回中間跳動時一律不比對：往前跳讓系統時鐘量的來回超過上限；往回跳讓它變成負的，或藏住一段長的來回，這時單調時鐘量的超過上限（只看系統時鐘時，往回跳一小時減 5 秒會讓落後將近一小時的時鐘被當成已校正）。比對的其餘規則不變（範圍、每條連線重新算、被拒絕或沒送出的不記）。`GatewayClock::compare`、`clock.rs` 的模組說明、`Pipeline::run`、`acknowledged`、README「時鐘」照此改寫 | `upload::a_slow_acknowledgement_never_judges_a_clock_20_s_ahead_synchronized`（沒有 timesyncd 的旗標，時鐘快 20 秒，雲端在送出 30 秒後以正確的時間確認：`server_time` 落在 34 秒寬的範圍內，但不比對，之後的批次與確認之後送出的心跳都是未校正；同一條連線上的對照：放上旗標後，立刻回來的確認判定快 20 秒的時鐘為未校正；拿掉旗標、時鐘改正後，立刻回來的確認判定為已校正）；單元測試 `clock::tests::only_an_acknowledgement_within_the_round_trip_limit_is_compared`（兩個時鐘各自剛好在上限時比對、超過 1 ms 就不比對，狀態不變）、`clock::tests::a_wall_clock_step_between_the_publish_and_the_ack_is_never_compared`（往前跳一小時、往回跳 6 秒、往回跳一小時減 5 秒而真正的來回是一小時，三種都不比對）。既有的時鐘測試（`a_clock_an_hour_ahead_is_found_off_when_the_ack_answers_a_publish_on_a_new_connection`、`the_clouds_time_decides_clock_synced_for_batches_and_heartbeats`、`a_refused_publish_does_not_widen_the_window_of_the_clock_comparison`、`every_acknowledgement_outcome_is_handled_as_the_table_says`、`desired_config::polled_readings_go_up_as_a_batch_and_the_clouds_acknowledgement_releases_it`）的確認都在送出後立刻回來，預期都不用改 | 以腳本把來回的檢查改成永遠不成立（`if false && (...)`）：新的整合測試失敗在第一個斷言（30 秒後的確認把時鐘判定為已校正）；還原後以雜湊確認與修正後相同，測試通過 |
| 移到 `371572a` 後第一次 | 這一批的確認可能在它的送出還沒結束時（broker 的 PUBACK 還沒處理完）就被上傳處理：這次被 broker 接受的送出要等送出結束才記下，所以確認的 `server_time` 沒有東西可比，批次卻馬上被拿掉、送出被中止，時鐘的狀態就沒有依這次確認更新（這段程式在移動之前就有） | 送出還在進行時到的、屬於這一批的確認先留著（只留最後一則），送出結束、記下被接受的送出之後再照確認的表處理；別的批次的確認照舊不理會。`Pipeline::run` 的表與 README 的確認表各加一列 | `upload::an_acknowledgement_before_the_puback_waits_for_the_send_and_is_compared`（時鐘快一小時、timesyncd 的旗標在；雲端以正確的時間確認，確認比 PUBACK 先到：PUBACK 之前批次還在，PUBACK 之後批次拿掉、時鐘判定為未校正，之後 30 秒沒有重送） | 以腳本把留住確認的條件改成永遠不成立（`if false && ...`）：新測試失敗（「the batch is kept while its send waits for the PUBACK」，`pending_batches` 是 0），其餘 515 個通過；還原後以雜湊確認與修正後相同 |
| 移到 `371572a` 後第二次 | `serve` 在 READY=1 的通知失敗時提早返回，pipeline 不寫磁碟（與「整合後第三次」的說法相同） | 誤報，沒有改行為：那裡只記警告，照常往下走到完整的停止；`serve` 唯一的提早返回是訊號處理建立失敗，在 pipeline 啟動之前。在該處加了註解說明 | — | — |
| 移到 `371572a` 後第三次 | `disk_limit` 為 0 而緩衝目錄在啟動時不存在時，之後不再檢查；目錄之後才帶著舊批次出現時，那些批次不會被讀出、送出 | 誤報，沒有改行為：上限大於 0 的執行在打開磁碟時就建立目錄（`disk::prepare_dir`），在寫任何一批之前；unit 的沙箱只讓 agent 寫它的狀態目錄，上限 0 的執行也不建立目錄，所以啟動時不存在的目錄，在這次執行中不會出現舊批次。在 `Buffer::open_disk` 加了註解說明 | — | — |
| 移到 `371572a` 後第四次 | `AgeClock::raise_floor` 在抬高下限前後各讀一次時鐘，以兩次的差作為移動量：兩次讀取之間牆上時鐘若往前跳（NTP 校正），跳的量也被加到之前形成的批次的形成時間，這些批次看起來比實際年輕，可能過了 30 天還不丟（這段程式在移動之前就有） | 兩個時鐘只讀一次（`AgeClock::clocks`），抬高前後的時間都從這一次讀取算，結果只有抬高的量；之後的下限更新與 `now()` 相同 | `clock::tests::a_wall_clock_step_during_a_floor_raise_is_not_counted_as_the_raise`（牆上時鐘在 `raise_floor` 第一次讀取之後往前跳一小時，把下限抬到時鐘之後 10 分鐘：回傳 10 分鐘） | 改回修正時失敗：回傳 3,600,000 而不是 600,000，其餘 516 個照樣通過；還原後以雜湊確認與修正後相同 |

**移到 main `371572a`**（2026-10-29）：其間 main 加了 TPM 金鑰的佈建與背書金鑰（Laysi/trustiot-v2#728、Laysi/trustiot-v2#739）、`provision` 子命令（Laysi/trustiot-v2#743）與憑證換發（Laysi/trustiot-v2#745）。本分支的變更套到 `371572a` 上，要合併的是兩邊都改的地方：設定檔同時有 `[renewal]` 與 `[buffer]`；心跳的 `cert_not_after` 保留 main 的 `watch::Receiver`（換發後是新憑證的），另外帶本分支的 `buffer` 與 `clock_synced`；`main.rs` 照本分支的結構，換發照 main 的做法在建立 link 之後啟動，main 在 `main.rs` 的 `dispatch` 函式由 `dispatch.rs` 取代；`tests/link.rs`、`tests/renewal.rs` 與 `tests/upload.rs` 的心跳設定，以及 README 的模組表（拿掉「還沒有：憑證換發」）、設定表與測試清單。換發後的連線輪替對上傳來說與一般的重連相同：輪替一開始 link 就不再收新的 publish（`reserve` 回 `NotConnected`），在路上的那一批等新的 session 連上後再送一次，時鐘的比對在新的連線上重新算；已被 broker 接受的那一批，雲端的確認在舊連線結束前到了就照常拿掉，沒到就重送，雲端回 `duplicate`。

## 結果

最終的 commit（以 main `371572a` 為基底，含「與設定套用（Laysi/trustiot-v2#94）的整合」一節列出的所有提交前審查的修正，2026-10-29）在建置主機的 Linux VM（aarch64）上執行，設了 `CI=1`，所以要容器的測試（Mosquitto、swtpm、Postgres、ClickHouse 等）都有跑，不會略過：

- `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`：通過。
- `cargo test --locked --workspace --no-fail-fast`：2,356 通過、0 失敗、3 忽略。忽略的是 `agent` 兩個記憶體的量測（要在 release build 單獨跑，結果見「獨立攻擊審查」第一輪第 3 點與第二輪 B），以及 `services/assistant` 原本就有的一個 doc test。`cargo test -p repo-check` 與 `cargo test -p protocol` 都在其中。
- 其中 `agent`：517 通過、0 失敗、2 忽略。
  - 各檔：單元測試 330、執行檔的單元測試 3、`binary` 12、`broker` 5、`dependencies` 1、`desired_config` 8、`link` 56、`liveness` 4、`modbus_address_guard` 1、`modbus_fake_endpoint` 9、`modbus_rtu_converter` 8、`modbus_rtu_framing` 6、`modbus_tcp_framing` 8、`polling_recovery` 3、`provision` 3、`renewal` 5、`systemd_unit` 6、`tpm_endorsement` 5、`tpm_provision` 6、`tpm_swtpm` 1、`transport` 19、`upload` 18。
  - main `371572a` 本身在同一台 VM 上是 420 通過、0 失敗、0 忽略。本分支多 97 個，其中 2 個來自移動之後兩次提交前審查的修正（`upload` 1、單元測試 1）。
  - 修正之前的數字：最初 315（`upload` 10），Codex 第一次的修正之後 320，第一輪「獨立攻擊審查」的修正之後 342，第三次 Codex 審查的修正之後 344，第四次 Codex 審查與第二輪「獨立攻擊審查」的修正之後 354，Codex 第六到第八次的修正之後 364（還在 `509c3f1` 上）。移到 `5dad042` 之後多的 13 個是 main 新加的 Modbus 連線位址限制的測試，第十次與第十二次的修正各多 1 個（`upload` 與單元測試），在 `5dad042` 上是 379。整合之後多的 18 個：Laysi/trustiot-v2#94 的測試 14 個（`desired_config` 7、單元測試 7）、整合新加的 3 個（`main.rs` 的單元測試、`binary`、`desired_config` 各 1），與整合後第一次提交前審查的 1 個（單元測試），在 `fdf7bd0` 上是 397；整合後第二次提交前審查（來回時間的上限）多 3 個（單元測試 2、`upload` 1），是 400。
- `cargo deny --locked check`：advisories、bans、licenses、sources 都通過。本分支沒有改相依（`Cargo.toml` 與 `Cargo.lock` 與 main `371572a` 相同）。
- aarch64：VM 本身就是 aarch64 Linux，上面的建置與測試就是 aarch64 的（glibc，ring 的 C 程式以 VM 的 gcc 編譯）。release 用的 zig 交叉建置以 CI 的 aarch64 job 為準；在 `fdf7bd0` 上（x86_64 Linux）以 `cargo zigbuild --locked --release` 交叉建置通過，產出 `ELF 64-bit LSB pie executable, ARM aarch64`。
- 測試結束後沒有留下容器或 volume。

### 新的測試

| 項目 | 測試 | 驗證的內容 |
| --- | --- | --- |
| 斷網一小時 | `upload::an_hour_offline_then_every_reading_arrives_once_in_order` | 3 台設備各 2 個資料點、每秒一次，記憶體上限 8 KiB：線上 1 分鐘、斷網 1 小時、再線上 2 分鐘。斷網時積壓 350 批以上、大多在磁碟（有 segment 檔）、沒有丟棄；恢復後雲端收到的每一筆剛好一次，時間戳與值不變，同一串流的序號依序，沒有 `duplicate`；最後緩衝的筆數、批數、磁碟 bytes 與丟棄數都是 0，segment 檔都刪掉 |
| 強制結束後重送 | `upload::after_a_forced_end_disk_batches_go_again_and_new_ones_use_a_new_stream` | 記憶體上限 2 KiB、斷網後形成的批次一部分在記憶體、其餘在磁碟，然後不 flush 就結束（abort 所有工作）。新的行程從同一個目錄讀回磁碟的批次，以原本的串流與連續的序號送出，序號從記憶體遺失的那幾批之後開始；遺失的只有斷網後最舊的那一段、不超過記憶體上限；新的讀值用新的串流，雲端（每個串流一個高水位）當成新的，沒有任何一批被當成 `duplicate` |
| 上限：記憶體滿、磁碟不能用 | `upload::memory_full_with_a_failing_disk_drops_the_oldest_and_keeps_going` | 緩衝目錄建不起來（上一層不存在）：記錄「cannot open the buffer directory」；記憶體滿時丟最舊的，`reason=memory_full` 的警告；每一筆不是在等就是計入 `dropped_records`；恢復連線後留下的與之後的新資料都送到，丟掉的都比留下的舊 |
| 上限：磁碟滿 | `upload::a_full_disk_drops_the_oldest_and_keeps_going` | 記憶體 4 KiB、磁碟 64 KiB、離線 400 秒約 200 KB：`reason=disk_full`；磁碟 bytes 不超過上限，等於 segment 檔大小的總和；恢復後送完、檔案刪光，丟掉的都比留下的舊 |
| 新的串流 ID | `upload::a_restart_and_a_fresh_state_directory_each_get_a_new_stream` | 同一個狀態目錄重啟一次、再換一個全新的狀態目錄：三次的串流 ID 都不同，各自從序號 1 開始，雲端沒有 `duplicate`；緩衝目錄建在狀態目錄底下 |
| 時鐘往回調整與年齡 | `upload::a_backward_clock_step_does_not_keep_a_batch_past_thirty_days` | 批次寫進磁碟後停止，牆上時鐘往回調 10 天再啟動：形成後 30 天少 1 分鐘時還在；30 天又 1 分鐘之後的下一個收集窗（停住的時鐘 30 秒內）就丟掉、計入 `dropped_records`，磁碟清空。只看牆上時鐘的話要再等 10 天 |
| 校正狀態的改變 | `upload::a_change_of_the_sync_state_splits_the_batch` | timesyncd 的檔案在第 5 秒出現：同一個收集窗分成兩批，序號 1 是 `clock_synced = false`（5 秒的讀值）、序號 2 是 `true`（4 秒），時間戳與值不變 |
| 雲端時間的比對 | `upload::the_clouds_time_decides_clock_synced_for_batches_and_heartbeats` | 雲端時鐘快一小時的確認讓時鐘變成未校正：之後的批次與心跳都是 `clock_synced = false`；時間對的確認讓兩者回到 `true`；所有讀值的時間戳都沒有改 |
| 確認的每一種結果 | `upload::every_acknowledgement_outcome_is_handled_as_the_table_says` | 同一條連線上依序：`committed` 之後送下一批；別的序號、別的串流的確認不理會，沒有確認就在 60 秒（`ACK_TIMEOUT`）時原樣重送；`duplicate` 釋放；`retry` 5 秒後原樣重送；不認得的狀態留到期限再送；`rejected` 釋放且不算丟棄；PUBACK 0x87 在 1 秒、再 2 秒後重送；PUBACK 之後、確認之前斷線，以及 PUBACK 之前斷線，下一條連線第一個送的都是同一批同樣的 bytes；最後緩衝為空、丟棄 0 |
| 真的 broker | `broker::a_batch_goes_up_through_mosquitto_and_the_clouds_acknowledgement_releases_it` | agent 的 link 經 WebSocket 內的 mTLS 連 Mosquitto，訂閱 `down/telemetry-ack`（閘道器角色允許）；雲端端的訂閱在 `up/telemetry` 收到 QoS 1、非 retained 的批次（序號 1、3 筆、時間戳原樣）；確認送來之前批次一直在緩衝；雲端以 `mosquitto_pub` 在 `down/telemetry-ack` 送 `committed` 後，批次離開緩衝，時鐘算已校正 |
| 壞掉的紀錄 | `upload::a_corrupt_or_torn_record_is_never_sent_and_unknown_files_stay` | 磁碟上 3 批，把第 2 批 payload 的一個 byte 改掉、在 segment 結尾加上只寫一半的紀錄、目錄裡放一個 `notes.txt`：重啟後只送出序號 1 與 3，序號 2 從沒送出、它的 20 筆計入 `dropped_records`；`notes.txt` 原封不動；segment 送完後刪掉 |
| 執行檔 | `binary::connected_the_agent_applies_its_configuration_reports_it_and_polls_it_after_a_restart`（整合之前是本 issue 改名的 `..._subscribes_to_telemetry_acks_...`，整合時併進 Laysi/trustiot-v2#94 的這個測試）、`binary::a_batch_in_the_state_directory_goes_up_and_the_clouds_acknowledgement_releases_it` | 連上後送一個 SUBSCRIBE（`down/config` 與 `down/telemetry-ack`），SIGTERM 照樣 `offline`／`shutdown`、DISCONNECT 0x00、結束碼 0；`STATE_DIRECTORY` 底下建了 0700 的 `buffer/`，日誌沒有開不了目錄的錯誤；留在緩衝裡的一批送出、由確認拿掉（見「與設定套用（Laysi/trustiot-v2#94）的整合」） |
| systemd unit | `systemd_unit::the_sandbox_lines_stay` | `Environment=MALLOC_MMAP_THRESHOLD_=65536`；`systemd-analyze verify` 與評分照樣通過 |
| 單元測試 | `buffer::tests::*`（31，含兩個平常略過的量測）、`buffer::disk::tests::*`（24）、`clock::tests::*`（9）、`upload::tests::*`（10），數目是最終的 commit 的；`config::tests::buffer_settings_are_bounded` 等、`heartbeat::tests::the_buffer_and_the_clock_state_come_from_their_sources` | 記憶體到磁碟的 FIFO 順序、磁碟滿與失敗時丟哪一批、磁碟失敗後 60 秒再試、30 天過期；檔頭 checksum、只寫一半的紀錄、壞掉的 payload 從不讀出、壞掉的檔頭只略過該 segment 的其餘部分、不認得的檔案不動、`.tmp` 刪掉、symbolic link 與一般檔案不當成目錄、寫入失敗後換新 segment、重開後依串流與序號排序；時鐘的兩個階段與年齡時鐘的下限；收集窗依 series 分組、依時鐘狀態分批、序號用完換串流、收集窗的上限、確認佇列只留最新的 8 則、`retry` 與退避的範圍；設定的範圍與預設值；心跳的 `buffer` 與 `clock_synced` 取自緩衝與時鐘 |

### Mutation 檢查：4 處改壞，每一處都有測試失敗

| 改壞的地方 | 失敗的測試 |
| --- | --- |
| 接受任何一批的確認（不比對串流與序號） | `every_acknowledgement_outcome_is_handled_as_the_table_says` |
| 讀磁碟時不檢查 payload 的 checksum | `a_corrupt_or_torn_record_is_never_sent_and_unknown_files_stay` |
| 年齡只看牆上時鐘 | `a_backward_clock_step_does_not_keep_a_batch_past_thirty_days` |
| 每次啟動用同一個串流 ID | `a_restart_and_a_fresh_state_directory_each_get_a_new_stream`、`after_a_forced_end_disk_batches_go_again_and_new_ones_use_a_new_stream` |

年齡那一項第一次沒有被抓到：最後的等待只以牆上時間（60 秒）為上限，停住的時鐘每次推進 1 秒，迴圈跑過了 10 天的時間。改成以停住的時鐘為上限（下一個收集窗，30 秒內）之後才抓到；修正後的測試就是上表的版本。

### 提交前審查（Codex）

提交前的 Codex 審查（2026-10-28）提出兩點，都在程式裡確認屬實。每一點先寫回歸測試，在還沒修的程式上跑、確認失敗，再修；修好之後再把修正逐一改回去跑一次（以腳本比對雜湊確認已還原），每一次都只有對應的測試失敗。

| 問題 | 修正 | 測試 | 修正前／改回修正時 |
| --- | --- | --- | --- |
| 1. `Buffer::open_disk` 在 `disk_limit == 0` 時不開目錄就返回，之前（上限較大時）留在磁碟上的批次永遠不送、也不計數 | 不論上限多少都開目錄、讀回全部批次並照順序送出；上限只決定新的一批能不能寫進磁碟。上限 0 而目錄不存在時不建立、也不算錯誤。上限調小之後超過上限的批次先留著，等記憶體滿、新的一批要用磁碟時，才照「磁碟滿」的規則從最舊的丟（計數，`disk_full`），直到回到上限以下。README、`agent.example.toml` 與 `buffer` 模組的說明照此改寫 | `buffer::tests::batches_an_earlier_process_left_are_sent_with_the_disk_switched_off`（回歸測試）；`with_the_disk_switched_off_a_missing_directory_is_neither_created_nor_an_error`、`batches_above_a_lowered_disk_limit_are_kept_until_the_disk_needs_room`（記下選定的行為） | 失敗：`open_disk` 回傳 `None`，預期 `Some(100)` |
| 2. `Buffer::flush` 最後的 `sync()` 失敗時，這次寫進去的批次仍記成在磁碟上，也沒有回報 `NotFlushed`，停止之後斷電就無聲遺失 | 最後的 fsync 失敗時，這次以 `sync = false` 寫進去的批次都算不持久：移出緩衝、計入 `NotFlushed`（批數、筆數與錯誤）。紀錄留在檔案裡、不釋放，下一個行程讀得到完整的就照樣送出。`NotFlushed` 帶錯誤原因、不受日誌節流 | `buffer::tests::a_failed_sync_at_the_stop_reports_its_batches_as_lost`（以 `DiskStore` 只在測試編譯的 `fail_sync` 注入失敗） | 失敗：沒有 `NotFlushed`，只有一筆 `DiskFailed` |
| 2 的同類問題：只有 `flush` 以 `sync = false` 寫入，而 `DiskStore::sync` 只 fsync 目前的 segment；停止時寫滿一個 segment 換到下一個，前一個的檔案在 `seal_writer` 關掉，那些批次從沒 fsync，即使最後的 sync 成功也一樣 | 換 segment 時，還有沒 fsync 的紀錄的檔案留著，`sync()` 對每一個都 fsync（全部都試，回傳第一個錯誤）；以 `sync = true` 寫入時的 fsync 本來就涵蓋同一個檔案先前的寫入 | `buffer::tests::a_stop_that_spans_segments_syncs_every_segment_it_wrote`（`DiskStore` 只在測試時記下 sync 過的 segment） | 失敗：sync 過的 segment 是 `[2]`，預期 `[0, 1, 2]` |

修好之後：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` 通過；`CI=1 cargo test -p agent` 320 通過、0 失敗、0 略過（單元測試 181，多了上表 5 個）；`cargo test -p repo-check` 通過。

第二次 Codex 審查（2026-10-28）又提出一點，同樣在程式裡確認屬實：

| 問題 | 修正 | 測試 | 修正前 |
| --- | --- | --- | --- |
| 3. `Pipeline::run` 開了磁碟就開始送，第一次 `expire` 要等第一個收集窗關閉（一個上傳間隔之後）；連線已經在時，從磁碟讀回、早已超過 30 天的批次會先送出。在路上的批次等確認或 `retry` 期間跨過 30 天，期限一到也照樣重送 | 分層處理：（1）開磁碟、以磁碟上最新的形成時間抬高年齡時鐘的下限之後，立刻 `expire`，才進入送出的迴圈；之後由 `tick` 重試才開成的磁碟也一樣（`close_window` 抬高下限後立刻 `expire`）。（2）開始送最舊的一批之前檢查它的年齡，過期就先 `expire` 再取下一批。（3）重送（確認逾時、`retry` 或退避的期限到了、換了連線）之前檢查年齡，過期就不送，交回（2）丟掉。（4）在 link 佇列等到位置、讀出 payload 之後、真的送出之前再檢查一次（等位置可能很久）。過期的一律不送，計入 `dropped_records`（`expired`）。`Pipeline::run` 的對應表與 README 加上這一列 | `upload::an_expired_backlog_found_at_start_is_never_sent`（磁碟上 3 批共 60 筆，關機 31 天後啟動、一啟動就連線：雲端一批都沒收到，60 筆在停住的時鐘 5 秒內計入 `dropped_records`，之後的新讀值照常送出）；`upload::a_batch_in_flight_that_crosses_thirty_days_is_not_sent_again`（等確認時跨過 30 天，確認逾時後不重送；`retry` 等待期間跨過 30 天，期限到了也不重送，兩次都計入）；`upload::an_expired_backlog_found_when_the_disk_opens_late_is_never_sent`（狀態目錄啟動時不在、之後才出現，由 `tick` 開成的磁碟上過期的批次同樣不送） | 前兩個測試失敗：在路上的那一批 30 天後仍然重送（「expected nothing for 120s, got Some(Publish(... up/telemetry ...))」）；啟動時過期的積壓沒有在 5 秒內丟掉（「not within 5s of the paused clock」） |

這一點的修正分好幾層，改回其中一層時其他層可能接手，所以分組改回，只跑這三個測試（`cargo test -p agent --test upload` 的這三個，每次限 240 秒的牆上時間）：

| 改回的部分 | 結果 |
| --- | --- |
| 不改（修正後） | 3 個都通過 |
| A：啟動開磁碟後不 `expire`，開始送之前不檢查年齡 | `an_expired_backlog_found_at_start_is_never_sent` 失敗；`a_batch_in_flight_that_crosses_thirty_days_is_not_sent_again` 停不下來（送出前的檢查拒送、沒有人把過期的那批丟掉，上傳在同一批上空轉，停住的時鐘不再前進），240 秒後強制結束 |
| B：重送前與送出前都不檢查年齡 | `a_batch_in_flight_that_crosses_thirty_days_is_not_sent_again` 失敗 |
| C：晚開成的磁碟不 `expire`，開始送之前不檢查年齡 | `an_expired_backlog_found_when_the_disk_opens_late_is_never_sent` 失敗；`a_batch_in_flight_...` 同 A 停不下來 |
| 只拿掉重送前的檢查 | 3 個都通過：送出前的檢查接手（預期如此） |
| 只拿掉開始送之前的檢查 | 兩個積壓的測試通過（開磁碟後的 `expire` 接手）；`a_batch_in_flight_...` 同 A 停不下來，240 秒後強制結束 |

每一次都以腳本比對雜湊確認已還原。A 與 C 的空轉只在拿掉第（2）層時出現：完整的修正裡，送出前的檢查拒送之後一定由（2）丟掉，之後年齡時鐘不會倒退（見下方「獨立攻擊審查」第 2 點），所以不會空轉。

第三次 Codex 審查（2026-10-28，在下方「獨立攻擊審查」的修正之後）又提出一點，同樣屬實：

| 問題 | 修正 | 測試 | 改回修正時 |
| --- | --- | --- | --- |
| 4. `DiskStore::create_segment` 先以 `symlink_metadata` 確認 `.log` 的名稱沒有東西，再 `rename`：兩者之間別的行程建立的檔案會被 `rename` 取代（「獨立攻擊審查」第 4 點的修正留下的空檔） | 以 `std::fs::hard_link(.tmp, .log)` 發布 segment：`link(2)` 在名稱已經存在時不可分割地以 `AlreadyExists` 失敗，從不取代既有的項目；成功後刪掉 `.tmp` 的名稱、fsync 目錄。名稱被佔（`.tmp` 或 `.log`）時只刪這次自己以 `O_EXCL` 建的 `.tmp`，既有的項目不動，跳過這個編號、同一次寫入改用下一個（連續最多 16 個，之後當成磁碟失敗）。在 link 與刪掉 `.tmp` 之間當掉時，留下的 `.tmp` 是 `.log` 的第二個名稱，啟動時刪掉它只拿掉這個名稱。檔案系統不支援 hard link 時（例如 `EPERM`、`Unsupported`）當成磁碟失敗、走既有的重試，不改用 `rename`。資料分割區是 ext4。沒有新的相依，也沒有 `unsafe` | `buffer::disk::tests::a_name_taken_right_before_publishing_is_left_alone`（只在測試編譯的 `taken_before_publishing` 在發布前一刻建立 `.log`：那個檔案原封不動，紀錄進了下一個編號的 segment，沒有留下 `.tmp`，下一個行程不動那個檔案、找得到紀錄）；`a_temporary_name_left_by_a_crash_after_publishing_is_removed_alone`（以 hard link 做出當掉後的狀態：啟動時刪掉 `.tmp`，`.log` 的 link 數回到 1，3 筆紀錄都在、payload 讀得回來，新的 segment 用下一個編號）；`a_name_that_appears_after_start_is_never_replaced_or_removed` 改成跳過編號之後同一次寫入就成功 | 改回「先檢查再 `rename`」（名稱在檢查與 `rename` 之間被佔）：`a_name_taken_right_before_publishing_is_left_alone` 失敗；不改的基準 3 個都通過，檔案以雜湊確認已還原 |

第四次 Codex 審查（2026-10-28）提出年齡時鐘的並行問題（`AgeClock::now` 在更新共用的下限之前就算好回傳值），與「獨立攻擊審查」第二輪一起修正，見該節的表。

從第五次起，每一輪都由這個 worktree 自己執行提交前的檢查（`git add -A` 後執行 `sh .githooks/pre-commit`，審查的就是暫存區的差異，不會 commit），直到它放行：

| 輪次 | 問題 | 修正 | 測試 | 改回修正時 |
| --- | --- | --- | --- | --- |
| 第五次 | 檔頭通過檢查、payload 卻被截斷的紀錄（寫到一半時行程結束），啟動時只計入 `torn_records` 就停下，它的形成時間沒有進入年齡時鐘的下限；`Buffer::open_disk` 於是從比較舊的批次抬高下限。系統時鐘往前跳、在重啟前又改回時，較舊的未確認批次可能多留超過 30 天，在雲端的去重紀錄過期之後再送一次 | 新的 `Recovery::newest`：啟動時每一筆檔頭通過 checksum 與欄位檢查的紀錄都把形成時間算進去，不論之後被保留、因超過上限而釋放、payload 被截斷，或在 `Buffer` 裡因重複而去掉；`Buffer::open_disk` 以它作為下限（原本只看去重之後保留的批次）。依審查的要求一併檢查了其他路徑：重新對齊時略過的 bytes 裡沒有通過檢查的檔頭（找到的第一個通過檢查的檔頭就從那裡繼續讀）；超過上限而丟掉的都比保留的舊，原本就不影響最大值，現在也明確算進去；停止時的 `open_for_appends` 本來就把截斷紀錄的檔頭算進最新時間，它只讀編號最大的 segment，少算只會讓這個行程的批次在下次啟動時看起來比較舊（提早過期），不會多留；送出時才發現 payload 壞掉而丟掉的批次，啟動時它的形成時間已經算進下限，丟掉之後它的檔頭在 segment 刪掉之前也還在。檔頭的 checksum 對、但欄位是這個格式不會寫的值（payload 長度、序號、串流 ID）的紀錄不算：那不是我們寫的 | `buffer::tests::a_torn_record_still_raises_the_floor_to_its_formation_time`（第 2 筆比第 1 筆晚 40 天形成、payload 被截斷：下限是第 2 筆的時間，第 1 筆 40 天大而過期）；`a_batch_found_twice_raises_the_floor_to_its_newest_copy`；`buffer::disk::tests::the_stop_open_counts_a_torn_last_record_for_the_newest_time` | 只讓保留的紀錄進入最新時間（原本的做法）：截斷的兩個測試失敗。下限只看去重後保留的批次（原本的做法）：截斷與重複的兩個測試都失敗。檔案以雜湊確認已還原 |
| 第六次 | 兩個問題。(1) 年齡時鐘記住的往前跳只在記憶體裡：批次寫進磁碟之後，系統時鐘往前跳、在重啟前又改回，之後又沒有形成新的批次時，重啟後的下限只來自系統時鐘與磁碟上的形成時間，往前跳的那段被忘掉，批次可能多留超過 30 天。(2) 作業系統給不出亂數時，串流 ID 退回時鐘的奈秒與行程 ID；時鐘沒設定或停住、行程 ID 又重複時，重啟後可能重用之前的串流與序號，雲端把新的資料當成重複 | (1) 緩衝目錄多一個 `clock` 檔（32 bytes：magic、版本、保留的 0、年齡時鐘的毫秒、checksum）。磁碟上有 segment 時，例行整理每 60 秒（`CLOCK_SAVE_EVERY`）存一次，正常停止寫完批次後也存；寫法是以 `O_EXCL` 建 `clock.tmp`、fsync、`rename` 蓋過 `clock`、fsync 目錄，當在任何一步都留下完整的舊檔或新檔。兩種開啟（`open_until`、停止時的 `open_for_appends`）都讀它，下限取它與磁碟上最新的形成時間中較大的；不是一般檔案、格式或 checksum 不對時不用，記一筆警告，下次存檔時取代。存檔失敗時記一筆警告（節流），60 秒後再試。當掉時遺失年齡時鐘在最後一次存檔之後算進去的時間（見第十二次），而且只在重啟時的系統時鐘落後時。(2) 建立 pipeline 時取一次 16 bytes 的亂數（`StreamIds`）：第一個串流是它的 32 個十六進位字元，序號用完之後的串流加上 `-1`、`-2`……；取不到亂數時 `upload::pipeline` 回 `NoRandomness`，執行檔記一筆錯誤、以結束碼 1 結束，不再退回時鐘與行程 ID（沒有亂數時 TLS 本來也無法運作）。連線的 session ID（`link.rs`，main 上原有的程式）在沒有亂數時同樣退回時鐘，這次沒有改：執行檔在沒有亂數時已經不會連線 | `buffer::tests::a_restart_keeps_a_forward_step_the_age_clock_counted_while_batches_were_on_disk`（批次寫進磁碟，時鐘往前 20 天又改回，不停止就丟掉緩衝；重啟時系統時鐘只比批次晚 90 秒：這一批是 20 天大，再 10 天就過期）；`a_stop_saves_the_age_clocks_time_with_the_batches_it_writes`；`housekeeping_saves_the_age_clocks_time_every_minute_while_batches_are_on_disk`（磁碟上沒有批次時不存，之後最多每 60 秒存一次）；`buffer::disk::tests::the_clock_file_keeps_the_age_clocks_time_and_a_damaged_one_is_ignored`（兩種開啟都讀到、mode 0600、改一個 bit 就不用、留下的 `clock.tmp` 不列為 ignored 且下次存檔時清掉、symbolic link 不跟隨也不改到它指向的檔案）；`upload::tests::without_randomness_the_pipeline_is_not_created`；`stream_ids_are_valid_random_and_distinct` 改為也檢查之後的串流 | 開啟時不讀 `clock`：重啟與停止的兩個測試失敗。例行整理不存：重啟與「每 60 秒」的兩個測試失敗。停止時不存：停止的測試失敗。停止時的開啟不讀 `clock`：`clock` 檔的測試失敗。沒有亂數時退回時鐘與行程 ID（原本的做法）：`without_randomness_the_pipeline_is_not_created` 失敗。檔案以雜湊確認已還原 |
| 第七次 | 連線的執行緒意外結束時，`serve` 取消 pipeline 之後立刻回傳，不等它把記憶體裡的批次寫進磁碟；接著 runtime 的收尾（`shutdown_timeout`，1 秒）把 pipeline 的工作丟掉，記憶體裡的批次就此遺失 | 不論是收到訊號、連線的執行緒或 pipeline 意外結束，都走同一段停止：取消兩者，在 `STOP_LIMIT`（10 秒）之內等還在跑的那一個（連線送出 `offline`／`shutdown` 並斷線，pipeline 把記憶體寫進磁碟），之後才回傳；已經結束的那一個不再等。意外結束時結束碼仍是 1。`main.rs` 的模組說明與 README 的「正常停止」補上這一句 | `main.rs` 的 `tests::an_unexpected_end_of_the_connection_still_writes_the_batches_in_memory_to_disk`：link 永遠連不上，視窗裡有一筆讀值，連線的執行緒一開始就結束；`serve` 回傳時結束碼是 1，而且緩衝目錄裡已經有帶紀錄的 segment（連續跑 5 次都通過） | 改回「意外結束就立刻回傳」（原本的做法）：測試失敗（回傳時磁碟上沒有批次）。檔案以雜湊確認已還原 |
| 第八次 | 批數到上限（`MAX_BATCHES`）時，`Buffer::push` 先丟最舊的一批騰出位置，之後才看新的一批放不放得下；新的一批哪裡都放不下（比記憶體上限大、磁碟又不收）時它也被丟掉，兩批都遺失 | 批數上限的騰位置移到 `insert`：新的一批已經放進記憶體或寫進磁碟之後才丟最舊的，放不下的那一批只丟它自己。依同一個原則檢查了其他路徑：記憶體與磁碟的騰位置本來就只在新的一批放得下那一邊時才做（`can_fit`）；磁碟騰出位置之後寫入才失敗的情況（`push` 與停止時的 `flush` 都有）在寫之前無從得知，沒有改，列在「結論」的待測與風險裡。README 的上限表跟著改 | `buffer::tests::at_the_batch_limit_a_batch_that_cannot_be_stored_drops_no_other`（批數上限 2、記憶體 1,000 bytes：沒有磁碟時，以及磁碟的 segment 建不起來時，第 3 批 2,000 bytes 被丟掉，第 1、2 批都還在，沒有 `too_many_batches`）；`the_batch_limit_drops_the_oldest` 與 `start_up_keeps_at_most_the_batch_limit_of_the_newest_batches` 照樣通過 | 改回「先丟最舊的再放」（原本的做法）：新的測試失敗，另外兩個照樣通過。檔案以雜湊確認已還原 |
| 第九次 | 審查認為停止時的 `open_for_appends` 以 `entry.metadata()` 計算 segment 的大小會跟隨 symbolic link：指向大檔案的 `seg-<n>.log` 連結會讓磁碟看起來已滿，`flush` 就丟掉放得下的批次 | 判斷為誤報，行為不改，只在那一行上方加一句註解說明。Unix 上 `std::fs::DirEntry::metadata` 不跟隨 symbolic link（標準函式庫的文件：等同於對路徑呼叫 `symlink_metadata`），程式只把 `file_type().is_file()` 為真的 `.log` 算進大小，symbolic link 的 `is_file()` 是 false | 以釘選的 Rust 1.99.0 在建置主機上實測（repo 外的小程式）：目錄裡一個指向 1 GiB 稀疏檔的 `seg-00000000000000000001.log`，`entry.metadata()` 回 `is_file=false`、`is_symlink=true`、長度 127（連結本身）；跟隨連結的 `fs::metadata` 才是 1073741824 | — |
| 第十次 | 批次第一次送出的時間（時鐘比對的下限）在 `permit.publish` 之前就記下，broker 拒絕之後也留著。之後同一條連線上的重送成功時，它的確認是從那次沒送到雲端的送出算起：拒絕之後的退避最長 5 分鐘，快了這麼多的時鐘也會被當成已校正 | `attempt()` 改成：送出之前，若記下的是別條連線的送出就清掉（確認只會回答同一條連線上的送出）；在 `permit.publish` 之前讀系統時鐘，只有 broker 接受（`Ok`，PUBACK 0x00 或 0x10）而且這條連線還沒有記錄時才記下。被拒絕的（0x80 以上）、超過大小上限的、沒送出去的（`NotConnected`、`Busy`、`Stopped`）都不建立也不放寬記錄；PUBACK 之前連線就斷了的（`ConnectionLost`）也不記：下一次送出在新的連線（新的 session）上，送出之前就先清掉舊連線的記錄。確認比 PUBACK 先處理而還沒有記錄時不比對，狀態不變。同一類的檢查：預約位置失敗、磁碟上的紀錄讀不出來、送出前過期，本來就在記錄之前返回；比對只有 `acknowledged` 一處。已經被確認（`retry` 或不認得的狀態）過的那次送出仍是下限：它確實到了雲端，同一條連線上之後的確認可能回答的是它之前的送出，這是原本的設計，沒有改。`InFlight::published`、`FirstPublish`、`Pipeline::run` 與 `clock.rs` 的說明，以及 README「時鐘」都改成「第一次被 broker 接受的送出」 | `upload::a_refused_publish_does_not_widen_the_window_of_the_clock_comparison`（系統時鐘比雲端快 5 秒；broker 拒絕三次，等 1、2、4 秒，第四次在同一條連線上接受；確認的 `server_time` 比被接受的送出早 5 秒，超過 2 秒的範圍：算未校正）。修正之前它失敗在最後的斷言（時鐘被當成已校正）；`a_clock_an_hour_ahead_is_found_off_when_the_ack_answers_a_publish_on_a_new_connection`、`the_clouds_time_decides_clock_synced_for_batches_and_heartbeats`、`every_acknowledgement_outcome_is_handled_as_the_table_says` 照樣通過 | 改回「送出之前就記下、不看結果」（原本的做法）：新的測試失敗。改成「被拒絕的也以送出前的時間記下」：新的測試失敗。另外三個測試兩次都通過。檔案以雜湊確認已還原 |
| 第十一次 | 放行（第十次的修正之後） | — | — | — |
| 第十二次 | 第十一次放行、只更新本報告的數字之後，對最後暫存的狀態再跑的一次。例行整理存進 `clock` 檔的是呼叫端在工作開始之前拿到的年齡時鐘時間（`Buffer::tick` 的 `now`）；其間另一個讀取（上傳的）可能已經看到系統時鐘往前跳、把年齡時鐘推前，存下的較早的值就少了這一段。之後系統時鐘調回、行程在下一次存檔前結束時，重啟後批次看起來比較年輕，可能在雲端的去重紀錄過期之後再送一次 | `Buffer::save_clock` 改成在寫入前一刻從共用的年齡時鐘讀一次（`age.now()`，不小於呼叫端的 `now`）並存那個值；沒有年齡時鐘時照舊存 `now`。依同一類檢查了其他存檔與抬高下限的路徑：停止時的存檔本來就在存之前讀年齡時鐘，現在也經過同一個函式；`open_for_appends`、晚開成的磁碟與啟動時的開啟只抬高下限（`raise_floor` 在鎖裡讀、算、記），不存呼叫端的快照。存檔之後才發生的讀取無法不靠每次讀取都存檔來避免，那是「當掉時遺失最後一次存檔之後算進去的時間」的已知限制，不加每次讀取的存檔。README「時鐘」與本報告「留下的風險」改正：遺失的不是固定的 60 秒，而是年齡時鐘在最後一次存檔之後算進去的（通常不到 60 秒的真實時間，但也包括其間往前跳又調回的量），而且只在重啟時的系統時鐘落後時 | `buffer::tests::housekeeping_saves_the_age_clocks_time_as_it_is_when_saving`（先取得例行整理用的時間，再讓另一個讀取在系統時鐘往前 20 天時讀年齡時鐘，系統時鐘調回之後才執行例行整理：`clock` 檔的時間不小於那次讀到的值）。修正之前它失敗（存的是 20 天前的值）；`a_restart_keeps_a_forward_step_the_age_clock_counted_while_batches_were_on_disk`、`a_stop_saves_the_age_clocks_time_with_the_batches_it_writes`、`housekeeping_saves_the_age_clocks_time_every_minute_while_batches_are_on_disk` 照樣通過 | 改回「存呼叫端的 `now`」（原本的做法）：新的測試失敗，另外三個照樣通過。檔案以雜湊確認已還原 |

第十二次之後各次的結果不再寫回本報告：寫回本身會改變要審查的暫存內容。

### 獨立攻擊審查

2026-10-28 另一個 agent 在獨立的 worktree 裡對這個變更做攻擊審查（以 Codex 修正之前的版本為底，附上重現問題的 probe 測試），提出下表 10 點；它另外提出的「停止時跨 segment 的 flush 只 fsync 最後一個」已由上方 Codex 的第 2 點修好。probe 移植成回歸測試（測試的說明寫著對應的點）。不改的四點見表後。

每一點修好之後，以腳本把修正改回去一次（精確的文字替換，只建置、執行對應的測試，每次限 300 秒的牆上時間，跑完以雜湊確認檔案已還原）：21 次全部還原；兩次不改的基準（單元測試 19 個、`upload` 2 個）都通過，19 次改回都只有預期的測試失敗。

| 問題 | 修正 | 回歸測試 | 改回修正時 |
| --- | --- | --- | --- |
| 1. 高：SIGTERM 時的 flush 是 O(n²)，會超過停止的時間。每一批以線性搜尋找兩次（`position(id)`）；審查者在 release build 量到 16 MiB 的 267 bytes 批次（62,836 批）要 7.3 到 8.6 秒、64 MiB 要 292 秒。超過 `STOP_LIMIT`（10 秒）時行程照樣結束，flush 還在跑：批次遺失卻沒有 `NotFlushed`，還可能留下寫一半的紀錄 | flush 一次走過 FIFO（以位置走，不靠 id 排序，因為晚開成的磁碟會把批次放到前面）。寫入的期限是收到停止後 5 秒（`FLUSH_LIMIT`，停止時記下時間），每寫一筆之前檢查，時間到了就停在紀錄的邊界；每寫滿一個 segment 就 fsync 它，所以期限之後只剩目前這個 segment 的一次 fsync，都在 `STOP_LIMIT` 之內。沒寫的、寫入或 fsync 失敗的批次以 `NotFlushed` 回報（批數、筆數與原因，原因之一是「the time for the stop ran out」）。main 在 `STOP_LIMIT` 內等 pipeline 結束，pipeline 記下 `NotFlushed` 之後才結束 | `buffer::tests::a_stop_writes_a_full_memory_of_small_batches_within_its_time`（62,836 批 × 267 bytes，期限是測試給的 `FLUSH_LIMIT`：全部寫完、沒有 `NotFlushed`，debug build 約 0.7 秒）；`a_stop_that_runs_out_of_time_reports_the_rest_as_not_flushed`（期限在中途到：停在兩筆之間，寫了的 fsync 過、讀得回來，其餘以 `NotFlushed` 回報）；`a_stop_that_spans_segments_syncs_every_segment_it_wrote` 加上斷言：寫滿的 segment 在換下一個時就 fsync | 1a 加回每批兩次線性搜尋：第一個測試失敗（5.15 秒時期限已到、沒有寫完）。1b 拿掉期限：第二個失敗。1c 寫滿時不 fsync：第三個失敗 |
| 2. 中：年齡時鐘會倒退。`now()` 是 `max(牆上時鐘, 下限 + 單調時間)`，不記得回傳過什麼：牆上時鐘往前跳 40 天又改正時，`now()` 退回 39 天，跳的期間形成的批次會留過 30 天（真實的時間），雲端的完成紀錄可能先過期，批次被寫入兩次 | 每讀一次就把下限推到 `回傳值 − 單調時間`（`fetch_max`），所以永不回傳比之前的值加上其間的單調時間更小的值。關機期間時鐘往回跳的已知限制不變 | `clock::tests::a_forward_step_that_is_corrected_never_makes_the_age_clock_run_backward`、`buffer::tests::a_batch_formed_during_a_forward_step_expires_within_thirty_real_days`（兩個 probe） | 兩個都失敗 |
| 3. 中：緩衝批次的索引沒有上限。每一批（包括磁碟上的）在記憶體裡有一個索引項，審查者量到讀回磁碟後每批約 260 bytes（30 萬批讓 RSS 從 8.9 MiB 到 85 MiB）。小型現場預設設定斷線 30 天約 26 萬批、約 64 MiB；`upload_interval_s = 1` 時約 750 MiB，1 GB 的板子會 OOM，重啟後復原又重建索引而反覆當掉。批次很小也讓補送很慢：一個往返一批 | 三者都做：（a）上傳不通時（沒有連線，或等著的批次超過一批）收集視窗最長 5 分鐘（`OUTAGE_WINDOW`），或估計到約一批（批次上限的 2 倍）就關閉，所以斷線每 5 分鐘一批，30 天約 8,640 批（原本 26 萬批），與上傳間隔無關；（b）索引項變小：同一個串流共用一份 `Arc<StreamId>`（啟動讀磁碟時也共用），長度存 `u32`，一項 96 bytes；（c）批數上限 `MAX_BATCHES` = 65,536（記憶體與磁碟合計），新的一批超過時丟最舊的（`too_many_batches`）；啟動讀磁碟時同時留著的紀錄最多 1.5 倍，多的邊讀邊丟最舊的，最後留下形成時間最新的 65,536 批。上限下的量測（release build，`index_memory_of_a_full_batch_limit`，平常略過）：磁碟上 196,608 批（上限的 3 倍）讀完留下 65,536 批，RSS 比讀之前多 8.0 MiB（每批約 128 bytes），讀的過程中的高峰多 14.5 MiB，同時留著的紀錄最多 98,304 筆。斷線期間 5 分鐘一批時上限是 227 天，預設設定的小型現場在 30 天內不會因為批數而丟資料。決策 9 補上 2026-10-28 的說明：「縮短上傳間隔」只適用於上傳順利時，代價是斷線期間斷電最多多遺失 5 分鐘還沒關閉的視窗 | `upload::a_long_outage_forms_one_batch_per_five_minutes`（每秒一筆、斷線 6 小時：等著的是 71 到 72 批，不是 2,160 批；恢復後 74 次以內送完；之後一分鐘回到每 10 秒一批）；`buffer::tests::the_batch_limit_drops_the_oldest`；`start_up_keeps_at_most_the_batch_limit_of_the_newest_batches`（同時留著最多 150 筆，segment 檔最多 2 個）；`buffer::disk::tests::recovery_keeps_at_most_its_limit_of_the_newest_records`；`an_index_entry_stays_small`（`Entry` 最多 96 bytes、`Found` 最多 80 bytes） | 3a 斷線時照樣每個上傳間隔關閉：`a_long_outage_...` 失敗。3b 不共用串流 ID：`records_after_a_corrupt_header_are_found_again` 的共用斷言失敗。3c 新的批次沒有上限：`the_batch_limit_drops_the_oldest` 失敗；啟動時不篩：`recovery_keeps_...` 與 `start_up_keeps_...` 都失敗 |
| 4. 低：symbolic link 與外來的檔案可能被覆寫或刪掉 | 啟動時目錄裡每個 `seg-<n>` 名稱（不論檔案、目錄還是 symbolic link）都讓下一個編號跳過它；一般檔案以外的不讀、不刪、不採用；`.tmp` 以 `O_EXCL` 建立，失敗時只刪自己建的；改名前以 `symlink_metadata` 確認目標不存在，存在就放棄這個編號（檢查與改名之間仍有空檔，第三次 Codex 審查改成以 hard link 發布，見上方）；`u64::MAX` 不使用（用完就失敗，不重用） | `buffer::disk::tests::names_in_use_at_start_are_never_taken`、`a_name_that_appears_after_start_is_never_replaced_or_removed`、`the_highest_segment_number_is_never_used`（三個 probe） | 4a 只有一般檔案佔編號：第一個失敗。4b 建不起來的 `.tmp` 照樣刪：第二個失敗。4c 改名前不檢查：第二個失敗。4d 用最大的編號：第三個失敗 |
| 5. 低：一筆紀錄的檔頭壞掉，那個 segment 其餘的紀錄都遺失，也沒有計數 | 檔頭的 checksum 不符或不完整時，往後找下一個 record magic 而且檔頭 checksum 對得上的位置，從那裡繼續讀；略過的 bytes 數記在警告（`unreadable_bytes`），結尾寫一半的那一筆照舊算一筆不完整的紀錄 | `buffer::disk::tests::records_after_a_corrupt_header_are_found_again`（第 2 筆的檔頭翻一個 bit、payload 裡放一個不成檔頭的 magic：其餘 49 筆都找到，`unreadable_bytes` 剛好是第 2 筆的長度）；`buffer::tests::records_after_a_corrupt_header_are_kept`（49 批、490 筆留著，丟棄 0） | 兩個都失敗 |
| 6. 低：啟動時開不了的磁碟，停止時的 flush 不再試，記憶體裡的批次直接算遺失 | 停止時磁碟沒開、磁碟可以用而且記憶體裡有批次時，不管 60 秒的重試等待，先再開一次 | `buffer::tests::a_stop_tries_a_disk_that_failed_to_open_earlier`（probe） | 失敗 |
| 7. 低：`clock_synced` 的範圍從跨連線的第一次送出算起：時鐘快一小時、確認在第一次送出兩小時後才到時，會算成已校正 | 每條連線都是 Clean Start、沒有 session，確認只會回答同一條連線上的送出，所以下限改成這一批在目前這條連線上第一次送出的時間，換連線就重算。README 照此改寫 | `upload::a_clock_an_hour_ahead_is_found_off_when_the_ack_answers_a_publish_on_a_new_connection`（probe 的情境，經整個上傳流程） | 失敗 |
| 8. 低：`Reconnecting` 可能在一條線上的連線上永遠等下去：`reserve` 失敗之後才讀 session，工作被延遲時會把新的連線記成失敗的那條 | `reserve` 之前先記下當時的 session，失敗時算到它；拿到位置時用位置所屬的 session（`PublishPermit::session`）。另外 `Reconnecting` 有期限（`ACK_TIMEOUT`）：期限到了有連線就在這條送，沒有就再等一個期限 | `upload::tests::a_batch_waiting_for_a_new_connection_goes_on_the_one_up_after_the_ack_timeout` | 拿掉期限：失敗 |
| 9. 低：有些遺失只在節流的日誌裡：雲端 `rejected` 的批次、`BatchBuilder` 不收的讀值與編碼失敗的批次，節流期間略過的只計次數，沒有讀值的筆數 | 三者各自合計：每 60 秒最多一行，帶上一行之後的次數、讀值筆數（`readings`）與第一個的細節，停止時記下還沒記的。`dropped_records` 的意思不變（緩衝滿或過期等，沿用原本的計數） | `upload::tests::every_lost_reading_is_in_some_log_line`（一分鐘內多次：日誌各行的 `readings` 加起來是 100、3、11，等於實際遺失的筆數） | 改回原本的節流（略過的不記筆數）：失敗 |
| 10. 資訊：`memory_limit_mib = 0` 與 `disk_limit_mib = 0` 同時設定時，連線中也每一批都丟；已經存在的 `buffer/` 目錄 mode 比 0700 寬 | 設定檢查拒絕兩者都是 0；已經存在、其他使用者可以進入的目錄，agent 是擁有者時透過以 `lstat` 確認過的同一個目錄的 handle 改成 0700 並警告，不是擁有者時警告 | `config::tests::buffer_settings_are_bounded` 加上兩者都是 0 的情況；`buffer::disk::tests::an_existing_directory_open_to_others_is_made_private` | 10a 接受兩者都是 0：失敗。10b 不改 mode：失敗 |

第 5 點的 probe 預期 500 筆讀值都計入（留著的加上丟棄的）。修正後留著 49 批、490 筆；壞掉的那一筆檔頭讀不出來，它有幾筆讀值無從得知，所以不計入 `dropped_records`，改以 `unreadable_bytes` 回報它的 bytes。這是「只計真的讀不出來的」的結果，不是遺漏。

第 8 點的歸屬修正（`reserve` 失敗時算到哪一條連線）沒有決定性的回歸測試：要重現，得讓工作剛好在 `reserve` 失敗與讀 session 之間被延遲到換了連線。改以期限兜底，期限有上表的測試。

依審查的建議不改、維持 README 的說明：確認在 `Sending` 期間到達時的重複送出（雲端回 `duplicate`，無害）；`buffer.directory` 裡的 `..`（unit 的 `ProtectSystem` 限制可寫的地方）；調小的磁碟上限在下一次放入新的一批時才生效；大過上限的一批卡住 FIFO 直到過期。

#### 第二輪

2026-10-28 同一位審查者以前三次 Codex 修正之後的程式再審一輪，新的 probe 加在同一個檔案的最後。第一輪各點的狀態（審查者的結論）：

| 第一輪的點 | 第二輪的結果 |
| --- | --- |
| 1. 停止時的 flush（O(n²)、期限、`NotFlushed`） | 已修好，沒有退步 |
| 2. 年齡時鐘倒退 | 已修好，沒有退步 |
| 3. 索引沒有上限 | 已修好，沒有退步 |
| 4. symbolic link 與外來的檔案 | 已修好，沒有退步 |
| 5. 壞掉的檔頭之後重新對齊 | 已修好，沒有退步 |
| 6. 停止時重試開不了的磁碟 | 已修好，沒有退步 |
| 7. `clock_synced` 的範圍 | 已修好，沒有退步（修正在 `upload.rs` 依連線記下第一次送出；`GatewayClock::compare` 照設計不變，所以那個單元層級的 probe 改為略過） |
| 8. `Reconnecting` 的期限 | 已修好，沒有退步 |
| 9. 遺失的讀值都有數字 | 已修好，沒有退步 |
| 10. 兩個上限都是 0、目錄權限 | 已修好，沒有退步 |

新的兩個低風險問題與幾個資訊項目，加上第四次 Codex 審查的一點（年齡時鐘的並行），修正與測試如下。每一點修好之後同樣以腳本改回一次（只建置、執行對應的測試，每次限 300 秒，跑完以雜湊確認已還原）：13 次全部還原；不改的基準（新的 11 個單元測試）與 release build 的量測都通過，11 次改回都只有預期的測試失敗。

| 問題 | 修正 | 回歸測試 | 改回修正時 |
| --- | --- | --- | --- |
| A. 低：磁碟晚開成時，這個行程剛形成的批次可能被當成過期丟掉。啟動時開不了（暫時的錯誤），系統時鐘又比磁碟上最新的形成時間落後 30 天以上（例如 RTC 遺失了時間、NTP 還連不上）時，`close_window` 開成磁碟、把下限抬到磁碟上的最新一批，開成之前形成的批次看起來就超過 30 天 | 抬高下限移進 `Buffer`（`with_age_clock`）：開成磁碟時抬高下限，`AgeClock::raise_floor` 回傳它把時鐘往前推了多少，開成之前這個行程的批次（記憶體裡的；磁碟沒開時沒有寫到磁碟上的）形成時間加上同樣的量，年齡不變。它們仍排在磁碟上原有的批次之後，FIFO 的順序不變。停止時才開成的磁碟同樣處理（下一列），寫進磁碟的形成時間與磁碟上原有的在同一個尺度上，下一次啟動也不會被當成過期 | `buffer::tests::a_late_disk_open_keeps_the_age_of_batches_formed_before_it`（probe 的情境：系統時鐘落後 400 天，磁碟 61 秒後開成，兩批都是 61 秒大、都沒過期）；`a_stop_opens_a_disk_that_failed_before_for_appends_only` 的後半（停止時寫進去的那一批，下一次啟動照樣在） | 開成時抬高下限但不移動之前的批次：兩個測試都失敗。只拿掉停止時的抬高：第二個失敗 |
| B. 低（要有 0700 目錄的寫入權限）：啟動讀磁碟時，串流 ID 的表留著讀過的每一個 ID，包括已經丟掉的紀錄的。100 萬筆 1 byte、每筆一個不同的 64 字元串流的目錄，讓高峰多了 264 MiB | 每次丟掉紀錄（`prune`）時，同時清掉只剩表本身在用的 ID（`Arc::strong_count` 為 1），表的大小就不超過留著的紀錄數 | `buffer::disk::tests::recovery_keeps_only_the_stream_ids_of_the_records_it_keeps`（1,000 筆各自不同的串流、上限 10：同時留著的 ID 最多 15 個，留下最新的 10 筆）；release build 的量測 `recovery_memory_of_records_on_distinct_streams`（平常略過）：同樣 100 萬筆、121 MB，高峰多 37.8 MiB（改回時 263.8 MiB），讀完約 1.1 秒 | 留著所有 ID：單元測試失敗；量測超過 64 MiB 的上限而失敗 |
| 資訊 1. 停止的路上不能有跑太久的步驟：停止時的 `close_window` 會呼叫 `tick`，可能開始完整讀磁碟；`flush` 重試開不了的磁碟時也跑完整的復原，在 SD 卡上可能要幾十秒 | `Buffer` 有停止的旗標（`with_stop`），一收到停止就設（pipeline 的 watcher，以及跳出主迴圈時再設一次）：`tick` 不再開磁碟；`DiskStore::open_until` 每讀一筆紀錄前看旗標，停止時在下一筆就結束、開啟算失敗。`flush` 的重試改用 `DiskStore::open_for_appends`：只列出名稱與 segment 的大小，只讀編號最大的 segment 的紀錄檔頭找最新的形成時間（同樣受停止的期限限制），其他什麼都不讀、不刪；原有的批次留給下次啟動。期限之內沒寫的照舊以 `NotFlushed` 回報 | `buffer::tests::housekeeping_opens_no_disk_once_stopping`；`a_stop_opens_a_disk_that_failed_before_for_appends_only`（只有 `DiskOpenedForStop` 一個事件、緩衝裡只有這個行程的那一批、原有的 segment 位元組不變）；`buffer::disk::tests::a_stop_ends_reading_the_directory_at_the_next_record`（第 6 次詢問就結束、`Interrupted`、檔案不變，之後正常開啟找到 20 筆）；`the_stop_open_reads_and_removes_nothing`（最新的形成時間、大小、留下的 `.tmp` 不動、新的 segment 跳過佔著的編號） | 停止時照樣例行開磁碟：第一個失敗。停止時完整讀磁碟（原本的做法）：第二個失敗。讀取不看停止：第三個失敗 |
| 資訊 2. 目錄沒有鎖：兩個 agent 可能同時用同一個目錄 | 開啟時對目錄本身取得排他的 `flock(2)`（`File::try_lock`，標準函式庫的穩定 API，沒有 `unsafe`），存在 `DiskStore` 裡直到它結束。被別的 store 鎖著時回傳 `WouldBlock`、訊息寫明「in use by another agent」，`Buffer` 照既有的路徑當成開不了、60 秒後再試。選擇鎖目錄本身而不是另建鎖檔：不多一個要建立、檢查（symbolic link）或從目錄列表排除的檔案，行程結束（包括當掉）時核心就放開 | `buffer::disk::tests::a_second_store_on_the_same_directory_is_refused_while_the_first_is_open`（第二個 `open` 與 `open_for_appends` 都被拒，第一個結束後就開得了） | 不取鎖：失敗 |
| 資訊 3. 刪不掉的 `.tmp`（例如 EPERM）讓 `open` 一直失敗 | 記一筆警告、留著它，編號照樣算佔用，繼續開啟 | `buffer::disk::tests::a_leftover_temporary_file_that_cannot_be_removed_is_left_and_its_number_skipped`（目錄唯讀時開啟成功、`.tmp` 還在、下一個編號是 6；以 root 執行時唯讀不起作用，測試會說明後略過，這次不是 root） | 刪不掉就失敗（原本的做法）：失敗 |
| 資訊 4. segment 檔頭的 12–16 bytes 沒有檢查 | 視為保留、必須是 0；不是 0 的檔案不是我們的：不讀、不刪，記一筆警告 | `buffer::disk::tests::a_segment_whose_reserved_header_bytes_are_not_zero_is_left_alone` | 不檢查：失敗（兩筆紀錄被讀進來） |
| 第四次 Codex 審查：`AgeClock::now` 在更新共用的下限之前就算好回傳值。一個呼叫算出較小的值後被暫停，另一個呼叫看到系統時鐘往前跳、抬高下限並回傳，前一個接著回傳那個過時的小值，可能讓超過 30 天的批次被送出，或讓新的批次被丟掉 | 下限改成 `Mutex<u64>`：讀系統時鐘與單調時鐘、算出 `max(系統時鐘, 下限 + 單調時間)`、記下新的下限、回傳，全在同一個鎖裡；`raise_floor` 也一樣（並回傳把時鐘往前推了多少，給 A 用）。中毒的鎖照 crate 的慣例取回內容繼續用。呼叫很少、臨界區很小 | `clock::tests::a_caller_paused_across_a_forward_step_never_returns_an_earlier_value_than_one_already_returned`（只在測試編譯的掛鉤讓呼叫 A 在算出值之後、記下之前停住；這時系統時鐘往前跳 40 天，呼叫 B 若不必等 A 就能完成，就拿到往前跳之後的值；之後時鐘改回、A 繼續。修正後 B 必須等 A，不會有「B 先完成、A 之後才交出較早的值」；之後的呼叫也不早於 A）；`the_floor_rises_to_the_newest_stored_batch` 加上 `raise_floor` 回傳值的斷言 | 改回「鎖外算值、再鎖住記下」（原子變數版本的行為）：失敗 |

依審查的建議不改、在 README 說明：形成時間接近 `u64::MAX` 的惡意紀錄（要在 0700 的目錄裡放一筆 checksum 正確的紀錄，需要 agent 使用者或 root 的權限；README 的「已知的限制」加了一句）；`ignored` 清單的長度；斷線期間的視窗與斷電的說明（第一輪已寫）。原本也列為不改的「並行的 `AgeClock::now()` 相差幾毫秒」，因第四次 Codex 審查改為修正（上表最後一列）。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 斷網一小時後恢復，資料依序補送且不重複（測試） | 通過 | `an_hour_offline_then_every_reading_arrives_once_in_order` |
| 已寫入磁碟的批次在行程被強制結束後以同一序號重送；記憶體中的批次遺失時，重啟後的新批次不會被雲端當成重送丟棄（測試） | 通過 | `after_a_forced_end_disk_batches_go_again_and_new_ones_use_a_new_stream`，單元測試 `batches_an_earlier_process_left_are_sent_with_the_disk_switched_off`、`a_stop_that_spans_segments_syncs_every_segment_it_wrote` |
| 緩衝達上限時丟棄最舊的資料並記錄，程式不當掉（測試） | 通過 | `memory_full_with_a_failing_disk_drops_the_oldest_and_keeps_going`、`a_full_disk_drops_the_oldest_and_keeps_going`，單元測試 `a_full_disk_drops_the_oldest_batches_first`、`a_failing_disk_keeps_the_newest_batches_in_memory_and_is_tried_again_later`、`batches_above_a_lowered_disk_limit_are_kept_until_the_disk_needs_room`、`a_failed_sync_at_the_stop_reports_its_batches_as_lost`、`the_batch_limit_drops_the_oldest`、`start_up_keeps_at_most_the_batch_limit_of_the_newest_batches`、`a_stop_that_runs_out_of_time_reports_the_rest_as_not_flushed` |
| 重裝後使用新的串流 ID（測試） | 通過 | `a_restart_and_a_fresh_state_directory_each_get_a_new_stream`，單元測試 `stream_ids_are_valid_random_and_distinct`、`without_randomness_the_pipeline_is_not_created` |
| 時鐘跳動的情境有測試並符合本 issue 決定的處理方式 | 通過 | `a_backward_clock_step_does_not_keep_a_batch_past_thirty_days`、`a_change_of_the_sync_state_splits_the_batch`、`the_clouds_time_decides_clock_synced_for_batches_and_heartbeats`、`a_clock_an_hour_ahead_is_found_off_when_the_ack_answers_a_publish_on_a_new_connection`，單元測試 `clock::tests::*`（含 `a_forward_step_that_is_corrected_never_makes_the_age_clock_run_backward`、`a_caller_paused_across_a_forward_step_never_returns_an_earlier_value_than_one_already_returned`）、`a_batch_formed_during_a_forward_step_expires_within_thirty_real_days`、`a_late_disk_open_keeps_the_age_of_batches_formed_before_it`、`a_restart_keeps_a_forward_step_the_age_clock_counted_while_batches_were_on_disk`、`a_stop_saves_the_age_clocks_time_with_the_batches_it_writes`、`a_refused_publish_does_not_widen_the_window_of_the_clock_comparison`、`a_slow_acknowledgement_never_judges_a_clock_20_s_ahead_synchronized`，單元測試 `a_wall_clock_step_between_the_publish_and_the_ack_is_never_compared`、`clock::tests::a_wall_clock_step_during_a_floor_raise_is_not_counted_as_the_raise`、`upload::an_acknowledgement_before_the_puback_waits_for_the_send_and_is_compared` |
| 確認的每一種結果（設計決定 4 的對應表） | 通過 | `every_acknowledgement_outcome_is_handled_as_the_table_says` |
| 經真的 broker 的批次與確認 | 通過 | `a_batch_goes_up_through_mosquitto_and_the_clouds_acknowledgement_releases_it` |
| 壞掉或只寫一半的紀錄不送出，不認得的檔案不動 | 通過 | `a_corrupt_or_torn_record_is_never_sent_and_unknown_files_stay`，單元測試 `buffer::disk::tests::*`（含 `records_after_a_corrupt_header_are_found_again`、`names_in_use_at_start_are_never_taken`、`a_name_that_appears_after_start_is_never_replaced_or_removed`） |
| 與設定套用（Laysi/trustiot-v2#94）的整合：輪詢的讀值成為批次、由雲端的確認拿掉；執行檔訂閱兩個頻道、緩衝在狀態目錄底下；停止時正在進行的輪詢的讀值也寫進磁碟（2026-10-29） | 通過 | `desired_config::polled_readings_go_up_as_a_batch_and_the_clouds_acknowledgement_releases_it`、`binary::a_batch_in_the_state_directory_goes_up_and_the_clouds_acknowledgement_releases_it`、`binary::connected_the_agent_applies_its_configuration_reports_it_and_polls_it_after_a_restart`、`main.rs` 的 `a_stop_lets_the_polls_in_flight_end_and_writes_their_readings_to_disk` |

待測（依驗證分級屬於進階驗證，2026-10-28 延後到 Laysi/trustiot-v2#718）：

- 在 lab 的閘道器上實際斷網、斷電：本報告的斷電只以「不 flush 就結束」模擬，沒有在 SD 卡上拔電；執行檔現在會輪詢（與 Laysi/trustiot-v2#94 整合之後），但讀值由執行檔自己輪詢、送上雲端的端對端測試要有真的設備（測試機器上的假端點被位址限制拒絕），對執行檔送 SIGKILL 的端對端測試也一樣，都在閘道器上做。
- 閘道器上的記憶體與補送速度：`MALLOC_MMAP_THRESHOLD_` 的效果沿用可行性測試（2026-10-13）的數據，沒有以這個實作重測。索引的記憶體（每批約 128 bytes）與停止時 16 MiB 的 flush 速度只在建置主機（x86_64）量過；SD 卡上 5 秒（`FLUSH_LIMIT`）寫得完多少還沒量，寫不完的會以 `NotFlushed` 記下。
- 與雲端 ingest（Laysi/trustiot-v2#91）的實際來回：本報告的雲端是測試裡的替身（每個串流一個高水位）與 Mosquitto 上的 `mosquitto_pub`。

留下的風險：

- 磁碟騰出位置之後寫入才失敗：`push` 為了寫進磁碟而丟掉最舊的批次（`disk_full`）之後，寫入若失敗（例如資料分割區壞掉），新的一批又比記憶體上限大時也會遺失；停止時的 `flush` 同樣。寫之前無從得知寫入會失敗，所以沒有改；遺失的批次都有計數與警告。
- 年齡時鐘的 `clock` 檔最多每 60 秒存一次：當掉時，年齡時鐘在最後一次存檔之後算進去的都會遺失（通常不到 60 秒的真實時間，但也包括其間往前跳又調回的量），而且只在重啟時的系統時鐘比年齡時鐘落後時；關機的那段時間在系統時鐘往回跳時也不算（見 README「時鐘」的已知的限制）。
