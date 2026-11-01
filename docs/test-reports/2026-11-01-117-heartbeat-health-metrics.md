# 閘道器心跳的運作指標與設備讀取統計（2026-11-01）

Laysi/trustiot-v2#117（清單 H1 的閘道器端）：心跳附帶 CPU、記憶體、資料分割區用量、SoC 溫度、供電不足與降頻旗標、開機時間與重啟次數、程式與設定版本、離線緩衝積壓、時鐘是否已校正、憑證到期日，並記錄每台設備的讀取統計（最後成功讀取時間、成功率所需的請求數與失敗數、逾時與錯誤類型）。這份報告是驗收條件的證據：全部由自動化測試以 `CI=1` 實際執行，需要容器的測試不略過。

協定本身沒有改動：`protocol::health::Heartbeat` 早已有這些欄位（Laysi/trustiot-v2#269 之前就定義好），這次是 agent 把它們填進去。雲端的消費端（gateway-ops）只讀心跳的共同欄位，所以不必更動。

## 環境

- **程式碼**：Laysi/trustiot-v2#117 的工作分支，基於 main `0627c59a`。
- **機器**：建置主機（arm64 Linux VM），經檢查佇列執行；容器測試由 podman 起 Mosquitto 與 Debian 13 容器。aarch64 的交叉建置在建置主機（x86_64 Linux）。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0；交叉建置用 `.prototools` 釘選的 zig 與 cargo-zigbuild。
- **設定**：`CI=1`。故障注入用 `testkit::modbus::FakeModbus`（Modbus TCP）與停住的時鐘（`testkit::clock::manual`）：逾時是測試把時鐘推過去才發生，不是機器慢。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
CI=1 cargo test --locked --no-fail-fast -p agent -p protocol -p repo-check -p service-kit
CI=1 cargo test --locked -p agent --test device_health -- --nocapture
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
```

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings` | 通過 |
| `CI=1 cargo test --locked --no-fail-fast -p agent -p protocol -p repo-check -p service-kit` | 73 個測試目標全部 ok、0 失敗；`agent` 的單元測試 400 個通過、2 個 ignored（量測用，要明確指定才跑） |
| `cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent` | 通過（aarch64、glibc） |
| 安全審查 L1 修正之後：`cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`CI=1 cargo test --locked --no-fail-fast -p agent -p protocol -p repo-check`、同樣的 zigbuild | 全部通過（52 個測試目標 ok、0 失敗）；心跳的兩個新測試連跑 6 次都通過 |

與這次有關的測試：

- **心跳的欄位（驗收條件 1）**：
  - `edge/agent/src/heartbeat.rs` 的 `a_heartbeat_carries_every_operating_metric_of_the_checklist` 以各來源的值建出心跳，經 `encode`／`decode` 往返後逐欄檢查：程式與設定版本、CPU（20 %）、記憶體、資料分割區、SoC 溫度（48.5 度）、供電不足與降頻旗標（`now`、`since_boot`）、開機秒數、開機次數與本次開機後的啟動次數、agent 執行秒數、離線緩衝、時鐘、憑證到期日，以及一台設備的統計。
  - 同一檔案的 `the_cpu_is_measured_between_heartbeats_and_throttling_flags_stay_seen`：CPU 是兩次心跳之間的差；兩次讀取之間出現又被韌體清掉的降頻旗標留在 `since_boot`。`unreadable_proc_files_report_zeros_and_still_encode`：讀不到的來源回報 0 或不送欄位，心跳仍可編碼。
  - `edge/agent/src/host.rs`：`/proc/stat`、溫度檔、降頻字（有無 `0x`、壞字串）、`statvfs`、開機次數（同一次開機重啟 agent 加啟動次數、換 boot ID 加開機次數、檔案壞掉或寫不進去不擋計數、讀不到 boot ID 回 0）的解析與計算。
  - 協定側：`crates/protocol` 的既有測試（golden、compat、validation、limits，1000 台設備的最壞心跳仍放得進 1 MiB）全部通過，協定沒有改動。
- **故障注入下各錯誤類型的計數（驗收條件 2）**：`edge/agent/tests/device_health.rs` 的 `injected_faults_are_counted_under_their_kind` 對 testkit 假 Modbus 端點跑真正的輪詢迴圈，每一步之後檢查計數：

| 注入的故障 | 預期的計數 | 結果 |
| --- | --- | --- |
| 正常讀取 | `requests` 1、`last_success` 是讀值的時間、資料點 1/0 失敗、沒有 `last_failure` | 通過 |
| Modbus 例外碼 2、11、2 | `exception` 3、`exception_codes` {2: 2, 11: 1}、最近一次輪詢的資料點 1/1 失敗、`last_failure` 是例外 2 | 通過 |
| 設備沉默（不回應） | `timeout` 1、`last_failure` 是 `timeout` | 通過 |
| 其他站號的回應（`unit_mismatch(9)`） | `unit_mismatch` 1，那個請求逾時，`timeout` 2 | 通過 |
| 恢復正常 | 資料點 1/0 失敗、`last_failure` 仍是 `timeout` | 通過 |
| 連線被重設，重試也被重設 | `connect` 1，`requests` 8，各類型計數 {connect 1, timeout 2, exception 3, unit_mismatch 1, invalid_value 0} | 通過 |
| 值解不出來（NaN 浮點數，請求成功） | `invalid_value` 1、`requests` 1、資料點 1/1 失敗、`last_success` 為空 | 通過 |
| 連不上（沒有人聽的埠）兩次 | `connect` 2、`requests` 2 | 通過 |

  同一個測試檢查：設備在第一次輪詢之前就在統計裡（請求數 0）；統計放進心跳後通過協定的驗證；`HealthSink` 把每次輪詢、啟動與移除原樣交給下一個 `Sink`（11 次輪詢、3 次啟動、最後是 `removed ghost`），移除的設備從統計消失。`edge/agent/src/health.rs` 的單元測試另外檢查：略過的請求不算請求、例外碼超過 16 種時只算在 `exception`、部分失敗的輪詢、重啟的設備保留計數而移除再加入的從 0 開始、惡意的失敗說明（NUL、過長）進了統計之後仍在協定允許的範圍。

- **其他**：`edge/agent/tests/secret_inputs.rs` 的 libc 允許清單加了 `statvfs`（資料分割區用量用，唯讀）；該測試通過，agent 仍不讀其他環境變數、不啟動其他程式。`tests/link.rs`、`tests/upload.rs`、`tests/renewal.rs`、`tests/root_crl_update.rs`、`tests/binary.rs` 對心跳的既有測試全部通過，包括在停住時鐘下的心跳（心跳改在 blocking pool 讀檔，沒有影響）。

- **心跳不被卡住的讀取擋住（安全審查 L1）**：心跳讀 `/proc`、`/sys` 與 `statvfs` 的 blocking 讀取原本沒有時限，卡住（例如韌體 mailbox 不回應）時心跳永久停止。現在 `HostReader` 最多等心跳間隔的四分之一（1 到 5 秒，`host_read_limit`），逾時就以空的樣本照常送出（欄位為 0、溫度與降頻旗標不送），卡住的讀取留在它的執行緒上，同時只有一個，之後的心跳繼續等同一個，結束後第一個發現的心跳使用它的結果。測試以具名管道（`mkfifo`，沒有寫入端，讀取會一直卡住）當作永不回應的檔案：
  - `heartbeat.rs` 的 `a_host_read_that_never_returns_delays_the_heartbeat_by_the_limit_and_holds_one_thread`：停住的時鐘由測試推過時限，連續 4 次心跳都以空樣本回來，`started` 保持 1（只有一個讀取）；放行之後第一個發現它結束的心跳拿到結果，下一次才開新的讀取（`started` 為 2）。`the_wait_for_the_host_is_a_quarter_of_the_interval_within_bounds` 檢查時限的算法。
  - `tests/link.rs` 的 `heartbeats_go_on_while_a_host_file_never_answers`：整條 `heartbeat::run` 對腳本化的 broker，溫度檔卡住時連續 4 則心跳照樣送出、`soc_temp_celsius` 為空。tokio 的停住時鐘在有 blocking 執行緒忙碌時不會自己前進，所以測試自己一秒一秒推進；因此這個測試不斷言心跳之間的間隔（那由上一個測試的時限負責）。
  - `tests/link.rs` 的測試結束或失敗時都會放行管道（`Drop`），否則 runtime 會等卡住的執行緒而讓整個測試行程不結束。

一次觀察：第一次完整執行時 `edge/agent/tests/desired_config.rs` 的 `polled_readings_go_up_as_a_batch_and_the_clouds_acknowledgement_releases_it` 有一次以測試自己的有界等待逾時（`testkit::clock::bounded`）；那個測試不經過這次改動的程式（它的 `Sink` 是自己的 `Watched`），建置主機當時同時有其他工作。之後同一個測試單獨連跑 5 次、兩次完整執行都通過，沒有重現。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 心跳訊息包含上述全部欄位（protocol 測試） | 通過：協定的欄位早已齊全且有 golden 與驗證測試；agent 端以實際的來源填滿每個欄位，往返後逐欄檢查 |
| 故障注入下各錯誤類型的計數正確（測試） | 通過：連不上、逾時、Modbus 例外碼、站號不符，另加解不出的值，各自的計數與請求數、最後成功與最後失敗都對 |

待測（進階驗證，依常設決定延後，不影響上面的驗收）：

- 在真正的閘道器（Raspberry Pi OS）上讀 `get_throttled`、`thermal_zone0` 與 `statvfs` 的實測值；本次只以檔案內容的單元測試與沙箱的說明（唯讀的 `/proc`、`/sys`）驗證，閘道器對 agent 是唯讀的，沒有部署。
- 計算量與記憶體的量測（每次心跳多讀四個檔案與一次 `statvfs`）。
