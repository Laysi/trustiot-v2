# agent 的連線位址限制：只連檢查過的解析位址

Laysi/trustiot-v2#653

## 環境

- 建置主機（x86_64 Linux）；工具版本依 `.prototools` 與 `rust-toolchain.toml`。
- 分支 `feat/653-agent-resolved-address-guard`。下表的檢查與突變測試在實作 commit `5868db29`（以 main 的 `0dc8fd96` 為基底）上執行；rebase 到 main 的 `1ad0ffc5` 之後（實作 commit 變成 `2fd10473`，另加 `services/assets` 的文件與註解更新）重跑 workspace clippy、aarch64 建置、agent、repo-check 與 assets 的單元測試，全部通過（[after-rebase.txt](data/2026-10-24-653-agent-address-guard/after-rebase.txt)）。
- 全部是單元與整合測試：假 Modbus 端點（`crates/testkit` 的 `FakeModbus`）與測試自己的 listener 都在執行測試的機器上；沒有用到任何閘道器。
- 位址限制的單元測試以替身解析器與撥號紀錄（`src/modbus/address.rs` 的 `hooks`，只在 `#[cfg(test)]`）驗證查了幾次、撥了哪些位址；替身只真的撥 loopback，所以 169.254/16 與私有位址的測試不會對建置主機所在的網路送封包。

## 指令

從 worktree 根目錄：

```sh
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p agent --lib -- -D warnings          # 沒有 dev-dependency，也就沒有 test-exempt-addresses
cargo build --locked --target aarch64-unknown-linux-gnu -p agent
cargo tree --locked -p agent -e features,normal -i agent      # 正式建置的 feature
cargo tree --locked -p agent -e features,normal,dev -i agent  # 含 dev-dependency 的 feature
cargo test --locked -p agent
cargo test --locked -p repo-check
cargo deny --locked check
cargo run --locked -q -p openapi -- --check
```

另做兩個突變測試（改完跑測試、再還原）：讓守門員放行所有位址，以及丟掉檢查過的位址、撥號前再解析一次名稱。

## 結果

| 檢查 | 結果 | 原始資料 |
| --- | --- | --- |
| workspace clippy（`-D warnings`） | 通過 | [checks.txt](data/2026-10-24-653-agent-address-guard/checks.txt) |
| agent lib clippy（沒有測試 feature） | 通過 | 同上 |
| aarch64 建置 | 通過 | 同上 |
| 正式建置的 feature | 只有 `default`；`test-exempt-addresses` 只經 `[dev-dependencies]` 出現 | 同上 |
| `cargo test -p agent` | 109 個全部通過（lib 74、`modbus_address_guard` 1、`modbus_fake_endpoint` 9、`modbus_rtu_converter` 8、`modbus_rtu_framing` 6、`modbus_tcp_framing` 8、`polling_recovery` 3） | [agent-tests.txt](data/2026-10-24-653-agent-address-guard/agent-tests.txt) |
| `cargo test -p repo-check` | 通過，含新的 `test_only_features_are_only_enabled_by_dev_dependencies` 與它的單元測試 | 同上、checks.txt |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都 ok（`libc` 早已在 lockfile 與 `[workspace.dependencies]`） | checks.txt |
| openapi | 沒有變化 | checks.txt |
| 突變：放行所有位址 | `refused_hosts_reach_nothing`、`refused_hosts_are_never_dialed`、`only_the_allowed_addresses_of_a_name_are_dialed`、`a_name_is_resolved_once_and_its_checked_address_dialed`、`hosts_on_the_gateway_itself_reach_nothing` 失敗 | [mutations.txt](data/2026-10-24-653-agent-address-guard/mutations.txt) |
| 突變：撥號前再解析一次 | `a_name_is_resolved_once_and_its_checked_address_dialed`、`only_the_allowed_addresses_of_a_name_are_dialed`、`link_local_and_private_addresses_are_dialed` 失敗 | 同上 |

各測試驗證什麼：

- `address::tests::refused_addresses`、`field_networks_are_allowed`、`interfaces_hold_their_addresses_and_subnet_broadcasts`：每一類拒絕的位址（含 IPv4-mapped 寫法、閘道器自己的位址與網段廣播），以及允許的 APIPA、私有位址、ULA、link-local、/31 的對端。
- `tcp::tests::refused_hosts_reach_nothing`：以正式的 `TcpConnection::new` 與系統解析器，對 `127.0.0.1`、`127.0.0.53`、`::1`、`::ffff:127.0.0.1`、`0.0.0.0`、`::`、`::ffff:0.0.0.0`、`localhost`、`224.0.0.1`、`ff02::1`、`255.255.255.255`、`169.254.169.254`、`::ffff:169.254.169.254`、`fd00:ec2::254`，以及執行機器每個介面位址與每個 IPv4 網段的廣播位址讀取：每個都是 `connect` 失敗、說明以 refusal 開頭；一個聽在所有位址（IPv6 雙協定）的 listener 一條連線都沒收到。
- `tcp::tests::refused_hosts_are_never_dialed`：同一組主機，每個只查一次名稱，撥號紀錄是空的。
- `tcp::tests::only_the_allowed_addresses_of_a_name_are_dialed`：名稱解析成 `127.0.0.2`、`169.254.169.254`、`::1`、`224.0.0.1`、`127.0.0.1`（只有最後一個被測試放行）時，只撥 `127.0.0.1`，讀到值。
- `tcp::tests::a_name_is_resolved_once_and_its_checked_address_dialed`：解析器第一次回允許的位址、之後回被拒絕的位址：第一次連線只查一次、撥的就是那個位址；重連時重新解析，新的位址被拒絕、沒有撥號。
- `tcp::tests::link_local_and_private_addresses_are_dialed`：`169.254.1.20`、`169.254.169.253`、10/8、172.16/12、192.168/16、ULA 與 IPv4-mapped 的私有位址都依序撥號（IPv4-mapped 的以 IPv4 撥）。
- `tests/modbus_address_guard.rs`：經 `Endpoints::new`（agent 輪詢走的路徑）主機為 `127.0.0.1`、`127.0.0.2`、`::1`、`::ffff:127.0.0.1`、`localhost`、`169.254.169.254`、`fd00:ec2::254` 的設備，輪詢的資料點都以 `connect` 失敗；`TcpConnection::new` 對設定驗證會擋的 `0.0.0.0`、`::`、`255.255.255.255`、`224.0.0.1`、`ff02::1` 也拒絕。之後放行 127.0.0.1 的對照組讀到值，而且是假端點接受的第一條連線、第一個請求。
- 既有的 agent 測試（假端點都在 127.0.0.1）以 `exempt_for_tests` 放行 127.0.0.1 後全部照常通過。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 主機解析成被拒絕的位址時不建立 TCP 連線，讀取以 `connect` 類失敗回報 | 通過：`refused_hosts_reach_nothing`、`refused_hosts_are_never_dialed`、`hosts_on_the_gateway_itself_reach_nothing` |
| 主機解析成多個位址時只連允許的位址 | 通過：`only_the_allowed_addresses_of_a_name_are_dialed` |
| 連到的是檢查過的解析位址，名稱只解析一次 | 通過：`a_name_is_resolved_once_and_its_checked_address_dialed`；突變 2 讓它失敗 |
| 169.254/16（metadata 除外）與私有位址照常可以連 | 通過：`field_networks_are_allowed`、`link_local_and_private_addresses_are_dialed` |
| 正式設定路徑無法關掉限制；測試開關只在測試程式碼可及之處 | 通過：`exempt_for_tests` 只在 `#[cfg(test)]` 或 `test-exempt-addresses` feature 下存在，正式建置的 feature 樹沒有它（`cargo tree`），`repo-check` 在任何正式依賴、其他 feature 或 `default` 打開它時失敗；agent 目前沒有設定檔、環境變數或 MQTT 設定的讀取路徑，`Endpoints::new`／`TcpConnection::new` 沒有任何參數能放行位址 |
| `cargo test -p agent` 與 aarch64 建置通過 | 通過 |

## 2026-10-27：與 main 合併之後

main 合併進分支兩次，都以 signed merge，沒有 rebase：

- `304de010`（合併 commit `47959cf5`）帶進 `tools/lab-dataset`（Laysi/trustiot-v2#685）。它的錄製工具在 `Recorder::run` 裡自己建 `Endpoints::new`，測試 `tests/record.rs` 的假 RS485 轉換器聽在 127.0.0.1，合併後被守門員拒絕：輪詢一直連不上而進入退避，測試在等第 2 秒的輪詢時逾時失敗（下表第一列）。
- 修正（commit `d778f9b3`）：`Recorder::run` 改由呼叫端傳入 `Endpoints`。指令傳 `Recorder::endpoints()`，也就是正式路徑；測試傳同一組 endpoints 加上 `exempt_for_tests([127.0.0.1])`，這個方法因 lab-dataset 的 `[dev-dependencies]` 打開 agent 的 `test-exempt-addresses` 才存在。lab-dataset 自己的程式沒有測試用的 feature 或 `cfg`，所以 `cargo build -p lab-dataset` 建出的指令放行不了任何位址；`repo-check` 的 `test_only_feature_uses` 本來就允許 dev-dependency 打開它。
- `509c3f1f`（Laysi/trustiot-v2#574，合併 commit `2e224402`）帶進 agent 服務本身（設定、MQTT、TLS、TPM、`main.rs`）。衝突在 `Cargo.lock`、`edge/agent/Cargo.toml` 與 `edge/agent/README.md`，兩邊都保留；`Cargo.lock` 以 main 的為底由 cargo 重新產生，只多了 agent 的 `agent`（自我 dev-dependency）與 `libc` 兩條。#574 的程式沒有建立 Modbus 連線：`main.rs` 還不輪詢設備，新的對外連線只有 `transport.rs` 連 MQTT broker，位址來自閘道器本機的設定檔，不是租戶設定，所以不需要守門員。

| 檢查（合併 commit `2e224402` 的樹，`CI=1`） | 結果 | 原始資料 |
| --- | --- | --- |
| `cargo test -p lab-dataset --test record`，合併 `304de010` 後、修正前 | 失敗（預期）：`timed out after 60s waiting for 5 polls by second 2` | [lab-dataset-record.txt](data/2026-10-24-653-agent-address-guard/lab-dataset-record.txt) |
| 同一個測試，修正後 | 通過 | 同上 |
| `cargo fmt --all --check` | 通過 | [after-merge-574.txt](data/2026-10-24-653-agent-address-guard/after-merge-574.txt) |
| `cargo test -p agent` | 291 個全部通過（lib 162、`main.rs` 1、`binary` 7、`broker` 4、`dependencies` 1、`link` 51、`liveness` 4、`modbus_address_guard` 1、`modbus_fake_endpoint` 9、`modbus_rtu_converter` 8、`modbus_rtu_framing` 6、`modbus_tcp_framing` 8、`polling_recovery` 3、`systemd_unit` 6、`tpm_swtpm` 1、`transport` 19）；`dependencies` 確認自我 dev-dependency 與 `libc` 沒有帶進禁止的 crate | 同上 |
| `cargo test -p lab-dataset` | 53 個全部通過（lib 37、`anomalies` 4、`clickhouse_load` 1、`record` 1、`rollups` 1、`synthetic_fixture` 8、doctest 1） | 同上 |
| `cargo test -p repo-check` | 28 個全部通過（lib 18、`repository` 10），含 `test_only_feature_uses` | 同上 |
| workspace clippy（`-D warnings`） | 通過 | 同上 |
| `cargo build --target aarch64-unknown-linux-gnu -p agent` | 失敗：自 #574 起 agent 依賴 ring，ring 要用目標平台的 C 編譯器，建置主機沒有 `aarch64-linux-gnu-gcc`；與本分支無關 | 同上 |
| `cargo zigbuild --target aarch64-unknown-linux-gnu -p agent`（CI 與 CLAUDE.md 的做法） | 通過 | 同上 |

待測：在閘道器上以 systemd 執行 agent 的端到端實測（Laysi/trustiot-v2#102）還不存在；它用正式路徑，假 Modbus 要放在閘道器以外的位址，README 已寫明。
