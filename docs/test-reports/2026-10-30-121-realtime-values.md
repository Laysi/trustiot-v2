# 閘道器即時送值（2026-10-30）

對應 issue：Laysi/trustiot-v2#121 閘道器即時送值：即時 topic、不進緩衝、不等確認（rebuild checklist I3，閘道器端）。原始資料在 [data/2026-10-30-121-realtime-values/](data/2026-10-30-121-realtime-values/)。

## 環境

- 機器：建置主機（x86_64 Linux），以 podman 執行容器（Mosquitto、swtpm）。
- 工具：rustc 1.99.0（`rust-toolchain.toml`）；zig 0.16.0、cargo-zigbuild 0.23.4（`.prototools`）。
- 程式：分支 `feat/121-realtime-values`（以 main 的 `e4994a25` 為基礎）的工作樹；第二輪在修正「租約快速開關」之後，第三輪在定案的規則（新的即時節拍最早在這台設備上一次輪詢開始後一個間隔）之後。
- 設備：testkit 的假 Modbus TCP 端點（`testkit::modbus::FakeModbus`）。雲端：`tests/live.rs` 用腳本化的 broker（`tests/support`），收發的是 agent 真的訂閱、分派、輪詢與發布；`crates/protocol/tests/broker.rs` 用真的 Mosquitto 與 dynsec。時間：tokio 的停住時鐘，加上 `testkit::clock::manual()`。
- 沒有在閘道器硬體上執行，也沒有碰 lab 的閘道器。

## 指令

全部在 repo 根目錄執行。

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --exclude agent
cargo test --locked -p agent                      # 含 Mosquitto 與 swtpm 容器的測試
cargo test --locked -p agent --lib polling
cargo test --locked -p agent --test live --test upload --test desired_config --test binary
cargo test --locked -p protocol --test broker     # 已含在 --workspace 裡
cargo build --locked --target aarch64-unknown-linux-gnu -p protocol
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
cargo run -p openapi -- --check
```

## 結果

全部通過（[checks.txt](data/2026-10-30-121-realtime-values/checks.txt)，去掉了編譯進度的行）：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test --workspace --exclude agent` | 全部通過，0 個失敗（含 `protocol` 的 `broker`、`lab-dataset`） |
| `cargo test -p agent` | 全部通過（lib 355、binary 14、broker 5、desired_config 8、link 56、live 2、upload 18、transport 19 等） |
| `cargo test -p agent --lib`（第三輪） | 357 個通過，含 `live` 與 `polling` 的新測試 |
| `cargo test -p agent --test live --test upload --test desired_config --test binary --test broker`（第三輪） | 2、18、8、14、5 個通過；`--test live` 再連跑 5 次都通過 |
| `cargo clippy -p agent --all-targets -- -D warnings`（第三輪） | 通過 |
| aarch64 的 `protocol` 與 `trustiot-agent`（zigbuild，glibc） | 建置成功；唯一的警告是 Zig 連結器關於最佳化參數的訊息，與本次變更無關 |
| `cargo run -p openapi -- --check` | `api/openapi.json` 沒有變 |

各測試驗證的內容：

| 測試 | 驗證 |
| --- | --- |
| `live::a_leased_devices_samples_go_up_at_once_outside_the_buffer_until_the_lease_ends` | 第一輪輪詢後 1 秒，雲端在 `down/live-control` 開 3 秒的租約：時鐘沒有前進，閘道器就輪詢 meter-a，把讀值發布到 `up/live`（QoS 0、不 retain、沒有 packet ID）；收集視窗的筆數不變；租約期間每秒一筆（`regular` 為 false）；3 秒到期後沒有任何多做的輪詢與 `up/live`，下一次輪詢是原本排定的 6 秒那次（`regular`），進收集視窗；之後的遙測批次只有排定的輪詢的讀值 |
| `live::a_renewal_extends_the_lease_and_stop_ends_it_at_once` | 1 秒時開始的 2 秒租約在 2 秒時以 `renew` 延成 10 秒，3 秒時仍有即時輪詢；`stop` 後租約立刻不再涵蓋，沒有多做的輪詢；之後的遙測批次只有排定的輪詢 |
| `polling::a_lease_adds_polls_at_its_interval_and_the_regular_ones_keep_their_rate` | 輪詢間隔 1 秒、2.5 秒時開了 200 ms 的租約、6.25 秒時結束：排定的輪詢每秒一次（租約期間由第一個跨過該秒的即時輪詢擔任），沒有錯過；多做的輪詢從 2.5 秒起每 200 ms 一次，租約結束後就停 |
| `polling::a_live_poll_across_a_regular_due_time_is_its_regular_poll` | 輪詢要 150 ms、即時間隔 300 ms：跨過排定時間的即時輪詢算成那個時間的排定輪詢，`missed` 全為 0 |
| `polling::a_lease_no_faster_than_the_poll_interval_adds_no_poll` | 租約的間隔不比輪詢間隔短：不多輪詢 |
| `polling::leases_that_come_and_go_quickly_do_not_poll_faster_than_their_interval` | 租約每 10 ms 在 200 ms、停、150 ms、100 ms、停之間切換，持續 1 秒：多做的輪詢彼此至少相隔 100 ms、最多 11 次；排定的輪詢每秒一次，沒有錯過 |
| `polling::a_new_lease_waits_one_interval_after_the_last_poll` | 1050 ms 時開始 200 ms 的租約（之前沒有租約，或有一個不比 1 秒輪詢間隔快的 2 秒租約）：第一個多做的輪詢在 1200 ms，也就是 1 秒那次輪詢之後一個間隔，不是 1050 ms；所以在輪詢途中開始的租約讓那次輪詢成為即時值時，下一筆即時值也不會太近 |
| `live` 單元測試（`src/live.rs`） | 租約涵蓋到結束的那一刻為止；`renew`、`stop`、多個租約取最短的間隔；不認得的 `op`；最多 64 個還沒結束的租約（已保留的照常 `renew`、`stop`，到期的不佔名額）；解不開的訊息；輪詢迴圈在租約變化時醒來；值不能送的讀值（NaN）不送 |
| `upload` 與 `desired_config` 的既有測試 | 加了即時值之後，上傳、確認、離線緩衝與設定套用的行為不變 |
| `binary` | 真的執行檔訂閱的清單多了 `down/live-control` |
| `protocol` 的 `broker::the_gateway_role_allows_exactly_the_protocol_channels`（真的 Mosquitto 與 dynsec） | 閘道器 gw-a 發布到 gw-b 的每個上行 topic（含 `up/live`）：QoS 1 得到 PUBACK 0x87，QoS 0 的雲端訂閱者（`gw/+/up/#`）收不到；訂閱 gw-b 的 `up/live` 得到 SUBACK 0x87，同一個 SUBSCRIBE 裡自己的 `down/live-control` 得到 QoS 1 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 即時值在取樣後立即送出且不進入緩衝（測試） | 通過：`a_leased_devices_samples_go_up_at_once_outside_the_buffer_until_the_lease_ends` |
| 租約到期後停止送出（測試） | 通過：同上的 `ttl_s` 到期、`a_renewal_extends_the_lease_and_stop_ends_it_at_once`、`src/live.rs` 的單元測試 |
| 只能發到自己的即時 topic（範圍） | 通過：`broker::the_gateway_role_allows_exactly_the_protocol_channels` |
| 在閘道器硬體上、經真的 broker 送到雲端 | 待測：雲端轉送是 Laysi/trustiot-v2#120，硬體上的端到端驗證不在本 issue 範圍 |
| 多做的輪詢對 RS485 匯流排的負載 | 待測：需要 lab 的閘道器與實際的轉換器；限制已寫進 `edge/agent/README.md`「即時值」 |
