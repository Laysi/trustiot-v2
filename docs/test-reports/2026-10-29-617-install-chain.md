# 憑證鏈與根憑證裝回閘道器（2026-10-29）

Laysi/trustiot-v2#617：閘道器端的 `trustiot-agent provision install`（檢查後寫進 agent 設定的 `identity.certificate` 與 `broker.ca`；參數、檢查、寫入規則與結束碼見 [edge/agent/README.md](../../edge/agent/README.md)「安裝憑證鏈與根憑證」），以及工作站端：`install` 模組（送出前的檢查、送到閘道器的內容與結束碼 30–35）與 Laysi/trustiot-v2#616 的 `register --gateway` 在登錄後經 ssh 呼叫它（見 [factory/workstation/README.md](../../factory/workstation/README.md)「把憑證鏈與根憑證裝回閘道器」）。這份報告是驗收條件的證據：全部由自動化測試實際執行，TPM 是 `testkit::swtpm` 起的 swtpm，`CI` 有設，需要容器的測試不能略過。

## 環境

- **程式碼**：分支 `feat/617-install-chain`，起初基於 main `ea5f27c7`（Laysi/trustiot-v2#613 的 `provision` 子命令）：agent 端是 commit `d14f38a7`，工作站端是 `e134c66a`。之後以 merge 併入 main `371572a3`（Laysi/trustiot-v2#83 的憑證換發；`edge/agent/src/setup.rs` 的 import 有衝突，只合併兩邊的 import）成為 `748fbcde`，再加上不倒退換發過的憑證的檢查 `67e57bcb`。安全審查的修正是 `cc60a3f1`、`0c3f50c9`、`b44bde19`、`14709937`、`e8ef0e5c`、`a1f3edb7`（見「安全審查」），測試的修正是 `67bc7f93`。Laysi/trustiot-v2#616 的分支 `feat/616-workstation-ssh` 以 merge 併入兩次（`9140103a`，head `a9478968`；`a869740e`，head `b0dbc723`，含它的安全審查修正；兩次都只有 README 的衝突，兩邊的內容都保留），接上 ssh 流程的是 `c0e0d166`，之後併入 main `41a0a342`（`4d759884`）與 `781073bc`（`e70b07a8`，Laysi/trustiot-v2#755，不動工作站與 agent）。Laysi/trustiot-v2#616 的第二輪安全審查修正（head `be91ce29`：主機金鑰以 `HostKeyAlias=trustiot-<硬體 ID>` 綁硬體 ID、只查工具自己的 `known_hosts`、每次執行前以 `ssh -G` 檢查、結束碼 27）以第三次 merge 併入（`7c5ca0fb`）：`gateway.rs`、`lib.rs`、`pipeline.rs`、README 與兩個測試檔有衝突，安裝步驟改走同一個 `run_retried`，所以和 `provision` 用同一組選項、同樣先做 `ssh -G` 檢查。Laysi/trustiot-v2#616 以 squash 合併成 main 的 `b7ac543b`（PR Laysi/trustiot-v2#757）之後，再併入 main（`56cf34d5`）：工作站的檔案內容與 `be91ce29` 相同，合併後的樹與 `7c5ca0fb` 完全相同，PR 的差異只剩本 issue 的變更。最後併入 main `b6a3a6d7`（`dcebecdc`，Laysi/trustiot-v2#758 的離線緩衝；`edge/agent/src/lib.rs` 的模組宣告與 `tests/binary.rs` 的模組說明有衝突，兩邊都保留；新的 `[buffer]` 設定是選填的，安裝測試寫的設定檔照樣載入），並拿掉 agent 說明裡 `provision install` 遺留的 `--config`（`aa6a747a`）。ssh 安裝步驟的安全審查（head `4e27e69e`）的修正是 `029d3e3f`、`4ed4170a`、`66a4642a`、`34b18659`、`9b9f4b06`（見「安全審查（第二輪）」），之後併入 main `f46c8a89`（`6e1ce8db`，Laysi/trustiot-v2#752 與 Laysi/trustiot-v2#759，不動工作站與 agent，沒有衝突）。報告的 commit 只加文件。
- **機器**：建置主機（x86_64 Linux）與建置主機（arm64 Linux VM），經檢查佇列執行，各列寫明是哪一台；swtpm 由 `testkit::swtpm` 在容器裡啟動。測試以一般使用者執行，所以「擁有者是 root」的情況驗的是拒絕與失敗時的清理（見下方）；以 root 執行時同一個測試改驗擁有者。
- **工具**：`.prototools` 與 `rust-toolchain.toml` 釘選的 Rust；aarch64 建置用 `rustup target add aarch64-unknown-linux-gnu` 的標準目標。
- **設定**：`CI=true`。測試從不開啟主機的 `/dev/tpmrm0`：swtpm 的測試以注入的連線函式取代裝置（函式檢查設定的裝置路徑是 `/dev/tpmrm0`），執行檔的測試在讀設定之前就結束。工作站的 ssh 測試用放在 `PATH` 最前面的假 `ssh`，從不連到真的閘道器。

## 指令

在 repo 根目錄：

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
CI=true cargo test --locked -p agent
CI=true cargo test --locked -p workstation
cargo test --locked -p repo-check
cargo build --locked --target aarch64-unknown-linux-gnu -p agent
cargo deny --locked check
cargo test --locked --workspace --exclude agent
```

驗收證據可以單獨重跑：

```sh
CI=true cargo test --locked -p agent --test provision_install --test binary
CI=true cargo test --locked -p agent --lib provision::install
CI=true cargo test --locked -p workstation --lib install
CI=true cargo test --locked -p workstation --test ssh --test pipeline_swtpm
```

## 結果

| 指令 | 哪個 commit | 結果 |
| --- | --- | --- |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`CI=true cargo test --locked -p agent`、`cargo test --locked -p workstation`、`cargo test --locked -p repo-check`、`cargo build --locked --target aarch64-unknown-linux-gnu -p agent`、`cargo deny --locked check` | `e134c66a` | 全部通過：agent 398 個測試（lib 239、`binary` 13、`provision_install` 3、`provision` 3、`tpm_provision` 6 等），workstation 80 個（lib 49、`identity` 4、`login` 17、`register` 10），repo-check 28 個，0 失敗；aarch64 建置完成；`advisories ok, bans ok, licenses ok, sources ok`（exit 0 at 16:53:12 (build host)） |
| `cargo test --locked --workspace --exclude agent`（含 `tools/sql-check` 與 `repo-check`） | `e134c66a` 加上本報告（未 commit 的工作樹） | 通過：236 組、1844 個測試通過，0 失敗，1 個 ignored（exit 0 at 17:26:32 (build host)） |
| 同上，在 arm64 macOS 的建置主機 | 同上 | `ai-clients` 的 `an_unanswered_connection_attempt_is_a_connect_timeout` 失敗（`the accept queue never filled up after 64 connections`）：測試依賴 Linux 在 accept 佇列滿時不回應 SYN 的行為，與本變更無關；上一列在 x86_64 Linux 重跑通過 |
| 上面「驗收證據」的前三個指令 | `e134c66a` | 全部通過，每個測試的名稱與結果在 [install-tests.txt](data/2026-10-29-617-install-chain/install-tests.txt) |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`CI=true cargo test --locked -p agent` | merge 之後、與 `67e57bcb` 相同的工作樹（commit 前） | 全部通過：agent 434 個測試（lib 264，含 Laysi/trustiot-v2#83 的換發；`provision_install` 4，多了 `a_renewed_certificate_is_never_replaced_by_an_older_one`），0 失敗（exit 0 at 18:09:42 (build host)） |
| `cargo test --locked --workspace --exclude agent`、`cargo build --locked --target aarch64-unknown-linux-gnu -p agent`、`cargo deny --locked check` | `5c8fe456`（程式與 `67e57bcb` 相同） | 全部通過：workspace（不含 agent，含 workstation、`tools/sql-check` 與 `repo-check`）1844 個測試通過，0 失敗，1 個 ignored；aarch64 建置完成；`advisories ok, bans ok, licenses ok, sources ok`（exit 0 at 20:10:17 (build host)）。第一次排進佇列時建置主機磁碟不足、工作沒有執行，清出空間後補跑 |
| `cargo clippy --locked -p agent --all-targets -- -D warnings`、`CI=true cargo test --locked -p agent` | 安全審查的修正之後（`a1f3edb7` 與 `9140103a`）加上 `67bc7f93` 的測試修正（commit 前） | 第一次：`the_root_crl_is_read_only_from_a_directory_only_root_or_the_agent_can_write` 失敗（測試把陌生人的目錄寫成屬於 agent 帳號，預期的訊息不符）；修正後第二次：`provision_install` 的 `wrong_certificates_...` 失敗（CRL 放在 `files()` 比對的狀態目錄裡，改寫 CRL 就讓比對不同，FIFO 的情況還會卡在讀取）。CRL 移到自己的目錄後全部通過：lib 271、`binary` 13、`provision_install` 4 等，0 失敗（exit 0 at 21:10:18 (build host)） |
| `cargo clippy --locked -p workstation --all-targets -- -D warnings`、`CI=true cargo test --locked -p workstation` | 與 `c0e0d166` 相同的工作樹（commit 前） | 全部通過：lib 65、`identity` 4、`login` 17、`pipeline_swtpm` 1、`register` 10、`ssh` 10、`verify` 4，0 失敗（exit 0 at 21:24:40 (build host (arm64 Linux VM))） |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`CI=true cargo test --locked -p agent` | `845ee863`（併入 main `41a0a342` 與 Laysi/trustiot-v2#616 `b0dbc723` 之後）加上本報告 | 全部通過：agent 441 個測試，0 失敗（exit 0 at 21:30:40 (build host)）。第一次以 `a869740e` 跑時 `cargo fmt --all --check` 失敗，原因是 Laysi/trustiot-v2#616 併進來的一個測試沒有經過 rustfmt，`845ee863` 只是 rustfmt 的排版 |
| `cargo test --locked --workspace --exclude agent`（含 workstation 的 `ssh` 與 `pipeline_swtpm`、`tools/sql-check` 與 `repo-check`） | 同上 | 通過：241 組、1882 個測試通過，0 失敗，1 個 ignored（exit 0 at 21:51:52 (build host (arm64 Linux VM))） |
| `cargo build --locked --target aarch64-unknown-linux-gnu -p agent`、`cargo deny --locked check` | 同上 | 通過：aarch64 建置完成；`advisories ok, bans ok, licenses ok, sources ok`（exit 0 at 22:07:04 (build host)） |
| `cargo fmt --all --check`、`cargo test --locked --workspace --exclude agent`、`CI=true cargo test --locked -p agent` | `e70b07a8`（併入 main `781073bc` 之後） | 通過：241 組、1905 個通過，0 失敗，1 個 ignored；agent 441 個，0 失敗（exit 0 at 22:35:55 (build host)） |
| `cargo fmt --all --check`、`cargo clippy --locked -p workstation --all-targets -- -D warnings`、`CI=true cargo test --locked -p workstation` | 第三次併入 Laysi/trustiot-v2#616（`be91ce29`）的工作樹，與 `7c5ca0fb` 相同（commit 前） | 通過：9 組、120 個測試，0 失敗，`pipeline_swtpm` 不能略過（exit 0 at 22:40:01 (build host (arm64 Linux VM))） |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo deny --locked check`、`cargo test --locked --workspace --exclude agent`、`CI=true cargo test --locked -p agent` | `7c5ca0fb`，樹與 `56cf34d5`（併入 main `b7ac543b`）相同 | 全部通過：`advisories ok, bans ok, licenses ok, sources ok`；241 組、1911 個通過，0 失敗，1 個 ignored；agent 441 個，0 失敗（exit 0 at 23:29:30 (build host)）。agent 的程式碼在 `845ee863` 之後沒有變，aarch64 建置沿用上一列 |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo deny --locked check`、`cargo build --locked --target aarch64-unknown-linux-gnu -p agent`、`cargo test --locked --workspace --exclude agent`、`CI=true cargo test --locked -p agent` | `aa6a747a`（併入 main `b6a3a6d7` 之後，2026-10-30 執行） | 全部通過：`advisories ok, bans ok, licenses ok, sources ok`；aarch64 建置完成；241 組、1911 個通過，0 失敗，1 個 ignored；agent 538 個（含 Laysi/trustiot-v2#758 的測試與 `--help` 的新檢查），0 失敗（exit 0 at 00:08:18 (build host)） |
| `cargo fmt --all --check`、`cargo clippy --locked -p agent -p workstation --all-targets -- -D warnings`、`CI=true cargo test --locked -p agent --test provision_install --test binary`、`CI=true cargo test --locked -p agent --lib provision`、`CI=true cargo test --locked -p workstation` | 與 `029d3e3f`（L4）相同的工作樹（commit 前） | 第一次 clippy 的 `type_complexity` 擋下測試裡的一個型別，改成型別別名後全部通過：agent `provision_install` 4、`binary` 14、lib 的 `provision` 38，workstation 各組，0 失敗（exit 0 at 00:39:12 (build host)） |
| `cargo clippy --locked -p workstation --all-targets -- -D warnings`、`cargo test --locked -p workstation` | 與 `4ed4170a`（L3）相同的工作樹（commit 前） | 全部通過：lib 71、`ssh` 14、`verify` 5 等，0 失敗（exit 0 at 00:49:12 (build host (arm64 Linux VM))）。之前一版以 stderr 的 `sudo:` 判斷 sudo 拒絕，也通過（00:44:56），commit 前的審查指出 agent 也印得出那段文字，改成由安裝腳本以 `sudo -n -l` 確認後回 92 |
| 同上 | 與 `66a4642a`（I1）相同的工作樹（commit 前） | 全部通過：lib 72，其餘同上，0 失敗（exit 0 at 00:51:00 (build host (arm64 Linux VM))） |
| 同上 | 與 `34b18659`（L2）相同的工作樹（commit 前） | 全部通過：lib 75，其餘同上，0 失敗（exit 0 at 00:54:38 (build host (arm64 Linux VM))） |
| `cargo test --locked -p workstation --test verify`（README 的結束碼表與 `--help`） | 與 `9b9f4b06`（L1，只改 README）相同的工作樹（commit 前） | 5 個通過，0 失敗（exit 0 at 00:56:18 (build host (arm64 Linux VM))） |
| `cargo fmt --all --check`、`cargo clippy --locked --workspace --all-targets -- -D warnings`、`cargo deny --locked check`、`cargo build --locked --target aarch64-unknown-linux-gnu -p agent`、`cargo test --locked --workspace --exclude agent`、`CI=true cargo test --locked -p agent` | `6e1ce8db`（第二輪修正併入 main `f46c8a89` 之後） | 全部通過：`advisories ok, bans ok, licenses ok, sources ok`；aarch64 建置完成；245 組、1937 個通過，0 失敗，1 個 ignored；agent 538 個，0 失敗（exit 0 at 01:22:51 (build host)） |

## 測試內容

- `edge/agent/tests/provision_install.rs`（swtpm；與執行檔走同一段程式 `agent::provision::install::run`，TPM 金鑰由 `tpm::ensure_key` 在 swtpm 的持久 handle 建立，測試 PKI 以 rcgen 產生：自簽根 CA、根簽的簽發 CA、簽發 CA 簽的 leaf；設定檔與目錄在測試的目錄裡，權限 0644 與 0755）：
  - `wrong_certificates_are_refused_and_nothing_is_written`：11 種錯的輸入，在空的目錄與已裝好的檔案上各跑一次，都是結束碼 8、stderr 說明原因並寫 `nothing was written`，兩個目錄的每個檔案（名稱、inode、權限、擁有者、內容）與執行前相同：leaf 的公鑰不是 TPM 金鑰的（軟體金鑰）、CN 是別的硬體 ID、鏈通不到給的根（leaf 與簽發 CA 接上別的根）、根不是 CA、順序顛倒（根、簽發 CA、leaf）、缺簽發 CA（leaf 直接接根，以及只有 leaf）、夾帶 `PRIVATE KEY` 區塊、區塊以外的文字、超過 32 KiB、空的輸入。設定了 `broker.crl` 而 CRL 是別的根簽的：結束碼 8，訊息指出 `broker.crl`；換成這個根簽、撤銷了簽發 CA 的 CRL：結束碼 8，`does not pass as a client certificate`；`broker.crl` 是指向可用 CRL 的符號連結，或是 FIFO：結束碼 9，`is not a regular file`，沒有讀它（FIFO 不會卡住）；都沒有寫任何檔案。換成這個根簽、沒有撤銷的 CRL：通過，兩個檔案都是 `unchanged`。自發但不是自簽的根、偽造的簽章由單元測試涵蓋。
  - `the_installed_files_pass_the_agents_checks_a_rerun_writes_nothing_and_a_new_root_replaces_them`：第一次兩個檔案都是 `written`，內容是 leaf 加簽發 CA 與根憑證的 PEM、權限 0644、擁有者是給的帳號；`agent::setup::Setup::new_with`（與 agent 啟動相同的檢查，用 swtpm 的金鑰）接受裝好的檔案，閘道器 ID 是硬體 ID、憑證在效期內。以 CRLF 換行重跑：兩個都是 `unchanged`，inode 與內容不變。換一整套 PKI：兩個都是 `written`、inode 都換了（原子替換）、`Setup` 仍接受。只換簽發 CA：只有鏈的檔案重寫，根的 inode 不變。
  - `a_renewed_certificate_is_never_replaced_by_an_older_one`：裝上 2026 年起效的憑證（代表 agent 換發過的）之後，送 2025 年起效、同一把 TPM 金鑰的憑證（代表重送登錄拿到的）：結束碼 0，stdout 剛好是 `<identity.certificate>: renewed` 與 `<broker.ca>: unchanged` 兩行，stderr 說 `holds a newer certificate for this TPM key` 與 `it is kept`、沒有叫人刪掉檔案，兩個檔案都沒有被改寫；刪掉 `broker.ca` 再送一次：`renewed` 與 `written`，憑證的檔案還是同一個 inode、根照常裝上（第二輪安全審查 L4 之前這種情況是結束碼 8）；同一張 2026 年的是 `unchanged`；2027 年的照常寫入；現有檔案是別把金鑰、較新的憑證時，照常換成送來的，`Setup` 接受。換了根之後，舊根之下較新的憑證不擋新根的安裝（安全審查 L2）。
  - `owners_are_set_per_file_and_an_unusable_configuration_is_refused_before_reading`：以一般使用者執行、根憑證的擁有者設為 root（0:0）時，設定檔屬於測試的使用者而不是 root：結束碼 9（`must belong to root (uid 0)`），沒有讀、沒有寫任何檔案（設定不了擁有者時刪掉暫存檔由單元測試涵蓋）；以 root 執行時改驗兩個檔案的擁有者。金鑰不是 TPM、`identity.certificate` 或 `broker.ca` 的目錄不存在、目錄經符號連結或 `..`、兩個路徑指向同一個檔案、`broker.crl` 與憑證是同一個檔案、設定檔不存在：結束碼 9，stdin 沒有被讀（讀就 panic），TPM 沒有被開啟（開就 panic），沒有寫任何檔案。
- `edge/agent/tests/binary.rs`（真的執行檔）：參數錯誤（缺 `--hardware-id`、不合法的硬體 ID、重複或未知的參數、`--help` 與其他參數一起）結束碼 2，`--config` 結束碼 2（`--config is not accepted`），`--help` 單獨使用印出自己的說明；沒有 agent 設定（固定的 `/etc/trustiot/agent.toml`）時結束碼 9，沒有寫任何檔案。
- agent 的單元測試（`src/provision/install.rs`、`src/directory.rs`、`src/renewal/tests.rs`）：參數規則；結束碼與 `provision` 的 3–7 不重疊；以 `getpwnam_r` 查帳號；只接受 `CERTIFICATE` 區塊與空行（CRLF 可以），錯誤訊息不引用輸入；鏈的檢查（含偽造的簽章、自發但不是自簽的根）；寫入在第二個檔案中途失敗時兩個舊檔案完整、沒有留下暫存檔；內容正確但權限、擁有者不對或是符號連結的檔案會被替換；設定不了擁有者時暫存檔被刪除；目錄開好之後被搬走，寫入仍進到檢查過的那個目錄（`writes_go_to_the_directory_that_was_checked_even_if_it_is_moved`）；設定檔要屬於 root、別人不能寫，經符號連結的設定檔被拒絕；`identity.certificate` 與 `broker.ca` 的目錄要屬於各自的擁有者、別人不能寫；`broker.crl` 的目錄要屬於 root 或 agent 帳號、別人不能寫；以 fd 持有的目錄拒絕含連結或 `..` 的路徑、名稱只在開好的目錄裡解析；換發寫入憑證時等 `provision install` 持有的目錄鎖（`the_write_waits_for_the_directory_lock_that_provision_install_holds`）。
- 工作站的單元測試（`factory/workstation/src/install.rs`，以記下參數與 stdin 的替身代替閘道器）：送出的命令是固定的 `provision install --hardware-id tg-0001`，stdin 剛好是 leaf、簽發 CA、根三張重新編碼的 PEM，紀錄裡 PEM 以外的文字不送出；日誌有 `installed`。紀錄的根憑證不是操作員給的指紋、紀錄的 `root_sha256` 不是、或整筆紀錄一致地換成別的根（只有操作員的指紋擋得下）：結束碼 30，替身沒有被呼叫，日誌有 `root_mismatch`。鏈只有 leaf：結束碼 31；鏈不通、CN 不符、三張憑證、夾帶私鑰：結束碼 34，都沒有送出；紀錄是 `pending`、沒有紀錄：34，沒有送出；硬體 ID 含空白與分號：結束碼 2。閘道器回 8：32；9、2（agent 太舊）與 127（沒有 agent）：33；安裝腳本表示 sudo 拒絕的 92：33，訊息指向 sudoers 與 Laysi/trustiot-v2#219、說重跑沒有用，日誌的 `stop` 是 `sudo_refused`；狀態 1（即使 stderr 以 `sudo:` 開頭）、其他狀態與訊號：34，訊息裡沒有控制字元與 Unicode 格式字元（U+202E 等），日誌有 `remote_status` 與 `stop` 為 `failed`。狀態 0 而 stdout 不是 agent 的兩行報告（14 種：空的、一行、三行、沒有結尾換行、多一個空行、第二行是 `renewed`、其他字詞、相對路徑、只有 `/`、ESC、U+202E、CRLF、其他文字、不是 UTF-8）：34，沒有 `installed` 日誌；agent 的報告（含 `renewed`）照常算完成，`renewed` 時日誌的 `kept_renewed` 是 true。閘道器回 0 而日誌那一行寫不進去（日誌檔權限 0644）：35，訊息說閘道器裝好了憑證、附上報告，不說沒有裝上。各停止種類的結束碼是 30、31、32、33、33、34、35，名稱各不相同。`src/pipeline.rs`：登錄後的包裝只在 35 以外的停止說憑證沒有裝上，其他錯誤是 34。`Payload` 的 `Debug` 不顯示內容。`src/gateway.rs`：安裝的遠端指令是固定腳本加上檢查過的硬體 ID 與 `sudo` 或 `direct`，不合規則的硬體 ID 是用法錯誤；腳本以工具讀的同一個狀態（92）表示 sudo 拒絕，92 不和上傳腳本的 90、91 或 ssh 的 255 重疊；`Remote` 的 ssh 實作只接受 `provision install --hardware-id <id>` 這一種參數，硬體 ID 不合 `GatewayId` 的規則時在 ssh 執行前拒絕。
- `factory/workstation/tests/ssh.rs`（假的 `ssh` 記錄每次呼叫的 argv、環境與 stdin，在假的遠端以 `sh -c` 執行；遠端的 agent 是 shell 的替身）：
  - `only_the_agent_the_hardware_id_and_the_handle_cross_to_the_gateway`：`--upload-agent` 時，檢查與 `provision` 的 stdin 是 agent，第三次遠端呼叫是安裝：argv 是與前兩次相同的選項（含 `HostKeyAlias=trustiot-tg-0001` 與工具自己的 `known_hosts`）、目的地與安裝的固定腳本。這三次遠端呼叫（檢查、`provision`、安裝）之前各有一次同樣選項的 `ssh -G`，所以 `ssh -G` 也是三次（假的 ssh 不把 `ssh -G` 算進遠端呼叫）。安裝的 stdin 剛好是紀錄裡的閘道器憑證、簽發 CA、根三張 PEM（逐一比對 DER，區塊之間也沒有其他文字），由閘道器上已安裝的 agent 收到；環境只有保留的變數，cookie、驗證碼、Access 標頭、工作站私鑰與憑證、簽章與 nonce 都沒有出現在任何一次呼叫的 argv、環境或 stdin（驗收條件 5）。
  - `the_installed_agent_runs_without_an_upload_and_sudo_never_asks`：`--remote-sudo` 時安裝也以 `sudo -n trustiot-agent provision install --hardware-id tg-0002` 執行，stdin 同上。
  - `ssh_failures_are_tried_again_and_every_run_reports_the_same`：安裝的回答遺失（ssh 255）時重試，兩次的 argv 與 stdin 相同，結束碼 0。
  - `an_ssh_configuration_that_skips_the_host_key_check_is_refused`（Laysi/trustiot-v2#616 的測試）：設定是 `StrictHostKeyChecking yes` 時，檢查、`provision` 與安裝三次呼叫都照設定、沒有加 `accept-new`。Laysi/trustiot-v2#616 另外兩個先登錄成功的測試（`a_known_hardware_id_on_another_tpm_stops_with_27_even_with_allow_new_key`、`a_known_tpm_with_another_key_stops_with_20`）先在假的遠端裝上 agent，第一次登錄才會以 0 結束。
  - `the_installation_after_the_registration_stops_with_its_own_exit_codes`：閘道器回 8 是 32、9 是 33、2 是 33（訊息說 agent 太舊）；沒有安裝 agent、以 `--upload-agent` 登錄時安裝得到 127，是 33（訊息說沒有 agent），上傳的 agent 只做了檢查、沒有收到憑證；`--remote-sudo` 而閘道器的 sudo 沒有安裝的規則（`sudo -n` 與 `sudo -n -l` 都拒絕）是 33，訊息指向 sudoers 與 Laysi/trustiot-v2#219、不說重跑是安全的，`stop` 是 `sudo_refused`；sudo 有規則而 agent 自己回 1 是 34，訊息不提 sudoers；ssh 每次都連不上是 34；閘道器回報 `renewed` 是 0，印出「kept the certificate its agent renewed」、不叫人刪檔案，日誌是 `installed`、`kept_renewed` 為 true；每一種都先印出登錄的那一行、訊息說明登錄已記下，紀錄維持 `registered`，日誌最後一行是 `install_stopped` 與對應的 `stop`；最後重跑得到 200、裝上，日誌是 `installed`。平台只登錄了一次。
- `factory/workstation/tests/pipeline_swtpm.rs`（swtpm）：遠端的 agent 是 bash 的轉接，把參數與 stdin 交給測試，由測試以 `agent::provision::run` 與 `agent::provision::install::run` 執行真的 `trustiot-agent provision` 與 `provision install`（設定檔與目錄在測試的目錄裡、擁有者是測試的使用者）。第一次登錄（201）時閘道器沒有安裝 agent：安裝停在 33，沒有任何檔案；以已安裝的 agent 重跑（200、同一個序號）：真的 `provision install` 以 swtpm 的金鑰檢查並裝上憑證鏈與根憑證，`broker.ca` 是登錄的根、`identity.certificate` 是憑證加簽發 CA，`agent::setup::Setup::new_with` 以 TPM 金鑰接受它們；模擬重刷（遠端檔案與裝好的憑證都刪掉、新的 TPM 連線、映像帶回 agent，並以 `--upload-agent` 佈建）：200、同一個序號、同樣的兩個檔案再裝一次，`Setup` 接受；之後的 `tpm2_clear` 與 `--allow-new-key` 和 Laysi/trustiot-v2#616 相同，沒有到安裝那一步。

## 寫作期間的審查閘門

commit 前的 Codex 審查擋下三處，都已修正並由上面的測試覆蓋：

| 項目 | 處理 |
| --- | --- |
| agent：`identity.certificate` 與 `broker.ca` 只比對字串，經符號連結或 `..` 指向同一個檔案時，第二次 rename 會蓋掉第一個 | 先解開兩個檔案所在的目錄再比對；後來改成以開好的目錄的 dev/inode 比對（安全審查 M1）；`broker.crl` 與任一個相同也拒絕（結束碼 9） |
| 工作站：`root_sha256` 與 `root_certificate` 在同一筆紀錄，改得了紀錄的人可以兩個一起換 | `install` 多了 `pinned_root_sha256` 參數，是操作員這次執行給的 `--root-sha256`，不從紀錄讀；根憑證與紀錄的 `root_sha256` 都要等於它 |
| agent（M1 的修正）：`broker.crl` 所在的目錄沒有檢查，類型檢查與開檔之間可能被換成裝置檔 | `broker.crl` 的目錄要屬於 root 或 agent 帳號、別人不能寫，否則結束碼 9 |

併入 Laysi/trustiot-v2#83 之後發現：重送登錄時平台回的永遠是登錄當時的憑證，已經換發過的閘道器重跑安裝會換回較舊、可能已撤銷或過期的憑證。agent 端因此加上第 5 項檢查（同一把金鑰、notBefore 較晚的憑證不被取代），測試是 `a_renewed_certificate_is_never_replaced_by_an_older_one`。起初這種情況是結束碼 8，第二輪安全審查（L4）改成保留較新的憑證、照常裝根、結束碼 0 並回報 `renewed`。

## 安全審查

head `5c8fe456` 的安全審查沒有 Critical 或 High。以下每項一個 commit，都經 Codex 審查閘門：

| 項目 | 處理 | commit |
| --- | --- | --- |
| M1：以 root 執行卻接受任意 `--config`，設定決定寫入位置 | 拿掉 `--config`（結束碼 2），設定固定是 `/etc/trustiot/agent.toml`，檔案要屬於 root、別人不能寫；`broker.ca` 的目錄要屬於 root、`identity.certificate` 的目錄要屬於 `trustiot-agent`、`broker.crl` 的目錄要屬於其中之一，別人都不能寫。所有路徑從 `/` 以 `O_DIRECTORY|O_NOFOLLOW` 逐層開啟，之後的讀、建立、rename、刪除與 fsync 都在同一個目錄 fd 上 | `e8ef0e5c` |
| L1：防倒退檢查與 rename 之間，換發可能剛寫入新憑證而被蓋掉 | install 從檢查前到 rename 完成持有 `identity.certificate` 目錄的 `flock`；換發寫檔也取同一把鎖 | `a1f3edb7` |
| L2：換根之後，舊根之下較新的憑證會卡住重新安裝 | 現有的鏈要在要安裝的根（與根 CRL）之下通過用戶端憑證檢查才算較新 | `b44bde19` |
| L3：讀 `broker.crl` 會跟隨連結、可能卡在 FIFO | 先確認是一般檔案，以 `O_NOFOLLOW|O_NONBLOCK` 開、再確認一次；不是一般檔案時結束碼 9 | `0c3f50c9` |
| I3：用戶端憑證檢查沒有帶根 CRL | 以根 CRL 檢查，根撤銷的簽發 CA 被拒絕（結束碼 8） | `cc60a3f1` |
| I5：工作站顯示閘道器的輸出時沒有濾掉 Unicode 格式字元 | 控制字元與 Cf 類字元都換成空白 | `14709937` |

仍有的風險：上層目錄（例如 `/etc`、`/var/lib`）的擁有者不檢查，靠作業系統的預設；`identity.certificate` 的目錄屬於 agent 帳號，agent 被攻破時可以在裡面放檔案，但 install 只在開好的目錄裡建立新檔與 rename，讀的時候不跟隨連結、也不開裝置檔。閘道器那一側把 ssh 限定成 `provision` 子命令的轉接程式與 sudoers 是 Laysi/trustiot-v2#219（出廠映像的前提）。接上 ssh 的安裝步驟（`c0e0d166` 與 `7c5ca0fb` 的調整）是在 `5c8fe456` 的安全審查之後才寫的，由下面的第二輪安全審查涵蓋。安裝那一次的 `ssh -G` 被拒絕（結束碼 34、登錄已記下）沒有專門的測試：假的 ssh 每次 `ssh -G` 都回同樣的設定，做不出「前兩次通過、安裝那次被拒」；這條路徑與 `provision` 共用 `checked_options`（Laysi/trustiot-v2#616 的測試涵蓋拒絕的條件），安裝這邊對應到 `Error::Failed`（之後轉成結束碼 34）的那一行只經過編譯與閱讀。

## 安全審查（第二輪）

head `4e27e69e` 對接上 ssh 的安裝步驟做的安全審查沒有 Critical、High 或 Medium。以下每項一個 commit；有改程式的都加了回歸測試。I2、I4、I5 不在本 issue 處理，I3 在 Laysi/trustiot-v2#219 處理。

| 項目 | 處理 | commit |
| --- | --- | --- |
| L4：閘道器已經有 agent 換發過的較新憑證時，安裝以結束碼 8 拒絕並叫人先刪掉那個檔案，照做會丟掉較新的憑證 | agent 保留那張憑證、照常裝根、結束碼 0，那一行回報 `renewed`；工作站當作完成，日誌記 `kept_renewed`，印出閘道器保留了換發過的憑證。兩份 README 都拿掉刪檔的建議 | `029d3e3f` |
| L3：登錄後安裝的其他失敗是一般的結束碼 1，sudo 拒絕時只叫人重跑 | 其他失敗一律是新的結束碼 34（`failed`，原本沒有用到）。sudo 拒絕不從 stderr 的文字猜：`--remote-sudo` 的安裝腳本在 `sudo -n` 回 1 時以 `sudo -n -l` 問同一個指令能不能免密碼執行，不能時以 92 結束，工具對應成結束碼 33（`sudo_refused`），訊息指向 sudoers 規則與 Laysi/trustiot-v2#219，說修好之前重跑沒有用。sudoers 的 `listpw` 設成 `always` 時會把 agent 自己的 1 當成 sudo 拒絕，README 寫明；預設的 `any` 沒有這個問題 | `4ed4170a` |
| I1：閘道器回 0 時不看輸出就當成裝好 | 狀態 0 時 stdout 要剛好是 agent 的兩行報告（絕對路徑、`: `、`written`／`unchanged`，第一行還可以是 `renewed`，UTF-8、沒有控制或格式字元、以換行結束），否則結束碼 34，不記 `installed` | `66a4642a` |
| L2：閘道器回報成功、但日誌那一行寫不進去時，訊息說憑證沒有裝上 | 新的停止種類 `NotLogged`，結束碼 35（原本沒有用到），訊息說閘道器已經裝好、附上報告，修好日誌後重跑會補上那一行；登錄後的包裝只在其他停止說「沒有裝上」 | `34b18659` |
| L1：`verify` 的結束碼 25 叫人不要裝憑證，但 `register --gateway` 在 `verify` 之前就裝好了 | 25 的處理改成隔離閘道器（不出貨、不上線），帶 `registrations.jsonl`、`registrations/<硬體 ID>.json` 與印出的事件回報平台管理員，由平台決定撤銷哪一張；「不要裝」只留給沒有 `--gateway` 的檔案登錄。README 開頭說明 `verify` 是事後查證，不是安裝前的關卡（只改文件） | `9b9f4b06` |

仍有的風險：結束碼 25 時工具的訊息還是只說回報平台管理員，隔離的指示只在 README；sudo 拒絕的判斷依賴 sudoers 的 `listpw` 是預設值。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 錯的憑證都被拒絕且沒有寫任何檔案（leaf 公鑰不是 TPM 的、CN 不是硬體 ID、鏈不通到給的根、根不是自簽 CA、順序顛倒或缺簽發 CA）（swtpm） | 通過（`wrong_certificates_are_refused_and_nothing_is_written`） |
| 重跑內容相同時不寫任何檔案（inode 與內容不變）；內容不同時原子更新 | 通過（`the_installed_files_pass_the_agents_checks_a_rerun_writes_nothing_and_a_new_root_replaces_them`） |
| 擁有者與權限符合規則，中斷不留下半個檔案 | 通過（同上的權限與擁有者；`owners_are_set_per_file_...` 與單元測試 `a_write_that_fails_half_way_...`、`a_staged_file_is_removed_...`）。根憑證屬於 root 的寫入成功路徑要以 root 執行測試才驗得到，這次以一般使用者執行，驗的是拒絕與失敗時的清理 |
| agent::setup 接受裝好的檔案（swtpm 金鑰） | 通過（`Setup::new_with`，同上；工作站端的 `pipeline_swtpm` 也以登錄得到的憑證驗過） |
| 工作站端：登錄後自動執行遠端安裝；假的 ssh 程式看到的 stdin 只有憑證鏈與根憑證 PEM | 通過（`tests/ssh.rs` 的 `only_the_agent_the_hardware_id_and_the_handle_cross_to_the_gateway` 與 `the_installed_agent_runs_without_an_upload_and_sudo_never_asks`；`pipeline_swtpm` 以真的 `provision install` 裝上） |
| 紀錄裡 root_certificate 與 root_sha256 不符時，不執行遠端安裝、固定結束碼 | 通過（`a_root_that_is_not_the_pinned_one_is_never_sent`，結束碼 30）；另外也以操作員當次給的指紋比對 |
| `CI=true cargo test --locked -p agent`、`cargo test -p workstation`、`cargo build --locked --target aarch64-unknown-linux-gnu -p agent`、`cargo clippy --workspace --all-targets -- -D warnings` | 通過（見「結果」最後一列，`6e1ce8db`） |
| 在真的閘道器上安裝 | 待測：不在本 issue 的範圍（Laysi/trustiot-v2#618），目前不動 lab 的閘道器 |
