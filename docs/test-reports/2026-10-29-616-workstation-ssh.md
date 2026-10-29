# 工作站經 ssh 登錄閘道器的驗收證據

Laysi/trustiot-v2#616：`trustiot-workstation register --gateway <ssh 目的地>` 在閘道器上執行 `trustiot-agent provision`、檢查報告後登錄；`verify` 在平台的事件紀錄找登錄事件。這份報告記錄安全審查的修正之後的結果（見「安全審查的修正」）。

## 環境

- 建置主機：建置主機（x86_64 Linux），經協調者的檢查佇列執行，`CI=1`（需要容器的測試沒有 podman 或 docker 時失敗、不略過）。
- 工具版本依 `.prototools` 與 `rust-toolchain.toml`。
- 分支 `feat/616-workstation-ssh`：完整的檢查在 commit `8219b52b` 的 tree 上；之後的 `c8efbb1c` 只改了 README 的一句話，在它上面另跑了讀 README 的 `tests/verify.rs` 與 `bun test scripts`。之後只加了這份報告。
- 閘道器端：沒有用到真的閘道器。ssh 是放在 `PATH` 前面的假 `ssh`（`factory/workstation/tests/support/ssh.rs`），它記錄 argv、環境、stdin，回答 `ssh -G`，以 `env -i` 在暫存目錄的「遠端」執行收到的指令；TPM 是 testkit 在拋棄式容器裡啟動的 swtpm（`testkit::swtpm`），`trustiot-agent provision` 由測試直接呼叫 `agent::provision::run`。
- 平台端：`tests/support/registry.rs` 的測試替身（登錄端點與事件紀錄端點），回應依 `services/certs` 的契約。

## 指令

從 repository 根目錄執行：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --exclude agent --no-fail-fast
cargo test --locked -p workstation --test pipeline_swtpm -- --nocapture
cargo deny --locked check
cargo test --locked -p workstation --test verify
bun install --cwd scripts --frozen-lockfile && bun test scripts
```

`agent` 不在這次的 workspace 測試裡：這個分支沒有改 `edge/agent`，它的測試在 Laysi/trustiot-v2#613 驗過；`pipeline_swtpm` 以 dev-dependency 使用它的 `provision` 模組。

## 結果

摘錄在 [data/2026-10-29-616-workstation-ssh/checks.txt](data/2026-10-29-616-workstation-ssh/checks.txt)：只留 `test result`、`Running`、證據行、cargo-deny 的結論與檢查佇列的結束行，刪了檢查佇列連線的狀態行。

2026-10-29 在同一個工作裡依序執行，全部通過（`exit 0 at 22:06:51 (build host, 616-c4c0f1c0)`）：

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `cargo test --locked --workspace --exclude agent --no-fail-fast` | 通過；`test result` 合計 1876 passed、0 failed、1 ignored（含 `repo-check`、`sql-check`） |
| workstation 的測試 | 單元測試 61、`tests/ssh.rs` 13、`tests/verify.rs` 5、`tests/register.rs` 10、`tests/login.rs` 17、`tests/pipeline_swtpm.rs` 1，全部通過；`services/certs` 的 `tests/workstation.rs` 4、`tests/workstation_tool.rs` 1 通過 |
| `cargo test --locked -p workstation --test pipeline_swtpm -- --nocapture` | 1 passed（swtpm 在容器裡啟動，沒有略過） |
| `cargo deny --locked check` | advisories ok, bans ok, licenses ok, sources ok |

在 `c8efbb1c` 上（22:08:32）：`tests/verify.rs` 5 passed（含比對 README 與 `--help` 的 `the_readme_and_the_help_agree`）；`bun test scripts` 1704 pass、0 fail、9 skip（略過的是需要另外開啟的 podman 端到端與映像測試，與這次的改動無關）。這個工作的結束行印成 `exit  at 22:08:32`，沒有印出結束碼；兩組測試的輸出都是全部通過。

swtpm 測試印出的證據行（依序為第一次登錄、重跑、模擬重刷、`tpm2_clear` 之後、加上 `--allow-new-key`、同一顆 TPM 帶著那次建立的金鑰再跑；2164261120 即預設 handle `0x81000100`）：

```text
evidence: exit codes [Some(0), Some(0), Some(0), Some(20), Some(10), Some(20)]; platform answers [201, 200, 200, 409]; ssh calls 10; agent runs 9; persistent handles after --allow-new-key [2164261120]
```

最後一次停在 20，而不是 27：swtpm 的 `tpm2_clear` 沒有改變 EK，工具認得是同一顆 TPM，只是金鑰不是紀錄的那把；平台的回答仍是四次，什麼都沒送出。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 假的 `ssh` 記錄 argv 與 stdin：到閘道器的只有 agent 執行檔、硬體 ID 與 handle；沒有 cookie、驗證碼、工作站憑證或私鑰、簽章標頭；遠端的清理指令有執行 | 通過 | `tests/ssh.rs` 的 `only_the_agent_the_hardware_id_and_the_handle_cross_to_the_gateway`：argv 逐字比對固定選項（含綁硬體 ID 的主機金鑰選項）與遠端指令，stdin 逐 byte 等於 agent，環境只有白名單的變數；`assert_nothing_secret_crossed` 在 argv、環境與 stdin 找 session cookie、登入驗證碼、Access 憑證、工作站憑證與私鑰的每一行、送出的簽章、nonce 與憑證標頭的值，以及這些標頭的名稱，都找不到；結束後「遠端」的暫存目錄已不存在。`the_installed_agent_runs_without_an_upload_and_sudo_never_asks`：不上傳時 stdin 為空、`sudo -n` |
| swtpm 整條流程：201、重跑 200 同序號、模擬重刷 200、`tpm2_clear` 後停下、`--allow-new-key` 時 409 `public_key_mismatch` 與固定結束碼 | 通過 | `tests/pipeline_swtpm.rs` 的 `a_gateway_registers_reruns_and_stops_after_tpm2_clear`，證據行見上：清除後停在 20（沒有建立金鑰），`--allow-new-key` 時平台回 409、工具結束碼 10 |
| ssh 連線失敗會自動重試，每次重試的 `provision` 結果相同 | 通過 | `ssh_failures_are_tried_again_and_every_run_reports_the_same`（假 ssh 在執行前與執行後失敗，每次的報告相同、只登錄一次）；swtpm 測試的第一次登錄也經過一次連線失敗與一次遺失的回應，金鑰相同；一直連不上時結束碼 23（`a_gateway_that_stays_unreachable_stops_with_23`） |
| `verify` 顯示剛登錄的事件 | 通過（測試替身） | `tests/verify.rs` 的 `verify_shows_the_registration_event_of_the_gateway` 對事件紀錄的測試替身；找不到時結束碼 24（`verify_without_an_event_ends_with_24`）；swtpm 測試在第一次登錄後也跑 `verify` |
| 硬體 ID 或 handle 含空白、分號、引號、`$(…)` 時在送出前被拒 | 通過 | `values_that_could_change_the_remote_command_are_refused_before_ssh_runs`：每個值結束碼 2、假 ssh 一次都沒被執行；另有 `ssh_options_without_gateway_are_refused` |
| `cargo test -p workstation` 與 workspace clippy 通過；README 的操作員步驟與 `--help` 一致 | 通過 | 上面的檢查；`the_readme_and_the_help_agree` 比對 README 與 `--help` 的子指令、選項與結束碼表（含 25、26、27） |

另外驗證的失敗路徑：閘道器上的 agent 失敗、上傳的 agent 雜湊不符、ssh 連線在 agent 執行中被掛斷（HUP）時結束碼 22、不採用輸出，上傳的檔案都被遠端的清理刪掉（`a_failure_on_the_gateway_stops_with_22_and_its_output_is_not_used`）；違反讀者義務的報告與超過 16 KiB 的輸出以 21 拒絕（`reports_that_break_a_reader_obligation_are_refused_with_21`）；工作站有紀錄但 TPM 沒有金鑰時停在 20（`a_known_gateway_whose_tpm_has_no_key_stops_with_20`）。

### 安全審查的修正

對 `a9478968` 的安全審查（沒有 Critical 或 High）之後加的測試，都在上面的執行裡通過：

| 項目 | 測試 | 驗證了什麼 |
| --- | --- | --- |
| M1 另一顆 TPM | `tests/ssh.rs` 的 `a_known_hardware_id_on_another_tpm_stops_with_27_even_with_allow_new_key` | 已登錄的硬體 ID，報告的 EK 不同（TPM 有金鑰、沒有金鑰，給不給 `--allow-new-key` 四種）一律結束碼 27，只執行了檢查，日誌記下回報與紀錄的 EK 雜湊，平台只收到第一次登錄，紀錄不變 |
| M1 同一顆 TPM 換了金鑰 | `a_known_tpm_with_another_key_stops_with_20`；swtpm 測試的最後一步 | EK 相同、公鑰不同時結束碼 20、只執行了檢查、什麼都沒送出；給 `--allow-new-key` 時送出、平台 409、結束碼 10，紀錄保留原本的公鑰 |
| M1 檢查不能建立金鑰 | `pipeline::tests::a_good_report_passes_and_an_absent_key_only_on_inspection` | 檢查的報告說金鑰是這次建立的，以 `CreatedOnInspection` 拒絕（結束碼 21） |
| M2 主機金鑰綁硬體 ID | `only_the_agent_the_hardware_id_and_the_handle_cross_to_the_gateway`、`an_ssh_configuration_that_skips_the_host_key_check_is_refused`、`gateway::tests` 的兩個單元測試 | 每次呼叫都帶 `HostKeyAlias=trustiot-<硬體 ID>`、工具自己的 `UserKnownHostsFile`、`GlobalKnownHostsFile=/dev/null`、`VerifyHostKeyDNS=no`，`ask` 改成 `accept-new`；`ssh -G` 顯示關掉主機金鑰檢查、`/dev/null` 或別的 known_hosts 檔、系統的 known_hosts、DNS 主機金鑰或 `KnownHostsCommand` 時拒絕執行，閘道器上什麼都沒跑；`ssh -G` 輸出超過上限時被結束 |
| L1 `verify` 的結束碼 | `tests/verify.rs` 的 `verify_tells_a_mismatched_event_and_a_missing_record_apart` | 事件與紀錄不符 25、沒有紀錄可比對 26 |
| L2 上傳的 agent 檔案 | `an_agent_file_others_could_replace_is_never_uploaded`、`files::tests::executables_writable_by_others_are_refused` | 群組或其他人可寫入時結束碼 1，ssh 沒有執行 |
| L3 只用設定的金鑰 | `only_the_agent_the_hardware_id_and_the_handle_cross_to_the_gateway` | argv 帶 `IdentitiesOnly=yes` |
| Info 1 格式字元 | `http::tests::format_characters_and_separators_are_escaped` | U+202E、U+200B、行與段落分隔字元印成跳脫 |

### 待測與延後

- **閘道器端的 ssh 限制**（強制指令的轉接程式、sudoers、工作站改送短請求）：沒有實作，是出廠映像階段的前提條件，記在 Laysi/trustiot-v2#219。在那之前 `--upload-agent` 與 `--remote-sudo` 只用在 lab 與展示用的閘道器。
- **殘餘風險：沒有證明金鑰在 TPM 裡**：沒有以 EK 做 TPM2_MakeCredential／TPM2_ActivateCredential，被控制的閘道器（或冒用位址的機器）第一次登錄時能送出不在 TPM 裡的金鑰；延後到 Laysi/trustiot-v2#756（`factory/workstation/README.md`「安全上的做法與限制」）。
- **沒有紀錄的另一台閘道器**：工作站認不出，靠操作員核對 ssh 目的地與標籤（README「硬體 ID 從標籤來」）。這是設計上的限制，不是待測項目。
- **真的 ssh 與真的閘道器**：待測。這張 issue 只用假 ssh 與 swtpm；對 lab 的閘道器登錄在 Laysi/trustiot-v2#618。
- **進階驗證**（依驗證分級延後到 Laysi/trustiot-v2#753）：
  - ssh 卡住或很慢時的期限（單次 300 秒、整體 420 秒）：程式有這兩個期限，但沒有測試讓 ssh 一直不結束、等到期限，這條路徑還沒實測。
  - 對 `services/certs` 的真實服務跑 `verify`：`services/certs/tests/workstation_tool.rs` 目前只涵蓋 Laysi/trustiot-v2#615 的登錄。
