# certs（憑證服務）

自建 CA 的線上簽發 CA（決策 6、12；[部署與可搬移性](../../docs/architecture/deployment.md)的「憑證與佈建」）：以離線根 CA 簽出的簽發 CA 為閘道器簽 mTLS 用戶端憑證，定期重發每個簽發 CA 的 CRL，給 MQTT broker 讀。

目前是 Laysi/trustiot-v2#78 的範圍（2026-10-21）：

- 服務骨架：自己的 schema `certs`、服務角色、簽發紀錄與 CRL 兩張表（沒有租戶資料，見[資料庫](#資料庫)）。
- CA 階層與私鑰保管（見下一節），以及根 CA 的離線儀式工具 `certs-ca`。
- 從 CSR 簽閘道器憑證的程式（`src/issue.rs`）：只取 CSR 的公鑰，CN、EKU 與效期由 CA 決定。還沒有對外的簽發 API：出廠登錄（Laysi/trustiot-v2#79）與換發端點（Laysi/trustiot-v2#81）呼叫它，撤銷（Laysi/trustiot-v2#82）寫簽發紀錄的撤銷欄位。
- CRL 依排程重發、到期前告警，以及給 broker 的 `crl.pem` 與 `ca.pem`。broker 怎麼取用它們見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「MQTT broker」（Laysi/trustiot-v2#84）。

## CA 階層與私鑰保管

2026-10-21 決定（Laysi/trustiot-v2#78）。

```text
離線根 CA（ECDSA P-384，預設 20 年，pathLen 1）
  └─ 簽發 CA，每一代一張（ECDSA P-256，預設 2 年、不超過根，pathLen 0）
       └─ 閘道器用戶端憑證（ECDSA P-256 的 TPM 金鑰，預設 90 天、不超過簽發 CA）
```

- **根 CA**：私鑰只在離線的機器上產生與使用，不進叢集、不進 git（加密的也不放）、不在任何連網主機；以 age 加密給兩位維運人員，存兩份離線媒體，各由一人保管（2026-10-22 決定；與 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「根 CA 私鑰」相同）。只在簽發 CA 的儀式與備份檢查時取出。根憑證是公開資料，放在 chart 的 values。
- **簽發 CA**：私鑰在叢集內產生，沒有人建立、看到、複製或輸入它，做法與內部身分簽章金鑰相同（[docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「閘道器憑證 CA」）：
  - chart 宣告空的 Secret `<release>-certs-ca-keys`。certs pod 的 init container `ca-keys`（migration 映像的 `migrate certs-ca-keys`，[tools/migrate](../../tools/migrate/README.md)）產生 P-256 金鑰，寫進這個 Secret 的 `issuing-ca-keys.json`（格式在 [crates/ca-keys](../../crates/ca-keys/README.md)），再把每把金鑰的 CSR 寫進 ConfigMap `<release>-certs-ca-requests`（`issuing-ca-generation-<代>.csr.pem`）。
  - 維運人員把 CSR 帶到離線的機器，以根 CA 簽出簽發 CA 憑證（`certs-ca root sign`），憑證（公開）放進 values 的 `certificateAuthority.issuingCertificates`。chart 把根憑證與簽發 CA 憑證寫進 ConfigMap `<release>-certs-ca-certificates`，服務每 30 秒重讀，不必重啟。
  - 只有 certs 的主容器以唯讀 volume 掛這個 Secret（0440，root 與 pod 的 `fsGroup` 65533），主容器沒有 ServiceAccount token；只有 init container 有 token，Role 只允許對這個 Secret 與 CSR 的 ConfigMap `get`、`patch`。其他 pod、Role 與任何服務的自訂 `env` 都碰不到它（chart 測試）。
  - 一把金鑰有了鏈到根的憑證才簽章；同時有幾把可用時，代最大、憑證這一刻有效的一把簽新憑證，舊的照常發 CRL，直到它被移除。

比較過的做法：

| 做法 | 不採用的原因 |
| --- | --- |
| 單層：叢集裡的 CA 直接當閘道器的信任根 | 叢集或備份外洩就要換掉所有閘道器的信任根，只能靠 OTA 一台一台改；兩層時換簽發 CA 不動閘道器的信任設定 |
| 簽發 CA 私鑰由人產生，SaaS 以 SOPS 放進 git（原本的清冊寫法） | 私鑰經過人與工作站，SOPS 的 age 私鑰外洩時連歷史版本都能解，輪替時也要人再碰一次私鑰；叢集內部用的密鑰都改成在叢集內產生、不經過人（[docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「注入方式」與「內部身分簽章金鑰」） |
| 簽發 CA 私鑰存在 KMS 或 HSM | SaaS 與地端要兩套做法，地端客戶多半沒有；之後要更強的保護時再評估，私鑰的使用都在 `src/authority.rs` 一處，換成外部簽章不動其他程式 |
| 根 CA 也放在線上（例如叢集內） | 根是信任的源頭，線上就只剩叢集的保護；根只在儀式時使用，離線的成本低 |
| 由 cert-manager 管 CA | 它的 CA Issuer 要把私鑰放在 Secret 裡由人提供，也不發 CRL；broker 的 CRL 是決策 6 的要求 |

已知的限制（後續 issue 見 Laysi/trustiot-v2#78 的實作紀錄）：

- 根 CA 不發 CRL：簽發 CA 外洩時，靠「移除它的金鑰、broker 不再讀到它的 CRL」讓 broker 拒絕它簽的閘道器憑證（見[簽發 CA 金鑰遺失或外洩](#簽發-ca-金鑰遺失或外洩)）。閘道器驗 broker 的伺服器憑證時看不到這件事；broker 伺服器憑證怎麼簽、閘道器信任什麼由 Laysi/trustiot-v2#535 決定。
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

## 輪替

**簽發 CA**：預設效期 2 年，每年輪替一次，也就是目前的簽發 CA 剩不到 1 年時（指標 `certs.ca.remaining`）。閘道器憑證不會超過簽發 CA 的效期，剩不到 1 小時時不再簽，所以不能拖到最後。

1. `certificateAuthority.keyGeneration` 加一（例如 `"2"`）再升級：certs 滾動重啟，第一個新 pod 的 init container 加一把第 2 代金鑰、寫出 `issuing-ca-generation-2.csr.pem`；第 1 代照常簽章，服務載入時與之後每 `crl.check_interval` 記 INFO `issuing CA keys wait for a certificate ...`，`waiting` 寫出第 2 代。
2. 照「[發簽發 CA 憑證](#發簽發-ca-憑證)」第 3 到 5 步簽第 2 代，憑證**加在**清單後面（第 1 代的留著）。設定後第 2 代簽新憑證，第 1 代繼續發 CRL。
3. 第 1 代簽的閘道器憑證都過期之後（第 2 步之後再過 `gateway.validity`，預設 90 天，加一天），`certificateAuthority.removeKeysBeforeGeneration` 設成 `"2"` 升級：init container 移除第 1 代金鑰，它的 CRL 不再發布。再把第 1 代的憑證從清單拿掉（留著也只會被略過並記在日誌）。

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
3. 閘道器經換發端點以 TPM 金鑰重新取得憑證（決策 12，Laysi/trustiot-v2#81），不必寄回。
4. 依 [docs/runbooks/secrets.md](../../docs/runbooks/secrets.md) 的「外洩處理」調查並開 `security` issue。

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

每張簽出的憑證先寫進 `certs.issued_certificates`，寫入成功才交給呼叫端（`issue::record`，在呼叫端的交易裡）。

## CRL 與告警

`src/crl.rs`、`src/monitor.rs`（驗收條件 2）：

- 啟動時與之後每 `crl.check_interval`（預設 5 分鐘），拿到工作鎖（`service_kit::job_lock`，所有副本中只有一個）的副本為每個憑證還有效、CRL 剩不到 `crl.renew_before`（預設 5 天）或還沒有 CRL 的簽發 CA 發一張新的：nextUpdate 是這一輪之後 `crl.validity`（預設 7 天），thisUpdate 往前 5 分鐘（同閘道器憑證的 notBefore，broker 的時鐘稍慢也不會看到還沒生效的 CRL），CRL 編號遞增（資料庫只接受更大的編號，兩個副本重疊也不會倒退）。CRL 列出該 CA 還沒過期的撤銷憑證與撤銷時間、原因；簽發者與 authority key identifier 對得上簽發 CA 的憑證，OpenSSL（Mosquitto）以此找到 CRL。
- broker 檢查每張閘道器憑證的簽發者的 CRL；CRL 過期後，那個簽發 CA 的閘道器全部被拒絕，所以 CRL 不能斷。每個副本每 `crl.check_interval` 讀一次每個 CRL 的 nextUpdate，以 OTLP 匯出指標（屬性 `certs.ca.key_id`、`certs.ca.generation`）：

| 指標 | 意義 |
| --- | --- |
| `certs.crl.remaining`（秒） | 距 CRL 的 nextUpdate 的秒數，過了是負數；還沒有 CRL 時沒有資料點 |
| `certs.crl.expiring`（0 或 1） | CRL 剩不到 `crl.alert_before`（預設 3 天）或還沒有 CRL 時是 1；這段期間每次檢查都記一行 ERROR |
| `certs.ca.remaining`（秒） | 距簽發 CA 憑證到期的秒數，給輪替用（見[輪替](#輪替)） |
| `certs.ca.signing`（0 或 1，沒有屬性） | 有效期內的簽發 CA 憑證存在、服務能簽章時是 1；是 0 時每次檢查記一行 WARN 並列出等待憑證的金鑰（見[發簽發 CA 憑證](#發簽發-ca-憑證)） |

指標的值在讀取時從上次讀到的時間算出，資料庫連不上時仍繼續倒數。Grafana 的告警（決策 23，[可觀測性](../../docs/architecture/observability.md)的「憑證到期」，Laysi/trustiot-v2#183）在 `certs.crl.expiring` 是 1、或這個序列沒有資料（服務停了）時觸發。`renew_before` 必須介於 `alert_before` 與 `validity` 之間，且兩者的差至少是兩次檢查，所以正常時重發一輪失敗也不會告警。

## 端點

只有公開資料，都在 `/internal` 底下：入口不轉送 `/internal`（決策 27），叢集內只有 `mesh.calls` 列出 certs 的服務，以及 MQTT broker 的 pod（`mqttBroker.enabled` 時由 chart 自動加上，Laysi/trustiot-v2#84）連得到。

| 端點 | 回應 |
| --- | --- |
| `GET /internal/certs/v1/crl.pem` | 每個憑證還有效的簽發 CA 的目前 CRL，PEM（Mosquitto 的 `crlfile`）。其中一個還沒有 CRL 時 503，取用端保留手上的檔案；沒有任何可用的簽發 CA 時是空檔，broker 因此改用不信任任何閘道器的佔位 CA，拒絕每張閘道器憑證（Mosquitto 載入空的 CRL 檔會停止，所以 broker 不直接用它） |
| `GET /internal/certs/v1/ca.pem` | 根憑證，接著每個可用的簽發 CA 憑證，PEM。還沒載入 CA 時 503 |

沒有需要內部身分的 API，所以 `tools/openapi` 把 certs 列在 `SERVICES_WITHOUT_API`。

## 設定

設定由 `service_kit::config` 載入（kit README 的「設定」）。時間長度的寫法是 `30s`、`5m`、`7d` 這類（humantime）。

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 平台設定：Postgres 位置；只接受 `verify-full`，`_CA_FILE` 必填 |
| `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | 服務角色 `certs_app` 的帳密：密碼來自 Secret `<release>-certs-password` 的 `certs_app` key |
| `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 每個副本的連線上限，2–100，預設 5 |
| `TRUSTIOT__CA__KEYS_FILE` | 簽發 CA 金鑰檔；chart 設成 `/etc/trustiot/certs-ca-keys/issuing-ca-keys.json` |
| `TRUSTIOT__CA__ROOT_CERTIFICATE_FILE` | 根憑證；chart 設成 `/etc/trustiot/certs-ca/root.pem` |
| `TRUSTIOT__CA__CERTIFICATES_FILE` | 簽發 CA 憑證；chart 設成 `/etc/trustiot/certs-ca/issuing.pem` |
| `TRUSTIOT__CA__RELOAD_INTERVAL` | 多久重讀上面三個檔案，1 秒到 1 小時，預設 `30s`。檔案沒變時不重建；變了但不能用時保留上一份可用的，ERROR `cannot load the issuing CA` 每個問題只記一次 |
| `TRUSTIOT__GATEWAY__VALIDITY` | 閘道器憑證效期，1 小時到 398 天，預設 `90d` |
| `TRUSTIOT__CRL__CHECK_INTERVAL` | CRL 工作與指標的週期，至少 1 秒，預設 `5m` |
| `TRUSTIOT__CRL__VALIDITY` | 新 CRL 的效期，最多 31 天，預設 `7d` |
| `TRUSTIOT__CRL__RENEW_BEFORE` | 剩不到這麼久就重發，預設 `5d` |
| `TRUSTIOT__CRL__ALERT_BEFORE` | 剩不到這麼久就告警，預設 `3d` |

三個 `TRUSTIOT__CA__*` 由 chart 設定，自訂 `env` 不能覆寫。健康埠（預設 8081）只有 `/healthz` 與 `/readyz`，端點在 8080。readiness 要服務角色開得出通過 service-kit session 檢查的連線，而且資料庫有這一版預期的每個 migration 檔案；能不能簽章不影響 readiness，看指標 `certs.ca.signing`（理由見[發簽發 CA 憑證](#發簽發-ca-憑證)第 2 步）。設定不能用時以 exit code 1 結束。

## 資料庫

schema `certs`，DDL 在 `migrations/postgres/`，由 migration Job（登記在 `tools/migrate/src/registry.rs` 的 `PRODUCT`；不產生也不消費事件）以 migration 角色套用。檔案清單在 `src/schema.rs`，Job 與服務的 readiness 編譯同一份。

| 角色 | 權限 |
| --- | --- |
| `certs_owner`（migration 角色） | 擁有 schema 與所有表 |
| `certs_app`（服務角色） | `issued_certificates` 的 `SELECT`、`INSERT`（撤銷的 `UPDATE` 由 Laysi/trustiot-v2#82 在自己的 migration 授權）；`crls` 的 `SELECT`、`INSERT`、`UPDATE` |

| 表 | 內容 | RLS |
| --- | --- | --- |
| `issued_certificates` | 每張簽出的憑證：序號（主鍵）、簽發 CA 的金鑰 id、profile、CN、效期、簽發時間、DER，撤銷時間與原因 | 無：不是租戶資料。憑證屬於閘道器，閘道器屬於哪個租戶是登錄（Laysi/trustiot-v2#79）與資產服務的紀錄；之後在這裡存租戶 id 時要改成租戶表 |
| `crls` | 每個簽發 CA 目前的 CRL：金鑰 id（主鍵）、CRL 編號、thisUpdate、nextUpdate、DER | 無：公開資料 |
| `trustiot_migrate_applied` | migration Job 套用過的檔案，`certs_app` 只能 `SELECT` | 無 |

這裡不存任何私鑰：簽發 CA 私鑰在 Secret，根私鑰離線，閘道器的私鑰不離開 TPM。

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
| `tests/issuance.rs` | 驗收條件 1：要求別的 CN、SAN、serverAuth、codeSigning、各種 keyUsage 與 CA 的 CSR，簽出的都是 CA 決定的 profile，並能在根之下驗證為 TLS 用戶端憑證、不能當伺服器憑證或簽其他憑證；別的曲線或演算法、驗不過的簽章、改過的主體、過大或多餘的位元組都拒絕；代最大的有效簽發 CA 簽章；簽發紀錄寫入正確，`certs_app` 改不了（容器） |
| `tests/crl.rs` | 驗收條件 2：CRL 的內容，以及 TLS 伺服器依它拒絕撤銷的憑證、CRL 過期後拒絕全部；新 CRL 的 nextUpdate 減 thisUpdate 是 `crl.validity` 加往前的 5 分鐘；真的服務依排程為每個簽發 CA 重發 CRL 並由 `crl.pem` 發布；以 superuser 拿住工作鎖、把 nextUpdate 調近時 `certs.crl.expiring` 經 OTLP 變成 1 並記 ERROR，放開後工作重發、指標回到 0（容器） |
| `tests/key_custody.rs` | 驗收條件 3：程式檢查，只有列出並說明理由的位置能把私鑰變成位元組或文字；執行時，日誌開到 `trace`、開 OTLP、有等待中的金鑰、重讀與兩種壞掉的金鑰檔，日誌、端點與遙測都不含私鑰的任何片段（PKCS#8 涵蓋私鑰純量的任 24 個 base64 字元，其餘部分是演算法與公鑰，簽發 CA 憑證本來就公開；私鑰純量的十六進位、base64 與十進位位元組）（容器） |
| `tests/ceremony.rs` | 真的 `certs-ca`：`root create` 的私鑰只到 stdout、`root sign` 只從 stdin 讀、簽出的簽發 CA 服務接受並印出金鑰 id、`root check` 認得自己的私鑰；別的私鑰、P-256 私鑰、根的名稱、0 天、已存在的輸出檔都拒絕，且不印出私鑰；真的服務在儀式前就緒、`certs.ca.signing` 是 0、每次檢查記 WARN 與等待中的金鑰 id，掛上憑證後不重啟就變成 1 並發出第一張 CRL（容器） |
| `tests/cross_tenant.rs` | [crates/testkit](../../crates/testkit/README.md) 的覆蓋檢查：服務角色碰得到的每張表都要宣告；這裡的表都不是租戶資料而豁免（容器） |

chart 的部分（Secret、Role、掛載、values 的檢查）在 `deploy/chart/tests/chart.test.ts` 的 “gateway certificate authority”，init container 對假的 Kubernetes API 的行為在 `tools/migrate/tests/ca_keys.rs`，金鑰檔的格式與規則在 `crates/ca-keys` 的單元測試。
