# 根 CRL：chart 帶入、certs 驗章採用與到期告警

Laysi/trustiot-v2#830（Laysi/trustiot-v2#560 的子項）。

## 環境

- 建置主機（x86_64 Linux）：整個 workspace 的 clippy 與測試、agent 測試、`cargo deny`、`bun test deploy/chart`、`bun test scripts`。跑的是安全審查前的 commit（`683a484f`）。
- 建置主機（aarch64 Linux VM）：安全審查修正之後的 certs 與 root-crl 測試、clippy、agent 以 aarch64 原生建置、`bun test deploy/chart`。
- 兩台都設 `CI=1`：需要容器的測試（certs 的服務測試用拋棄式 Postgres）沒有容器時失敗，不會略過。
- 工具版本依 `.prototools` 與 `rust-toolchain.toml`。
- 分支 `feat/830-root-crl-load-alert`，以 `origin/main` 的 `1da444a0` 為基底。
- 測試用的根與根 CRL 都在測試執行時以 rcgen 產生，沒有固定的金鑰檔。
- 另以 OpenSSL 簽一張 P-384 的真實根 CRL，在本機跑一次 `helm template`，確認 chart 的 DER 開頭檢查接受真實的 CRL。這張 CRL 沒有進版控。

## 指令

```sh
# 建置主機（x86_64）
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --exclude agent
cargo test --locked -p agent
cargo deny --locked check
bun test deploy/chart
bun install --cwd scripts --frozen-lockfile && bun test scripts

# 建置主機（aarch64），安全審查修正之後
cargo clippy --locked -p certs -p root-crl -p agent --all-targets -- -D warnings
CI=1 cargo test --locked -p certs -p root-crl
CI=1 cargo test --locked -p certs --test root_crl -- --nocapture --test-threads 1
cargo build --locked -p agent -p root-crl
bun test deploy/chart

# 真實的 CRL 渲染一次（rootCrl 的值是 OpenSSL 簽的 CRL 的 base64 DER）
helm template t deploy/chart -f deploy/chart/values-saas.yaml -f deploy/chart/tests/values/saas-instance.yaml \
  --set services.certs.enabled=true --set certificateAuthority.rootCrl=<base64 DER>
```

## 結果

| 項目 | 結果 |
| --- | --- |
| x86_64：fmt、workspace clippy | 通過 |
| x86_64：`cargo test --workspace --exclude agent` | 全部通過（[workspace-x86_64.txt](data/2026-10-31-830-root-crl-load-alert/workspace-x86_64.txt)） |
| x86_64：`cargo test -p agent` | 全部通過，lib 360 passed（2 ignored），和改動前的數量相同 |
| `cargo deny --locked check` | advisories、bans、licenses、sources 都是 ok |
| `bun test deploy/chart` | 712 pass、0 fail（x86_64 與 aarch64 各一次） |
| `bun test scripts` | 1732 pass、0 fail（有 9 個略過） |
| aarch64：certs 與 root-crl 的測試 | 全部通過，其中 `tests/root_crl.rs` 5 passed（[aarch64.txt](data/2026-10-31-830-root-crl-load-alert/aarch64.txt)） |
| aarch64：clippy | 第一次失敗：clippy 擋下 `bounded` 以位元組切字串（`string_slice`）。改成以字元截斷後，第二次通過 |
| aarch64：agent 建置 | `trustiot-agent` 是 `ELF 64-bit LSB pie executable, ARM aarch64` |
| 真實 CRL 的 `helm template` | 渲染成功。ConfigMap 的 `root-crl.pem` 是 64 字元一行的 PEM `X509 CRL`；certs 的環境變數有 `TRUSTIOT__CA__ROOT_CRL_FILE=/etc/trustiot/certs-ca/root-crl.pem` |

`tests/root_crl.rs` 的輸出在 [certs-root-crl-tests.txt](data/2026-10-31-830-root-crl-load-alert/certs-root-crl-tests.txt)。

拒絕的案例都記了 ERROR，也都保留了之前採用的根 CRL：

- 別的根簽的；
- 用同名但不同金鑰簽的；
- 帶 issuing distribution point；
- 兩張 CRL；
- 一張憑證；
- 超過 64 KiB；
- 60 000 位元組的 PEM 標頭行，problem 被截到 1024 字元；
- 編號比較小；
- 編號相同但內容不同；
- 採用過之後檔案變成空的。

PEM 太大的案例：

- 2297 筆撤銷的 CRL，寫成一行 base64 時是 64 537 位元組，在 agent 的上限內；重新編成每行 64 字元交出去時是 65 544 位元組，超過上限，所以被拒絕。
- 少一筆（2296 筆）時是 65 516 位元組，被採用。

服務測試用真的 certs 執行檔、拋棄式 Postgres 與假的 OTLP collector：

- 啟動時採用 1 號（剩 30 天），`certs.root_crl.expiring` 是 1，`remaining` 約 2 592 000 秒，並記 WARN。
- 改檔之後，不重啟就採用了 2 號，`expiring` 變回 0。
- 換成別的根簽的 3 號：記 ERROR（`kept_number` 是 "2"），`certs.root_crl.invalid` 是 1。
- 換成已過期的 4 號：照樣採用，`remaining` 是負數，`expiring` 是 1，`invalid` 回到 0，並記 WARN（`expired: true`）。

## 結論

| 驗收條件 | 結果 |
| --- | --- |
| 1. chart 從 values 帶入根 CRL、寫進 ConfigMap；沒設時是空值；不是 base64 PEM `X509 CRL` 的內容時渲染失敗（`bun test deploy/chart`） | 通過。新增的 chart 測試確認：寫入的 PEM 與值相同、沒設時是空字串；憑證、PKCS#8、v1 CRL、長度不對、超過 32768 字元、一般文字、PEM 文字與非字串的值，都會讓渲染失敗，不論有沒有 schema 驗證；certs 關閉時也一樣檢查。真實的 OpenSSL CRL 能渲染 |
| 2. 以根憑證驗章並檢查格式才採用；別的根簽的或格式不符時記 ERROR、不採用（測試） | 通過（`a_root_crl_is_adopted_only_when_the_root_signed_it_in_the_agents_format`，以及服務測試）。檢查程式是 agent 共用的 `crates/root-crl` |
| 3. 剩不到 60 天時記 WARN、告警指標是 1；過期後仍採用、仍告警（固定時鐘的測試） | 通過（`the_alert_holds_below_60_days_and_after_expiry_while_the_crl_stays_adopted` 以固定時鐘測了 61、60、60 天減 1 秒、59、0 與 -30 天；服務測試以 OTLP 讀到指標） |
| 4. ConfigMap 更新之後不重啟就用新的根 CRL（測試） | 通過（`a_new_root_crl_file_is_adopted_without_a_restart`；服務測試以 rename 模擬 kubelet 的更新） |
| 5. 文件更新；`CI=1 cargo test -p certs` 與 `bun test deploy/chart` 通過；有測試報告 | 通過（本報告） |

待測，延後為進階驗證：

- 在真的叢集上，以 kubelet 的 ConfigMap 更新換根 CRL。
- Grafana 告警規則的實測。
- 跨平台的重複驗證。
- 目前 rcgen 簽不出沒有 CRL 編號或沒有 nextUpdate 的 CRL，所以 certs 拒絕這兩種的規則還沒有測試。
