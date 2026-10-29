# gateway-ops：retained 下發、合併節流、套用回報與重發

對應 Laysi/trustiot-v2#720。

這份報告記錄 2026-10-29 的檢查：gateway-ops 把 desired 設定以 retained、QoS 1 發布到閘道器的 `down/config`，合併節流、broker 重啟後重發、套用回報的記錄與拒絕、readiness 跟著 broker 連線，以及整個 workspace、腳本的測試。後半段的結果是安全審查（M1、M2、L1、L2、L3）修正之後的樹。

## 環境

- 程式碼：分支 `feat/720-gateway-ops-delivery`，基於 main `371572a3`。第一輪：`d0ac0cf3`（下發）與 `7680e89c`（測試用 broker 的連接埠重試）。安全審查後：`64e967c0`（M1）、`ba1a4464`（M2）、`65a2b792`（L1）、`5be643c2`（L2）、`3bad6845`（L3，只改 README）。除了另外註明的項目，以下結果都在 `3bad6845` 的樹上執行；之後的 commit 只更新這份報告與原始資料。
- 建置主機（arm64 Linux VM）：整個 workspace 的 `cargo fmt --check`、clippy（先 `cargo clean -p gateway-ops`，確定 gateway-ops 真的重新檢查）、測試（`agent` 除外，這次沒有改 `edge/`），以及 gateway-ops 的測試與 evidence 行。
- 建置主機（x86_64 Linux）：`cargo deny`、腳本的測試與型別檢查，程式碼同 `3bad6845`，樹上另有更新中的這份報告。arm64 Linux VM 上沒有釘選版本的 cargo-deny、helm 與 actionlint，所以這三項不在那裡跑。
- 第一輪（`7680e89c`）另在 macOS 的建置主機上跑 gateway-ops 的測試與 `tests/delivery.rs` 連跑 4 次，在 x86_64 Linux 的建置主機上跑整個 workspace。
- 所有建置主機都設了 `CI=1`：容器測試以 podman 執行，沒有容器執行環境時失敗而不是略過。
- Postgres 是 `testkit::product` 的 TLS Postgres：migration Job 套用整個產品的 schema，服務以 `verify-full`、以 `gateway_ops_app` 與 `gateway_ops_platform` 連線。測試的服務角色連線池剛好是 `MIN_DATABASE_CONNECTIONS`（7）條，所以所有 Postgres 測試都在下限的連線數下通過。
- Broker 是拋棄式的 Mosquitto（`testkit::mqtt::mosquitto::MOSQUITTO_IMAGE`，和 chart 的 `mqttBroker.image` 同一個釘選），只聽 MQTT 5，`max_packet_size`、`max_inflight_messages`、`max_keepalive` 和 chart 的服務 listener 相同，**沒有持久化**：重啟就清掉所有 retained 訊息。gateway-ops 的帳號、角色與預設權限取自 migration Job 的 `migrate::mqtt_accounts`；測試用的閘道器是一個可以讀所有 `down/config`、發布所有 `up/config-report` 的服務帳號（client crate 只連服務埠）。密碼都在執行時隨機產生。

## 指令

```sh
# 建置主機（arm64 Linux VM），3bad6845 的樹
cargo fmt --all --check
cargo clean -p gateway-ops
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --exclude agent
cargo test --locked -p gateway-ops -- --nocapture --test-threads=1

# 建置主機（x86_64 Linux）
cargo deny --locked check
bun install --cwd scripts --frozen-lockfile
bun test scripts
bun run --cwd scripts typecheck

# 第一輪，建置主機（macOS），7680e89c
for i in 1 2 3 4; do cargo test --locked -p gateway-ops --test delivery; done
```

## 結果

gateway-ops 每個測試與 evidence 行見 [gateway-ops-tests.txt](data/2026-10-29-720-gateway-ops-delivery/gateway-ops-tests.txt)，workspace 每個測試執行檔的結果見 [workspace-summary.txt](data/2026-10-29-720-gateway-ops-delivery/workspace-summary.txt)，第一輪連跑 4 次見 [delivery-repeat.txt](data/2026-10-29-720-gateway-ops-delivery/delivery-repeat.txt)。

gateway-ops 全部 40 個測試通過：單元測試 23、`delivery` 6、`desired_config` 7、`startup` 1、`cross_tenant` 1、`generation_errors` 1、`unknown_events` 1。

### 合併節流與 retained（`ten_quick_changes_are_published_once_retained_as_the_latest_version`）

節流時間窗設成 10 秒，同一台閘道器連續存下並通知第 2～11 版：

| 項目 | 結果 |
| --- | --- |
| 合併的通知 | 9 |
| 發布 | 1 次，第 11 版，第一個通知後 10.0 秒（時間窗結束時） |
| `config_deliveries` | 只有第 11 版，`published = true` |
| 之後才連上的訂閱者 | 收到 retained 的第 11 版，QoS 1 |

`src/delivery/schedule.rs` 的單元測試以手動時鐘證明同一件事：十次請求合併 9 次，時間窗結束前什麼都不取，結束時取出一筆、帶第 10 版；晚到的舊版本通知不會降低要等的版本；後來的通知不會延後時間窗。

### broker 重啟與重發（`a_broker_restart_republishes_every_gateway_and_readiness_follows_the_connection`）

兩台閘道器（最新版本 2 與 3），broker 停掉：

- readiness 的 `mqtt` 檢查失敗，訊息 `not connected to the mqtt broker with the apply report subscription granted`。
- broker 重新啟動（retained 訊息全部消失）之後：恢復 ready；重發次數 1 → 2，重發的發布累計 4 次（重發涵蓋資料庫裡所有閘道器，包括同一個測試執行檔先前的測試留下的，所以這個數字隨測試順序而變）；新的訂閱者收到兩台各自的最新版本 `2`、`3`；兩台的 `config_deliveries` 都記下重發（`last_published_at` 晚於第一次發布）。

`a_replica_finding_the_republish_lock_taken_leaves_the_republish_to_its_holder`：job lock `gateway-ops/config-republish` 在別的 session 手上時，重發略過 1 次、執行 0 次、重發的發布 0 次。

### 別的 replica 正在發布（`a_gateway_another_replica_is_publishing_waits_for_its_publish_lock`）

別的 session 拿著這台閘道器的發布鎖（`delivery::publish_lock_key`）時通知一個新版本：5 秒（5 個時間窗）內 0 次發布，這台閘道器仍在排程裡；鎖放開後送出第 2 版。

### 套用回報（`apply_reports_are_recorded_under_the_topics_gateway_and_unreasonable_ones_refused`）

| 回報 | 結果 |
| --- | --- |
| 第 2 版 `rejected`，`active_version` 1，一個 `out_of_range` issue | 記在第 2 版的租戶，連同 issue |
| 第 2 版 `applied` | 同一列覆蓋成 `applied`、`active_version` 2、沒有 issue |
| 第 2 版 `applied`，payload 另外帶 `gateway`、`hardware_id` 指向另一台閘道器 | 記在 topic 的閘道器；被指名的那台沒有任何回報紀錄 |
| 同一則第 2 版 `applied` 再送 3 次 | 2 則判定為重複、沒有再寫入，1 則在處理前被下一則取代；記錄次數不變，`apply report recorded` 仍只有 2 行；另一個 replica 的連線池寫同一則也得到 `Repeat` |
| `version` 99 | 拒絕，`version_ahead` |
| `active_version` 50 | 拒絕，`version_ahead` |
| 沒有任何設定的閘道器 | 拒絕，`unknown_gateway` |
| 解不開的 payload | 拒絕，`decode` |
| 存下第 3 版之後回報第 2 版 `applied` | 記錄，並再發布第 3 版 |

被拒的回報不改變任何紀錄。所有日誌都不含 broker 密碼（測試讀密碼檔比對）。retained、QoS 0、topic 不對、未知 `outcome`、不存在的版本、先前綁定的版本、最新一版不屬於最新綁定，在 `src/delivery/reports.rs` 的單元測試；待處理的回報（每台閘道器只留最新一則、保留排隊位置、滿了放棄等最久的、資料庫錯誤時延遲重試到上限、重試中的回報被新回報取代）也在那裡。

### 一台閘道器洗版（`a_gateway_flooding_its_report_topic_does_not_crowd_out_another`）

安全審查 M1 的測試：一台閘道器連送 500 則版本從沒產生過的回報，接著另一台閘道器送一則正常的 `applied` 回報。

| 項目 | 結果 |
| --- | --- |
| 另一台閘道器的回報 | 記錄（`applied`） |
| 洗版的 500 則 | 136 則讀過資料庫後拒絕，364 則在處理前被同一台閘道器的下一則取代 |
| 因待處理的位置不足而放棄的回報 | 0 |

拒絕與取代的比例取決於回報 task 的速度，每次執行不同（第一次在同一台主機上是 135 與 365）；不變的是兩者合計 500、沒有放棄任何回報、另一台的回報被記錄。

### 真的執行檔（`tests/startup.rs`）

`the_service_delivers_versions_retained_follows_the_broker_and_stops_on_sigterm`：服務 ready；閘道器有第 1、2 版，新的訂閱者收到 retained 的第 2 版（QoS 1）；broker 停掉時 `/readyz` 回 `503 {"status":"unavailable"}`，日誌記下失敗的檢查是 `mqtt`；broker 回來後恢復 ready，日誌有 2 次重發，新的訂閱者收到 retained 的第 2 版；SIGTERM 之後以 0 結束；stdout 與 stderr 都不含 broker 密碼。

### 跨租戶

`tests/cross_tenant.rs` 加上 `config_deliveries`：別的租戶讀不到、改不到。

### 測試用 broker 的連接埠競態

第一輪修正前在 macOS 建置主機上看過兩次失敗：一次是 `ten_quick_changes_are_published_once_retained_as_the_latest_version`，`podman run` 失敗於 `Listen failed for HOST TCP port 127.0.0.1/<port>: Address already in use`（測試先綁一個空的 loopback 埠再放掉，交給容器執行環境綁定，中間被別的程序拿走）；另一次是 `a_gateway_another_replica_is_publishing_waits_for_its_publish_lock`，當時的輸出沒有留下 panic 訊息。`7680e89c` 讓 broker 換一個埠重試（最多 5 次，認 podman 與 docker 的訊息），之後同一台主機連跑 4 次加上一次完整的 gateway-ops 測試，所有測試都通過。第二次失敗的原因沒有直接證據，推斷是同一個競態。

### workspace

| 檢查 | 位置 | 結果 |
| --- | --- | --- |
| `cargo fmt --all --check` | 建置主機（arm64 Linux VM），`3bad6845` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings`（先 `cargo clean -p gateway-ops`） | 建置主機（arm64 Linux VM），`3bad6845` | 通過 |
| `cargo test --locked --workspace --exclude agent` | 建置主機（arm64 Linux VM），`3bad6845` | 236 個 test result，1862 passed、0 failed、1 ignored（既有的 `assistant` doc-test）；包括 gateway-ops 全部的測試、`tools/sql-check`、`repo-check`、OpenAPI 檢查 |
| `cargo deny --locked check` | 建置主機（x86_64 Linux） | advisories、bans、licenses、sources 都 ok |
| `bun test scripts`、`bun run --cwd scripts typecheck` | 建置主機（x86_64 Linux） | 1704 pass、9 skip、0 fail（skip 的是既有、以環境變數開啟的容器與 e2e 測試或只在別的平台跑的測試，例如 `scripts/tests/e2e.test.ts`；這次沒有改 `scripts/`）；typecheck 通過 |

`agent` 不在這次的範圍內（沒有改 `edge/`），由 CI 照常跑。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 設備變更後產生的新版本以 retained 訊息送出，新的訂閱者收到最新版 | 通過：之後才連上的訂閱者收到 retained 的最新版（`tests/delivery.rs` 與真的執行檔） |
| 連續十次快速變更只送出合併後的結果，最終 retained 的版本正確 | 通過：合併 9 次、發布 1 次，retained 的是第 11 版 |
| broker 重啟或清空 retained 之後，服務重發所有閘道器的 desired 設定 | 通過：沒有持久化的 broker 重啟後，兩台閘道器的最新版本都重新 retained；同一時間只有一個 replica 重發 |
| 套用回報記下版本與錯誤；topic 與內容的閘道器不同時以 topic 為準；不合理的版本被拒 | 通過：見「套用回報」的表；一台閘道器洗版擠不掉別台的回報 |
| broker 連不上時 readiness 不是 200，連上後恢復 | 通過：`/readyz` 503、失敗的檢查是 `mqtt`，broker 回來後 ready |
| `cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings`、`bun test scripts` 通過；容器測試以 CI 設定時不略過 | 通過：workspace 的 fmt、clippy、測試（`agent` 除外）與 `bun test scripts` 都通過；所有建置主機都設了 `CI=1`，容器測試真的啟動了 Postgres 與 Mosquitto，沒有略過 |

延後的進階驗證（另開 P2 issue）：大機隊的重發時間與資源量測、多個 replica 同時運作的實測、資料庫或 broker 的故障注入。
