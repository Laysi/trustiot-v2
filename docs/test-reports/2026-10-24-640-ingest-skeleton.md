# ingest：服務骨架、Postgres schema 與閘道器綁定投影

對應 Laysi/trustiot-v2#640。

這份報告記錄 2026-10-24 在開發者工作站與建置主機上的檢查：ingest 執行檔的 readiness、`ingest.gateway_bindings` 由 `assets.gateway_bound` 維護的行為、跨租戶負向測試，以及登記 schema 之後 workspace 與腳本的測試。2026-10-27 補上安全審查的修正（綁定的先後改依事件位置）與合併 main 到 `ff01511e` 之後的檢查，見「安全審查之後」與「合併 main 之後」。

## 環境

- 程式碼：分支 `feat/640-ingest-service-skeleton`，基於 main `1ad0ffc5`；ingest 的測試在 `a768442c` 的工作樹快照上執行；之後合併 main 的 `757f52df`（`117fdf2c`），整個 workspace 的檢查見「workspace 檢查」。2026-10-27：安全審查的修正 `3ca69d15`，再合併 main 到 `dab88223`（`6d6418f1`）、`509c3f1f`（`82286ea5`）與 `ff01511e`（`9ba9e950`），最後一次合併的樹上的檢查見「合併 main 之後」。
- 建置主機（x86_64 Linux）：Rust 的 clippy 與測試、`bun test scripts`、`bun test deploy/chart`。每次在工作樹的快照上執行，設了 `CI=1`，所以沒有容器執行環境時容器測試失敗而不是略過。
- 開發者工作站（x86_64 Linux）：`cargo fmt --all --check`、`cargo metadata --locked`、`bun test scripts/tests/product-logins.test.ts scripts/tests/migrate.test.ts scripts/ci/changes.test.ts`。
- Postgres 是 `testkit::product` 的 TLS Postgres（migration Job 套用整個產品 schema，服務以 `verify-full` 連線）；ClickHouse 是 `services/ingest/tests/common` 的測試容器，套用全部 ClickHouse migration。

## 指令

```sh
# ingest 的測試（建置主機）
cargo clippy --locked -p ingest -p migrate -p openapi --all-targets -- -D warnings
cargo test --locked -p ingest --lib --test startup --test gateway_bindings --test cross_tenant -- --nocapture

# workspace 與腳本（建置主機）
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace
cargo deny --locked check
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts
bun test deploy/chart
```

## 結果

ingest 測試的輸出見 [ingest-tests.txt](data/2026-10-24-640-ingest-skeleton/ingest-tests.txt)。

### readiness（`tests/startup.rs`）

四個 ingest 執行檔同時啟動，Postgres 角色是 `ingest_app` 與 `ingest_platform`，ClickHouse 帳號持有 `trustiot_ingest`、密碼在執行時產生：

| 情境 | `/readyz` | 日誌的 `readiness check failed` |
| --- | --- | --- |
| Postgres 與 ClickHouse 都連得上 | 200 `{"status":"ready"}` | 無 |
| ClickHouse 連不上（關閉的埠） | 503 | `check=clickhouse`，`cannot connect to 127.0.0.1:<埠>: Connection refused` |
| ClickHouse 拒絕密碼 | 503 | `check=clickhouse`，`ClickHouse error 516 (HTTP 403)` |
| Postgres 連不上（關閉的埠） | 503 | `check=postgres`，`timed out after 800 ms` |

每個實例的日誌都不含 ClickHouse 密碼；ClickHouse 失敗的兩個實例只有 `clickhouse` 檢查失敗，Postgres 失敗的實例沒有 `clickhouse` 檢查失敗。最後再查一次第一個實例，仍是 200。

### 綁定對照表（`tests/gateway_bindings.rs`）

ingest 執行檔以真的 Postgres 執行（ClickHouse 指向關閉的埠，消費端不需要它）。資產服務自己的 `GatewayBinder` 以平台管理員身分把閘道器綁給租戶 A，之後依序。第 4、5 步是安全審查之後（2026-10-27）以事件位置排序的版本，結果見「安全審查之後」；原本的版本以 `bound_at` 排序，2026-10-24 的輸出在 data 檔：

| 步驟 | 結果 |
| --- | --- |
| 1. 綁定 | 對照表一列：租戶 A、該 `binding_id`、`unbound_at` 空；`gateway_tenant` 回租戶 A 與 `binding_id`；沒綁過的閘道器回 `None` |
| 2. 整個訂閱以 `Replay::Reprocess` 重播 | 仍是一列，`recorded_at` 不變，日誌記 `created=false` |
| 3. 同一個綁定以新的事件 ID 再送一次 | 仍是一列 |
| 4. 事件紀錄裡較晚的綁定（租戶 B），`occurred_at` 卻早一小時（時鐘偏差的資產服務副本） | 兩列，`gateway_tenant` 回租戶 B 與它的 `binding_id`：先後依事件位置，不依時鐘 |
| 5. 較早的綁定晚到：刪掉租戶 A 那一列（當作它的事件還沒記錄，例如進了 dead letter），再整個訂閱以 `Replay::Reprocess` 重播 | 租戶 A 那一列重新寫入（日誌 `created=true`），仍是兩列，`gateway_tenant` 仍回租戶 B |
| 6. 已記錄的 `binding_id` 配上另一個閘道器 | 進 dead letter，原因 `assets.gateway_bound names a recorded binding with another gateway`；另一個閘道器沒有任何列；之後的事件照常套用 |
| 7. 平台角色 `ingest_platform` 寫入綁定 | `42501`（只有 SELECT） |

### 安全審查之後

`427d9690` 的安全審查指出：`bound_at` 與 UUIDv7 的 `binding_id` 都來自寫入事件的資產服務副本的時鐘，一個時鐘偏差的副本可能讓較舊的綁定排成目前的綁定。`3ca69d15` 改成依事件在資產服務 outbox 紀錄裡的位置（`position_tx`、`position_id`）排序，與 certs 的投影（Laysi/trustiot-v2#632）同一條規則；`bound_at` 只供閱讀，拿掉 `unbound_at >= bound_at` 的 CHECK。上表第 4、5 步是新版本，在 `9ba9e950` 的樹上的輸出（[ingest-tests-2026-10-27.txt](data/2026-10-24-640-ingest-skeleton/ingest-tests-2026-10-27.txt)）：

- 第 4 步之後兩列（`after a later binding with a skewed clock`），第 5 步重播之後仍是同樣的兩列（`after the earlier binding arrived late`），兩次 `gateway_tenant` 都回租戶 B；測試斷言這兩點。
- 第 6 步的 dead letter 原因與先前相同。
- `cross_tenant` 19 probes、0 failed；`startup` 的四個情境與上表相同。

### 跨租戶負向測試（`tests/cross_tenant.rs`）

19 個 probe 全部通過：`ingest.gateway_bindings` 以服務角色在租戶 A、B 各只看到自己的兩列，沒有租戶時看不到任何列；更新、搬到另一個租戶、寫入另一個租戶都回 `42501`。outbox 消費端的四張表與 `trustiot_migrate_applied` 以理由豁免，ingest 沒有 HTTP API。

### workspace 檢查

合併 main 之前的樹（`a768442c`），建置主機：

```
$ cargo fmt --all --check
$ cargo clippy --locked --workspace --all-targets -- -D warnings
（沒有輸出）
$ cargo deny --locked check
advisories ok, bans ok, licenses ok, sources ok
$ bun run --cwd scripts typecheck
（沒有錯誤）
$ bun test scripts
 1662 pass
 9 skip
 0 fail
$ cargo test --locked --workspace
test a_fresh_database_gets_every_migration_once ... FAILED
$ bun test deploy/chart
(fail) saas shape > rendered manifests pass kubeconform -strict for Kubernetes 1.36.0 [61379.77ms]
 647 pass
 1 fail
exit 1 at 15:50:40 (build host)
```

exit 1 來自後兩個指令，`bun test scripts` 本身全部通過。`cargo test --locked --workspace` 在 `migrate` 的 `fresh_database` 失敗：`assets.outbox_event` 的 `consumer_read` 現在也列 `ingest_platform`（ingest 消費 assets 的事件），測試的預期清單還沒加，`169b1592` 補上。`bun test deploy/chart`（不是 `bun test scripts`）有一個 kubeconform 的 case 在 61 秒時失敗，是單一指令到了 60 秒的上限（`deploy/chart/tests/timing.ts` 的 `COMMAND_TIMEOUT_MS`），與這個變更無關，下一輪重跑通過。加上 `fresh_database` 的修正（`169b1592`，之後只改了註解）重跑：

```
$ cargo test --locked --workspace --no-fail-fast
196 個 test result 全部 ok（含 migrate 的 fresh_database、registry，testkit，ingest）
$ bun test deploy/chart
 648 pass
 0 fail
exit 0 at 16:20:27 (build host)
```

合併 main 的 `757f52df`（Laysi/trustiot-v2#655）之後（`117fdf2c`）：衝突只在 registry 與登入清單，兩邊都保留。工作站上 `cargo fmt --all --check`、`cargo metadata --locked` 與 `bun test scripts/tests/product-logins.test.ts scripts/tests/migrate.test.ts`（23 pass、0 fail）通過。2026-10-24 建置主機的磁碟空間不足，整套檢查改在下面最後一次合併之後的樹上執行。

### 合併 main 之後

2026-10-27 再合併 main 三次：

- `dab88223`（`6d6418f1`）沒有衝突。
- `509c3f1f`（`82286ea5`，含 Laysi/trustiot-v2#687 的彙總表與 Laysi/trustiot-v2#574 的閘道器 agent）有四個衝突，兩邊都保留：
  - `Cargo.lock` 取 main 的版本，再由 cargo 重新解析，與 main 只差 ingest 的相依清單。
  - `services/ingest/Cargo.toml` 保留服務的相依與 `[[bin]]`，dev-dependency 用 main 的 `chrono`（`features = ["now"]`）。
  - `services/ingest/src/lib.rs` 的模組清單加上 `rollup`。
  - `services/ingest/README.md` 開頭一段合併兩邊的說明。
- `ff01511e`（`9ba9e950`，含 Laysi/trustiot-v2#632 的 certs 綁定投影、Laysi/trustiot-v2#606 的 AI 閘道管理 API、Laysi/trustiot-v2#644 的助理對話儲存）有兩個衝突，兩邊都保留：
  - `tools/migrate/tests/fresh_database.rs`：沒有密碼的角色依 registry 的順序是 `assistant_platform`（main）、`ai_gateway_app`、`ingest_app`、`ingest_platform`。`assets.outbox_event` 的讀取者是 `certs_platform`、`identity_platform`、`ingest_platform`（assets 不再讀自己的事件，是 main 的變更），之後是 main 對 certs 消費端權限與表的檢查。
  - `scripts/tests/product-logins.test.ts`：assets 與 certs 的 `consumes` 用 main 的預期，chart 的平台登入清單是 `assets`、`certs`、`identity`、`ingest`、`telemetry-query`。
  - 自動合併的 `tools/migrate/src/registry.rs`（ingest 在 ai-gateway 之後，`consumes: &["assets"]`）與 `Cargo.lock`（與 main 只差 ingest 的相依）也逐一確認過。

`9ba9e950` 的樹，建置主機，`CI=1`：

```
$ cargo fmt --all --check
$ cargo clippy --locked --workspace --all-targets -- -D warnings
（沒有輸出）
$ cargo test --locked -p ingest -- --nocapture
9 個 test result 全部 ok，33 passed
$ cargo test --locked -p migrate
24 個 test result 全部 ok，170 passed（含 fresh_database、registry）
$ cargo test --locked -p repo-check
3 個 test result 全部 ok，28 passed
$ cargo test --locked -p sql-check
3 個 test result 全部 ok，26 passed
$ cargo run --locked -p openapi -- --check
api/openapi.json is up to date
$ cargo deny --locked check
advisories ok, bans ok, licenses ok, sources ok
$ bun test deploy/chart
 659 pass
 0 fail
$ bun install --cwd scripts --frozen-lockfile
$ bun test scripts
 1704 pass
 9 skip
 0 fail
exit 0 at 23:10:58 (build host)
```

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| `cargo run -p ingest` 以 `bun scripts/dev.ts env` 的 Postgres 設定加 ClickHouse 設定啟動，兩者都連得上時 readiness 200，任一個連不上時不是 200 | 通過：`tests/startup.rs` 執行同一個執行檔，Postgres 的平台設定是 `dev.ts env` 印出的同一組變數（`TRUSTIOT_POSTGRES_*`，由 `testkit::product` 給值），ClickHouse 是既有的測試容器 |
| 平台管理員綁定後對照表有租戶與 `binding_id`；同一事件兩次、整個訂閱重播都只有一列；較舊的綁定晚到不覆蓋較新的 | 通過：上方「綁定對照表」第 1 至 5 步 |
| 跨租戶負向測試涵蓋新的表並通過 | 通過：`tests/cross_tenant.rs` 19 個 probe；`cargo test -p testkit` 在 workspace 的測試裡通過（「workspace 檢查」） |
| migration Job 建立 `ingest` schema、角色與 outbox 消費的權限；`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings` 通過 | 合併 main 之前通過：`fresh_database` 確認兩個 ingest 角色存在且 NOLOGIN、`ingest_platform` 列在 `assets.outbox_event` 的 `consumer_read`，workspace 的測試與 clippy 通過。合併 main 到 `ff01511e` 之後（`9ba9e950`）也通過：clippy、`migrate`（含 `fresh_database`）、`ingest`、`repo-check`、`sql-check` 的測試（「合併 main 之後」）；整個 workspace 的測試由 PR 的 CI 執行 |
