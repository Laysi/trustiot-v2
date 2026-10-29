# 憑證鏈與根憑證裝回閘道器（2026-10-29）

Laysi/trustiot-v2#617：閘道器端的 `trustiot-agent provision install`（檢查後寫進 agent 設定的 `identity.certificate` 與 `broker.ca`；參數、檢查、寫入規則與結束碼見 [edge/agent/README.md](../../edge/agent/README.md)「安裝憑證鏈與根憑證」），以及工作站端的 `install` 模組（送出前的檢查、送到閘道器的內容與結束碼 30–33，見 [factory/workstation/README.md](../../factory/workstation/README.md)「把憑證鏈與根憑證裝回閘道器」）。這份報告是已完成的驗收條件的證據：全部由自動化測試實際執行，TPM 是 `testkit::swtpm` 起的 swtpm，`CI` 有設，需要容器的測試不能略過。從 Laysi/trustiot-v2#616 的 ssh 流程在登錄後呼叫工作站端，以及假 ssh 程式看到的 stdin 的測試，還沒有做（見「結論」）。

## 環境

- **程式碼**：分支 `feat/617-install-chain`，起初基於 main `ea5f27c7`（Laysi/trustiot-v2#613 的 `provision` 子命令）：agent 端是 commit `d14f38a7`，工作站端是 `e134c66a`。之後以 merge 併入 main `371572a3`（Laysi/trustiot-v2#83 的憑證換發；`edge/agent/src/setup.rs` 的 import 有衝突，只合併兩邊的 import）成為 `748fbcde`，再加上不倒退換發過的憑證的檢查 `67e57bcb`。報告的 commit 只加文件與測試輸出。
- **機器**：建置主機（x86_64 Linux），經檢查佇列執行；swtpm 由 `testkit::swtpm` 在容器裡啟動。測試以一般使用者執行，所以「擁有者是 root」的情況驗的是失敗時的清理（見下方）；以 root 執行時同一個測試改驗擁有者。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust；aarch64 建置用 `rustup target add aarch64-unknown-linux-gnu` 的標準目標。
- **設定**：`CI=true`。測試從不開啟主機的 `/dev/tpmrm0`：swtpm 的測試以注入的連線函式取代裝置（函式檢查設定的裝置路徑是 `/dev/tpmrm0`），執行檔的測試在讀設定之前就結束。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=true cargo test --locked -p agent
cargo test --locked -p workstation
cargo test --locked -p repo-check
cargo build --locked --target aarch64-unknown-linux-gnu -p agent
cargo deny --locked check
cargo test --locked --workspace --exclude agent
```

驗收證據可以單獨重跑：

```sh
CI=true cargo test --locked -p agent --test provision_install --test binary
CI=true cargo test --locked -p agent --lib provision::install
cargo test --locked -p workstation --lib install
```

## 結果

| 指令 | 哪個 commit | 結果 |
| --- | --- | --- |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`CI=true cargo test --locked -p agent`、`cargo test --locked -p workstation`、`cargo test --locked -p repo-check`、`cargo build --locked --target aarch64-unknown-linux-gnu -p agent`、`cargo deny --locked check` | `e134c66a` | 全部通過：agent 398 個測試（lib 239、`binary` 13、`provision_install` 3、`provision` 3、`tpm_provision` 6 等），workstation 80 個（lib 49、`identity` 4、`login` 17、`register` 10），repo-check 28 個，0 失敗；aarch64 建置完成；`advisories ok, bans ok, licenses ok, sources ok`（exit 0 at 16:53:12 (build host)） |
| `cargo test --locked --workspace --exclude agent`（含 `tools/sql-check` 與 `repo-check`） | `e134c66a` 加上本報告（未 commit 的工作樹） | 通過：236 組、1844 個測試通過，0 失敗，1 個 ignored（exit 0 at 17:26:32 (build host)） |
| 同上，在 arm64 macOS 的建置主機 | 同上 | `ai-clients` 的 `an_unanswered_connection_attempt_is_a_connect_timeout` 失敗（`the accept queue never filled up after 64 connections`）：測試依賴 Linux 在 accept 佇列滿時不回應 SYN 的行為，與本變更無關；上一列在 x86_64 Linux 重跑通過 |
| 上面「驗收證據」的三個指令 | `e134c66a` | 全部通過，每個測試的名稱與結果在 [install-tests.txt](data/2026-10-29-617-install-chain/install-tests.txt) |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`CI=true cargo test --locked -p agent` | merge 之後、與 `67e57bcb` 相同的工作樹（commit 前） | 全部通過：agent 434 個測試（lib 264，含 Laysi/trustiot-v2#83 的換發；`provision_install` 4，多了 `a_renewed_certificate_is_never_replaced_by_an_older_one`），0 失敗（exit 0 at 18:09:42 (build host)） |
| `cargo test --locked --workspace --exclude agent`、`cargo build --locked --target aarch64-unknown-linux-gnu -p agent`、`cargo deny --locked check` | `5c8fe456`（程式與 `67e57bcb` 相同） | 全部通過：workspace（不含 agent，含 workstation、`tools/sql-check` 與 `repo-check`）1844 個測試通過，0 失敗，1 個 ignored；aarch64 建置完成；`advisories ok, bans ok, licenses ok, sources ok`（exit 0 at 20:10:17 (build host)）。第一次排進佇列時建置主機磁碟不足、工作沒有執行，清出空間後補跑 |

## 測試內容

- `edge/agent/tests/provision_install.rs`（swtpm；與執行檔走同一段程式 `agent::provision::install::run`，TPM 金鑰由 `tpm::ensure_key` 在 swtpm 的持久 handle 建立，測試 PKI 以 rcgen 產生：自簽根 CA、根簽的簽發 CA、簽發 CA 簽的 leaf）：
  - `wrong_certificates_are_refused_and_nothing_is_written`：11 種錯的輸入，在空的目錄與已裝好的檔案上各跑一次，都是結束碼 8、stderr 說明原因並寫 `nothing was written`，兩個目錄的每個檔案（名稱、inode、權限、擁有者、內容）與執行前相同：leaf 的公鑰不是 TPM 金鑰的（軟體金鑰）、CN 是別的硬體 ID、鏈通不到給的根（leaf 與簽發 CA 接上別的根）、根不是 CA、順序顛倒（根、簽發 CA、leaf）、缺簽發 CA（leaf 直接接根，以及只有 leaf）、夾帶 `PRIVATE KEY` 區塊、區塊以外的文字、超過 32 KiB、空的輸入。設定了 `broker.crl` 而 CRL 是別的根簽的：結束碼 8，訊息指出 `broker.crl`，沒有寫任何檔案；換成這個根簽的 CRL：通過，兩個檔案都是 `unchanged`。自發但不是自簽的根、偽造的簽章由單元測試涵蓋。
  - `the_installed_files_pass_the_agents_checks_a_rerun_writes_nothing_and_a_new_root_replaces_them`：第一次兩個檔案都是 `written`，內容是 leaf 加簽發 CA 與根憑證的 PEM、權限 0644、擁有者是給的帳號；`agent::setup::Setup::new_with`（與 agent 啟動相同的檢查，用 swtpm 的金鑰）接受裝好的檔案，閘道器 ID 是硬體 ID、憑證在效期內。以 CRLF 換行重跑：兩個都是 `unchanged`，inode 與內容不變。換一整套 PKI：兩個都是 `written`、inode 都換了（原子替換）、`Setup` 仍接受。只換簽發 CA：只有鏈的檔案重寫，根的 inode 不變。
  - `a_renewed_certificate_is_never_replaced_by_an_older_one`（併入 Laysi/trustiot-v2#83 之後加的）：裝上 2026 年起效的憑證（代表 agent 換發過的）之後，送 2025 年起效、同一把 TPM 金鑰的憑證（代表重送登錄拿到的）：結束碼 8、`holds a newer certificate for this TPM key`，沒有寫任何檔案；同一張 2026 年的是 `unchanged`；2027 年的照常寫入；現有檔案是別把金鑰、較新的憑證時，照常換成送來的，`Setup` 接受。
  - `owners_are_set_per_file_and_an_unusable_configuration_is_refused_before_reading`：根憑證的擁有者設為 root（0:0）時，以一般使用者執行：結束碼 1，訊息提示要 root，`nothing was replaced`，兩個目錄裡沒有任何檔案（已經寫好的鏈的暫存檔也刪掉了）。金鑰不是 TPM、`identity.certificate` 或 `broker.ca` 的目錄不存在、兩個路徑經符號連結或 `..` 指向同一個檔案、`broker.crl` 與憑證是同一個檔案、設定檔不存在：結束碼 9，stdin 沒有被讀（讀就 panic），TPM 沒有被開啟（開就 panic），沒有寫任何檔案。
- `edge/agent/tests/binary.rs`（真的執行檔）：參數錯誤（缺 `--hardware-id`、不合法的硬體 ID、重複或未知的參數、`--help` 與其他參數一起）結束碼 2，`--help` 單獨使用印出自己的說明；沒有 agent 設定時結束碼 9，沒有寫任何檔案。
- agent 的單元測試（`src/provision/install.rs`）：參數規則；結束碼與 `provision` 的 3–7 不重疊；以 `getpwnam_r` 查帳號（root 是 0:0、不存在的帳號、含 NUL 的名稱）；只接受 `CERTIFICATE` 區塊與空行（CRLF 可以），錯誤訊息不引用輸入；鏈的檢查（含偽造的簽章、自發但不是自簽的根）；寫入在第二個檔案中途失敗時（注入錯誤的寫入函式），兩個舊檔案完整、沒有留下暫存檔；內容正確但權限、擁有者不對或是符號連結的檔案會被替換；設定不了擁有者時暫存檔被刪除。
- 工作站的單元測試（`factory/workstation/src/install.rs`，以記下參數與 stdin 的替身代替閘道器）：送出的命令是固定的 `provision install --hardware-id tg-0001`，stdin 剛好是 leaf、簽發 CA、根三張重新編碼的 PEM，紀錄裡 PEM 以外的文字不送出；日誌有 `installed`。紀錄的根憑證不是操作員給的指紋、紀錄的 `root_sha256` 不是、或整筆紀錄一致地換成別的根（只有操作員的指紋擋得下）：結束碼 30，替身沒有被呼叫，日誌有 `root_mismatch`。鏈只有 leaf：結束碼 31；鏈不通、CN 不符、三張憑證、夾帶私鑰：結束碼 1，都沒有送出；紀錄是 `pending`、沒有紀錄：沒有送出；硬體 ID 含空白與分號：結束碼 2。閘道器回 8 與 9：結束碼 32 與 33，其他狀態與訊號是 1，訊息裡沒有控制字元，日誌有 `remote_status`。`Payload` 的 `Debug` 不顯示內容。

## 寫作期間的審查閘門

commit 前的 Codex 審查擋下兩處，都已修正並由上面的測試覆蓋：

| 項目 | 處理 |
| --- | --- |
| agent：`identity.certificate` 與 `broker.ca` 只比對字串，經符號連結或 `..` 指向同一個檔案時，第二次 rename 會蓋掉第一個 | 先解開兩個檔案所在的目錄（`canonicalize`）再比對；`broker.crl` 與任一個相同也拒絕（結束碼 9） |
| 工作站：`root_sha256` 與 `root_certificate` 在同一筆紀錄，改得了紀錄的人可以兩個一起換 | `install` 多了 `pinned_root_sha256` 參數，是操作員這次執行給的 `--root-sha256`，不從紀錄讀；根憑證與紀錄的 `root_sha256` 都要等於它 |

併入 Laysi/trustiot-v2#83 之後發現：重送登錄時平台回的永遠是登錄當時的憑證，已經換發過的閘道器重跑安裝會換回較舊、可能已撤銷或過期的憑證。agent 端因此加上第 5 項檢查（同一把金鑰、notBefore 較晚的憑證不被取代，結束碼 8），測試是 `a_renewed_certificate_is_never_replaced_by_an_older_one`。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 錯的憑證都被拒絕且沒有寫任何檔案（leaf 公鑰不是 TPM 的、CN 不是硬體 ID、鏈不通到給的根、根不是自簽 CA、順序顛倒或缺簽發 CA）（swtpm） | 通過（`wrong_certificates_are_refused_and_nothing_is_written`） |
| 重跑內容相同時不寫任何檔案（inode 與內容不變）；內容不同時原子更新 | 通過（`the_installed_files_pass_the_agents_checks_a_rerun_writes_nothing_and_a_new_root_replaces_them`） |
| 擁有者與權限符合規則，中斷不留下半個檔案 | 通過（同上的權限與擁有者；`owners_are_set_per_file_...` 與單元測試 `a_write_that_fails_half_way_...`、`a_staged_file_is_removed_...`）。根憑證屬於 root 的寫入成功路徑要以 root 執行測試才驗得到，這次以一般使用者執行，驗的是失敗時的清理 |
| agent::setup 接受裝好的檔案（swtpm 金鑰） | 通過（`Setup::new_with`，同上） |
| 工作站端：登錄後自動執行遠端安裝；假的 ssh 程式看到的 stdin 只有憑證鏈與根憑證 PEM | 未完成：要接在 Laysi/trustiot-v2#616 的 ssh 流程上，那個分支還沒好。已完成的部分：`install::install` 送出的 stdin 只有三張 PEM（`only_the_chain_and_the_root_are_sent_with_a_fixed_command`，替身不是 ssh 程式） |
| 紀錄裡 root_certificate 與 root_sha256 不符時，不執行遠端安裝、固定結束碼 | 通過（`a_root_that_is_not_the_pinned_one_is_never_sent`，結束碼 30）；另外也以操作員當次給的指紋比對 |
| `CI=true cargo test --locked -p agent`、`cargo test -p workstation`、`cargo build --locked --target aarch64-unknown-linux-gnu -p agent`、`cargo clippy --workspace --all-targets -- -D warnings` | 通過 |
| 在真的閘道器上安裝 | 待測：不在本 issue 的範圍，目前不動 lab 的閘道器 |
