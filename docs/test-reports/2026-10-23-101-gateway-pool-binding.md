# assets 服務：閘道器設備池與綁定 API（2026-10-23）

Laysi/trustiot-v2#101：出廠登錄過的閘道器（`certs.gateway_registered`）進入資產服務的設備池，平台管理員把它綁定到一個租戶；綁定可以重試、不留下卡住的狀態，並寫出事件 `assets.gateway_bound`。設計與取捨在 [services/assets/README.md](../../services/assets/README.md) 的「閘道器設備池與綁定」。這份報告是四條驗收條件的證據。

## 環境

- **程式碼**：分支 `feat/101-gateway-pool-binding`。
  - 下面的完整檢查跑在以 main `47713f04` 為基底的實作 commit 上。
  - 之後 rebase 到 main `994ffae7`（Laysi/trustiot-v2#588 的 assistant 對話儲存，也改了 `tools/migrate/src/registry.rs` 與 `scripts/tests/product-logins.test.ts`），沒有衝突，實作 commit 變成 `236930c6`。
  - rebase 後的樹重跑了 `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test -p repo-check`、`CI=1 cargo test -p migrate`、`cargo run -p openapi -- --check`、`CI=1 cargo test -p assets`、`cargo test -p testkit`、`cargo test -p contract`、`bun test scripts`（1597 通過、9 略過、0 失敗）、`bun run --cwd scripts typecheck`、`bun run --cwd frontend api:check`、`bun run --cwd frontend test`（131 通過），全部通過。
  - 這份報告與它的 data 目錄在實作之後的 commit 才加入。
- **本機檢查**：開發者工作站（x86_64 Linux），本機檢查佇列一次一個工作，`CARGO_BUILD_JOBS=2`。
  - 需要容器的測試設了 `CI=1`，以拋棄式的 TLS Postgres（podman）執行，不略過。
  - 工具版本是 `.prototools` 與 `rust-toolchain.toml` 釘選的版本。
- **日期**：2026-10-23。

## 指令

在 repo 根目錄，經本機檢查佇列依序執行：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p contract
cargo test -p testkit
cargo test -p repo-check
CI=1 cargo test -p migrate
cargo run -p openapi -- --check
CI=1 cargo test -p assets --test cross_tenant -- --nocapture
CI=1 cargo test -p assets --test gateway_pool
bun install --cwd scripts --frozen-lockfile
bun test scripts
bun run --cwd scripts typecheck
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
bun run --cwd frontend test
bun run --cwd frontend lint
```

在這之前另跑過一次 `cargo clippy -p assets --all-targets -- -D warnings && CI=1 cargo test -p assets`（整個 assets 套件，包括既有的 `tenant_created`、`device_models`、`migrations`、`background_identity`），全部通過。`api/openapi.json` 以 `cargo run -p openapi` 產生，前端的 `frontend/src/api/schema.gen.ts` 以 `bun run --cwd frontend api:generate` 產生。

## 結果

### 全部通過

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p contract` | 通過，0 失敗。新的 `gateway_bound_decodes_every_version` 解出 fixture `assets.gateway_bound/v1.json`，檢查租戶、actor 與 payload 只有 `binding_id`、`hardware_id` |
| `cargo test -p testkit` | 通過，0 失敗 |
| `cargo test -p repo-check` | 17 + 9 通過 |
| `CI=1 cargo test -p migrate` | 通過，0 失敗。registry 的 assets 現在消費 `["identity", "certs", "assets"]`，Job 依此把 `assets_platform` 加進 `certs.outbox_event` 的讀者 |
| `cargo run -p openapi -- --check` | `api/openapi.json is up to date` |
| `CI=1 cargo test -p assets --test cross_tenant -- --nocapture` | 1 通過；報告見[cross-tenant-report.txt](data/2026-10-23-101-gateway-pool-binding/cross-tenant-report.txt) |
| `CI=1 cargo test -p assets --test gateway_pool` | 6 通過；見[gateway-pool-tests.txt](data/2026-10-23-101-gateway-pool-binding/gateway-pool-tests.txt) |
| `bun test scripts` | 1597 通過、9 略過、0 失敗（`product-logins.test.ts` 的 assets 改為消費 `["identity", "certs", "assets"]`） |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun run --cwd frontend api:check`、`typecheck`、`lint` | 通過 |
| `bun run --cwd frontend test` | 131 通過、0 失敗（含新 message key 的 zh-TW 與 en 翻譯檢查） |

### `tests/gateway_pool.rs` 的六個測試

| 測試 | 證明 |
| --- | --- |
| `factory_registrations_fill_the_pool` | 以 `certs_platform` 照出廠登錄的方式（平台管理員的平台交易、全平台事件）寫 `certs.gateway_registered`：閘道器出現在 `GET /api/assets/v1/platform/gateways/{hardware_id}`，`binding` 是 `null`；`state=unbound` 的列表（每頁 1 筆，跟著 `next_after` 走完）有它，`state=bound` 沒有。同一個硬體 ID 再登錄一次：訂閱處理了第二個事件（`outbox_inbox` 有它）、兩個事件都沒有 dead letter，設備池仍只有一列，`workstation` 仍是第一次的值 |
| `a_binding_is_written_with_its_event_once` | 綁定回 201，資料庫有一列目前的綁定；`assets.outbox_event` 有一個 `assets.gateway_bound`：aggregate `gateway_binding`、ID 是綁定 ID、序號 1，租戶是目標租戶，actor 是做綁定的平台管理員，payload 只有 `binding_id` 與 `hardware_id`。同一個綁定再送回 200、內容相同，列與事件都沒有增加。另一個租戶回 409 `gateway_binding.bound_to_another_tenant`，回應不含原租戶的 ID，什麼都沒寫。設備池顯示這個綁定。日誌 `bound a gateway` 依序 `created` 為 true、false，帶管理員的 `user_id`；日誌沒有 token |
| `a_failed_binding_leaves_nothing_and_the_retry_succeeds` | superuser 建一個 trigger，讓這台閘道器的 `assets.gateway_bound` 寫入失敗（在綁定的 INSERT 之後）：回 503，綁定與事件都沒有留下，ERROR 日誌只有錯誤種類 `outbox database`、沒有資料庫的訊息。拿掉 trigger 後重試回 201（一個綁定、一個事件），再送回 200，另一個租戶回 409 |
| `concurrent_bindings_give_one_binding` | 五次：兩個租戶同時綁定同一台，結果一定是一個 201、一個 409，只有一個綁定與一個事件。五次：同一個租戶同時兩次，一個 201、一個 200，兩個回應相同，只有一個綁定與一個事件 |
| `only_a_platform_administrators_own_request_gets_anywhere` | 三個路由：沒有 token、受眾是別的服務 401；租戶管理員、帶 `act`（別的服務代為交換）的平台管理員 403。被拒的使用者沒有任何 `platform database access` 日誌（在讀任何資料之前就拒絕），拒絕的日誌帶 `user_id`；日誌沒有 token；沒有寫出綁定 |
| `bad_requests_are_refused_and_write_nothing` | 不合硬體 ID 規則的路徑與設備池沒有的閘道器 404；列表參數錯誤（`state=all`、`limit=0`、`limit=501`、`limit=x`、`after=..`、未知參數）400；`Content-Type` 不是 JSON 400；body 超過 4 KiB 400 `gateway_binding.body_too_large`；不是物件 400 `gateway_binding.body_invalid`；缺 `tenant_id`、不是 UUID、未知欄位、服務不認得的租戶各回 422 與對應的 field error；什麼都沒寫 |

### 跨租戶負向測試（`tests/cross_tenant.rs`）

`assets.gateway_bindings` 以 `pg_table!` 宣告（每個租戶 5 列，canary 是硬體 ID）：

| 探測 | 結果 |
| --- | --- |
| read as tenant A / B | 只看得到自己的 5 列，看不到對方的 |
| read without a tenant | 一列都看不到 |
| update own rows | 被拒（42501；服務角色沒有 `UPDATE`） |
| move own rows into the other tenant | 被拒（42501） |
| insert into the other tenant | 被拒（42501） |

`assets.gateway_pool` 沒有 `tenant_id`、服務角色只能 `INSERT`，以理由豁免；三個平台路由只給平台管理員（租戶使用者一律 403，由 `gateway_pool.rs` 測），以理由豁免。既有的表與端點全部照舊通過。完整報告：[cross-tenant-report.txt](data/2026-10-23-101-gateway-pool-binding/cross-tenant-report.txt)。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. 出廠登錄後，該閘道器出現在設備池（測試） | 通過：`factory_registrations_fill_the_pool` |
| 2. 綁定中途失敗後重試成功，不留下卡住的狀態（測試） | 通過：`a_failed_binding_leaves_nothing_and_the_retry_succeeds`；並行時也只有一個綁定：`concurrent_bindings_give_one_binding` |
| 3. 綁定產生事件（測試） | 通過：`a_binding_is_written_with_its_event_once`，以及 contract 的 fixture 測試 |
| 4. 跨租戶負向測試通過 | 通過：`tests/cross_tenant.rs` |

待測：沒有 Kubernetes 上的端到端驗證（nightly 的 smoke 沒有這條流程）；平台管理員的畫面還不存在。

## 安全審查後的修正（2026-10-23）

安全審查沒有 Critical、High 或 Medium。以下修正其中兩項 Low，修正在 `3e6623e1` 之後的 commit。

### L2：設備池消費端遇到表會拒絕的值時，永遠重試

**問題**：outbox 對資料庫錯誤一律重試。如果寫入違反 CHECK（23514）或值太長（22001），這個事件會一直重試，卡住 `assets.certs-events`，之後的閘道器都進不了設備池。

**修正**：
- handler 寫入前先依表的 CHECK 檢查 `workstation`：必須是 1–255 字。不合就是永久錯誤。
- 寫入回 SQLSTATE class 22 或 23514 時也是永久錯誤，事件進 dead letter；其他資料庫錯誤照舊重試。

**新測試**：
- 單元測試 `values_the_table_refuses_are_permanent_and_other_failures_are_retried`：列出哪些 SQLSTATE 是永久錯誤、哪些重試。
- 整合測試 `events_the_pool_refuses_are_dead_lettered_and_later_gateways_still_enter`，依序寫三個事件：
  1. `workstation` 有 256 字的事件：被 handler 拒絕。
  2. 表的 CHECK 拒絕的事件：測試以 superuser 暫時加一個 `NOT VALID` 的 CHECK，寫入時回 23514。
  3. 一台合法的閘道器：照樣進入設備池。

  前兩個事件都在 dead letter，也都不在設備池。

### L3：「拒絕前沒讀任何資料」的測試看不到綁定的租戶交易

**問題**：背景 handle 開租戶交易時只在 DEBUG 記錄，而且不帶使用者，所以測試看不到綁定開的租戶交易。

**修正**：`GatewayBinder::bind` 每開一個租戶交易，就記一行 INFO `tenant transaction for a platform administrator`，帶 `user_id`、`tenant_id`、`hardware_id`、`purpose`，不記 token。

**測試**：`only_a_platform_administrators_own_request_gets_anywhere` 加了對照組：平台管理員本人讀設備池並綁定，讀取有 `platform database access`、綁定有一行上述的 INFO 日誌（含租戶與硬體 ID）。被拒的租戶管理員與帶 `act` 的平台管理員，兩種日誌都沒有。

### 指令與結果

經本機檢查佇列執行，全部通過：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `cargo test -p repo-check` | 17 + 9 通過 |
| `cargo run -p openapi -- --check` | 通過（API 型別沒有改變） |
| `CI=1 cargo test -p assets` | 全部通過：單元測試 21、`gateway_pool.rs` 7，其餘測試檔與之前相同 |

### 證明測試抓得到退化

在檢查用的副本裡暫時改程式碼，跑對應的測試，跑完還原：

| 改動 | 結果 |
| --- | --- |
| 拿掉綁定的兩道檢查（handler 的 `refuse` 與 `GatewayBinder` 的檢查） | `only_a_platform_administrators_own_request_gets_anywhere` 失敗：租戶管理員的綁定回 201，不是 403 |
| 讓表會拒絕的值照舊重試（拿掉 handler 的檢查與 SQLSTATE 對應） | `events_the_pool_refuses_are_dead_lettered_and_later_gateways_still_enter` 失敗：之後的閘道器 60 秒內沒有進入設備池 |

只拿掉其中一道檢查時，回應仍是 403：另一道檢查在任何交易開始前就拒絕了，所以這種退化在行為上看不出來，兩道檢查是互相備援。

## 合併 main（2026-10-23）

把 main `b7da9509` 合併進分支，merge commit 是 `4e414977`。這次的 main 含 Laysi/trustiot-v2#589（nightly 部署 certs）與 Laysi/trustiot-v2#593（平台管理員設定租戶成員角色，事件 `identity.member_role_changed`）。

三個內容衝突，都保留兩邊：

| 檔案 | 解法 |
| --- | --- |
| `crates/contract/README.md` | 事件列表同時列出 `MemberRoleChanged` 與 `GatewayBound`；ID 列表保留 `GatewayBindingId` |
| `crates/contract/tests/events.rs` | 同時匯入 `TenantRole` 與 `GatewayBindingId`；兩個事件的解碼測試都保留 |
| `tools/migrate/README.md` | identity 的檔案列出 `0005_member_roles.sql`；assets 的 `0003_gateway_pool.sql` 與 certs 消費端的說明保留 |

生成的檔案沒有直接相信文字合併的結果，而是重新產生：`cargo run -p openapi`，以及 `bun install --cwd frontend --frozen-lockfile && bun run --cwd frontend api:generate`。產生的 `api/openapi.json` 與 `frontend/src/api/schema.gen.ts`，和合併結果逐位元組相同。

合併後經本機檢查佇列執行，全部通過：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| `CI=1 cargo test -p assets -p contract -p migrate -p repo-check --no-fail-fast` | 全部通過，0 失敗；`gateway_bound_decodes_every_version` 與 `member_role_changed_decodes_every_version` 都通過 |
| `cargo run -p openapi -- --check` | 通過 |
| `bun run --cwd frontend api:check`、`bun run --cwd frontend test` | 通過；131 通過、0 失敗 |
| `bun test scripts` | 1648 通過、9 略過、0 失敗 |

## 日誌斷言改為先等待（2026-10-23）

測試以讀取執行緒收集服務的 stdout（`tests/support/mod.rs` 的 `Output`），所以一行日誌可能比它的回應晚到。`tests/gateway_pool.rs` 原本在回應之後立刻讀日誌，有四處，和 Laysi/trustiot-v2#597（certs 測試在 CI 偶發失敗）是同一類問題。

四處都改用 `wait_for`，先等到預期的日誌行出現，再做原本的斷言：

- 設備池讀取的平台交易日誌行；
- 兩次綁定各一行的 `bound a gateway`；
- 失敗的綁定的 `could not bind a gateway`；
- 允許的綁定開啟租戶交易時的那一行。

等待的上限是新的 `LOG_LIMIT`（30 秒）。日誌行依序到達，所以等到允許的綁定那一行之後，被拒的請求有沒有日誌行，就可以照原本的方式斷言。

經建置主機的檢查佇列執行（`CI=1`）：

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked -p assets --all-targets -- -D warnings` | 通過 |
| `cargo test --locked -p assets --test gateway_pool`，連跑 3 次 | 每次 7 通過、0 失敗 |
