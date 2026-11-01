# 方案權益模型與伺服器端檢查（2026-11-01）

Laysi/trustiot-v2#111：身分服務管理每個租戶的方案（Starter、Business、Enterprise、Enterprise On-Prem）、Enterprise 的合約覆寫值與權益快照（`0010_entitlements.sql`、`services/identity/src/entitlements.rs`），每次快照改變在同一個交易寫出 `identity.tenant_entitlements_changed` v1（`{plan, entitlements, entitlements_version}`，`crates/contract` 的 `events::identity::TenantEntitlementsChanged`）；各服務檢查用的型別與拒絕回應在 `contract::entitlements`。平台管理員以 `GET`／`PUT /api/identity/v1/platform/tenants/{tenant_id}/entitlements` 讀取與設定，租戶成員以 `GET /api/identity/v1/entitlements` 讀自己租戶的權益。權益清單與各服務的檢查點在 [docs/product/plans.md](../product/plans.md)「權益清單」。這份報告是驗收條件的證據：全部由自動化測試以 `CI=1` 實際執行，需要容器的測試不略過。

## 環境

- **程式碼**：Laysi/trustiot-v2#111 的分支，基於 main `0627c59a`（之後 main 只多了 Flux 的版本釘選，與這次的檔案無關）。
- **機器**：建置主機（x86_64 Linux）與建置主機（arm64 Linux VM），經檢查佇列執行；前端與 `scripts/` 的 Bun 測試在工作站執行。容器測試由 podman 起 `testkit::product` 的拋棄式 TLS Postgres，以 migration Job 的程式套用整個產品的 schema（含 `0010_entitlements.sql`），再以真的 `identity` 執行檔（服務角色 `identity_app`、平台角色 `identity_platform`）回應請求。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 與 Bun。
- **設定**：`CI=1`。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
cargo test --locked -p contract
CI=1 cargo test --locked -p identity
cargo test --locked -p testkit
cargo test --locked -p repo-check
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend test
```

驗收證據可以單獨重跑：

```sh
CI=1 cargo test --locked -p identity --test entitlements
CI=1 cargo test --locked -p identity --test tenants
cargo test --locked -p identity --lib entitlements
cargo test --locked -p contract --test events tenant_entitlements_changed
cargo test --locked -p contract --lib entitlements
```

## 結果

全部通過（2026-11-01）：

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings` | 通過 |
| `cargo run -p openapi -- --check` | 通過（新增 3 個操作與 `Entitlements`、`Limit`、`Plan`、`EntitlementOverrides`、`SetEntitlements`、`TenantEntitlements`、`CurrentEntitlements` 等 schema） |
| `cargo test -p contract` | 單元 97、`tests/events.rs` 23 個通過（含 `tenant_entitlements_changed_decodes_every_version` 與 `entitlements` 模組的上限、序列化、拒絕回應測試） |
| `cargo test -p identity --lib entitlements` | 7 個通過 |
| `CI=1 cargo test -p identity` | 每個測試檔都通過，含 `tests/entitlements.rs`、`tests/tenants.rs`、`tests/event_log.rs`、`tests/cross_tenant.rs`、`tests/permissions.rs`、`tests/tenant_state.rs`、`tests/members.rs`、`tests/login.rs`（路由清單加上新端點後重跑） |
| `cargo test -p testkit -p repo-check -p openapi` | 通過 |
| `bun run --cwd frontend api:check`、`typecheck`、`lint`、`test` | 通過（680 個測試，含伺服器訊息 key 的翻譯檢查） |
| `bun test scripts/ci/changes.test.ts` | 342 個通過 |

`tests/entitlements.rs`（一個測試，共用一個產品資料庫）實際檢查的事：

1. 服務啟動前以只寫兩欄的方式建立的舊租戶（版本 0），在服務啟動後補上 Starter 第 1 版，outbox 有一筆事件，actor 是 `{"kind": "service", "service": "identity"}`，payload 是完整快照與版本 1。
2. 經 API 建立的新租戶是 Starter 第 1 版；平台的 `GET` 與成員的 `GET /api/identity/v1/entitlements` 都回同一份快照。
3. 升級到 Business 是版本 2；同樣的請求（`overrides` 省略、`null`、`{}`）回 200、資料列與 outbox 都不變。
4. Enterprise 帶覆寫值 `{"max_gateways": 50, "max_members": "unlimited", "sso": false}` 是版本 3，快照中這三項是覆寫值、其餘是 Enterprise 的預設值，成員讀到的也是覆寫後的值；拿掉覆寫值是版本 4、再加回是版本 5。outbox 裡這個租戶的五筆事件依序是版本 1–5 的完整快照，actor 是平台管理員。
5. 租戶有 5 個成員時降到 Starter（成員上限 3）：版本 6，5 個成員都還在，之後經成員 API 改其中一人的角色仍回 200。
6. 另一個租戶的 viewer 只讀到自己租戶；assets 代 A 租戶 viewer 換發的 token 讀到 A。
7. 4 個同時送出的相同升級都回 200 與版本 2，只多一筆事件。
8. 拒絕（租戶 viewer、租戶 admin、平台管理員換發的 token 403；沒有 token 401；給 assets 的 token 401；沒有租戶 403；停權的租戶 403 `tenant_suspended`；不存在的租戶 404；壞的租戶 ID、四種非 JSON 的 `Content-Type`、超過 8192 位元組、五種壞的 body 400；五種 422 各帶自己的欄位與 key）之後，所有租戶的權益欄位與 outbox 筆數都不變。
9. superuser 改 `entitlements_changed_at` 被 trigger 改回；服務角色在租戶交易裡：照規則加一的更新與 `FOR NO KEY UPDATE` 可以；改時間、`INSERT` 指定方案或版本 42501；版本不變、跳號、沒改內容卻加版本、改小 42501；非 Enterprise 的覆寫值、不認得的方案、拿掉快照、覆寫值不是物件 23514；在 A 的交易更新 B 影響 0 列、讀不到 B。
10. 日誌有 `changed a tenant's entitlements` 與 `gave a tenant its first entitlements`。

`tests/tenants.rs` 另外確認建立租戶時列、`identity.tenant_created`、Starter 第 1 版的快照與 `identity.tenant_entitlements_changed`（aggregate `tenant_entitlements`、序號 1、actor 同一位平台管理員）在同一個交易；撤銷服務角色寫權益欄位的權限時建立租戶回 503 且沒有留下租戶。`tests/cross_tenant.rs` 以 testkit 的探測確認 `GET /api/identity/v1/entitlements` 只回自己租戶的列（正向對照看得到自己的租戶 ID、看不到另一個租戶）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 各服務可取得租戶目前的權益（API 與事件的測試） | 通過：API 見上方 2、4、6；事件見 1、4 與 `tests/tenants.rs`；事件契約與固定樣本見 `crates/contract/tests/events.rs` |
| Enterprise 覆寫值優先於方案預設（測試） | 通過：上方 4，及單元測試 `overrides_take_precedence_over_the_plan` |
| 權益變更（含降級）不擋資料寫入（測試；已啟用的警報不受影響改由 Laysi/trustiot-v2#167 驗收） | 通過：上方 5（降級後成員都在、成員角色照常可寫）；這次沒有任何寫入路徑讀權益。已啟用的警報依 issue 由 Laysi/trustiot-v2#167 驗收 |
| 權益清單與各服務的檢查點寫進 docs/product/plans.md | 完成：「權益清單」一節；單元測試 `the_plans_document_states_these_values` 確認表格與程式的預設值逐格相同 |

各服務自己的檢查點（閘道器數、成員數、規則數、可查期間、採樣間隔、即時圖表、通知通道與進階功能）依 issue 的範圍不在這次，在 plans.md 對照表列出的各 issue 實作。
