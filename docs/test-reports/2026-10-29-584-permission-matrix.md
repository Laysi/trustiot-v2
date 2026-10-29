# 權限矩陣：文件與宣告式的角色測試（2026-10-29）

Laysi/trustiot-v2#584：[權限矩陣](../architecture/permissions.md)寫進文件，`crates/testkit` 的 `permissions` 模組讓每個服務宣告自己每個操作的要求，測試對照 OpenAPI 文件、矩陣文件與真的服務；先套用 identity 與 certs，並讓 identity 的兩條平台操作紀錄路由拒絕帶 `act` 的 token（Laysi/trustiot-v2#104 2026-10-23 的決定）。這份報告是驗收條件的證據：全部由自動化測試以 `CI=1` 實際執行，需要容器的測試不略過。

## 環境

- **程式碼**：分支 `feat/584-permission-matrix`。實作先基於 main `781073bc` 檢查，之後 rebase 到 main `b7ac543b`（Laysi/trustiot-v2#757，只改 factory/workstation 與 `services/certs/README.md` 的另一段，沒有衝突）。完整檢查（下表最後一列）在 rebase 後的樹上執行。
- **機器**：建置主機（arm64 Linux VM），經檢查佇列執行。容器測試由 podman 起 `testkit::product` 的拋棄式 TLS Postgres，以 migration Job 的程式套用整個產品的 schema，再以真的 `identity`、`certs` 執行檔（各自的 app 與 platform 角色）回應探測。testkit 的自我測試在測試行程裡起範例服務，不需要容器。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust 1.99.0、Bun 1.4.2。
- **設定**：`CI=1`。identity 的探測不設 AI gateway 的管理位址，模型角色 API 對通過授權的呼叫者回 503。

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
cargo test -p testkit --lib permissions
cargo test -p testkit --test permissions
CI=1 cargo test -p identity --test permissions -- --nocapture
cargo test -p identity --lib event_log
CI=1 cargo test -p certs --test permissions -- --nocapture
```

「修正前失敗」的兩個反例在建置主機的工作副本上做，做完還原：

1. 把 `services/identity/src/event_log.rs` 換回 main 的版本（拿掉拒絕 `act` 的修正），跑 `CI=1 cargo test -p identity --test permissions every_operation -- --nocapture`。
2. 把矩陣文件 `GET /api/identity/v1/platform/event-log` 那列的要求從「平台管理員，本人」改成「已登入」，跑 `cargo test -p identity --test permissions the_matrix_document -- --nocapture`。

## 結果

| 指令 | 在哪裡、哪個樹 | 結果 |
| --- | --- | --- |
| 上方「單獨重跑」的五個指令 | 建置主機（arm64 Linux VM），rebase 後的樹 | testkit 單元測試 8 個、自我測試 5 個、identity 2 個（141 個探測）、identity `event_log` 單元測試 6 個、certs 2 個（36 個探測）全部通過。輸出在 [probes.txt](data/2026-10-29-584-permission-matrix/probes.txt) |
| 反例 1：拿掉 event-log 的修正 | 建置主機（arm64 Linux VM），rebase 後的樹 | 如預期失敗：兩條平台操作紀錄路由的「platform administrator, exchanged token」得 200，`141 probes, 2 failed`；還原後通過 |
| 反例 2：矩陣文件與宣告不一致 | 建置主機（arm64 Linux VM），rebase 後的樹 | 如預期失敗：「line 74 says `已登入`, the declaration `平台管理員，本人`」，`30 probes, 1 failed`；還原後通過 |
| 重新產生 `api/openapi.json`（`cargo run -p openapi`）與前端 client（`api:generate`），比對 commit 的檔案 | 建置主機（arm64 Linux VM），rebase 後的樹 | 與 commit 的檔案相同（差異 0 行） |
| `cargo fmt --all --check`，兩個 clippy，`cargo run --locked -p openapi -- --check`，`cargo test --locked --workspace --exclude agent`，`api:check` | 建置主機（arm64 Linux VM），rebase 後的樹 | 全部通過：fmt、兩個 clippy 沒有警告；`api/openapi.json is up to date`；workspace 245 個測試執行檔、1921 個測試通過、0 失敗、1 個 ignored；`api:check` 通過 |
| `bun test scripts/ci/changes.test.ts`，`bun scripts/ci/check-workflows.ts`，`bun scripts/ci/check-secret-handling.ts` | 本機 | 283 個測試通過；workflow policy 與 secret handling 沒有違規 |

反例與完整檢查的輸出在 [checks.txt](data/2026-10-29-584-permission-matrix/checks.txt)。

`cargo deny` 沒有跑：這次沒有改任何相依套件（`Cargo.lock` 與各 `Cargo.toml` 的相依沒有變，testkit 的 `Cargo.toml` 只改了 description）。

探測看到的行為（identity，摘自 probes.txt）：

| 操作 | 換發的 token（帶 `act`） | 其他 |
| --- | --- | --- |
| 兩條平台操作紀錄路由 | 403 `permission_denied`（本次修正） | 平台管理員本人 200；租戶 admin、沒有租戶、不認得的角色 403 |
| `POST /api/identity/v1/tenants` | 400（通過授權，已知落差） | 平台管理員本人 400（沒有 body）；租戶 admin 等 403 |
| `DELETE /api/identity/v1/sessions`（登出所有裝置） | 204（宣告為「已登入」，換發的 token 可以） | 見結論的後續建議 |
| 「已登入，本人」的四個操作 | 403 | 沒有租戶的使用者通過授權 |
| `GET /api/identity/v1/event-log` | 租戶 admin 換發的 token 200 | viewer、editor、沒有租戶的平台管理員 403 |

certs 的三個操作：沒有 token、偽造的 token、給其他服務的 token 都是 401 `unauthenticated`；沒有租戶、租戶 viewer、editor、admin、不認得的角色、平台管理員換發的 token 都是 403 `permission_denied`；平台管理員本人 400（沒有 body）。探測前後比對簽發與撤銷的憑證、出廠登錄、工作站 nonce 與 outbox 的筆數，沒有改變。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| identity 與 certs 的每個操作都有宣告，每一列的探測都通過（測試） | 通過：identity 15 個、certs 3 個操作全部宣告（覆蓋檢查對照 `openapi()`），141 與 36 個探測 0 失敗 |
| 拿掉範例服務的一個角色檢查時，套件失敗（自我測試） | 通過：`each_removed_check_fails_on_its_operation_and_caller` 對七種缺陷各自檢查失敗的操作、呼叫者與失敗數 |
| `docs/architecture/permissions.md` 與宣告不一致時，測試失敗（修正前失敗、修正後通過） | 通過：反例 2 失敗、還原後通過；自我測試另外涵蓋要求不同、少一列、多一列、漏寫落差、重複、缺少整節 |
| identity 的平台操作紀錄路由對帶 `act` 的 token 回 403（測試） | 通過：探測與單元測試 `the_back_office_logs_refuse_exchanged_tokens`；反例 1 證明拿掉修正時探測失敗 |

與 issue 文字的差異：issue 列的要求類別有「只限叢集內呼叫」，但 `openapi()` 目前沒有這種操作（叢集內的路由在 `/internal`，不在 `openapi()`，由 chart 的呼叫矩陣管），所以沒有這個類別；矩陣文件「不在矩陣裡的端點」寫明。
