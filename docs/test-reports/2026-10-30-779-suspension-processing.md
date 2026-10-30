# 停權的「處理中」：certs 確認之前不顯示「已停權」（2026-10-30）

Laysi/trustiot-v2#779：certs 套用租戶狀態後寫 `certs.tenant_state_applied` v1（`{state_version}`），身分服務以訂閱 `identity.tenant-state-applied.certs` 消費，把確認的版本只往大的方向存進 `identity.tenants.certs_applied_version`（`0009_tenant_state_applied.sql`）；`GET`/`PUT /api/identity/v1/platform/tenants/{tenant_id}/state` 的回應多了 `status`：停權在確認的版本小於 `state_version` 時是 `processing`，確認後是 `suspended`，恢復立即是 `active`。certs 端還沒有發出確認（Laysi/trustiot-v2#108），所以測試以 `certs_app` 經 `outbox::publish_as` 在租戶交易裡寫 certs 的 outbox 當替身，寫法與 certs 服務將來的寫法相同。這份報告是驗收條件的證據：全部由自動化測試以 `CI=1` 實際執行，需要容器的測試不略過。

## 環境

- **程式碼**：分支 `feat/779-suspension-pending-ui`，基於 main `6afacf1f`（Laysi/trustiot-v2#776 合併後）。
- **機器**：建置主機（arm64 Linux VM），經檢查佇列執行。容器測試由 podman 起 `testkit::product` 的拋棄式 TLS Postgres，以 migration Job 的程式套用整個產品的 schema（含 `0009_tenant_state_applied.sql`），再以真的 `identity` 執行檔（服務角色 `identity_app`、平台角色 `identity_platform`）回應請求與消費 certs 的 outbox。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust、Bun 與 Helm。
- **設定**：`CI=1`。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
CI=1 cargo test --locked -p identity --test state_applied
CI=1 cargo test --locked --no-fail-fast --workspace --exclude agent
CI=1 cargo test --locked -p migrate
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun install --cwd scripts --frozen-lockfile
bun test deploy/chart
```

驗收證據可以單獨重跑：

```sh
CI=1 cargo test -p identity --test state_applied -- --nocapture
cargo test -p identity --lib tenant_state
cargo test -p contract --test events tenant_state_applied
```

## 結果

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過，沒有警告 |
| `cargo run --locked -p openapi -- --check` | `api/openapi.json is up to date` |
| `CI=1 cargo test --locked -p identity --test state_applied` | 1 個通過、0 失敗 |
| `CI=1 cargo test --locked --no-fail-fast --workspace --exclude agent` | 2028 個通過、0 失敗、1 個 ignored（256 個測試執行檔）。其中 `identity` 的 `tests/state_applied.rs`（下表）、`tests/tenant_state.rs`、`tests/cross_tenant.rs`（服務角色新的欄位 UPDATE 仍只碰得到交易的租戶）、`tests/chart_connections.rs` 與連線池的單元測試（每個消費端在兩個池各有一條連線）、`tenant_state` 的單元測試（`status` 的規則、OpenAPI 的 `TenantStatus`），`contract` 的 `tenant_state_applied_decodes_every_version`（fixture 解碼、偽造的生產者被拒），以及 `migrate` 的測試都通過 |
| `bun run --cwd frontend api:check` | 通過（committed 的 API client 與 `api/openapi.json` 一致） |
| `bun test deploy/chart` | 711 個通過、0 失敗（連線預算：identity 每個副本 33 條，所有服務同時滾動更新時需要 236 條，上限 240） |

Rust 的檢查在建置主機（arm64 Linux VM）上執行，`exit 0 at 20:05:25 (build host)`（整個 workspace）與 `exit 0 at 18:29:57 (build host)`（openapi、`migrate`、`api:check`）。`bun test deploy/chart` 在建置主機（x86_64 Linux）上執行：第一次 710 個通過、1 個失敗（下段的第一個問題），修正後重跑「Postgres connection budget」的 9 個測試全部通過。

整個 workspace 的前一次執行（同一個樹）回報 `error: 1 target failed`，但輸出裡沒有任何失敗的測試或 panic；當時的輸出過濾沒有留下是哪個執行檔，原因不明。同一個樹重跑一次全部通過（上表）。

`tests/state_applied.rs`（`a_suspension_is_processing_until_certs_confirms_its_version`）依序驗證：

| 步驟 | 驗證的行為 |
| --- | --- |
| 1 | 新租戶是 `active`；停權的 `PUT` 回應與之後的 `GET` 都是 `suspended`、版本 2、`processing` |
| 2 | 版本 1（建立）的確認不結束它，仍是 `processing` |
| 3 | 版本 2 的確認處理後是 `suspended`，存的確認版本是 2 |
| 4 | 換原因得版本 3，又是 `processing`；重播版本 2 的確認不結束它；版本 3 的確認後是 `suspended` |
| 5 | 恢復（版本 4）的回應與之後的讀取立即是 `active`，沒有任何確認 |
| 6 | 再停權（版本 5）：恢復的確認（版本 4）不結束它；版本 5 的確認後是 `suspended` |
| 7 | 租戶 B 停權、恢復、再停權到版本 4，確認以 4、2、3 的順序到達：每一步都是 `suspended`，存的確認版本停在 4 |
| 8 | 租戶 C（版本 2）收到版本 7、版本 0、與不存在的租戶的確認：各進死信，原因分別是 `the confirmed state version is newer than the tenant's`、`the confirmed state version is out of range`、`the confirmation names a tenant the identity service does not have`，C 仍是 `processing`、確認版本 0。之後 C 真的停權到版本 6、7，仍是 `processing`；以 `replay_from(START, SkipProcessed)` 重新處理死信，版本 7 的早到確認再進死信（`the confirmation was written before the tenant's state version`），C 仍是 `processing`；certs 在這之後寫的版本 7 確認才讓它變成 `suspended` |
| 9 | certs 全平台的註冊事件被略過，不進死信 |
| 10 | 事件紀錄記下租戶 A 的 6 個確認，`actor_service` 是 certs |
| 11 | 資料庫的規則：`identity_app` 把確認的版本改小、改成 0 得 42501（trigger），超過 `state_version` 得 23514（CHECK），在 A 的交易裡改 B 的列 0 列；superuser 改小也得 42501 |
| 12 | 日誌有 `certs applied a tenant's state` 與三種拒絕的 WARN |

開發中的執行找到兩個問題，都已修正並由上面的執行涵蓋：`deploy/chart/tests/chart.test.ts` 的「identity 多兩個副本」案例仍預期每個副本 31 條連線（改成 33）；安全審查指出版本超前的確認進死信後，若在租戶真的到了那個版本之後被重播會被接受，所以加上「確認寫在租戶進入該版本之後」的條件（`stored_at` 晚於 `state_changed_at`）與步驟 8 的重播測試。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 停權後讀到「處理中」，直到收到同版本或更新版本的確認（測試） | 通過：步驟 1 到 4、6、8 |
| 舊版本的確認不會把較新的停權標成已停權（測試） | 通過：步驟 2、4、6、7，與步驟 8 的重播；資料庫也擋（步驟 11） |
| 恢復後立即讀到正常（測試） | 通過：步驟 5 |
| `cargo run -p openapi -- --check`、`CI=1 cargo test -p identity`、`CI=1 cargo test -p migrate`、`cargo clippy --workspace --all-targets -- -D warnings` 通過 | 通過：見結果表 |

不在這次範圍：certs 實際踢線、等 CRL 生效並發出確認（Laysi/trustiot-v2#108），certs 訂閱租戶狀態（Laysi/trustiot-v2#778），後台畫面（Laysi/trustiot-v2#132）。
