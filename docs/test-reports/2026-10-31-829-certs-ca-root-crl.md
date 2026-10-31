# certs-ca 簽根 CRL

Laysi/trustiot-v2#829

## 環境

- 建置主機（arm64 Linux 虛擬機），Rust 1.99.0（`rust-toolchain.toml`），Bun 依 `.prototools`；分支 `feat/829-certs-ca-root-crl`，commit `c8d6e296`（自 main `1da444a0`）。執行時設了 `CI=1`，需要容器的測試失敗而不是略過。
- 全部在行程內：真的 `certs-ca` 執行檔與 `ceremony::root_crl`，rustls 0.23（rustls-webpki 0.103，ring provider）當作 webpki，agent 的 `RootCrl::load`。沒有碰任何叢集或閘道器。
- 測試用的根、簽發 CA 與私鑰都在測試時產生、不寫進 repo。agent 端的測試資料 `edge/agent/tests/fixtures/root-crl/` 是 2026-10-31 在建置主機上以 `certs-ca` 產生的公開憑證與 CRL，產生它們的私鑰在同一個暫時目錄裡刪掉（步驟在那個目錄的 README）；`gitleaks dir --redact` 掃過那個目錄，沒有發現。

## 指令

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked -p certs --test ceremony_root_crl -- --nocapture
cargo test --locked -p agent --test root_crl_fixture -- --nocapture
cargo test --locked --workspace --exclude agent
cargo test --locked -p agent
bun install --cwd scripts --frozen-lockfile && bun test scripts
```

## 結果

2026-10-31 一次跑完，全部 exit 0。`services/certs/tests/ceremony_root_crl.rs`（記錄時叫 `root_crl.rs`，見下）與 agent 測試的完整輸出（每個拒絕的訊息、`certs-ca` 的 stderr）在 [data/2026-10-31-829-certs-ca-root-crl/root-crl-nocapture.txt](data/2026-10-31-829-certs-ca-root-crl/root-crl-nocapture.txt)，路徑以 `<host copy>` 代替。

| 檢查 | 結果 |
| --- | --- |
| `cargo fmt --all --check` | 通過 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 通過（先 touch 這次改的檔案，確認 certs 與 agent 重新檢查） |
| `certs --test ceremony_root_crl` | 3 passed |
| `agent --test root_crl_fixture` | 1 passed |
| `cargo test --workspace --exclude agent` | 全部 ok，0 failed |
| `cargo test -p agent` | 全部 ok，0 failed（2 ignored 是原本就有的） |
| `bun test scripts` | 1732 pass、9 skip、0 fail |

這次記錄之後，測試檔從 `tests/root_crl.rs` 改名為 `tests/ceremony_root_crl.rs`，因為 Laysi/trustiot-v2#830 在同一個目錄加了服務端的 `tests/root_crl.rs`；data 目錄裡的輸出是改名前的原樣。同一天在建置主機上以新名字重跑 `cargo fmt --all --check`、`cargo clippy --locked -p certs -p agent --all-targets -- -D warnings`、`CI=1 cargo test --locked -p certs --test ceremony_root_crl -- --nocapture`（3 passed）與 `CI=1 cargo test --locked -p agent --test root_crl_fixture`（1 passed），全部 exit 0。

`tests/ceremony_root_crl.rs` 的三個測試：

- `webpki_refuses_the_issuing_ca_a_root_crl_revokes`：第一張根 CRL 編號 1、沒有項目；第二張撤銷第 1 代（`ca-compromise`）。webpki 照閘道器 agent 的設定（`WebPkiServerVerifier`、帶根 CRL、允許狀態未知）時，第 1 代簽的 broker 伺服器憑證得到 `InvalidCertificate(Revoked)`，第 2 代的接受。同名但別的金鑰簽的 CRL 得到 `InvalidCertRevocationList(BadSignature)`，所以 webpki 確實以根的公鑰驗了 CRL。每一張都檢查 DER 欄位：version 2、`30 0a 06 08 2a 86 48 ce 3d 04 03 03`（ecdsa-with-SHA384、不帶參數）在簽章演算法兩處各出現一次、簽發者與根的主體逐位元組相同、以根的公鑰驗得過、延伸欄位恰好是 authority key identifier（等於根的 subject key identifier）與 CRL 編號且都不是 critical（所以沒有 issuing distribution point、不是 delta CRL）、nextUpdate 減 thisUpdate 等於 400 天、PEM 不超過 64 KiB。
- `entries_carry_over_until_400_days_after_their_certificates_expire`（固定時鐘）：編號 1 到 6 依序加一，上一張的項目保留原本的撤銷時間與原因；第 1 代（效期 2 天）過期後仍列著，thisUpdate 正好是 notAfter 加 400 天時還在，晚一秒拿掉；拿掉的憑證留在檔案裡之後被略過；這次才撤銷的憑證先放進 `--previous-certificates` 也接受；過期不到 400 天的憑證可以撤銷；每一項帶憑證公鑰的金鑰 id；上一張與新的 SHA-256 相符。十一種拒絕（別的根或同名別的金鑰簽的上一張、thisUpdate 晚於時鐘的上一張、缺憑證的項目、還在保留期卻沒被列出的憑證、別的根或同名別的金鑰簽的要撤銷的憑證、根自己、不是 CA、已撤銷、過期超過 400 天）、沒有憑證的要撤銷的文字（空字串與只有換行）、別的私鑰、過期的根，都拒絕並指出原因。
- `certs_ca_root_crl_reads_the_key_from_stdin_and_refuses_misuse`：執行檔從 stdin 讀私鑰簽出編號 1 與 2，stderr 印出上一張與新的 SHA-256、每一項的序號、主體、金鑰 id、原因與到期，並提醒「順序」；輸出不含私鑰。十種誤用都不寫檔、不印私鑰：別的根的上一張、別的根的要撤銷的憑證、已存在的輸出檔、終端機的 stdin（pseudo-terminal）、別的根的私鑰、沒有 `--previous-crl`、沒有 `--previous-certificates`、空的 `--revoke` 檔、`--revoke` 沒有 `--reason`、不認得的原因。

`agent --test root_crl_fixture`：agent 的 `RootCrl::load` 以 `certs-ca` 簽的測試資料當作 `broker.ca` 與根 CRL，在 thisUpdate 後一天載入為編號 2、沒過期、只列第 1 代；在 nextUpdate 後一秒照常載入並標成過期。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 簽出的 CRL 由 webpki 解析並以根驗章（單元測試） | 通過：`webpki_refuses_the_issuing_ca_a_root_crl_revokes`；agent 的 `RootCrl::load` 也接受（`root_crl_fixture`） |
| 列出指定的簽發 CA 序號；編號是上一張加一，第一張是 1；上一張的項目沿用；過期的依規則拿掉（單元測試） | 通過：`entries_carry_over_until_400_days_after_their_certificates_expire`。規則是過期滿 400 天才拿掉（安全審查後定的，見 `services/certs/README.md`「根 CRL」） |
| 拒絕：上一張不是這個根簽的、要撤銷的憑證不是這個根簽的、輸出檔已存在、私鑰來自終端機 | 通過：前兩項在兩個測試都有，後兩項在 `certs_ca_root_crl_reads_the_key_from_stdin_and_refuses_misuse` |
| DER 欄位：沒有 IDP、不是 delta、沒有其他 critical 延伸欄位、ecdsa-with-SHA384、nextUpdate 是 thisUpdate 後 400 天 | 通過：每一張簽出的 CRL 都經 `check_format` 逐項檢查 |
| README 儀式步驟、「根 CRL」、已知的限制、secrets.md 更新；已知的限制不再說 certs-ca 不能簽根 CRL | 通過（文件變更，見 PR） |
| `CI=1 cargo test -p certs` 與 workspace clippy 通過；測試報告在 docs/test-reports/ | 通過：本報告 |

待測（延後的進階驗證）：以第二個實作（例如 `openssl crl -verify`）再解析與驗章一次；在實體閘道器上以這個工具簽的根 CRL 跑一次握手。前者是同一件事的第二個來源，後者要等 certs 提供根 CRL 與閘道器取用（Laysi/trustiot-v2#830、Laysi/trustiot-v2#563）才有意義。
