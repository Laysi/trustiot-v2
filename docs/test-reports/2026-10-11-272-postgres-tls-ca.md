# Laysi/trustiot-v2#272 Postgres TLS 只信任設定的 CA

對應 issue：Laysi/trustiot-v2#272。修正 Laysi/trustiot-v2#53（PR Laysi/trustiot-v2#263）的 `verify-full` 連線同時信任 webpki 公開根憑證的問題。

## 環境

| 項目 | 值 |
| --- | --- |
| 分支 | `fix/272-postgres-tls-ca`，從 main `e8f180b` 開出 |
| 程式碼閱讀 | 本機 cargo registry 的 `sqlx-core 0.9.0`（`Cargo.lock` 鎖定的版本）、`webpki-roots 1.0.9` |
| 測試機器 | build host `ruisi-wsl`（Ubuntu on WSL2，x86_64，20 核，Docker），以 `rcheck.sh 272 '<指令>'` 執行，`CI=1`（容器測試不能略過） |
| 工具 | rustc 1.99.0（`.prototools` 與 `rust-toolchain.toml`），cargo-deny 依 `.prototools` |
| Postgres | `tests/support/postgres.rs` 的 `postgres:18.6-alpine`（digest 固定），以測試時產生的 CA 簽的伺服器憑證啟用 TLS，憑證只寫 `127.0.0.1` |

## 驗證：回報是否成立

成立。`sqlx-core 0.9.0` 的 `src/net/tls/tls_rustls.rs`：

- 第 140 行 `let mut cert_store = import_root_certs();` 先建好信任清單，第 150 行再把 `ssl_root_cert` 的每張憑證 `cert_store.add(...)` 加進去，第 173、178 行 `with_root_certificates(cert_store)` 交給 rustls 的 `WebPkiServerVerifier`。設定的 CA 是「加上去」，不是「取代」。
- `import_root_certs` 依 feature 有三種（第 211、216、234 行）：`webpki-roots` 開、`rustls-native-certs` 關時放入 `webpki_roots::TLS_SERVER_ROOTS`（1.0.9 有 121 張）；`rustls-native-certs` 開時改載入系統憑證庫；兩者都關時才是空的。
- service-kit 用的 sqlx feature `tls-rustls-ring-webpki` 開啟 `sqlx-core/_tls-rustls-ring-webpki` → `webpki-roots`。sqlx 公開的 rustls feature（`tls-rustls-ring-webpki`、`tls-rustls-ring-native-roots`、`tls-rustls-aws-lc-rs`）全部帶 webpki-roots 或系統憑證庫；兩者都關的組合無法編譯（沒有 crypto provider）。`native-tls` 版本同樣保留系統內建的根憑證。`PgConnectOptions` 與 `PgPoolOptions` 也沒有自訂 TLS 連線器的介面。
- 所以 main 上任何一家公開 CA 為資料庫主機名稱簽的憑證都會通過 `verify-full`。下方的突變測試量到 main 的設定下 sqlx 會信任 121 張公開根憑證。

## 做法

- 根目錄 `Cargo.toml` 以 `[patch.crates-io]` 把 `webpki-roots` 換成 `crates/webpki-roots-empty`（套件名 `webpki-roots`、版本 1.0.9、`TLS_SERVER_ROOTS` 為空）。sqlx 的信任清單因此只剩設定的 CA；不帶 root 憑證的連線設定一律 `UnknownIssuer`。
- `PostgresConfig::ca_file` 由 `Option<PathBuf>` 改為必填的 `PathBuf`；`connect_options()` 一律設定 `ssl_root_cert`（同時蓋掉 `PGSSLROOTCERT`）。範例服務沒有 `TRUSTIOT_POSTGRES_CA_FILE` 時以 exit code 1 拒絕啟動。chart 在 `postgres.mode=external` 時本來就要求 `caSecret.name`（`values.schema.json`），不需修改。
- 守門測試 `crates/service-kit/tests/postgres_trust.rs`：編進來的 `webpki_roots::TLS_SERVER_ROOTS` 必須是空的；`Cargo.lock` 裡 `webpki-roots` 只能有一個且是本地 crate、只有 sqlx-core（與這個測試）用它、sqlx-core 不能帶 `rustls-native-certs`、`rustls-platform-verifier`、`webpki-root-certs`、`native-tls`、`openssl`、`security-framework`。

## 指令

從 repo 根目錄：

```
bash <scratchpad>/rcheck.sh 272 'set -e; cargo fmt --all --check; cargo clippy --locked --workspace --all-targets -- -D warnings; cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings; cargo test --locked --workspace; cargo test --locked -p service-kit --no-default-features; cargo deny --locked check'
bash <scratchpad>/rcheck.sh 272 'cargo test --locked -p service-kit --test postgres_trust -- --nocapture; cargo test --locked -p service-kit --test tenant_db -- --nocapture'
```

突變測試（只改 build host 上的複本，證明守門測試擋得下較弱的實作）：

```
# 1. 回到 main 的相依設定：crates.io 的 webpki-roots、沒有 patch
sed -i -e "/^\[patch.crates-io\]/,/^webpki-roots = /d" -e "s|^webpki-roots = { path = \"crates/webpki-roots-empty\" }|webpki-roots = \"1\"|" Cargo.toml
cargo test -p service-kit --test postgres_trust -- --nocapture
# 2. sqlx 改用系統憑證庫
sed -i -e "s|\"tls-rustls-ring-webpki\"|\"tls-rustls-ring-native-roots\"|" crates/service-kit/Cargo.toml
cargo test -p service-kit --test postgres_trust the_lock_file
```

## 結果

| 情境（真 Postgres，伺服器憑證由測試 CA 簽、名稱 `127.0.0.1`） | 結果 |
| --- | --- |
| `PostgresConfig.ca_file` = 測試 CA，主機 `127.0.0.1` | 連上，`pg_stat_ssl.ssl = true` |
| `ca_file` = 另一張 CA + 測試 CA 的 bundle | 連上 |
| `ca_file` = 另一張 CA（代表公開 CA），主機名稱相符 | 拒絕：`invalid peer certificate: UnknownIssuer` |
| `ca_file` = 測試 CA，主機 `localhost`（同一台伺服器） | 拒絕：`certificate not valid for name "localhost"` |
| 沒有設定 root 憑證的 `verify-full` 連線設定 | 拒絕：`UnknownIssuer` |
| 範例服務沒有 `TRUSTIOT_POSTGRES_CA_FILE`，或設為空字串 | exit code 1，`configuration key platform.postgres_ca_file is invalid: is required when database is set` |

| 守門測試 | 本分支 | 突變 1（main 的相依設定） | 突變 2（native roots） |
| --- | --- | --- | --- |
| `sqlx_is_built_with_no_public_roots` | 通過 | 失敗：`sqlx would trust 121 public roots` | （未跑） |
| `the_lock_file_gives_sqlx_no_trust_anchor_but_the_configured_ca` | 通過 | 失敗：`exactly one webpki-roots in the graph` | 失敗：`sqlx-core is built with rustls-native-certs` |
| `only_the_configured_ca_is_trusted`（真 Postgres） | 通過 | 通過（測試 CA 不是公開 CA，真 handshake 分不出差別，所以由上面兩項擋） | （未跑） |

全部檢查：`cargo fmt --check`、兩種 clippy、`cargo test --locked --workspace`（含 `tenant_db` 11 項、`postgres_trust` 3 項，CI=1 沒有略過）、`cargo test -p service-kit --no-default-features`、`cargo deny --locked check`（`advisories ok, bans ok, licenses ok, sources ok`）都通過。

原始輸出：

- [fix-postgres-tests.txt](data/2026-10-11-272-postgres-tls-ca/fix-postgres-tests.txt)：本分支的 `postgres` 單元測試、`postgres_trust`、`tenant_db`（`--nocapture`）
- [fix-workspace-and-native-roots-mutation.txt](data/2026-10-11-272-postgres-tls-ca/fix-workspace-and-native-roots-mutation.txt)：全 workspace 檢查、cargo deny、突變 2
- [mutation-crates-io-webpki-roots.txt](data/2026-10-11-272-postgres-tls-ca/mutation-crates-io-webpki-roots.txt)：突變 1

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 驗證結果記錄在 issue：公開根憑證是否被信任 | 通過：成立，main 的設定下 sqlx 信任 webpki-roots 的 121 張公開根憑證加上設定的 CA（見「驗證」，issue comment 另記） |
| 非設定 CA 簽發的被拒；設定 CA 簽發、主機名稱正確的被接受；主機名稱錯的被拒（真 Postgres） | 通過（容器以 Docker 執行，issue 寫的 podman 與 Docker 走同一段 `tests/support` 程式）；公開根憑證被排除由兩項守門測試證明，main 的設定下兩項都失敗 |
| 沒有設定 CA 時 service-kit 拒絕啟動（測試） | 通過：`a_service_without_a_postgres_ca_refuses_to_start`；`PostgresConfig::ca_file` 為必填型別 |
| `cargo deny --locked check` 通過；README 已更新；證據存到 `docs/test-reports/` | 通過 |

## 補充：合併 main `b8d1afa` 後重跑（2026-10-11）

使用者在 2026-10-11 接受 workspace 層級的 `webpki-roots` 替換，條件是追蹤上游；上游提供官方做法後依 Laysi/trustiot-v2#304 移除（決策 29、兩份 README 與守門測試的失敗訊息都指向它）。

合併 main `b8d1afa` 之後（含 OTLP 匯出、release workflow、本機開發環境的 Postgres TLS），在 build host 以 `rcheck.sh 272` 重跑：

- `cargo tree -i webpki-roots --locked -e normal,build,dev`：只有 sqlx-core（以及 service-kit 的 dev-dependency）依賴它；`rustls` 也只有 sqlx-core 使用，OTLP 匯出的 HTTP client 沒有用 TLS 根憑證。
- `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo clippy --locked -p service-kit --no-default-features --all-targets -- -D warnings`：通過。
- `cargo test --locked --workspace`（`CI=1`，沒有 `skipped:`；`postgres_trust` 3 項、`tenant_db` 11 項）與 `cargo test --locked -p service-kit --no-default-features`：通過。
- `cargo deny --locked check`：`advisories ok, bans ok, licenses ok, sources ok`。
- 突變 1（crates.io `webpki-roots`、沒有 patch）重跑：兩項守門測試照樣失敗，失敗訊息指向 Laysi/trustiot-v2#304。

原始輸出：[after-merge-b8d1afa.txt](data/2026-10-11-272-postgres-tls-ca/after-merge-b8d1afa.txt)。
