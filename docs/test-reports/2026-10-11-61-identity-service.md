# Laysi/trustiot-v2#61 身分服務骨架：ext_authz 端點與內部身分簽發

對應 issue：Laysi/trustiot-v2#61。驗證 `services/identity` 的 ext_authz 端點、內部身分簽發與 chart 的金鑰欄位，逐條對照 issue 的四項驗收條件。服務說明見 [services/identity/README.md](../../services/identity/README.md)。

## 環境

| 項目 | 值 |
| --- | --- |
| 受測 commit | `feat/61-identity-service` 的 `bda79cf`：功能 commit `e50018c`、`9b8e2a2`，加上合併 main `7c2e405` 的 commit。這次合併配合 main 的變更：Postgres CA 改為必填、健康埠分離、testkit 的跨租戶測試 |
| 建置與測試機 | ruisi-wsl（WSL2，Linux 6.18.33.2-microsoft-standard-WSL2 x86_64，20 核，Docker 29.8.2），`CI=1`：找不到容器執行環境時容器測試改為失敗，不會略過。測試用的副本就是 `bda79cf` 的檔案 |
| 本機 | Fedora，核心 7.2.8-300.fc45.x86_64；只跑 gitleaks |
| 工具 | rustc 1.99.0、Bun 1.4.2、Helm 4.3.0、actionlint、cargo-deny（皆由 `.prototools` 固定） |
| Postgres | 測試自行啟動的容器 `postgres:18.6-alpine@sha256:77f5851…`，以自簽 CA 開 TLS，服務以 `verify-full` 連線，而且只信任這個 CA；測完移除 |
| 沒有用到 | Kubernetes、lab 機器（經入口的端到端驗證在 Laysi/trustiot-v2#77） |

## 指令

在 repo 根目錄執行。build host 上每一步都會執行，不因前一步失敗而停止：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo test --locked --workspace --no-fail-fast
cargo test --locked -p service-kit --no-default-features
cargo test --locked -p identity
cargo test --locked -p identity --test cross_tenant -- --nocapture
cargo run --locked -p openapi -- --check
cargo deny --locked check
bun install --cwd scripts --frozen-lockfile && bun run --cwd scripts typecheck
bun test scripts
bun test deploy/chart
bun scripts/ci/check-workflows.ts
bun scripts/ci/check-secret-handling.ts
bun scripts/ci/lint-workflows.ts
bun install --cwd deploy/host --frozen-lockfile
bun run --cwd deploy/host typecheck
bun run --cwd deploy/host test
bun run --cwd deploy/host build
```

本機：

```sh
gitleaks detect --redact --source . --log-opts="origin/main..HEAD"
gitleaks dir --redact <git archive HEAD 解開的目錄>
```

測試用到的祕密都是測試當場產生的：
- 簽章私鑰由固定種子在記憶體產生，寫進 cargo 的暫存目錄。
- 資料庫密碼是測試常數。

沒有任何私鑰進 repo。

## 結果

| 檢查 | 結果 | 原始資料 |
| --- | --- | --- |
| `cargo test -p identity` | 單元 31 個、`cross_tenant` 1 個、`ext_authz` 7 個全部通過；整合測試真的連到 TLS Postgres | [identity-tests.txt](data/2026-10-11-61-identity-service/identity-tests.txt) |
| 跨租戶負向測試（testkit） | 16 個探測、0 個失敗 | [identity-tests.txt](data/2026-10-11-61-identity-service/identity-tests.txt) 的最後一段 |
| fmt；clippy（workspace 與 service-kit 不含預設 feature）；`cargo test --workspace`（73 組測試結果：60 個測試執行檔加上 doc test，0 個失敗）；service-kit 不含預設 feature 的測試；openapi 比對；cargo-deny | 全部通過 | [workspace-checks.txt](data/2026-10-11-61-identity-service/workspace-checks.txt) |
| `bun test deploy/chart`：helm lint、kubeconform、chart 規則、README 欄位檢查、e2e 的單元測試 | 232 個通過 | 同上 |
| `bun test scripts` | 340 個通過；1 個略過，是需要 `TRUSTIOT_DEV_E2E=1` 的 podman 端到端測試，CI 也不跑 | 同上 |
| scripts 型別檢查、workflow 政策、密鑰處理檢查、actionlint | 全部通過 | 同上 |
| deploy/host 的型別檢查、測試（111 個）、建置 | 全部通過 | 同上 |
| gitleaks（本分支的 4 個 commit，以及匯出的 HEAD 目錄樹） | 沒有發現 | [gitleaks.txt](data/2026-10-11-61-identity-service/gitleaks.txt) |

跨租戶負向測試的內容：
- 服務角色 `identity_app` 讀 `identity.memberships` 時，只看得到自己租戶的 2 列。
- 沒有設定租戶時一列也看不到。
- 更新、把列搬到對方租戶、以對方租戶插入，都被拒（`42501`）。
- `identity.users`、`identity.sessions` 不是租戶資料，以理由豁免。
- 服務沒有租戶資料的 API。

過程中發現並修正的問題：

- **`role` 欄位**：`tools/sql-check` 把測試的 `UPDATE identity.memberships SET role = …` 當成切換資料庫角色而擋下。欄位改名為 `tenant_role`，之後的租戶 API（Laysi/trustiot-v2#65）改角色時也不會再撞到。
- **測試私鑰**：gitleaks 把單元測試裡一段 X25519 私鑰（全為 `0x07` 的測試值）判定為私鑰。改成執行時組出 PEM，原始碼裡不再有私鑰區塊。
- **chart 的 values 規則（Laysi/trustiot-v2#276）**：這條規則把 `internalIdentity.publicKeys[].x` 判成「看起來隨機的金鑰」。公鑰不是祕密，所以只豁免這一個欄位；同樣的值放在其他欄位（例如 `d`）仍然會報，有測試證明。
- **健康埠（Laysi/trustiot-v2#286）**：service-kit 改成另開健康埠，預設 8081，平行測試會搶這個埠。測試改用空閒埠，並確認 ext_authz 端點不在健康埠上（回 404）。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 有效 session 的請求，回應帶內部身分標頭，且通過 Laysi/trustiot-v2#52 的驗證 | 通過 | `tests/ext_authz.rs::an_active_session_gets_one_token_that_the_routed_service_accepts`（真的服務、真的 Postgres）：回 200 且剛好一個 `x-internal-identity`，以 service-kit 的 `IdentityVerifier` 驗證通過；使用者、租戶、角色、平台管理者旗標與 60 秒效期都正確；給 `certs` 的 token 被 `assets` 以 `WrongAudience` 拒絕。單元測試 `an_active_session_gets_one_verified_identity_for_the_routed_service`、`tokens_pass_service_kit_verification_for_their_audience_only` 也通過 |
| 請求已帶同名身分標頭時，ext_authz 允許的回應仍只帶身分服務簽的標頭（經入口的覆寫在 Laysi/trustiot-v2#77 驗證） | 通過 | 同一個整合測試對真的服務送出 `x-internal-identity`（一個是先前簽好的真 token、一個是假值）與三種底線寫法：回應仍只有一個標頭，是剛簽的新 token。單元測試 `forged_identity_headers_never_reach_the_allow` 另外涵蓋拒絕的回應一律不帶這個標頭。經入口的覆寫：待測，在 Laysi/trustiot-v2#77 |
| 無效或過期 session 回 401 | 通過 | `invalid_or_expired_sessions_get_401`（真的服務與 Postgres）：沒有 cookie、只有其他 cookie、格式不對、不存在、過期、已撤銷，以及使用者刪除後原本有效的 session，都回 401 且不帶身分標頭。cookie 重複由單元測試 `a_missing_repeated_or_malformed_session_cookie_is_refused` 與 `sessions_that_are_missing_unknown_or_malformed_get_401` 涵蓋 |
| 私鑰不出現在日誌與任何 API 回應 | 通過 | `secrets_never_reach_logs_or_responses`：檢查的對象是允許、拒絕、403 的回應，API 埠與健康埠的 `/healthz`、`/readyz`，不存在的路徑（含 `/.well-known/jwks.json`），以及服務停止後的完整日誌。這些都不含私鑰的 PEM、私鑰的各種編碼與 `PRIVATE KEY` 字樣；日誌也不含 session cookie、簽出的 token 與資料庫密碼。`unusable_keys_and_settings_stop_startup` 確認金鑰不能用時的錯誤訊息不含檔案內容；另有單元測試 `errors_and_debug_output_never_show_the_private_key` |

另外驗證（不在驗收條件內）：
- 改角色與移除成員，下一個請求就生效（`role_changes_apply_to_the_next_request`）。
- 服務角色不能建立或修改 session（`the_service_role_cannot_create_or_change_sessions`）。
- 服務角色讀不到也改不了其他租戶的成員資料（`cross_tenant.rs`）。
- 資料庫連不上時 ext_authz 回 503，兩個埠的 readiness 都回 503（`an_unreachable_database_fails_closed`）。
- 健康埠不提供 ext_authz。
- 公鑰沒部署、同一把公鑰列兩次、路由設定不能用、沒有 Postgres CA 時拒絕啟動（`unusable_keys_and_settings_stop_startup`）。

待測：經 Istio 入口的端到端行為需要叢集，在 Laysi/trustiot-v2#77 驗證，包括：
- Envoy 以身分服務的值覆寫用戶端的標頭。
- 把身分服務的 5xx 轉成 `statusOnError` 503。
- 轉給身分服務的 Host 是原始主機。
