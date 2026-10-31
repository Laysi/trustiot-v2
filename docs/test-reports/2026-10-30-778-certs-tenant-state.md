# certs 訂閱身分服務的租戶狀態（2026-10-30）

Laysi/trustiot-v2#778：certs 新增第二個 outbox 消費端（訂閱 `certs.identity-events`，從頭讀身分服務的 log），以 `identity.tenant_created`（正常、版本 1）與 `identity.tenant_state_changed`（`{state, reason, state_version}`）維護 `certs.tenant_statuses`；只有存下「正常」的租戶換發，停權、刪除、不認得的狀態與沒有紀錄的租戶都讀成停權；兩個消費端任一個落後或 lag 不明時換發回 503。這份報告是驗收條件的證據：全部由自動化測試以 `CI=1` 實際執行，需要容器的測試不略過。

## 環境

- **程式碼**：分支 `feat/778-certs-tenant-state`，以 main `6afacf1f` 為基礎，之後 rebase 到 main `34d863c6`（只多一份測試報告）。容器測試在第一個 commit（rebase 後是 `2f625e91`）上執行；第二個 commit（`029e3333`）只改 migration 的 `lock_timeout`、一行 WARN 與文件，之後以下方「指令」的 Rust 檢查在建置主機上重跑：`cargo fmt --all --check`、clippy 沒有警告，`CI=1 cargo test --locked --workspace --exclude agent` 的 202 個測試執行檔全部通過、0 失敗。`bun test scripts` 與 `cargo deny` 在建置主機缺少 helm 與 cargo-deny，改在本機（x86_64 Linux）執行：`bun test scripts` 抓到 `scripts/tests/product-logins.test.ts` 還寫著 certs 只消費 `assets`，修正後 1711 通過、0 失敗；`cargo deny --locked check` 的 advisories、bans、licenses、sources 都是 ok。
- **機器**：建置主機（arm64 Linux VM），經檢查佇列執行。容器測試由 podman 起 `testkit::product` 的拋棄式 TLS Postgres，以 migration Job 的程式套用整個產品的 schema（含 `0007_tenant_state_events.sql`），再以真的 `certs` 執行檔（服務角色 `certs_app`、平台角色 `certs_platform`）處理事件與換發請求；租戶以身分服務自己的程式碼（`identity::tenants::TenantStore::create`）建立。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的版本。
- **設定**：`CI=1`。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=1 cargo test --locked --workspace --exclude agent
bun install --cwd scripts --frozen-lockfile
bun test scripts
cargo deny --locked check
```

驗收證據可以單獨重跑（`--nocapture` 印出 `evidence:` 行）：

```sh
CI=1 cargo test --locked -p certs --test tenant_states --test tenant_state_lag --test renewal --test bindings -- --nocapture
CI=1 cargo test --locked -p certs
CI=1 cargo test --locked -p migrate
```

## 結果

原始輸出（`evidence:` 行與每個測試執行檔的結果）：[container-tests.txt](data/2026-10-30-778-certs-tenant-state/container-tests.txt)。

- `cargo test -p certs`：單元測試 53 個與 20 個測試執行檔全部通過，0 失敗；`cargo test -p migrate` 全部通過（含 `fresh_database`）。
- `tests/tenant_states.rs`（真的服務在租戶建立與變更之後才啟動，從頭重播）：
  - 身分服務的 5 個租戶與 certs 的副本相同：`active` 1、`suspended` 2、`active` 4（停權、換原因、恢復）、`deleted` 3、`suspended` 8（8 個並行變更，版本 2 到 8 連續；相同的變更不產生新版本）。
  - 亂序到達：版本 5、2、9、3、7、4、8、6（奇數停權）最後是 `suspended` 9；版本 4、7、2、10、3、6 最後是 `active` 10。較舊的每一筆 `stored=false`。
  - 讀成停權：`active`、`resumed`、`ends_active` 是 `false`；`suspended`、`deleted`、`ends_suspended`、`unrecognized`、`never_created`（沒有紀錄）是 `true`。
  - 晚到的舊快照與整個訂閱重播：38 個事件重送，只存下 1 筆（重播之後才寫的哨兵租戶），其他每一列的狀態、版本與 `updated_at` 都不變；0 筆 dead letter。
- `tests/tenant_state_lag.rs`：身分服務寫一個這個版本不認得的 `identity.tenant_state_changed` 版本，消費端重試（日誌 `outbox event failed; retrying`，subscription `certs.identity-events`，reason `a tenant state event of a version newer than this build`），0 筆 dead letter；lag 超過 1 秒後取挑戰回 503 `certificate_renewal.bindings_stale`、`Retry-After: 30`；那個租戶仍是 `active` 1。
- `tests/renewal.rs`：正常的租戶換發成功，讀取日誌帶 `"tenant_status":"active"`；停權（`rn-susp`）、刪除（`rn-del`）、沒有狀態紀錄（`rn-unknown`）的租戶都回 403 `tenant_suspended`、`error.tenant_suspended`，`certs.renewal.refused{reason="tenant_suspended"}` 是 3。
- `tests/bindings.rs`：`deleted` 3 在 `active` 4、`unrecognized` 5 之後仍是 `deleted` 3；`unrecognized` 2 被 `active` 2 取代、不被 `active` 1 取代；表以 23514 拒絕其他狀態值。
- `tools/migrate/tests/fresh_database.rs`：`identity.outbox_event` 的 `consumer_read` policy 列出 `assets_platform`、`certs_platform`、`identity_platform`、`telemetry_query_platform`；`certs_platform` 在 `identity` schema 只碰得到 `outbox_event`，只有 `SELECT`、沒有寫入與 `CREATE`。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. 事件延遲與重播不會讓狀態倒退（測試） | 通過：`tests/tenant_states.rs` 的晚到舊快照與整個訂閱重播，沒有任何一列改變 |
| 2. 停權與恢復交錯且亂序到達，最後的狀態等於最後一次操作（測試） | 通過：`tests/tenant_states.rs` 的兩組亂序事件、8 個並行變更 |
| 3. 從頭重播後 certs 有每個既有租戶的狀態；已刪除與沒有紀錄的租戶讀成停權，換發被拒（測試） | 通過：`tests/tenant_states.rs`（狀態與讀取）、`tests/renewal.rs`（403 `tenant_suspended`） |
| 4. 訂閱落後時換發回 503（測試） | 通過：`tests/tenant_state_lag.rs` |
| 5. `CI=1 cargo test -p certs`、`CI=1 cargo test -p migrate`、`cargo clippy --workspace --all-targets -- -D warnings` 通過 | 通過（見「結果」與 PR 的檢查） |
