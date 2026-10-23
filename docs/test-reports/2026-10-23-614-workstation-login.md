# 產線工作站工具：工作站身分與平台管理員登入

Laysi/trustiot-v2#614

## 環境

- 建置主機（x86_64 Linux），podman（certs 測試的 Postgres 容器），Rust 1.99.0（`rust-toolchain.toml`），`.prototools` 的 cargo-deny。每個指令都帶 `CI=1`：需要容器的測試在沒有容器時失敗而不是略過。
- 分支 `feat/614-workstation-skeleton`。第 6 輪在 origin/main 6da9b15f 上（見「安全審查之後」）；前三輪在 origin/main 74216494（含 Laysi/trustiot-v2#604）上，之後 rebase 到 83e159cd（含 Laysi/trustiot-v2#620，同樣改了 `crates/contract/src/certs.rs` 與 `services/certs/README.md`，沒有衝突）再跑一次。
- 只用 loopback 上的替身：登入、簽發與登出都對 `tests/support/mod.rs` 的 axum 替身（加上一個接受連線後中斷或不回應的 TCP 替身），沒有連到任何真實環境、SMTP 或展示主機。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo run -p openapi -- --check
CI=1 cargo test -p workstation
CI=1 cargo test -p workstation --test login init_login_enroll_and_logout_at_trace_leak_no_secret -- --nocapture
CI=1 cargo test -p contract -p certs -p repo-check --no-fail-fast
cargo deny --locked check
cargo build --release --locked -p workstation
bun test scripts/ci/changes.test.ts
```

## 結果

輸出摘錄（指令、測試名稱、結果行，以及 TRACE 流程測試的完整輸出；第 1 輪是 74216494 上的整批檢查，第 2、3 輪是修正一句提示文字之後重跑的 clippy、測試、cargo deny、release 建置與流程測試，第 4 輪是 rebase 到 83e159cd 後的 fmt、workspace clippy、`openapi --check`、workstation／contract／certs／repo-check 測試、cargo deny 的 bans／licenses／sources 與 release 建置，第 5 輪是把私鑰 PEM 標記與一個測試值改成執行時組成、讓 gitleaks 不再誤判之後重跑的 workstation 測試，第 6 輪是安全審查的修正之後、rebase 到 6da9b15f 的整批檢查，見最後一節）：[checks.txt](data/2026-10-23-614-workstation-login/checks.txt)。

- `cargo fmt --check`、workspace 的 clippy、`openapi --check`（`api/openapi.json` 不變）通過。
- `cargo test -p workstation`：單元測試 24 個、`tests/identity.rs` 4 個、`tests/login.rs` 13 個，全部通過。
- `cargo test -p contract -p certs -p repo-check`：全部通過。工作站用途的 OID、DER 與主體前綴搬到 `contract::certs`，certs 經 `issue::` 重新匯出，certs 的 `tests/workstation.rs`、`tests/registration.rs` 照常通過；repo-check 接受新的 workspace 成員。
- `cargo deny --locked check`：bans、licenses、sources 通過；advisories 回報 RUSTSEC-2026-0386（tokio 1.53.2 的 `AsyncFd::new`，修正版本 1.53.3）。這是 main 上原有的 tokio 版本（這個分支的 `Cargo.lock` 只多了 `workstation` 一筆、沒有新的第三方套件）；升級 tokio 交給另外的變更。
- `cargo build --release --locked -p workstation` 產生 `trustiot-workstation`。
- gitleaks：第一次掃描把私鑰 PEM 的開始與結束標記常數（`src/identity.rs`）和一個測試用的假 Access id（`src/http.rs`）判為洩漏；兩者都不是機密，改成執行時組成之後，分支的每個 commit（`git format-patch` 的輸出）與 `git archive HEAD` 的樹都掃不到。
- `bun test scripts/ci/changes.test.ts` 282 個通過：`factory/` 的變更本來就會跑 `rust` job，不需要改路徑規則。

| 測試 | 內容 | 結果 |
| --- | --- | --- |
| `tests/identity.rs` `init_creates_a_0600_key_and_a_csr_and_never_replaces_the_key` | `init` 建立 0600 的私鑰、0700 的目錄、0644 的 CSR；再跑一次私鑰不變 | 通過 |
| `tests/identity.rs` `a_key_file_wider_than_0600_is_refused_by_name_without_its_contents` | 0640、0604、0644 等權限的私鑰在 `init`、`install-certificate` 與載入時都被拒絕；錯誤指出檔名與權限，不含 PEM 內容 | 通過 |
| `tests/identity.rs` `install_certificate_refuses_every_profile_but_the_workstation_one` | 拒絕：沒有 EKU、只有 clientAuth、工作站用途加 clientAuth、CN 沒有 `workstation:`、閘道器的硬體 ID 主體、過期、還沒生效、另一把金鑰的憑證、兩張憑證；合格的存成 0644 | 通過 |
| `tests/identity.rs` `the_binary_initialises_and_installs` | 執行檔的 `init`、`install-certificate` 結束碼 0，拒絕時 1，用法錯誤 2 | 通過 |
| `tests/login.rs` `init_login_enroll_and_logout_at_trace_leak_no_secret` | 以 `TRUSTIOT_WORKSTATION_LOG=trace` 跑 `init`、`login`、`enroll`、`logout`：替身剛好收到 `send_code`、`verify_code`、簽發、`DELETE` 四個請求，每個都帶 `--headers-file` 的兩個標頭，cookie 只在後兩個；session 檔 0600、目錄 0700，登出後刪除；所有 stdout 與 stderr 都找不到驗證碼、cookie、私鑰的 base64 行與兩個標頭值 | 通過 |
| `tests/login.rs` `send_code_is_sent_once_when_the_connection_drops`、`..._when_the_answer_times_out`、`..._when_the_service_is_unavailable` | 連線被中斷、`--timeout 2` 逾時、503 時，替身只收到一個連線或請求；前兩者提示「可能寄出也可能沒有」、至少等 60 秒 | 通過 |
| `tests/login.rs` `an_accepted_send_code_with_an_unreadable_answer_still_asks_for_the_code` | 202 但內容讀不懂時照樣請操作員輸入驗證碼，只寄一封 | 通過 |
| `tests/login.rs` `a_wrong_code_can_be_entered_again_without_a_new_email` | 格式不對的輸入不送出、錯的驗證碼 401 後可以再輸入，只有一次 `send_code` | 通過 |
| `tests/login.rs` `a_session_that_cannot_be_stored_is_revoked` | session 檔寫不進去時，工具以那個 cookie 送 `DELETE` 撤銷 session，以 1 結束，輸出沒有 cookie | 通過 |
| `tests/login.rs` `the_session_is_only_sent_to_its_own_origin` | session 只送給它的 origin；已有別的平台的 session 時 `login` 在寄信之前就拒絕，`logout` 之後才能登入別的平台 | 通過 |
| `tests/login.rs` `a_session_file_wider_than_0600_is_refused`、`login_refuses_to_start_without_a_runtime_directory`、`the_headers_file_is_checked_before_any_request` | session 檔或標頭檔權限太寬、沒有 `XDG_RUNTIME_DIR`、標頭名稱不是那兩個或重複、少一個時，在送出任何請求前就拒絕，錯誤不含值 | 通過 |
| `tests/login.rs` `logout_of_an_ended_session_removes_the_file`、`enroll_after_the_session_ended_removes_the_session_file` | 平台自己的 401（`code` 是 `unauthenticated`）時先送 `DELETE` 確認、再刪除 session 檔並請操作員重新登入 | 通過 |
| `src/http.rs` 單元測試 | `--ca-file` 的 HTTPS：只信任該 CA、帶額外標頭、沒有 `Origin` 與 `Sec-Fetch-Site`；不受信任的伺服器在送出前就被拒絕；錯誤只顯示 M1 的 code、key 與欄位 | 通過 |
| `src/secret.rs`、`src/files.rs` 單元測試 | 機密型別的 `Debug` 與 `Display` 是 `<redacted>`；0600 建立、不覆寫、整檔替換、拒絕符號連結與過大的檔案、拒絕別人可寫的目錄 | 通過 |

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 私鑰檔與 session 檔建立時權限是 0600；權限比 0600 寬的檔案被拒絕使用，錯誤訊息指出是哪個檔案、不含內容（測試） | 通過：`tests/identity.rs` 的前兩個測試、`a_session_file_wider_than_0600_is_refused`、流程測試的 session 檔權限 |
| `install-certificate` 拒絕：EKU 不是工作站用途、CN 沒有 `workstation:` 前綴、已過期或還沒生效、公鑰與私鑰不符；合格的憑證被存放（測試） | 通過：`install_certificate_refuses_every_profile_but_the_workstation_one` |
| 對 loopback 的登入替身：`login` 兩步成功後 cookie 存成 0600 的檔案、沒有印出；`logout` 讓替身收到帶那個 cookie 的 `DELETE` 並刪除檔案；`send_code` 在連線失敗或逾時時不會自動重送（測試） | 通過：流程測試與三個 `send_code_is_sent_once_*` |
| 以 TRACE 執行 `init`、`enroll`、`login`、`logout` 的日誌與輸出中，找不到驗證碼、cookie、私鑰與 `--headers-file` 的值（測試，搜尋輸出） | 通過：`init_login_enroll_and_logout_at_trace_leak_no_secret`，輸出在 checks.txt |
| `--headers-file` 的標頭每個請求都帶，只接受那兩個標頭名稱，值不在日誌（測試） | 通過：流程測試、`the_headers_file_is_checked_before_any_request`、`src/http.rs` 單元測試 |
| `cargo build --release --locked -p workstation`、`cargo test -p workstation`、`cargo deny --locked check`、workspace 的 clippy 與 `cargo test -p repo-check` 通過 | 通過：rebase 到含 tokio 1.53.3 的 6da9b15f 之後全部通過（第 6 輪）；之前的輪次 advisories 因 main 上原有的 tokio 1.53.2（RUSTSEC-2026-0386）失敗 |

延後的驗證（Laysi/trustiot-v2#614「不包含」）：在真實的入口與 SMTP 上以真人走一遍登入，以及 `login` 在 Cloudflare Access 之後的長時間行為。

## 安全審查之後（2026-10-24）

獨立的安全審查沒有 Critical 或 High。以下各項以 commit「fix(workstation): harden certificate storage, session rejection and file checks」處理，之後 rebase 到 origin/main 6da9b15f（含 Laysi/trustiot-v2#622 的 tokio 1.53.3），在建置主機上（`CI=1`）重跑：`cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo run -p openapi -- --check`、`cargo test -p workstation -p contract -p certs -p repo-check --no-fail-fast`（workstation 單元測試 30 個、`tests/identity.rs` 4 個、`tests/login.rs` 17 個）、`cargo deny --locked check`（advisories、bans、licenses、sources 都 ok）、`cargo build --release --locked -p workstation` 與 TRACE 流程測試，全部通過（checks.txt 第 6 輪）。

| 項目 | 問題 | 處理 | 測試 |
| --- | --- | --- | --- |
| M1 | `install-certificate` 與 `enroll` 存的是整個輸入：夾帶 `PRIVATE KEY` 區塊、雜訊文字與 ANSI 跳脫序列的檔案會被接受，私鑰落進 0644 的 `workstation.pem` | 輸入只能有一個 PEM 區塊、標籤是 `CERTIFICATE`，否則整個拒絕；存放的是由檢查過的 DER 重新編碼的 PEM，不是輸入原文 | `install_certificate_refuses_every_profile_but_the_workstation_one`（私鑰在前或在後的組合被拒絕、沒有寫入；帶雜訊文字的輸入存成與重新編碼相同的檔案）、`identity::tests::certificates_are_re_encoded_and_other_blocks_refused` |
| M2 | 任何 401 都刪除 session 檔、卻沒有撤銷 session；certs 在無法檢查內部 token 時、前面的代理也可能回 401 | 只有平台自己的 401（M1 `code` 是 `unauthenticated`）才算 session 被拒，而且先送 `DELETE /api/identity/v1/sessions/current`，204 或平台自己的 401 才刪除檔案；其他情況保留檔案，請操作員執行 `logout`；`logout` 也只在這兩種回應刪除檔案 | `a_rejected_session_is_revoked_before_its_file_is_removed`、`a_proxys_401_keeps_the_session_file`、`a_rejected_session_that_cannot_be_revoked_keeps_its_file`、`http::tests::only_the_platforms_own_401_rejects_a_session` |
| L1 | `enroll` 印出回應裡的序號，沒有檢查格式或是否等於憑證的序號 | 序號要是 2 到 40 個小寫十六進位數字，去掉前導 0 之後等於憑證的序號，否則不存放；印出的一律是憑證本身的序號 | `an_answer_whose_serial_is_not_the_certificates_is_refused`（另一個序號、跳脫序列、大寫）、`identity::tests::serials_are_lower_case_hex_compared_without_leading_zeros` |
| L2 | FIFO 放在檔案的位置會讓工具卡住 | 以 `O_NONBLOCK` 開啟，由原有的一般檔案檢查拒絕；公開檔案也要求是一般檔案 | `files::tests::fifos_are_refused_without_blocking` |
| L3 | 沒有檢查擁有者；目錄檢查跟隨符號連結 | 目錄與每個存密鑰的檔案（私鑰、session、`--headers-file`）都要屬於工具的有效使用者（`geteuid`，唯一的 unsafe 區塊，附理由）；目錄以 `symlink_metadata` 檢查，符號連結被拒絕 | `files::tests::files_and_directories_of_other_users_are_refused`（不用 root 建不出別人擁有的檔案，所以直接測檢查函式，並確認自己的檔案通過；以別的 uid 擁有的實際檔案沒有測）、`files::tests::a_link_to_a_directory_is_refused` |
| I1 | `http://` 接受 `localhost` 這個名稱 | 只接受 loopback 的 IP 位址 | `config::tests::origins_are_https_or_loopback_http_without_anything_else`（`http://localhost:4000`、`http://LocalHost` 被拒絕） |
| I4 | 伺服器的文字（M1 的 code、key、欄位，序號）原樣印到終端機 | 印出前把控制字元改寫成 `\u{..}`、超過 120 字元截斷；序號只印檢查過的 | `http::tests::api_errors_show_only_m1_codes_keys_and_fields`（欄位名稱含跳脫序列與換行）、上面 L1 的測試 |
| I5 | `--headers-file` 的錯誤訊息會引用不認得的標頭名稱 | 只回報行號（重複或缺少時是兩個合法名稱之一），不引用檔案的任何文字 | `http::tests::the_headers_file_names_exactly_the_access_headers`（值寫在名稱的位置、只有值的行） |
| I2、I3、I7 | 文件 | README：每位操作員用自己的 Linux 帳號、交班時 `logout`；不會清零的副本加上 stdin 的緩衝區與長輸入時 `String` 重新配置留下的舊緩衝區；DNS 解析不在 `--timeout` 之內 | 不適用 |
| I6 | issue 寫 hyper-rustls，實作用 tokio-rustls 加 hyper 的連線 API | 保留：理由寫在 PR 內文（不用連線池，避免自動重送 `send_code`） | 不適用 |
