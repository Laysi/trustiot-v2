# 產線工作站工具：簽章的登錄用戶端（重試、回應檢查與紀錄）

Laysi/trustiot-v2#615

## 環境

- 建置主機（x86_64 Linux），podman（certs 測試的 Postgres 容器），Rust 1.99.0（`rust-toolchain.toml`），`.prototools` 的 cargo-deny 與 gitleaks。每個指令都帶 `CI=1`：需要容器的測試在沒有容器時失敗而不是略過。
- 分支 `feat/615-workstation-register`，rebase 到 origin/main 0dc8fd96 之後的 commit 2fae93f6。
- 只用 loopback 上的替身與暫時的 Postgres：`factory/workstation` 的測試對 `tests/support/registry.rs`（raw TCP 的登錄替身）；`services/certs/tests/workstation_tool.rs` 對真的 certs binary 與 `testkit::product` 的 Postgres，前面是測試裡的 ingress 替身。沒有連到任何真實環境、SMTP 或展示主機。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo run --locked -q -p openapi -- --check
CI=1 cargo test --locked -p workstation -p certs -p repo-check --no-fail-fast
CI=1 cargo test --locked -p certs --test workstation_tool -- --nocapture
cargo deny --locked check
gitleaks git --redact --log-opts="origin/main..HEAD" .
git archive HEAD | tar -x -C <dir> && gitleaks dir --redact <dir>
```

## 結果

輸出摘錄：[checks.txt](data/2026-10-24-615-workstation-register/checks.txt)。

- `cargo fmt --check`、workspace 的 clippy、`openapi --check`（`api/openapi.json` 不變）通過。
- `cargo test -p workstation`：單元測試 44 個、`tests/identity.rs` 4 個、`tests/login.rs` 17 個、`tests/register.rs` 10 個，全部通過。
- `cargo test -p certs`：全部通過，含新的 `tests/workstation_tool.rs`。`cargo test -p repo-check` 通過。
- `cargo deny --locked check`：advisories、bans、licenses、sources 都 ok。`Cargo.lock` 只多了 workstation 的 `getrandom`、`sha2` 與 certs 的 dev-dependency `workstation`，沒有新的第三方套件。
- gitleaks：分支的 commit 與 `git archive HEAD` 的樹都沒有發現。
- `workstation_tool.rs` 的證據行：

```text
evidence: forwarded registrations [(201, true), (200, true), (200, false), (200, true), (201, false), (200, true)]; tool-0001 and tool-0002 each (1 registration, 1 gateway certificate, 1 certs.gateway_registered event)
```

`(狀態, 是否轉回給工具)`：tool-0001 的登錄（201）、重跑（200）、回應被丟掉的重跑（200 丟掉，重送 200）；tool-0002 第一個 201 就被丟掉，重送得到 200。

| 測試 | 內容 | 結果 |
| --- | --- | --- |
| `tests/register.rs` `a_lost_answer_is_sent_again_and_the_repeat_counts_as_success` | 替身登錄後不回任何 byte 就關連線；工具重送得到 200，以結束碼 0 完成；替身收到 2 個請求，都驗得過簽章、nonce 與簽章都不同、本文相同；紀錄 `registered`、0600；日誌 `started`、`attempt_failed`、`registered` | 通過 |
| `a_503_is_tried_again_and_the_next_answer_registers` | 503 後重試得到 201；日誌的 `attempt_failed` 是 `status 503` | 通過 |
| `each_refusal_stops_at_once_with_its_own_exit_code` | 每種拒絕與它的結束碼：409 `public_key_mismatch` → 10、409 `endorsement_key_mismatch` → 10、409 `public_key_in_use` → 11、409 `endorsement_key_in_use` → 11、409 `decommissioned` → 12、401 → 13、403 `stale` → 14、403 `revoked` → 15、403 `signature_invalid` → 19、403 `permission_denied` → 16、422 → 17；各只送 1 次，紀錄維持 `pending`，日誌最後是 `stopped`；401 先撤銷 session 再刪檔；`stale` 的訊息帶平台的 `Date` | 通過 |
| `an_answer_that_is_not_this_gateways_registration_is_not_recorded` | 回應的硬體 ID、兩個雜湊、CN、憑證公鑰不符，鏈換成根沒簽過的 CA、鏈的第一張不是憑證、鏈只有憑證：結束碼 18、只送 1 次、紀錄維持 `pending` | 通過 |
| `a_root_certificate_other_than_the_given_fingerprint_is_not_recorded` | `--root-sha256` 不符時 18；整套換成另一個根（本身一致）時，只有釘住的根擋得下，18；以 openssl 格式（大寫、`:` 分隔）的正確指紋重跑完成 | 通過 |
| `a_rerun_keeps_the_record_and_gets_the_same_registration` | 201 之後重跑 200、同一個序號；第二次的 `started` 記 `recorded_before: true` | 通過 |
| `a_gateway_key_recorded_under_another_hardware_id_is_not_sent` | 同一份 CSR 換一個硬體 ID：結束碼 11，替身沒收到第二個請求，沒有建立第二筆紀錄 | 通過 |
| `retries_stop_at_the_attempt_limit_and_a_rerun_completes` | `--attempts 2 --timeout 1`，503 然後不回應：結束碼 75、`gave_up` 記 2 次與 `timeout`；重跑得到 201 | 通過 |
| `a_write_cut_off_by_a_crash_leaves_the_previous_record_whole` | `ulimit -f 2` 讓程序寫 `registered` 紀錄時被 SIGXFSZ 殺掉：紀錄仍是完整的 `pending`，只留下一個點開頭、不是 JSON 的暫存檔，`records()` 略過它；重跑 200、`registered` | 通過 |
| `the_library_function_retries_until_settled_and_reports_the_attempts` | 函式庫函式（#616 用的）：503、502、丟掉的回應之後第 4 次 200；之後的 409 除役不重試，`registered` 的紀錄不被收回 | 通過 |
| 上面每個測試的 `assert_nothing_secret_shows` | TRACE 下的 stdout 與 stderr、紀錄、暫存檔與日誌，都找不到 cookie、回應 `Set-Cookie` 的值、私鑰的任何一行、任何請求的簽章或 nonce | 通過 |
| `services/certs/tests/workstation_tool.rs` | 見上方證據行：每個硬體 ID 都是 1 筆登錄、1 張閘道器憑證、1 個 `certs.gateway_registered` 事件；工具的紀錄是 `registered`，序號等於資料庫的 `certificate_serial`；`--root-sha256` 釘的是測試 CA 的根，真的鏈驗得過 | 通過 |
| 單元測試（`sign`、`retry`、`journal`、`register`） | 簽章以 ring 驗得過、改一個 byte 就驗不過；nonce 22 字元、不重複；退避的步驟 1、2、4、8、16、30 秒與抖動範圍；紀錄 0600 的往返、壞的紀錄被拒絕、權限 0640 的日誌被拒絕；硬體 ID 規則、輸入檢查、指紋格式、`Date` 的提示、鏈只接受 `CERTIFICATE` 區塊 | 通過 |

## 安全審查的修正

獨立的安全審查（對 e60ffb24）沒有 Critical 或 High；Medium 與 Low 都已在後續的 commit 修正。依專案規則，這些修正不另加邊界測試，只更新既有的測試讓它們通過。

| 項目 | 修正 | 既有測試的更新 |
| --- | --- | --- |
| M1：根憑證指紋原本是選用，沒有它時能改寫回應的人可以換掉閘道器唯一的信任錨 | `--root-sha256` 必填（沒有替代方式），`Registrar::root_sha256` 不再是 `Option`；紀錄與 `started` 日誌記下釘住的 `root_sha256`。紀錄的格式升為版本 2（必有 `root_sha256`）；先前寫下的版本 1 紀錄照樣讀得了，下次登錄同一個硬體 ID 時改寫成版本 2 | 每個 `tests/register.rs` 的指令都帶替身根憑證的指紋（`a_root_certificate_*` 照舊給自己的）；`a_lost_answer_*` 檢查紀錄的 `root_sha256`；函式庫測試與 `services/certs/tests/workstation_tool.rs` 傳入已知的指紋；`journal` 的單元測試多讀一筆版本 1 的紀錄，並拒絕沒有 `root_sha256` 的版本 2 |
| L1：403 `workstation_refused` 除了 `stale` 都叫操作員重新 `enroll` | 15 只留給 `revoked`、`expired`、`untrusted`、`not_workstation`；`missing`、`malformed`、`signature_invalid`、`replayed` 與不認得的原因改為結束碼 19，訊息指向改寫標頭的 proxy（或工具的 bug）、請操作員回報 | `each_refusal_*` 的表多一列 `signature_invalid` → 19，結束碼的數量改成 9 |
| L2：一筆讀不了或過大的紀錄擋住所有登錄 | 記錄前拒絕欄位過大的回應（結束碼 18），`Journal::write` 拒絕超過 256 KiB 的紀錄；重複公鑰掃描讀不了某筆紀錄時，訊息寫出檔案路徑與處理方式 | 無 |
| L3：寫日誌失敗會取代停止原因 | 停止結果保留原本的種類與結束碼，日誌錯誤附加在訊息後面 | 無 |
| L4：520–524 與非 M1 的回應沒有指引 | 520–524 和 502–504 一樣重試；500、其他 5xx 與沒有 M1 本文的回應，訊息說平台可能已經登錄、重跑是安全的 | 無 |

I1（簽章不綁定主機名稱，屬於契約的設計）與 I2（不檢查 leaf 與簽發 CA 的效期與 EKU，指紋必填後就不重要了）只是資訊，沒有修改。

修正後在建置主機上（`CI=1`）重跑 `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings` 與 `cargo test --locked -p workstation -p certs -p repo-check --no-fail-fast`，全部通過（workstation 單元測試 44 個、`tests/identity.rs` 4 個、`tests/login.rs` 17 個、`tests/register.rs` 10 個；certs 全部，含 `tests/workstation_tool.rs`；repo-check 全部；clippy 沒有警告），輸出摘錄在 [review-fixes.txt](data/2026-10-24-615-workstation-register/review-fixes.txt)。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 對以 `contract::certs::workstation_signing_input` 驗證的替身：回應遺失後重送得到 200 算成功；503 後重試成功；409、401、403 stale、422 各以固定結束碼停止、不重試；每次嘗試的 nonce 都不同（測試） | 通過：`a_lost_answer_*`、`a_503_*`、`each_refusal_*`，nonce 由 `assert_signed_afresh` 檢查 |
| 回應檢查：硬體 ID、兩個雜湊、CN、憑證公鑰任一不符時不記錄，固定結束碼（測試） | 通過：`an_answer_that_is_not_this_gateways_registration_is_not_recorded`（另加鏈與根的檢查） |
| 真的 certs + Postgres：登錄、重跑、「伺服器提交、回應遺失」→ 剛好一筆登錄、一張憑證、一個 `certs.gateway_registered` 事件 | 通過：`services/certs/tests/workstation_tool.rs` |
| 日誌與紀錄不含 cookie、私鑰、簽章標頭值（測試，TRACE） | 通過：`assert_nothing_secret_shows` |
| 原子紀錄：中斷的寫入不留下半個檔案（測試） | 通過：`a_write_cut_off_by_a_crash_leaves_the_previous_record_whole` 與 `journal` 的單元測試；留下的只有被略過的暫存檔，紀錄本身完整 |
| `cargo test -p workstation`、`cargo test -p certs`、`cargo clippy --workspace --all-targets -- -D warnings` 通過 | 通過 |

延後的驗證（進階，依組織規範另開 P2）：真實網路下的重試參數、服務在一次執行中途重啟、更多故障注入的組合。TPM 與 ssh 是 Laysi/trustiot-v2#616。
