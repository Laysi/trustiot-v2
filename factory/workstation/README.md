# factory/workstation

產線工作站工具 `trustiot-workstation`（決策 12，Laysi/trustiot-v2#80）。產線工作站以兩個身分呼叫出廠登錄 API：工作站自己的工作站憑證（私鑰只在工作站上），加上平台管理員的登入。這個工具管理這兩個身分：

- 工作站身分：`init` 產生私鑰與 CSR，`enroll` 請 certs 服務簽發工作站憑證並存放，`install-certificate` 檢查並存放別處拿到的憑證。取代 [services/certs/README.md](../../services/certs/README.md)「簽發、撤銷與換發」裡的 openssl 與 curl 步驟。
- 平台管理員登入：`login` 以 email 驗證碼登入，session 存在使用者的 runtime 目錄，直到 `logout` 或 session 結束。

簽章的登錄用戶端（`register`）是 Laysi/trustiot-v2#615，經 ssh 登錄閘道器是 Laysi/trustiot-v2#616，都還沒有。

## 建置

在可信的建置機（與部署的版本同一個 commit）：

```sh
cargo build --release --locked -p workstation
```

執行檔是 `target/release/trustiot-workstation`，記下它的 SHA-256 再帶到工作站上核對。工作站要有系統的 CA bundle（一般的 Linux 發行版都有），或以 `--ca-file` 指定平台的 CA。

## 檔案與權限

| 檔案 | 位置 | 權限 | 內容 |
| --- | --- | --- | --- |
| `workstation-key.pem` | 工作站目錄 | 0600，已存在就不覆寫 | 工作站的 ECDSA P-256 私鑰（PKCS #8 PEM），不離開這台工作站 |
| `workstation.csr` | 工作站目錄 | 0644 | CSR（公開資料） |
| `workstation.pem` | 工作站目錄 | 0644 | 工作站憑證（公開資料），通過檢查才寫入；寫入的是由檢查過的 DER 重新編碼的那一張，不是輸入的原文 |
| `session` | `$XDG_RUNTIME_DIR/trustiot-workstation/` | 0600 | 平台管理員的 session cookie 與它所屬的平台網址 |
| `--headers-file` 指定的檔案 | 操作員自己放 | 0600 | Cloudflare Access 的 service token（見下方） |

- 工作站目錄是 `--dir`；沒有給時是 `$XDG_DATA_HOME/trustiot-workstation`，再沒有就是 `~/.local/share/trustiot-workstation`。工具建立的目錄是 0700；群組或其他人可寫入的目錄、別的使用者的目錄，以及指向目錄的符號連結一律拒絕。
- 存放密鑰的檔案（私鑰、session、`--headers-file`）建立時是 0600。權限比 0600 寬（群組或其他人有任何權限，或有執行權限）時拒絕使用，錯誤訊息寫出是哪個檔案與它的權限，不含內容；以 `chmod 600 <檔案>` 修正。符號連結、不是一般檔案的東西（例如 FIFO，不會讓工具卡住）與別的使用者擁有的檔案也拒絕。
- session 只放在 `XDG_RUNTIME_DIR`（使用者登入時由系統建立、登出或重開機時清空的目錄）。沒有設定時 `login` 直接拒絕，不改放到持久的目錄：以一般的使用者登入工作站（桌面或 ssh 登入都會設定它）。
- 寫入都是先寫暫存檔再 rename，中斷時不會留下半個檔案。
- **每位操作員用自己的 Linux 帳號**，交班時執行 `logout`：以同一個使用者身分執行的任何程式都讀得到 session 檔與私鑰，檔案權限只擋其他帳號。

## 指令

共用的網路選項（`login`、`enroll`、`logout`）：

| 選項 | 說明 |
| --- | --- |
| `--ca-file <檔案>` | 只信任這個檔案裡的 CA 憑證（PEM），不用系統的 CA；只能用在 `https://` |
| `--headers-file <檔案>` | 每個請求都帶上的 Cloudflare Access 標頭（見下方） |
| `--timeout <秒>` | 每個請求從連線到收完回應的上限，1 到 600，預設 30。主機名稱的解析（DNS）不在這個上限內，工具結束的時間可能因此晚於 `--timeout`，上限由系統的解析器設定決定 |

`--url` 是平台的網址，只有 scheme、主機與埠號，例如 `https://<app 主機>`。只接受 `https://`；`http://` 只限本機的 IP 位址（127.0.0.1、`[::1]`，測試用；不接受 `localhost` 這個名稱，因為它要經過名稱解析）。網址不可帶帳密、路徑、query 或 fragment。

### 1. `init`：產生工作站的私鑰與 CSR

```sh
trustiot-workstation init
```

產生 ECDSA P-256 私鑰（`workstation-key.pem`，0600）與 CSR（`workstation.csr`）。私鑰已經存在時不覆寫：讀出它（權限要是 0600），為它重寫 CSR。CSR 的主體不影響結果，CN、EKU 與效期都由 certs 服務決定。

### 2. `login`：平台管理員登入

```sh
trustiot-workstation login --url https://<app 主機> --email <平台管理員的 email>
```

1. 工具送出 `send_code`：不論地址有沒有帳號，平台都回「已受理」。「已受理」的回應內容讀不懂時（例如中間的代理換了內容），工具照樣請你輸入驗證碼，因為平台已經受理、信可能已經寄出。
2. 打開信箱，把 8 位數的驗證碼輸入到工具的提示（從 stdin 讀；不接受命令列參數或環境變數）。驗證碼 10 分鐘內有效，同一個驗證碼最多試 5 次。輸入錯了可以在同一次執行裡再輸入（最多 3 次），不會再寄信。
3. 成功時 session 存進 `$XDG_RUNTIME_DIR/trustiot-workstation/session`。session 閒置 12 小時結束、最長 7 天；之後要再登入一次。同一台工作站已有同一個平台的 session 時，新的登入會讓平台撤銷舊的。工作站只保存一個 session：已有別的平台（另一個 `--url`）的 session 時，`login` 在寄信之前就拒絕，先 `logout` 撤銷它再登入；直接覆蓋會讓舊的 session 在平台上一直有效到過期、卻再也拿不到。

**驗證碼沒收到時**：工具不會自動重送 `send_code`。每送一次都可能寄出一封信並用掉這個地址的額度（同一個地址 60 秒內一封、每小時 5 封、每天 20 封，超過時平台照樣回「已受理」但不寄），而且平台的回應不會說有沒有寄出。所以：

- 先看信箱（包括垃圾郵件）。
- 沒有信就**至少等 60 秒**，再手動重跑 `login`。連線中斷或逾時時，工具會說明「可能寄出也可能沒有」，同樣先查信箱再決定。
- 連續重跑很快就會用完每小時 5 封的額度，那時要等到下一個小時。

### 3. `enroll`：請 certs 服務簽發工作站憑證

需要這台工作站上有平台管理員的 session（`login`）。

```sh
trustiot-workstation enroll --name line-a-ws-01
```

- `--name` 是工作站名稱：1 到 48 個 ASCII 英數字、`.`、`_`、`-`，以英數字開頭；憑證的 CN 是 `workstation:<名稱>`。
- 送出前先讀私鑰（權限要是 0600）並確認 CSR 是這把私鑰的；送出 `POST /api/certs/v1/workstation-certificates`，再對回來的憑證做 `install-certificate` 的檢查，通過才存成 `workstation.pem`。
- 成功時印出憑證的序號與到期時間。**序號要記下來**：撤銷工作站憑證時要用它（目前還沒有列出工作站憑證的 API）。
- 回來的憑證沒通過檢查時不存放，訊息寫出它的序號，請平台管理員撤銷那張。回應裡的序號必須是 2 到 40 個小寫十六進位數字，而且等於憑證本身的序號，否則也不存放；印出的序號一律取自憑證本身。
- 平台以它自己的 401（M1 錯誤的 `code` 是 `unauthenticated`）拒絕 session 時，工具先送 `DELETE /api/identity/v1/sessions/current` 撤銷它（certs 服務在無法檢查內部 token 時也會這樣回答，session 可能仍有效），平台確認撤銷或已結束才刪除 session 檔，請重新 `login`；撤銷沒有確認時保留檔案，請執行 `logout`。沒有 M1 錯誤本文的 401（例如前面的代理回的）不會刪除 session 檔。
- `--url` 可省略，預設是 session 所屬的平台；給了就要和 session 的相同，session 不會送到別的網址。

### 4. `install-certificate`：檢查並存放憑證

平台管理員在別的機器上簽發（例如管理介面或 API）、把憑證帶過來時：

```sh
trustiot-workstation install-certificate --certificate issued.pem
```

依序檢查，任何一項不過就拒絕、不存放：

1. 檔案裡剛好一張 PEM 憑證，而且沒有其他 PEM 區塊（私鑰、第二張憑證或憑證鏈都讓整個檔案被拒絕）。區塊外的文字略過；存放的是由憑證的 DER 重新編碼的 PEM，不是輸入的原文。
2. 不是 CA 憑證；keyUsage 有 digitalSignature。
3. extendedKeyUsage 含 TrustIoT 工作站用途（`2.25.316065004478888347166512336770143873285`，以 DER 比對，見 `contract::certs::WORKSTATION_PURPOSE_DER`），而且沒有 anyExtendedKeyUsage、clientAuth 或 serverAuth（閘道器憑證因此不會被當成工作站憑證）。
4. 主體只有一個 CN，是 `workstation:<名稱>`，名稱合乎上面的規則。
5. 現在在效期內（還沒生效或已過期都拒絕）。
6. 憑證的公鑰等於工作站私鑰的公鑰。

工具沒有簽發 CA 的憑證，所以不檢查憑證鏈；certs 服務在每個登錄請求都檢查。

### 5. `logout`：登出

```sh
trustiot-workstation logout
```

以 session 送出 `DELETE /api/identity/v1/sessions/current`：平台撤銷 session（204），或 session 早已結束（平台自己的 401，`code` 是 `unauthenticated`），兩種情況都刪除 session 檔。沒有回應或其他錯誤時保留檔案（session 可能仍有效），之後再試。工作站交班或不再使用時都要登出。

## 經 Cloudflare Access（`--headers-file`）

展示環境前面有 Cloudflare Access 時（Laysi/trustiot-v2#619），以 Access 的 service token 通過：

```text
# Cloudflare Access service token
cf-access-client-id: <client id>
cf-access-client-secret: <client secret>
```

- 只接受這兩個標頭，兩個都要有、各一次；空行與 `#` 開頭的行略過。其他標頭名稱一律拒絕。格式不對的行只以行號回報，不顯示那一行的任何內容。
- 檔案權限要是 0600。client secret 只交給持有工作站的操作員，不進 repo、issue、PR 或日誌；工具不把標頭值寫進日誌或錯誤訊息。
- 每個請求（`login` 的兩步、`enroll`、`logout`）都帶上。

## 輸出與日誌

- 結果印在 stdout，提示與日誌印在 stderr。結束碼：0 完成、1 失敗、2 命令列用法錯誤。
- `TRUSTIOT_WORKSTATION_LOG` 設定日誌等級：`off`、`error`、`warn`、`info`（預設）、`debug`、`trace`。只記這個工具自己的事件（方法、路徑、狀態碼、檔案路徑），連 `trace` 也不記驗證碼、cookie、`Set-Cookie`、私鑰或 `--headers-file` 的值；email 地址也不記。
- 平台回錯誤時只顯示狀態碼與 M1 錯誤的 `code`、訊息 key 與欄位，不顯示回應本文的其他內容。這些來自伺服器的文字與序號在印出前，控制字元（終端機的跳脫序列、換行）都改寫成 `\u{..}`，過長的截斷。

## 安全上的做法與限制

- **`http://` 只限 loopback 的 IP 位址**：不接受 `localhost` 這個名稱，它經過名稱解析，可能被導向別處。
- **不自動重試**：每個請求用一條新的連線、只送一次；沒有連線池（連線池可能在連線被關掉時重送請求）。
- **TLS**：rustls 與 ring，TLS 1.2 與 1.3；信任系統的 CA（`SSL_CERT_FILE`、`SSL_CERT_DIR` 有設時用它們）或只信任 `--ca-file`。憑證與主機名稱的檢查不能關閉；不走 proxy、不跟隨重新導向。
- **跨站檢查**：請求不帶 `Origin` 與 `Sec-Fetch-Site`，身分服務的跨站檢查因此放行（[services/identity/README.md](../../services/identity/README.md)「跨站請求」）。
- **session 綁定平台**：session 檔記下設定它的平台網址，cookie 只送回那個網址。
- **記憶體**：驗證碼、cookie、標頭值與私鑰的包裝型別在 drop 時清零（`zeroize`），`Debug` 與 `Display` 一律顯示 `<redacted>`。送出時另外產生的副本（HTTP 標頭值、交給 hyper 的請求本文）、ring 內部的金鑰物件，以及讀驗證碼時標準函式庫 stdin 的緩衝區不會被清零；輸入的一行很長時，存驗證碼的 `String` 重新配置留下的舊緩衝區也不會。
- **私鑰在一般的檔案裡**：工作站私鑰的保護靠檔案權限與工作站本身；能用 TPM 時再改放 TPM。憑證效期預設 180 天，外洩時以撤銷讓它立即失效（services/certs/README.md「簽發、撤銷與換發」）。

## 換發與撤銷

- 換發（同一把私鑰）：到期前再 `enroll` 一次，新的憑證取代 `workstation.pem`；再請平台管理員以 `superseded` 撤銷舊的序號。
- 換一把新的私鑰：把整個工作站目錄移到別處保存（不要只刪私鑰），以 `init` 建立新的，再 `enroll`；舊的憑證請平台管理員撤銷。
- 撤銷工作站憑證的 API 與原因見 services/certs/README.md「簽發、撤銷與換發」。

## 測試

```sh
cargo test -p workstation
```

測試只對 loopback 上的替身（axum）與測試用 CA，不連任何真實環境或郵件伺服器：

- `tests/identity.rs`：`init` 的權限與不覆寫、權限太寬的私鑰被拒絕、`install-certificate` 拒絕每一種不合格的憑證（含夾帶私鑰的檔案）並存放合格的，存放的檔案是重新編碼的憑證。
- `tests/login.rs`：以 TRACE 等級跑 `init`、`login`、`enroll`、`logout` 一輪，檢查替身收到的請求（每個請求都帶 `--headers-file` 的標頭、cookie 只在登入之後）、session 檔 0600 與登出後刪除，並在所有輸出中搜尋驗證碼、cookie、私鑰與標頭值；`send_code` 在連線中斷、逾時與 503 時都只送一次；沒有 `XDG_RUNTIME_DIR`、session 檔或標頭檔權限太寬、標頭名稱不對時，在送出任何請求前就拒絕；平台自己的 401 先撤銷再刪除 session 檔，代理的 401 與撤銷失敗時保留檔案；回應的序號不合格式或與憑證不符時不存放。
- 各模組的單元測試：包裝型別的遮蔽、檔案權限、FIFO 與符號連結目錄被拒絕、擁有者檢查、網址規則（含 `localhost` 被拒絕）、標頭檔格式與錯誤訊息不含內容、伺服器文字的控制字元改寫、HTTPS 只信任 `--ca-file`。
