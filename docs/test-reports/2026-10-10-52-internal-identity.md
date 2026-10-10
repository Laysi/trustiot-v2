# 內部身分 token 驗證驗收（2026-10-10）

對應 issue：Laysi/trustiot-v2#52 service-kit：驗證內部身分 token（簽章、受眾、到期）。原始資料在 [data/2026-10-10-52-internal-identity/](data/2026-10-10-52-internal-identity/)。

## 環境

- 機器：本機開發機，Fedora Linux 45（kernel 7.2.8），x86_64。驗收條件不需要 Kubernetes 叢集，本 issue 沒有 lab 端到端測試。
- 受測版本：分支 `feat/52-internal-identity-token` 的 commit `c2dfefa`（含修正收尾競態的 `6ef4b31`），基於 `origin/main` 的 `910961d`（含 Laysi/trustiot-v2#49、Laysi/trustiot-v2#51）。
- 工具：Rust 1.99.0（`.prototools`）、Bun 1.4.2（收集證據的腳本）；podman 6.1.3（`cargo test --workspace` 裡 Laysi/trustiot-v2#51 的容器測試）；gitleaks 8.30.1（proto 安裝）掃描祕密。所有 cargo 指令都設 `CARGO_BUILD_JOBS=2`。
- 新的相依套件：`ed25519-dalek` 3.0.0（2026-07-06 發佈，只用驗章，開 `fast`）、`base64` 0.22.1（`Cargo.lock` 已有，用於 base64url 嚴格解碼），都只在 service-kit 的 `contract` feature 裡。
- 測試用的簽章金鑰在程式裡由一個 byte 的種子導出（`SigningKey::from_bytes(&[seed; 32])`），只在測試中使用，repo 裡沒有任何金鑰檔案。RFC 8037 附錄 A.4 的範例只用到公鑰與已簽好的 JWS。

## 指令

```sh
# 1. 全部檢查
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p service-kit --no-default-features --all-targets -- -D warnings
cargo test --workspace
cargo test -p service-kit --no-default-features

# 2. 內部身分的測試
cargo test -p service-kit --lib identity          # 每一項檢查、公鑰設定、輪替
cargo test -p service-kit --doc                   # 含 compile-fail：背景工作拿不到使用者身分
cargo test -p service-kit --test identity -- --nocapture --test-threads 1   # 真的行程與 HTTP

# 3. 相依隔離：關閉 contract feature 時沒有 contract 也沒有 ed25519-dalek
cargo test -p service-kit --test dependency_isolation -- --nocapture
cargo tree --locked -p service-kit -e normal,build -i ed25519-dalek
cargo tree --locked -p service-kit --no-default-features -e normal,build -i ed25519-dalek
cargo tree --locked -p service-kit --no-default-features -e normal,build -i contract

# 4. 穩定性：行程層級的測試連跑 30 次
for i in $(seq 30); do cargo test -p service-kit --test identity --test errors; done
```

證據由 Bun 腳本執行上列指令並存下輸出（路徑換成 `.`）。

## 結果

| 項目 | 結果 | 原始資料 |
| --- | --- | --- |
| fmt、兩種 feature 的 clippy | 全部 exit 0 | [checks.txt](data/2026-10-10-52-internal-identity/checks.txt) |
| `cargo test --workspace` | exit 0；service-kit 單元測試 68 個（內部身分 27 個）、`tests/identity.rs` 3 個 | [checks.txt](data/2026-10-10-52-internal-identity/checks.txt) |
| `cargo test -p service-kit --no-default-features` | exit 0；`tests/identity.rs` 在沒有 feature 時編譯成 0 個測試 | [checks.txt](data/2026-10-10-52-internal-identity/checks.txt) |
| 內部身分單元測試 | 27 passed | [identity.txt](data/2026-10-10-52-internal-identity/identity.txt) |
| doctest | 2 個範例、5 個 compile-fail 全部通過 | [identity.txt](data/2026-10-10-52-internal-identity/identity.txt) |
| 範例服務的 `/v1/whoami` | 見下表 | [identity.txt](data/2026-10-10-52-internal-identity/identity.txt)、[log-sample.txt](data/2026-10-10-52-internal-identity/log-sample.txt) |
| 相依隔離 | 預設 feature 時 `ed25519-dalek` 只被 service-kit 引用；`--no-default-features` 時 `ed25519-dalek` 與 `contract` 都不在相依樹 | [dependencies.txt](data/2026-10-10-52-internal-identity/dependencies.txt) |
| 穩定性 | `--test identity --test errors` 連跑 30 次，0 次失敗 | [stability.txt](data/2026-10-10-52-internal-identity/stability.txt) |
| 祕密掃描 | gitleaks 沒有發現 | [secret-scan.txt](data/2026-10-10-52-internal-identity/secret-scan.txt) |

範例服務（真的行程，公鑰來自 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`）的回應：

| 請求 | 回應 | 日誌 `reason` |
| --- | --- | --- |
| 有效 token | 200，回傳使用者、租戶與角色 | — |
| 偽造簽章（別把私鑰簽、同一個 `kid`） | 401 `{"code":"unauthenticated","message_key":"error.unauthenticated"}` | `bad_signature` |
| 過期（1 小時前簽發、效期 60 秒） | 401 同上 | `expired` |
| 受眾是 `certs`（服務是 `assets`） | 401 同上 | `wrong_audience` |
| 沒有 `x-internal-identity` | 401 同上 | `missing` |
| 有效 token 放在 `Authorization` | 401 同上 | `missing` |
| 標頭送兩次 | 401 同上 | `repeated` |
| 設定新舊兩把公鑰，各自簽的 token | 兩個都 200 | — |
| 不在設定裡的 `kid` | 401 同上 | — |
| 公鑰設定含私鑰（`d`）、不是 JSON、沒有公鑰 | 服務拒絕啟動，exit code 1；錯誤寫出 `platform.internal_identity_keys` 與 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`，不含設定值 | — |

測試結束後檢查服務的全部 stdout 與 stderr：有效、偽造與過期的 token、簽章與公鑰都沒有出現。

單元測試另外涵蓋（`src/identity/*.rs`）：`alg` 為 `none`、`HS256`、`RS256`、`ES256`、`PS256`、大小寫不同的 `eddsa`、`Ed25519` 一律拒絕；`typ` 錯誤；標頭多出 `crit`、`jku`、`jwk`、`x5u`、`b64`、`cty` 或重複的 `alg`；未知 `kid`；簽章改一個 bit、非標準形式的簽章（S 加上群階 l）；payload 改受眾；簽發者不是 `identity`；效期超過 300 秒或不是正數；`iat`、`nbf` 在未來超過 5 秒；`"nbf": null`；重複的 `aud`；base64 補位、標準字母、非零尾端位元；超過 8192 bytes；RFC 8037 附錄 A.4 的範例簽章驗證通過。

收尾競態：開發期間，行程層級的測試在高負載下偶爾以 exit code 1 結束，日誌是 `server stopped unexpectedly`、`outcome` 為 `Ok(Ok(()))`：SIGTERM 讓 server 正常結束，但 `tokio::select!` 隨機選到了「server 結束」的分支。這是 Laysi/trustiot-v2#51 既有的問題，會讓本 issue 的測試不穩定，所以在 `6ef4b31` 以 `biased;` 修正（[shutdown-race.txt](data/2026-10-10-52-internal-identity/shutdown-race.txt)）。這個競態取決於負載，修正前後在較空閒時各連跑 30 次都沒有重現，證據是當時的日誌。

## 結論

| 驗收條件 | 結果 | 證據 |
| --- | --- | --- |
| 偽造簽章、過期、受眾不符、缺少標頭的請求都被拒絕並回 M1 錯誤代碼（各一個測試） | 通過 | `tests/identity.rs` 的 `only_requests_with_a_valid_token_reach_the_handler`（四種各送一次，都是 401 `unauthenticated`，並各自記下原因）；單元測試 `forged_signatures_are_rejected`、`expiry_and_not_before_allow_the_leeway_only`、`issuer_and_audience_must_match`、`the_token_comes_from_exactly_one_identity_header` |
| 以新 key id 簽發的 token 在公鑰輪替期間仍可驗證（測試） | 通過 | 單元測試 `tokens_verify_throughout_a_key_rotation`（輪替前、期間、之後三個階段）；`tests/identity.rs` 的 `tokens_under_either_key_verify_during_a_rotation` |
| 背景工作無法取得使用者身分的 extractor（型別或測試證明） | 通過（型別證明） | `UserIdentity` 的 5 個 compile-fail doctest：以欄位、`From`、`Into`（從 `ServiceIdentity`）、`Deserialize`、`Default` 建立都無法編譯；`ServiceIdentity` 的 doctest 示範背景工作以服務自己的身分執行 |

公鑰的取得方式已決定並寫進 `crates/service-kit/README.md`「公鑰與輪替」與決策 27：由部署以平台設定 `TRUSTIOT_INTERNAL_IDENTITY_KEYS`（JWK Set）交給每個服務，不由身分服務在執行時發布。chart 的 values 欄位與 ConfigMap 由 Laysi/trustiot-v2#61 加入。
