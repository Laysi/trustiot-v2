# certs（憑證服務）

自建 CA 的線上簽發 CA（決策 6、12；[部署與可搬移性](../../docs/architecture/deployment.md)的「憑證與佈建」）：以離線根 CA 簽出的簽發 CA 為閘道器簽 mTLS 用戶端憑證、為產線工作站簽工作站憑證，定期重發每個簽發 CA 的 CRL，給 MQTT broker 讀。

目前是 Laysi/trustiot-v2#78 的範圍（2026-10-21）：

- 服務骨架：自己的 schema `certs`、服務角色、簽發紀錄與 CRL 兩張表（沒有租戶資料，見[資料庫](#資料庫)）。
- CA 階層與私鑰保管（見下一節），以及根 CA 的離線儀式工具 `certs-ca`。
- 從 CSR 簽閘道器憑證的程式（`src/issue.rs`）：只取 CSR 的公鑰，CN、EKU 與效期由 CA 決定。還沒有對外的簽發 API：出廠登錄（Laysi/trustiot-v2#79）與換發端點（Laysi/trustiot-v2#81）呼叫它，撤銷（Laysi/trustiot-v2#82）寫簽發紀錄的撤銷欄位。
- CRL 依排程重發、到期前告警，以及給 broker 的 `crl.pem` 與 `ca.pem`。broker 怎麼取用它們見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「MQTT broker」（Laysi/trustiot-v2#84）。

Laysi/trustiot-v2#534（2026-10-22）加上：

- 產線工作站憑證的 profile（broker 拒絕它），以及平台管理員簽發與撤銷它的 API（見[工作站憑證](#工作站憑證)）。這是 certs 第一個對外的 API。
- 簽發 CA 有了第一張 CRL 才簽章（見[發簽發 CA 憑證](#發簽發-ca-憑證)第 5 步）。

## CA 階層與私鑰保管

2026-10-21 決定（Laysi/trustiot-v2#78）。

```text
離線根 CA（ECDSA P-384，預設 20 年，pathLen 1）
  └─ 簽發 CA，每一代一張（ECDSA P-256，預設 2 年、不超過根，pathLen 0）
       ├─ 閘道器用戶端憑證（ECDSA P-256 的 TPM 金鑰，預設 90 天、不超過簽發 CA）
       ├─ 產線工作站憑證（ECDSA P-256，預設 180 天、不超過簽發 CA；EKU 沒有 clientAuth，見「工作站憑證」）
       └─ broker 伺服器憑證（EKU 只有 serverAuth，profile 在 Laysi/trustiot-v2#557，金鑰與輪替在 Laysi/trustiot-v2#535）
```

根 CA 在每次儀式另簽一張根 CRL，列出撤銷的簽發 CA，給閘道器驗 broker 的伺服器憑證用（2026-10-23 決定，見[閘道器對 broker 伺服器憑證的信任](#閘道器對-broker-伺服器憑證的信任)；閘道器端從檔案載入並檢查已在 Laysi/trustiot-v2#86，簽發與提供還沒實作，Laysi/trustiot-v2#560）。

- **根 CA**：私鑰只在離線的機器上產生與使用，不進叢集、不進 git（加密的也不放）、不在任何連網主機；以 age 加密給兩位維運人員，存兩份離線媒體，各由一人保管（2026-10-22 決定；與 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「根 CA 私鑰」相同）。只在簽發 CA 的儀式與備份檢查時取出。根憑證是公開資料，放在 chart 的 values。
- **簽發 CA**：私鑰在叢集內產生，沒有人建立、看到、複製或輸入它，做法與內部身分簽章金鑰相同（[docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「閘道器憑證 CA」）：
  - chart 宣告空的 Secret `<release>-certs-ca-keys`。certs pod 的 init container `ca-keys`（migration 映像的 `migrate certs-ca-keys`，[tools/migrate](../../tools/migrate/README.md)）產生 P-256 金鑰，寫進這個 Secret 的 `issuing-ca-keys.json`（格式在 [crates/ca-keys](../../crates/ca-keys/README.md)），再把每把金鑰的 CSR 寫進 ConfigMap `<release>-certs-ca-requests`（`issuing-ca-generation-<代>.csr.pem`）。
  - 維運人員把 CSR 帶到離線的機器，以根 CA 簽出簽發 CA 憑證（`certs-ca root sign`），憑證（公開）放進 values 的 `certificateAuthority.issuingCertificates`。chart 把根憑證與簽發 CA 憑證寫進 ConfigMap `<release>-certs-ca-certificates`，服務每 30 秒重讀，不必重啟。
  - 只有 certs 的主容器以唯讀 volume 掛這個 Secret（0440，root 與 pod 的 `fsGroup` 65533），主容器沒有 ServiceAccount token；只有 init container 有 token，Role 只允許對這個 Secret 與 CSR 的 ConfigMap `get`、`patch`。其他 pod、Role 與任何服務的自訂 `env` 都碰不到它（chart 測試）。
  - 一把金鑰有了鏈到根的憑證、而且資料庫裡已有它的第一張 CRL 才簽章（Laysi/trustiot-v2#534）；同時有幾把可用時，代最大、憑證這一刻有效、有 CRL 的一把簽新憑證，舊的照常發 CRL，直到它被移除。

比較過的做法：

| 做法 | 不採用的原因 |
| --- | --- |
| 單層：叢集裡的 CA 直接當閘道器的信任根 | 叢集或備份外洩就要換掉所有閘道器的信任根，只能靠 OTA 一台一台改；兩層時換簽發 CA 不動閘道器的信任設定 |
| 簽發 CA 私鑰由人產生，SaaS 以 SOPS 放進 git（原本的清冊寫法） | 私鑰經過人與工作站，SOPS 的 age 私鑰外洩時連歷史版本都能解，輪替時也要人再碰一次私鑰；叢集內部用的密鑰都改成在叢集內產生、不經過人（[docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「注入方式」與「內部身分簽章金鑰」） |
| 簽發 CA 私鑰存在 KMS 或 HSM | SaaS 與地端要兩套做法，地端客戶多半沒有；之後要更強的保護時再評估，私鑰的使用都在 `src/authority.rs` 一處，換成外部簽章不動其他程式 |
| 根 CA 也放在線上（例如叢集內） | 根是信任的源頭，線上就只剩叢集的保護；根只在儀式時使用，離線的成本低 |
| 由 cert-manager 管 CA | 它的 CA Issuer 要把私鑰放在 Secret 裡由人提供，也不發 CRL；broker 的 CRL 是決策 6 的要求 |

已知的限制（後續 issue 見 Laysi/trustiot-v2#78 的實作紀錄）：

- 根 CRL 還沒實作：簽發 CA 外洩時，broker 這一側靠「移除它的金鑰、broker 不再讀到它的 CRL」拒絕它簽的閘道器憑證（見[簽發 CA 金鑰遺失或外洩](#簽發-ca-金鑰遺失或外洩)）；閘道器這一側要靠根 CRL 才拒絕外洩的簽發 CA 簽的假伺服器憑證（2026-10-23 決定，見[閘道器對 broker 伺服器憑證的信任](#閘道器對-broker-伺服器憑證的信任)）。`certs-ca` 簽根 CRL、certs 提供它（Laysi/trustiot-v2#560）、閘道器取用新的根 CRL（Laysi/trustiot-v2#563）都還沒做（閘道器從檔案載入並檢查已在 Laysi/trustiot-v2#86），這之前閘道器端的這個風險只能靠換根解除，而換根也還沒有做法（下一點）。
- 根 CRL 擋不住一直站在連線路徑上的攻擊者：它可以擋下新的根 CRL，閘道器手上的舊 CRL 不列外洩的簽發 CA。根 CRL 過期也不擋連線（理由見同一節），所以這段期間沒有上限。
- 根 CA 換新還沒有做法。地端的 CA 階層（誰保管客戶的根 CA）延後：地端專用的工作排在 SaaS 之後（決策 1 的 2026-10-18 補充，Laysi/trustiot-v2#446）；chart 本身兩種型態相同。

## 發簽發 CA 憑證

第一次啟用 certs 服務、輪替與更換金鑰都用同一個儀式。需要：

- 一台離線的機器（不接網路），上面有 `certs-ca` 與 `age`。`certs-ca` 在可信的建置機以 `cargo build --release --locked -p certs --bin certs-ca` 建出（與部署的映像同一個 commit），`age` 以 `bun scripts/flux/age.ts` 從原始碼建出（[deploy/flux](../../deploy/flux/README.md)），兩者記下 SHA-256 再以可移除媒體帶過去並核對。
- 兩位維運人員的 age recipient（公鑰）；各自的 identity（私鑰）由本人保管。

以下的 `<namespace>`、`<release>` 是安裝的值。檔名照抄即可，憑證與 CSR 都是公開資料，私鑰只經過管線。

### 1. 建立根 CA（只做一次）

在離線的機器上：

```sh
certs-ca root create --common-name "TrustIoT Gateway Root CA 1" --certificate root.pem \
  | age --encrypt -r <維運人員 A 的 recipient> -r <維運人員 B 的 recipient> -o root-key.age
```

- 私鑰只從 stdout 出來，stdout 是終端機時拒絕執行；`root.pem` 已存在時拒絕且不輸出私鑰。
- 把 `root-key.age` 複製到兩份離線媒體，兩位維運人員各保管一份，然後依「[根 CA 私鑰的備份](#根-ca-私鑰的備份)」各自檢查自己那一份。
- CN 不要放客戶名稱或內部主機名稱（values 可能放進公開的 repo）。

### 2. 設定根憑證並啟用服務

`root.pem` 轉成一行 base64（values 不接受換行；chart 再印回 PEM）：

```sh
sed '/-----/d' root.pem | tr -d '\n'
```

設成 `certificateAuthority.rootCertificate`，同時啟用 `services.certs.enabled`，升級。certs pod 的 init container 產生第 1 代金鑰與它的 CSR；服務照常就緒（簽發 CA 不是 readiness 的條件：儀式要多久都可以，升級不會因為等不到就緒而失敗、被 Flux 回滾），但還不能簽章：指標 `certs.ca.signing` 是 0，載入時與之後每 `crl.check_interval` 記一行 WARN `no issuing CA certificate is valid now, ... keys wait for a certificate ...`，欄位 `waiting` 寫出等待中的金鑰：`<金鑰 id> (generation 1, request issuing-ca-generation-1.csr.pem)`。金鑰 id 是公鑰的 SHA-256（十六進位 64 字元）。

### 3. 取出 CSR 與等待中的金鑰 id

```sh
kubectl -n <namespace> get configmap <release>-certs-ca-requests \
  -o jsonpath='{.data.issuing-ca-generation-1\.csr\.pem}' > issuing-ca-generation-1.csr.pem
kubectl -n <namespace> logs deployment/<release>-certs -c certs | grep 'keys wait for a certificate' | tail -n 1
```

把 `waiting` 裡這一代的金鑰 id 抄下來（輪替時同一行也列出其他等待中的代，以 `generation` 區分）。CSR 以可移除媒體帶到離線的機器。

### 4. 以根 CA 簽發

在離線的機器上，由保管媒體的維運人員解密：

```sh
age --decrypt -i <自己的 identity> root-key.age \
  | certs-ca root sign --root-certificate root.pem --request issuing-ca-generation-1.csr.pem \
      --common-name "TrustIoT Gateway Issuing CA G1 2026-10-21" --certificate issuing-ca-generation-1.pem
```

- 私鑰只從 stdin 進來，stdin 是終端機時拒絕執行。工具只取 CSR 的公鑰（要是 P-256、CSR 的簽章要對），其他欄位全部由它決定：`CN=<--common-name>`、CA、pathLen 0、keyCertSign 與 cRLSign、預設 730 天（`--days`）且不超過根；服務也拒絕載入效期超過根的簽發 CA 憑證，根到期之後沒有簽發 CA 能簽章。
- CN 每一張簽發 CA 都要不同（broker 以簽發者名稱找 CRL，服務也拒絕載入同名的憑證），建議寫代與儀式日期。
- 它在 stderr 印出 `for key <金鑰 id>`。**和第 3 步抄下的 id 一致才繼續。** CSR 經過 ConfigMap，能改 ConfigMap 的人可以換成自己的金鑰；不一致時刪掉這張憑證、不要放進 values，依 [docs/runbooks/secrets.md](../../docs/runbooks/secrets.md) 的「外洩處理」調查誰改了 ConfigMap。

### 5. 設定簽發 CA 憑證

同第 2 步把 `issuing-ca-generation-1.pem` 轉成一行 base64，加進 `certificateAuthority.issuingCertificates`，升級。pod 不重啟：kubelet 更新掛載的 ConfigMap（約 1 分鐘），服務最多 30 秒後重讀，記 INFO `loaded the issuing CA`（`current` 是新的金鑰 id），`certs.ca.signing` 變成 1。CRL 工作下一輪（最多 `crl.check_interval`，預設 5 分鐘）發出它的第一張 CRL；這之前 `crl.pem` 回 503，告警指標也是 1。

新的簽發 CA 要等第一張 CRL 寫進資料庫才簽章（Laysi/trustiot-v2#534）：broker 讀不到一個簽發 CA 的 CRL 時拒絕它簽的每一張憑證，先簽出去的憑證在那之前會被拒絕。這段期間由仍然有效、已有 CRL 的上一代繼續簽；第一次啟用時沒有上一代，工作站憑證的 API 回 503（`workstation_certificate.ca_not_ready`），等一輪之後再試。`certs.ca.signing` 只看憑證，所以比實際能簽章早最多一輪變成 1。簽發紀錄也只在簽發 CA 已有 CRL 時寫入（`issue::record`），之後的出廠登錄與換發端點同樣受這條限制。

## 輪替

**簽發 CA**：預設效期 2 年，每年輪替一次，也就是目前的簽發 CA 剩不到 1 年時（指標 `certs.ca.remaining`）。閘道器憑證不會超過簽發 CA 的效期，剩不到 1 小時時不再簽，所以不能拖到最後。

1. `certificateAuthority.keyGeneration` 加一（例如 `"2"`）再升級：certs 滾動重啟，第一個新 pod 的 init container 加一把第 2 代金鑰、寫出 `issuing-ca-generation-2.csr.pem`；第 1 代照常簽章，服務載入時與之後每 `crl.check_interval` 記 INFO `issuing CA keys wait for a certificate ...`，`waiting` 寫出第 2 代。
2. 照「[發簽發 CA 憑證](#發簽發-ca-憑證)」第 3 到 5 步簽第 2 代，憑證**加在**清單後面（第 1 代的留著）。設定後第 2 代有了第一張 CRL 就簽新憑證，第 1 代繼續發 CRL。
3. 第 1 代簽的閘道器與工作站憑證都過期之後（第 2 步之後再過 `gateway.validity` 與 `workstation.validity` 較長的一個，預設 180 天，加一天），`certificateAuthority.removeKeysBeforeGeneration` 設成 `"2"` 升級：init container 移除第 1 代金鑰，它的 CRL 不再發布。再把第 1 代的憑證從清單拿掉（留著也只會被略過並記在日誌）。

只往上走：rollback 到較小的 `keyGeneration` 不會產生金鑰。Secret 最多 4 把（`crates/ca-keys` 的 `MAX_KEYS`），超過時 init container 失敗、新 pod 起不來，所以先移除舊的再加。

**根 CA**：換根要更新每台閘道器的信任設定，是重大變更，另開 issue 規劃（[docs/runbooks/secrets.md](../../docs/runbooks/secrets.md) 的「CA 私鑰」）。

## 根 CA 私鑰的備份

- 兩份 `root-key.age` 在不同的離線媒體，由兩位維運人員各保管一份；兩人的 identity 都能解開任一份。
- 每次儀式之後、以及每年至少一次，各自在離線的機器上檢查自己那一份：

  ```sh
  age --decrypt -i <自己的 identity> root-key.age | certs-ca root check --root-certificate root.pem
  ```

  成功時印出 `the root private key belongs to the root certificate in root.pem (key <金鑰 id>)`，不寫任何檔案；解不開或不是這張根憑證的私鑰時失敗（exit code 1）。一份壞了就從另一份再複製一份。
- 維運人員離開時，以留下的人與新成員的 recipient 重新加密（解密的管線直接接 `age --encrypt`），換掉兩份媒體，舊的銷毀（[docs/runbooks/secrets.md](../../docs/runbooks/secrets.md) 的「維運人員加入與離開」）。重新加密只擋住之後的存取：他之前解得開根私鑰，要不要換根依同一份 runbook 的「外洩處理」判斷（換根另開 issue 規劃）。

簽發 CA 私鑰不備份（2026-10-22 決定）：遺失時以新的一代取代（下一節），所以叢集外不需要有它的副本。簽發紀錄（`certs.issued_certificates`，撤銷要用）在 Postgres 的備份裡（Laysi/trustiot-v2#134）。

## 簽發 CA 金鑰遺失或外洩

**外洩**（或懷疑有人拿到 Secret、certs pod 或它的節點）：先止血，接受短暫中斷。

1. 同一次升級把 `keyGeneration` 與 `removeKeysBeforeGeneration` 都設成新的一代（例如目前是 `"2"` 就都設 `"3"`）：init container 移除外洩的金鑰並加一把新的。外洩的簽發 CA 不再出現在 `ca.pem` 與 `crl.pem`，broker 讀不到它的 CRL，就拒絕它簽的每一張閘道器憑證（broker 的 CRL 檢查與它沒有可用的簽發 CA 時拒絕每張閘道器憑證，由 Laysi/trustiot-v2#84 的 Rust 測試驗證）。這時服務沒有可用的簽發 CA，不能簽章（`certs.ca.signing` 是 0，每次檢查記 WARN）。
2. 照「[發簽發 CA 憑證](#發簽發-ca-憑證)」簽新的一代；外洩的那一張從 `issuingCertificates` 拿掉。
3. 閘道器經換發端點以 TPM 金鑰重新取得憑證（決策 12，Laysi/trustiot-v2#81），不必寄回。產線工作站由平台管理員重新簽發（[工作站憑證](#工作站憑證)的「換發」），外洩的簽發 CA 簽的工作站憑證已經不能鏈到 `ca.pem`。
4. 依 [docs/runbooks/secrets.md](../../docs/runbooks/secrets.md) 的「外洩處理」調查並開 `security` issue。

根 CRL 實作之後（[閘道器對 broker 伺服器憑證的信任](#閘道器對-broker-伺服器憑證的信任)），第 2 步的儀式同時以根 CA 簽一張新的根 CRL，列出外洩的簽發 CA 憑證：閘道器取到它之後，就拒絕那個簽發 CA 簽的伺服器憑證。broker 伺服器憑證的私鑰外洩、簽發 CA 沒有外洩時也這樣做：照[輪替](#輪替)加一代、簽新的伺服器憑證，並在根 CRL 撤銷簽出舊伺服器憑證的那一代，因為閘道器只看得到根 CRL。broker 不讀根 CRL，所以舊的一代簽的閘道器憑證照常有效，舊金鑰照輪替的時程移除即可。在這之前，閘道器仍然相信外洩的簽發 CA 簽的伺服器憑證，見[已知的限制](#ca-階層與私鑰保管)。

**遺失**：Secret 被刪除、清空，或從備份還原（還原的物件 uid 不同）時，init container 把它當作遺失，以目前的 `keyGeneration` 產生新的一組。服務不能簽章（`certs.ca.signing` 是 0），每次檢查記 WARN 與 `waiting`；舊的憑證因為對不上任何金鑰而被略過，舊的簽發 CA 不再發 CRL。照「發簽發 CA 憑證」簽新的金鑰（CN 要和舊的不同，或先把舊的憑證從清單拿掉），閘道器同樣經換發端點更新憑證。不沿用還原的金鑰（2026-10-22 決定），因為無法確定沒有別人拿過那份副本；代價是還原後到儀式完成、閘道器換發之前，閘道器連不上 broker。

## 閘道器憑證

`src/issue.rs`（驗收條件 1）：CA 只從 CSR 取公鑰，並確認 CSR 由對應的私鑰簽名（持有證明）、金鑰是 P-256（閘道器 TPM 的曲線）、簽章是 ecdsa-with-SHA256、CSR 不超過 4096 bytes 且沒有多餘的位元組。CSR 裡的主體、SAN、keyUsage、extendedKeyUsage、basicConstraints 與其他延伸欄位一律忽略。

| 欄位 | 值 |
| --- | --- |
| 主體 | `CN=<閘道器 id>`，只有這一項（呼叫端決定 id：出廠登錄與換發端點） |
| basicConstraints | `cA=FALSE`（critical） |
| keyUsage | digitalSignature（critical） |
| extendedKeyUsage | 只有 clientAuth |
| 金鑰識別碼 | subject 與 authority key identifier |
| 序號 | 128 位元隨機正整數 |
| 效期 | 簽發前 5 分鐘起（時鐘誤差）、`gateway.validity`（預設 90 天），不超過簽發 CA；簽發 CA 剩不到 1 小時時拒絕 |
| 沒有的欄位 | SAN、CRL distribution point（broker 從檔案讀 CRL，閘道器 id 就是 MQTT 身分） |

每張簽出的憑證先寫進 `certs.issued_certificates`，寫入成功才交給呼叫端（`issue::record`，在呼叫端的交易裡）；簽發 CA 還沒有 CRL 時不寫入，憑證也不交出。

## 閘道器對 broker 伺服器憑證的信任

2026-10-23 決定（Laysi/trustiot-v2#556）。閘道器的內層 TLS 在 WebSocket 裡直達 broker（決策 6），閘道器 agent（Laysi/trustiot-v2#86）照下表驗證 broker 的伺服器憑證（[edge/agent](../../edge/agent/README.md)「內層 TLS 對 broker 的信任」）。根 CRL 那一列，agent 從設定的檔案載入根 CRL 並檢查；換上新的根 CRL 時清掉 resumption 快取屬於取用新 CRL 的部分（Laysi/trustiot-v2#563），簽發與提供見[根 CRL](#根-crl)。

| 項目 | 設定 |
| --- | --- |
| 信任錨 | 只有根 CA 憑證（`certificateAuthority.rootCertificate` 的那一張），出廠登錄時交給閘道器（Laysi/trustiot-v2#79）。不信任系統或 WebPKI 的根、外層 TLS 的信任清單，也不把簽發 CA 當信任錨：換簽發 CA 不動閘道器的設定，與 broker 只信任 `ca.pem` 的理由相同 |
| 憑證鏈 | broker 在握手送出伺服器憑證，後面接簽出它的簽發 CA 憑證（Secret 的 `tls.crt`，Laysi/trustiot-v2#535），閘道器由此建到根 |
| 名稱 | SNI 與驗證都用 Laysi/trustiot-v2#557 定的固定名稱（SaaS 與地端相同，不依部署的主機名稱或外層 TLS 連的網址）。名稱驗證不能關 |
| 用途 | 伺服器憑證要有 EKU serverAuth（webpki 驗伺服器憑證時的要求）。同一個簽發 CA 簽的閘道器憑證只有 clientAuth，工作站憑證只有自訂用途，都不能當伺服器憑證；Laysi/trustiot-v2#557 的 profile 是唯一有 serverAuth 的，之後新增的 profile 不可以有 serverAuth 或沒有 EKU（webpki 對沒有 EKU 的憑證不檢查用途） |
| 撤銷 | 以根 CRL 檢查鏈上的簽發 CA。伺服器憑證本身的撤銷狀態不檢查：閘道器沒有簽發 CA 的 CRL（它每週重發，列的多半是閘道器憑證）。rustls 的 `WebPkiServerVerifier` 以 `with_crls` 帶入根 CRL；不用 `only_check_end_entity_revocation`（要檢查到簽發 CA 那一層）；要用 `allow_unknown_revocation_status`（伺服器憑證的簽發者沒有 CRL，不允許時每張都被拒）；不用 `enforce_revocation_expiration`（見下方「過期」） |
| 協定 | TLS 1.3、ChaCha20-Poly1305 排第一、開 session resumption（決策 6）。換上新的根 CRL 時清掉 resumption 的快取，下一次連線完整握手、以新的 CRL 驗證 |

### 簽發 CA 外洩時：比較過的做法

簽發 CA 的私鑰在叢集裡。它外洩時，broker 這一側在[簽發 CA 金鑰遺失或外洩](#簽發-ca-金鑰遺失或外洩)的第 1 步就拒絕它簽的閘道器憑證；但閘道器信任根，所以仍然接受它簽的假伺服器憑證。站在內層 TLS 路徑上的攻擊者因此可以假扮 broker：收下這台閘道器的遙測（也能回 `down/telemetry-ack`，閘道器以為送到了而丟掉暫存的資料）、送 `down/config`、`down/live-control`、`down/log-request`。它不能把連線轉給真的 broker（閘道器的私鑰在 TPM，外洩的簽發 CA 簽的閘道器憑證也已被 broker 拒絕），也不能經此更新閘道器的程式（OTA 由另一把金鑰簽章，決策 11）。

| 做法 | 結果 |
| --- | --- |
| **採用**：根 CA 在每次儀式簽一張長效的根 CRL，交給閘道器；閘道器以它檢查鏈上的簽發 CA | 外洩後的儀式本來就要簽新的一代（[簽發 CA 金鑰遺失或外洩](#簽發-ca-金鑰遺失或外洩)第 2 步），每年的輪替也是一次儀式，所以根 CRL 不多一次儀式，也不讓根上線。閘道器取到新的根 CRL 之後就拒絕外洩的簽發 CA，不必換根、不必一台一台改信任錨。CRL 是根簽的公開資料，經什麼管道送都不怕被竄改。代價：`certs-ca` 要能簽 CRL、要有送到閘道器的管道、agent 要帶 CRL 驗證；擋不住一直站在路徑上、從頭擋下新 CRL 的攻擊者（見下方「剩下的風險」） |
| 伺服器憑證改由另一個 CA 簽 | 那個 CA 若也在 certs 的 Secret 或 pod 裡，外洩範圍相同，等於沒有隔離。放在別的工作負載（例如 broker 端）只擋得住 certs pod 被入侵，擋不住叢集、etcd 或 namespace 管理者層級的外洩，卻多一套金鑰、儀式、輪替與 CRL（理由同[工作站憑證](#profile-的決定同一個簽發-ca以-eku-區分)不另立 CA）。改成在離線儀式直接簽長效的伺服器憑證也一樣：伺服器憑證的私鑰本身就在叢集的 Secret 裡，它外洩時同樣能假扮 broker，與 Laysi/trustiot-v2#535 的叢集內到期前輪替也衝突。不論哪一種，要讓閘道器不再相信某張憑證還是需要撤銷管道，所以不採用 |
| 接受風險，外洩時換根 | 換根要更新每台閘道器的信任錨（OTA 或其他管道），離線的閘道器要等到它下次更新；有路徑位置的攻擊者同樣可以擋下更新。換根目前沒有做法（[輪替](#輪替)），又是重大變更，把它當成每次簽發 CA 外洩的處理代價太高。保留給根 CA 私鑰本身外洩的情況（根 CRL 撤銷不了根） |
| 只靠外層 TLS（攻擊者還要站上內層 TLS 的路徑） | 只拿到私鑰、沒有路徑位置的攻擊者確實做不了事，但外層 TLS 不是我們的安全層：[部署與可搬移性](../../docs/architecture/deployment.md)寫明外層只負責穿過網路，工廠防火牆可以解開 443 並讓客戶把攔截用的 CA 加進外層的信任清單；SaaS 的 Cloudflare、tunnel、入口 gateway 與 mqtt-bridge 都看得到內層的位元組；而簽發 CA 最可能的外洩來源（叢集、certs pod 或它的節點被入侵）本身就給了叢集內的路徑位置。算深度防禦，不當作不需要撤銷的理由 |

### 根 CRL

做法（實作在後續 issue，`certs-ca` 的指令與參數由它決定，這裡定的是規則）：

- **簽發**：每次儀式（第一次發簽發 CA、每年輪替、外洩）在離線的機器上以根 CA 簽一張新的根 CRL。它累積列出撤銷的簽發 CA 憑證，到那張憑證過期才拿掉；CRL 編號只增加；上一張根 CRL 是輸入，工具確認它由同一個根簽章，再加上這次撤銷的項目。正常輪替不撤銷舊的一代（它自然過期）；外洩，或 broker 伺服器憑證的私鑰外洩時，撤銷簽出它的簽發 CA。
- **效期**：nextUpdate 是簽發後 400 天：儀式至少每年一次（簽發 CA 每年輪替），多一個月的餘裕。certs 服務在根 CRL 剩不到 60 天時告警，提醒該辦儀式。
- **發佈**：根 CRL 是公開資料，和根憑證一樣放在 values，certs 服務提供給閘道器。閘道器在出廠登錄時拿到當時的一張，之後定期經 HTTPS 取最新的（端點與路徑在後續 issue 依決策 27 的例外路徑規則決定；換發端點是同一類例外）。閘道器只接受由信任錨簽章、編號比手上大的根 CRL，存下來，重開機後沿用。
- **順序**：撤銷某一代的根 CRL 放進 values 之前，broker 要已換上新一代簽的伺服器憑證（Laysi/trustiot-v2#535 的換新），否則取到新 CRL 的閘道器連不上 broker。儀式可以同時簽出新的簽發 CA 憑證與根 CRL，根 CRL 晚一步放進 values。
- **過期**：nextUpdate 過了，閘道器仍然以手上的根 CRL 驗證、照常連線，並在狀態回報裡標出根 CRL 已過期。不擋連線，是因為根 CRL 要靠人到離線的機器簽，儀式晚了不該讓整個機隊斷線；改由 certs 的告警與閘道器的狀態回報讓人知道。
- **還沒有根 CRL 時**（目前，以及後續 issue 完成前出廠的閘道器）：閘道器不檢查撤銷，照常連線；取到第一張之後開始檢查。
- **格式**（2026-10-23，Laysi/trustiot-v2#86 的安全審查；`certs-ca` 簽根 CRL 時照做，Laysi/trustiot-v2#560）：閘道器的 agent 拒絕不符的根 CRL、不啟動。
  - 不帶 issuing distribution point：webpki 只把帶它的 CRL 用在範圍內的憑證，範圍排除簽發 CA（例如 onlyContainsUserCerts）或 URI 對不上簽發 CA 的 CRL distribution point 時，等於不檢查。
  - 不是 delta CRL，也不帶 webpki 不認得的 critical extension。
  - 簽章演算法要是 rustls 的 ring provider 接受的（根是 P-384，就是 ecdsa-with-SHA384），AlgorithmIdentifier 照標準編碼、不多帶參數：webpki 依位元組比對演算法，對不上時每次握手都失敗。
- **換根與主體**：新的根要用新的主體（subject）。CRL 只以主體指出簽發者，兩個同名的根同時在信任錨裡時，webpki 會拿錯的根的金鑰驗 CRL，經另一個根的連線全部失敗；agent 拒絕主體重複的 `broker.ca`。一張根 CRL 只涵蓋簽它的根底下的簽發 CA：換根期間 `broker.ca` 有兩個根時，agent 只載入一張根 CRL（`broker.crl`），另一個根底下的簽發 CA 不檢查撤銷。這是已知的限制，支援多張根 CRL 與換根的做法一起規劃（根 CA 換新還沒有做法，見[已知的限制](#ca-階層與私鑰保管)；取用多張根 CRL 屬於 Laysi/trustiot-v2#563 的範圍）。

剩下的風險：

- 攻擊者若在外洩被發現之前就站上某台閘道器的連線路徑，並一直擋下新的根 CRL，那台閘道器就一直相信外洩的簽發 CA；根 CRL 過期也不擋連線，所以沒有時間上限。這要攻擊者在每一台目標閘道器的路徑上長期存在，比一般的外洩難得多；被擋下時閘道器的狀態回報會一直停在舊的 CRL 編號，可以從這裡察覺。
- 根 CRL 實作之前，簽發 CA 外洩時閘道器端只能靠換根，而換根還沒有做法。展示環境接受這個風險（展示切片，`docs/roadmap/milestones.md`）。

## 工作站憑證

2026-10-22 決定（Laysi/trustiot-v2#534）。產線工作站以工作站憑證加平台管理員帳號呼叫出廠登錄 API（決策 12，Laysi/trustiot-v2#79）。工作站憑證不能當閘道器憑證連 broker。

### profile 的決定：同一個簽發 CA，以 EKU 區分

| 做法 | 結果 |
| --- | --- |
| **採用**：同一個簽發 CA；工作站憑證的 extendedKeyUsage 只有 TrustIoT 工作站用途（自訂 OID），沒有 clientAuth | broker（Mosquitto 以 OpenSSL 驗用戶端憑證，用途 `ssl_client`）對有 EKU 卻沒有 clientAuth 的用戶端憑證拒絕握手（`unsupported certificate purpose`），rustls（webpki）也一樣（`tests/workstation.rs` 兩者都測）。CA 階層、儀式、私鑰保管、`ca.pem` 與 CRL 都不變：[docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「簽發 CA 私鑰」本來就寫它簽發閘道器與工作站憑證。撤銷的工作站憑證列在同一個簽發 CA 的 CRL |
| 另立一個只簽工作站憑證的簽發 CA，broker 只信任閘道器的簽發 CA | 隔離最強，但多一套金鑰、儀式、輪替與 CRL，chart 與 init container 要分兩組，broker 的信任檔也要和 `ca.pem` 分開；改變已記錄的 CA 階層。EKU 已足以讓 broker 拒絕，不值得多一套 |
| 不加 EKU，只靠 CN 前綴或 broker 的 ACL 區分 | 沒有 EKU 的憑證對任何用途都有效，broker 的 TLS 層會接受它；只靠 ACL 等於讓工作站憑證登入 broker，錯一條規則就能冒用閘道器 |

第二道防線：即使哪個驗證端忽略 EKU，CN 是 `workstation:<名稱>`，含 `:`，永遠不是合法的閘道器 id（`protocol::GatewayId` 只允許英數字、`.`、`_`、`-`）；broker 以 CN 當使用者名稱（`use_identity_as_username`）時對不到任何閘道器的 ACL。

### 欄位

`src/issue.rs` 的 `workstation_certificate`：CA 只從 CSR 取公鑰，持有證明、金鑰與 CSR 的檢查和閘道器憑證相同（ECDSA P-256、ecdsa-with-SHA256、不超過 4096 bytes）；CSR 的主體、SAN、keyUsage、extendedKeyUsage、basicConstraints 與其他延伸欄位一律忽略。工作站的私鑰不經過 certs 服務。

| 欄位 | 值 |
| --- | --- |
| 主體 | `CN=workstation:<名稱>`，只有這一項；名稱是簽發請求的 `workstation`：1 到 48 個 ASCII 英數字、`.`、`_`、`-`，以英數字開頭 |
| basicConstraints | `cA=FALSE`（critical） |
| keyUsage | digitalSignature（critical） |
| extendedKeyUsage | 只有 TrustIoT 工作站用途 `2.25.316065004478888347166512336770143873285`（UUID `edc7e7d8-ab9e-4c72-a01b-aa39eb83e905` 在 2.25 弧下的 OID，不必註冊；`issue::WORKSTATION_PURPOSE`）；沒有 clientAuth、serverAuth 或 anyExtendedKeyUsage |
| 金鑰識別碼 | subject 與 authority key identifier |
| 序號 | 128 位元隨機正整數 |
| 效期 | 簽發前 5 分鐘起、`workstation.validity`（預設 180 天），不超過簽發 CA；簽發 CA 剩不到 1 小時時拒絕 |
| 沒有的欄位 | SAN、CRL distribution point |
| 簽發紀錄 | `certs.issued_certificates` 的一列，`profile` 是 `workstation`、`subject` 是整個 CN |

效期預設 180 天：平台管理員一年換發兩次；工作站的私鑰在一般主機上（能用 TPM 時才放在 TPM），效期短一點能限制外洩的影響，撤銷讓它更早失效。可設 1 小時到 398 天（`TRUSTIOT__WORKSTATION__VALIDITY`）。

### 出廠登錄 API 可以依賴的欄位（Laysi/trustiot-v2#79）

Cloudflare Tunnel 在邊緣終止外層 TLS，登錄 API 拿不到工作站的 TLS 用戶端憑證，所以改由工作站以私鑰簽請求：請求帶工作站憑證（PEM）與以它的私鑰做的簽章（ECDSA P-256、SHA-256）。登錄 API 依序確認：

1. 憑證由 `ca.pem` 裡的一個簽發 CA 簽發、鏈到根，現在在效期內。
2. extendedKeyUsage **含**工作站用途。比對 OID 的 DER 內容（`issue::WORKSTATION_PURPOSE_DER`），不要比對文字：有些函式庫把超過 64 位元的弧印成十六進位。這是必要條件：閘道器憑證沒有這個用途，所以同一個簽發 CA 的閘道器憑證不能冒充工作站。
3. 主體只有一個 CN，以 `workstation:` 開頭，之後是工作站名稱，寫進操作紀錄。
4. keyUsage 有 digitalSignature，請求的簽章以憑證的公鑰驗證通過。簽的內容由 Laysi/trustiot-v2#79 決定，要綁定請求本身（方法、路徑、本文的雜湊與時間或 nonce），不能重送。
5. 簽發紀錄有這個序號、`profile` 是 `workstation`、`revoked_at` 是 NULL。撤銷**立即**經由這一項生效；CRL 要等下一張排程的 CRL（見下面的「撤銷」）。登錄 API 不在 certs 服務裡時，經 certs 的內部 API 查（由 Laysi/trustiot-v2#79 定義），不直接讀這張表。
6. 呼叫者同時以平台管理員帳號登入（決策 12）。

### 簽發、撤銷與換發

前提：

- chart 的 `ingress.routes` 有 `{host: app, path: /api/certs, match: PathPrefix, service: certs}`（[deploy/chart/README.md](../../deploy/chart/README.md) 的「需要 cookie 的路由」），而且身分服務啟用：certs 以它的公鑰驗內部身分 token（`TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`）。
- 呼叫者是平台管理員（`platform_admin`）。API 只接受入口 cookie 檢查簽出的 token：沒有或驗不過 401 `unauthenticated`；不是平台管理員，或是別的服務以 token 交換代為呼叫（token 帶 `act`）時 403 `permission_denied`。簽發與撤銷憑證是管理員自己的動作，不讓任何服務代為執行。
- 所有回應帶 `cache-control: no-store`；請求的 `Content-Type` 要是 `application/json`（跨來源的網頁因此要先過 CORS preflight，沒有任何東西回應它）。

管理介面不在這個 issue 的範圍；在那之前以 HTTP 呼叫，帶平台管理員的 session cookie（`__Host-trustiot-session`，它是密鑰，不要留在 shell 歷史或檔案裡）。

**簽發**：在工作站上產生金鑰與 CSR，私鑰不離開工作站（之後由 Laysi/trustiot-v2#80 的工作站工具取代），例如：

```sh
openssl ecparam -name prime256v1 -genkey -noout -out workstation-key.pem
openssl req -new -sha256 -key workstation-key.pem -subj "/CN=line-a-ws-01" -out workstation.csr
```

CSR 的主體與其他欄位不影響結果。把 `workstation.csr`（公開資料）帶給平台管理員：

```sh
read -rs SESSION
jq -n --arg name line-a-ws-01 --rawfile csr workstation.csr '{workstation: $name, certificate_request: $csr}' \
  | curl -sS --fail-with-body -X POST "https://<app 主機>/api/certs/v1/workstation-certificates" \
      -H 'content-type: application/json' --cookie "__Host-trustiot-session=$SESSION" \
      --data-binary @- > issued.json
jq -r .certificate issued.json > workstation.pem
jq -r '.serial, .not_after' issued.json
```

201 回應有 `serial`（撤銷時用，要記下來）、`workstation`、`subject`、`ca_key_id`、`not_before`、`not_after` 與 PEM 的 `certificate`。`workstation.pem` 交給工作站，和它的私鑰放在一起。錯誤照 M1 格式：

| 回應 | 情況 |
| --- | --- |
| 400 `invalid_request` | `Content-Type` 不是 JSON、本文超過 16 KiB（`workstation_certificate.body_too_large`）或不是 JSON 物件（`workstation_certificate.body_invalid`） |
| 422 `validation_failed` | 不認得的欄位（`unknown_field`，例如自己送 `subject`）、缺欄位（`field_required`）、名稱不合規則（`name_invalid`）、不是一個 PEM CSR（`request_invalid`）、不是 P-256 或 ecdsa-with-SHA256（`request_unsupported`）、簽章對不上金鑰（`request_signature_invalid`）；鍵都以 `workstation_certificate.` 開頭，`field_errors` 指出欄位 |
| 503 `unavailable` | 還沒有能簽章的簽發 CA（`workstation_certificate.ca_not_ready`：沒有載入、沒有有效且已有 CRL 的、或剩不到 1 小時），或資料庫失敗；什麼都沒簽 |

**撤銷**：

```sh
curl -sS --fail-with-body -X POST "https://<app 主機>/api/certs/v1/workstation-certificates/<serial>/revoke" \
  -H 'content-type: application/json' --cookie "__Host-trustiot-session=$SESSION" \
  --data '{"reason": "key_compromise"}'
```

`reason` 可省略（CRL 不寫原因），或是 `unspecified`、`key_compromise`、`affiliation_changed`、`superseded`、`cessation_of_operation`、`privilege_withdrawn`。200 回傳撤銷後的紀錄（`revoked_at`、`revocation_reason`）；序號不是工作站憑證（包括閘道器憑證的序號）時 404，已經撤銷時 409（`workstation_certificate.already_revoked`）。撤銷不能收回，原因也不能再改（見「資料庫」）：撤銷錯了，就為工作站簽一張新的。簽發紀錄立即標記，登錄 API 依它立即拒絕；簽發 CA 的下一張排程 CRL 列出它：CRL 剩不到 `crl.renew_before` 時重發，預設設定下最多約 2 天加一輪 `crl.check_interval`。撤銷後立即重發 CRL 是 Laysi/trustiot-v2#82。

**換發**：到期前由平台管理員再簽一張（最好在工作站上產生新的金鑰與 CSR），名稱相同；工作站換用新憑證之後，以 `superseded` 撤銷舊的。舊的在撤銷或到期前仍然有效。還沒有列出工作站憑證的 API，序號要從簽發的回應記下來。

每次簽發與撤銷記一行 INFO（`issued a workstation certificate`、`revoked a workstation certificate`），欄位有平台管理員的使用者 id、序號、簽發 CA 的金鑰 id 與工作站名稱，不含 CSR 或憑證；被拒絕的請求記 INFO `refused: not a platform administrator, ...`。

## CRL 與告警

`src/crl.rs`、`src/monitor.rs`（驗收條件 2）：

- 啟動時與之後每 `crl.check_interval`（預設 5 分鐘），拿到工作鎖（`service_kit::job_lock`，所有副本中只有一個）的副本為每個憑證還有效、CRL 剩不到 `crl.renew_before`（預設 5 天）或還沒有 CRL 的簽發 CA 發一張新的：nextUpdate 是這一輪之後 `crl.validity`（預設 7 天），thisUpdate 往前 5 分鐘（同閘道器憑證的 notBefore，broker 的時鐘稍慢也不會看到還沒生效的 CRL），CRL 編號遞增（資料庫只接受更大的編號，兩個副本重疊也不會倒退）。CRL 列出該 CA 還沒過期的撤銷憑證與撤銷時間、原因；簽發者與 authority key identifier 對得上簽發 CA 的憑證，OpenSSL（Mosquitto）以此找到 CRL。
- broker 檢查每張閘道器憑證的簽發者的 CRL；CRL 過期後，那個簽發 CA 的閘道器全部被拒絕，所以 CRL 不能斷。每個副本每 `crl.check_interval` 讀一次每個 CRL 的 nextUpdate，以 OTLP 匯出指標（屬性 `certs.ca.key_id`、`certs.ca.generation`）：

| 指標 | 意義 |
| --- | --- |
| `certs.crl.remaining`（秒） | 距 CRL 的 nextUpdate 的秒數，過了是負數；還沒有 CRL 時沒有資料點 |
| `certs.crl.expiring`（0 或 1） | CRL 剩不到 `crl.alert_before`（預設 3 天）或還沒有 CRL 時是 1；這段期間每次檢查都記一行 ERROR |
| `certs.ca.remaining`（秒） | 距簽發 CA 憑證到期的秒數，給輪替用（見[輪替](#輪替)） |
| `certs.ca.signing`（0 或 1，沒有屬性） | 有效期內的簽發 CA 憑證存在、服務能簽章時是 1（新的簽發 CA 在第一張 CRL 之前也算，見[發簽發 CA 憑證](#發簽發-ca-憑證)第 5 步）；是 0 時每次檢查記一行 WARN 並列出等待憑證的金鑰 |

指標的值在讀取時從上次讀到的時間算出，資料庫連不上時仍繼續倒數。Grafana 的告警（決策 23，[可觀測性](../../docs/architecture/observability.md)的「憑證到期」，Laysi/trustiot-v2#183）在 `certs.crl.expiring` 是 1、或這個序列沒有資料（服務停了）時觸發。`renew_before` 必須介於 `alert_before` 與 `validity` 之間，且兩者的差至少是兩次檢查，所以正常時重發一輪失敗也不會告警。

## 端點

`/internal` 底下只有公開資料：入口不轉送 `/internal`（決策 27），叢集內只有 `mesh.calls` 列出 certs 的服務，以及 MQTT broker 的 pod（`mqttBroker.enabled` 時由 chart 自動加上，Laysi/trustiot-v2#84）連得到。

| 端點 | 回應 |
| --- | --- |
| `GET /internal/certs/v1/crl.pem` | 每個憑證還有效的簽發 CA 的目前 CRL，PEM（Mosquitto 的 `crlfile`）。其中一個還沒有 CRL 時 503，取用端保留手上的檔案；沒有任何可用的簽發 CA 時是空檔，broker 因此改用不信任任何閘道器的佔位 CA，拒絕每張閘道器憑證（Mosquitto 載入空的 CRL 檔會停止，所以 broker 不直接用它） |
| `GET /internal/certs/v1/ca.pem` | 根憑證，接著每個可用的簽發 CA 憑證，PEM。還沒載入 CA 時 503 |

給平台管理員的 API 在 `/api/certs/v1/` 底下，經入口的 cookie 路由、驗內部身分 token（用法與錯誤見[工作站憑證](#工作站憑證)的「簽發、撤銷與換發」，`src/workstations.rs`）：

| 端點 | 回應 |
| --- | --- |
| `POST /api/certs/v1/workstation-certificates` | 201，簽出並記錄的工作站憑證 |
| `POST /api/certs/v1/workstation-certificates/{serial}/revoke` | 200，撤銷後的紀錄 |

規格在 `api/openapi.json`（`tools/openapi` 的 `SERVICES`；`/internal` 的端點不在其中），請求與回應的型別在 `crates/contract` 的 `certs` 模組。

## 設定

設定由 `service_kit::config` 載入（kit README 的「設定」）。時間長度的寫法是 `30s`、`5m`、`7d` 這類（humantime）。

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 平台設定：Postgres 位置；只接受 `verify-full`，`_CA_FILE` 必填 |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 平台設定：身分服務的公鑰（JWK Set），驗內部身分 token 用；chart 在身分服務啟用時設定，檔案變更時不必重啟就重讀。沒有這個變數時服務不啟動 |
| `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | 服務角色 `certs_app` 的帳密：密碼來自 Secret `<release>-certs-password` 的 `certs_app` key |
| `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 每個副本的連線上限，2–100，預設 5 |
| `TRUSTIOT__CA__KEYS_FILE` | 簽發 CA 金鑰檔；chart 設成 `/etc/trustiot/certs-ca-keys/issuing-ca-keys.json` |
| `TRUSTIOT__CA__ROOT_CERTIFICATE_FILE` | 根憑證；chart 設成 `/etc/trustiot/certs-ca/root.pem` |
| `TRUSTIOT__CA__CERTIFICATES_FILE` | 簽發 CA 憑證；chart 設成 `/etc/trustiot/certs-ca/issuing.pem` |
| `TRUSTIOT__CA__RELOAD_INTERVAL` | 多久重讀上面三個檔案，1 秒到 1 小時，預設 `30s`。檔案沒變時不重建；變了但不能用時保留上一份可用的，ERROR `cannot load the issuing CA` 每個問題只記一次 |
| `TRUSTIOT__GATEWAY__VALIDITY` | 閘道器憑證效期，1 小時到 398 天，預設 `90d` |
| `TRUSTIOT__WORKSTATION__VALIDITY` | 工作站憑證效期，1 小時到 398 天，預設 `180d`（理由見[工作站憑證](#工作站憑證)） |
| `TRUSTIOT__CRL__CHECK_INTERVAL` | CRL 工作與指標的週期，至少 1 秒，預設 `5m` |
| `TRUSTIOT__CRL__VALIDITY` | 新 CRL 的效期，最多 31 天，預設 `7d` |
| `TRUSTIOT__CRL__RENEW_BEFORE` | 剩不到這麼久就重發，預設 `5d` |
| `TRUSTIOT__CRL__ALERT_BEFORE` | 剩不到這麼久就告警，預設 `3d` |

三個 `TRUSTIOT__CA__*` 由 chart 設定，自訂 `env` 不能覆寫。健康埠（預設 8081）只有 `/healthz` 與 `/readyz`，端點在 8080。readiness 要服務角色開得出通過 service-kit session 檢查的連線、有一組可用的內部身分公鑰，而且資料庫有這一版預期的每個 migration 檔案；能不能簽章不影響 readiness，看指標 `certs.ca.signing`（理由見[發簽發 CA 憑證](#發簽發-ca-憑證)第 2 步）。設定不能用時以 exit code 1 結束。

## 資料庫

schema `certs`，DDL 在 `migrations/postgres/`，由 migration Job（登記在 `tools/migrate/src/registry.rs` 的 `PRODUCT`；不產生也不消費事件）以 migration 角色套用。檔案清單在 `src/schema.rs`，Job 與服務的 readiness 編譯同一份。

| 角色 | 權限 |
| --- | --- |
| `certs_owner`（migration 角色） | 擁有 schema 與所有表 |
| `certs_app`（服務角色） | `issued_certificates` 的 `SELECT`、`INSERT`，以及只限 `revoked_at`、`revocation_reason` 兩欄的 `UPDATE`（`0002_workstation_certificates.sql`，Laysi/trustiot-v2#534；閘道器的撤銷 Laysi/trustiot-v2#82 沿用），改不了簽出的內容、也不能刪除；`crls` 的 `SELECT`、`INSERT`、`UPDATE` |

撤銷是最終的，對所有角色都一樣，包括擁有者與 superuser：`0002_workstation_certificates.sql` 的 trigger `issued_certificates_guard` 在每次 `UPDATE` 前檢查兩件事。一是撤銷過的列不能再改：不能收回撤銷、改撤銷時間或原因，否則有問題或被入侵的服務能讓憑證從下一張 CRL 消失。二是 `UPDATE` 只能改撤銷的兩欄：欄位權限已經這樣限制服務角色，trigger 再擋下擁有者與 superuser；它比對撤銷兩欄以外的整列，所以之後新增的欄位也不能改，除非新的 migration 另外處理。違反時錯誤是 SQLSTATE 42501（`insufficient_privilege`），訊息寫明序號。服務撤銷時只更新 `revoked_at IS NULL` 的列，不會碰到 trigger；trigger 只有擁有者能停用（`ALTER TABLE`），只有 superuser 能略過（`session_replication_role`）。

| 表 | 內容 | RLS |
| --- | --- | --- |
| `issued_certificates` | 每張簽出的憑證：序號（主鍵）、簽發 CA 的金鑰 id、profile（`gateway` 或 `workstation`）、CN、效期、簽發時間、DER，撤銷時間與原因 | 無：不是租戶資料。閘道器憑證屬於閘道器，閘道器屬於哪個租戶是登錄（Laysi/trustiot-v2#79）與資產服務的紀錄；產線工作站屬於平台，不屬於租戶；之後在這裡存租戶 id 時要改成租戶表 |
| `crls` | 每個簽發 CA 目前的 CRL：金鑰 id（主鍵）、CRL 編號、thisUpdate、nextUpdate、DER | 無：公開資料 |
| `trustiot_migrate_applied` | migration Job 套用過的檔案，`certs_app` 只能 `SELECT` | 無 |

這裡不存任何私鑰：簽發 CA 私鑰在 Secret，根私鑰離線，閘道器的私鑰不離開 TPM，工作站的私鑰不離開工作站。

## 映像

`deploy/images/certs/image.toml`：與其他服務相同，交叉編譯成 `aarch64-unknown-linux-musl` 的靜態執行檔（只有 `certs`，`certs-ca` 不進映像），以 `deploy/images/rust-binary.Containerfile` 包成映像。chart 的 `services.certs.enabled` 預設 `false`；nightly 與展示環境的啟用是後續工作。chart 的設定在 [deploy/chart/README.md](../../deploy/chart/README.md) 的「閘道器憑證 CA」。

## 測試

在 repo 根目錄執行：

```sh
CARGO_BUILD_JOBS=2 cargo test -p certs
```

標註需要容器的測試以 `testkit::product` 起一個帶 TLS 的暫時 Postgres（podman 或 docker），由 migration Job 的程式套用整個產品的 schema，再以 `certs_app` 啟動真的 binary；沒有容器時略過，設了 `CI` 時改為失敗。

| 測試 | 驗證 |
| --- | --- |
| `tests/issuance.rs` | 驗收條件 1：要求別的 CN、SAN、serverAuth、codeSigning、各種 keyUsage 與 CA 的 CSR，簽出的都是 CA 決定的 profile，並能在根之下驗證為 TLS 用戶端憑證、不能當伺服器憑證或簽其他憑證；別的曲線或演算法、驗不過的簽章、改過的主體、過大或多餘的位元組都拒絕；代最大的有效簽發 CA 簽章；簽發 CA 還沒有 CRL 時簽發紀錄寫不進去，有了才寫入且內容正確；`certs_app` 只能改撤銷的兩欄，改不了簽出的內容、也刪不掉；撤銷之後 `certs_app` 與 superuser 都不能收回、改撤銷時間或原因，superuser 也改不了簽出的內容，被 trigger 以 42501 拒絕、列不變（容器） |
| `tests/crl.rs` | 驗收條件 2：CRL 的內容，以及 TLS 伺服器依它拒絕撤銷的憑證、CRL 過期後拒絕全部；新 CRL 的 nextUpdate 減 thisUpdate 是 `crl.validity` 加往前的 5 分鐘；真的服務依排程為每個簽發 CA 重發 CRL 並由 `crl.pem` 發布；以 superuser 拿住工作鎖、把 nextUpdate 調近時 `certs.crl.expiring` 經 OTLP 變成 1 並記 ERROR，放開後工作重發、指標回到 0（容器） |
| `tests/key_custody.rs` | 驗收條件 3：程式檢查，只有列出並說明理由的位置能把私鑰變成位元組或文字；執行時，日誌開到 `trace`、開 OTLP、有等待中的金鑰、重讀與兩種壞掉的金鑰檔，日誌、端點（含簽發工作站憑證的回應）與遙測都不含私鑰的任何片段（PKCS#8 涵蓋私鑰純量的任 24 個 base64 字元，其餘部分是演算法與公鑰，簽發 CA 憑證本來就公開；私鑰純量的十六進位、base64 與十進位位元組）（容器） |
| `tests/ceremony.rs` | 真的 `certs-ca`：`root create` 的私鑰只到 stdout、`root sign` 只從 stdin 讀、簽出的簽發 CA 服務接受並印出金鑰 id、`root check` 認得自己的私鑰；別的私鑰、P-256 私鑰、根的名稱、0 天、已存在的輸出檔都拒絕，且不印出私鑰；真的服務在儀式前就緒、`certs.ca.signing` 是 0、每次檢查記 WARN 與等待中的金鑰 id，掛上憑證後不重啟就變成 1 並發出第一張 CRL（容器） |
| `tests/cross_tenant.rs` | [crates/testkit](../../crates/testkit/README.md) 的覆蓋檢查：服務角色碰得到的每張表、規格裡的每個端點都要宣告；這裡的表都不是租戶資料、工作站憑證的兩個端點只給平台管理員，都豁免，並以 `without_tenant_data` 說明理由（容器） |
| `tests/workstation.rs` | Laysi/trustiot-v2#534：CSR 要求別的 CN、O、SAN、CA、keyUsage 與 clientAuth、serverAuth、anyExtendedKeyUsage 時，簽出的仍是 CA 決定的工作站 profile（EKU 只有工作站用途），rustls 驗用戶端憑證時接受閘道器憑證、拒絕工作站憑證；簽章驗不過、主體改過或 P-384 的 CSR 拒絕；沒有 token、不是 token、別的金鑰簽的、別的 audience、過期的 token 回 401，租戶管理員、沒有租戶的使用者與代使用者交換來的平台管理員 token 回 403，簽發與撤銷都一樣，而且簽發紀錄不變；壞的 CSR、不認得的欄位與錯的 Content-Type 不簽發；閘道器憑證、不存在或格式錯的序號撤銷回 404，撤銷寫入原因並記 INFO，重複撤銷回 409，下一張排程 CRL 列出它與原因；Mosquitto（OpenSSL）以 `ca.pem` 與 `crl.pem` 驗用戶端憑證時接受閘道器憑證，拒絕撤銷的閘道器憑證與 API 簽出的工作站憑證（容器） |

chart 的部分（Secret、Role、掛載、values 的檢查）在 `deploy/chart/tests/chart.test.ts` 的 “gateway certificate authority”，init container 對假的 Kubernetes API 的行為在 `tools/migrate/tests/ca_keys.rs`，金鑰檔的格式與規則在 `crates/ca-keys` 的單元測試。
