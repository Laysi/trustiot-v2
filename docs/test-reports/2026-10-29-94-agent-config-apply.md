# 閘道器 agent 接收與套用設定（2026-10-29）

對應 issue：Laysi/trustiot-v2#94 閘道器設定接收與套用（rebuild checklist E1，閘道器這一側）。原始資料在 [data/2026-10-29-94-agent-config-apply/](data/2026-10-29-94-agent-config-apply/)。

## 環境

- 機器：建置主機（x86_64 Linux），以 podman 執行容器（Mosquitto、swtpm）。
- 工具：rustc 1.99.0（`rust-toolchain.toml`）；zig 0.16.0、cargo-zigbuild 0.23.4（`.prototools`）。
- 程式：分支 `feat/94-agent-config-apply` 的 `c87b03d1`（以 main 的 `5dad0429` 為基礎）。
- 設備：testkit 的假 Modbus TCP 端點（`testkit::modbus::FakeModbus`，127.0.0.1 的臨時埠，測試以 `exempt_for_tests` 放行 loopback；正式的位址防護不變）。broker：`desired_config` 用腳本化的 broker（`tests/support`），`binary` 用測試行程內的 WebSocket 前端與 MQTT broker。測試 CA 與憑證都在測試時產生。
- 沒有在閘道器硬體上執行，也沒有碰 lab 的閘道器。

## 指令

全部在 repo 根目錄執行。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p agent --test desired_config --test binary -- --test-threads=1
cargo test -p agent --lib desired
cargo test -p agent                         # 全部 agent 測試，含 Mosquitto 與 swtpm 容器
cargo test -p repo-check -p protocol
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
```

## 結果

全部通過（[checks.txt](data/2026-10-29-94-agent-config-apply/checks.txt)，去掉了編譯進度的行）：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p agent --test desired_config` | 7 個通過 |
| `cargo test -p agent --test binary` | 7 個通過 |
| `cargo test -p agent --lib desired` | 6 個通過（`store` 的單元測試） |
| `cargo test -p agent` | 全部通過（lib 169、binary 7、broker 4、desired_config 7、link 51、transport 19、systemd_unit 6、tpm_swtpm 1 等） |
| `cargo test -p repo-check -p protocol` | 全部通過 |
| aarch64 的 `trustiot-agent`（zigbuild，glibc） | 建置成功 |

各測試驗證的內容：

| 測試 | 驗證 |
| --- | --- |
| `desired_config::a_removed_device_is_no_longer_polled_and_its_state_is_cleared` | v2 移除兩台設備：回報 `applied` 之前已呼叫 `Sink::removed`；之後三輪輪詢的請求只問留下的設備（unit 1）；只有被移除設備用的端點，連線關閉、不再收到請求 |
| `desired_config::a_device_removed_during_its_request_is_stopped_after_the_answer` | 假端點扣住被移除設備的回應：套用在回應放行之前不會結束；放行後，該設備最後一次輪詢（若有）讀到自己的值且在移除之前；同一個連線上的另一台設備之後三輪都讀到自己的值，端點自始至終只有一條連線 |
| `desired_config::a_change_leaves_the_other_devices_polling_untouched` | v2 改一台的輪詢間隔、加一台：沒變的設備 `scheduled` 照原節拍前進、`missed` 為 0、每輪都讀到自己的值、沒有 watchdog 事件；端點的連線沒有換過；沒有任何設備被清掉 |
| `desired_config::an_invalid_configuration_is_rejected_with_its_issues_and_the_previous_one_stays` | 型號不存在的 v2：`rejected`、`active_version` 1、issue `unknown_model` 在 `devices[0].model`；解不開的 v3：`rejected`、`decode_failed`；之後 v1 仍是使用中的版本、存檔仍是 v1、照節拍輪詢 |
| `desired_config::versions_follow_the_protocol_rules` | 空 payload、讀不到版本：不回報、不變；同版本：`applied`；同版本但解不開：`rejected`、使用中版本不變；較舊：`stale` |
| `desired_config::after_a_restart_without_network_the_saved_configuration_is_polled` | 套用後停掉，在同一個狀態目錄以永遠連不上的 link 重新啟動：立刻以存下的版本輪詢 |
| `desired_config::after_a_restart_the_gateway_converges_to_the_latest_version` | 重開後先以存下的 v1 輪詢；連上後 broker 送 retained v2：套用、存檔、回報 `applied` v2；再次重連重送 v2 只回報、不重套；再離線重開，輪詢的是 v2 |
| `binary::connected_the_agent_applies_its_configuration_reports_it_and_polls_it_after_a_restart` | 真的執行檔：只訂閱 `config`；收到 retained v1 後回 PUBACK、送 `applied` v1，狀態目錄（`STATE_DIRECTORY`）的 `desired-config.cbor` 等於收到的 payload；SIGTERM 正常結束；在沒有 broker 時重新啟動，日誌顯示以存下的 v1 輪詢（設備位址 127.0.0.1 被位址防護拒絕，不送任何封包） |
| `store` 單元測試 | 原子存檔（0600、不留暫存檔）、存檔中斷電留下的暫存檔被忽略、超過 1 MiB 的檔案拒絕、`STATE_DIRECTORY` 的解讀 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 斷電重開後收斂到最新版本（測試） | 通過：`after_a_restart_the_gateway_converges_to_the_latest_version`、`binary` 的重開。「重開」是在同一個狀態目錄重新啟動 agent（設定這一側，或整個執行檔），與斷電後新行程看到的磁碟內容相同；存檔途中斷電由 `store` 的單元測試驗證。在閘道器硬體上真的斷電重開：待測，不在本 issue 範圍 |
| 移除的設備不再被輪詢（測試） | 通過：`a_removed_device_is_no_longer_polled_and_its_state_is_cleared`、`a_device_removed_during_its_request_is_stopped_after_the_answer` |
| 設定變更後其他設備的最新值與在線狀態不變（測試） | 通過：`a_change_leaves_the_other_devices_polling_untouched` |
| 無效設定被拒、回報錯誤且維持上一版（測試） | 通過：`an_invalid_configuration_is_rejected_with_its_issues_and_the_previous_one_stays`、`versions_follow_the_protocol_rules` |
| 斷網狀態下重開機，閘道器以上一版設定繼續採集（測試） | 通過：`after_a_restart_without_network_the_saved_configuration_is_polled`、`binary` 的離線重開 |
