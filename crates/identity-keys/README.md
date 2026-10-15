# identity-keys

身分服務簽內部身分 token 的金鑰（決策 27），在叢集內產生、公開與輪替（Laysi/trustiot-v2#404）。設計、比較過的做法與每個失敗情況在 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「內部身分簽章金鑰」，操作在 [docs/runbooks/secrets.md](../../docs/runbooks/secrets.md)。這個 crate 只有資料格式與規則，不碰 Kubernetes 也不做 I/O；三個使用者：

| 誰 | 用什麼 |
| --- | --- |
| 身分服務 pod 的 init container（`tools/migrate` 的 `migrate identity-signing-keys`，`src/signing_keys.rs`） | `SigningKeys::plan` 與 `publish`：每次執行決定要匯入、撤銷、新增或移除哪幾把，經 Kubernetes API 寫進 Secret 與 ConfigMap |
| 身分服務（`services/identity/src/issuer.rs`） | `SigningKeys::parse` 讀掛載的 Secret，`signing_key` 選出這一刻簽章的金鑰 |
| 本機開發環境（`scripts/lib/identity.ts`） | 不連結這個 crate，以 TypeScript 寫出同樣格式的兩個檔案；`scripts/tests/identity.test.ts` 檢查欄位名稱與這裡一致 |

## 格式

- **簽章金鑰檔** `signing-keys.json`（`SIGNING_KEYS_FILE`，Secret `<release>-identity-signing-keys` 的 key，也是身分服務掛載的檔名）：`{"keys":[{"kid","generation","x","d","published_at","signs_from"}, …]}`。`x`、`d` 是 Ed25519 公鑰與私鑰的 base64url（不補 `=`），`published_at`、`signs_from` 是 Unix 秒，還沒公開的金鑰兩者都是 `null`。同一組金鑰寫出的文字永遠相同（`to_json`），所以 init container 能以比較文字判斷 Secret 有沒有變。
- **公鑰檔** `jwks.json`（`JWKS_FILE`，ConfigMap `<release>-internal-identity-keys` 的 key，也是每個服務掛載的檔名）：RFC 7517 的 JWK Set，每把只有 `kty`（`OKP`）、`crv`（`Ed25519`）、`kid`、`x`、`alg`（`EdDSA`）、`use`（`sig`）。由簽章金鑰檔推出（`jwks`），列出簽章金鑰檔裡的每一把，不多也不少；`crates/service-kit` 以它驗證 token。
- **kid**：公鑰的 RFC 7638 thumbprint（SHA-256，base64url，43 個字元）。從人建立的私鑰匯入的金鑰保留原本的 kid。
- **代**（generation）：chart 的 `internalIdentity.keyGeneration`，十進位數字，最大 `MAX_GENERATION`；匯入的金鑰是代 0。
- 上限 8 把（`crates/service-kit` 接受的上限）。

## 規則

- **新增**：要求的代大於 Secret 裡每一把的代時，以作業系統的 CSPRNG 產生一把新金鑰。init container 先把它寫進 Secret（沒有時間，誰都不會用它簽章），再把公鑰寫進 ConfigMap，成功後才寫下 `published_at`（這一刻）與 `signs_from`（`published_at` 加 `keyPropagationSeconds`；沒有其他還能簽章的金鑰時就是 `published_at`）。
- **簽章**（`signing_key`）：`signs_from` 已到、代不小於 `revokeBeforeGeneration`、身分服務自己看到的公鑰檔以同一個 kid 列出同一把公鑰的金鑰中，`signs_from` 最晚的一把。
- **移除舊金鑰**：比正在簽章的金鑰舊的每一把，在新金鑰的 `signs_from` 之後再過 `keyPropagationSeconds` 加 305 秒（`TOKEN_EXPIRY_SECS`：驗證端接受的最長效期 300 秒加 5 秒時鐘誤差）的第一次執行時移除。
- **撤銷**：代比 `revokeBeforeGeneration` 小的金鑰立即移除；沒有其他還能簽章的金鑰，所以新金鑰立即開始簽章。
- **只往上走**：要求的代不大於現有的代時不產生金鑰（rollback 不會再輪替一次）；新金鑰還在等 `signs_from` 時又要求更大的代，它沒簽過任何 token，直接換成新的一把。
- **匯入**（從人建立的私鑰搬過來）：Secret 還沒有金鑰時，以 `internalIdentity.publicKeys` 裡對得上的 kid、代 0 匯入，立即簽章；設定期間不產生新金鑰。

`Debug`、錯誤與 `Change`（init container 記在日誌的變更）只寫 kid、代與時間，不寫金鑰；私鑰在記憶體裡以 `zeroize` 清除。

## 測試

`cargo test -p identity-keys`：格式的往返與固定文字、kid 是 thumbprint、拒絕不合規的文件（重複的 kid、超過 8 把、不是 Ed25519 的金鑰、私鑰與公鑰不符）、每一條規則（新增、簽章時間、撤銷、移除、rollback、匯入）以假的時鐘與固定的金鑰驗證，以及 `Debug` 與錯誤不含私鑰。init container 對假的 Kubernetes API 的行為在 `tools/migrate/tests/signing_keys.rs`，身分服務的選擇與重讀在 `services/identity/tests/`。
