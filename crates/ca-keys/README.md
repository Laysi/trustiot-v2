# ca-keys

certs 服務的簽發 CA 金鑰（決策 6、12），在叢集內產生，每把金鑰的 CSR 交給離線根 CA 簽發（Laysi/trustiot-v2#78）。CA 階層、保管方式與操作在 [services/certs/README.md](../../services/certs/README.md)，設計摘要在 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「閘道器憑證 CA」。這個 crate 只有資料格式與規則，不碰 Kubernetes 也不做 I/O；兩個使用者：

| 誰 | 用什麼 |
| --- | --- |
| certs pod 的 init container（`tools/migrate` 的 `migrate certs-ca-keys`，`src/ca_keys.rs`） | `CaKeys::plan` 決定要新增或移除哪幾把，`to_json` 寫進 Secret，`requests` 寫進 CSR 的 ConfigMap |
| certs 服務（`services/certs/src/authority.rs`） | `CaKeys::parse` 讀掛載的 Secret；金鑰有了根簽的憑證才以 `CaKey::into_signing_key` 交給簽章的程式 |

## 格式

- **金鑰檔** `issuing-ca-keys.json`（`KEYS_FILE`，Secret `<release>-certs-ca-keys` 的 key，也是 certs 掛載的檔名）：`{"keys":[{"id","generation","created_at","private_key","request"}, …]}`，依代排序。`private_key` 是 P-256 私鑰的 PKCS#8 DER（base64），`request` 是這把金鑰的 CSR（PEM），`created_at` 是 Unix 秒。同一組金鑰寫出的文字永遠相同（`to_json`），init container 以比較文字判斷 Secret 有沒有變。
- **CSR**（ConfigMap `<release>-certs-ca-requests` 的 key `issuing-ca-generation-<代>.csr.pem`，`request_file`）：以金鑰自己簽名，主體只有 `CN=TrustIoT issuing CA key, generation <代>`，讓維運人員認得是哪一把；簽發 CA 憑證的主體與延伸欄位由離線根 CA 決定。
- **金鑰 id**（`key_id`）：公鑰 DER SubjectPublicKeyInfo 的 SHA-256，十六進位小寫 64 字元。certs 服務在日誌寫出等待憑證的金鑰 id，`certs-ca root sign` 印出它簽的金鑰 id，維運人員比對兩者。
- **代**（generation）：chart 的 `certificateAuthority.keyGeneration`，1 到 `MAX_GENERATION`（9 位數字）。
- 最多 4 把（`MAX_KEYS`）：目前簽章的、等待憑證的，與憑證還有效的舊金鑰。

`parse` 拒絕不合規的文件：多出或缺少欄位、代或時間超出範圍、不是 base64、不是 P-256、id 與公鑰不符、CSR 不是這把金鑰簽的、重複的 id 或代、超過 4 把或超過 64 KiB。

## 規則（`CaKeys::plan`）

- **移除**：代比 `removeKeysBeforeGeneration` 小的金鑰移除（0 不移除）。
- **新增**：剩下的金鑰沒有一把的代不小於要求的代時，以作業系統的 CSPRNG 產生一把那一代的 P-256 金鑰與它的 CSR。
- **只往上走**：要求的代不大於現有的代時不產生金鑰，chart 的 values rollback 不會再換一次。
- 要求的代比 `removeKeysBeforeGeneration` 小（會移除自己要求的金鑰），或結果超過 4 把時，回錯誤，不改任何東西。

`Debug`、錯誤與 `plan` 的變更紀錄只寫金鑰 id、代與時間，不寫金鑰。文件的文字、解碼的私鑰與每把金鑰的 PKCS#8 副本在 drop 時以 `zeroize` 清除；ring 自己的金鑰物件不在此列。

## 測試

`cargo test -p ca-keys`：格式的往返與固定文字、id 是公鑰的 SHA-256、CSR 以金鑰自己簽名、拒絕上面列出的每種不合規文件、每一條規則（新增、移除、只往上走、上限、移除自己要求的代），以及 `Debug` 與錯誤不含私鑰。init container 對假的 Kubernetes API 的行為在 `tools/migrate/tests/ca_keys.rs`，服務怎麼選簽章的金鑰在 `services/certs/tests/`。
