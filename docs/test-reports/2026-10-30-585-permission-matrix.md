# 權限矩陣：assets 與 telemetry-query 的宣告、共用的角色檢查（2026-10-30）

Laysi/trustiot-v2#585：`crates/service-kit` 加上共用的角色檢查（`Requirement`、`UserIdentity::require` 與 `meets`），assets、certs、identity 原本各自寫的檢查改用它；assets 的 `GET /api/assets/v1/tenant` 與 telemetry-query 的四個操作改成要求租戶 viewer，而且在讀參數、body 或資料庫之前檢查；assets 與 telemetry-query 用 Laysi/trustiot-v2#584 的套件宣告每個操作；testkit 加一個單元測試，檢查[權限矩陣](../architecture/permissions.md)涵蓋 `api/openapi.json` 的每個操作。這份報告是驗收條件的證據：全部由自動化測試以 `CI=1` 實際執行，需要容器的測試不略過。

## 環境

- **程式碼**：分支 `feat/585-permission-matrix`，基於 main `f46c8a89`（Laysi/trustiot-v2#584 合併後）。完整檢查在 commit `64d6eb21` 的樹上執行；檢查之後 main 沒有新的 commit。
- **機器**：建置主機（arm64 Linux VM），經檢查佇列執行。容器測試由 podman 起 `testkit::product` 的拋棄式 TLS Postgres，以 migration Job 的程式套用整個產品的 schema，再以真的 `assets`、`telemetry-query`、`identity`、`certs` 執行檔（各自的資料庫角色）回應探測。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2。
- **設定**：`CI=1`。telemetry-query 的探測不起 ClickHouse：服務的 ClickHouse 位址指向測試裡的一個本機埠，這個埠只計算接受的連線數，探測後斷言沒有任何連線。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
CI=1 cargo test --locked --workspace --exclude agent
bun install --cwd frontend --frozen-lockfile && bun run --cwd frontend api:check
bun test scripts/ci/changes.test.ts
bun scripts/ci/check-workflows.ts
bun scripts/ci/check-secret-handling.ts
```

驗收證據可以單獨重跑（`--nocapture` 印出每個探測）：

```sh
CI=1 cargo test -p assets -p telemetry-query -p identity -p certs --test permissions -- --nocapture
cargo test -p service-kit --lib identity::permission
cargo test -p testkit --lib permissions
```

「修正前失敗」的反例在建置主機的工作副本上做：把 `services/assets/src/tenants.rs`、`services/telemetry-query/src/history.rs` 與 `services/telemetry-query/src/semantic/mod.rs` 的修正反向套用（`git apply -R`，其餘的變更與新的測試保留），跑 `cargo test --no-fail-fast -p assets -p telemetry-query --test permissions -- --nocapture`。

## 結果

| 指令 | 在哪裡、哪個樹 | 結果 |
| --- | --- | --- |
| 四個服務的 `tests/permissions.rs` | 建置主機（arm64 Linux VM），`64d6eb21` | 各 2 個測試通過。宣告檢查與探測：assets 192 個、telemetry-query 48 個、identity 141 個、certs 36 個探測，全部 0 失敗；telemetry-query 的探測沒有連到 ClickHouse。assets 與 telemetry-query 的每個探測在 [probes.txt](data/2026-10-30-585-permission-matrix/probes.txt) |
| `service-kit` 的 `identity::permission` 單元測試 | 同上 | 6 個通過（角色階層含換發的 token；不認得的角色、沒有租戶、平台管理員都沒有租戶權限；平台操作只接受本人；每種拒絕都是同一個 403 body） |
| `testkit` 的 `permissions` 單元測試 | 同上 | 10 個通過，其中 `the_matrix_covers_every_operation_of_the_api_spec` 對照 commit 的 `api/openapi.json`，`the_spec_coverage_finds_what_is_missing_twice_or_gone` 檢查漏列、重複與已不存在的操作會被找出來 |
| 反例：拿掉 assets 與 telemetry-query 的修正 | 建置主機（arm64 Linux VM），`08af8ad8` 之前的工作樹 | 如預期失敗：assets `192 probes, 1 failed`（`GET /api/assets/v1/tenant` 的不認得角色得 404，應為 403）；telemetry-query `48 probes, 10 failed`（四個操作的不認得角色，以及 series、statistics、usage 的沒有租戶與沒有租戶的平台管理員，得 400 或 404，應為 403） |
| `cargo fmt --all --check`，兩個 clippy，`cargo run --locked -p openapi -- --check`，`CI=1 cargo test --locked --workspace --exclude agent` | 建置主機（arm64 Linux VM），`64d6eb21` | 全部通過：fmt、兩個 clippy 沒有警告；`api/openapi.json is up to date`；workspace 1935 個測試通過、0 失敗、1 個 ignored |
| `api:check`，`bun test scripts/ci/changes.test.ts`，`bun scripts/ci/check-workflows.ts`，`bun scripts/ci/check-secret-handling.ts` | 本機 | `api:check` 通過；283 個測試通過；workflow policy 與 secret handling 沒有違規 |

反例與完整檢查的輸出在 [checks.txt](data/2026-10-30-585-permission-matrix/checks.txt)。

`cargo deny` 沒有跑：這次沒有改任何相依套件（`Cargo.lock` 與各 `Cargo.toml` 都沒有變）。

探測看到的行為（摘自 probes.txt）：

| 操作 | 拒絕（403 `permission_denied`） | 通過授權 |
| --- | --- | --- |
| assets 的讀取（租戶、型號、設備，六個） | 沒有租戶、沒有租戶的平台管理員、不認得的角色 | viewer、editor、admin 與換發的 viewer token（沒有 body 或資料時得 404 等，不是 401/403） |
| assets 的寫入（型號、匯入、設備，七個） | 沒有租戶、沒有租戶的平台管理員、viewer、不認得的角色 | editor、admin 與換發的 editor token（沒有 body 得 400 等） |
| assets 的設備池（三個平台路由） | 沒有租戶的使用者、每種租戶角色（含不認得的角色）、平台管理員換發的 token | 平台管理員本人（列表 200；沒有 body 得 400，沒有資料得 404） |
| telemetry-query 的四個操作 | 沒有租戶、沒有租戶的平台管理員、不認得的角色 | viewer、editor、admin 與換發的 viewer token（沒有參數或 body 得 400；`latest` 找不到這個租戶的帳號得 404） |

沒有 token、偽造的 token、給其他服務的 token 在每個操作都是 401 `unauthenticated`。assets 的探測前後比對型號、資料點、設備、綁定與 outbox 的筆數，沒有改變。identity 與 certs 改用共用檢查之後，原本的探測（141 與 36 個）與既有的整合測試（包括對拒絕日誌字串的斷言）照樣通過。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 每個服務的每一列都通過 Laysi/trustiot-v2#584 的探測（測試） | 通過：assets 16 個、telemetry-query 4 個操作全部宣告（覆蓋檢查對照 `openapi()`），連同 identity、certs，四個服務 417 個探測 0 失敗 |
| `Unrecognized` 角色在所有租戶路由都得 403（修正前失敗、修正後通過） | 通過：每個要求租戶角色的操作都有「不認得的角色」探測，修正後全部 403；反例中 `GET /api/assets/v1/tenant` 與 telemetry-query 的四個操作失敗 |
| `docs/architecture/permissions.md` 涵蓋 `api/openapi.json` 的全部公開操作（測試） | 通過：`the_matrix_covers_every_operation_of_the_api_spec` 檢查 spec 的每個操作恰好列在一個服務的一節，且矩陣列的操作都還在 spec 裡 |

不在這次範圍：identity 的 `POST /api/identity/v1/tenants` 與 `DELETE /api/identity/v1/sessions` 對換發 token 的處理由 Laysi/trustiot-v2#760 修正，這次沒有改這兩個 handler 與它們的宣告。
