# 閘道器 dynsec 角色改為精確發布規則（2026-10-11）

對應 issue：Laysi/trustiot-v2#268（驗證報告 [2026-10-11-50-verify](2026-10-11-50-verify.md) 的 F3）。

`crates/protocol` 產生的閘道器角色 `trustiot-gateway` 原本以 `publishClientSend trustiot/v1/gw/%u/up/#` 允許發布，閘道器能在自己的 `up/` 下任意新增 topic 並留下 retained 訊息。修正後每個 `up` channel 一條精確規則（目前 7 條：telemetry、config-report、status、heartbeat、live、log、log-dump），`down` 的訂閱與接收規則不變。retained 的決定寫在 `crates/protocol/README.md` 的 ACL 一節。

## 環境

- 本機 x86_64 Linux（Fedora，kernel 7.2.8），rustc 1.99.0，podman 6.1.3；機器同時有其他 agent 在跑（load average 約 28），所以容器測試的耗時偏長。
- Mosquitto：`docker.io/library/eclipse-mosquitto@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408`（2.1.2-alpine 的 multi-arch index，與 `scripts/lib/stack.ts` 相同；本機解析出的映像 id 為 `8c4aa03990b1`，與驗證報告當時用的映像相同）。測試以 dynsec plugin 啟動 broker，listener 只開在容器內的 127.0.0.1，admin 密碼為每次隨機產生、經環境變數傳入；容器記憶體上限 128 MB，測完移除。
- 基準 commit：`a4bfe2b`（main）加上本分支 `fix/268-gateway-acl-exact` 的變更。

## 指令

```sh
# 全部 protocol 測試，含真 broker 的 tests/broker.rs（需要 podman 或 docker）
CARGO_BUILD_JOBS=2 cargo test -p protocol
# 只跑真 broker 測試
CARGO_BUILD_JOBS=2 cargo test -p protocol --test broker
# 閘道器目標建置與相依套件白名單
CARGO_BUILD_JOBS=2 cargo build --locked --target aarch64-unknown-linux-gnu -p protocol
CARGO_BUILD_JOBS=2 cargo test -p protocol --test dependencies
# 沒有容器執行環境時：本機略過、CI 失敗（把 PATH 指到空目錄模擬）
env -i PATH=/empty target/debug/deps/broker-<hash>
env -i CI=1 PATH=/empty target/debug/deps/broker-<hash>
```

突變測試（證明舊規則會讓同一組測試失敗）：暫時把 `gateway_role_acls()` 改回修正前的規則（只有一條 `publishClientSend trustiot/v1/gw/%u/up/#`，加上不變的 `down` 規則），執行 `cargo test --no-fail-fast -p protocol --lib --test broker --test readme --test adversarial`，再把程式還原。README 也做過一次突變：在 ACL 表多加一列 `up/#`，`cargo test -p protocol --test readme` 失敗，還原後通過。

## 結果

原始資料：[`data/2026-10-11-268-gateway-acl-exact/`](data/2026-10-11-268-gateway-acl-exact/)

| 檔案 | 內容 |
| --- | --- |
| [`cargo-test-protocol.log`](data/2026-10-11-268-gateway-acl-exact/cargo-test-protocol.log) | 修正後 `cargo test -p protocol`：全部通過，`tests/broker.rs` 2 項通過（本機負載高時約 80–90 秒） |
| [`mutation-legacy-rule.log`](data/2026-10-11-268-gateway-acl-exact/mutation-legacy-rule.log) | 角色改回 `up/#` 後的同一組測試：單元測試 3 項、`broker` 1 項、`readme` 1 項失敗 |

真 broker 測試（`crates/protocol/tests/broker.rs`）的檢查與結果：

| 檢查 | 修正後 | 修正前的 `up/#` 規則 |
| --- | --- | --- |
| 以 `gateway_role_acls()` 的 JSON 送 `createRole`，`getRole` 讀回的規則與產生的完全相同 | 通過 | 通過 |
| 7 個 `up` channel 依 `Channel::delivery` 的 QoS 與 retain 發布，雲端訂閱者（`--retain-as-published`）收到的 topic、QoS、retain 與長度完全符合 | 通過 | 通過（舊規則同樣允許，突變紀錄中主測試在下一項才失敗） |
| `up/` 下 10 個非協定 topic（`up`、`up/`、`up/spam`、`up/spam/1`、`up/telemetry/extra`、`up/telemetry/`、`up/Telemetry`、`up/telemetry-ack`、`up/config`、`up//status`），以 QoS 1、retain、1 KiB 發布 | 每個都得到 PUBACK 0x87，雲端收不到，broker 不留 retained | 每個都 PUBACK 0x00、雲端收到、留下 retained（30 項違規，`the_wildcard_rule_before_the_fix_fails_the_same_check` 斷言全部出現；主測試在此失敗） |
| 允許的 `up/telemetry` 設 retain 發布 | broker 接受並保存（dynsec 不看 retain 旗標，與 README 的決定一致），測試隨即以空 payload 清除；之後 `up/` 下只剩 `up/status` 有 retained | — |
| 閘道器逐一訂閱 4 個 `down` channel，雲端依 `Channel::delivery` 發布 | 4 個都授權並送達 | — |
| 閘道器帶 `up/status` 的 LWT（retain、QoS 1）連線後被 `kill -9` | 雲端收到 LWT，retain 為 1 | — |
| 閘道器帶 `up/spam` 的 LWT 連線後被 `kill -9` | 雲端收不到，broker 不留 retained | — |
| 遷移以舊規則建立的角色：`modifyRole` 帶 `acls: gateway_role_acls()`，`getRole` 確認，再以空 payload 的 retained 清掉殘留，重跑非協定 topic 檢查 | — | `modifyRole` 換掉整份 ACL（`up/#` 消失），之後檢查 0 項違規 |

單元與一致性測試：

- `acl::tests::publish_rules_are_exact_one_per_up_channel`：發布規則數等於 `up` channel 數、順序與 `Channel::ALL` 相同、不含 `+` 或 `#`；對非協定 topic，`filter_matches` 與 `filter_covers` 都回 false。
- `acl::tests::the_wildcard_rule_before_the_fix_fails_the_same_check`：同一組非協定 topic 對 `up/#` 都會比對成功。
- `acl::tests::rules_serialize_for_dynamic_security`：產生的 `createRole` JSON 與預期完全相同（10 條規則）。
- `tests/readme.rs` 的 `the_readme_lists_the_gateway_acl`：README 的 ACL 表必須與產生的規則逐列、依序完全相同，多一列（例如舊的 `up/#`）或少一列都會失敗。
- 沒有 podman 或 docker：本機兩項略過並印出原因；`CI=1` 時兩項都失敗（`podman or docker is required when CI is set`）。
- `cargo build --locked --target aarch64-unknown-linux-gnu -p protocol` 成功；`tests/dependencies.rs` 通過，沒有新增相依套件（真 broker 測試只用標準函式庫與既有的 dev-dependency `serde_json`，MQTT 用戶端是映像內的 `mosquitto_pub`、`mosquitto_sub`、`mosquitto_rr`）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 閘道器角色只有精確的上行頻道發布規則，沒有萬用字元（單元測試） | 通過：`publish_rules_are_exact_one_per_up_channel`、`rules_serialize_for_dynamic_security` |
| 真 Mosquitto 2.1.x（podman）：非協定 topic 被拒 | 通過：PUBACK 0x87、雲端收不到、無 retained |
| 真 Mosquitto 2.1.x：協定頻道照常可用 | 通過：7 個 `up`、4 個 `down` channel 與 `up/status` 的 LWT |
| 真 Mosquitto 2.1.x：修正前的規則讓同一個測試失敗 | 通過：`the_wildcard_rule_before_the_fix_fails_the_same_check`（同一容器測試以舊規則斷言 30 項違規），以及突變紀錄 `mutation-legacy-rule.log` |
| README 的 ACL 表、程式與測試一致；retained 的決定寫進 README | 通過：README ACL 一節的表與決定；`tests/readme.rs` 逐列比對 |
| 測試證據存到 `docs/test-reports/` | 本報告與 data 目錄 |

broker 層級的上限（maximum packet size、每個 client 的佇列與 retained 數量和大小）不在本 issue 範圍，由 Laysi/trustiot-v2#84 設定。
