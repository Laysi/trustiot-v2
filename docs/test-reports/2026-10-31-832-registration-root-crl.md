# 出廠登錄帶根 CRL、工作站裝到閘道器

Laysi/trustiot-v2#832

## 環境

- 建置主機（arm64 Linux VM）；`CI=1`，所以需要容器的測試（Postgres、swtpm）沒有容器時會失敗而不是略過。
- Rust 1.99.0（`rust-toolchain.toml`）、Bun 1.4.2（`.prototools`）。
- 分支 `feat/832-registration-root-crl`，commit 192a21f3（基於 main 的 f4d9ab22）。
- 全部是 loopback 的替身：真的 certs 服務與 Postgres、工作站的登錄替身（`factory/workstation/tests/support/registry.rs`）、假的 ssh，以及跑在 swtpm 上的真 `provision install`。沒有用到真的閘道器。

## 指令

```sh
cargo fmt --all --check
cargo clippy --locked --keep-going --workspace --all-targets -- -D warnings
cargo run --locked -p openapi -- --check
bun install --cwd frontend --frozen-lockfile
bun run --cwd frontend api:check
bun run --cwd frontend typecheck
CI=1 cargo test --locked -p certs -p workstation -p agent
CI=1 cargo test --locked --workspace --exclude agent
bun install --cwd scripts --frozen-lockfile && bun test scripts

# 驗收條件對應的測試（--nocapture 印出證據行）
CI=1 cargo test --locked -p certs --test registration the_answers_carry_the_root_crl -- --nocapture
CI=1 cargo test --locked -p workstation the_root_crl
CI=1 cargo test --locked -p workstation --test register an_answer_that_is_not
CI=1 cargo test --locked -p workstation --test pipeline_swtpm
CI=1 cargo test --locked -p agent --test provision_install
CI=1 cargo test --locked -p agent --lib provision::install
```

## 結果

- fmt、clippy（整個 workspace，`-D warnings`）、`openapi -- --check`（「api/openapi.json is up to date」）、前端的 `api:check` 與 `typecheck` 都通過。
- `CI=1 cargo test -p certs -p workstation -p agent`：所有 test result 都是 ok，0 failed。
- `cargo test --workspace --exclude agent` 與 `bun test scripts`：見下方「全工作區」。

### certs：回應帶根 CRL（`services/certs/tests/registration.rs`，真的服務）

`the_answers_carry_the_root_crl_adopted_with_their_root` 的輸出：

```text
no root CRL adopted: 201 without root_crl
root CRL 1 adopted: the 200 and the 201 carry it
root CRL 2 adopted: the repeat carries it
an older root CRL written back: the repeat still carries root CRL 2
```

- 根 CRL 檔是空的時，201 的 JSON 沒有 `root_crl` 這個鍵（不是 `null`），其餘欄位照舊。
- 採用 1 號之後，重送（200）與新登錄（201）都帶它：DER 與檔案相同，每行最多 64 字元；重送除了 `root_crl` 之外與第一次的登錄相同。
- 採用 2 號之後，重送帶 2 號。
- 把 1 號寫回去時，服務拒絕採用（ERROR，`kept_number` 是 2），回應仍然帶 2 號。

### 工作站（`factory/workstation`）

- `install::tests::the_root_crl_is_sent_last_and_the_agent_must_report_it`：
  - stdin 先是三張憑證，最後是重新編碼的 `X509 CRL`，DER 等於紀錄裡的根 CRL，紀錄裡前後的文字不會送出。
  - 報告 `written` 算安裝完成，日誌記 `"root_crl":true`；報告 `newer` 也算完成，日誌記 `"kept_newer_root_crl":true`。
  - 送了根 CRL 而報告只有兩行時，結束碼 34。
  - 舊 agent 的 8（「not a CERTIFICATE; only certificates are accepted」）對應結束碼 32，訊息提示更新 agent。
  - 沒送根 CRL 時 stdin 只有三張憑證；這時報告有三行是 34。
- `install::tests::a_root_crl_the_pinned_root_did_not_sign_is_never_sent`：紀錄裡的根 CRL 是別的根簽的、不是 PEM、或有兩個時，結束碼 34，替身沒有被呼叫。
- `tests/register.rs`：
  - `the_root_crl_of_the_answer_is_recorded_and_a_newer_one_replaces_it`：平台沒有根 CRL 時紀錄沒有，採用 3 號之後重跑記下 3 號，採用 4 號之後換成 4 號；回應帶著別的根簽的根 CRL 時結束碼 18，紀錄仍是 4 號。
  - `an_answer_that_is_not_this_gateways_registration_is_not_recorded` 加了 `ForeignRootCrl` 與 `RootCrlNotPem`。
- `tests/pipeline_swtpm.rs`（工作站執行檔 → 假 ssh → 真的 `provision install` 跑在 swtpm 上）：
  - 平台還沒有根 CRL 時，輸出寫「the registration has no root CRL」，`broker.crl` 沒有寫，agent 的啟動檢查是 `RootCrl::Missing`。
  - 平台採用 1 號之後模擬重刷：輸出寫「it has the registration's root CRL」，`broker.crl` 的 DER 等於平台採用的那份，agent 的啟動檢查載入 1 號。

### agent（`edge/agent`）

- `tests/provision_install.rs` 的 `a_root_crl_sent_is_installed_only_forward_and_all_or_nothing`（swtpm）：
  - 三個檔案都寫入，`broker.crl` 是標準 PEM，權限 0644，擁有者與目錄相同。
  - agent 的啟動檢查載入 2 號：自己的鏈以 `check_chain_under` 通過；經由這張根 CRL 撤銷的另一個簽發 CA 的鏈則以 revoked 失敗。
  - 重跑是 `unchanged`。送較小的編號時報告 `newer`，stderr 有說明。
  - 以下都以 8 結束，而且沒有寫任何檔案：同編號不同內容、別的根簽的、竄改過的、撤銷自己簽發 CA 的、根 CRL 放在第一個、兩個根 CRL、根 CRL 放在中間。
  - 送 3 號時寫入；不送根 CRL 時報告兩行。
  - 換新根時，新根與它的根 CRL 1 號一起寫入。
  - 沒有設定 `broker.crl`，或 `broker.crl` 的目錄不存在時，以 9 結束、不寫任何檔案。
- `an_install_plans_the_root_crl_only_under_the_lock_so_two_never_roll_it_back`：
  - 測試先持有狀態目錄的 flock，等 `/proc/locks` 顯示 install 在等這把鎖，再把 3 號放進去並放開鎖。這時送 2 號的 install 報告 `newer`，檔案仍是 3 號。
  - 變異檢查：把加鎖移回讀取 `broker.crl` 之後（審查時指出的順序），這個測試會以 `root-crl.pem: written` 失敗；改回來之後通過。
- 單元測試：
  - `standard_input_is_three_certificates_then_at_most_one_root_crl`
  - `crl_numbers_compare_as_numbers_of_any_size`

### 全工作區

- `CI=1 cargo test --locked --workspace --exclude agent`：265 個 test result，2076 passed、0 failed。
- `bun test scripts`：1732 pass、9 skip、0 fail，共 54 個檔案、1741 個測試。略過的是需要叢集或 podman e2e 的測試，與這次變更無關。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 有根 CRL 時登錄回應帶它，沒有時照常回應（certs 測試）；`openapi -- --check` 通過 | 通過 |
| 工作站把根 CRL 送到閘道器，`provision install` 裝好後，agent 以它檢查鏈上的簽發 CA（工作站與 agent 測試，替身） | 通過 |
| 不是要安裝的根所簽的根 CRL 讓安裝失敗，而且不留下半套檔案 | 通過 |
| 文件更新；`CI=1 cargo test -p certs -p workstation -p agent` 通過；測試報告 | 通過 |

待測（進階驗證，延後）：

- 在真的閘道器上安裝：目前只用替身，lab 的閘道器是展示用的閘道器。
- 在硬體上實測舊 agent 對上新工作站。
- 以第二個實作（例如 openssl）佐證閘道器上的根 CRL 檢查。
