# agent 沒有有效身分時不運作、不收密碼（D5）

Laysi/trustiot-v2#812

## 環境

- 建置主機（x86_64 Linux），Rust 1.99.0（`rust-toolchain.toml`），分支 `test/812-agent-fail-closed`。
- 全部在行程內與 loopback 上：真的 `trustiot-agent` 執行檔、測試自己扮演 systemd 的通知 socket（abstract Unix socket）、一個會接受連線的 loopback port 站在 broker 前端的位置。沒有碰任何閘道器或叢集。
- 測試用的 CA、憑證與金鑰都在測試時產生（`testkit::mqtt::TestPki`），不進 repo。

## 指令

```sh
cargo test --locked -p agent --test binary --test secret_inputs --test systemd_unit
cargo test --locked -p agent --test binary without_a_valid_identity -- --nocapture
```

## 結果

### 既有的涵蓋（本 issue 之前就有）

| 行為 | 測試 |
| --- | --- |
| `--check` 拒絕別張憑證的私鑰 | `tests/binary.rs` `check_accepts_a_working_configuration_and_refuses_a_key_of_another_certificate` |
| `--check` 拒絕 `broker.ca` 沒簽的憑證 | `tests/binary.rs` `check_refuses_a_certificate_the_broker_ca_did_not_issue` |
| `--check` 拒絕不是根簽的根 CRL | `tests/binary.rs` `check_reports_the_root_crl_and_refuses_one_the_root_did_not_sign` |
| 過期憑證只警告、照常啟動（刻意的例外） | `tests/binary.rs` `an_expired_certificate_only_warns_because_the_clock_may_be_wrong` |
| 有效身分時先送 `READY=1`、不等連線 | `tests/binary.rs` `without_a_broker_the_agent_is_ready_feeds_the_watchdog_and_stops_on_sigterm` |
| 設了密碼的 TPM 金鑰在開啟時失敗（`TPM_RC_BAD_AUTH`） | `tests/tpm_swtpm.rs` `the_tpm_key_signs_and_completes_an_mtls_handshake_and_unusable_keys_are_refused` |
| unit 的 `ExecStart` 只有 `--config <路徑>`、`Environment=` 只有 `MALLOC_MMAP_THRESHOLD_` | `tests/systemd_unit.rs` `systemd_learns_readiness_and_liveness_from_the_agent_and_always_restarts_it`、`the_sandbox_lines_stay` |

缺的是：服務路徑（不是 `--check`）在身分無效時的行為，特別是狀態目錄已有 `desired-config.cbor` 時；以及「不收密碼」在參數、設定檔、環境變數上的證明。

### 本 issue 新增

`tests/binary.rs` `without_a_valid_identity_the_agent_never_gets_ready_connects_or_polls_a_saved_configuration`：每種情況都在狀態目錄放一份上次套用的設備設定，以服務的方式（`NOTIFY_SOCKET`、`STATE_DIRECTORY`、`WATCHDOG_USEC`）啟動 agent。五種情況的日誌（2026-10-31 在建置主機上以暫時加上的 `eprintln!` 印出，不在提交的測試裡；略去時間戳，路徑是測試的暫存目錄）：

```text
CASE no configuration file: ERROR trustiot_agent: refusing to start: reading /tmp/testkit-agent-transport-3599650-0/agent.toml: No such file or directory (os error 2)
CASE no certificate: ERROR trustiot_agent: refusing to start: /tmp/testkit-agent-transport-3599650-1/agent.toml: identity.certificate / identity.key: reading the certificate file /tmp/testkit-agent-transport-3599650-1/gateway.pem: I/O error: No such file or directory (os error 2)
CASE no key: ERROR trustiot_agent: refusing to start: /tmp/testkit-agent-transport-3599650-2/agent.toml: identity.certificate / identity.key: reading the private key file /tmp/testkit-agent-transport-3599650-2/gateway.key: I/O error: No such file or directory (os error 2)
CASE a certificate the broker CA did not issue: ERROR trustiot_agent: refusing to start: /tmp/testkit-agent-transport-3599650-3/agent.toml: broker.ca: the gateway certificate does not pass as a client certificate of /tmp/testkit-agent-transport-3599650-3/broker-ca.pem: invalid peer certificate: UnknownIssuer
CASE a key of another certificate: ERROR trustiot_agent: refusing to start: /tmp/testkit-agent-transport-3599650-4/agent.toml: identity.certificate / identity.key: the private key does not belong to the certificate
test without_a_valid_identity_the_agent_never_gets_ready_connects_or_polls_a_saved_configuration ... ok
```

每種情況都斷言：結束碼 1；日誌有 `refusing to start` 與該情況的原因；日誌沒有 `polling with the saved configuration`、`applied the configuration`、`polled`；通知 socket 什麼都沒收到（沒有 `READY=1`，也沒有 `STATUS=`）；broker 的 port 沒有任何連線；`desired-config.cbor` 不變；離線緩衝的目錄沒有被建立。對照組（同一組檔案、身分完整）會送 `READY=1`、記 `polling with the saved configuration version=1`、連到那個 port，證明上面的「沒有」不是測試本身看不到。

`tests/secret_inputs.rs`（5 個測試）：

- `no_command_line_takes_a_pin_or_password`：`trustiot-agent`、`provision`、`provision install` 的 `--help` 列出的參數都不含 pin、pass、auth、secret；`--pin`、`--password`、`--passphrase`、`--auth`、`--key-auth`、`--tpm-pin`、`--secret` 都以結束碼 2、`unknown argument` 拒絕。
- `the_configuration_has_no_field_for_a_key_password`：`agent.example.toml` 的 TPM 金鑰加上 `pin`、`password`、`auth`、`auth_value`、`passphrase` 任一欄位（在 key 的表裡或 `[identity]` 裡）都以 `unknown field` 拒絕。
- `the_agent_reads_only_the_listed_environment_variables_and_starts_no_process`：以 token 掃描 `src/` 的正式程式碼（略過整行註解與字串內容），讀的環境變數剛好是 `JOURNAL_STREAM`、`NOTIFY_SOCKET`、`TRUSTIOT_AGENT_LOG`、`WATCHDOG_PID`、`WATCHDOG_USEC`、`STATE_DIRECTORY`；`std::env` 與 `std::process` 只用到 `var`、`var_os`、`args_os`、`ExitCode`、`id`（`env::` 的項目都當場呼叫，不 import、不改名）；libc 只用到檔案、帳號與介面的呼叫（允許清單）；沒有讀整個環境、沒有 `Command`、沒有 `extern` 區塊或組合語言。
- `the_scan_catches_the_ways_around_it_it_knows`：已知的繞法（單獨 import 的 `var`、分組 import、`std`／模組／函式的別名、`extern crate`、`libc` 的 glob 或分組 import、`execveat`、`syscall`、`extern "C"`、`CommandExt`、`asm!`、raw string）都被抓到，agent 實際的寫法不會。
- `the_scan_finds_variable_reads`：掃描本身找得到呼叫。

`tests/systemd_unit.rs` `the_unit_passes_no_secret_on_the_command_line_or_in_the_environment`：`ExecStart` 只有設定檔路徑、`Environment=` 只有 `MALLOC_MMAP_THRESHOLD_`，沒有 `EnvironmentFile`、`PassEnvironment`、任何 credential 設定或其他 `Exec*`；sysusers 只有帳號那一行。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 沒有設定檔或憑證、鏈驗不到 `broker.ca`、私鑰不符時：不送 READY=1、以非 0 結束、不連 broker、不輪詢設備，狀態目錄已有設備設定時也一樣 | 通過（`without_a_valid_identity_...`，五種情況加對照組） |
| PIN 或密碼不出現在命令列參數或環境變數：agent 不接受、unit 不設定 | 通過（`tests/secret_inputs.rs`、`the_unit_passes_no_secret_...`；設了密碼的 TPM 金鑰被拒在 `tests/tpm_swtpm.rs`） |
| `edge/agent/README.md` 寫明 fail-closed 的條件與過期憑證的例外 | 通過（「執行檔與設定」下的「沒有完成出廠登錄或沒有有效憑證就不運作（fail-closed）」） |

沒有在閘道器的實體 TPM 與真的 systemd 上重跑：屬於進階驗證（另一個環境的同一檢查），待測。
