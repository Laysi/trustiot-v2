# root-crl

根 CRL（離線根 CA 列出被撤銷的簽發 CA 的 CRL，Laysi/trustiot-v2#556）在被使用前必須通過的檢查。只有一份實作，兩個使用者對「什麼樣的根 CRL 可以用」的判斷因此不會分歧（Laysi/trustiot-v2#830）：

| 誰 | 用在哪裡 |
| --- | --- |
| 閘道器 agent（`edge/agent/src/tls.rs`，`RootCrl`） | 內層 TLS 用根 CRL 檢查 broker 的憑證鏈之前；不通過就不啟動 |
| certs 服務（`services/certs/src/root_crl.rs`） | 採用 chart 帶入的根 CRL 之前；不通過就不採用、記 ERROR，因此不會把 agent 拒絕的根 CRL 交給閘道器 |

根 CRL 的格式、發佈與過期規則在 [services/certs/README.md](../../services/certs/README.md) 的「根 CRL」。

## 檢查（`check`）

依序檢查，第一個不通過的理由就是錯誤：

1. 檔案最多 64 KiB（`MAX_BYTES`）。
2. 是 PEM，而且剛好一個 `X509 CRL` 區塊。
3. 能解析成 CRL，後面沒有多餘的位元組。
4. 沒有 issuing distribution point（根 CRL 涵蓋根簽的所有憑證）。
5. 簽發者是信任錨之一（以主體比對），以閘道器 TLS 握手接受的簽章演算法（ring，`signature_algorithms`，依 AlgorithmIdentifier 的位元組精確比對）驗證簽章通過，且信任錨的 key usage 允許 cRLSign（沒有 key usage 延伸欄位視為允許）。
6. webpki 以握手用的伺服器驗證器接受它（拒絕 delta CRL 與不認得的關鍵延伸欄位）。

過期與否不在這裡判斷：過期的根 CRL 仍然通過，由使用者決定怎麼處理（agent 照用，certs 照用並告警）。通過時回傳 `CheckedCrl`：DER、CRL number（十進位，沒有則為 `None`）、thisUpdate 與 nextUpdate（Unix 秒）。certs 另外要求 CRL number 與 nextUpdate，agent 不要求。

錯誤是簡短的英文理由。憑證或 CRL 裡的名稱來自任何人都可能寫入的檔案，所以去掉控制字元並截到 200 字（`shown`）才放進訊息。

## 規則

- agent 依賴這個 crate，所以它的相依套件必須能編到 aarch64，而且唯一的密碼學函式庫是 ring（`edge/agent/tests/dependencies.rs` 會檢查）。確認方式：`cargo build --locked --target aarch64-unknown-linux-gnu -p root-crl`。
- agent 的 TLS provider 的簽章演算法取自 `signature_algorithms`，所以這裡驗過的簽章，每次握手也會接受。
- 測試：`cargo test -p root-crl`（以 rcgen 在測試時產生根與 CRL）；完整的拒絕案例在 agent 的 `cargo test -p agent --lib tls` 與 certs 的 `cargo test -p certs --test root_crl`。
