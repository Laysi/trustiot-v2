# contract crate 驗收（2026-10-10）

對應 issue：Laysi/trustiot-v2#49 contract crate：服務間契約、事件格式與統一錯誤代碼（M1）。原始資料在 [data/2026-10-10-49-contract-crate/](data/2026-10-10-49-contract-crate/)。

## 環境

- 機器：本機開發機，Fedora Linux 45（kernel 7.2.8），x86_64。本 issue 不需要 lab 端到端測試，也沒有啟動容器。
- 受測版本：分支 `feat/49-contract-crate` 的 commit `6095203`，基底是 `integration/ms0` 的 `3a94ff1`；測試時工作目錄沒有未提交的變更。
- 工具：Rust 1.99.0（rustc 1.99.0 b940084d7 2026-09-28、cargo 1.99.0），由 `.prototools` 與 `rust-toolchain.toml` 鎖定；Bun 1.4.2 只用來收集證據；gitleaks 8.30.1 用來掃描祕密。
- client 測試的對方服務是測試在 `127.0.0.1` 隨機埠開的 TCP 假服務（`crates/contract/tests/common/peer.rs`），可以正常回應、完全不回應、讀完請求後送 RST（`SO_LINGER` 設為 0）或直接關閉連線。ztunnel 的拒絕用 RST 模擬。

## 指令

```sh
# 1. 格式、lint、測試（repo 根目錄）
CARGO_BUILD_JOBS=2 cargo fmt --all -- --check
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets -- -D warnings
CARGO_BUILD_JOBS=2 cargo test --workspace

# 只跑本 crate，或只跑單一測試
cargo test -p contract
cargo test -p contract --test client a_reset_connection_maps_to_unavailable

# 2. 編譯期保證：stable 的 rustdoc 不檢查 compile_fail doctest 標的錯誤碼，所以另外編譯同樣的程式，記錄實際錯誤。
#    compile-fail.txt 的四段程式各放進 repo 外暫時 crate 的 src/bin/<名稱>.rs，該 crate 以 path 依賴 crates/contract。
cargo build --bin no_timeout        # 預期 error[E0061]
cargo build --bin wrong_service     # 預期 error[E0271]
cargo build --bin bad_message_key   # 預期 error[E0080]
cargo build --bin bad_event_type    # 預期 error[E0080]

# 3. 傳輸錯誤鏈：對方送 RST 與直接關閉時 reqwest 回報的錯誤，以及 client 的分類。
#    探測程式與它的 Cargo.toml 在 transport-error-chains.txt 開頭，放在同一個暫時 crate。
cargo run -q --bin error_chains

# 4. 相依套件與授權
cargo tree -p contract -e normal --depth 1
cargo tree -p contract -e normal --prefix none --format '{p}: {l}'

# 5. 掃描已提交的檔案樹有沒有祕密（$TREE 是 repo 外的空目錄）
git archive --format=tar HEAD | tar -x -C "$TREE"
gitleaks dir --no-banner --no-color --redact "$TREE"
```

## 結果

| 步驟 | 結果 |
| --- | --- |
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0，沒有警告 |
| `cargo test --workspace` | exit 0。contract：單元測試 32、`tests/client.rs` 14、`tests/client_logging.rs` 1、`tests/error_format.rs` 7、`tests/events.rs` 9、doctest 5、compile_fail doctest 4，全數通過；repo-check 的 12 與 4 個測試也通過（[checks.txt](data/2026-10-10-49-contract-crate/checks.txt)） |
| 編譯期保證 | 四段程式都編譯失敗，錯誤如預期：少給逾時是 `E0061`（``argument #2 of type `Duration` is missing``）；把 assets 的 endpoint 交給 identity 的 client 是 `E0271`（`<ListDevices as Endpoint>::Service == Identity`）；訊息鍵寫成使用者文字是 `E0080`（`malformed message key`）；事件型別不是 `<領域>.<事件>` 是 `E0080`（``Event::TYPE must be `<domain>.<event>` in lower snake case``）（[compile-fail.txt](data/2026-10-10-49-contract-crate/compile-fail.txt)） |
| 傳輸錯誤鏈 | RST：錯誤鏈最內層是 `io::ErrorKind::ConnectionReset`（os error 104）。直接關閉：錯誤鏈裡沒有 I/O 錯誤，只有 `hyper::Error(IncompleteMessage)`，所以 client 直接依賴 hyper 來辨識。兩者都分類為 `connection_reset`，M1 代碼 `unavailable`（[transport-error-chains.txt](data/2026-10-10-49-contract-crate/transport-error-chains.txt)） |
| 相依套件 | 直接相依 11 個；連同間接相依共 87 個套件，授權是 MIT、Apache-2.0、Unicode-3.0，或由它們與 Unlicense 組成的 OR、AND 組合；TLS 與 HTTP/2、HTTP/3 套件 0 個（[dependencies.txt](data/2026-10-10-49-contract-crate/dependencies.txt)） |
| 祕密掃描 | 已提交的 420 KB 檔案樹 `no leaks found`。最後定稿 commit 前，測試用的假 token 長得像 JWT，被 gitleaks 的 `jwt` 規則抓到；它不是憑證，但會讓 CI 的掃描失敗，已換成不像 JWT 的值（[secret-scan.txt](data/2026-10-10-49-contract-crate/secret-scan.txt)） |

client 的失敗分類（`crates/contract/tests/client.rs`，`tests/client_logging.rs`）：

| 情境 | 測試 | `CallErrorKind` | M1 代碼 |
| --- | --- | --- | --- |
| 對方完全不回應，逾時 300 ms | `a_silent_peer_times_out_as_m1_timeout` | `timeout` | `timeout`（504）；經過時間不少於 300 ms 且少於 2.3 秒 |
| 對方讀完請求後送 RST | `a_reset_connection_maps_to_unavailable` | `connection_reset` | `unavailable`（503） |
| 對方讀完請求後直接關閉 | `a_connection_closed_without_a_response_is_a_reset` | `connection_reset` | `unavailable` |
| 沒有服務在聽 | `a_refused_connection_maps_to_unavailable` | `connect` | `unavailable` |
| 對方回 M1 錯誤（404 `not_found`） | `peer_m1_errors_pass_through` | `error_status` | 原樣傳遞，含訊息鍵與 details |
| 對方回 401 M1 `unauthenticated` | `a_peer_rejecting_our_credential_is_internal` | `error_status` | `internal` |
| 對方回非 M1 的 503、502、504、500、403 | `statuses_without_an_m1_body_map_by_status` | `error_status` | `unavailable`、`unavailable`、`timeout`、`internal`、`internal` |
| 對方回 302 | `redirects_are_not_followed` | `error_status` | `internal`，不跟隨 |
| 成功回應不符合回應型別、超過大小上限 | `a_response_that_breaks_the_contract_is_internal`、`an_oversized_response_is_rejected` | `invalid_response` | `internal`；錯誤訊息不引用對方的資料 |
| `Get` 帶 body、`Post` 沒有 body、路徑參數是空字串、`.` 或 `..` | `bad_requests_fail_before_reaching_the_network` | `invalid_request` | `internal`；沒有發出連線 |
| 失敗的日誌 | `failures_are_logged_with_peer_and_kind_but_not_the_token` | `connection_reset` | WARN 一筆，欄位 `peer=identity`、`kind=connection_reset`、`code=unavailable`、`timeout_ms=5000`，訊息提到 AuthorizationPolicy；所有日誌都不含 token 與路徑參數值 |

事件版本相容（`crates/contract/tests/events.rs`）：範例事件 `example.widget_renamed` 目前是第 2 版（第 1 版的 `name` 改名為 `display_name`，並新增選填的 `renamed_by`）。`old_versions_decode_with_the_current_consumer` 讀出 `tests/fixtures/events/example.widget_renamed/` 的 `v1.json` 與 `v2.json` 兩個完整信封，第 2 版的消費端把兩者都解成目前的型別（第 1 版的 `name` 轉成 `display_name`），並檢查 1 到 `VERSION` 的每一版都有樣本。其餘測試涵蓋：信封欄位與文件一致的往返、新版加的選填欄位被舊消費端忽略、比消費端新的版本回 `NewerVersion`（唯一可重試的解碼錯誤）、型別或產生端不符時拒絕、payload 不是 JSON 物件（含按位置排列的陣列）時拒絕。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 錯誤 JSON 格式有序列化與反序列化的往返測試，且錯誤型別能產生 OpenAPI schema（收進 api/openapi.json 的部分由 Laysi/trustiot-v2#55 驗證） | 通過：`tests/error_format.rs` 的往返測試涵蓋全部 12 個代碼、含 `details` 與 `field_errors` 的完整 body、領域訊息鍵、忽略較新版本加的欄位，以及拒絕不合法的 body。`ErrorBody`、`ErrorCode`、`FieldError`、`MessageKey` 由 utoipa 產生 schema，測試檢查其形狀並與固定樣本 `tests/fixtures/openapi/m1-error-schemas.json` 比對。收進 `api/openapi.json` 屬於 Laysi/trustiot-v2#55 |
| client 不給逾時無法建立（編譯期或建構子檢查），逾時與連線重置都轉成對應的 M1 代碼（單元測試） | 通過，兩種檢查都有：`InternalClient::new(base_url, timeout)` 少給逾時無法編譯（`E0061`）；逾時為 0 或超過 60 秒時建構子回錯誤（`timeouts_are_mandatory_and_bounded`）。逾時轉成 `timeout`，RST 與直接關閉都轉成 `unavailable`，見上方 client 失敗分類表 |
| 事件信封帶版本號，舊版事件可被新版消費端解析（相容性測試） | 通過：信封有 `version` 欄位；第 2 版的消費端解析第 1 版樣本（`old_versions_decode_with_the_current_consumer`） |
| crates/contract/README.md 說明新增 API 型別、事件與錯誤代碼的規則 | 通過：「內部 API」「事件」「錯誤格式（M1）」三節分別列出新增 endpoint、事件與錯誤代碼的步驟與相容性規則 |

待測：

- 真實 ztunnel 拒絕時本 client 的分類。本次以本機 RST 與直接關閉模擬；PoC（[docs/feasibility/2026-10-10-edge-poc.md](../feasibility/2026-10-10-edge-poc.md) Istio 段第 6 點）已觀察到 ztunnel 以連線重置拒絕，但目前還沒有服務在網格內呼叫另一個服務，無法用本 client 實測。待 Laysi/trustiot-v2#63 的 k3d 測試（未列在清單的呼叫被重置）時，以本 client 發出被拒絕的呼叫，確認 `kind=connection_reset` 與 WARN 日誌。
