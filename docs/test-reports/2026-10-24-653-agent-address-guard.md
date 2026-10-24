# agent 的連線位址限制：只連檢查過的解析位址

Laysi/trustiot-v2#653

## 環境

- 建置主機（x86_64 Linux，20 核），經檢查佇列執行；工具版本依 `.prototools` 與 `rust-toolchain.toml`。
- 分支 `feat/653-agent-resolved-address-guard`，實作 commit `5868db29`（之後只加了本報告）。
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

待測：在閘道器上以 systemd 執行 agent 的端到端實測（Laysi/trustiot-v2#102）還不存在；它用正式路徑，假 Modbus 要放在閘道器以外的位址，README 已寫明。
