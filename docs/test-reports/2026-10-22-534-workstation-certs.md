# certs 服務：工作站憑證的簽發、撤銷與 profile（2026-10-22）

Laysi/trustiot-v2#534：certs 服務簽發產線工作站憑證（同一個簽發 CA，extendedKeyUsage 只有 TrustIoT 工作站用途、沒有 clientAuth），平台管理員經入口的 cookie 路由以內部身分簽發與撤銷，撤銷的憑證列在簽發 CA 的下一張 CRL；簽發 CA 有了第一張 CRL 才簽章。profile 的決定、欄位與操作步驟在 [services/certs/README.md](../../services/certs/README.md) 的「工作站憑證」。這份報告是六條驗收條件的證據；前四條由下列自動化測試在建置主機上以 `CI=1` 實際執行（容器測試不略過）。

## 環境

- **程式碼**：分支 `feat/534-workstation-certs`，commit `9d19729`（這次的變更）合併 main `ba25b95e` 之後的 `c4ab26f`。
- **建置主機**（經檢查佇列）：x86_64 Linux，`CI=1`；需要容器的測試以 rootless podman 起拋棄式的 TLS Postgres（`crates/testkit`）與 Mosquitto（`docker.io/library/eclipse-mosquitto:2.1.2-alpine`，以 digest 釘選）。Rust 1.99.0（`rust-toolchain.toml`），bun 1.4.2、helm 4.3.0、kubeconform 0.8.0、cargo-deny 0.20.2（`.prototools` 釘選的版本）。
- **工作站**：輕量檢查（`scripts/ci/changes.test.ts`、`scripts/nightly/renovate.test.ts`）、`api/openapi.json` 之後的前端 client 重新產生，以及 gitleaks 8.30.1。
- **時間**：最後一次完整檢查 2026-10-22 23:21 排入佇列，23:52 結束（Asia/Taipei）。審查後補上的簽發紀錄 trigger（見「審查後補上：撤銷不能收回」）另跑一輪，2026-10-23 00:10 到 00:12。

## 指令

在 repo 根目錄，一次佇列工作依序執行（每一步的結束碼記在 [checks.txt](data/2026-10-22-534-workstation-certs/checks.txt)）：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
cargo run --locked -q -p openapi -- --check
cargo deny --locked check
cargo test --locked -p certs --test workstation --test cross_tenant -- --nocapture --test-threads 1
cargo test --locked -p testkit --test postgres_isolation a_suite_without_tenant_data -- --nocapture
bun test deploy/chart
bun install --cwd scripts --frozen-lockfile && bun test scripts && bun run --cwd scripts typecheck
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check && bun run --cwd frontend typecheck && bun run --cwd frontend lint
bun run --cwd frontend test && bun run --cwd frontend build && bun run --cwd frontend check:dist
```

驗收證據的那一步可以單獨重跑：`cargo test -p certs --test workstation -- --nocapture`（沒有 podman 或 docker 時容器測試略過，設 `CI` 時失敗）。`api/openapi.json` 在建置主機以 `cargo run --locked -q -p openapi` 產生後取回，工作站上再以 `bun run --cwd frontend api:generate` 產生 `frontend/src/api/schema.gen.ts`；上面的 `--check` 與 `api:check` 證明兩者和程式一致。工作站上另外執行 `bun test scripts/ci/changes.test.ts`（279 通過）與 `bun test scripts/nightly/renovate.test.ts`（24 通過）。

審查後補上 trigger 的那一輪（只改了 `0002_workstation_certificates.sql`、`tests/issuance.rs` 與 README，輸出在 [ledger-guard.txt](data/2026-10-22-534-workstation-certs/ledger-guard.txt)）：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --locked -p certs --no-fail-fast
cargo test --locked -p certs --test issuance the_ledger_records -- --nocapture
cargo test --locked -p migrate --no-fail-fast
cargo test --locked -p sql-check
cargo run --locked -q -p openapi -- --check
```

## 結果

### 建置主機：全部通過

| 指令 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | 通過 |
| `cargo test --workspace --locked --no-fail-fast` | 1068 通過、0 失敗（139 個測試執行檔，容器測試都有執行） |
| `cargo run --locked -q -p openapi -- --check` | `api/openapi.json is up to date`（certs 已從 `SERVICES_WITHOUT_API` 移到 `SERVICES`） |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都 ok |
| `cargo test -p certs --test workstation --test cross_tenant -- --nocapture` | 5 通過：`workstation` 4、`cross_tenant` 1；輸出見 [workstation-tests.txt](data/2026-10-22-534-workstation-certs/workstation-tests.txt) |
| `cargo test -p testkit --test postgres_isolation a_suite_without_tenant_data` | 1 通過（輸出同上） |
| `bun test deploy/chart` | 507 通過、0 失敗 |
| `bun test scripts` 與 typecheck | 1541 通過、4 略過（`scripts/tests/e2e.test.ts` 的三個 podman 端對端測試要 `TRUSTIOT_DEV_E2E=1`，另一個只在 aarch64 主機執行）、0 失敗；`tsc --noEmit` 通過 |
| 前端 `api:check`、`typecheck`、`lint`、`test`、`build`、`check:dist` | 都通過；`test` 115 通過（含新訊息鍵的 zh-TW 與 en 翻譯檢查） |

`gitleaks git --redact --log-opts="--diff-merges=first-parent origin/main..HEAD"` 與匯出的整棵樹的 `gitleaks dir --redact` 都沒有發現。

### 驗收條件 1：工作站憑證不能當閘道器憑證用

`services/certs/tests/workstation.rs` 的 `the_broker_accepts_a_gateway_certificate_and_refuses_a_workstation_certificate`（容器，真的 certs 行程）：以 broker 相同的方式驗用戶端憑證：Mosquitto 2.1.2（OpenSSL）的 TLS listener `require_certificate true`，`cafile` 是服務發布的 `ca.pem`，`crlfile` 是服務發布的 `crl.pem`，`use_identity_as_username true`。

| 用戶端憑證 | 結果 |
| --- | --- |
| 閘道器憑證（同一個簽發 CA） | 接受：`received CONNACK (0)`，broker 以 CN `gw-534-accepted` 當使用者名稱 |
| 撤銷的閘道器憑證（CRL 列出它之後） | 拒絕：`ssl/tls alert certificate revoked`，沒有 CONNACK |
| API 簽出的工作站憑證（CSR 要求了 clientAuth） | 拒絕：`ssl/tls alert unsupported certificate`（OpenSSL 的用途檢查），沒有 CONNACK |

`the_ca_decides_the_workstation_profile_and_a_tls_server_refuses_it` 另以 rustls（webpki）驗用戶端憑證：閘道器憑證通過，工作站憑證被拒（`certificate does not allow extended key usage for client authentication`）。

### 驗收條件 2：CSR 內偽造的欄位被忽略，持有證明不符的 CSR 被拒

- `the_ca_decides_the_workstation_profile_and_a_tls_server_refuses_it`：CSR 要求 `CN=gw-forged-admin` 與組織、DNS 與 IP 的 SAN、沒有 pathLen 限制的 CA、keyCertSign，以及 clientAuth、serverAuth 與 anyExtendedKeyUsage（測試先確認 CSR 真的要求了這些）；簽出的憑證主體只有 `CN=workstation:line-a-ws-01`，延伸欄位的集合固定為 basicConstraints（critical，`cA=FALSE`）、keyUsage（critical，只有 digitalSignature）、extendedKeyUsage（只有工作站用途，DER 逐位元組比對）、subject 與 authority key identifier；公鑰是 CSR 的，簽章出自簽發 CA，128 位元序號，效期是 180 天加往前的 5 分鐘。
- `a_request_without_proof_of_possession_is_refused`：簽章最後一個位元組改過、簽名後改過主體的 CSR 都是 `BadSignature`；P-384 的 CSR 拒絕。
- API 同樣拒絕（`only_platform_administrators_issue_and_revoke_and_the_crl_lists_a_revocation`）：簽章驗不過 422 `workstation_certificate.request_signature_invalid`、P-384 422 `workstation_certificate.request_unsupported`、自己送 `subject` 422 `workstation_certificate.unknown_field`，什麼都沒簽。

### 驗收條件 3：缺少身分、身分無效或不是平台管理員的請求被拒，沒有簽出或撤銷任何憑證

`only_platform_administrators_issue_and_revoke_and_the_crl_lists_a_revocation`（容器，真的 certs 行程與測試用的身分簽發者）。先以平台管理員簽一張當對照（201、`cache-control: no-store`、簽發紀錄多一列、一行 INFO `issued a workstation certificate`），再對簽發與撤銷各送一次：

| 請求 | 簽發 | 撤銷 |
| --- | --- | --- |
| 沒有內部身分標頭 | 401 `unauthenticated` | 401 `unauthenticated` |
| 不是 token | 401 | 401 |
| 別的金鑰簽的平台管理員 token | 401 | 401 |
| 給別的服務（assets）的平台管理員 token | 401 | 401 |
| 過期的平台管理員 token | 401 | 401 |
| 租戶管理員 | 403 `permission_denied` | 403 `permission_denied` |
| 沒有租戶的一般使用者 | 403 | 403 |
| assets 服務代平台管理員交換來的 token（帶 `act`） | 403 | 403 |

每個回應都帶 `cache-control: no-store`。之後測試以 superuser 讀簽發紀錄（這個簽發 CA 的所有列），和對照之後的內容完全相同：被拒絕的請求沒有簽出、也沒有撤銷任何憑證。管理員送錯 `Content-Type` 回 400，三種 422 之後簽發紀錄也不變。

`tests/cross_tenant.rs`：決策 29 的覆蓋檢查涵蓋 certs 的三張表與規格裡的兩個新端點，都以理由豁免（沒有租戶資料，工作站屬於平台），並以 `Suite::without_tenant_data` 宣告這個服務沒有租戶資料（`6 probes, 0 failed`）。`crates/testkit/tests/postgres_isolation.rs` 的 `a_suite_without_tenant_data_passes_only_while_that_holds` 證明這個宣告只在成立時通過：服務角色還碰得到租戶表時失敗（`declared without tenant data ..., but tenant tables or endpoints are declared`），沒有宣告時照舊以 `nothing is declared` 失敗。

### 驗收條件 4：撤銷的工作站憑證在簽發紀錄標記，並出現在下一張 CRL

同一個測試：閘道器憑證的序號、`00ff`、`not-hex`、`0` 撤銷都回 404（撤銷 API 只動工作站憑證）；平台管理員以 `key_compromise` 撤銷對照憑證回 200，回應帶 `revoked_at` 與 `revocation_reason`，記一行 INFO `revoked a workstation certificate`，簽發紀錄的那一列標記撤銷、原因 1，閘道器那一列不變；再撤銷一次 409 `workstation_certificate.already_revoked`。CRL 工作的下一輪重發 CRL 後，`crl.pem` 列出這個序號與 keyCompromise（`the published CRL lists 547932ec1b51ce6d73ae5cac5f75716b (keyCompromise)`）。測試把 CRL 的排程調快（每秒檢查、剩不到 3590 秒就重發），正式的預設是剩不到 5 天時重發。

撤銷之後不能收回，對任何角色都一樣：見下面的「審查後補上：撤銷不能收回」。

### 驗收條件 5：簽發 CA 要先有第一張 CRL 才簽章

這是 Laysi/trustiot-v2#78 安全審查的 Low，驗收條件只要求修正、不另加測試，所以沒有寫新的測試。修正在 `Authority::signer`（API 只用已有 CRL 的簽發 CA）與 `issue::record`（簽發 CA 沒有 CRL 時不寫入簽發紀錄，`RecordError::NoCrl`）；既有的 `tests/issuance.rs` 的簽發紀錄測試因此改成先確認沒有 CRL 時寫不進去，再寫入第一張 CRL 後才記錄，這一步也通過。README 的「發簽發 CA 憑證」第 5 步已同步。

### 驗收條件 6：README 與 OpenAPI

`services/certs/README.md` 新增「工作站憑證」：profile 的決定與比較過的做法、欄位表、Laysi/trustiot-v2#79 可以依賴的欄位、簽發、撤銷與換發的步驟；「端點」「設定」「資料庫」「測試」同步。`cargo run --locked -q -p openapi -- --check` 通過。

### 審查後補上：撤銷不能收回（2026-10-23）

審查發現欄位權限只限制服務角色能改哪兩欄，不限制怎麼改：有問題或被入侵的服務可以把撤銷時間改回 NULL、移動它或改原因，下一張 CRL 就漏掉這張憑證。`0002_workstation_certificates.sql`（還沒合併，所以直接改這個檔案）加上 trigger `issued_certificates_guard`（`BEFORE UPDATE`、每一列，對所有角色生效）：撤銷過的列不能再改；任何 `UPDATE` 只能改 `revoked_at` 與 `revocation_reason`，比對的是這兩欄以外的整列。違反時 SQLSTATE 42501（`insufficient_privilege`），訊息寫明序號。

`tests/issuance.rs` 的 `the_ledger_records_what_was_signed_and_the_service_cannot_change_it`（容器）：

| 角色與操作 | 結果 |
| --- | --- |
| `certs_app` 以 API 的寫法撤銷（`WHERE revoked_at IS NULL`） | 第一次更新 1 列，再一次 0 列、沒有錯誤（API 據此回 409） |
| `certs_app` 收回撤銷（兩欄設為 NULL）、改原因為 4、把原因設為 NULL、把撤銷時間提早一天 | 都被拒：42501 `certificate <序號> is revoked: its ledger row cannot change` |
| superuser 收回撤銷 | 被拒，同上 |
| 之後以 superuser 讀撤銷的那一列（`to_jsonb` 的整列） | 和撤銷後完全相同 |
| superuser 對還沒撤銷的列改 `subject`、延長 `not_after`、撤銷同時改 `certificate` | 都被拒：42501 `only the revocation of certificate <序號> can change, never what was signed`；整列不變 |

同一輪的 `cargo test -p certs`（30 通過：單元測試 12、`ceremony` 3、`crl` 2、`cross_tenant` 1、`issuance` 5、`key_custody` 3、`workstation` 4）、`cargo test -p migrate`（88 通過）、`cargo test -p sql-check`（26 通過）、fmt、clippy 與 `openapi --check` 都通過，0 失敗；既有的撤銷路徑（`tests/crl.rs`、`tests/workstation.rs` 的 API 撤銷與 CRL）照常通過。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 工作站憑證不能當閘道器憑證用（OpenSSL 的 TLS 伺服器信任 `ca.pem`、讀 `crl.pem`） | 通過 |
| CSR 偽造的 CN、EKU 與其他欄位被忽略；持有證明不符的 CSR 被拒 | 通過 |
| 缺少內部身分、身分無效或不是平台管理員的簽發與撤銷請求被拒，沒有簽出或撤銷任何憑證 | 通過 |
| 撤銷的工作站憑證在簽發紀錄標記，並出現在該簽發 CA 的下一張 CRL | 通過；撤銷不能收回，任何角色都改不了（2026-10-23 補上 trigger 與測試） |
| 「簽發 CA 要先有第一張 CRL 才簽章」已修正，README 同步 | 通過（Low，依驗收條件不另加測試） |
| README 寫明 profile、#79 可以依賴的欄位與步驟；`cargo run -p openapi -- --check` 通過 | 通過 |

沒有在叢集裡經真的入口與身分服務呼叫 `/api/certs`：nightly 與展示環境還沒有啟用 certs 服務（services/certs/README.md 的「映像」）。這是待測，等 certs 在 nightly 或展示環境啟用時，比照 `scripts/nightly/api.ts` 對資產服務的檢查補上。合併前的 nightly `smoke` 執行會以 migration Job 套用新的 `0002_workstation_certificates.sql`。
