# 本機開發環境為 `PRODUCT` 的每個登入角色綁密碼的測試報告（2026-10-15）

對應 issue：Laysi/trustiot-v2#407（本機開發環境：migration Job 的密碼綁定要涵蓋 `PRODUCT` 的每個服務登入角色）。

## 環境

- 開發工作站（x86_64，Linux），podman 6.1.3，Bun 1.4.2，Rust 1.99.0；`CARGO_BUILD_JOBS=2`，cargo 的 target 目錄在 worktree 內。
- 測試的分支：`fix/407-dev-product-logins`，以 `origin/main`（`8e29d03`，含資產服務，Laysi/trustiot-v2#411）為基底。此時 `PRODUCT` 有兩個服務：`identity`（有平台角色，chart 不綁，維持 NOLOGIN）與 `assets`（outbox 消費端，chart 綁 `assets_app` 與 `assets_platform`）。
- 映像照 `scripts/lib/stack.ts` 以 digest 釘選；沒有使用 Kubernetes。
- 端對端測試用自己的實例名稱（`e2e-<隨機>`）、狀態目錄（暫存目錄的 `XDG_STATE_HOME`）與隨機埠，不動平常使用的環境；一次只有一份端對端測試在這台工作站執行（鎖在 coordinator 的目錄）。
- 靜態檢查在建置主機上跑（check queue，label 407）。

## 指令

```sh
bun install --cwd scripts --frozen-lockfile
bun run --cwd scripts typecheck
bun test scripts
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/check-workflows.ts
cargo test --locked -p repo-check
# 建置主機（check queue，label 407）跑上面全部；端對端測試在工作站：
CARGO_BUILD_JOBS=2 TRUSTIOT_DEV_E2E=1 bun test scripts/tests/e2e.test.ts
```

## 結果

| 檢查 | 結果 |
| --- | --- |
| `bun run --cwd scripts typecheck` | 通過 |
| `bun test scripts`（建置主機） | 829 通過、2 略過（podman 端對端測試與一個 aarch64 測試，本來就需要環境）、0 失敗 |
| `bun scripts/ci/check-secret-handling.ts`、`bun scripts/ci/check-workflows.ts` | 無違規 |
| `cargo test --locked -p repo-check` | 通過 |
| `TRUSTIOT_DEV_E2E=1 bun test scripts/tests/e2e.test.ts` | 1 通過、0 失敗、178 個斷言，227.69 秒 |
| 對照：在 `registry.rs` 加一個服務、在 `_migrate.tpl` 的 `platformLogins` 加 `identity` | `bun test scripts/tests/product-logins.test.ts` 失敗，兩則訊息各指出要改 `scripts/lib/product-logins.ts` 的哪個清單 |

端對端測試新增的斷言（`scripts/tests/e2e.test.ts`）：

- `productLogins()` 是 `identity_app`、`assets_app`、`assets_platform`；state 檔對每一個有正確的角色名稱與 64 位十六進位密碼，三個密碼互不相同。
- `pg_roles`：`assets_app`、`assets_platform`、`identity_app`、`trustiot_owner` 可登入；`assets_owner`、`identity_owner`、`identity_platform` 不可登入；沒有 superuser。
- `env` 印出每個登入角色的 `TRUSTIOT_DEV_<服務>[_PLATFORM]_DATABASE_{USERNAME,PASSWORD}`，與 state 檔一致；以這些帳密經 TLS（`verify-full`，開發 CA，連 `127.0.0.1`）登入已發佈的 Postgres，`current_user` 等於該角色。
- 沒綁密碼的 `identity_platform`、`identity_owner`、`assets_owner` 即使給了密碼也登入失敗。
- 第二次 `up` 之後每個登入角色的密碼都換了，新密碼可登入、舊密碼失敗。
- 每個登入角色的密碼都不出現在 `up`、`status`、`seed` 的輸出與身分服務的日誌（原本只檢查 `identity_app`）。

原始資料（相對連結）：

- [e2e-output.txt](data/2026-10-15-407-dev-product-logins/e2e-output.txt)：端對端測試的輸出（略去編譯行；輸出不含任何密碼）。
- [rcheck-output.txt](data/2026-10-15-407-dev-product-logins/rcheck-output.txt)：建置主機上的檢查摘要。
- [mutation-output.txt](data/2026-10-15-407-dev-product-logins/mutation-output.txt)：暫時改 `registry.rs` 與 `_migrate.tpl` 後單元測試的失敗訊息（改動已還原，不在 commit 裡）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| `up` 之後，`PRODUCT` 的每個服務登入角色都能以 `env` 印出的帳密登入本機 Postgres，有自動測試 | 通過：端對端測試逐一登入 `identity_app`、`assets_app`、`assets_platform`；chart 不綁的 `identity_platform` 與各 owner 角色維持不能登入 |
| 在 `PRODUCT` 加一個服務而不更新本機清單時，`bun test scripts` 失敗，訊息說明要改哪裡 | 通過：`tests/product-logins.test.ts` 讀 `registry.rs` 與 `_migrate.tpl`；`mutation-output.txt` 是實際失敗的訊息，單元測試也以假的 registry 與 chart 內容覆蓋這兩種漂移 |
| `scripts/README.md` 的「身分服務」一節改寫成適用每個服務 | 通過：Job 與密碼的部分搬到新的「migration Job 與服務的登入角色」一節並改寫為適用每個服務，「身分服務」只留簽章金鑰與開發 session |

`scripts/lib/product-logins.ts` 的清單是人寫的（`PRODUCT_SERVICES`、`PLATFORM_LOGIN_SERVICES`），由單元測試對照 `registry.rs` 與 `_migrate.tpl`，所以漂移會在 `bun test scripts` 失敗，而不是在執行期被悄悄略過。
