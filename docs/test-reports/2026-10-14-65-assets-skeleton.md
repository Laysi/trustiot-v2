# 資產服務骨架與 TenantCreated 事件（2026-10-14）

對應 issue：Laysi/trustiot-v2#65（決策 4、27、29）。原始資料在 [data/2026-10-14-65-assets-skeleton/](data/2026-10-14-65-assets-skeleton/)。

## 環境

- **受測程式碼**：分支 `feat/65-assets-skeleton`，rebase 到 main `71769da` 之後。全部檢查先在 `255dca9` 跑過一次，修正後在加入本報告的 `4808c96`（程式碼與分支最新的 commit 相同，之後只改報告）整套重跑；中間的 `772dd19`、`ca83d7c` 也各自重跑受影響的測試（見 `workspace-checks.txt`）。
- **Rust 測試**：建置主機（x86_64 Linux，Docker），經檢查佇列執行，`CI=1`，所以需要容器的測試沒有容器時會失敗而不是略過。Postgres 是 testkit 的 TLS Postgres（PostgreSQL 18），產品 schema 由 migration Job 本身套用（`testkit::product`）。Rust 1.99.0、zig 0.16.0、cargo-zigbuild 0.23.4，皆由 `.prototools` 鎖定。
- **chart 與腳本測試**：建置主機與開發用工作站（x86_64 Linux），bun 1.4.2、helm 4.3.0，由 `.prototools` 鎖定。
- **沒有 Kubernetes**：本 repo 規定工作站不跑 Kubernetes。經入口的 k3d 驗證由 nightly 的 smoke 執行（`scripts/nightly/api.ts`），要等 PR 帶 `smoke` label 時跑，見「結論」。

## 指令

在 repo 根目錄，經檢查佇列：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p service-kit --no-default-features --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo run --locked -q -p openapi -- --check
cargo deny --locked check
cargo test --locked -p assets --test cross_tenant --test tenant_created -- --nocapture
cargo test --locked -p identity --test tenants --test cross_tenant -- --nocapture
cargo test --locked -p testkit --test postgres_isolation -- --nocapture a_table_with_one_row_per_tenant
cargo test --locked -p migrate --test registry --test fresh_database --test file_escapes
export CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text)"
cargo zigbuild --locked --release --target aarch64-unknown-linux-musl -p assets -p identity

bun install --cwd scripts --frozen-lockfile
bun test deploy/chart
bun test deploy/flux
bun test scripts
bun run --cwd scripts typecheck
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
bun scripts/ci/lint-workflows.ts
```

## 結果

| 項目 | 結果 |
| --- | --- |
| fmt、clippy（workspace；service-kit 不含 `contract` feature） | 通過 |
| `cargo test --workspace` | 99 個測試執行檔全部 `ok`，沒有失敗 |
| `api/openapi.json` | 與程式一致（身分服務的 `createTenant`、資產服務的 `getAssetsTenant`） |
| `cargo deny` | advisories、bans、licenses、sources 都 ok |
| `bun test deploy/chart` | 313 通過、0 失敗（含新測試：資產服務的兩個密碼來自自己的同一個 Secret） |
| `bun test deploy/flux` | 19 通過 |
| `bun test scripts` | 759 通過、0 失敗、2 略過（第一次跑時 `policy.test.ts` 抓到一個看起來像密鑰的常數名稱，改名後通過） |
| typecheck、secret handling、workflow policy、actionlint、`changes.test.ts` | 通過 |
| aarch64 musl 建置 | `assets`、`identity` 都是靜態連結的 aarch64 ELF |

跨租戶負向測試的報告（`--nocapture`）：

| 服務 | 探測 | 檔案 |
| --- | --- | --- |
| 身分服務 | 39 個，0 失敗：`memberships`、`tenants`（每個租戶一列）、`outbox_event` 各五種資料庫探測；`users`、`sessions`、`outbox_aggregate` 豁免並寫明理由；`POST /api/identity/v1/tenants` 豁免（只給平台管理者、不讀既有租戶的資料，由 `tests/tenants.rs` 測） | [identity-tenants-and-cross-tenant.txt](data/2026-10-14-65-assets-skeleton/identity-tenants-and-cross-tenant.txt) |
| 資產服務 | 23 個，0 失敗：`tenants` 的五種資料庫探測、`GET /api/assets/v1/tenant` 以兩個租戶互相攻擊與沒有租戶的使用者（403）、端點探測前後對方的列沒有改變；消費端的四張表豁免 | [assets-tenant-created-and-cross-tenant.txt](data/2026-10-14-65-assets-skeleton/assets-tenant-created-and-cross-tenant.txt) |
| testkit 的 `one_per_tenant` | RLS 開著時 25 個探測全部通過（同一份報告印兩次：測試本身與 `assert_isolated` 各印一次）；關掉 RLS 後測試斷言套件失敗並指出 `example_settings` | [testkit-one-per-tenant.txt](data/2026-10-14-65-assets-skeleton/testkit-one-per-tenant.txt) |

`tests/tenant_created.rs`（`every_new_tenant_gets_one_row_whatever_is_replayed`）以身分服務自己的 `TenantStore` 建立兩個租戶：資產服務的 API 最後回 200 且只有自己的租戶；`assets.tenants` 剛好一列，以 `outbox::Replay` 重播整個事件記錄、同一個事件再送一次之後仍然一列、`created_at` 不變；日誌的 `created` 依序是 true、false、false；沒有 dead letter；沒有 token、錯的受眾 401，沒有租戶 403，還沒處理的租戶 404。`tests/tenants.rs`（`platform_administrators_create_tenants_with_their_event`）證明租戶列與事件在同一個交易（事件的 `tx_order` 等於列的 `xmin`），撤銷 `identity_app` 寫 outbox 的權限時回 503 且沒有留下列。

## 結論

1. **建立租戶後資產服務最終出現對應的列，事件重播不產生重複（測試）**：通過，`services/assets/tests/tenant_created.rs`（上述）。
2. **經入口帶有效 cookie 呼叫資產服務 API 得到該租戶範圍的資料；沒有 cookie 回 401（k3d 測試）**：**待測**。測試已寫好並有單元測試（`scripts/nightly/api.test.ts`：等待、只送一次 POST、逾時訊息、每個 shape 以 helm 實際渲染 API 檢查的 values），但 k3d 只在 nightly 的 smoke 執行，要等 PR 帶 `smoke` label 的執行或下一次排程；結果出來後補進本報告。服務層級的同一件事（token 的租戶、沒有 token 401）已由 Rust 測試證明。
3. **資產與身分服務的跨租戶負向測試接上 testkit 並通過**：通過（上表）。
