# Laysi/trustiot-v2#61 身分服務骨架：ext_authz 端點與內部身分簽發

對應 issue：Laysi/trustiot-v2#61。驗證 `services/identity` 的 ext_authz 端點、內部身分簽發與 chart 的金鑰欄位，逐條對照 issue 的四項驗收條件；也記錄獨立驗證（[2026-10-11-61-verify](2026-10-11-61-verify.md)）發現的 F1、F3 修正後的結果。服務說明見 [services/identity/README.md](../../services/identity/README.md)。

## 環境

| 項目 | 值 |
| --- | --- |
| 受測 commit | `feat/61-identity-service` 的 `0767a2d`：功能 commit `e50018c`、`9b8e2a2`，驗證者的測試 `71f6989`、`b19fb27`，F1 修正 `c36bc5f`、F3 修正 `69ce7b1`、讓驗證者測試符合 `tools/sql-check` 的 `0767a2d`，以及合併 main `1ad700f`（#321 依路徑決定 CI job、#322 測試時序、#278 OpenTelemetry collector）的 `1a675d6` |
| 建置與測試機 | 本機 Fedora，核心 7.2.8-300.fc45.x86_64，8 核，podman；經共用的檢查佇列一次一個工作執行，`CI=1`（找不到容器執行環境時容器測試改為失敗，不會略過）、`CARGO_BUILD_JOBS=6`。cargo 的 target 目錄是這個 worktree 自己的 `./target`，不與其他 worktree 共用 |
| 工具 | rustc 1.99.0、Bun 1.4.2、Helm 4.3.0、actionlint、cargo-deny、gitleaks（皆由 `.prototools` 固定） |
| Postgres | 測試自行啟動的容器 `postgres:18.6-alpine@sha256:77f5851…`，以自簽 CA 開 TLS，服務以 `verify-full` 連線，而且只信任這個 CA；測完移除 |
| 沒有用到 | Kubernetes、lab 機器（經入口的端到端驗證在 Laysi/trustiot-v2#77） |

前一版報告（`bda79cf`，在 ruisi-wsl 上跑）的結果由這一版取代。佇列在 2026-10-11 晚上以前讓所有 worktree 共用一個 cargo target 目錄，可能拿到別的分支編好的 crate，所以本報告的結果全部在這個 worktree 自己的 target 目錄重跑；唯一沿用的是 F1 修正前的對照（見「F1」），那次 cargo 從這個 worktree 重新編譯了 `contract`、`service-kit`、`identity`、`testkit`，日誌附在原始資料。

## 指令

在 repo 根目錄執行。每一步都會執行，不因前一步失敗而停止：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings
cargo test --locked --workspace --no-fail-fast
cargo test --locked -p service-kit --no-default-features
cargo test --locked -p identity
cargo test --locked -p identity --test cross_tenant -- --nocapture
cargo test --locked -p identity --test ext_authz a_slow_database_connect -- --nocapture
# 同樣的資料庫測試，另開 2 × nproc 個忙迴圈（8 核機器上 16 個）壓 CPU：
cargo test --locked -p identity --test ext_authz --test adversarial
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

gitleaks：

```sh
gitleaks detect --redact --source . --log-opts="origin/main..HEAD"
gitleaks detect --redact --source . --log-opts="--full-history --diff-filter=tuxdb --diff-merges=first-parent HEAD"
gitleaks dir --redact <git archive HEAD 解開的目錄>
```

測試用到的祕密都是測試當場產生的：
- 簽章私鑰由固定種子在記憶體產生，寫進 cargo 的暫存目錄。
- 資料庫密碼是測試常數。

沒有任何私鑰進 repo。

## 結果

| 檢查 | 結果 | 原始資料 |
| --- | --- | --- |
| `cargo test -p identity` | 單元 31 個、`adversarial` 11 個、`cross_tenant` 1 個、`ext_authz` 8 個、`token_claims` 8 個全部通過；整合測試真的連到 TLS Postgres | [identity-tests.txt](data/2026-10-11-61-identity-service/identity-tests.txt) |
| 跨租戶負向測試（testkit） | 16 個探測、0 個失敗 | 同上 |
| 連線很慢時（F1，見下節） | 就緒時 3 條連線；就緒後 20 個檢查全部 200，沒有開新連線；切斷所有連線後 4 個檢查回 503（不帶身分標頭），第 5 個回 200，之後 10 個都 200 | 同上的最後一段 |
| CPU 壓力下的資料庫測試 | 16 個忙迴圈（8 核），測完時 1 分鐘平均負載 17.73：`adversarial` 11 個、`ext_authz` 8 個全部通過 | [identity-under-load.txt](data/2026-10-11-61-identity-service/identity-under-load.txt) |
| fmt；clippy（workspace 與 service-kit 不含預設 feature）；`cargo test --workspace`；service-kit 不含預設 feature 的測試；openapi 比對；cargo-deny | 全部通過（exit 0） | [workspace-checks.txt](data/2026-10-11-61-identity-service/workspace-checks.txt)；為了縮短輸出，`cargo test --workspace` 只留最後 40 行摘要，判定以 exit code 為準 |
| `bun test deploy/chart`：helm lint、kubeconform、chart 規則、README 欄位檢查、e2e 的單元測試 | 261 個通過 | 同上 |
| `bun test scripts` | 518 個通過；1 個略過，是需要 `TRUSTIOT_DEV_E2E=1` 的 podman 端到端測試，CI 也不跑 | 同上 |
| scripts 型別檢查、workflow 政策、密鑰處理檢查、actionlint | 全部通過 | 同上 |
| deploy/host 的型別檢查、測試（111 個）、建置 | 全部通過 | 同上 |
| gitleaks（本分支的 10 個 commit、CI 掃的整段 first-parent 歷史 77 個 commit、匯出的 HEAD 目錄樹） | 沒有發現 | [gitleaks.txt](data/2026-10-11-61-identity-service/gitleaks.txt) |

跨租戶負向測試的內容：
- 服務角色 `identity_app` 讀 `identity.memberships` 時，只看得到自己租戶的 2 列。
- 沒有設定租戶時一列也看不到。
- 更新、把列搬到對方租戶、以對方租戶插入，都被拒（`42501`）。
- `identity.users`、`identity.sessions` 不是租戶資料，以理由豁免。
- 服務沒有租戶資料的 API。

### F1：開新連線比檢查期限慢

獨立驗證發現：連線池取得連線的逾時就是檢查的 0.4 秒，開到一半的連線跟著檢查一起被取消；readiness 只要一條連線，一個檢查卻可能要兩條。負載高的機器上開一條連線（TCP、TLS、SCRAM、service-kit 的 session 檢查）超過 0.4 秒時，連線池永遠補不到連線。

重現不靠機器負載：新測試 `a_slow_database_connect_never_fails_a_check_after_ready` 在 Postgres 前面放一個 TCP 代理（`tests/support/mod.rs` 的 `SlowProxy`），每條新連線先等 1 秒才接到伺服器。

| | 結果 | 原始資料 |
| --- | --- | --- |
| 修正前（`388e9b7` 的程式加上這個測試） | 30 秒內一直沒有就緒，readiness 記 `pool timed out while waiting for an open connection`；測試失敗 | [f1-slow-connect-before.txt](data/2026-10-11-61-identity-service/f1-slow-connect-before.txt) |
| 修正後（`0767a2d`） | 就緒、檢查不開新連線、斷線後自動恢復（上表） | [identity-tests.txt](data/2026-10-11-61-identity-service/identity-tests.txt) |

修正的內容（`c36bc5f`，設計在 services/identity/README.md 的「連線池」）：
- 取得連線（含開新連線）的逾時改為 5 秒，與檢查期限分開。
- 每次查詢在自己的 task 裡跑：檢查到 0.4 秒照樣回 503，但開到一半的連線會開完，留給下一個檢查。同時在跑的查詢每條連線最多 4 個，每個最多 10 秒。
- 背景的保溫工作讓連線池常駐 3 條連線，開好之後 readiness 才通過，所以入口只把檢查送給不必等新連線的副本。
- `TRUSTIOT__DATABASE__MAX_CONNECTIONS` 的下限因此從 1 改為 3。

### F3：簽章私鑰檔的權限

chart 原本以 Kubernetes 預設的 0644 掛載簽章私鑰。現在（`69ce7b1`）是 0440，擁有者是 root 與 pod 的 `fsGroup`；身分服務的 `fsGroup` 預設 65532，服務以群組身分讀檔，容器裡其他使用者讀不到。OpenShift 把 `services.identity.podSecurityContext.fsGroup` 設成 `null`，由 restricted-v2 SCC 指派，權限仍是 0440。chart 測試確認：
- 只有身分服務有 `fsGroup`。
- 設成 `null` 時 `fsGroup` 消失，但 `defaultMode` 仍是 0440。

### 過程中發現並修正的問題

- **`role` 欄位**：`tools/sql-check` 把測試的 `UPDATE identity.memberships SET role = …` 當成切換資料庫角色而擋下。欄位改名為 `tenant_role`，之後的租戶 API（Laysi/trustiot-v2#65）改角色時也不會再撞到。
- **測試私鑰**：gitleaks 把單元測試裡一段 X25519 私鑰（全為 `0x07` 的測試值）判定為私鑰。改成執行時組出 PEM，原始碼裡不再有私鑰區塊。
- **chart 的 values 規則（Laysi/trustiot-v2#276）**：這條規則把 `internalIdentity.publicKeys[].x` 判成「看起來隨機的金鑰」。公鑰不是祕密，所以只豁免這一個欄位；同樣的值放在其他欄位（例如 `d`）仍然會報，有測試證明。
- **健康埠（Laysi/trustiot-v2#286）**：service-kit 改成另開健康埠，預設 8081，平行測試會搶這個埠。測試改用空閒埠，並確認 ext_authz 端點不在健康埠上（回 404）。
- **驗證者的測試違反 `tools/sql-check`**：`tests/adversarial.rs` 原本以 `format!` 與 `AssertSqlSafe` 組 `CREATE DATABASE`、`LOCK TABLE`，`cargo test --workspace` 因此失敗。`0767a2d` 改成靜態 SQL（fixture 收靜態的 `CREATE DATABASE` 敘述，與 `tests/ext_authz.rs` 相同；鎖表走靜態敘述的表格）。檔案裡對服務角色 session 的攻擊保留：以 `set_config` 設不存在、空白、格式錯誤的租戶，以及 `SET ROLE`、`SET SESSION AUTHORIZATION`。`tools/sql-check` 只對這個測試檔免除 `SessionSetting`，理由寫在例外清單，與 ingest 的 ClickHouse 測試相同。
- **合併 main（`1a675d6`）**：與 #278 的 collector 在 chart 衝突三處，都保留兩邊：
  - 受保護的 Secret 參照同時列出簽章私鑰與 collector 的 Secret。
  - 服務 env 的註解同時寫 collector 位址與身分服務的設定。
  - chart README 的密鑰約定兩句都留。

佇列環境的問題（與本分支無關）：上面的完整檢查通過之後，為了留下完整的 `cargo test --workspace` 清單再跑一次，所有用容器的測試（outbox、protocol 的 broker、service-kit、testkit 等）都失敗。原因是 podman 的 lock 用完：`allocating lock for new volume: allocation failed; exceeded num_locks (2048)`。主機上累積了約 2,000 個沒有容器引用的匿名 volume，大多建立於 2026-10-10、2026-10-11，是測試的 Postgres 容器移除後留下的。清理主機與修正測試的容器清理不在本分支範圍，交給協調者處理；本報告的結果取自 lock 用完之前那次完整通過的執行。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 有效 session 的請求，回應帶內部身分標頭，且通過 Laysi/trustiot-v2#52 的驗證 | 通過 | `tests/ext_authz.rs::an_active_session_gets_one_token_that_the_routed_service_accepts`（真的服務、真的 Postgres）：回 200 且剛好一個 `x-internal-identity`，以 service-kit 的 `IdentityVerifier` 驗證通過；使用者、租戶、角色、平台管理者旗標與 60 秒效期都正確；給 `certs` 的 token 被 `assets` 以 `WrongAudience` 拒絕。單元測試 `an_active_session_gets_one_verified_identity_for_the_routed_service`、`tokens_pass_service_kit_verification_for_their_audience_only` 也通過；驗證者的 `token_claims.rs` 另外確認竄改、改簽、改受眾、改時間的 token 都被拒 |
| 請求已帶同名身分標頭時，ext_authz 允許的回應仍只帶身分服務簽的標頭（經入口的覆寫在 Laysi/trustiot-v2#77 驗證） | 通過 | 同一個整合測試對真的服務送出 `x-internal-identity`（一個是先前簽好的真 token、一個是假值）與三種底線寫法：回應仍只有一個標頭，是剛簽的新 token。單元測試 `forged_identity_headers_never_reach_the_allow` 另外涵蓋拒絕的回應一律不帶這個標頭。經入口的覆寫：待測，在 Laysi/trustiot-v2#77 |
| 無效或過期 session 回 401 | 通過 | `invalid_or_expired_sessions_get_401`（真的服務與 Postgres）：沒有 cookie、只有其他 cookie、格式不對、不存在、過期、已撤銷，以及使用者刪除後原本有效的 session，都回 401 且不帶身分標頭。cookie 重複由單元測試 `a_missing_repeated_or_malformed_session_cookie_is_refused` 與 `sessions_that_are_missing_unknown_or_malformed_get_401` 涵蓋；驗證者的 `cookie_parsing_accepts_only_one_exact_session_cookie`、`session_expiry_and_revocation_are_decided_by_the_database_clock` 也通過 |
| 私鑰不出現在日誌與任何 API 回應 | 通過 | `secrets_never_reach_logs_or_responses`：檢查的對象是允許、拒絕、403 的回應，API 埠與健康埠的 `/healthz`、`/readyz`，不存在的路徑（含 `/.well-known/jwks.json`），以及服務停止後的完整日誌。這些都不含私鑰的 PEM、私鑰的各種編碼與 `PRIVATE KEY` 字樣；日誌也不含 session cookie、簽出的 token 與資料庫密碼。驗證者的 `nothing_secret_is_logged_at_any_level` 在 info 到 trace（含 sqlx、hyper、rustls 的 trace）各等級重複檢查。`unusable_keys_and_settings_stop_startup` 確認金鑰不能用時的錯誤訊息不含檔案內容；另有單元測試 `errors_and_debug_output_never_show_the_private_key` |

另外驗證（不在驗收條件內）：
- 改角色與移除成員，下一個請求就生效（`role_changes_apply_to_the_next_request`）。
- 服務角色不能建立或修改 session（`the_service_role_cannot_create_or_change_sessions`）。
- 服務角色讀不到也改不了其他租戶的成員資料（`cross_tenant.rs`、`memberships_are_isolated_by_tenant_and_by_user`）。
- 資料庫連不上、被鎖、連線被殺、帳密錯誤時 ext_authz 回 503 且不放行，兩個埠的 readiness 都回 503（`an_unreachable_database_fails_closed` 與驗證者的資料庫故障測試）。
- 開新連線比檢查期限慢時，就緒後的檢查不失敗，斷線後自動恢復（F1）。
- 健康埠不提供 ext_authz。
- 公鑰沒部署、同一把公鑰列兩次、路由設定不能用、沒有 Postgres CA 時拒絕啟動（`unusable_keys_and_settings_stop_startup`）。

待測：經 Istio 入口的端到端行為需要叢集，在 Laysi/trustiot-v2#77 驗證，包括：
- Envoy 以身分服務的值覆寫用戶端的標頭。
- 把身分服務的 5xx 轉成 `statusOnError` 503。
- 轉給身分服務的 Host 是原始主機。
- readiness 要等 3 條連線開好，入口因此只把檢查送給就緒的副本。
- 簽章私鑰檔在叢集裡的實際權限（0440、群組是 `fsGroup`）。
