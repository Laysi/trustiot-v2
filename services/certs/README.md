# certs（憑證服務）

自建 CA 的線上簽發 CA（決策 6、12；[部署與可搬移性](../../docs/architecture/deployment.md)的「憑證與佈建」）：以離線根 CA 簽出的簽發 CA 為閘道器簽 mTLS 用戶端憑證、為產線工作站簽工作站憑證、為 MQTT broker 簽 TLS 伺服器憑證，定期重發每個簽發 CA 的 CRL，給 MQTT broker 讀。

目前是 Laysi/trustiot-v2#78 的範圍（2026-10-21）：

- 服務骨架：自己的 schema `certs`、服務角色、簽發紀錄與 CRL 兩張表（沒有租戶資料，見[資料庫](#資料庫)）。
- CA 階層與私鑰保管（見下一節），以及根 CA 的離線儀式工具 `certs-ca`。
- 從 CSR 簽閘道器憑證的程式（`src/issue.rs`）：只取 CSR 的公鑰，CN、EKU 與效期由 CA 決定。出廠登錄（Laysi/trustiot-v2#79，已完成）與換發端點（Laysi/trustiot-v2#81）呼叫它，撤銷（Laysi/trustiot-v2#82）寫簽發紀錄的撤銷欄位。
- CRL 依排程重發、到期前告警，以及給 broker 的 `crl.pem` 與 `ca.pem`。broker 怎麼取用它們見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「MQTT broker」（Laysi/trustiot-v2#84）。

Laysi/trustiot-v2#534（2026-10-22）加上：

- 產線工作站憑證的 profile（broker 拒絕它），以及平台管理員簽發與撤銷它的 API（見[工作站憑證](#工作站憑證)）。這是 certs 第一個對外的 API。
- 簽發 CA 有了第一張 CRL 才簽章（見[發簽發 CA 憑證](#發簽發-ca-憑證)第 5 步）。

Laysi/trustiot-v2#557（2026-10-23）加上：

- MQTT broker 的 TLS 伺服器憑證的 profile（EKU 只有 serverAuth，不能當閘道器憑證）、閘道器驗證的名稱 `mqtt-broker.trustiot.internal`，以及只給 broker 的憑證 CronJob 的簽發端點，在自己的埠上（見[broker 伺服器憑證](#broker-伺服器憑證)）。私鑰在叢集內產生、取得與到期前換發的是 chart 的 CronJob `<release>-mqtt-broker-certificate`（Laysi/trustiot-v2#535，[deploy/chart/README.md](../../deploy/chart/README.md) 的「伺服器憑證」）。

Laysi/trustiot-v2#79（2026-10-23）加上：

- 出廠登錄 API（見[出廠登錄](#出廠登錄)）：產線工作站以工作站憑證簽請求、平台管理員登入，登錄閘道器的硬體 ID、TPM 金鑰與背書金鑰，取得第一張閘道器憑證；以平台角色 `certs_platform` 寫入，並發出全平台事件 `certs.gateway_registered`。certs 從此是事件的產生者。

Laysi/trustiot-v2#632（2026-10-24）加上：

- 消費資產服務的 `assets.gateway_bound`，維護閘道器目前綁定的租戶，以及租戶狀態的表（2026-10-30 起由身分服務的 `identity.tenant_created`、`identity.tenant_state_changed` 寫入，Laysi/trustiot-v2#778），和平台角色的跨租戶讀取介面，給換發（Laysi/trustiot-v2#81）與 broker 權限（Laysi/trustiot-v2#633）用（見[閘道器的綁定與租戶狀態](#閘道器的綁定與租戶狀態)）。certs 從此也是事件的消費端，也開始存租戶 ID（兩張照決策 29 的租戶表）。

Laysi/trustiot-v2#633（2026-10-29）加上：

- 以 broker 帳號 `service:certs` 連 MQTT broker 的服務埠，依 Postgres 冪等地同步 dynamic security 裡閘道器的 client 與角色：連線（含重連）時與定期做完整比對，出廠登錄與綁定之後立刻同步那一台（見[broker 權限（dynamic security）](#broker-權限dynamic-security)）。broker 重啟、dynsec 檔被 init container 重設之後，閘道器的權限由 certs 自動補回。

Laysi/trustiot-v2#81（2026-10-29）加上：

- 憑證換發端點 `POST /certs/renew`（見[換發](#換發)）：閘道器以 TPM 金鑰簽一次性挑戰，依登錄、目前的綁定與租戶狀態，為登錄的公鑰簽新憑證；限流、消費端落後時 503，成功寫租戶事件 `certs.gateway_certificate_renewed`，已登錄的硬體 ID 被拒時依時間窗合併成全平台事件 `certs.gateway_renewal_refused`。簽發 commit 之後也立刻同步那一台的 broker 權限。

Laysi/trustiot-v2#636（2026-10-30）加上：

- 平台管理員撤銷一台閘道器的 API `POST /api/certs/v1/gateways/{hardware_id}/revoke`（見[閘道器憑證的撤銷](#閘道器憑證的撤銷)）：撤銷那台閘道器每張還有效的憑證、立刻為它們的簽發 CA 重發 CRL、再拿掉 broker 的 client（斷開連線）。以 `key_compromise` 撤銷過的閘道器從此不能換發。撤銷後的 CRL 重發從此每個副本最多同時兩個（`crl::MAX_CONCURRENT_REISSUES`）。

## CA 階層與私鑰保管

2026-10-21 決定（Laysi/trustiot-v2#78）。

```text
離線根 CA（ECDSA P-384，預設 20 年，pathLen 1）
  └─ 簽發 CA，每一代一張（ECDSA P-256，預設 2 年、不超過根，pathLen 0）
       ├─ 閘道器用戶端憑證（ECDSA P-256 的 TPM 金鑰，預設 90 天、不超過簽發 CA）
       ├─ 產線工作站憑證（ECDSA P-256，預設 180 天、不超過簽發 CA；EKU 沒有 clientAuth，見「工作站憑證」）
       └─ MQTT broker 的伺服器憑證（ECDSA P-256，預設 30 天、不超過簽發 CA；EKU 只有 serverAuth，見「broker 伺服器憑證」；私鑰與換發在 chart 的憑證 CronJob，Laysi/trustiot-v2#535）
```

根 CA 在每次儀式另簽一張根 CRL，列出撤銷的簽發 CA，給閘道器驗 broker 的伺服器憑證用（2026-10-23 決定，見[閘道器對 broker 伺服器憑證的信任](#閘道器對-broker-伺服器憑證的信任)；閘道器端從檔案載入並檢查已在 Laysi/trustiot-v2#86，`certs-ca root crl` 簽發已在 Laysi/trustiot-v2#829，chart 帶入與 certs 採用已在 Laysi/trustiot-v2#830，出廠登錄時交給閘道器已在 Laysi/trustiot-v2#832，經 HTTPS 提供（`GET /certs/root-crl`，見[端點](#端點)）已在 Laysi/trustiot-v2#831）。

- **根 CA**：私鑰只在離線的機器上產生與使用，不進叢集、不進 git（加密的也不放）、不在任何連網主機；以 age 加密給兩位維運人員，存兩份離線媒體，各由一人保管（2026-10-22 決定；與 [docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「根 CA 私鑰」相同）。只在簽發 CA 的儀式與備份檢查時取出。根憑證是公開資料，放在 chart 的 values。
  - 展示環境例外（2026-10-28 決定，Laysi/trustiot-v2#619）：展示環境的根不做離線儀式，私鑰以權限 0600 的明文檔存在一位保管人的電腦上，仍不進叢集、不進 git（[docs/runbooks/demo-environment.md](../../docs/runbooks/demo-environment.md)「簽發 CA 的儀式」）。沒有專用的離線機器與第二位保管人、快閃媒體又不適合長期保存時，上面的做法執行不了，所以正式環境的保管方式與儀式在 Laysi/trustiot-v2#716 重新決定。
- **簽發 CA**：私鑰在叢集內產生，沒有人建立、看到、複製或輸入它，做法與內部身分簽章金鑰相同（[docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「閘道器憑證 CA」）：
  - chart 宣告空的 Secret `<release>-certs-ca-keys`。certs pod 的 init container `ca-keys`（migration 映像的 `migrate certs-ca-keys`，[tools/migrate](../../tools/migrate/README.md)）產生 P-256 金鑰，寫進這個 Secret 的 `issuing-ca-keys.json`（格式在 [crates/ca-keys](../../crates/ca-keys/README.md)），再把每把金鑰的 CSR 寫進 ConfigMap `<release>-certs-ca-requests`（`issuing-ca-generation-<代>.csr.pem`）。
  - 維運人員把 CSR 帶到離線的機器，以根 CA 簽出簽發 CA 憑證（`certs-ca root sign`），憑證（公開）放進 values 的 `certificateAuthority.issuingCertificates`。chart 把根憑證與簽發 CA 憑證寫進 ConfigMap `<release>-certs-ca-certificates`，服務每 30 秒重讀，不必重啟。重讀在阻塞執行緒上做，不占 async worker；某一次重讀 panic 時記 ERROR `periodic job panicked; it runs again next period`（`job` 是 `ca-reload`），下個週期照常重讀，而且全部重讀一次（[service-kit 的「每副本的週期性工作」](../../crates/service-kit/README.md#每副本的週期性工作periodic)）。
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

- 已出廠的閘道器拿不到新的根 CRL：簽發 CA 外洩時，broker 這一側靠「移除它的金鑰、broker 不再讀到它的 CRL」拒絕它簽的閘道器憑證（見[簽發 CA 金鑰遺失或外洩](#簽發-ca-金鑰遺失或外洩)）；閘道器這一側要靠根 CRL 才拒絕外洩的簽發 CA 簽的假伺服器憑證（2026-10-23 決定，見[閘道器對 broker 伺服器憑證的信任](#閘道器對-broker-伺服器憑證的信任)）。儀式已能以 `certs-ca root crl` 簽根 CRL（Laysi/trustiot-v2#829，見[發簽發 CA 憑證](#發簽發-ca-憑證)第 4 步），chart 帶入根 CRL、certs 檢查後採用並在剩不到 60 天時告警（Laysi/trustiot-v2#830），閘道器也能從檔案載入並檢查（Laysi/trustiot-v2#86），出廠登錄時也拿到 certs 當時採用的那一張（Laysi/trustiot-v2#832），certs 也已在 gateway 主機經 HTTPS 提供（Laysi/trustiot-v2#831，見[端點](#端點)）；但閘道器取用新的根 CRL（Laysi/trustiot-v2#563）還沒做，所以閘道器只有出廠時拿到的那一張（出廠時 certs 還沒採用根 CRL 的閘道器一張也沒有），之後才撤銷的簽發 CA 到不了已出廠的閘道器。這之前閘道器端的這個風險只能靠換根解除，而換根也還沒有做法（下一點）。
- 閘道器不檢查 broker 伺服器憑證本身的撤銷（它沒有簽發 CA 的 CRL，見同一節的「撤銷」）。broker 的私鑰外洩時，根 CRL 實作之後照[簽發 CA 金鑰遺失或外洩](#簽發-ca-金鑰遺失或外洩)最後一段，以根 CRL 撤銷簽出那張憑證的一代；在那之前，持有私鑰又站上連線路徑的攻擊者可以冒充 broker 到那張憑證過期為止，所以它的效期短（預設 30 天，見[broker 伺服器憑證](#broker-伺服器憑證)）。
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

還沒設定根憑證就啟用服務時（展示環境的做法，根在同一次離線作業建立，見 [docs/runbooks/demo-environment.md](../../docs/runbooks/demo-environment.md)「簽發 CA 的儀式」），服務載入不了 CA，只記 ERROR `cannot load the issuing CA` 與每輪的 WARN `no issuing CA is loaded, ...`，沒有 `waiting`。這時金鑰 id 改從 init container 的日誌讀：`kubectl -n <namespace> logs deployment/<release>-certs -c ca-keys | grep 'the issuing CA keys are in place'`，欄位 `keys` 是金鑰 id，`generations` 是對應的代。

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

同一次作業再以根 CA 簽一張新的根 CRL（規則在[根 CRL](#根-crl)）。第一次沒有上一張：

```sh
age --decrypt -i <自己的 identity> root-key.age \
  | certs-ca root crl --root-certificate root.pem --previous-crl none \
      --previous-certificates none --crl root-crl-1.pem
```

之後每次以上一張為輸入，CRL 編號加一：

```sh
age --decrypt -i <自己的 identity> root-key.age \
  | certs-ca root crl --root-certificate root.pem --previous-crl root-crl-<n>.pem \
      --previous-certificates revoked-issuing-cas.pem --crl root-crl-<n+1>.pem
```

這次要撤銷簽發 CA 時（[簽發 CA 金鑰遺失或外洩](#簽發-ca-金鑰遺失或外洩)），再加上 `--revoke <那一代的憑證> --reason <原因>`。

- 私鑰只從 stdin 進來，stdin 是終端機時拒絕執行；`--crl` 的檔案已存在時拒絕。`--previous-crl` 與 `--previous-certificates` 一定要給，沒有時寫 `none`：漏給上一張會讓編號從 1 重來、之前的撤銷全部消失。`--revoke` 的檔案裡沒有任何憑證時拒絕。
- **上一張要是最後簽的那一張**：工具看不出有沒有更新的根 CRL，所以它在 stderr 印出上一張的編號與 SHA-256，也印出新的那張的；兩者都抄進儀式紀錄，下一次核對印出的上一張就是紀錄裡最後簽的那一張。簽過的編號不再簽第二次：簽錯了也不要拿同一張上一張重簽，改以簽錯的那張為上一張再簽一張（新的編號），否則兩張同編號、內容不同的根 CRL 會分不出哪張有效。
- 工具確認上一張根 CRL 由這個根簽章（簽發者是根的主體、以根的公鑰驗得過）、只帶它自己寫的欄位、thisUpdate 不晚於這台機器的時間；`--previous-certificates` 與 `--revoke` 的每張憑證都要由這個根簽出、是 CA，不能是根自己；要撤銷的憑證不能已經被上一張撤銷，也不能過期超過 400 天。任何一項不符就拒絕、不寫檔。
- **項目何時拿掉**：CRL 只記序號，看不出憑證何時過期，所以上一張列的每一項都要在 `--previous-certificates` 給出那張憑證，少了就拒絕。憑證過期超過 400 天（新 CRL 的 thisUpdate 晚於它的 notAfter 加 400 天）的項目才拿掉，其餘照原本的撤銷時間與原因沿用。不在過期當下就拿掉，是因為閘道器的時鐘可能慢、被調回，或離線機器的時鐘可能快：那時閘道器仍認為那張憑證有效，提早拿掉就是提早解除撤銷；多留 400 天（一張根 CRL 的效期）蓋過這些誤差，項目很小，多留的代價不大。維運人員保存一個 `revoked-issuing-cas.pem`：每撤銷一張就把那張憑證接在後面（`cat issuing-ca-generation-<代>.pem >> revoked-issuing-cas.pem`，在簽之前或之後接都可以）。檔案裡過期超過 400 天的憑證會被略過，所以不必刪；其他上一張卻沒列、這次也沒有 `--revoke` 的憑證會被拒絕。還沒撤銷過任何簽發 CA 時寫 `--previous-certificates none`。這個檔案與每一張根 CRL 都是公開資料，和 `root.pem` 放在一起保存；遺失時從 values 的歷史版本（`certificateAuthority.issuingCertificates`）找回那幾張憑證。
- **原因**（`--reason`，寫進每一項的 reasonCode）：簽發 CA 的私鑰外洩或可能外洩是 `ca-compromise`；簽發 CA 沒外洩、只是要換掉它（例如 broker 伺服器憑證的私鑰外洩）是 `superseded`；提早停用是 `cessation-of-operation`；`unspecified` 不寫 reasonCode。閘道器不看原因，被列出就拒絕；原因只是紀錄。
- **時間**：thisUpdate 是執行時往前 1 小時（和簽發 CA 憑證的 notBefore 相同：離線機器的時鐘最不可靠，往前留給時鐘慢的叢集與閘道器），nextUpdate 是 thisUpdate 後 400 天。新撤銷的項目的撤銷時間是 thisUpdate。
- 簽出後工具再解析一次自己的輸出，確認符合「根 CRL」的「格式」並以根的公鑰驗得過，PEM 不超過閘道器 agent 讀的 64 KiB；不符就不寫檔。
- 它在 stderr 印出上一張與新的 CRL 的編號與 SHA-256、thisUpdate、nextUpdate、每一項（序號、主體、金鑰 id、撤銷時間、原因、到期）與拿掉的項目。**核對列出的就是要撤銷的那幾代、拿掉的確實已過期超過 400 天，再把檔案帶走。** 有新撤銷的項目時它提醒「根 CRL」的「順序」：broker 換上別一代簽的伺服器憑證之後，才把這張根 CRL 放進 values。

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
2. 照「[發簽發 CA 憑證](#發簽發-ca-憑證)」簽新的一代；外洩的那一張從 `issuingCertificates` 拿掉。同一次儀式（第 4 步）簽新的根 CRL 時加上 `--revoke <外洩的那一代的憑證> --reason ca-compromise`，再把那張憑證接到 `revoked-issuing-cas.pem`。根 CRL 撤銷的是憑證（序號），不是金鑰：外洩的金鑰若有不只一張憑證（例如同一份 CSR 簽過兩次），每一張都放進 `--revoke`，再核對 stderr 列出的金鑰 id 涵蓋外洩的那把金鑰的每一張憑證。照[根 CRL](#根-crl)的「順序」，broker 換上新一代簽的伺服器憑證之後，才把這張根 CRL 放進 values。
3. 閘道器經換發端點以 TPM 金鑰重新取得憑證（決策 12，Laysi/trustiot-v2#81），不必寄回。產線工作站由平台管理員重新簽發（[工作站憑證](#工作站憑證)的「換發」），外洩的簽發 CA 簽的工作站憑證已經不能鏈到 `ca.pem`。
4. 依 [docs/runbooks/secrets.md](../../docs/runbooks/secrets.md) 的「外洩處理」調查並開 `security` issue。

第 2 步簽的根 CRL 列出外洩的簽發 CA 憑證（[閘道器對 broker 伺服器憑證的信任](#閘道器對-broker-伺服器憑證的信任)）：閘道器取到它之後，就拒絕那個簽發 CA 簽的伺服器憑證。broker 伺服器憑證的私鑰外洩、簽發 CA 沒有外洩時也這樣做：照[輪替](#輪替)加一代、簽新的伺服器憑證，並在根 CRL 以 `--reason superseded` 撤銷簽出舊伺服器憑證的那一代，因為閘道器只看得到根 CRL。broker 不讀根 CRL，所以舊的一代簽的閘道器憑證照常有效，舊金鑰照輪替的時程移除即可。閘道器取到這張根 CRL 之前（certs 提供根 CRL 與閘道器取用新的根 CRL 還沒做，Laysi/trustiot-v2#560、Laysi/trustiot-v2#563），它仍然相信外洩的簽發 CA 簽的伺服器憑證，見[已知的限制](#ca-階層與私鑰保管)。

**遺失**：Secret 被刪除、清空，或從備份還原（還原的物件 uid 不同）時，init container 把它當作遺失，以目前的 `keyGeneration` 產生新的一組。服務不能簽章（`certs.ca.signing` 是 0），每次檢查記 WARN 與 `waiting`；舊的憑證因為對不上任何金鑰而被略過，舊的簽發 CA 不再發 CRL。照「發簽發 CA 憑證」簽新的金鑰（CN 要和舊的不同，或先把舊的憑證從清單拿掉），閘道器同樣經換發端點更新憑證。不沿用還原的金鑰（2026-10-22 決定），因為無法確定沒有別人拿過那份副本；代價是還原後到儀式完成、閘道器換發之前，閘道器連不上 broker。

## 閘道器的租戶歸屬與換發條件

規則在[架構決策](../../docs/architecture/decisions.md)的決策 6、12 與清單 B1、D2–D4（2026-10-24 決定，Laysi/trustiot-v2#638），這裡只摘要 certs 要做的事：

- 閘道器憑證只帶硬體 ID（`CN`），不帶租戶；綁定改變時憑證內容不變，簽發紀錄也不存租戶（見[資料庫](#資料庫)）。租戶一律是該硬體 ID 在資產服務目前的綁定：certs 消費 `assets.gateway_bound`（之後加上 Laysi/trustiot-v2#115 的解綁事件）維護自己的對照表，並消費身分服務的租戶事件，以事件收斂的本地狀態表記租戶狀態（清單 B1 的通知模式，Laysi/trustiot-v2#632、Laysi/trustiot-v2#778，見[閘道器的綁定與租戶狀態](#閘道器的綁定與租戶狀態)）；換發與 broker 權限都不同步呼叫資產服務或身分服務。
- **換發**（Laysi/trustiot-v2#81）：登錄有效（未除役）、閘道器目前有綁定、該租戶未停權才簽發。簽不簽由簽發當下的綁定與租戶狀態決定，不是把租戶寫進憑證。還沒綁定（在設備池裡）的閘道器不換發，出廠憑證到期後要等綁定才能換發。只有狀態是「正常」的租戶換發：停權、刪除、這個版本不認得的狀態，以及還沒有狀態紀錄的租戶都讀成停權（fail closed，2026-10-30 決定，Laysi/trustiot-v2#778）。停權之後的 broker 權限、撤銷與斷線是 Laysi/trustiot-v2#108。
- **broker 權限**（Laysi/trustiot-v2#633）：閘道器在 dynsec 有角色的條件是已出廠登錄、沒有除役、目前有綁定，而且至少有一張沒撤銷、沒過期的閘道器憑證；還沒綁定的閘道器連得上 broker，但不能發布或訂閱任何東西；解綁就拿掉角色。
- **解綁與改綁**：解綁時撤銷這台閘道器目前的憑證（Laysi/trustiot-v2#115，以 Laysi/trustiot-v2#636 的撤銷函式）。閘道器交給下一個租戶時一定換新的 SD 卡；新卡在閘道器綁到新租戶之後，以同一顆 TPM 經換發取得憑證。把舊卡裝回同一台硬體不在支援的流程內（見決策 12）。

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

做法（簽發是 `certs-ca root crl`，Laysi/trustiot-v2#829；其餘的實作在 Laysi/trustiot-v2#560 的其他子 issue 與 Laysi/trustiot-v2#563，這裡定的是規則）：

- **簽發**：每次儀式（第一次發簽發 CA、每年輪替、外洩）在離線的機器上以根 CA 簽一張新的根 CRL。它累積列出撤銷的簽發 CA 憑證，到那張憑證過期才拿掉；CRL 編號只增加；上一張根 CRL 是輸入，工具確認它由同一個根簽章，再加上這次撤銷的項目。正常輪替不撤銷舊的一代（它自然過期）；外洩，或 broker 伺服器憑證的私鑰外洩時，撤銷簽出它的簽發 CA。
  - 已實作（Laysi/trustiot-v2#829）：`certs-ca root crl`，指令與檢查在[發簽發 CA 憑證](#發簽發-ca-憑證)第 4 步。第一張的編號是 1（`--previous-crl none`），之後是上一張加一。CRL 只記序號，所以「過期才拿掉」靠維運人員一併提供上一張列的每張憑證（`--previous-certificates`，缺一張就拒絕），憑證過期超過 400 天才拿掉（閘道器時鐘慢或離線機器時鐘快時，過期當下拿掉等於提早解除撤銷；Laysi/trustiot-v2#829 的安全審查）；比較過由維運人員指定拿掉哪些序號，人工判斷容易拿錯，拿錯就是提早解除撤銷，所以不採用。每一項帶撤銷時間與原因（reasonCode：`ca-compromise`、`superseded`、`cessation-of-operation`，或不帶），閘道器不看原因，只當紀錄。thisUpdate 是簽發前 1 小時（離線機器的時鐘最不可靠，和簽發 CA 憑證的 notBefore 相同），nextUpdate 是 thisUpdate 後 400 天。
- **效期**：nextUpdate 是簽發後 400 天：儀式至少每年一次（簽發 CA 每年輪替），多一個月的餘裕。certs 服務在根 CRL 剩不到 60 天時告警，提醒該辦儀式。
- **發佈**：根 CRL 是公開資料，和根憑證一樣放在 values，certs 服務提供給閘道器。閘道器在出廠登錄時拿到當時的一張，之後定期經 HTTPS 取最新的：gateway 主機上不需要 cookie 的例外路徑 `GET /certs/root-crl`（決策 27，與換發端點同一類例外；2026-10-31 起，Laysi/trustiot-v2#831），回應的契約在[端點](#端點)。閘道器只接受由信任錨簽章、編號比手上大的根 CRL，存下來，重開機後沿用。
  - **chart 與載入**（Laysi/trustiot-v2#830）：`certificateAuthority.rootCrl` 是根 CRL 的 DER 的一行 base64，chart 把它印成 PEM `X509 CRL`，放進根憑證所在的 ConfigMap（`root-crl.pem`，沒設時是空檔）；chart 只確認它長得像 v2 CRL，憑證、常見格式的私鑰（PKCS#8、SEC 1、PKCS#1）與 PEM 文字渲染失敗（[deploy/chart/README.md](../../deploy/chart/README.md)「閘道器憑證 CA」）。certs 在重讀 CA 檔案的同一輪（`ca.reload_interval`，預設 30 秒）讀它，以當時載入的根憑證檢查，通過才採用：檢查與閘道器 agent 是同一份（[crates/root-crl](../../crates/root-crl/README.md)，下方「格式」與 agent 的「載入」），另外要有 CRL 編號（閘道器以它保留最新的一張）與 nextUpdate（告警要用）。所以 certs 交出去的根 CRL 都是 agent 接受的。kubelet 更新 ConfigMap 之後最多一輪就換上，不必重啟。換上的 PEM 是 certs 重新編成每行 64 字元的那份，比 agent 讀的上限（64 KiB）大時也不採用：檔案裡的 base64 寫成較長的行可以剛好在上限內，交出去的卻超過。
  - **只往前**：同一個根之下，編號比採用中的那張小、編號相同內容卻不同，或採用過之後檔案變成空的，都不採用（下一點）。這是 Helm 回滾（Flux 的 `remediation.strategy: rollback`）或改錯 values 會留下的狀態，照收就等於把剛撤銷的簽發 CA 又交給閘道器信任。換了根之後重新起算。採用過的那張只記在行程裡：certs 重新啟動後照檔案採用，跨重啟的保護與閘道器端「只接受編號更大的」一起在 Laysi/trustiot-v2#563 設計。
  - **不能採用時**（別的根簽的、格式不符、比採用中的舊、檔案讀不到）：記 ERROR `cannot adopt the root CRL in the CA certificates ConfigMap`（`problem`，`kept_number` 是保留的那張的編號），之後每次檢查（`crl.check_interval`）再記一次，`certs.root_crl.invalid` 是 1（見[CRL 與告警](#crl-與告警)）。保留上一張採用過、而且是以目前的根檢查過的根 CRL；換了根之後沒有這一張，就不提供根 CRL，直到放進新的根簽的一張。還沒採用過時，檔案是空的表示還沒有根 CRL，不是錯誤：不提供、不告警。`problem` 最多 1024 字元（`root_crl::MAX_PROBLEM_CHARS`），PEM 解析錯誤引用整行檔案時也不會讓每次檢查都記一行很長的日誌。
  - **交給閘道器的介面**：採用的根 CRL（PEM、編號、thisUpdate、nextUpdate）由 `authority::CaState::root_crl`（或與簽發 CA 同一份快照的 `authority_and_root_crl`）取得；經 HTTPS 提供在 `GET /certs/root-crl`（Laysi/trustiot-v2#831，見[端點](#端點)）。出廠登錄的回應以 `authority_and_root_crl` 的同一份快照帶 `root_crl`（Laysi/trustiot-v2#832，見[出廠登錄](#出廠登錄)的「請求與回應」），所以回應裡的根 CRL 一定是以同一個回應的 `root_certificate` 檢查過的；工作站把它送到閘道器、agent 寫進 `broker.crl`，只往前換（[edge/agent/README.md](../../edge/agent/README.md)「安裝憑證鏈與根憑證」）。
- **順序**：撤銷某一代的根 CRL 放進 values 之前，broker 要已換上新一代簽的伺服器憑證（Laysi/trustiot-v2#535 的換新），否則取到新 CRL 的閘道器連不上 broker。儀式可以同時簽出新的簽發 CA 憑證與根 CRL，根 CRL 晚一步放進 values。
- **過期**：nextUpdate 過了，閘道器仍然以手上的根 CRL 驗證、照常連線，並在狀態回報裡標出根 CRL 已過期。不擋連線，是因為根 CRL 要靠人到離線的機器簽，儀式晚了不該讓整個機隊斷線；改由 certs 的告警與閘道器的狀態回報讓人知道。certs 照樣採用並提供過期的根 CRL（閘道器手上的這張仍比沒有好），`certs.root_crl.expiring` 從剩不到 60 天起一直是 1，每次檢查記 WARN，直到換上新的一張。
- **還沒有根 CRL 時**（目前，以及後續 issue 完成前出廠的閘道器）：閘道器不檢查撤銷，照常連線；取到第一張之後開始檢查。
- **格式**（2026-10-23，Laysi/trustiot-v2#86 的安全審查；`certs-ca root crl` 照做，簽出後再解析一次自己的輸出確認，Laysi/trustiot-v2#829）：閘道器的 agent 拒絕不符的根 CRL、不啟動。`certs-ca` 簽的根 CRL 是 version 2，只帶兩個非 critical 的延伸欄位：authority key identifier（根的 subject key identifier）與 CRL 編號；簽發者與根的主體逐位元組相同。
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
4. keyUsage 有 digitalSignature，請求的簽章以憑證的公鑰驗證通過。簽的內容綁定請求本身（方法、路徑、本文的雜湊、時間與 nonce），不能重送；格式見[出廠登錄](#出廠登錄)的「工作站簽章」。
5. 簽發紀錄有這個序號、`profile` 是 `workstation`、內容相同、`revoked_at` 是 NULL。撤銷**立即**經由這一項生效；CRL 在撤銷回應之前重發（見下面的「撤銷」）。登錄 API 在 certs 服務裡，在登錄的交易中直接讀這張表。
6. 呼叫者同時以平台管理員帳號登入（決策 12）。

Laysi/trustiot-v2#79 照這個清單實作（`src/registrations.rs` 的 `verify_workstation` 與 `check_ledger`）。

### 簽發、撤銷與換發

前提：

- chart 的 `ingress.routes` 有 `{host: app, path: /api/certs, match: PathPrefix, service: certs}`（[deploy/chart/README.md](../../deploy/chart/README.md) 的「需要 cookie 的路由」），而且身分服務啟用：certs 以它的公鑰驗內部身分 token（`TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE`）。
- 呼叫者是平台管理員（`platform_admin`）。API 只接受入口 cookie 檢查簽出的 token：沒有或驗不過 401 `unauthenticated`；不是平台管理員，或是別的服務以 token 交換代為呼叫（token 帶 `act`）時 403 `permission_denied`。簽發與撤銷憑證是管理員自己的動作，不讓任何服務代為執行。
- 所有回應帶 `cache-control: no-store`；請求的 `Content-Type` 要是 `application/json`（跨來源的網頁因此要先過 CORS preflight，沒有任何東西回應它）。

工作站上以產線工作站工具做這些步驟（[factory/workstation](../../factory/workstation/README.md)，Laysi/trustiot-v2#614）：`init` 產生私鑰與 CSR，平台管理員在工作站上 `login` 之後 `enroll` 簽發並檢查、存放憑證。以下的 openssl 與 curl 步驟是沒有工具時的做法，也說明 API 本身。管理介面不在這個 issue 的範圍；在那之前以 HTTP 呼叫，帶平台管理員的 session cookie（`__Host-trustiot-session`，它是密鑰，不要留在 shell 歷史或檔案裡）。

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

`reason` 可省略（CRL 不寫原因），或是 `unspecified`、`key_compromise`、`affiliation_changed`、`superseded`、`cessation_of_operation`、`privilege_withdrawn`。200 回傳撤銷後的紀錄（`revoked_at`、`revocation_reason`）；序號不是工作站憑證（包括閘道器憑證的序號）時 404，已經撤銷時 409（`workstation_certificate.already_revoked`）。撤銷不能收回，原因也不能再改（見「資料庫」）：撤銷錯了，就為工作站簽一張新的。簽發紀錄立即標記，登錄 API 依它立即拒絕。撤銷的交易提交之後、回應之前，服務為那張憑證的簽發 CA 重發 CRL（Laysi/trustiot-v2#635，見[CRL 與告警](#crl-與告警)），所以 200 回來時 `crl.pem` 已經列出它。重發失敗時撤銷照樣成立、回應照樣是 200，服務記一行 ERROR，CRL 工作最晚一輪 `crl.check_interval`（預設 5 分鐘）之後列出它。broker 再過最多一個 `mqttBroker.trustInterval`（預設 30 秒）載入新的 CRL，之後拒絕這張憑證的連線。

**換發**：到期前由平台管理員再簽一張（最好在工作站上產生新的金鑰與 CSR），名稱相同；工作站換用新憑證之後，以 `superseded` 撤銷舊的。舊的在撤銷或到期前仍然有效。還沒有列出工作站憑證的 API，序號要從簽發的回應記下來。

每次簽發與撤銷記一行 INFO（`issued a workstation certificate`、`revoked a workstation certificate`），欄位有平台管理員的使用者 id、序號、簽發 CA 的金鑰 id 與工作站名稱，不含 CSR 或憑證；撤銷這一行在交易提交之後、重發 CRL 之前寫，重發的結果另外記（`issued a new CRL` 或 ERROR）；被拒絕的請求記 INFO `refused: not a platform administrator, ...`。

## 出廠登錄

2026-10-23 決定（Laysi/trustiot-v2#79，重建清單 D1，決策 12）。產線工作站為每台閘道器登錄一次：硬體 ID、TPM 金鑰的公鑰（以 CSR 送來，同時證明持有私鑰）與 TPM 背書金鑰（EK）的公鑰，取得閘道器的第一張憑證（profile 見[閘道器憑證](#閘道器憑證)）。程式在 `src/registrations.rs`，請求與回應的型別是 `contract::certs::RegisterGateway`、`GatewayRegistration`。

### 兩個身分，缺一不可

| 身分 | 來源 | 少了它 |
| --- | --- | --- |
| 平台管理員（A2） | 入口 cookie 檢查簽出的內部身分 token，規則同[工作站憑證](#工作站憑證)的「簽發、撤銷與換發」：沒有或驗不過 401，不是平台管理員或 token 帶 `act` 時 403 | 什麼都不登錄 |
| 產線工作站 | 請求以工作站憑證的私鑰簽章（下一節），憑證要通過「出廠登錄 API 可以依賴的欄位」的六項 | 403 `permission_denied`，鍵 `gateway_registration.workstation_refused`，`details.reason` 說明原因 |

平台管理員怎麼來（2026-10-23 決定）：Laysi/trustiot-v2#103 的 email 一次性驗證碼登入已經上線，所以沒有 MS1 專用的過渡做法。平台管理員帳號照 [services/identity/README.md](../../services/identity/README.md) 以擁有者角色寫入 `users`（`platform_admin = true`），正式建立流程在 Laysi/trustiot-v2#104；管理員在工作站上登入，取得 session cookie，請求經入口換成內部身分 token。工作站沒有自己的帳號，它的身分只來自工作站憑證。

工作站被拒時回 403 而不是 401：401 會讓前端當成 session 過期而要求重新登入，但問題在工作站，不在管理員的 session。`details.reason` 是 `missing`（少了標頭）、`malformed`（標頭重複或格式錯）、`untrusted`（不是載入的簽發 CA 簽的，或簽發紀錄沒有這張、內容不同）、`expired`、`not_workstation`（例如閘道器憑證）、`signature_invalid`、`stale`（時間差超過 5 分鐘）、`revoked`、`replayed`（nonce 用過）。

### 工作站簽章

Cloudflare 與 mesh 在服務之前終止 TLS，用戶端憑證到不了服務，所以由工作站簽請求。四個標頭各一個：

| 標頭 | 值 |
| --- | --- |
| `trustiot-workstation-certificate` | 工作站憑證，DER 的標準 base64（含 padding） |
| `trustiot-workstation-timestamp` | 簽章時間，Unix 秒，只有十進位數字；與服務的時鐘差不超過 300 秒 |
| `trustiot-workstation-nonce` | 22 到 64 個 base64url 字元，同一張憑證永不重複，例如 16 bytes 亂數的 base64url（不含 padding） |
| `trustiot-workstation-signature` | ECDSA P-256、SHA-256 的簽章（DER），標準 base64 |

簽的內容是下面六行，以 `\n` 分隔、最後沒有換行（`contract::certs::workstation_signing_input`）：`trustiot-workstation-request-v1`、方法（`POST`）、路徑與查詢字串（`/api/certs/v1/gateway-registrations`）、時間、nonce、本文 bytes 的 SHA-256（小寫十六進位）。簽章因此綁定這一個請求：換本文、路徑或時間都驗不過；完全相同的請求再送一次，nonce 已經記在資料庫（`certs.workstation_request_nonces`，過了時間的接受範圍就刪除），回 `replayed`。工作站的時鐘要校時。

工作站上以產線工作站工具登錄（[factory/workstation](../../factory/workstation/README.md) 的 `register`，Laysi/trustiot-v2#615）。工作站有了工作站憑證（`enroll`），平台管理員在工作站上 `login` 之後：

```sh
trustiot-workstation register --hardware-id <硬體 ID> --csr gateway.csr --ek ek.pem \
  --root-sha256 <根憑證的 SHA-256>
```

工具每次嘗試都以新的時間與 nonce 重新簽章；只在連線失敗、逾時與 502、503、504、Cloudflare 的 520 到 524 時重試（靠下一節的冪等保證：服務已提交、回應遺失時，重送得到 200 與同一筆登錄）；409、401、403、422 各自以固定的結束碼停止、不重試；200 或 201 先核對硬體 ID、兩個雜湊、憑證的 CN 與公鑰、`certificate_chain` 經簽發 CA 到 `root_certificate` 的簽章，以及 `--root-sha256` 釘住的根憑證（必填：`root_certificate` 是閘道器的唯一信任錨，指紋由平台管理員從儀式建立的根憑證算出交給產線），才記為登錄完成，結果記在工作站的 `registrations/<硬體 ID>.json` 與 `registrations.jsonl`。結束碼與紀錄的格式見工具的 README。閘道器的 CSR 與 EK 由工具經 ssh 在閘道器上執行 `trustiot-agent provision` 取得（`register --gateway`，Laysi/trustiot-v2#616），或以檔案給（`--csr`、`--ek`）。

### 請求與回應

本文是 JSON 物件，三個欄位都必填，不認得的欄位拒絕：

| 欄位 | 值 |
| --- | --- |
| `hardware_id` | 閘道器的 ID，也是它憑證的 CN 與 MQTT 身分：1 到 64 個 ASCII 英數字、`.`、`_`、`-`，以英數字開頭（`protocol::GatewayId`） |
| `certificate_request` | 閘道器在 TPM 裡產生的金鑰的 CSR，PEM；規則同閘道器憑證（ECDSA P-256、ecdsa-with-SHA256、DER 不超過 4096 bytes），CSR 的主體與延伸欄位一律忽略 |
| `endorsement_key` | TPM 背書金鑰的公鑰，PEM `PUBLIC KEY`（SubjectPublicKeyInfo），DER 不超過 2048 bytes：RSA 2048、3072 或 4096 位元，或 ECDSA P-256、P-384（TPM 2.0 的 EK 範本） |

| 情況 | 回應 |
| --- | --- |
| 新的硬體 ID | 201：簽出閘道器憑證、寫進簽發紀錄、寫入登錄，並寫一筆 `certs.gateway_registered` 事件，全在平台角色的同一個交易裡 |
| 同一個硬體 ID、同一把公鑰與 EK 再登錄 | 200：第一次的登錄與同一張憑證；除了 nonce 什麼都不寫，不簽新憑證、不發事件。工作站沒收到回應而重送時就是這樣 |
| 同一個硬體 ID、不同的公鑰或 EK | 409 `conflict`，`gateway_registration.public_key_mismatch` 或 `endorsement_key_mismatch` |
| 已除役的硬體 ID | 409，`gateway_registration.decommissioned`；除役的硬體 ID 目前不能再登錄（暫定，見[規則與取捨](#規則與取捨)） |
| 新的硬體 ID，公鑰已經登錄過，或 EK 屬於一台使用中的閘道器 | 409，`gateway_registration.public_key_in_use` 或 `endorsement_key_in_use` |
| 兩個相同的新登錄同時到達 | 一個 201、一個 200 與同一張憑證，只有一張憑證與一筆事件：同一個硬體 ID 的登錄排隊，後到的一個看到先到的結果（見[規則與取捨](#規則與取捨)） |
| 兩個新的硬體 ID 同時以同一把公鑰或同一個 EK 登錄 | 一個 201、一個 409 `public_key_in_use` 或 `endorsement_key_in_use`，與先後到達時相同：後寫入的一個撞到唯一鍵，交易整個回滾，重試一次就看到先到的登錄 |

回應的欄位：`hardware_id`、`public_key_sha256`、`endorsement_key_sha256`（DER 的 SHA-256，小寫十六進位）、`workstation`（登錄它的工作站名稱）、`registered_at`、`serial`、`ca_key_id`、`not_before`、`not_after`、PEM 的 `certificate`、PEM 的 `certificate_chain`，以及 PEM 的 `root_certificate`（根 CA 憑證，閘道器驗 broker 伺服器憑證的唯一信任錨，見[閘道器對 broker 伺服器憑證的信任](#閘道器對-broker-伺服器憑證的信任)；新登錄與重送都回目前的根憑證），以及 `root_crl`（Laysi/trustiot-v2#832）。

`root_crl` 是 certs 目前採用的根 CRL（[根 CRL](#根-crl)）：一個 PEM `X509 CRL` 區塊、每行 64 字元，由同一個回應的 `root_certificate` 簽章（與根憑證取自同一份快照，採用時就以這個根檢查過），有 CRL 編號，格式是閘道器 agent 的 `broker.crl` 接受的。還沒採用任何根 CRL 時（例如 values 還沒設 `certificateAuthority.rootCrl`，或換了根之後還沒放進新根簽的一張），回應**沒有這個欄位**（不是 `null`），其他欄位照舊；閘道器在拿到根 CRL 之前不檢查撤銷。新登錄與重送都回當時採用的那一張，所以重送的 200 除了 `root_crl` 之外與第一次的登錄相同，`root_crl` 則可能比第一次的新。工作站記下時以同一份檢查（`crates/root-crl`）確認它由 `--root-sha256` 釘住的根簽章、有編號，不符就不記為登錄完成（[factory/workstation/README.md](../../factory/workstation/README.md)）。

`certificate_chain`（2026-10-23，Laysi/trustiot-v2#610）是閘道器當 MQTT 用戶端憑證送出的鏈：先是 `certificate`，再接簽出它的簽發 CA 憑證，不含根。閘道器 agent 的 `identity.certificate` 就是這個格式（[edge/agent/README.md](../../edge/agent/README.md)），工作站可以在送到閘道器之前檢查 leaf → 簽發 CA → `root_certificate`。新登錄放的是剛簽章的簽發 CA；重送以登錄的 `ca_key_id` 從已載入的簽發 CA 找，所以[輪替](#輪替)期間載入兩把時，放的是簽出這張憑證的那一把，不是現在簽章的那一把。重送時那把簽發 CA 已經不載入（輪替第 3 步移除之後，它簽的憑證理應都過期了）：`certificate_chain` 只有 `certificate`，記一行 WARN `the issuing CA of a repeated gateway registration is no longer loaded ...`，欄位只有 `ca_key_id`；回應仍是 200 與第一次的登錄，照[規則與取捨](#規則與取捨)的「重送回原本的憑證」。

其他錯誤照 M1 格式、依序檢查：`Content-Type` 不是 JSON、本文超過 16 KiB 或不是 JSON 物件 400（`gateway_registration.body_too_large`、`body_invalid`）；還沒載入簽發 CA、無法檢查工作站時 503（`gateway_registration.ca_not_ready`）；工作站被拒 403；欄位問題 422，`field_errors` 指出每一個（鍵 `gateway_registration.unknown_field`、`field_required`、`hardware_id_invalid`、`request_invalid`、`request_unsupported`、`request_signature_invalid`、`endorsement_key_invalid`、`endorsement_key_unsupported`）；現在沒有能簽章的簽發 CA 或資料庫失敗 503，什麼都沒登錄。所有回應帶 `cache-control: no-store`。

### 規則與取捨

- **硬體 ID 只登錄一次，除役後目前也不能再登錄（暫定，2026-10-23）**：硬體 ID 是憑證的 CN、MQTT 使用者名稱與事件的 aggregate id，重用會讓舊憑證、舊紀錄與新裝置混在一起，所以現在從嚴。換主機板或 TPM 就是一台新的閘道器、新的硬體 ID。登錄的列不能刪除或改寫，對所有角色都一樣（trigger `gateway_registrations_guard`，見[資料庫](#資料庫)）；唯一能做的改變是設定一次 `decommissioned_at`，除役 API 是 Laysi/trustiot-v2#115。除役本身不可逆；但除役的硬體 ID 能不能再登錄（例如維修後當作一筆新的登錄），與資產服務一起在 Laysi/trustiot-v2#115 決定。要放寬就需要新的 migration：登錄表的主鍵是硬體 ID，`certs.gateway_registered` 事件的 aggregate id 也是硬體 ID。
- **公鑰永不登錄兩次**；**EK 只在使用中的閘道器之間唯一**：除役閘道器的 TPM 可以用新的硬體 ID 再登錄。
- **重送回原本的憑證**：即使那張憑證之後被撤銷或過期，重送仍回同一筆登錄與同一張憑證；新憑證走換發（Laysi/trustiot-v2#81），不走登錄。
- **EK 只記錄、比對，不驗證**：服務檢查 EK 的格式與演算法，但不驗證 TPM 製造商的 EK 憑證鏈，也不做 TPM 的 credential activation；信任來自產線工作站與平台管理員兩個身分。要證明 CSR 的金鑰真的在那顆 TPM 裡，是之後的工作。
- **撤銷與登錄不會交錯**：登錄從讀工作站的簽發紀錄到 commit 都持有那張憑證的共享 advisory lock（`issue::LOCK_WORKSTATION_SHARED`），撤銷工作站憑證先取它的排他鎖。所以先 commit 的撤銷一定被登錄讀到；已經讀到「有效」的登錄，會在撤銷之前 commit。用 advisory lock 而不是列鎖，是因為列鎖需要平台角色沒有的 `UPDATE` 權限。
- **同一個硬體 ID 的登錄排隊**（2026-10-30，Laysi/trustiot-v2#782）：登錄讀完工作站的簽發紀錄之後、寫 nonce 與以硬體 ID 查既有登錄之前，先取以硬體 ID 為鍵、到交易結束的 advisory lock（`registrations::LOCK_REGISTRATION`）。交易是 READ COMMITTED，每個查詢看得到它開始之前已經 commit 的資料；沒有這個鎖時，兩個相同的登錄同時到達，後到的一個可能查硬體 ID 時還看不到先到的一個，查公鑰時卻已經看到它 commit，把重送答成 409 `public_key_in_use`，工作站依 runbook 就會停下來。排隊之後，後到的一個查硬體 ID 時就看到先到的結果，回 200 與同一張憑證。選鎖而不是在金鑰衝突時再查一次硬體 ID：鎖讓同一個硬體 ID 的整串檢查不會交錯，不必逐一找出查詢之間的時間窗。不同硬體 ID 的登錄不會因為這個鎖互相等（硬體 ID 的雜湊碰撞時才會）；排隊中的登錄還沒寫 nonce、也還沒刪過期的 nonce，不會拿著別的登錄要等的列鎖，它們之間的公鑰或 EK 衝突由唯一鍵與一次重試處理。滾動更新期間，舊版副本不取這個鎖，它與新版副本同時登錄同一個硬體 ID 時仍可能遇到原本的 409。
- **readiness 不看平台角色**：平台角色的連線池（每個副本最多 4 條）是 lazy 的，登不進去時只有登錄回 503，CRL 與 `ca.pem` 照常提供給 broker。

### 事件與紀錄

`certs.gateway_registered`（`contract::events::certs::GatewayRegistered`，版本 1）：全平台事件（`tenant_id` 是 null），aggregate 是 `gateway_registration`、id 是硬體 ID，actor 是平台管理員；payload 有 `hardware_id`、`public_key_sha256`、`endorsement_key_sha256`、`certificate_serial`、`workstation`。只有產生者的平台角色能寫全平台事件，所以登錄整個以 `certs_platform` 執行。身分服務的事件紀錄消費它，記在全平台的事件紀錄（`services/identity/README.md`）；資產服務消費它，把閘道器放進設備池（Laysi/trustiot-v2#101，`services/assets/README.md` 的「閘道器設備池與綁定」）。

每次新登錄記一行 INFO `registered a gateway`（平台管理員的使用者 id、工作站、硬體 ID、序號、簽發 CA 的金鑰 id、到期時間），重送記 `a gateway registration repeated an existing one`，工作站被拒記 `refused a gateway registration: ...` 與原因；都不含 CSR、金鑰或憑證。平台角色的每個交易另有 service-kit 的 `platform database access`（用途 `gateway-registration`）。

## 閘道器的綁定與租戶狀態

2026-10-24 起（Laysi/trustiot-v2#632，`src/bindings.rs`）。閘道器憑證只帶硬體 ID、不帶租戶；閘道器屬於哪個租戶一律是資產服務「目前的綁定」，各服務以事件維護自己的對照表（決策 12，見[閘道器的租戶歸屬與換發條件](#閘道器的租戶歸屬與換發條件)）。certs 的對照表給換發（Laysi/trustiot-v2#81）、broker 權限（Laysi/trustiot-v2#633）與停權（Laysi/trustiot-v2#108）讀，它們的請求路徑上不必呼叫其他服務。租戶狀態 2026-10-30 起來自身分服務的事件（Laysi/trustiot-v2#778，`src/tenant_states.rs`，見[租戶狀態](#租戶狀態)）。

### 消費的事件

- 訂閱 `certs.assets-events`（`SUBSCRIPTION`，永遠不改名），讀資產服務的 outbox。`tools/migrate/src/registry.rs` 把 certs 列為 `assets` 的消費端，migration Job 因此讓 `certs_platform` 讀 `assets.outbox_event`，並在 `certs` 建立消費端的四張表。訂閱從頭讀，所以這個版本之前的綁定也會補上。租戶狀態有自己的訂閱，見[租戶狀態](#租戶狀態)。
- `assets.gateway_bound`（`contract::events::assets::GatewayBound`）：消費端在事件的租戶交易裡以 `certs_app` 寫 `gateway_bindings` 一列（租戶、綁定 ID、硬體 ID、事件的 `occurred_at`、事件在資產服務 log 的位置）。主鍵（租戶、綁定 ID）已經有這一列時什麼都不寫，所以同一個事件送兩次、整個訂閱重播都只有一列、內容不變。每次處理記一行 INFO `applied assets.gateway_bound`（硬體 ID、綁定 ID、租戶、事件 ID、`created`）。
- 錯誤分類比照資產服務的設備池消費端：

| 情況 | 處理 |
| --- | --- |
| 硬體 ID 或綁定 ID 不合規則（解碼失敗）、沒有租戶（全平台事件）、aggregate 不是該綁定的 `gateway_binding`、表以 SQLSTATE 22xxx 或 23514 拒絕的值 | 每次重試都會失敗：進 dead letter，訂閱繼續處理之後的事件 |
| 其他資料庫錯誤（連線、逾時、序列化衝突等） | 暫時的：退避後重試同一個事件 |
| aggregate 是 `gateway_binding`、但這個版本不認得的事件（例如 Laysi/trustiot-v2#115 的解綁事件，在資產服務先部署時） | 重試，不略過：綁定的事件會改變目前的綁定，略過就永遠遺失，閘道器會一直留在舊的租戶。訂閱停在這個事件，lag 告警會顯示，直到認得它的 certs 版本上線 |
| 資產服務其他 aggregate 的事件（設備型號、設備） | 略過：與綁定無關 |

### 目前的綁定：以事件在 log 的順序判斷新舊

一台閘道器目前的綁定，是它在資產服務 log 裡位置最後（`position_tx`、`position_id`，即 `crates/outbox` 的 `Position`）的那個綁定，而且沒有 `unbound_at`。不用時間判斷：資產服務要等前一個綁定結束（解綁，Laysi/trustiot-v2#115）才建立新的綁定，新綁定的交易在前一個綁定 commit 之後才寫入，它的事件在 log 裡一定排在後面；`occurred_at` 來自各副本的時鐘，不保證這個順序。這依賴資產服務的一個不變式：綁定交易的第一個寫入必須是 `gateway_bindings` 的 `INSERT ... ON CONFLICT`，交易在它等完前一個綁定之後才取得交易 ID，事件的位置（`tx_order`）就是這個 ID（`services/assets/src/gateways/binding.rs` 的註解）。較舊的綁定晚到時（例如修好後重播的 dead letter）照樣存下，但不會變成目前的綁定。`bound_at` 只供讀取。

表沒有「每台閘道器一列」的唯一限制，也不在處理時更新別的列：消費端在事件的租戶交易裡，RLS 看不到、也改不了別的租戶的列，閘道器換租戶時新舊綁定在不同租戶。所以一個綁定一列，誰是目前的綁定在讀取時由平台角色決定。

解綁（Laysi/trustiot-v2#115）之後會以自己的事件，在同一個租戶（綁定的 aggregate）設定該列的 `unbound_at`；欄位已經在表上，讀取也已經把結束的綁定當作沒有綁定，不必遷移資料。那時再給 `certs_app` 更新這一欄的權限。`unbound_at` 不與 `bound_at` 比較（表上沒有這個 CHECK）：兩個時間來自不同副本的時鐘，順序以 log 為準，時鐘偏差不能讓解綁被拒、進 dead letter。

### 租戶狀態

2026-10-30 起（Laysi/trustiot-v2#778，`src/tenant_states.rs`）由身分服務的事件寫入，租戶的狀態機在身分服務（Laysi/trustiot-v2#776，`services/identity/README.md`）。

`tenant_statuses` 每個租戶一列：狀態（`active`、`suspended`、`deleted` 或 `unrecognized`）與版本號。寫入只有 `store_tenant_status`：版本號比已存的大才覆蓋（清單 B1 的通知模式），所以晚到或重播的舊狀態不會蓋掉新的，同時寫同一個租戶的交易會排隊再比較；它要在該租戶的交易裡執行，寫別的租戶被 RLS 以 42501 拒絕。另有兩條規則，寫在同一個語句裡：

- `deleted` 是終態：已存 `deleted` 的列不再被任何狀態覆蓋，版本號再大也一樣。身分服務的狀態機本來就不離開刪除，這裡再擋一次，事件有誤也不會讓刪除的租戶恢復換發。
- `unrecognized`（這個版本不認得的狀態）可以被同一個版本號的已知狀態取代：舊版本存下不認得的狀態，升級到認得它的版本之後重播訂閱，同一個事件就寫入正確的狀態。

**讀成停權**（`bindings::reads_as_suspended`）：只有存下 `active` 的租戶不是停權；`suspended`、`deleted`、`unrecognized` 與沒有紀錄的租戶都讀成停權（fail closed，2026-10-30 決定）。綁定只發生在租戶建立之後，兩個事件在不同的訂閱：`identity.tenant_created` 還沒處理時，新租戶的閘道器換發會得到 403 `tenant_suspended`，退避重試後成功。

#### 消費的租戶事件

- 訂閱 `certs.identity-events`（`tenant_states::SUBSCRIPTION`，永遠不改名），讀身分服務的 outbox。`tools/migrate/src/registry.rs` 把 certs 也列為 `identity` 的消費端（`consumes: &["assets", "identity"]`），migration Job 因此讓 `certs_platform` 讀 `identity.outbox_event`（只有 `SELECT`，`tools/migrate/tests/fresh_database.rs` 檢查）。訂閱從頭讀，所以這個版本之前建立的租戶與它們的狀態也會補上；整個訂閱重播（`Replay::Reprocess`）得到同樣的表。
- `identity.tenant_created`（`contract::events::identity::TenantCreated`）：狀態 `active`、版本 1。
- `identity.tenant_state_changed`（`TenantStateChanged`）：payload 的 `state` 與 `state_version`（第一次變更是 2，之後每次加一）。不存 `reason`：換發只看是否停權。
- 兩者都在事件的租戶交易裡以 `certs_app` 呼叫 `store_tenant_status`。租戶取自 envelope（身分服務寫入時 RLS 已檢查），不取自 payload。每次處理記一行 INFO `applied a tenant state`（`tenant_id`、`status`、`version`、`event_type`、`event_id`、`stored`；`stored` 是 `false` 表示版本不比已存的新）。
- 身分服務的其他事件（成員、使用者、AI 模型角色）略過：租戶的狀態只經 `identity.tenant_state_changed` 改變（`crates/contract`）。aggregate 是 `tenant`、但這個版本不認得的事件類型也略過（不重試：重試會讓任何無害的新租戶事件擋住全平台的換發），但記一行 WARN `skipped an unknown event of a tenant`（`event_type`、`event_version`、`tenant_id`、`event_id`）；身分服務在租戶 aggregate 加新的事件類型時，要檢查這個消費端。

錯誤分類與綁定的消費端不同，處理租戶事件的任何失敗都**重試，處理常式不送進 dead letter**（`crates/outbox` 本身仍會把解不開的信封、信封與 log 的租戶不一致、連續 panic 三次的事件送進 dead letter，那時那次變更就遺失了：`certs.outbox_dead_letter` 裡訂閱 `certs.identity-events` 的任何一列都要告警處理）。反過來，一個永遠處理不了的租戶事件會讓全平台的換發都回 503，直到有人處理：升級到處理得了它的版本，或以 `store_tenant_status` 在該租戶的交易寫入正確的狀態，再以 `outbox::replay_from` 從那個事件之後接著處理。lag（`outbox.consumer.lag.age`，訂閱 `certs.identity-events`）也要告警。

| 情況 | 處理 |
| --- | --- |
| 這個版本不認得的事件版本（身分服務先部署）、解碼失敗、沒有租戶、aggregate 不是該租戶的 `tenant`、`state_version` 小於 2 的變更、資料庫錯誤 | 重試同一個事件，不略過：略過或進 dead letter 都可能遺失一次停權，租戶會一直讀成正常。訂閱停在這個事件，lag 增加，換發隨即回 503（[檢查順序與錯誤](#檢查順序與錯誤)第 3 列），直到修好或認得它的版本上線（測試：`tests/tenant_state_lag.rs`） |
| 同一個事件版本裡這個版本不認得的 `state` 值（`TenantState::Unrecognized`） | 存成 `unrecognized`，讀成停權；認得它的版本重播時以同一個版本號改寫 |

租戶狀態改變時不觸發 broker 權限的同步，也不撤銷憑證：停權之後的 dynsec 期望狀態、撤銷與斷線是 Laysi/trustiot-v2#108。這裡只讓不是正常的租戶換發不到新憑證。

**降版**：降回不認得 `deleted`、`unrecognized` 的版本時，舊版本把這兩種值當成壞掉的列，換發回 503 而不簽發。但舊版本把沒有紀錄的租戶讀成正常，也沒有身分服務事件的消費端，表停在降版當下：降版期間停權或刪除的租戶仍讀成正常、照樣換發，直到再升級（訂閱從停下的位置接著處理）。所以降版期間避免停權，或盡快升回。

### 讀取介面

`Projection`（以平台角色 `certs_platform` 的連線池建立）：

| 方法 | 給誰 | 回傳 |
| --- | --- | --- |
| `gateway_tenant(hardware_id, purpose)` | 需要單獨讀一台閘道器的綁定時（換發改用一個 JOIN，見下方） | 目前的綁定（租戶、綁定 ID、硬體 ID、`bound_at`）；從沒綁定過或最後的綁定已結束時是 `None` |
| `tenant_suspended(tenant_id, purpose)` | 停權（Laysi/trustiot-v2#108） | 是否讀成停權：只有存下 `active` 是 `false`，沒有紀錄、`suspended`、`deleted`、`unrecognized` 都是 `true`；表裡有其他值時回錯誤 |
| `bound_gateways(purpose)` | broker 權限的完整比對（Laysi/trustiot-v2#633） | 所有租戶目前已綁定的閘道器，依硬體 ID 的位元組順序 |

這三個讀取都不在請求者的租戶交易裡：換發是沒有使用者身分的公開請求，同步要看全部租戶。所以是平台角色的跨租戶讀取：

- 兩張表的 `platform_access` policy 只給 `certs_platform`、只限 `SELECT`，它也只有這兩張表的 `SELECT`；查詢只取上表的欄位。
- 每次讀取是一筆自己的 `PlatformDb::begin` 交易，第一個語句就是 `SET TRANSACTION READ ONLY`，資料庫拒絕其中的任何寫入。`PlatformDb::begin` 每筆交易記一行 INFO `platform database access`，帶 actor（`service`、`certs`）與呼叫端給的 `purpose`（例如 `renew-gateway-certificate`），所以每次讀取都在稽核日誌裡。不用 `PlatformDb::reader`：它只在建立時記一次 INFO、之後每筆交易只記 DEBUG，適合每秒輪詢的消費端；這三個讀取頻率低（一台閘道器幾十天換發一次、同步定期一次），每次都要留下紀錄。
- 租戶交易讀不到別的租戶：`certs_app` 對 `gateway_bindings` 只有 `INSERT`（`ON CONFLICT DO NOTHING` 不指定衝突目標就不需要 `SELECT`），讀不到任何綁定；`tenant_statuses` 的 `tenant_isolation` 只讓它讀寫交易的租戶那一列。
- 平台角色的連線池（`PLATFORM_CONNECTIONS`，每個副本 4 條）由出廠登錄、兩個消費端（綁定與租戶狀態，Laysi/trustiot-v2#778 加上第二個）的輪詢與 lag 量測、以及這三個讀取共用，都是短交易；沒有新增連線池，chart 的連線預算不變。換發（Laysi/trustiot-v2#81）不呼叫這三個方法，而以同一種交易（`Projection::begin`：唯讀、同樣的上限與稽核紀錄）執行一個 JOIN，一次讀出登錄、目前的綁定與租戶狀態（見[換發](#換發)的「權限」）。
- 每個讀取交易在伺服器端設上限（`SET LOCAL`）：`statement_timeout` 5 秒（`READ_STATEMENT_TIMEOUT`）、`lock_timeout` 2 秒（`READ_LOCK_TIMEOUT`，例如等 migration 的 `ALTER TABLE`）。慢的查詢不會長時間佔住共用池的連線，超過時 Postgres 取消它，讀取回錯誤。

## 換發

2026-10-29 起（Laysi/trustiot-v2#81，清單 D2，決策 12；`src/renewal.rs`）。憑證快到期、已經過期或被撤銷的閘道器，經 gateway 主機上的公開路徑 `/certs/renew`（443、只驗伺服器憑證，chart 的 `ingress.publicPaths.certRenewal`）以 TPM 金鑰證明自己，取得新的閘道器憑證。端點沒有 cookie、沒有使用者身分（決策 27 的明列例外），在簽章驗過之前什麼都不信任，所以它本身就是對外的安全關卡。線上格式（請求、回應、要簽的內容、簽章與 nonce 的寫法）在 [crates/protocol](../../crates/protocol/README.md) 的「憑證換發」，閘道器 agent 的換發排程（Laysi/trustiot-v2#83）照那一節與本節實作。

### 流程

兩步都是 `POST /certs/renew`，以本文的 `step` 區分：

1. **取挑戰**（`step: challenge`）：服務產生 32 bytes 的隨機 nonce，與硬體 ID、用途（`cert-renewal`）與到期時間（資料庫時鐘的現在加 30 秒，Unix 秒）存進 `certs.renewal_challenges`，回 200 與這四個欄位。只檢查硬體 ID 的格式，不查登錄、綁定或除役：不論硬體 ID 存不存在，回應都一樣，不提供列舉閘道器的管道。
2. **兌換**（`step: renew`）：閘道器以 TPM 金鑰簽挑戰的內容（`protocol::renewal::signing_input`），送回硬體 ID、nonce 與簽章。服務依序：
   1. 以 nonce 刪除挑戰（`DELETE ... RETURNING`，自己一個語句、立刻 commit）：之後不論結果如何，這個挑戰都不能再用。找不到 409。
   2. 挑戰是發給別的硬體 ID 或別的用途：403。已過期（資料庫時鐘）：410。
   3. 以平台角色的唯讀交易執行一個 JOIN，讀出登錄的公鑰與除役時間、這台閘道器在資產服務 log 裡最後的綁定（是否仍有效）與該租戶的狀態，記一行稽核日誌（見「紀錄」）。
   4. 沒有登錄：401，與簽章不對相同。用**登錄的**公鑰、以**存下的**挑戰欄位重組要簽的內容，驗 ECDSA P-256、SHA-256 簽章：不對 401。
   5. 已除役 403；沒有有效的綁定 403；租戶不是正常（停權、刪除、不認得的狀態或沒有紀錄）403。簽章驗過之後才看閘道器的狀態，所以這些原因只回給持有 TPM 金鑰的閘道器。
   6. 以 `certs_app` 在閘道器租戶的交易裡：為**登錄的公鑰**簽閘道器憑證（profile 同[閘道器憑證](#閘道器憑證)，不接受 CSR，換 TPM 是新的硬體 ID）、寫簽發紀錄、寫租戶事件 `certs.gateway_certificate_renewed`，一起 commit；回 201。commit 之後請 broker 權限的同步只比對這一台（見「規則與取捨」的「broker 角色」）。

回應的欄位與出廠登錄相同：`hardware_id`、`serial`、`ca_key_id`、`not_before`、`not_after`、`certificate`、`certificate_chain`（憑證再接簽出它的簽發 CA）與 `root_certificate`（`protocol::renewal::RenewedCertificate`）。

換發不看閘道器現有的憑證：請求不帶憑證、端點不經 mTLS，所以目前的憑證已過期或已撤銷都不影響換發（測試撤銷了閘道器唯一的憑證之後換發）；例外是以 keyCompromise 撤銷過的閘道器，它的登錄金鑰不再可信，從此不能換發（Laysi/trustiot-v2#636，見[閘道器憑證的撤銷](#閘道器憑證的撤銷)的「撤銷與換發」）。舊憑證**不撤銷**，留到自然到期（不做 `superseded` 撤銷）：換發後閘道器可能還沒換上新憑證（寫檔失敗、重開機），舊憑證仍要能連 broker；要讓舊憑證立刻失效的情況（解綁、除役、外洩）由撤銷處理（Laysi/trustiot-v2#636、Laysi/trustiot-v2#115）。

### 檢查順序與錯誤

所有回應都帶 `cache-control: no-store`，錯誤照 M1 格式（`contract::ErrorBody`）。每個請求依序檢查，前一項不過就不做後面的：

| 順序 | 情況 | 狀態 | `code` | `message_key`（`certificate_renewal.*`） | 日誌與指標的 `reason` | 計入時間窗 |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | `Content-Type` 不是 JSON、本文超過 1 KiB（`details.max_bytes`）或不是 JSON 物件 | 400 | `invalid_request` | 無、`body_too_large`、`body_invalid` | `invalid_request` | 否 |
| 1 | 欄位缺少或格式不對（`field_errors` 指出每一個） | 422 | `validation_failed` | `field_required`、`step_invalid`、`hardware_id_invalid`、`nonce_invalid`、`signature_malformed` | `invalid_request` | 否 |
| 2 | 超過這一步的全域限流（取挑戰、兌換各一道，所有硬體 ID 一起算） | 429，`Retry-After`：要等的秒數 | `rate_limited` | 無 | `rate_limited` | 否 |
| 3 | 任一個消費端（綁定的 `certs.assets-events`、租戶狀態的 `certs.identity-events`）的 lag 不明，或最舊的未處理事件超過 `renewal.max_consumer_lag` | 503，`Retry-After: 30` | `unavailable` | `bindings_stale` | `bindings_stale` | 否 |
| 4 | 還沒載入簽發 CA | 503，`Retry-After: 60` | `unavailable` | `ca_not_ready` | `ca_not_ready` | 否 |
| 5 | 2 秒內輪不到資料庫工作（`renewal.max_concurrent`） | 503，`Retry-After: 1` | `unavailable` | 無 | `busy` | 否 |
| 6 | 挑戰不存在：沒發過、用過、過期超過 120 秒已清除 | 409 | `conflict` | `challenge_unknown` | `challenge_unknown` | 是 |
| 6 | 挑戰是別的硬體 ID 或別的用途的 | 403 | `permission_denied` | `challenge_mismatch` | `challenge_mismatch` | 是 |
| 6 | 挑戰過期 | 410 | `conflict` | `challenge_expired` | `challenge_expired` | 是 |
| 6 | 簽章不對，或硬體 ID 沒有登錄 | 401 | `unauthenticated` | `signature_refused` | `signature_invalid` | 是（沒登錄的硬體 ID 不寫入） |
| 6 | 簽章驗過，但超過這個硬體 ID 自己的限流（只計簽章驗過的兌換，Laysi/trustiot-v2#801） | 429，`Retry-After`：要等的秒數 | `rate_limited` | 無 | `gateway_rate_limited` | 是 |
| 6 | 已除役 | 403 | `permission_denied` | `decommissioned` | `decommissioned` | 是 |
| 6 | 有一張閘道器憑證以 keyCompromise 撤銷過（Laysi/trustiot-v2#636，見[閘道器憑證的撤銷](#閘道器憑證的撤銷)） | 403 | `permission_denied` | `key_compromised` | `key_compromised` | 是 |
| 6 | 沒有有效的綁定（在設備池、或最後的綁定已結束） | 403 | `permission_denied` | `not_bound` | `not_bound` | 是 |
| 6 | 租戶不是正常：停權、刪除、這個版本不認得的狀態，或沒有狀態紀錄（見[租戶狀態](#租戶狀態)） | 403 | `tenant_suspended` | `error.tenant_suspended` | `tenant_suspended` | 是 |
| 任何一步 | 資料庫失敗、簽章失敗 | 503，`Retry-After: 5` | `unavailable` | 無 | `unavailable` | 否 |

- M1 沒有 410 的 `code`，所以過期的挑戰回 410、`code` 用 `conflict`，以 `message_key` 區分；閘道器只需要看 HTTP 狀態。
- 全域限流與 lag、CA 的檢查兩步都做，所以取挑戰也可能回 429 或 503；兌換的檢查（第 6 列）只在兌換時發生，取挑戰除了格式與這幾項之外不拒絕。
- 限流排在 lag 與 CA 的檢查之前：超過限流的請求只記 DEBUG，所以 lag 不明時，大量捏造的硬體 ID 造成的 INFO 拒絕（`bindings_stale`）最多是限流放行的數量（測試）。lag 不明的期間 503 也會扣全域限流，但不扣硬體 ID 自己的限流（那一道在簽章驗過之後才算）。
- 第 2 到 5 項在碰挑戰之前，所以被它們拒絕的兌換不會讓挑戰作廢：閘道器在挑戰的 30 秒內重試同一個挑戰仍然可以。第 6 項的每一種拒絕都已經刪掉挑戰，包括超過硬體 ID 自己的限流的 429：閘道器照 `Retry-After` 等，再從取挑戰重來。
- 閘道器的處理（Laysi/trustiot-v2#83）：429 與 503 等 `Retry-After` 指定的秒數；其他拒絕以退避重試；每次重試都從取挑戰開始（409、410 與第 6 項的其他拒絕之後，挑戰都已作廢）。

### 限流與資源上限

- **限流**（`src/renewal/limiter.rs`）：全域每一步各一道（取挑戰一道、兌換一道，所有硬體 ID 一起算），在格式檢查之後、其他檢查與任何資料庫工作之前；每個硬體 ID 一道，只在兌換的簽章以登錄的公鑰驗過之後才檢查、才扣（Laysi/trustiot-v2#801，理由見「規則與取捨」）。都是 token bucket（以 GCRA 實作）：每一步的全部硬體 ID 一次 `renewal.global_burst`（預設 100）、之後每 `renewal.global_interval`（預設 50 ms，每秒 20 個）多一個；一個硬體 ID 一次 `renewal.gateway_burst` 個簽章驗過的兌換（預設 2）、之後每 `renewal.gateway_interval`（預設 60 秒）多一個。被拒的請求什麼都不改。全域分成兩步，大量取挑戰的請求就用不到兌換的額度：已經拿到挑戰的閘道器仍然能兌換（測試；還剩下的風險見「規則與取捨」）。記憶體裡每個硬體 ID 的項目只來自簽章驗過的兌換，所以未經驗證的請求不會增加項目；時間已經過去的項目在項目數超過門檻時清掉。入口 Cloudflare 的限流另計（展示環境的設定見 `docs/runbooks/demo-environment.md`）。
- **簽章之前的工作有上限**：兌換在簽章驗過之前要刪一次挑戰、以平台角色讀一次登錄（一行 INFO 稽核紀錄）、驗一次 ECDSA 簽章，這些只受兌換的全域限流（預設每個副本每秒 20 個）與下面的並行上限限制，不受硬體 ID 自己的限流限制。
- **限流是每個副本各自計算**：`replicas` 是 N 時，實際上限是 N 倍。certs 目前一個副本；增加副本時把每個副本的值除以 N。
- **並行**：每個副本同時做資料庫工作的請求最多 `renewal.max_concurrent`（預設 2，最多是平台角色連線池的 4 條減 1），輪不到的請求最多等 2 秒，之後回 503。所以大量閘道器同時退避重試時，平台角色的連線池仍留給出廠登錄與消費端；chart 的連線預算（`PLATFORM_CONNECTIONS` 的 4 條）不必改。
- **挑戰表有上限**：每次取挑戰在同一個語句裡刪掉過期超過 120 秒的挑戰（留 120 秒，讓稍晚的兌換得到 410 而不是 409），而寫入受取挑戰的全域限流限制，所以每個副本的列數最多約是那一道的速率乘以（30 秒效期加 120 秒）：預設是每秒 20 個乘 150 秒，3000 列再加上一次的 burst，每列約 100 bytes。未經驗證的請求不能讓資料庫無上限成長（測試以 50 個新硬體 ID 的請求確認只存下限流放行的那幾列）。
- **被拒的計數有上限**：只有已登錄的硬體 ID 會寫 `renewal_refusals`（`INSERT ... SELECT` 自登錄表，沒登錄就什麼都不寫），每個硬體 ID 每個時間窗一列，所以列數最多是已登錄的閘道器數乘以還沒回報的時間窗數；每次計數是一個短交易，次數受兌換的全域限流限制。

### 事件

- **`certs.gateway_certificate_renewed`**（`contract::events::certs::GatewayCertificateRenewed`，版本 1）：每次換發一筆，**租戶事件**（`tenant_id` 是綁定的租戶），與簽發紀錄在同一個交易（測試比對兩列的交易 ID）。actor 是 certs 服務自己（沒有使用者）。payload：`hardware_id`、`binding_id`、`certificate_serial`、`ca_key_id`、`not_before`、`not_after`。aggregate 是 `gateway_certificate`、id 是 `<租戶 ID>:<硬體 ID>`（`gateway_certificate_aggregate_id`），不是單純的硬體 ID：outbox 的 aggregate 列在第一個事件時記下租戶，RLS 不讓另一個租戶的交易再寫同一個 aggregate，閘道器改綁到下一個租戶後若沿用同一個 aggregate，換發的交易會被以 42501 拒絕。每個租戶各自一條序列，同一個租戶裡的換發依序排列。
- **`certs.gateway_renewal_refused`**（`GatewayRenewalRefused`，版本 1）：已登錄的硬體 ID 在一個時間窗（`renewal.refusal_window`，預設 1 小時，從 2000-01-01T00:00:00Z 起對齊）內被拒的請求合併成一筆，**全平台事件**（`tenant_id` 是 null：被拒的請求還不能確定屬於哪個租戶，沒有綁定的閘道器也沒有租戶），aggregate 是 `gateway_registration`、id 是硬體 ID（同 `certs.gateway_registered`），actor 是 certs。payload：`hardware_id`、`window_start`、`window_end`、`refusals`（次數）、`reasons`（每個原因的次數，例如 `{"signature_invalid": 3}`）。只計「計入時間窗」那一欄是「是」的原因，也就是兌換時的決定；格式錯誤、全域限流、503 不計，它們與這台閘道器無關。計入的原因分兩類：`challenge_unknown`、`challenge_mismatch`、`challenge_expired`、`signature_invalid` 不需要金鑰就能造成；`gateway_rate_limited`、`decommissioned`、`key_compromised`、`not_bound`、`tenant_suspended` 只在簽章驗過之後出現（前一類為什麼仍計入，見「規則與取捨」）。硬體 ID 自己的限流在除役、keyCompromise、綁定與租戶狀態的判斷之前，所以一把金鑰超過額度的重試記成 `gateway_rate_limited`，這幾個原因的次數會比嘗試的次數少（每個額度仍會出現一次）。沒有登錄的硬體 ID 不寫任何東西（決策 34 的事件紀錄永久保存）。
  - 每次被拒：平台角色在自己的短交易裡，把這次加到該硬體 ID 目前時間窗的列（`INSERT ... ON CONFLICT DO UPDATE`；時間窗以語句的開始時間決定，語句最多 5 秒）。寫入失敗只記 WARN，回應不變。
  - 每個副本每 `renewal.refusal_flush_interval`（預設 60 秒）找出結束超過 60 秒的時間窗：在同一個交易裡刪掉那一列並寫事件（`FOR UPDATE SKIP LOCKED`），所以多個副本也只寫一筆。60 秒大於計數語句的 5 秒上限，回報時不會還有計數寫進同一個時間窗、另開一列。
- 兩個事件都由身分服務的事件紀錄消費、記錄（全平台的與租戶的事件紀錄）；資產服務只處理 `certs.gateway_registered`，略過其他 certs 事件。

### 紀錄與指標

日誌（JSON，INFO，不含 nonce、簽章、公鑰或憑證）：

| 訊息 | 欄位 | 時機 |
| --- | --- | --- |
| `read a gateway for a certificate renewal` | `hardware_id`、`registered`、`decommissioned`、`tenant_id`、`binding_id`、`tenant_status`（存下的狀態，沒有紀錄是 `none`）、`tenant_suspended` | 每次兌換讀取閘道器時（查「誰讀了哪台閘道器」），另有 service-kit 的 `platform database access`（用途 `renew-gateway-certificate`） |
| `renewed a gateway certificate` | `hardware_id`、`tenant_id`、`binding_id`、`serial`、`ca_key_id`、`not_after` | 每次換發，簽發交易 commit 之後、請 broker 權限的同步比對這一台之前 |
| `refused a certificate renewal request` | `hardware_id`、`step`（`challenge` 或 `renew`）、`reason` | 每次拒絕；`rate_limited`（全域限流）記 DEBUG，而全域限流在其他檢查之前，所以超過限流的大量請求不變成大量 INFO（`bindings_stale`、`ca_not_ready` 這些 INFO 的拒絕最多是限流放行的數量）；讀不出硬體 ID 的本文（400、422）也只記 DEBUG；`gateway_rate_limited` 記 INFO，只有持有登錄金鑰的請求到得了 |
| `reported refused renewals of a registered gateway` | `hardware_id`、`window_start`、`window_end`、`refusals` | 寫出 `certs.gateway_renewal_refused` 時 |

計數與回報的平台交易也有 `platform database access`（用途 `count-renewal-refusal`、`report-renewal-refusals`；沒有到期的時間窗時只記 DEBUG）。

指標（OTLP counter）：`certs.renewal.challenges`（發出的挑戰）、`certs.renewal.issued`（換發的憑證）、`certs.renewal.refused`（屬性 `reason` 同上表、`step` 是 `challenge`、`renew` 或讀不出本文時的 `unknown`）。

兩種 429 的 `reason` 不同（Laysi/trustiot-v2#801 起；之前 `rate_limited` 也包含每個硬體 ID 的限流），告警（Laysi/trustiot-v2#183）照這個區分：

- `reason="rate_limited"`：超過某一步的全域限流，`step` 是哪一步。持續上升表示有人在用完全域額度，這段期間這個副本上每台閘道器都可能收到 429；同時 `certs.renewal.challenges` 停在全域速率附近。
- `reason="gateway_rate_limited"`（`step` 一定是 `renew`）：一個硬體 ID 簽章驗過的兌換太頻繁，可能是閘道器的換發排程出錯，或登錄的金鑰在別人手上。從 INFO 日誌與 `certs.gateway_renewal_refused` 的 `reasons` 查是哪個硬體 ID。

### 權限

- **挑戰**：`certs_app` 對 `renewal_challenges` 有 `SELECT`、`INSERT`、`DELETE`（`DELETE ... RETURNING` 需要 `SELECT`）。挑戰只有 nonce、硬體 ID、用途與時間，沒有租戶資料，所以沒有 RLS；平台角色碰不到這張表。
- **讀閘道器**：沿用平台角色 `certs_platform` 的唯讀查詢（`Projection::begin`：`SET TRANSACTION READ ONLY`、`statement_timeout` 5 秒、`lock_timeout` 2 秒、每筆交易一行 INFO 稽核紀錄），不給 `certs_app` 任何登錄表的權限。查詢只取需要的欄位：登錄的公鑰與 `decommissioned_at`、最後一個綁定的租戶、綁定 ID 與 `unbound_at`，以及租戶狀態；不讀 EK、工作站或第一張憑證。
- **簽發**：簽發紀錄與租戶事件以 `certs_app` 在**閘道器目前綁定的租戶**的交易裡寫入（`BackgroundTenantDb::begin`）。這是 `BackgroundTenantDb` 在 handler 路徑上的例外（[crates/service-kit](../../crates/service-kit/README.md) 的「資料庫」）：請求沒有使用者身分，租戶不取自請求，而取自簽章驗過之後、平台角色剛從綁定表讀出的值；`BackgroundTenantDb` 只交給換發的元件，handler 本身拿不到。
- **被拒的計數**：`certs_platform` 對 `renewal_refusals` 有 `SELECT`、`INSERT`、`UPDATE`、`DELETE`；只有它能讀登錄表（決定要不要計），也只有它能寫全平台事件。`certs_app` 碰不到這張表。

### 規則與取捨

- **挑戰存在資料庫，不用無狀態的 HMAC 挑戰**：一次性要靠刪除那一列保證，HMAC 挑戰仍然要記已兌換的 nonce，還多一把要保管與輪替的伺服器金鑰。到期以資料庫時鐘判斷，多個副本看到同一個時間。
- **讀取與簽發之間沒有鎖**：綁定或租戶狀態在讀取之後、簽發交易 commit 之前改變（毫秒級）時，這次換發仍依讀到的狀態簽發。與下一項相同，靠解綁的撤銷（Laysi/trustiot-v2#115）與 broker 的權限同步（Laysi/trustiot-v2#633）收斂。
- **消費端落後的上限**：lag 每 15 秒量一次、超過 45 秒沒有新的量測就當作不明，所以換發依據的綁定最多落後約 `renewal.max_consumer_lag` 加 45 秒（預設約 105 秒）。剛綁定不到這段時間的閘道器可能得到 `not_bound`，退避重試後成功；消費端停在不認得的綁定事件時（[消費的事件](#消費的事件)），每個換發都回 503，直到認得它的版本上線。租戶狀態的消費端（Laysi/trustiot-v2#778）同樣計入：兩個消費端取較慢的（`renewal::slowest`：任一個不明就不明，未處理的事件數相加，最舊的事件取較舊的），所以停權之後最多約同樣的時間內仍可能換發，消費端停在租戶事件上時每個換發都回 503。
- **時間差**：兌換時已登錄的硬體 ID 要驗簽章並寫一次計數，沒登錄的不用，所以回應時間可能透露硬體 ID 是否登錄。接受：硬體 ID 不是密鑰（印在設備上、是憑證的 CN 與 MQTT 帳號），回應內容本身不透露。
- **每個硬體 ID 的限流只算簽章驗過的兌換**（Laysi/trustiot-v2#801）：硬體 ID 不是密鑰，任何人都能送以它為名的請求。原本每個硬體 ID 的限流在簽章之前、兩步都算，知道硬體 ID 的人每 30 秒送一個請求，就能讓那台閘道器一直收到 429；這遠低於入口每個 IP 的限流，也不必換來源位址。現在簽章之前只有每一步的全域限流，硬體 ID 自己那一道在簽章以登錄的公鑰驗過之後才檢查、才扣，只有持有 TPM 金鑰的閘道器用得完（測試：以硬體 ID 為名、沒有金鑰的 8 個請求之後，真正的閘道器仍換發 2 次，第 3 次才因自己的限流 429）。
  - **簽章驗過之後的那一道**：限制一把金鑰多常拿到憑證與狀態判斷（簽發紀錄與租戶事件永久保存）。預設一次 2 個、之後每 60 秒 1 個，和原本（一次 4 個請求、之後每 30 秒 1 個，一次換發是 2 個請求）一樣，一把金鑰平均每分鐘最多 1 張。超過時回 429 與 `Retry-After`，挑戰已作廢，閘道器照 `Retry-After` 從取挑戰重來；原因是 `gateway_rate_limited`，記 INFO、計入被拒的時間窗，因為只有持有金鑰的請求到得了。額度在除役、keyCompromise、綁定與租戶狀態的判斷之前扣，所以也限制一把金鑰多常拿到這些判斷；簽發時服務自己的失敗（資料庫、簽章失敗、還沒有簽發 CA，回 503）退還這次的額度，閘道器照 `Retry-After` 重試不會撞上自己的限流。退還只在這個硬體 ID 期間沒有其他兌換通過時才做：那時把時間往回移一格正好等於沒扣過；之後有別的兌換時無法準確還原，寧可不退，不讓退還超過 burst。
  - **取捨**：簽章之前不再有每個硬體 ID 的公平性。一個硬體 ID（捏造的，或出錯一直重試的閘道器）可以用掉整個全域額度，效果和以大量捏造的硬體 ID 用完全域額度相同（下一項），一樣由入口每個 IP 的限流擋。挑戰表的上限本來就不靠每個硬體 ID 的限流，而是靠取挑戰的全域限流與過期清除（「限流與資源上限」），所以不變；簽章驗證的工作量同樣受兌換的全域限流與並行上限限制。
  - **比較過、沒有採用**：取挑戰時每個硬體 ID 最多留幾個挑戰、新的取代最舊的：捏造的取挑戰請求會一直擠掉真正閘道器手上的挑戰，一樣換發不了。簽章之前另留一道每個硬體 ID 的額度：仍是不用金鑰就能用完的額度。以來源 IP 區分：服務看不到可信的來源位址（下一項的「在哪裡擋」）。
  - **還剩下什麼**：要讓某一台閘道器換發不了，現在得用完一步的全域額度，所以這個副本上每台閘道器都會一起收到 429，`rate_limited` 指標也會上升，不能再只針對一台、低速而不被發現地做。在全域速率以內送會做資料庫工作的請求、佔滿 `renewal.max_concurrent` 的名額，效果也一樣是整個副本：其他請求等 2 秒後得到 503（`reason="busy"`）。有很多來源位址的攻擊者仍做得到（下一項），也仍能讓已登錄的硬體 ID 寫出被拒的事件（再下一項）。
- **全域限流仍可被耗盡**：任何人都能以捏造的硬體 ID 送超過全域速率（預設每個副本每秒 20 個）的取挑戰請求，這段期間這個副本上每台閘道器取挑戰都得到 429；以捏造的 nonce 大量兌換也同樣會用完兌換那一道。全域分成兩步只保證一步的洪水不佔用另一步：洪水開始時已經拿到挑戰的閘道器仍能兌換，其他閘道器照 `Retry-After` 重試，洪水停止後恢復。過期或被撤銷的閘道器只能經這個端點恢復，所以洪水期間它們也恢復不了。
  - **怎麼發現**：`certs.renewal.refused` 的 `reason="rate_limited"` 持續上升（`step` 分出是哪一步），同時 `certs.renewal.challenges` 停在全域速率附近。被拒的計數不含全域限流，所以這不寫事件；超過全域限流的請求只記 DEBUG。
  - **在哪裡擋**：以來源 IP 限流屬於入口，不在服務裡：服務看到的來源位址取決於入口的暴露方式與哪個標頭可信（mqtt-bridge 為此要設定 `TRUSTIOT_CLIENT_IP_SOURCE`），而每個來源 IP 的計數在最前面做才不會先佔用服務的額度。所以 Cloudflare 的速率限制規則要涵蓋 `/certs/renew`（與 `/mqtt` 共用同一條，見 `docs/runbooks/demo-environment.md` 的「`/mqtt` 的速率限制」），這是端點在展示叢集對外開放之前的前提條件（同一份文件的「換發端點上線」）。那條規則每個 IP 每 10 秒 40 次，要壓住預設的每秒 20 個需要 5 個以上的來源 IP，所以它提高門檻但不能消除這個風險，能換 IPv6 位址的攻擊者仍然做得到；那時再調低全域限流或在入口加上其他規則。
- **未經驗證的流量也會寫出被拒的事件**（Laysi/trustiot-v2#801 的決定：維持）：對已登錄的硬體 ID 送捏造的 nonce（`challenge_unknown`）、別的硬體 ID 的挑戰（`challenge_mismatch`）或錯的簽章（`signature_invalid`）都不需要金鑰，仍計入時間窗，所以任何知道硬體 ID 的人都能讓那台閘道器寫出 `certs.gateway_renewal_refused`。維持計入的理由：
  - 這個事件是「有人在試已登錄的硬體 ID」唯一永久保存的紀錄；日誌與指標都會過期。
  - 服務分不出捏造的請求與真正閘道器的失敗：TPM 的金鑰被清掉、換了 TPM 卻沒重新登錄、或閘道器的換發程式出錯時，真正的閘道器也得到 `signature_invalid` 或 `challenge_*`，這正是維運要看到的。只計簽章驗過的原因，這些就看不到了。
  - 寫入量有上限，而且與攻擊者的請求速率無關：每個已登錄的硬體 ID 每個時間窗最多一筆，沒有登錄的硬體 ID 什麼都不寫。最壞的情況是有人知道每個已登錄的硬體 ID、每個時間窗對每一個至少送一次，預設 1 小時的時間窗下每台閘道器每天 24 筆。這做得到：最便宜的是一個帶捏造 nonce 的兌換（`challenge_unknown`），連取挑戰都不用；兌換的全域限流（預設每個副本每秒 20 個，每小時 72,000 個）涵蓋得了數萬台閘道器；硬體 ID 印在設備上、可能有規律，回應時間也可能透露它是否登錄（上面的「時間差」）。以 1 萬台估算，是每天 24 萬筆、每年約 8,800 萬筆永久保存的事件，量由攻擊者決定。這不是這次才有的：改之前每個硬體 ID 的限流也遠高於每個時間窗一次。平台事件紀錄怎麼保存或彙整這種事件，另外評估。
  - 沒有另外降低寫入頻率：只把未驗證原因的時間窗拉長，要在 `renewal_refusals` 的主鍵加上類別（migration），上限也仍是已登錄的閘道器數乘以時間窗數。要降低最壞的寫入量時調長 `renewal.refusal_window`（最多 1 天，每台每天最多 1 筆），代價是事件晚一點出現。
  - 事件的 `reasons` 分得出兩類（「事件」）：`challenge_*` 與 `signature_invalid` 不需要金鑰，其他只在簽章驗過之後出現。Laysi/trustiot-v2#183 設事件速率的告警時以此區分：只有前一類的事件可能是有人在試硬體 ID，也可能是閘道器的金鑰或程式壞了。
- **broker 角色**：簽發交易 commit 之後呼叫 `BrokerSync::gateway`，和出廠登錄相同：[broker 權限](#broker-權限dynamic-security)的同步只比對這一台。閘道器唯一沒撤銷、沒過期的憑證過期或被撤銷之後，它在 dynsec 的 client 會在下一次完整比對被刪掉（[條件](#條件)）；換發 commit 之後新的憑證符合條件，這次單台比對就把 client 加回來，閘道器換上新憑證連線時就能發布，不必等下一次完整比對（`dynsec.interval`，預設 5 分鐘）。只在 commit 之後呼叫：在交易裡呼叫時，比對可能在 commit 之前讀 Postgres，看不到新憑證。換發不等比對完成，比對失敗或排不進佇列也不影響 201：記 WARN，由下一次完整比對補上。沒有設定 broker 時這個呼叫什麼都不做。
- **路徑固定**：服務只在 `/certs/renew` 提供端點（`protocol::renewal::RENEWAL_PATH`），chart 的 `ingress.publicPaths.certRenewal` 只決定入口的路由，改成別的路徑時入口與服務對不上。

## broker 權限（dynamic security）

2026-10-29 起（Laysi/trustiot-v2#633，`src/dynsec.rs`）。Mosquitto 以 dynamic security（dynsec）決定每個 client 能做什麼；閘道器以憑證的 CN（硬體 ID）當 username，dynsec 裡沒有這個 client 時連得上，但任何發布或訂閱都被拒絕（PUBACK／SUBACK 0x87）。chart 的 init container 只寫服務帳號、chart 的角色與群組（[deploy/chart/README.md](../../deploy/chart/README.md) 的「角色（dynamic security）」），閘道器的 client 由 certs 維護。

### 條件

閘道器在 dynsec 有 client 的條件（2026-10-24 決定，見[閘道器的租戶歸屬與換發條件](#閘道器的租戶歸屬與換發條件)）：已出廠登錄、沒有除役、目前有綁定（資產服務 log 裡最後的綁定沒有結束，見[閘道器的綁定與租戶狀態](#閘道器的綁定與租戶狀態)），而且至少有一張沒撤銷、沒過期的閘道器憑證。符合時 client 的 username 是硬體 ID、沒有密碼、只有 chart 的閘道器角色 `trustiot-gateway`（`protocol::acl::GATEWAY_ROLE`，只能發布自己的 `up/`、訂閱自己的 `down/`），沒有群組、沒有停用；不符合時沒有 client。查詢是 `DESIRED`，以平台角色 `certs_platform` 的唯讀交易讀（每次讀取有一行 `platform database access`，purpose `sync-broker-permissions`）。租戶停權（Laysi/trustiot-v2#108）不在條件內，由該 issue 決定要不要加入。

### 只動閘道器的 client

- 只比對、只改 username 是合規則 `protocol::GatewayId` 的 client。服務帳號（`service:*`，不可能是合規則的硬體 ID）、chart 的角色與群組永遠不送任何指令，仍由 init container 擁有。反過來說，username 符合硬體 ID 格式的 client 全部由 certs 管理：有人手動建立的 `admin`、`debug` 這類 client，因為不是 Postgres 裡該有角色的閘道器，會在下一次完整比對被刪除。要手動建立 client 時，名稱要帶 `:`（硬體 ID 不能有）。
- 缺少的閘道器：一個 `createClient`，同時帶角色、不帶密碼。Mosquitto 2.1 的 `createClient` 不踢掉已經連著的同名連線，而 ACL 每次檢查都依 username 查 client，所以已連線、剛綁定的閘道器不必重連就能發布。
- 多出來的閘道器 client：`deleteClient`，Mosquitto 同時斷開它的連線。
- 角色不對、有群組、被停用、綁了 client id，或 text name 不是 certs 設的 `TrustIoT gateway (certs-managed)` 的閘道器 client：刪除後重建，同時清掉任何人手動設的密碼。Mosquitto 2.1 的 `listClients` 與 `getClient` 都不透露 client 有沒有密碼，所以以 text name 判斷是不是 certs 建立的：別人建立（可能帶密碼）的會被換掉。certs 建立之後才被 `setClientPassword` 設上的密碼看不出來；能下這個指令的只有 dynsec 管理角色，而持有它本來就等同掌控 broker。剩下的風險：外洩的 dynsec 管理帳密（`service:certs` 的密碼）可以替某台閘道器的 client 設上密碼，再以那個硬體 ID 和密碼登入服務埠 1883，繞過憑證與 CRL，取得那台閘道器的權限。chart 目前的緩解是 1883 只在叢集內：broker 的 Service 是沒有 node port 的 ClusterIP、不能有入口路由，AuthorizationPolicy 只讓 `mesh.calls` 列出 `mqtt-broker` 的服務（`ingest`、`gateway-ops`、`certs`）的 pod 連 1883，NetworkPolicy 只放行同一批 pod（見 [deploy/chart/README.md](../../deploy/chart/README.md) 的「MQTT broker」）。所以要利用它，還得在這三個服務的 pod 裡執行程式。
- 沒有差異時不送任何修改指令（只有 `listClients` 或 `getClient` 的讀取）。
- **一次刪掉一大批的保護**（`guard`）：每個 `deleteClient` 都會斷開閘道器的連線，所以 Postgres 突然說很少或沒有閘道器該有角色時（`certs_platform` 的權限或 policy 壞了、綁定表被清空或重建、從較舊的備份還原），照做會讓整批閘道器失去發布權限。完整比對要刪除加上重建的 client 數超過上限時（10 個，或 broker 上閘道器 client 的 10%，取大者：`ALWAYS_ALLOWED_REMOVALS`、`MAX_REMOVAL_PERCENT`），或 Postgres 說沒有任何閘道器該有角色、broker 卻有閘道器 client 時，這一輪不送任何刪除與重建，只送新增，並記一行 ERROR `refused to remove or replace this many gateway clients of the broker at once`（數量與上限，不列硬體 ID）；下一輪再比對一次。單台的同步（登錄、綁定、換發之後）不受限制。所以最後一台閘道器的權限不會由完整比對拿掉：解綁與撤銷之後的單台同步（Laysi/trustiot-v2#115、Laysi/trustiot-v2#636）會拿掉；大量撤銷或解綁也是經由這些單台同步逐台處理，不靠完整比對。ERROR 持續出現時，先查 `certs_platform` 能不能讀到登錄、簽發紀錄與綁定。
- broker 的回應要完全符合 Mosquitto 2.1 的格式才採用：`listClients` 的每個項目要有 `roles` 與 `groups` 陣列、每個角色有名稱，各頁加起來要剛好是 `totalCount` 個不重複的 client（頁數不夠、重複、`totalCount` 在頁之間改變都算錯）；任何型別的 `error` 都算失敗。不符合時這一輪不送任何修改，記 WARN 後重試，避免因為格式改變或不完整的清單，把整批 client 當成不對而刪除重建，或漏掉多出來的 client。

### 時機

- **完整比對**：每次連上 broker（第一次與每次重連，例如 broker 重啟、dynsec 檔被重設之後）以及每 `dynsec.interval`（預設 5 分鐘）。先以 `listClients` 分頁（每頁 500）讀 broker 的全部 client，再讀 Postgres，所以這中間 commit 的變更不是兩邊都看到，就是由下一項的立即同步補上。
- **立即同步一台**：出廠登錄 commit 之後（`registrations`）、換發的簽發交易 commit 之後（`renewal`，見[換發](#換發)的「broker 角色」）、閘道器憑證撤銷並重發 CRL 之後（`gateways`，見[閘道器憑證的撤銷](#閘道器憑證的撤銷)）、綁定事件處理完之後（`SyncAfterBinding`：在事件的交易裡記下交易 ID，worker 等它 commit 再比對，最多 10 秒，否則留到下一輪）只比對那一台（`getClient`）。
- **單一副本**：每次比對，不論完整或單台，都在工作鎖 `dynsec`（`service_kit::job_lock`）之內，兩個副本的修改不會交錯；拿不到鎖的副本 1 秒後再試。每個副本各自連 broker，所以各自的請求與事件都能同步。
- 失敗（broker 拒絕指令、沒有回應、Postgres 讀不到）記 WARN，最多 30 秒（或較短的 `dynsec.interval`）後重做完整比對；單台的請求排隊等不到時（佇列 4096）記 WARN 丟棄，下一次完整比對涵蓋它。

### 連線與帳號

- 用 `crates/mqtt-client`，帳號 `service:certs`，密碼檔每次連線重讀。chart 在 broker 開啟時給 certs 這些設定（`TRUSTIOT__MQTT__HOST`、`PORT`、`USERNAME`、`PASSWORD_FILE`，自訂 `env` 不能覆寫），並只把 broker 帳號 Secret 的 `certs` key 以目錄掛在 `/etc/trustiot/mqtt-broker-account`（密碼檔是其中的 `certs`）；輪替密碼時 certs 不重啟，kubelet 換上新檔案後下一次重連就用新密碼（Laysi/trustiot-v2#634，[deploy/chart/README.md](../../deploy/chart/README.md)「服務的帳號」）。`service:certs` 是唯一有 dynsec 管理角色的服務帳號（[docs/architecture/secrets.md](../../docs/architecture/secrets.md) 的「MQTT dynsec 管理帳密」），設定只接受這個帳號。
- 指令送到 `$CONTROL/dynamic-security/v1`，回應從 `$CONTROL/dynamic-security/v1/response` 讀，以每個指令的 `correlationData` 對應；Mosquitto 只把回應送給發出指令的 client。一則訊息最多 50 個變更，等回應最多 30 秒。
- 沒有設定 broker（`TRUSTIOT__MQTT__HOST` 等都沒設）時不同步，啟動時記一行 INFO `no mqtt broker is configured; the broker's gateway permissions are not synced`。broker 連不上不影響 readiness：CRL 與 CA 鏈仍要提供給 broker，同步只是延後。

### 日誌

- 每個變更一行 INFO `changed a gateway's broker permissions`（`hardware_id`、`action`：`added`、`removed` 或 `replaced`、`role`）；broker 拒絕時一行 WARN `the broker refused a change of a gateway's permissions`。
- 完整比對有變更時一行 INFO `brought the broker's gateway permissions in line with postgres`（各項數量），沒有變更時只有 DEBUG `the broker's gateway permissions match postgres`。
- 不記 broker 密碼（`crates/mqtt-client` 從不顯示它），也不記指令的原文。

## 閘道器憑證的撤銷

2026-10-30 起（Laysi/trustiot-v2#636，`src/gateways.rs`）。決策 6 的撤銷三步驟：在簽發紀錄撤銷閘道器的憑證、立刻重發 CRL、拿掉 broker 的 client 並斷開連線。平台管理員以 API 撤銷一台閘道器；停權（Laysi/trustiot-v2#108）與除役、解綁（Laysi/trustiot-v2#115）之後以 `GatewayRevocation::revoke` 加上 `Requester::Service` 呼叫同一個流程。

### API

前提與工作站憑證的 API 相同（見[工作站憑證](#工作站憑證)的「簽發、撤銷與換發」）：入口的 cookie 路由、平台管理員本人（換發的 token 403）、回應 `cache-control: no-store`、請求的 `Content-Type` 是 `application/json`。

```sh
read -rs SESSION
curl -sS --fail-with-body -X POST "https://<app 主機>/api/certs/v1/gateways/<硬體 ID>/revoke" \
  -H 'content-type: application/json' --cookie "__Host-trustiot-session=$SESSION" \
  --data '{"reason": "key_compromise"}'
```

`reason` 可省略（CRL 不寫原因），或是與工作站憑證相同的六個值之一。撤銷這台閘道器每一張還有效（沒撤銷、沒過期）的閘道器憑證；200 回傳 `hardware_id` 與撤銷的每一張（`serial`、`ca_key_id`、`not_before`、`not_after`、`revoked_at`、`revocation_reason`），依序號排序。已經撤銷的憑證保留原本的撤銷時間與原因，過期的不動。錯誤照 M1 格式：

| 回應 | 情況 |
| --- | --- |
| 400 `invalid_request` | `Content-Type` 不是 JSON、本文超過 1 KiB（`gateway_revocation.body_too_large`）或不是 JSON 物件（`gateway_revocation.body_invalid`） |
| 401 `unauthenticated` | 沒有或驗不過內部身分 token |
| 403 `permission_denied` | 不是平台管理員，或 token 帶 `act`（別的服務代為呼叫） |
| 404 `not_found` | 這個硬體 ID 沒有出廠登錄過（路徑不是合規則的硬體 ID 也是） |
| 409 `conflict` | 登錄過，但沒有還有效的憑證可撤銷：都已撤銷或都過期（`gateway_revocation.no_valid_certificate`）；同一個請求重送也得到它 |
| 422 `validation_failed` | 不認得的欄位（`gateway_revocation.unknown_field`）、不是列出的原因（`gateway_revocation.reason_invalid`，例如 `certificate_hold`：撤銷不能收回） |
| 500 `internal` | 撤銷的 task 異常結束；不確定撤銷了沒有，再呼叫一次就知道（409 表示已撤銷） |
| 503 `unavailable` | 資料庫失敗，通常什麼都沒撤銷；只有提交途中斷線時撤銷可能已經成立，但沒有立即的 CRL 與 broker 步驟（由 CRL 工作與下一次完整比對補上）。再呼叫一次就知道（409 表示已撤銷）。ERROR 行帶平台管理員的使用者 id |

被拒絕的請求不撤銷任何憑證、不發 CRL、不動 broker。「登錄過」以簽發紀錄判斷：出廠登錄與第一張閘道器憑證在同一個交易寫入、換發只為登錄過的硬體 ID 簽、沒有任何角色刪得掉簽發紀錄，所以簽發紀錄有這個硬體 ID 的閘道器憑證，就是登錄過；不必以平台角色讀登錄表。

每次撤銷記一行 INFO `revoked a gateway's certificates`（`requested_by`：`platform_admin` 加上 `user_id`，或 `service` 加上 `purpose`；`hardware_id`、逗號分隔的 `serials` 與 `ca_key_ids`、`reason`），在交易提交之後、重發 CRL 之前寫，不含憑證。寫事件紀錄不在這個 issue 的範圍。

### 順序與理由

1. **簽發紀錄**：一個交易撤銷這台閘道器所有還有效的憑證（`revoked_at` 是資料庫的時間），提交之後就成立、不能收回（見[資料庫](#資料庫)）。CRL 與 broker 權限都從簽發紀錄算出，所以它先做：之後兩步失敗也不會讓撤銷消失，例行工作會補上。
2. **CRL**：為撤銷的憑證的每個簽發 CA 重發一張 CRL（同一個 CA 的幾張合成一張，`crl::Reissuer::after_revocations`），一個 CA 接著一個，回應前等它完成，每個最多 10 秒（`crl::REISSUE_TIMEOUT`）。失敗時記 ERROR，撤銷照樣成立，CRL 工作最晚一輪 `crl.check_interval` 之後列出它（見[CRL 與告警](#crl-與告警)）。
3. **broker**：請這個副本的 broker 同步比對這一台（見[broker 權限](#broker-權限dynamic-security)的「時機」）。沒有還有效的憑證，所以不該有 client：同步送 `deleteClient`，Mosquitto 同時斷開它的連線。回應不等這一步。

CRL 在斷線之前：斷線時 `crl.pem` 已經列出撤銷的憑證，閘道器馬上重連時，broker 最多再過一個 `mqttBroker.trustInterval` 就以新的 CRL 拒絕它。反過來先斷線，重連的閘道器要多等重發 CRL 的時間。斷線用 broker 同步而不是直接送 `deleteClient`：所有閘道器 client 的修改都在工作鎖之內、以 Postgres 為準，撤銷與同時進行的換發或綁定不會交錯成和 Postgres 不一致的結果。

整個流程在自己的 task 執行：管理員的連線中斷或請求逾時，不會讓它停在撤銷與重發之間。

### 斷線與拒絕的時間（N 與 M）

- **N：撤銷到已經連線的閘道器被斷開，正常時 30 秒以內。** 組成是簽發紀錄的交易、重發 CRL（最多 10 秒）、等工作鎖（每 1 秒再試，`dynsec::LOCK_RETRY`）與 broker 處理 `deleteClient`。「正常」指 broker 連得上、沒有另一個副本正好在做完整比對：完整比對拿著工作鎖直到做完，閘道器很多時它可能要數十秒，N 就再加上它剩下的時間。`tests/dynsec.rs` 兩個副本、請求送到第二個時量到 回應 6 毫秒、斷線 8 毫秒（建置主機）。broker 連不上或拒絕時，同步記 WARN，最多 30 秒（`dynsec::ERROR_RETRY`）後的完整比對再刪除。
- **M：撤銷到重連在 TLS 被拒，正常時 40 秒以內**：回應時 CRL 已經重發（最多 10 秒），broker 的 sidecar 再過最多一個 `mqttBroker.trustInterval`（預設 30 秒）取得、檢查並以 SIGHUP 載入。在那之前閘道器重連時 TLS 通過，但 dynsec 沒有它的 client，發布與訂閱都得到 0x87；載入之後 TLS 1.3 交握在伺服器檢查用戶端憑證時失敗（rustls 的 `CertificateRevoked`）。立即重發失敗時再加最多一輪 `crl.check_interval`（預設 5 分鐘）。

### 哪些 dynsec 指令會斷線

`tests/dynsec.rs` 對 chart 的 Mosquitto（`eclipse-mosquitto:2.1.2-alpine`）以 `mosquitto_ctrl dynsec` 對一台已連線、有角色的閘道器逐一下指令，之後發布兩次觀察：

| 指令 | 已連線的閘道器 |
| --- | --- |
| `deleteClient` | 斷線（DISCONNECT 0x98，administrative action）。撤銷用的是它 |
| `removeClientRole`、`addClientRole` | 斷線（0x98）：Mosquitto 2.1 改到 client 本身就斷開它的連線，不只刪除 |
| `disableClient` | 斷線（0x98），但閘道器立刻以憑證重連、照樣能發布：dynsec 的停用只擋密碼登入，擋不住以憑證登入的閘道器，所以停權（Laysi/trustiot-v2#108）不能靠它 |
| `enableClient` | 不斷線 |

所以撤銷只要刪除 client 就會斷線，不必另外踢線；新增 client（`createClient`）則不斷開已經連著的同名連線（見[broker 權限](#broker-權限dynamic-security)的「只動閘道器的 client」）。這是 chart 釘住的 broker 映像的行為，升級 Mosquitto 時這個測試會發現它改變。

### 撤銷與換發：原因決定閘道器能不能回來

換發（見[換發](#換發)）看的是登錄、目前的綁定與租戶狀態，以及這台閘道器有沒有憑證以 keyCompromise 撤銷過：

- **以 `key_compromise` 撤銷**：閘道器從此不能換發（403 `certificate_renewal.key_compromised`）。換發證明身分的就是登錄的 TPM 金鑰，金鑰不再可信，就不能再拿它換憑證；照決策 12，換 TPM 視同新設備，舊的除役、新的回產線登錄。閘道器被偷、或懷疑金鑰被濫用時用這個原因，它才真的被擋在外面。這個決定不能收回（撤銷的原因不能改，見[資料庫](#資料庫)）。
- **其他原因或不寫原因**：已登錄、沒有除役、有綁定、租戶沒停權的閘道器之後仍能以 TPM 金鑰換到新憑證，broker 同步隨即補回它的 client。所以這種撤銷是「作廢目前的憑證、讓閘道器重新換發」，不是把它擋在外面；要擋，再除役或解綁（Laysi/trustiot-v2#115），或停權它的租戶（Laysi/trustiot-v2#108）。

`tests/dynsec.rs` 兩條都走過：不寫原因的撤銷之後換發成功、新憑證能發布；再以 `key_compromise` 撤銷，連線被斷開，換發得到 403，簽發紀錄沒有多一張，broker 沒有 client。

撤銷與換發的簽發以同一個鎖排序（`gateways::LOCK_GATEWAY_CERTIFICATES`，以硬體 ID 為鍵、到交易結束的 advisory lock）：撤銷的交易先取它再撤銷；換發的簽發交易先取它，再查一次有沒有 keyCompromise 撤銷，才簽章。所以換發在讀取之後、簽章之前遇到剛提交的 keyCompromise 撤銷，仍然被拒絕（`tests/dynsec.rs` 拿著鎖讓換發等，期間提交撤銷，換發得到 403、沒有簽出）；撤銷等著一個正在簽章的換發時，換發提交之後撤銷也撤銷它簽出的那張。回應列出的就是這台閘道器在那一刻全部的有效憑證。

## broker 伺服器憑證

2026-10-23 決定（Laysi/trustiot-v2#557）。MQTT broker 在閘道器埠（8883）以這張憑證向閘道器證明自己；閘道器以下面的信任錨與名稱驗證它。和閘道器、工作站憑證同一個簽發 CA，以 EKU 區分（理由同[工作站憑證](#工作站憑證)的「profile 的決定」：多一個簽發 CA 要多一套金鑰、儀式、輪替與 CRL）。私鑰在叢集內由 chart 的 CronJob `<release>-mqtt-broker-certificate` 產生，不經過 certs 服務；它在儀式之後取得第一張憑證、過了效期一半就以新的私鑰換發，寫進只有 broker 的 pod 掛的 Secret `<release>-mqtt-broker-tls`，broker 以 SIGHUP 換上，不中斷連線（Laysi/trustiot-v2#535，[deploy/chart/README.md](../../deploy/chart/README.md) 的「伺服器憑證」）。閘道器怎麼驗證它（信任錨、名稱、用途、以根 CRL 檢查簽發 CA）見[閘道器對 broker 伺服器憑證的信任](#閘道器對-broker-伺服器憑證的信任)（Laysi/trustiot-v2#556）。

### 閘道器驗證的名稱與信任錨

| 項目 | 值 |
| --- | --- |
| 名稱 | `mqtt-broker.trustiot.internal`（`issue::BROKER_SERVER_NAME`），SaaS 與地端相同。閘道器以這個名稱做 SNI 與名稱檢查，不論實際連到哪個位址（SaaS 的入口主機、地端的主機名稱或 IP），連線位址另外設定 |
| 信任錨 | 根 CA 憑證（chart values 的 `certificateAuthority.rootCertificate`，也是 `ca.pem` 的第一張）。broker 送出伺服器憑證與簽它的簽發 CA 憑證（簽發端點回的 `certificate_chain`），閘道器以根驗證整條鏈，簽發 CA 換代時不必改閘道器；設定的全貌見[閘道器對 broker 伺服器憑證的信任](#閘道器對-broker-伺服器憑證的信任) |

| 名稱的做法 | 結果 |
| --- | --- |
| **採用**：固定的 `mqtt-broker.trustiot.internal` | `.internal` 是 ICANN 在 2024-07-29 保留給私有用途的頂級網域，公開 CA 不會為它簽憑證，也不會被別人註冊；兩種部署型態、每個安裝都是同一個名稱，閘道器出廠設定不必依客戶改 |
| SaaS 的閘道器主機名稱（`hosts.gateway`） | 地端沒有固定的公開主機名稱，有時只有 IP，兩種型態要兩套設定；公開網域的名稱也能從公開 CA 取得憑證，信任錨若誤設成系統 CA 就被冒充 |
| Service 名稱 `<release>-mqtt-broker.<namespace>.svc` | 隨 release 與 namespace 改變，閘道器也看不到叢集內的名稱 |
| IP SAN | 地端的位址會變，SaaS 經入口時是負載平衡器的位址 |

依 Laysi/trustiot-v2#84 的約定（[deploy/chart/README.md](../../deploy/chart/README.md) 的「伺服器憑證」），憑證另外帶 broker 的 Service 名稱：chart 開啟 broker 時以 `TRUSTIOT__BROKER__SERVICE_NAME` 交給服務 `<release>-mqtt-broker.<namespace>.svc`，成為第二個 SAN。憑證 CronJob 拿到同一個值，簽回的憑證名稱不是這兩個就不寫入。

### 欄位

`src/issue.rs` 的 `broker_certificate`：CA 只從 CSR 取公鑰，持有證明、金鑰與 CSR 的檢查和閘道器憑證相同（ECDSA P-256、ecdsa-with-SHA256、不超過 4096 bytes）；CSR 的主體、SAN、keyUsage、extendedKeyUsage、basicConstraints 與其他延伸欄位一律忽略，名稱只來自設定。

| 欄位 | 值 |
| --- | --- |
| 主體 | `CN=mqtt-broker.trustiot.internal`，只有這一項 |
| subjectAltName | DNS `mqtt-broker.trustiot.internal`，設了 `broker.service_name` 時再加它；沒有 IP |
| basicConstraints | `cA=FALSE`（critical） |
| keyUsage | digitalSignature（critical） |
| extendedKeyUsage | 只有 serverAuth |
| 金鑰識別碼 | subject 與 authority key identifier |
| 序號 | 128 位元隨機正整數 |
| 效期 | 簽發前 5 分鐘起、`broker.validity`（預設 30 天，可設 1 小時到 90 天），不超過簽發 CA；簽發 CA 剩不到 1 小時時拒絕 |
| 沒有的欄位 | CRL distribution point（閘道器只讀根 CRL，不查伺服器憑證本身的撤銷） |
| 簽發紀錄 | `certs.issued_certificates` 的一列，`profile` 是 `broker`、`subject` 是 CN |

不能當閘道器憑證：EKU 沒有 clientAuth，broker（Mosquitto 以 OpenSSL 驗用戶端憑證，用途 `ssl_client`）與 rustls 都在握手拒絕它（`tests/broker.rs`）。第二道防線：閘道器 id 不能是這個名稱（不分大小寫；`gateway_certificate` 回 `ReservedGatewayId`）。閘道器憑證沒有 SAN，OpenSSL 的名稱檢查在沒有 DNS SAN 時改看 CN，所以這樣的閘道器憑證會對得上 broker 的名稱；它的 EKU 只有 clientAuth，TLS 用戶端本來就拒絕它當伺服器憑證，這一道是給忽略 EKU 的用戶端。

**效期與換發**：預設 30 天，過了效期一半（剩 15 天）就換發。broker 在叢集內自動換發、不需要人，所以效期短不增加工作，卻限制外洩的私鑰可用的時間：閘道器不查這張憑證本身的撤銷，只能以根 CRL 撤銷簽出它的整個簽發 CA 那一代（[簽發 CA 金鑰遺失或外洩](#簽發-ca-金鑰遺失或外洩)最後一段，根 CRL 在 Laysi/trustiot-v2#560 實作），在那之前只有到期能讓它失效；剩一半時換，certs 或簽發 CA 中斷兩週內都還換得到。每次換發都產生新的私鑰（Laysi/trustiot-v2#535）。簽發 CA 換代時，新的一張由新的一代簽，閘道器信任的是根，不受影響。

### 簽發端點

`src/broker.rs`：`POST /internal/certs/v1/broker-certificate`，只在 broker 埠（`broker.listen_addr`，預設 8082，chart 的埠名 `broker-certs`）。API 埠沒有這個路徑，broker 埠也只有這個路徑。端點本身不驗證呼叫者：授權在 mesh，chart 為這個埠建自己的 Service、AuthorizationPolicy 與 NetworkPolicy `<release>-certs-broker`，只放行 broker 的憑證 CronJob 的 ServiceAccount `<release>-mqtt-broker-certificate`，broker 的 pod、入口 gateway 與其他服務都只到 API 埠（[deploy/chart/README.md](../../deploy/chart/README.md) 的「服務間授權（mesh）」與「MQTT broker」）。拿到這個名稱的憑證就能對每台閘道器冒充 broker，所以它不和 `/internal` 的公開資料共用埠。

請求的本文是 DER 的 PKCS #10 CSR，`Content-Type: application/pkcs10`。每個回應帶 `cache-control: no-store`；錯誤的本文是一行純文字，不重複請求的內容：

| 回應 | 情況 |
| --- | --- |
| 201，JSON | 簽出並寫進簽發紀錄：`serial`、`ca_key_id`、`names`、`not_before`、`not_after`、`certificate_chain`（伺服器憑證接著簽發 CA 憑證，PEM，可以直接當 `tls.crt`） |
| 415 | `Content-Type` 不是 `application/pkcs10`（可以帶參數） |
| 413 | 本文超過 4096 bytes |
| 422 | 不是一個 DER CSR（PEM 也不行）、不是 P-256 或 ecdsa-with-SHA256、簽章對不上金鑰 |
| 503 | 還沒有能簽章的簽發 CA（沒有載入、沒有有效且已有 CRL 的、或剩不到 1 小時），或資料庫失敗；什麼都沒簽 |
| 500 | 簽章失敗 |

每次簽發記一行 INFO `issued the MQTT broker's server certificate`（序號、簽發 CA 的金鑰 id、名稱、到期時間），不含 CSR 或憑證；啟動時記 INFO `serving the MQTT broker's certificate issuance`（位址與名稱）。

Laysi/trustiot-v2#535 原本也寫「雲端服務以我們 CA 的用戶端憑證連 broker」：不適用，Laysi/trustiot-v2#84 決定服務在 1883 以 dynamic security 的帳密加 mesh 的 mTLS 連線，這個 CA 不為服務簽用戶端憑證。

## CRL 與告警

`src/crl.rs`、`src/monitor.rs`（驗收條件 2）：

- 啟動時與之後每 `crl.check_interval`（預設 5 分鐘），拿到工作鎖（`service_kit::job_lock`，所有副本中只有一個）的副本為每個憑證還有效、CRL 剩不到 `crl.renew_before`（預設 5 天）、還沒有 CRL、或有憑證在目前 CRL 的 thisUpdate 之後撤銷（見下面的「撤銷後的 CRL」）的簽發 CA 發一張新的：nextUpdate 是這一輪之後 `crl.validity`（預設 7 天），thisUpdate 往前 5 分鐘（同閘道器憑證的 notBefore，broker 的時鐘稍慢也不會看到還沒生效的 CRL），CRL 編號遞增（資料庫只接受更大的編號，兩個副本重疊也不會倒退）。CRL 列出該 CA 還沒過期的撤銷憑證與撤銷時間、原因；簽發者與 authority key identifier 對得上簽發 CA 的憑證，OpenSSL（Mosquitto）以此找到 CRL。
- **撤銷後的 CRL**（Laysi/trustiot-v2#635）：撤銷的交易提交之後，呼叫端立刻為那張憑證的簽發 CA 重發一張 CRL（`crl::reissue`；處理常式用 `crl::Reissuer::after_revocation`）。它不看 `renew_before`，其餘規則與例行工作相同：thisUpdate 往前 5 分鐘、nextUpdate 是 `crl.validity`、編號加一、列出所有還沒過期的撤銷。它在自己的交易裡，看得到剛提交的撤銷。工作站憑證的撤銷在回應之前等它完成，最多 10 秒（`crl::REISSUE_TIMEOUT`；API 埠的 `http.request_timeout` 較短時取它的一半）；等這個 CA 的鎖（例行工作或另一次重發正在發這個 CA 的 CRL 時）每次最多其中三分之一（Postgres 的 `lock_timeout`，兩次等鎖加起來仍比 10 秒短），逾時在資料庫端就結束，連線回到連線池，不會一直卡著。重發在獨立的 task 執行，客戶端斷線不會中斷它。閘道器憑證的撤銷（Laysi/trustiot-v2#636）沿用同一個函式（`crl::Reissuer::after_revocations`），同一台閘道器在同一個 CA 的幾張憑證合成一張新 CRL。每次重發等鎖時佔著服務角色連線池（預設 5 條）的一條連線，所以每個副本同時最多兩次重發（`crl::MAX_CONCURRENT_REISSUES`，工作站與閘道器的撤銷共用），其餘的不佔連線、在自己的 10 秒內排隊；新 CRL 列出它開始之前提交的每一個撤銷，排隊不會讓撤銷漏列。同時撤銷很多台（大量停權或解綁）時，排不到的重發逾時記 ERROR，由例行工作補上，連線池留給其他請求。
- **立即重發失敗時**（資料庫斷線、逾時、副本停止）：撤銷照樣成立，記一行 ERROR `could not issue a new CRL after a revocation; ...`（金鑰 id、序號、原因）。例行工作補上：簽發 CA 有還沒過期的憑證在目前 CRL 的 thisUpdate 之後撤銷時也重發（`issued a new CRL` 的 `cause` 是 `unlisted_revocation`）。條件只看資料庫，所以不論哪個副本拿到工作鎖、服務是否重啟過，撤銷都最晚一輪 `crl.check_interval` 之後進 CRL。比的是 thisUpdate（那一輪往前 5 分鐘），不是那一輪的時間：在那一輪讀撤銷清單之後才提交的撤銷，以及資料庫與副本相差 5 分鐘以內的時鐘，都還抓得到。代價是撤銷後的 5 分鐘內，每一輪都會多發一張內容相同的 CRL（預設設定下多 1 到 2 張），broker 的 sidecar 因此多重新載入幾次（不中斷連線）。撤銷寫入的 `revoked_at` 取更新那一刻的時鐘（`clock_timestamp()`），不取交易開始的時間，等鎖的時間不會讓它比提交早太多。這個保證的前提是副本與資料庫的時鐘相差不到 5 分鐘，而且撤銷的 UPDATE 到 commit 不超過 5 分鐘（例如同步複寫卡住時可能超過）；不成立又剛好立即重發也失敗時，撤銷仍會在下一次因 `renew_before` 換發時列入，最長約 `crl.validity` 減 `crl.renew_before` 再加一輪（預設約 2 天）。
- **並行**：同一個簽發 CA 的每一張新 CRL（例行或撤銷後）都在一個交易裡，先取這個 CA 的 advisory lock（`crl::LOCK_CA_CRL`），再鎖它的列、讀目前的編號與撤銷清單、寫入。兩者同時執行時一個等另一個：後者讀得到前者的編號與它之前提交的每個撤銷，編號只加一，不倒退、不重複。還沒有 CRL 的 CA 沒有列可鎖，由 advisory lock 涵蓋；只取列鎖的舊版副本（滾動更新期間）在列存在之後仍然與它互斥。
- **撤銷到 broker 拒絕的最長時間**：正常時，撤銷回應時新的 CRL 已經在 `crl.pem`，broker 的 sidecar 再過最多一個 `mqttBroker.trustInterval`（預設 30 秒，[deploy/chart/README.md](../../deploy/chart/README.md) 的「信任檔與重新載入」）載入，之後拒絕那張憑證的新連線；立即重發失敗時，再加最多一輪 `crl.check_interval`（預設 5 分鐘）。重新載入不中斷既有的連線；已經連線的閘道器由撤銷之後的 broker 同步斷開（Laysi/trustiot-v2#636，見[閘道器憑證的撤銷](#閘道器憑證的撤銷)）。
- broker 檢查每張閘道器憑證的簽發者的 CRL；CRL 過期後，那個簽發 CA 的閘道器全部被拒絕，所以 CRL 不能斷。每個副本每 `crl.check_interval` 讀一次每個 CRL 的 nextUpdate，以 OTLP 匯出指標（屬性 `certs.ca.key_id`、`certs.ca.generation`）：

| 指標 | 意義 |
| --- | --- |
| `certs.crl.remaining`（秒） | 距 CRL 的 nextUpdate 的秒數，過了是負數；還沒有 CRL 時沒有資料點 |
| `certs.crl.expiring`（0 或 1） | CRL 剩不到 `crl.alert_before`（預設 3 天）或還沒有 CRL 時是 1；這段期間每次檢查都記一行 ERROR |
| `certs.ca.remaining`（秒） | 距簽發 CA 憑證到期的秒數，給輪替用（見[輪替](#輪替)） |
| `certs.ca.signing`（0 或 1，沒有屬性） | 有效期內的簽發 CA 憑證存在、服務能簽章時是 1（新的簽發 CA 在第一張 CRL 之前也算，見[發簽發 CA 憑證](#發簽發-ca-憑證)第 5 步）；是 0 時每次檢查記一行 WARN 並列出等待憑證的金鑰 |

指標的值在讀取時從上次讀到的時間算出，資料庫連不上時仍繼續倒數。Grafana 的告警（決策 23，[可觀測性](../../docs/architecture/observability.md)的「憑證到期」，Laysi/trustiot-v2#183）在 `certs.crl.expiring` 是 1、或這個序列沒有資料（服務停了）時觸發。`renew_before` 必須介於 `alert_before` 與 `validity` 之間，且兩者的差至少是兩次檢查，所以正常時重發一輪失敗也不會告警。

**根 CRL**（Laysi/trustiot-v2#830，見[根 CRL](#根-crl)）：根 CRL 由離線的根 CA 在儀式簽，服務不能自己重發，只能提早提醒。每個副本匯出下面三個指標（沒有屬性），值在讀取時從採用的根 CRL 與當下的時間算出；每 `crl.check_interval` 檢查一次，剩不到 60 天（`root_crl::ALERT_BEFORE_SECS`）或已過期時記 WARN `the root CRL expires in less than 60 days or has expired; ...`（`number`、`next_update`、`remaining_seconds`、`expired`），不能採用時記 ERROR（見「根 CRL」的「不能採用時」）。還沒有根 CRL（沒設或空檔）是正常狀態，兩個告警指標都是 0。

| 指標 | 意義 |
| --- | --- |
| `certs.root_crl.remaining`（秒） | 距採用的根 CRL 的 nextUpdate 的秒數，過了是負數；沒有根 CRL 時沒有資料點 |
| `certs.root_crl.expiring`（0 或 1） | 採用的根 CRL 剩不到 60 天或已過期時是 1；過期後仍採用、仍是 1，直到換上新的一張 |
| `certs.root_crl.invalid`（0 或 1） | 根 CRL 檔不能採用（別的根簽的、格式不符、讀不到）時是 1；這時閘道器拿到的是保留的上一張或沒有 |

Grafana 在 `certs.root_crl.expiring` 或 `certs.root_crl.invalid` 是 1 時告警（可觀測性的「憑證到期」）；`invalid` 的規則要持續至少兩個 `ca.reload_interval`（`for:`，預設設定下 1 分鐘以上）才觸發：kubelet 換 ConfigMap 的那一刻，根憑證與根 CRL 可能分在兩次讀取之間，一輪以舊根檢查新 CRL（或反過來），記一行 ERROR、`invalid` 是 1，下一輪自己恢復，不會採用錯的 CRL。60 天是給辦一次根 CA 儀式的時間：要約人到離線的機器（[發簽發 CA 憑證](#發簽發-ca-憑證)），根 CRL 的效期是 400 天，所以正常每年的儀式之後不會告警。

## 端點

`/internal` 底下只有公開資料：入口不轉送 `/internal`（決策 27），叢集內只有 `mesh.calls` 列出 certs 的服務，以及 MQTT broker 的 pod（`mqttBroker.enabled` 時由 chart 自動加上，Laysi/trustiot-v2#84）連得到。

| 端點 | 回應 |
| --- | --- |
| `GET /internal/certs/v1/crl.pem` | 每個憑證還有效的簽發 CA 的目前 CRL，PEM（Mosquitto 的 `crlfile`）。其中一個還沒有 CRL 時 503，取用端保留手上的檔案；沒有任何可用的簽發 CA 時是空檔，broker 因此改用不信任任何閘道器的佔位 CA，拒絕每張閘道器憑證（Mosquitto 載入空的 CRL 檔會停止，所以 broker 不直接用它） |
| `GET /internal/certs/v1/ca.pem` | 根憑證，接著每個可用的簽發 CA 憑證，PEM。還沒載入 CA 時 503 |

broker 埠（`broker.listen_addr`，預設 8082）只有一個端點，mesh 只放行 MQTT broker 的憑證 CronJob 的 pod（見[broker 伺服器憑證](#broker-伺服器憑證)的「簽發端點」）：

| 端點 | 回應 |
| --- | --- |
| `POST /internal/certs/v1/broker-certificate` | 201，簽出並記錄的 broker 伺服器憑證與鏈 |

給平台管理員的 API 在 `/api/certs/v1/` 底下，經入口的 cookie 路由、驗內部身分 token（用法與錯誤見[工作站憑證](#工作站憑證)的「簽發、撤銷與換發」，`src/workstations.rs`）：

| 端點 | 回應 |
| --- | --- |
| `POST /api/certs/v1/workstation-certificates` | 201，簽出並記錄的工作站憑證 |
| `POST /api/certs/v1/workstation-certificates/{serial}/revoke` | 200，撤銷後的紀錄 |
| `POST /api/certs/v1/gateway-registrations` | 201 新登錄、200 重送，登錄紀錄與閘道器憑證；另外要工作站的簽章（[出廠登錄](#出廠登錄)，`src/registrations.rs`） |
| `POST /api/certs/v1/gateways/{hardware_id}/revoke` | 200，撤銷的每一張閘道器憑證（[閘道器憑證的撤銷](#閘道器憑證的撤銷)，`src/gateways.rs`） |

規格在 `api/openapi.json`（`tools/openapi` 的 `SERVICES`；`/internal` 的端點不在其中），請求與回應的型別在 `crates/contract` 的 `certs` 模組。

閘道器的換發在 gateway 主機的公開路徑上，入口不驗 cookie（決策 27 的明列例外），由閘道器的 TPM 簽章驗證（[換發](#換發)，`src/renewal.rs`）。它是閘道器的協定，不是前端的 API，所以不在 `api/openapi.json`，格式在 `crates/protocol` 的 `renewal` 模組：

| 端點 | 回應 |
| --- | --- |
| `POST /certs/renew` | 取挑戰 200；兌換 201，新的閘道器憑證、鏈與根憑證 |

閘道器取根 CRL 也在 gateway 主機的公開路徑上（2026-10-31 起，Laysi/trustiot-v2#831；決策 27 的明列例外，chart 的 `ingress.publicPaths.rootCrl`）。根 CRL 是公開資料、由離線的根 CA 簽章，閘道器自己檢查（[crates/root-crl](../../crates/root-crl/README.md)，與 certs 採用前的檢查相同），所以請求不帶 cookie，也不需要任何憑證。它是閘道器的協定，不在 `api/openapi.json`；路徑與媒體類型在 `crates/protocol` 的 `root_crl` 模組（`ROOT_CRL_PATH`、`ROOT_CRL_CONTENT_TYPE`）。這一段是閘道器端定期取用（Laysi/trustiot-v2#563）的契約（`src/http.rs` 的 `gateway_routes`，測試在 `tests/root_crl_endpoint.rs`）：

| 端點 | 回應 |
| --- | --- |
| `GET /certs/root-crl` | 200，採用中的根 CRL（PEM）；沒有可交出的根 CRL 時 404 |
| `HEAD /certs/root-crl` | 與 `GET` 相同的狀態與標頭，沒有本文 |
| 其他方法 | 405，`Allow: GET,HEAD`，沒有本文 |

- **200**：`authority::CaState::root_crl` 採用的那一張（[根 CRL](#根-crl)的「交給閘道器的介面」），一個 PEM `X509 CRL`，每行 64 字元：certs 採用時重新編碼的那份，最多 64 KiB（`root_crl::MAX_BYTES`，採用時就檢查過，比上限大的不採用）。`Content-Type: application/x-pem-file`，與 `/internal/certs/v1/` 的檔案相同。用 PEM 不用 DER 的 `application/pkix-crl`：agent 載入的設定檔（`broker.crl`）與出廠登錄交的都是 PEM，閘道器存下來就能直接用；`application/pkix-crl` 依 RFC 2585 是 DER 的媒體類型，用在 PEM 上會誤導。
- **404**：沒有可交出的根 CRL：values 的 `certificateAuthority.rootCrl` 還是空的（第一次儀式之前），或換了根之後還沒有新的根簽的那一張（「不能採用時」）。本文固定是 `no root CRL is published`（`text/plain; charset=utf-8`），不說是哪一種、不帶內部資訊。不用 503：這是正常、可能持續好幾個月的狀態，5xx 會讓入口與監控當成故障；不用 204：把 2xx 當成功的用戶端會去解析空的本文。檔案裡的根 CRL 不能採用時，服務照「只往前」繼續提供保留的上一張，所以回滾或改錯 values 不會讓閘道器拿到較舊的那張，也不會變成 404。
- **快取**：每個回應（含 404 與 405）都是 `Cache-Control: no-store`。內容只在儀式後改變，但閘道器取根 CRL 是為了在簽發 CA 外洩時盡快拒絕它，路上的快取不能把新的撤銷擋住。沒有 `ETag` 與條件式請求：根 CRL 只有幾百位元組到幾 KiB，閘道器比對 CRL 編號就知道要不要換，省下的流量不值得多一套快取語意。
- **防護**（對外開放前的安全關卡）：處理常式不讀請求的任何部分，本文、標頭與查詢字串都不影響回應，宣告了本文卻不送的請求也立刻得到回應；不碰資料庫，只回記憶體裡採用的那一份，所以沒有服務內的限流（每個請求只複製最多 64 KiB）；以來源 IP 限流屬於入口，與換發相同（[換發](#換發)的「規則與取捨」）。**對外開放的前提**：每個請求都會留一行存取日誌，沒有限流的公開路徑能被拿來放大日誌量，所以讓閘道器經網際網路取用之前（Laysi/trustiot-v2#563），入口的速率限制要涵蓋這個路徑並實測拿到 429；展示環境的做法在 [runbook](../../docs/runbooks/demo-environment.md) 的「架構」，地端入口的限流與換發一樣還沒有（Laysi/trustiot-v2#113）。入口只以 Exact 比對路由這一個路徑，送到 API 埠（不是 broker 埠），certs 的其他路徑在 gateway 主機照樣要 cookie，`/internal` 照舊拒絕（`deploy/chart/tests/chart.test.ts` 的路由清單證明）。
- **閘道器端**（Laysi/trustiot-v2#563 照做）：200 時以 `crates/root-crl` 檢查，只換上 CRL 編號比手上大的那張；200 以外（包括入口或網路的錯誤）都保留手上的根 CRL，下次輪詢再取。**路徑固定**：服務只在 `/certs/root-crl` 提供，chart 的 `ingress.publicPaths.rootCrl` 只決定入口的路由，改成別的路徑時入口與服務對不上。
- **展示環境**：chart 已為 gateway 主機掛上這條路由，但展示環境的 tunnel 還沒讓它略過 Cloudflare Access，閘道器在那裡的請求會被 Access 擋下；等閘道器端要用時照 [展示環境 runbook](../../docs/runbooks/demo-environment.md)「Cloudflare 上的設定」開放（Laysi/trustiot-v2#563）。

## 設定

設定由 `service_kit::config` 載入（kit README 的「設定」）。時間長度的寫法是 `30s`、`5m`、`7d` 這類（humantime）。

| 變數 | 說明 |
| --- | --- |
| `TRUSTIOT_POSTGRES_HOST`、`_PORT`、`_DATABASE`、`_SSLMODE`、`_CA_FILE` | 平台設定：Postgres 位置；只接受 `verify-full`，`_CA_FILE` 必填 |
| `TRUSTIOT_INTERNAL_IDENTITY_KEYS_FILE` | 平台設定：身分服務的公鑰（JWK Set），驗內部身分 token 用；chart 在身分服務啟用時設定，檔案變更時不必重啟就重讀。沒有這個變數時服務不啟動 |
| `TRUSTIOT__DATABASE__USERNAME`、`TRUSTIOT__DATABASE__PASSWORD` | 服務角色 `certs_app` 的帳密：密碼來自 Secret `<release>-certs-password` 的 `certs_app` key |
| `TRUSTIOT__DATABASE__PLATFORM_USERNAME`、`TRUSTIOT__DATABASE__PLATFORM_PASSWORD` | 平台角色 `certs_platform` 的帳密（出廠登錄寫全平台事件、讀資產服務與身分服務的事件、跨租戶讀綁定與租戶狀態用）：密碼來自同一個 Secret 的 `certs_platform` key；帳號不能和服務角色相同，密碼不能是空的 |
| `TRUSTIOT__DATABASE__MAX_CONNECTIONS` | 每個副本服務角色的連線上限，2–100，預設 5；平台角色另有最多 4 條；chart 以 `services.certs.database.maxConnections` 設定。連線池的大小或數量改變時，chart 的連線預算（`deploy/chart/templates/_postgres.tpl`，見 chart README 的「連線預算」）要一起改，`tests/chart_connections.rs` 比對兩者 |
| `TRUSTIOT__CA__KEYS_FILE` | 簽發 CA 金鑰檔；chart 設成 `/etc/trustiot/certs-ca-keys/issuing-ca-keys.json` |
| `TRUSTIOT__CA__ROOT_CERTIFICATE_FILE` | 根憑證；chart 設成 `/etc/trustiot/certs-ca/root.pem` |
| `TRUSTIOT__CA__CERTIFICATES_FILE` | 簽發 CA 憑證；chart 設成 `/etc/trustiot/certs-ca/issuing.pem` |
| `TRUSTIOT__CA__ROOT_CRL_FILE` | 選填：根 CRL（Laysi/trustiot-v2#830）；chart 設成 `/etc/trustiot/certs-ca/root-crl.pem`，沒設 `certificateAuthority.rootCrl` 時是空檔。要是絕對路徑。沒有這個變數或檔案是空的表示沒有根 CRL；有內容時以根憑證檢查，通過才採用（見[根 CRL](#根-crl)） |
| `TRUSTIOT__CA__RELOAD_INTERVAL` | 多久重讀上面四個檔案，1 秒到 1 小時，預設 `30s`。檔案沒變時不重建；變了但不能用時保留上一份可用的，ERROR `cannot load the issuing CA` 每個問題只記一次（根 CRL 的規則見[根 CRL](#根-crl)） |
| `TRUSTIOT__GATEWAY__VALIDITY` | 閘道器憑證效期，1 小時到 398 天，預設 `90d` |
| `TRUSTIOT__WORKSTATION__VALIDITY` | 工作站憑證效期，1 小時到 398 天，預設 `180d`（理由見[工作站憑證](#工作站憑證)） |
| `TRUSTIOT__BROKER__LISTEN_ADDR` | broker 伺服器憑證的簽發埠，預設 `0.0.0.0:8082`；chart 設成 `services.certs.ports` 的 `broker-certs`。被占用時服務不啟動 |
| `TRUSTIOT__BROKER__VALIDITY` | broker 伺服器憑證效期，1 小時到 90 天，預設 `30d`（理由見[broker 伺服器憑證](#broker-伺服器憑證)） |
| `TRUSTIOT__BROKER__SERVICE_NAME` | 憑證在 `mqtt-broker.trustiot.internal` 之後的第二個 DNS 名稱：小寫的 DNS 名稱，兩段以上、每段 1 到 63 個英數字或中間的 `-`，不能有萬用字元；預設沒有。chart 在 broker 開啟時設成 `<release>-mqtt-broker.<namespace>.svc` |
| `TRUSTIOT__CRL__CHECK_INTERVAL` | CRL 工作與指標的週期，至少 1 秒，預設 `5m` |
| `TRUSTIOT__CRL__VALIDITY` | 新 CRL 的效期，最多 31 天，預設 `7d` |
| `TRUSTIOT__CRL__RENEW_BEFORE` | 剩不到這麼久就重發，預設 `5d` |
| `TRUSTIOT__CRL__ALERT_BEFORE` | 剩不到這麼久就告警，預設 `3d` |
| `TRUSTIOT__RENEWAL__GATEWAY_BURST` | 換發：每個硬體 ID 一次可做的簽章驗過的兌換數（簽章之前的請求不算，Laysi/trustiot-v2#801），1–100，預設 `2` |
| `TRUSTIOT__RENEWAL__GATEWAY_INTERVAL` | 換發：每個硬體 ID 每隔多久多一個簽章驗過的兌換，1 秒到 1 小時，預設 `60s` |
| `TRUSTIOT__RENEWAL__GLOBAL_BURST` | 換發：每一步（取挑戰、兌換各自計算）全部硬體 ID 一次可送的請求數，1–10000，預設 `100` |
| `TRUSTIOT__RENEWAL__GLOBAL_INTERVAL` | 換發：每一步全部硬體 ID 每隔多久多一個請求，1 ms 到 60 秒，預設 `50ms`（每一步每秒 20 個） |
| `TRUSTIOT__RENEWAL__MAX_CONCURRENT` | 換發：每個副本同時做資料庫工作的請求數，1–3（平台角色的 4 條連線減 1），預設 `2` |
| `TRUSTIOT__RENEWAL__MAX_CONSUMER_LAG` | 換發：消費端最舊的未處理事件超過這麼久（或 lag 不明）就回 503，1 秒到 1 小時，預設 `60s` |
| `TRUSTIOT__RENEWAL__REFUSAL_WINDOW` | 換發：已登錄的硬體 ID 被拒的請求合併成一筆事件的時間窗，1 分鐘到 1 天，預設 `1h` |
| `TRUSTIOT__RENEWAL__REFUSAL_FLUSH_INTERVAL` | 換發：每個副本多久找一次結束的時間窗寫出事件，1 秒到 1 小時，預設 `60s` |
| `TRUSTIOT__MQTT__HOST`、`TRUSTIOT__MQTT__PORT` | MQTT broker 的服務埠（[broker 權限](#broker-權限dynamic-security)），埠預設 1883。整組 `TRUSTIOT__MQTT__*` 都沒設時不同步閘道器的權限 |
| `TRUSTIOT__MQTT__USERNAME` | 只能是 `service:certs`（唯一有 dynsec 管理角色的帳號） |
| `TRUSTIOT__MQTT__PASSWORD_FILE` | broker 帳號密碼檔的絕對路徑，每次連線重讀 |
| `TRUSTIOT__DYNSEC__INTERVAL` | 完整比對的週期，1 秒到 1 小時，預設 `5m` |

三個 `TRUSTIOT__CA__*` 由 chart 設定，自訂 `env` 不能覆寫。`TRUSTIOT__RENEWAL__*` 沒有 chart 的欄位，預設值不合用時以 `services.certs.env` 設定：和 `TRUSTIOT__GATEWAY__VALIDITY`、`TRUSTIOT__CRL__*` 一樣，chart 只為它要協調的設定開欄位（連線預算、埠、檔案與位址），而 `renewal.max_concurrent` 由服務限制在平台角色的連線池之內，不影響 chart 的連線預算；限流是每個副本各自計算（見[換發](#換發)的「限流與資源上限」）。健康埠（預設 8081）只有 `/healthz` 與 `/readyz`，端點在 8080，broker 伺服器憑證的簽發在 8082。readiness 要服務角色開得出通過 service-kit session 檢查的連線、有一組可用的內部身分公鑰，而且資料庫有這一版預期的每個 migration 檔案；能不能簽章不影響 readiness，看指標 `certs.ca.signing`（理由見[發簽發 CA 憑證](#發簽發-ca-憑證)第 2 步）。設定不能用時以 exit code 1 結束。

## 資料庫

schema `certs`，DDL 在 `migrations/postgres/`，由 migration Job（登記在 `tools/migrate/src/registry.rs` 的 `PRODUCT`；從 Laysi/trustiot-v2#79 起產生事件、有平台角色，從 Laysi/trustiot-v2#632 起消費 `assets` 的事件，從 Laysi/trustiot-v2#778 起也消費 `identity` 的事件）以 migration 角色套用。outbox 的表（產生端的 `outbox_event`、`outbox_aggregate` 與它們的租戶政策，消費端的 `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version`）由 Job 依 `crates/outbox` 建立，同每個產生者與消費端。檔案清單在 `src/schema.rs`，Job 與服務的 readiness 編譯同一份。

| 角色 | 權限 |
| --- | --- |
| `certs_owner`（migration 角色） | 擁有 schema 與所有表 |
| `certs_app`（服務角色） | `issued_certificates` 的 `SELECT`、`INSERT`，以及只限 `revoked_at`、`revocation_reason` 兩欄的 `UPDATE`（`0002_workstation_certificates.sql`，Laysi/trustiot-v2#534；閘道器的撤銷 Laysi/trustiot-v2#82 沿用），改不了簽出的內容、也不能刪除；`crls` 的 `SELECT`、`INSERT`、`UPDATE`；登錄的兩張表完全碰不到。`0005_gateway_bindings.sql`（Laysi/trustiot-v2#632）：`gateway_bindings` 只有 `INSERT`，`tenant_statuses` 的 `SELECT`、`INSERT`、`UPDATE`，都受 RLS 限制在交易的租戶；消費端的四張表（Job 授權）。`0006_certificate_renewal.sql`（Laysi/trustiot-v2#81）：`renewal_challenges` 的 `SELECT`、`INSERT`、`DELETE`；換發的簽發紀錄與租戶事件在閘道器租戶的交易裡寫（見[換發](#換發)的「權限」） |
| `certs_platform`（平台角色，Laysi/trustiot-v2#79） | `0004_gateway_registrations.sql`：`crls` 的 `SELECT`；`issued_certificates` 與 `gateway_registrations` 的 `SELECT`、`INSERT`；`workstation_request_nonces` 的 `SELECT`、`INSERT`、`DELETE`；outbox 的寫入（Job 授予每個產生者的平台角色）。`0005_gateway_bindings.sql`：`gateway_bindings` 與 `tenant_statuses` 的 `SELECT`（`platform_access`，所有租戶）；`assets.outbox_event` 的 `SELECT`（Job 授予消費端的平台角色），Laysi/trustiot-v2#778 起還有 `identity.outbox_event` 的 `SELECT`。`0006_certificate_renewal.sql`：`renewal_refusals` 的 `SELECT`、`INSERT`、`UPDATE`、`DELETE`；換發讀登錄、綁定與租戶狀態都用上面已有的 `SELECT`，在唯讀交易裡、只取需要的欄位。除了 `renewal_refusals` 沒有任何 `UPDATE` |

撤銷是最終的，對所有角色都一樣，包括擁有者與 superuser：`0002_workstation_certificates.sql` 的 trigger `issued_certificates_guard` 在每次 `UPDATE` 前檢查兩件事。一是撤銷過的列不能再改：不能收回撤銷、改撤銷時間或原因，否則有問題或被入侵的服務能讓憑證從下一張 CRL 消失。二是 `UPDATE` 只能改撤銷的兩欄：欄位權限已經這樣限制服務角色，trigger 再擋下擁有者與 superuser；它比對撤銷兩欄以外的整列，所以之後新增的欄位也不能改，除非新的 migration 另外處理。違反時錯誤是 SQLSTATE 42501（`insufficient_privilege`），訊息寫明序號。服務撤銷時只更新 `revoked_at IS NULL` 的列，不會碰到 trigger；trigger 只有擁有者能停用（`ALTER TABLE`），只有 superuser 能略過（`session_replication_role`）。

| 表 | 內容 | RLS |
| --- | --- | --- |
| `issued_certificates` | 每張簽出的憑證：序號（主鍵）、簽發 CA 的金鑰 id、profile（`gateway`、`workstation` 或 `broker`；`broker` 由 `0003_broker_certificates.sql` 加入）、CN、效期、簽發時間、DER，撤銷時間與原因 | 無：不是租戶資料。閘道器憑證屬於閘道器，閘道器屬於哪個租戶是資產服務的綁定（見[閘道器的租戶歸屬與換發條件](#閘道器的租戶歸屬與換發條件)；certs 的副本在 `gateway_bindings`）；產線工作站與 MQTT broker 屬於平台，不屬於租戶；之後在這裡存租戶 id 時要改成租戶表 |
| `crls` | 每個簽發 CA 目前的 CRL：金鑰 id（主鍵）、CRL 編號、thisUpdate、nextUpdate、DER | 無：公開資料 |
| `gateway_registrations` | 每台登錄的閘道器：硬體 ID（主鍵）、TPM 金鑰的公鑰（唯一）、EK 與它的 SHA-256（使用中的列之間唯一）、第一張憑證的序號、登錄的工作站與它的憑證序號、平台管理員、登錄時間、除役時間。trigger 讓所有角色（含擁有者與 superuser）都不能刪除、清空或改寫，只能把 `decommissioned_at` 從 NULL 設定一次，違反時 SQLSTATE 42501 | 無：閘道器出廠時不屬於任何租戶，之後由資產服務指派 |
| `workstation_request_nonces` | 工作站請求用過的 nonce：憑證序號、nonce、過期時間；過了時間的接受範圍才刪除 | 無 |
| `gateway_bindings` | 資產服務的每個綁定（[閘道器的綁定與租戶狀態](#閘道器的綁定與租戶狀態)）：租戶與綁定 ID（主鍵）、硬體 ID、`bound_at`、事件在資產服務 log 的位置 `position_tx`、`position_id`、留給 Laysi/trustiot-v2#115 的 `unbound_at`、`recorded_at`。索引 `gateway_bindings_latest`（硬體 ID、位置由新到舊）給平台角色的跨租戶查詢，是「每個索引以 `tenant_id` 開頭」刻意的例外 | `tenant_isolation`；`certs_platform` 另有唯讀的 `platform_access` |
| `tenant_statuses` | 每個租戶的狀態：租戶（主鍵）、`active`、`suspended`、`deleted` 或 `unrecognized`（後兩者 `0007_tenant_state_events.sql`，Laysi/trustiot-v2#778）、版本號（不小於 0）、`updated_at` | `tenant_isolation`；`certs_platform` 另有唯讀的 `platform_access` |
| `renewal_challenges` | 換發的挑戰（[換發](#換發)）：nonce（主鍵，32 bytes）、硬體 ID、用途、到期時間（Unix 秒，資料庫時鐘）、`created_at`。兌換時先刪除；每次取挑戰刪掉過期超過 120 秒的列 | 無：沒有租戶資料，挑戰對有沒有綁定的閘道器都一樣 |
| `renewal_refusals` | 已登錄的硬體 ID 每個時間窗被拒的次數：硬體 ID（外鍵到 `gateway_registrations`）與時間窗開始（主鍵）、時間窗結束、次數、每個原因的次數（jsonb）。時間窗回報成事件時刪除 | 無：只有平台角色碰得到，被拒時還不知道屬於哪個租戶 |
| `outbox_subscription`、`outbox_inbox`、`outbox_dead_letter`、`outbox_version` | 消費端的訂閱位置、處理過與進 dead letter 的事件 ID（`crates/outbox`） | 無：不是租戶資料 |
| `trustiot_migrate_applied` | migration Job 套用過的檔案，`certs_app` 只能 `SELECT` | 無 |

這裡不存任何私鑰：簽發 CA 私鑰在 Secret，根私鑰離線，閘道器的私鑰不離開 TPM，工作站的私鑰不離開工作站，broker 的私鑰只在 broker 的 Secret 與產生它的 CronJob 的記憶體裡。

## 映像

`deploy/images/certs/image.toml`：與其他服務相同，交叉編譯成 `aarch64-unknown-linux-musl` 的靜態執行檔（只有 `certs`，`certs-ca` 不進映像），以 `deploy/images/rust-binary.Containerfile` 包成映像。chart 的 `services.certs.enabled` 預設 `false`；2026-10-23（Laysi/trustiot-v2#517）起 nightly 從原始碼建置它、以兩份 values 啟用，並以每次執行自建的測試根 CA 照上面「發簽發 CA 憑證」的步驟走一次（`certs-ca` 隨映像的建置一起編出，只在 runner 上執行；檢查項目見 [CI 與發佈](../../docs/architecture/ci-release.md)「nightly.yml」的「做什麼」第 12 項）。展示環境的啟用是後續工作。chart 的設定在 [deploy/chart/README.md](../../deploy/chart/README.md) 的「閘道器憑證 CA」。

## 測試

在 repo 根目錄執行：

```sh
CARGO_BUILD_JOBS=2 cargo test -p certs
```

標註需要容器的測試以 `testkit::product` 起一個帶 TLS 的暫時 Postgres（podman 或 docker），由 migration Job 的程式套用整個產品的 schema，再以 `certs_app`（與平台角色 `certs_platform`）啟動真的 binary；沒有容器時略過，設了 `CI` 時改為失敗。

| 測試 | 驗證 |
| --- | --- |
| `tests/issuance.rs` | 驗收條件 1：要求別的 CN、SAN、serverAuth、codeSigning、各種 keyUsage 與 CA 的 CSR，簽出的都是 CA 決定的 profile，並能在根之下驗證為 TLS 用戶端憑證、不能當伺服器憑證或簽其他憑證；別的曲線或演算法、驗不過的簽章、改過的主體、過大或多餘的位元組都拒絕；代最大的有效簽發 CA 簽章；簽發 CA 還沒有 CRL 時簽發紀錄寫不進去，有了才寫入且內容正確；`certs_app` 只能改撤銷的兩欄，改不了簽出的內容、也刪不掉；撤銷之後 `certs_app` 與 superuser 都不能收回、改撤銷時間或原因，superuser 也改不了簽出的內容，被 trigger 以 42501 拒絕、列不變（容器） |
| `tests/crl.rs` | 驗收條件 2：CRL 的內容，以及 TLS 伺服器依它拒絕撤銷的憑證、CRL 過期後拒絕全部；新 CRL 的 nextUpdate 減 thisUpdate 是 `crl.validity` 加往前的 5 分鐘；真的服務依排程為每個簽發 CA 重發 CRL 並由 `crl.pem` 發布；以 superuser 拿住工作鎖、把 nextUpdate 調近時 `certs.crl.expiring` 經 OTLP 變成 1 並記 ERROR，放開後工作重發、指標回到 0。Laysi/trustiot-v2#635：撤銷後 `crl::reissue` 不看 `renew_before` 發出下一個編號、列出它、thisUpdate 與效期照規則；撤銷了卻沒有重發時，CRL 還沒到 `renew_before` 的下一輪也重發並列出它，thisUpdate 超過撤銷時間後不再重發；不認得的金鑰 id 拒絕；24 個撤銷各自重發、同時 4 個 task 跑例行工作時，每個編號只用一次、觀察到的編號不倒退，每張編號不小於某次重發的 CRL 都列出那次重發之前的撤銷（容器） |
| `tests/key_custody.rs` | 驗收條件 3：程式檢查，只有列出並說明理由的位置能把私鑰變成位元組或文字；執行時，日誌開到 `trace`、開 OTLP、有等待中的金鑰、重讀與兩種壞掉的金鑰檔，日誌、端點（含簽發工作站憑證與 broker 伺服器憑證的回應，以及 broker 埠的其他路徑）與遙測都不含私鑰的任何片段（PKCS#8 涵蓋私鑰純量的任 24 個 base64 字元，其餘部分是演算法與公鑰，簽發 CA 憑證本來就公開；私鑰純量的十六進位、base64 與十進位位元組）（容器） |
| `tests/ceremony.rs` | 真的 `certs-ca`：`root create` 的私鑰只到 stdout、`root sign` 只從 stdin 讀、簽出的簽發 CA 服務接受並印出金鑰 id、`root check` 認得自己的私鑰；別的私鑰、P-256 私鑰、根的名稱、0 天、已存在的輸出檔都拒絕，且不印出私鑰；真的服務在儀式前就緒、`certs.ca.signing` 是 0、每次檢查記 WARN 與等待中的金鑰 id，掛上憑證後不重啟就變成 1 並發出第一張 CRL（容器） |
| `tests/ceremony_root_crl.rs` | Laysi/trustiot-v2#829：`certs-ca root crl` 與 `ceremony::root_crl`。webpki 照閘道器 agent 的內層 TLS 設定（rustls 的 `WebPkiServerVerifier`、帶根 CRL、允許狀態未知）時，撤銷的那一代簽的 broker 伺服器憑證以 `Revoked` 拒絕、另一代的接受；同名但別的金鑰簽的 CRL 是 `BadSignature`，所以 webpki 確實以根的公鑰驗了 CRL。DER 欄位：version 2、ecdsa-with-SHA384 的 AlgorithmIdentifier 不帶參數且逐位元組出現兩次、簽發者等於根的主體、只有 authority key identifier（等於根的 subject key identifier）與 CRL 編號、沒有 critical、nextUpdate 是 thisUpdate 後 400 天。固定時鐘下第一張是 1、之後加一，上一張的項目保留原本的撤銷時間與原因，憑證過期後仍列著，過期滿 400 天那一刻（thisUpdate 等於 notAfter 加 400 天）還在、晚一秒拿掉，檔案裡拿掉的憑證之後被略過；這次才撤銷的憑證可以先放進檔案；過期不到 400 天的憑證仍可撤銷；每一項帶金鑰 id，上一張與新的 SHA-256 相符。拒絕：別的根或同名別的金鑰簽的上一張、thisUpdate 晚於時鐘的上一張、缺憑證的項目、還在保留期卻沒被列出的憑證、別的根或同名別的金鑰簽的、根自己、不是 CA、已撤銷、過期超過 400 天的要撤銷的憑證、沒有憑證的要撤銷的檔案、別的私鑰、過期的根；執行檔另外拒絕已存在的輸出檔、終端機的 stdin（pseudo-terminal）、沒有 `--previous-crl`、沒有 `--previous-certificates`、空的 `--revoke` 檔、`--revoke` 沒有 `--reason`、不認得的原因，都不寫檔、不印出私鑰（`-- --nocapture` 印出每個拒絕的訊息） |
| `tests/root_crl_endpoint.rs` | Laysi/trustiot-v2#831：閘道器取根 CRL 的[端點](#端點)。沒有根 CRL 時 `GET`、`HEAD` 是 404 與固定本文；採用之後 200、本文就是採用的那張（DER 等於檔案、不超過 `root_crl::MAX_BYTES`），`HEAD` 200 沒有本文；帶 cookie、偽造的 `x-internal-identity`、查詢字串、Range，以及宣告 1 MiB 本文卻不送的請求，得到同樣的 200；換一張就換，換了沒有自己根 CRL 的根又是 404。其他六種方法 405 與 `Allow: GET,HEAD`、沒有本文；路徑的變形（結尾斜線、子路徑、`.pem`、大小寫、`%2f`）、換發路徑與 `/internal` 的檔案在這組路由上都是 404。全部都是 `no-store`。真的服務（容器）在 API 埠上：寫入根 CRL 檔後不重啟就從 404 變成 200，broker 埠沒有這條路徑 |
| `tests/cross_tenant.rs` | [crates/testkit](../../crates/testkit/README.md) 的覆蓋檢查：服務角色碰得到的每張表、規格裡的每個端點都要宣告；CA 的表都不是租戶資料而豁免（broker 伺服器憑證也屬於平台），outbox 的兩張表照租戶表探測（服務角色寫不了別的租戶的事件），所以不再以 `without_tenant_data` 宣告；Laysi/trustiot-v2#632 的 `gateway_bindings` 與 `tenant_statuses` 照租戶表探測（每個租戶一列的 `one_per_tenant`），消費端的四張表豁免；登錄的表服務角色碰不到；工作站憑證的兩個端點、出廠登錄與閘道器憑證的撤銷只給平台管理員，豁免並說明理由；broker 的簽發端點在自己的埠上，不在規格裡；Laysi/trustiot-v2#81 的 `renewal_challenges` 沒有租戶資料而豁免，`renewal_refusals` 服務角色碰不到，換發端點是閘道器的協定、不在規格裡（容器） |
| `tests/permissions.rs` | [權限矩陣](../../docs/architecture/permissions.md)（Laysi/trustiot-v2#584）：`openapi()` 的四個操作（Laysi/trustiot-v2#636 加上閘道器憑證的撤銷）都宣告為「平台管理員，本人」，與 OpenAPI 文件、矩陣文件的 `## certs` 一節相同（不需要資料庫）；真的服務上，沒有 token、偽造的 token（直接送到服務、不經入口）、給別的服務的 token 都 401，沒有租戶的使用者、租戶的 `viewer`、`editor`、`admin`、不認得的角色與換發的平台管理員 token 都 403，平台管理員本人的請求通過授權；探測前後簽發與撤銷的帳本、出廠登錄、nonce 與 outbox 都不變（容器） |
| `tests/renewal.rs` | Laysi/trustiot-v2#81：以工作站登錄閘道器（軟體 P-256 金鑰代替 TPM）、以 `assets_app` 寫 `assets.gateway_bound` 綁定。唯一的憑證被撤銷後換發成功：CN 是硬體 ID、公鑰是登錄的那一把、鏈接上簽發 CA 與根並在根之下驗證為用戶端憑證、`no-store`；簽發紀錄多一列，一筆租戶事件（租戶、序號、綁定 ID、`ca_key_id`）與它同一個交易，讀取的日誌帶硬體 ID 與租戶。租戶以 `store_tenant_status` 存成正常（Laysi/trustiot-v2#778）。除役、沒綁定過、綁定已結束、租戶停權、租戶已刪除、租戶沒有狀態紀錄都 403，不簽、不寫簽發紀錄與事件。重送 409；別的金鑰簽 401 且挑戰作廢；別台的挑戰 403 且對那台也作廢；別的用途的挑戰 403；過期 410 再送 409；沒登錄的硬體 ID 拿得到同樣的挑戰、兌換 401；格式錯 422。每次拒絕有 INFO 日誌、`certs.renewal.refused` 依原因的計數（OTLP）；已登錄的硬體 ID 的 7 次拒絕合併成時間窗的列，時間窗結束後成為一筆全平台事件（次數 7 與各原因），回報後列刪除、不再重複；沒登錄的硬體 ID 沒有列也沒有事件。取挑戰時刪掉過期的挑戰。平台角色登不進去（兩個消費端的 lag 都不明）的副本兩步都回 503 `bindings_stale`、`Retry-After: 30`，挑戰沒有作廢，正常的副本之後兌換成功。限流（Laysi/trustiot-v2#801）：以 `rl-ok` 為名、沒有金鑰的 8 個請求（4 個取挑戰、2 個別的金鑰簽的兌換 401、2 個捏造的 nonce 409）之後，`rl-ok` 仍換發成功 2 次；第 3 次簽章驗過但超過它自己的限流，429、`Retry-After` 約 1 小時、原因 `gateway_rate_limited`（INFO、計入時間窗），挑戰已作廢；取挑戰的全域限流超過後新硬體 ID 都 429、`Retry-After` 1–60 秒，只存下限流放行的挑戰，之後已經拿到挑戰的閘道器仍然兌換成功（兌換有自己的全域限流），捏造的兌換用完兌換那一道後 429；lag 不明的副本收到 30 個新硬體 ID 的 60 個請求時，只有限流放行的（每一步 12 個）回 503、各記一行 INFO，其餘 429、不記 INFO（容器；`-- --nocapture` 印出測試報告引用的證據行） |
| `tests/renewal_lag.rs` | Laysi/trustiot-v2#81：資產服務寫一個這個版本不認得的綁定事件，消費端停在它上面；lag 超過 `renewal.max_consumer_lag`（1 秒）後兩步都回 503 `bindings_stale`、`Retry-After: 30`，兌換沒有作廢挑戰。消費端從此停住，所以是自己的測試執行檔、自己的資料庫（容器） |
| `tests/tenant_states.rs` | Laysi/trustiot-v2#778 驗收條件 1 到 3：以身分服務自己的 `TenantStore::create` 建立租戶，再照身分服務的寫法（鎖住租戶列、版本號加一、同一個交易寫事件）變更狀態，之後才啟動真的服務，所以消費端從頭重播：正常、停權、恢復、刪除、8 個並行變更的租戶，加上只有事件的不認得狀態（`unrecognized`）。另外直接寫入亂序的變更（例如版本 5、2、9、3、7、4、8、6），最後的狀態是版本最大的那一次，較舊的每一筆 `stored=false`。等兩邊一致後，`certs.tenant_statuses` 與 `identity.tenants` 的每個租戶相同；`tenant_suspended` 只有正常的租戶是 `false`，刪除、沒有紀錄的都是 `true`；晚到的舊快照與整個訂閱重播（`Replay::Reprocess`）都不改任何一列（含 `updated_at`）；沒有 dead letter（容器；`-- --nocapture` 印出證據行） |
| `tests/tenant_state_lag.rs` | Laysi/trustiot-v2#778 驗收條件 4：身分服務寫一個這個版本不認得的 `identity.tenant_state_changed` 版本，消費端重試、不進 dead letter，lag 超過 `renewal.max_consumer_lag`（1 秒）後換發回 503 `bindings_stale`、`Retry-After: 30`；那個租戶仍是建立時的正常、版本 1。消費端從此停住，所以是自己的測試執行檔、自己的資料庫（容器） |
| `tests/bindings.rs` | Laysi/trustiot-v2#632：真的服務建立訂閱 `certs.assets-events`（讀 `assets`）；以 `assets_app` 照綁定的寫法在租戶交易寫 `assets.gateway_bound` 後，表裡有這台閘道器、正確的租戶、綁定 ID、`occurred_at` 與事件位置，`gateway_tenant` 與 `bound_gateways` 讀得到；同一個事件再送一次、整個訂閱重播（`Replay::Reprocess`）後仍是同一列、內容不變。自己的訂閱的消費端：同一台閘道器較舊的綁定（log 位置較前、別的租戶）在較新的之後才到時存下但不是目前的綁定；不合規則的硬體 ID 與沒有租戶的綁定進 dead letter，之後的綁定照常處理。租戶狀態：沒有紀錄讀成停權、只接受較大的版本號、寫入停權後 `tenant_suspended` 為真；刪除之後任何版本都不覆蓋，`unrecognized` 只被同一個版本的已知狀態取代，表拒絕其他值（23514）；租戶交易寫不了別的租戶的狀態（42501）、只看得到自己的。平台角色讀得到兩個租戶的綁定、取 log 最後的綁定、略過已結束的，每次讀取都有一行 `platform database access`（actor `service`、`certs`、呼叫端的 purpose）（容器） |
| `tests/dynsec.rs` | Laysi/trustiot-v2#633：Mosquitto 以 chart 的設定（`mosquitto.conf` 與閘道器 listener）與 init container 寫的 dynsec 檔（`migrate::mqtt_accounts::reconcile`）啟動，真的服務以 `service:certs` 在服務埠同步，閘道器以出廠登錄簽出的憑證在 8883 以 MQTT 5 連線：已登錄未綁定的閘道器連得上、發布得到 PUBACK 0x87，綁定後同一條連線就能發布（驗收條件 2）；兩台都綁定後，閘道器能發布自己的 `up/`、訂閱自己的 `down/`，發布或訂閱另一台的得到 0x87（驗收條件 1）；沒有變更時多次完整比對不送修改，dynsec 檔逐位元組不變（驗收條件 4）；broker 以只有 chart 項目加上一個不該存在的閘道器 client 的檔案重啟後，一輪比對補回兩台、刪掉多的，服務帳號、角色與群組不變（驗收條件 3）；`trace` 日誌不含 broker 密碼的任何 8 字元片段（驗收條件 5）。Laysi/trustiot-v2#81：閘道器唯一的憑證被撤銷、服務以 `dynsec.interval` 1 小時重啟後，連線時的完整比對刪掉它的 client（舊憑證連得上、發布得到 0x87）；換發 201 之後，單台比對把 client 加回來，日誌裡 `renewed a gateway certificate` 在 `changed a gateway's broker permissions`（`added`）之前，期間沒有完整比對的變更，新憑證的連線能發布。Laysi/trustiot-v2#636：先以 `mosquitto_ctrl dynsec` 對已連線的閘道器逐一下 `removeClientRole`、`addClientRole`、`disableClient`、`enableClient`、`deleteClient`，記下哪些斷線（見[閘道器憑證的撤銷](#閘道器憑證的撤銷)）；兩個副本、完整比對 1 小時一次，平台管理員的撤銷送到第二個：已連線的閘道器在 30 秒（N）內被斷開（DISCONNECT 0x98），第二個副本的日誌依序是撤銷、`issued a new CRL`（`revocation`）、`removed`，第一個副本什麼都沒改，回應時 `crl.pem` 已列出它；broker 載入新 CRL 之前重連得到 CONNACK 0x00、PUBACK 0x87，sidecar 的檢查（`migrate::mqtt_trust::validate`）與 SIGHUP 之後 TLS 交握以 `CertificateRevoked` 失敗，另一台照常發布；換發之後 client 回來、新憑證能發布；再以 `key_compromise` 撤銷，連線被斷開，換發 403 `key_compromised`、不簽新憑證（容器；`-- --nocapture` 印出測試報告引用的證據行）。同一個檔案的兩個測試共用 Postgres，一次一個 |
| `tests/gateway_revocation.rs` | Laysi/trustiot-v2#636 的 API：沒有 token、別的金鑰簽的 token 401，租戶管理員、沒有租戶的使用者、換發的平台管理員 token 403，錯的或沒有 `Content-Type`、過大或不是物件的本文 400，不認得的欄位與 `certificate_hold` 422，沒登錄的硬體 ID 與不是硬體 ID 的路徑 404，只剩過期憑證的閘道器 409；這 14 個請求前後簽發紀錄與兩個 CA 的 CRL 編號都不變。成功時兩個 CA 各一張還有效的憑證都以 `key_compromise` 撤銷、依序號回傳，先前的撤銷與過期的憑證不變，每個 CA 剛好多一張 CRL（`revocation`）並列出它們，INFO 一行帶平台管理員與序號、不含憑證；再撤銷一次 409、什麼都不變（容器） |
| `tests/registration.rs` | Laysi/trustiot-v2#79：工作站簽章只接受工作站憑證，閘道器憑證、別的 CA、別的金鑰、別的本文或路徑、超過 300 秒的時間、過期的憑證、缺少、重複或格式錯的標頭都拒絕並說明原因；簽發 CA 簽的、其餘都符合工作站 profile 的憑證，只要 keyUsage 沒有 digitalSignature（或沒有 keyUsage）或是 CA:TRUE，也以 `not_workstation` 拒絕；EK 只接受 RSA 2048/3072/4096 與 P-256、P-384。真的服務：新的硬體 ID 201、憑證 CN 是硬體 ID 且在根之下驗證為用戶端憑證、寫一筆全平台事件；重送 200 同一張憑證、什麼都不寫；完全相同的請求重送回 `replayed`；換公鑰或 EK 409，別台的公鑰或 EK 409，不支援的 EK 422；少了工作站簽章、沒有 token、別的簽發者、租戶管理員、交換來的 token、別的金鑰或本文、以閘道器憑證冒充工作站，都不登錄；兩個相同的登錄同時到達（以 superuser 拿住那個硬體 ID 的登錄鎖，等兩個請求都在查硬體 ID 之前等它才放開）一個 201、一個 200、同一張憑證，只簽一張；兩個新的硬體 ID 同時以同一把公鑰、或同一個 EK 登錄（以 superuser 拿住登錄表的 SHARE 鎖，等兩個請求都停在寫入之前才放開）一個 201、一個 409 `public_key_in_use` 或 `endorsement_key_in_use`，後者的憑證與事件都沒有留下（Laysi/trustiot-v2#782）；除役的硬體 ID 409，superuser 也刪不掉、清不掉、改不了；`certs_app` 讀不到登錄；撤銷的工作站憑證 403 `revoked`。Laysi/trustiot-v2#610：201 與 200 的 `certificate_chain` 是 `certificate` 再接簽發 CA，簽發 CA 的公鑰驗得過 leaf 的簽章、金鑰 id 等於 `ca_key_id`、根驗得過它；載入兩把簽發 CA 時重送放的是簽出它的第 1 代，不是現在簽章的第 2 代；第 1 代的金鑰與憑證移除後重送仍 200、同一張憑證，鏈只有 leaf，WARN 只帶 `ca_key_id`（容器） |
| `tests/workstation_tool.rs` | Laysi/trustiot-v2#615：產線工作站工具（`factory/workstation`）對真的服務：前面放一個檢查 session cookie、換成平台管理員內部 token 的 ingress 替身，工具 `enroll` 取得工作站憑證後登錄一台閘道器、重跑、再讓 ingress 在服務提交後丟掉回應（工具重送得到 200）；另一個新的硬體 ID 第一個 201 就被丟掉、重送得到 200。每個硬體 ID 都剛好一筆登錄、一張閘道器憑證、一筆 `certs.gateway_registered` 事件，工具的紀錄是 `registered`、序號等於資料庫的（容器） |
| `tests/broker.rs` | Laysi/trustiot-v2#557：CSR 要求別的 CN、O、DNS 與 IP 名稱、CA、keyCertSign 與 clientAuth、anyExtendedKeyUsage 時，簽出的仍是 CA 決定的 broker profile（主體、DNS 名稱依設定、EKU 只有 serverAuth）；rustls 以根為信任錨時，憑證對兩個名稱都驗得過，對別的名稱、IP 拒絕，信任別的根時拒絕，同名但別的 CA 簽的憑證也拒絕，當用戶端憑證時以用途拒絕；閘道器憑證不能當伺服器憑證，閘道器 id 不能是 broker 的名稱；簽章驗不過、P-384 或不是 CSR 時拒絕。真的服務在 broker 埠簽出、記為 `broker`、回整條鏈並記 INFO，日誌沒有憑證；錯的 `Content-Type`、PEM、壞的或過大的 CSR、別的曲線都不簽也不記；API 埠沒有這個端點，broker 埠沒有其他路徑。簽發 CA 還沒有第一張 CRL 時回 503、不記錄，有了才簽。Mosquitto（OpenSSL）以簽出的憑證與測試產生的私鑰當伺服器：信任根、連 `mqtt-broker.trustiot.internal` 的閘道器憑證連得上，別的名稱或信任別的 CA 時用戶端拒絕，broker 的憑證當用戶端憑證時 broker 以用途拒絕（容器） |
| `tests/broker_certificate.rs` | Laysi/trustiot-v2#535 驗收條件 1 與 3：chart 的 CronJob 執行的 `migrate mqtt-broker-certificate`（`converge_at`）對真的服務的 broker 埠與一個只有這個 Secret 的 Kubernetes API 替身：第一次執行把宣告的空 Secret 填入 `tls.crt`（憑證再接簽發 CA）與 `tls.key`、綁定 uid、記下代；Mosquitto 以 chart 的閘道器 listener（`deploy/chart/files/mosquitto/gateway-listener.conf`，MQTT 5、TLS 1.3）提供它，只信任根的 rustls 用戶端對 `mqtt-broker.trustiot.internal` 驗得過並拿到成功的 CONNACK，別的名稱、信任別的 CA、同名但別的 CA 簽的憑證都被用戶端拒絕。輪替一次：效期設成最短的 1 小時，效期過半前的執行不寫入，過半時（換發程式的時鐘；服務以實際時間簽）寫入新的憑證與新的金鑰，broker 收到 SIGHUP 後新連線驗到新的序號，重載前的連線仍回 PINGRESP；之後的執行保留新憑證。服務與 broker 的日誌都沒有私鑰（容器；`-- --nocapture` 印出測試報告引用的證據行） |
| `tests/workstation.rs` | Laysi/trustiot-v2#534：CSR 要求別的 CN、O、SAN、CA、keyUsage 與 clientAuth、serverAuth、anyExtendedKeyUsage 時，簽出的仍是 CA 決定的工作站 profile（EKU 只有工作站用途），rustls 驗用戶端憑證時接受閘道器憑證、拒絕工作站憑證；簽章驗不過、主體改過或 P-384 的 CSR 拒絕；沒有 token、不是 token、別的金鑰簽的、別的 audience、過期的 token 回 401，租戶管理員、沒有租戶的使用者與代使用者交換來的平台管理員 token 回 403，簽發與撤銷都一樣，而且簽發紀錄不變；壞的 CSR、不認得的欄位與錯的 Content-Type 不簽發；閘道器憑證、不存在或格式錯的序號撤銷回 404，撤銷寫入原因並記 INFO，重複撤銷回 409，撤銷回應時 `crl.pem` 已列出它與原因。Laysi/trustiot-v2#635（服務的 CRL 工作每小時一輪，啟動後不再插手）：撤銷的回應之後 `crl.pem` 立刻列出它、CRL 編號加一、log 的 `cause` 是 `revocation`、撤銷的 INFO 有平台管理員的使用者 id；另一個 session 鎖住這個 CA 的 CRL 列（立即重發等鎖逾時而失敗）時，撤銷照樣回 200 並寫入簽發紀錄、記 ERROR、CRL 不變；服務重啟、改成每秒一輪之後，下一輪在 CRL 離 `renew_before` 還有好幾天時列出它（`unlisted_revocation`）；Mosquitto（OpenSSL）以 `ca.pem` 與 `crl.pem` 驗用戶端憑證時接受閘道器憑證，拒絕撤銷的閘道器憑證與 API 簽出的工作站憑證（容器） |

chart 的部分（Secret、Role、掛載、values 的檢查）在 `deploy/chart/tests/chart.test.ts` 的 “gateway certificate authority”，broker 埠只有 broker 憑證的 CronJob 連得到在 “MQTT broker” 的 “the broker's certificate CronJob alone reaches the certs service's broker-certs port”，init container 對假的 Kubernetes API 的行為在 `tools/migrate/tests/ca_keys.rs`，金鑰檔的格式與規則在 `crates/ca-keys` 的單元測試。
