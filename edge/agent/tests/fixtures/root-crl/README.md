# 根 CRL 的測試資料

`tests/root_crl_fixture.rs` 用的公開資料，由離線儀式的工具 `certs-ca`（[services/certs](../../../../../services/certs/README.md)「根 CRL」）在 2026-10-31 產生，確認 agent 的 `RootCrl::load` 接受這個工具簽的根 CRL（Laysi/trustiot-v2#829）。

| 檔案 | 內容 |
| --- | --- |
| `root.pem` | `certs-ca root create` 的根憑證，當作 `broker.ca` |
| `issuing-ca-generation-1.pem` | `certs-ca root sign` 以這個根簽的簽發 CA 憑證 |
| `root-crl.pem` | 第二張根 CRL（編號 2），以 `--revoke issuing-ca-generation-1.pem --reason ca-compromise` 撤銷上面那一代 |

私鑰都沒有放進 repo：根私鑰與簽發 CA 的私鑰只在產生時的暫時目錄裡，產生完就刪掉。所以這些檔案不能再加簽，要換就照下面的步驟全部重做。

`root-crl.pem` 的 nextUpdate 是產生後 400 天，之後它就過期了。agent 刻意照常載入過期的根 CRL（「根 CRL」的「過期」）並標成過期；測試以 thisUpdate 後一天與 nextUpdate 後一秒兩個固定時間載入，不看今天的日期，所以不必定期重做。

重做（在 repo 根目錄；需要 `openssl` 產生 CSR）：

```sh
cargo build --locked -p certs --bin certs-ca
CA="${CARGO_TARGET_DIR:-$PWD/target}/debug/certs-ca"
work="$(mktemp -d)" && cd "$work"
"$CA" root create --common-name "TrustIoT agent test root" --certificate root.pem > root-key.pem
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout issuing-key.pem \
  -subj "/CN=request" -out g1.csr.pem
"$CA" root sign --root-certificate root.pem --request g1.csr.pem \
  --common-name "TrustIoT agent test issuing CA 1" --certificate issuing-ca-generation-1.pem < root-key.pem
"$CA" root crl --root-certificate root.pem --previous-crl none --previous-certificates none \
  --crl root-crl-1.pem < root-key.pem
"$CA" root crl --root-certificate root.pem --previous-crl root-crl-1.pem \
  --previous-certificates none --revoke issuing-ca-generation-1.pem --reason ca-compromise \
  --crl root-crl-2.pem < root-key.pem
rm -f root-key.pem issuing-key.pem
```

再把 `root.pem`、`issuing-ca-generation-1.pem` 與 `root-crl-2.pem`（改名為 `root-crl.pem`）複製到這個目錄。這裡的私鑰是測試用、用完即丟，所以寫進暫時目錄的檔案；正式的根私鑰只經過 age 的管線。
