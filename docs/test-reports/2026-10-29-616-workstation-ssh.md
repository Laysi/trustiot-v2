# 工作站經 ssh 登錄閘道器的驗收證據

Laysi/trustiot-v2#616：`trustiot-workstation register --gateway <ssh 目的地>` 在閘道器上執行 `trustiot-agent provision`、檢查報告後登錄；`verify` 在平台的事件紀錄找登錄事件。

## 環境

- 建置主機：建置主機（x86_64 Linux），經協調者的檢查佇列執行，`CI=1`（需要容器的測試沒有 podman 或 docker 時失敗、不略過）。
- 工具版本依 `.prototools` 與 `rust-toolchain.toml`。
- 分支 `feat/616-workstation-ssh`，tree 為 commit `9687f0a9`（之後只加了這份報告）。
- 閘道器端：沒有用到真的閘道器。ssh 是放在 `PATH` 前面的假 `ssh`（`factory/workstation/tests/support/ssh.rs`），它記錄 argv、環境、stdin，以 `env -i` 在暫存目錄的「遠端」執行收到的指令；TPM 是 testkit 在拋棄式容器裡啟動的 swtpm（`testkit::swtpm`），`trustiot-agent provision` 由測試直接呼叫 `agent::provision::run`。
- 平台端：`tests/support/registry.rs` 的測試替身（登錄端點與事件紀錄端點），回應依 `services/certs` 的契約。

## 指令

從 repository 根目錄執行：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --exclude agent --no-fail-fast
cargo test --locked -p workstation --test pipeline_swtpm -- --nocapture
cargo deny --locked check
bun install --cwd scripts --frozen-lockfile && bun test scripts
```

`agent` 不在這次的 workspace 測試裡：這個分支沒有改 `edge/agent`，它的測試在 Laysi/trustiot-v2#613 驗過；`pipeline_swtpm` 以 dev-dependency 使用它的 `provision` 模組。

## 結果

摘錄在 [data/2026-10-29-616-workstation-ssh/checks.txt](data/2026-10-29-616-workstation-ssh/checks.txt)（只刪了檢查佇列與它連線的狀態行，結尾補上結束碼與時間）。

2026-10-29 在同一個工作裡依序執行，全部通過（`exit 0 at 21:00:58 (build host)`）：

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `cargo test --locked --workspace --exclude agent --no-fail-fast` | 結束碼 0；摘錄只留最後 400 行（含 `repo-check`、`sql-check` 與 workstation 的每個測試檔），留下的 `test result` 合計 1671 passed、0 failed、1 ignored |
| workstation 的測試 | 單元測試 57、`tests/ssh.rs` 9、`tests/verify.rs` 4、`tests/pipeline_swtpm.rs` 1，全部通過；`services/certs` 的 `tests/workstation.rs` 4 通過 |
| `cargo test --locked -p workstation --test pipeline_swtpm -- --nocapture` | 1 passed（swtpm 在容器裡啟動，沒有略過） |
| `cargo deny --locked check` | advisories ok, bans ok, licenses ok, sources ok |
| `bun test scripts` | 1704 pass、0 fail、9 skip（略過的是需要另外開啟的 podman 端到端與映像測試，與這次的改動無關） |

swtpm 測試印出的證據行（依序為第一次登錄、重跑、模擬重刷、`tpm2_clear` 之後、加上 `--allow-new-key`；2164261120 即預設 handle `0x81000100`，`--allow-new-key` 之後 TPM 上只有新建的這一把金鑰）：

```text
evidence: exit codes [Some(0), Some(0), Some(0), Some(20), Some(10)]; platform answers [201, 200, 200, 409]; ssh calls 9; agent runs 8; persistent handles after --allow-new-key [2164261120]
```

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 假的 `ssh` 記錄 argv 與 stdin：到閘道器的只有 agent 執行檔、硬體 ID 與 handle；沒有 cookie、驗證碼、工作站憑證或私鑰、簽章標頭；遠端的清理指令有執行 | 通過 | `tests/ssh.rs` 的 `only_the_agent_the_hardware_id_and_the_handle_cross_to_the_gateway`：argv 逐字比對固定選項與遠端指令，stdin 逐 byte 等於 agent，環境只有白名單的變數；`assert_nothing_secret_crossed` 在 argv、環境與 stdin 找 session cookie、登入驗證碼、Access 憑證、工作站憑證與私鑰的每一行、送出的簽章、nonce 與憑證標頭的值，以及這些標頭的名稱，都找不到；結束後「遠端」的暫存目錄已不存在。`the_installed_agent_runs_without_an_upload_and_sudo_never_asks`：不上傳時 stdin 為空、`sudo -n` |
| swtpm 整條流程：201、重跑 200 同序號、模擬重刷 200、`tpm2_clear` 後停下、`--allow-new-key` 時 409 `public_key_mismatch` 與固定結束碼 | 通過 | `tests/pipeline_swtpm.rs` 的 `a_gateway_registers_reruns_and_stops_after_tpm2_clear`，證據行見上：清除後停在 20（沒有建立金鑰），`--allow-new-key` 時平台回 409、工具結束碼 10 |
| ssh 連線失敗會自動重試，每次重試的 `provision` 結果相同 | 通過 | `ssh_failures_are_tried_again_and_every_run_reports_the_same`（假 ssh 在執行前與執行後失敗，每次的報告相同、只登錄一次）；swtpm 測試的第一次登錄也經過一次連線失敗與一次遺失的回應，金鑰相同；一直連不上時結束碼 23（`a_gateway_that_stays_unreachable_stops_with_23`） |
| `verify` 顯示剛登錄的事件 | 通過（測試替身） | `tests/verify.rs` 的 `verify_shows_the_registration_event_of_the_gateway` 對事件紀錄的測試替身；找不到時結束碼 24（`verify_without_an_event_ends_with_24`）；swtpm 測試在第一次登錄後也跑 `verify` |
| 硬體 ID 或 handle 含空白、分號、引號、`$(…)` 時在送出前被拒 | 通過 | `values_that_could_change_the_remote_command_are_refused_before_ssh_runs`：每個值結束碼 2、假 ssh 一次都沒被執行；另有 `ssh_options_without_gateway_are_refused` |
| `cargo test -p workstation` 與 workspace clippy 通過；README 的操作員步驟與 `--help` 一致 | 通過 | 上面的檢查；`the_readme_and_the_help_agree` 比對 README 與 `--help` 的子指令、選項與結束碼表 |

另外驗證的失敗路徑：閘道器上的 agent 失敗、上傳的 agent 雜湊不符、ssh 連線在 agent 執行中被掛斷（HUP）時結束碼 22、不採用輸出，上傳的檔案都被遠端的清理刪掉（`a_failure_on_the_gateway_stops_with_22_and_its_output_is_not_used`）；違反讀者義務的報告與超過 16 KiB 的輸出以 21 拒絕（`reports_that_break_a_reader_obligation_are_refused_with_21`）；工作站有紀錄但 TPM 沒有金鑰時停在 20（`a_known_gateway_whose_tpm_has_no_key_stops_with_20`）。

### 待測與延後

- **閘道器端的 ssh 限制**（強制指令的轉接程式、sudoers、工作站改送短請求）：沒有實作，是出廠映像階段的前提條件，記在 Laysi/trustiot-v2#219。在那之前 `--upload-agent` 與 `--remote-sudo` 只用在 lab 與展示用的閘道器。
- **真的 ssh 與真的閘道器**：待測。這張 issue 只用假 ssh 與 swtpm；對 lab 的閘道器登錄在 Laysi/trustiot-v2#618。
- **進階驗證**（依驗證分級延後）：ssh 卡住或很慢時的期限（單次 300 秒、整體 420 秒）：程式有這兩個期限，但沒有測試讓 ssh 一直不結束、等到期限，這條路徑還沒實測，以及對 `services/certs` 真實服務跑 `verify`（`services/certs/tests/workstation_tool.rs` 目前只涵蓋 Laysi/trustiot-v2#615 的登錄）。
