# certs 服務：閘道器綁定與租戶狀態的投影（2026-10-24）

Laysi/trustiot-v2#632：certs 消費資產服務的 `assets.gateway_bound`，在自己的 schema 維護閘道器目前綁定的租戶與租戶狀態，給換發（Laysi/trustiot-v2#81）與 broker 權限（Laysi/trustiot-v2#633）以平台角色讀取。規則與取捨在 [services/certs/README.md](../../services/certs/README.md) 的「閘道器的綁定與租戶狀態」。這份報告是七條驗收條件的證據。

## 環境

- **程式碼**：分支 `feat/632-certs-binding-projection`，rebase 到 main `1ad0ffc5`（含 Laysi/trustiot-v2#624 的設備 API 與 Laysi/trustiot-v2#659 的決策 12）之後的實作 commit `5b2fd1da`。下面的數字都來自這棵樹；rebase 前在基底 `0dc8fd96` 上跑過同一組檢查，結果相同（全部通過）。
- **機器**：建置主機（x86_64 Linux）。
  - 設了 `CI=1`：需要容器的測試以拋棄式的 TLS Postgres 執行，不略過。
  - 工具版本是 `.prototools` 與 `rust-toolchain.toml` 釘選的：Rust 1.99.0、bun 1.4.2、helm 4.3.0、kubeconform 0.8.0。
- **日期**：2026-10-24。

## 指令

在 repo 根目錄依序執行：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -q -p openapi -- --check
CI=1 cargo test --locked --workspace
CI=1 cargo test --locked -p certs --test bindings -- --nocapture --test-threads=1
CI=1 cargo test --locked -p certs --test cross_tenant -- --nocapture
bun test deploy/chart
```

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy ... -D warnings` | 通過。前一輪在 `tests/bindings.rs` 有 `cloned_ref_to_slice_refs` 與 `type_complexity` 兩個錯誤，修正後通過 |
| `cargo run -p openapi -- --check` | `api/openapi.json is up to date`（沒有 API 變更） |
| `CI=1 cargo test --workspace` | 195 個測試執行檔（含 doc-test），1497 通過、0 失敗、1 略過（`services/assistant/src/delegation.rs` 標 `ignore` 的 doc 範例，不是這個變更的） |
| `cargo test -p certs --test bindings` | 3 通過（證據行見下） |
| `cargo test -p certs --test cross_tenant` | 56 個探測、0 失敗 |
| `bun test deploy/chart` | 648 通過、0 失敗（`_postgres.tpl` 只改了註解，連線預算不變） |

### `tests/bindings.rs` 的證據行

`a_binding_reaches_the_service_once_whatever_is_replayed`：啟動真的 certs 執行檔，測試以 `assets_app` 在租戶交易寫一筆 `assets.gateway_bound` 到 `assets.outbox_event`。

```text
evidence: stored binding [("01a16cca-1d43-7220-a9ec-e63e013c7884", "01a16cca-1d43-7220-a9ec-e640689fcf98", 2026-10-24T07:22:27.603Z, 840, 1, true, 2026-10-24T07:22:28.463709Z)]; log line {"fields":{"binding_id":"01a16cca-1d43-7220-a9ec-e640689fcf98","created":true,"event_id":"01a16cca-1d93-7565-ab71-ddb82e34693c","hardware_id":"tg-bind-01a16cca1d437220a9ece63f201cb8a2","tenant_id":"01a16cca-1d43-7220-a9ec-e63e013c7884"},"level":"INFO","message":"applied assets.gateway_bound","service":"certs",...}
evidence: 3 applications of tg-bind-01a16cca1d437220a9ece63f201cb8a2, 1 created a row; rows unchanged
```

表中的一列依序是租戶、綁定 ID、`bound_at`（事件的 `occurred_at`）、事件在 log 的位置（`tx_order`、`id`）、`unbound_at IS NULL`、`recorded_at`。之後測試把訂閱倒回這個事件之前、再倒回訂閱起點，都以 `Replay::Reprocess` 重新處理：處理了三次，只有第一次寫入一列，表的內容與第一次相同。測試也確認訂閱表有 `certs.assets-events`，來源是 `assets`。

`late_and_invalid_bindings_neither_win_nor_block`：

```text
evidence: late older binding stored [(<tenant A>, <binding A>, ..., 838, 2, ...), (<tenant B>, <binding B>, ..., 839, 1, ...)]; current Binding { hardware_id: GatewayId("tg-late-..."), tenant_id: TenantId(<tenant B>), binding_id: GatewayBindingId(<binding B>), ... }
evidence: dead letters "event cannot be decoded" and "assets.gateway_bound without a tenant"; the binding after them is current
```

較舊的綁定（租戶 A，log 位置 838）在較新的綁定（租戶 B，位置 839）之後才處理：它照樣存下，但 `gateway_tenant` 回的仍是租戶 B。硬體 ID 不合規則的事件與沒有租戶的事件各進一筆 dead letter，之後的綁定照常處理、成為目前的綁定。

`statuses_keep_the_newest_version_and_platform_reads_see_every_tenant`：

```text
evidence: 11 audit lines, e.g. Some({"actor": "service", "purpose": "test-renewal", "service": "certs"})
```

租戶狀態：版本 2 寫入「停權」後，版本 1 與版本 2 的「正常」都不覆蓋，版本 3 才覆蓋；沒有紀錄的租戶讀成正常；`tenant_suspended` 對停權的租戶回真。租戶交易寫別的租戶的狀態被拒（42501），也只讀得到自己那一列。平台讀取：跨兩個租戶的綁定都讀得到，同一台閘道器以 log 位置最後的綁定為準，已結束的綁定不列出，從沒綁定的閘道器是 `None`。測試做的 11 次讀取（10 次 `test-renewal`、1 次 `test-sync`）各有一行 INFO `platform database access`，actor 是 `service`、`certs`。

### 跨租戶探測（`cargo test -p certs --test cross_tenant`）

新的兩張表的探測全部通過（節錄）：

```text
pass | certs.gateway_bindings | read as tenant A | refused (42501)
pass | certs.gateway_bindings | update own rows as tenant A | refused (42501)
pass | certs.gateway_bindings | insert into tenant B | refused (42501)
pass | certs.tenant_statuses | read as tenant A | 1 rows visible, all own, none of tenant B
pass | certs.tenant_statuses | read without a tenant | no row visible
pass | certs.tenant_statuses | update own rows as tenant A | 1 rows reached, none of tenant B
pass | certs.tenant_statuses | move own rows into tenant B | refused (42501)
pass | certs.tenant_statuses | insert into tenant B | refused (42501)
56 probes, 0 failed
```

`certs_app` 對 `gateway_bindings` 只有 `INSERT`，所以讀與改都被拒（42501），連自己租戶的列也讀不到。

### 與 main 合併後的重跑

分支合併 main `757f52df`（Laysi/trustiot-v2#655，ai-gateway 的模型角色設定與它的連線預算）之後，合併 commit `9dea8175` 的樹在建置主機（x86_64 Linux）上依序執行：

```sh
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked -p certs -p migrate -p ai-gateway
cargo test --locked -p certs -p assets -p ai-gateway -p assistant -p identity -p telemetry-query --test chart_connections
bun test deploy/chart
cargo run --locked -q -p openapi -- --check
```

| 指令 | 結果 |
| --- | --- |
| `cargo clippy ... -D warnings` | 通過 |
| `CI=1 cargo test -p certs -p migrate -p ai-gateway` | 42 個測試執行檔，245 通過、0 失敗、0 略過 |
| `cargo test ... --test chart_connections`（六個服務） | 6 通過 |
| `bun test deploy/chart` | 655 通過、0 失敗 |
| `cargo run -p openapi -- --check` | `api/openapi.json is up to date` |

`exit 0 at 16:28:22 (build host)`

### 安全審查的修正之後

commit `6fcb98b1` 依安全審查改了四處（README 與 PR 說明有細節）：

- 這個版本不認得的 `gateway_binding` 事件改為重試，不略過。
- 拿掉 `unbound_at >= bound_at` 的 CHECK。
- 平台讀取交易設 `statement_timeout` 5 秒、`lock_timeout` 2 秒。
- 資產服務的綁定 `INSERT` 加上不變式註解。

依專案的驗證規則，這幾項（Medium、Low）不加新測試，既有的測試要照樣通過。

這個 commit 的重跑（`cargo clippy --workspace`、`CI=1 cargo test -p certs -p migrate`）是待測：2026-10-24 排進建置主機時，主機剩餘磁碟不到一個工作需要的量，工作沒有執行。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 綁定事件消費後表裡有閘道器、正確的租戶與綁定 ID；同一事件送兩次、整個訂閱重播都只有一列、內容不變 | 通過（`a_binding_reaches_the_service_once_whatever_is_replayed`，真的 Postgres，`CI=1`） |
| 較舊的綁定事件晚到不覆蓋較新的綁定 | 通過（`late_and_invalid_bindings_neither_win_nor_block`） |
| 租戶狀態只接受較大的版本號；沒有紀錄讀成正常；寫入停權後 `tenant_suspended` 回真 | 通過（`statuses_keep_the_newest_version_and_platform_reads_see_every_tenant`）。目前沒有事件寫入狀態（Laysi/trustiot-v2#107、#108 之後才有），測試以租戶交易直接呼叫 `store_tenant_status` 當作替身 |
| 不合規則的硬體 ID 或缺租戶的事件進 dead letter，之後的事件照常處理 | 通過（`late_and_invalid_bindings_neither_win_nor_block`） |
| 跨租戶負向測試；`gateway_tenant` 與所有已綁定的閘道器經平台角色讀得到所有租戶，每次讀取都有稽核日誌 | 通過（`cross_tenant` 56 個探測；`statuses_keep_...` 的 11 行稽核日誌）。稽核用 `PlatformDb::begin`（每筆交易一行 INFO），不是 `PlatformDb::reader`（之後每筆交易只記 DEBUG），理由在 README |
| migration Job 套用整個產品的 schema 成功，certs 的訂閱出現在 outbox 的訂閱表 | 通過。`cargo test -p migrate`（`fresh_database`）確認 Job 建立 certs 的消費端表、`certs_platform` 只能 `SELECT` `assets.outbox_event`，`assets_platform` 不再讀它。訂閱列由消費端啟動時建立，`a_binding_reaches_...` 在 Job 套用的 schema 上啟動真的服務後確認訂閱表有 `certs.assets-events` |
| `cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace` 通過 | 通過 |

待測（進階驗證，issue 已列為之後再做）：消費延遲與大量重播的收斂時間。
