# certs-ca 簽的根 CRL：以 OpenSSL 獨立檢查（2026-10-31）

Laysi/trustiot-v2#843（延後自 Laysi/trustiot-v2#829）：`certs-ca root crl` 簽的根 CRL，基本驗證用的都是 Rust 的同一組函式庫（webpki、x509-parser）。這份報告以另一個實作 OpenSSL 檢查兩張 CRL 的簽章、欄位與撤銷判定。只是證據，沒有改任何產品程式。

## 目的

1. OpenSSL 以根憑證驗過 `certs-ca` 簽的兩張根 CRL，欄位與 `services/certs/README.md`「根 CRL」的「格式」相同。
2. `openssl verify -crl_check` 加上這張 CRL，撤銷的那一代簽發 CA 憑證被判為 revoked、另一代通過。
3. 寫明 OpenSSL 的版本與來源。

## 環境

- **程式碼**：分支 `docs/843-openssl-root-crl`，基於 main `61dd8437`；`certs-ca` 以 `cargo build --locked -p certs --bin certs-ca` 從這個樹建置。
- **機器**：建置主機（x86_64 Linux）。
- **OpenSSL**（不在 `.prototools`，也沒有檢查碼可驗的 release 執行檔，所以用容器映像）：
  - 映像：`docker.io/library/debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad`（與 `crates/testkit` 的 swtpm 測試映像同一個 digest），每次執行以 `--rm` 啟動。
  - 套件：容器內 `apt-get install openssl=3.5.7-1~deb13u3`（版本釘死；映像本身不含 `openssl` 執行檔）。
  - 版本：`OpenSSL 3.5.7 9 Jun 2026`（Debian 的 build，`platform: debian-amd64`），`/usr/bin/openssl` 的 SHA-256 是 `acc10176d038aeb7b2a77d9ec972ac2985857a7191f7aecb7c54ef0e2c7e9549`。完整輸出：[openssl-version.txt](data/2026-10-31-843-openssl-root-crl/openssl-version.txt)。
  - CSR 也由同一個容器的 `openssl req` 產生（P-256），不影響被檢查的內容。
- **兩張被檢查的 CRL**：
  1. 測試資料 `edge/agent/tests/fixtures/root-crl/root-crl.pem`（編號 2，撤銷 `issuing-ca-generation-1.pem`，`ca-compromise`）。私鑰已不存在，原樣使用。
  2. 新做一次拋棄式儀式：建根、簽 3 代簽發 CA；CRL 1 是空的；CRL 2 撤銷第 1 代（`ca-compromise`）；CRL 3 以 CRL 2 為 `--previous-crl`、`revoked-issuing-cas.pem`（第 1 代）為 `--previous-certificates`，另撤銷第 2 代（`superseded`），所以帶 2 項，其中第 1 代是沿用的。
- **私鑰**：根私鑰與三把簽發 CA 私鑰只在暫時目錄，腳本結束時刪除；沒有進 repo 或資料目錄。資料目錄只有憑證、CRL 與 `certs-ca` 的 stderr（路徑改寫為 `<host copy>/`）。

## 指令

在 repo 根目錄、有 podman 與 `.prototools` 釘選之 Rust 的 x86_64 Linux 主機上：

```sh
bash docs/test-reports/data/2026-10-31-843-openssl-root-crl/run.sh <new or empty output directory>
```

[run.sh](data/2026-10-31-843-openssl-root-crl/run.sh) 建置 `certs-ca`、在容器裝釘死版本的 OpenSSL、做上述儀式，再由 [check-inside.sh](data/2026-10-31-843-openssl-root-crl/check-inside.sh) 在容器內對每張 CRL 執行：

```sh
openssl crl -in <crl> -CAfile <root.pem> -verify -noout
openssl crl -in <crl> -text -noout
openssl verify -CAfile <root.pem> <issuing-ca-generation-N.pem>                         # 對照組，不查 CRL
openssl verify -crl_check -CAfile <root.pem> -CRLfile <crl> <issuing-ca-generation-N.pem>
```

這份報告的檔案來自第一次執行：腳本當時直接寫進資料目錄，由工作把釘死的套件版本代入（事先查過 `apt-cache policy` 的 Candidate 為 `3.5.7-1~deb13u3`），輸出取回後，只把 stderr 裡的建置主機路徑改寫成 `<host copy>/`。之後 `run.sh` 改成寫進呼叫者給的空目錄（每次重跑會做新的憑證，序號與指紋和這份報告不同，所以不覆蓋存下的證據），並加了逐項斷言（CRL 驗不過、對照組失敗、撤銷與否和預期不同都會讓它失敗）與路徑改寫。以最終版本在建置主機再完整跑過一次（新的儀式）：結束碼 0，4 張 CRL 都 `verify OK`，撤銷判定與上表相同（CRL 3：第 1、2 代 revoked、第 3 代 OK），輸出沒有主機路徑或私鑰。

## 結果

### 驗章

| CRL | `openssl crl -verify` |
| --- | --- |
| 測試資料 `root-crl.pem`（編號 2） | `verify OK` |
| 新做的 CRL 1（空） | `verify OK` |
| 新做的 CRL 2（1 項） | `verify OK` |
| 新做的 CRL 3（2 項） | `verify OK` |

### 欄位對照（`-text`）

| 欄位 | 預期（`certs-ca` / README「格式」） | 測試資料 | 新 CRL 1 | 新 CRL 2 | 新 CRL 3 |
| --- | --- | --- | --- | --- | --- |
| Version | 2 | 2 (0x1) | 2 | 2 | 2 |
| 簽章演算法 | ecdsa-with-SHA384 | 相同 | 相同 | 相同 | 相同 |
| Issuer | 與根的 subject 相同 | `CN=TrustIoT agent test root` | `CN=TrustIoT CRL check test root` | 同左 | 同左 |
| AKI | 等於根的 SKI | `86:18:AE:48:…:80:86`，同根的 SKI | `B2:4D:1F:F4:…:96:E0`，同根的 SKI | 同左 | 同左 |
| CRL 編號 | 有，遞增 | 2 | 1 | 2 | 3 |
| 其他延伸欄位（含 issuing distribution point、delta CRL indicator、critical 延伸） | 沒有 | 只有 AKI 與 CRL 編號 | 同左 | 同左 | 同左 |
| thisUpdate | 簽發時往前 1 小時 | 2026-10-30 22:51:38Z | 2026-10-31 11:16:53Z（與 `certs-ca` 印的相同） | 同左 | 同左 |
| nextUpdate | thisUpdate 後 400 天 | 2027-12-04 22:51:38Z（+400 天） | 2027-12-05 11:16:53Z（+400 天） | 同左 | 同左 |
| 項目數 | 見下 | 1 | 0 | 1 | 2 |

項目逐項（序號取自 OpenSSL 的 `-text`；新 CRL 的預期取自 `certs-ca` 的 stderr，序號以小寫十六進位印出，OpenSSL 印大寫，其餘相同；測試資料的私鑰已不在，沒有當時的 stderr，序號改對 `issuing-ca-generation-1.pem` 的 `openssl x509 -serial`，原因取自 [fixture README](../../edge/agent/tests/fixtures/root-crl/README.md)）：

| CRL | 序號 | 撤銷時間 | reasonCode | 預期 | 相同 |
| --- | --- | --- | --- | --- | --- |
| 測試資料 | `547B246647389F1B69EEB659732B9B11` | 2026-10-30 22:51:38Z（等於 thisUpdate） | CA Compromise | 第 1 代的序號，`ca-compromise` | 是 |
| 新 CRL 2 | `5864825E95DEC4F5C87AA5BE7A0C89A2` | 2026-10-31 11:16:53Z | CA Compromise | stderr：`revoked now, ca-compromise` | 是 |
| 新 CRL 3，第 1 項 | `5864825E95DEC4F5C87AA5BE7A0C89A2` | 2026-10-31 11:16:53Z（沿用，與 CRL 2 相同） | CA Compromise | stderr：`revoked 2026-10-31T11:16:53Z, ca-compromise` | 是 |
| 新 CRL 3，第 2 項 | `5488301DB0ABA93D961A5C36E416211E` | 2026-10-31 11:16:53Z | Superseded | stderr：`revoked now, superseded` | 是 |

### 撤銷判定（`openssl verify -crl_check`）

「對照組」是不帶 `-crl_check` 的 `openssl verify`，三代、四組都是 `OK`，所以 revoked 的判定來自 CRL。

| CRL | 第 1 代 | 第 2 代 | 第 3 代 |
| --- | --- | --- | --- |
| 測試資料 | `error 23 ... certificate revoked`（退出碼 2） | 無此憑證 | 無此憑證 |
| 新 CRL 1（空） | OK | OK | OK |
| 新 CRL 2 | revoked | OK | OK |
| 新 CRL 3 | revoked | revoked | OK |

### 與 `certs-ca` 的實作比對

沒有任何差異：OpenSSL 對 4 張 CRL 的簽章、每個欄位與每一項都與 `certs-ca` 印出的與 README 規定的相同，撤銷判定與預期一致。

## 結論（驗收條件對照）

- OpenSSL 以根憑證驗過 `certs-ca` 簽的兩張根 CRL（測試資料與新做的帶 2 項、含沿用項目的 CRL 3，另加 CRL 1、2），欄位與列出的相同：通過。
- `openssl verify -crl_check` 以這張 CRL 判定撤銷的那一代為 revoked、另一代通過：通過（CRL 3：第 1、2 代 revoked，第 3 代 OK）。
- 報告寫明 OpenSSL 版本與來源：通過（OpenSSL 3.5.7，Debian trixie-slim 映像以 digest 釘選，套件 `openssl 3.5.7-1~deb13u3`）。

限制：OpenSSL 只在這台 x86_64 主機的這個映像上跑過一次；這是獨立實作的佐證，不取代 `crates/root-crl` 與 agent 的測試。

## 原始資料

[data/2026-10-31-843-openssl-root-crl/](data/2026-10-31-843-openssl-root-crl/)：

- `openssl-version.txt`：`openssl version -a` 與套件版本。
- `ceremony/`：根憑證、三代簽發 CA 憑證、三張 CRL（PEM）與 `certs-ca` 每一步的 stderr。
- `openssl/<fixture|ceremony-crl-1|2|3>/`：`crl-verify.txt`、`crl-text.txt`、`crl-summary.txt`、`root-text.txt` 與每代的 `verify-issuing-ca-generation-N.txt`。
- `run.sh`、`check-inside.sh`：重跑用。
