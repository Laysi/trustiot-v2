# Laysi/trustiot-v2#61 身分服務骨架：ext_authz 端點與內部身分簽發

對應 issue：Laysi/trustiot-v2#61。驗證 `services/identity` 的 ext_authz 端點、內部身分簽發與 chart 的金鑰欄位，逐條對照 issue 的四項驗收條件。服務說明見 [services/identity/README.md](../../services/identity/README.md)。

## 環境

| 項目 | 值 |
| --- | --- |
| 受測分支 | `feat/61-identity-service`：功能 commit `e50018c`、`9b8e2a2`。合併 main `e21ff41`（合併 commit `d5a41ff`）後跑 `cargo test -p identity` 的完整清單；再合併 main `b8d1afa`（合併 commit `6cb8f8c`）後重跑其餘所有檢查（第二次合併沒有動到 `services/identity`） |
| 建置與測試機 | ruisi-wsl（WSL2，Linux 6.18.33.2-microsoft-standard-WSL2 x86_64，20 核，Docker 29.7.2），`CI=1`：找不到容器執行環境時容器測試改為失敗，不會略過 |
| 本機 | Fedora，核心 7.2.8-300.fc45.x86_64；只跑 `tunnel-recovery.test.ts` 與 `bun run --cwd scripts typecheck` |
| 工具 | rustc 1.99.0、Bun 1.4.2、Helm 4.3.0（皆由 `.prototools` 固定） |
| Postgres | 測試自行啟動的容器 `postgres:18.6-alpine@sha256:77f5851…`，以自簽 CA 開 TLS，服務以 `verify-full` 連線；測完移除 |
| 沒有用到 | Kubernetes、lab 機器（經入口的端到端驗證在 Laysi/trustiot-v2#77） |

## 指令

從 repo 根目錄執行：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --no-fail-fast
cargo test --locked -p identity
cargo run --locked -p openapi -- --check
cargo deny --locked check
bun test deploy/chart/tests/chart.test.ts deploy/chart/tests/e2e/raw-client.test.ts deploy/chart/tests/e2e/ports.test.ts
bun test deploy/chart/tests/e2e/tunnel-recovery.test.ts
bun install --cwd scripts --frozen-lockfile && bun run --cwd scripts typecheck
gitleaks detect --redact --source . --log-opts="origin/main..HEAD"
```

`tests/ext_authz.rs` 用到的祕密都是測試當場產生的：簽章私鑰由固定種子在記憶體產生並寫進 cargo 的暫存目錄，資料庫密碼是測試常數；沒有任何私鑰進 repo。

## 結果

| 檢查 | 結果 | 原始資料 |
| --- | --- | --- |
| `cargo test -p identity` | 單元 31 個、整合 7 個全部通過（整合測試真的連到 TLS Postgres） | [identity-tests.txt](data/2026-10-11-61-identity-service/identity-tests.txt) |
| fmt、clippy、`cargo test --workspace`（66 個測試執行檔，含 `repo-check`、`sql-check`、`openapi`）、openapi 比對、cargo-deny | 全部通過 | [workspace-checks.txt](data/2026-10-11-61-identity-service/workspace-checks.txt) |
| chart：helm lint、kubeconform、chart 規則、README 欄位檢查（`chart.test.ts`、`raw-client.test.ts`、`ports.test.ts`） | 200 個通過 | [workspace-checks.txt](data/2026-10-11-61-identity-service/workspace-checks.txt) 的最後一段 |
| `tunnel-recovery.test.ts`（本機） | 15 個通過 | [local-checks.txt](data/2026-10-11-61-identity-service/local-checks.txt) |
| scripts 型別檢查（含 `deploy/chart/tests/`） | 通過 | [local-checks.txt](data/2026-10-11-61-identity-service/local-checks.txt) |
| gitleaks（本分支的 commit） | 沒有發現 | 推送前於本機執行 |

過程中發現並修正的問題：

- `tools/sql-check` 把測試的 `UPDATE identity.memberships SET role = …` 當成切換資料庫角色而擋下。欄位改名為 `tenant_role`，之後的租戶 API（Laysi/trustiot-v2#65）改角色時也不會再撞到。
- gitleaks 把單元測試裡一段 X25519 私鑰（全為 `0x07` 的測試值）判定為私鑰。改成執行時組出 PEM，原始碼裡不再有私鑰區塊；`gitleaks dir services/identity deploy/chart` 沒有發現。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 有效 session 的請求，回應帶內部身分標頭，且通過 Laysi/trustiot-v2#52 的驗證 | 通過 | `tests/ext_authz.rs::an_active_session_gets_one_token_that_the_routed_service_accepts`：真的服務、真的 Postgres，回 200 且剛好一個 `x-internal-identity`，以 service-kit 的 `IdentityVerifier` 驗證通過；使用者、租戶、角色、平台管理者旗標與 60 秒效期都正確；給 `certs` 的 token 被 `assets` 以 `WrongAudience` 拒絕。單元測試 `an_active_session_gets_one_verified_identity_for_the_routed_service`、`tokens_pass_service_kit_verification_for_their_audience_only` 也通過 |
| 請求已帶同名身分標頭時，ext_authz 允許的回應仍只帶身分服務簽的標頭（經入口的覆寫在 Laysi/trustiot-v2#77 驗證） | 通過 | 同一個整合測試對真的服務送出 `x-internal-identity`（一個是先前簽好的真 token、一個是假值）與三種底線寫法：回應仍只有一個標頭，是剛簽的新 token。單元測試 `forged_identity_headers_never_reach_the_allow` 另外涵蓋拒絕的回應一律不帶這個標頭。經入口的覆寫：待測，在 Laysi/trustiot-v2#77 |
| 無效或過期 session 回 401 | 通過 | `invalid_or_expired_sessions_get_401`（真的服務與 Postgres）：沒有 cookie、只有其他 cookie、格式不對、不存在、過期、已撤銷，以及使用者刪除後原本有效的 session，都回 401 且不帶身分標頭。cookie 重複由單元測試 `a_missing_repeated_or_malformed_session_cookie_is_refused` 與 `sessions_that_are_missing_unknown_or_malformed_get_401` 涵蓋 |
| 私鑰不出現在日誌與任何 API 回應 | 通過 | `secrets_never_reach_logs_or_responses`：允許、拒絕、403、healthz、readyz 與不存在的路徑（含 `/.well-known/jwks.json`）的完整回應，以及服務停止後的完整日誌，都不含私鑰的 PEM、私鑰的各種編碼、`PRIVATE KEY` 字樣；日誌也不含 session cookie、簽出的 token 與資料庫密碼。`unusable_keys_and_settings_stop_startup` 確認金鑰不能用時的錯誤訊息不含檔案內容；單元測試 `errors_and_debug_output_never_show_the_private_key` |

另外驗證（不在驗收條件內）：改角色與移除成員在下一個請求生效（`role_changes_apply_to_the_next_request`）；服務角色不能建立或修改 session、RLS 擋住其他租戶的成員資料（`the_service_role_cannot_create_or_change_sessions`）；資料庫連不上時 ext_authz 回 503、readiness 回 503（`an_unreachable_database_fails_closed`）；公鑰沒部署、同一把公鑰列兩次、路由設定不能用時拒絕啟動（`unusable_keys_and_settings_stop_startup`）。

待測：經 Istio 入口的端到端行為（Envoy 以身分服務的值覆寫用戶端的標頭、把身分服務的 5xx 轉成 `statusOnError` 503、轉給身分服務的 Host 是原始主機），需要叢集，在 Laysi/trustiot-v2#77 驗證。
