# factory/workstation

產線工作站工具 `trustiot-workstation`（決策 12，Laysi/trustiot-v2#80）。產線工作站以兩個身分呼叫出廠登錄 API：工作站自己的工作站憑證（私鑰只在工作站上），加上平台管理員的登入。這個工具管理這兩個身分：

- 工作站身分：`init` 產生私鑰與 CSR，`enroll` 請 certs 服務簽發工作站憑證並存放，`install-certificate` 檢查並存放別處拿到的憑證。取代 [services/certs/README.md](../../services/certs/README.md)「簽發、撤銷與換發」裡的 openssl 與 curl 步驟。
- 平台管理員登入：`login` 以 email 驗證碼登入，session 存在使用者的 runtime 目錄，直到 `logout` 或 session 結束。
- 出廠登錄：`register --gateway` 經 ssh 在閘道器上執行 `trustiot-agent provision`，取得 TPM 金鑰的 CSR 與 endorsement key，檢查後以兩個身分登錄（Laysi/trustiot-v2#616）；`register --csr --ek` 從檔案讀同樣的東西（Laysi/trustiot-v2#615）。兩者都是每次嘗試重新簽章、只重試安全的失敗、檢查回應是這台閘道器的登錄，結果記在工作站上。取代 services/certs/README.md「出廠登錄」原本的 openssl 與 curl 步驟。
- 查證：`verify` 在平台的事件紀錄裡找這台閘道器的 `certs.gateway_registered` 事件。

產線的一輪是：`init` → `login` → `enroll`（一台工作站做一次），每台閘道器 `register --gateway` → `verify`，交班時 `logout`。

## 建置

在可信的建置機（與部署的版本同一個 commit）：

```sh
cargo build --release --locked -p workstation
```

執行檔是 `target/release/trustiot-workstation`，記下它的 SHA-256 再帶到工作站上核對。工作站要有系統的 CA bundle（一般的 Linux 發行版都有），或以 `--ca-file` 指定平台的 CA。`register --gateway` 另外要系統的 OpenSSH 用戶端（`ssh`）。

閘道器的出廠映像還沒有 `trustiot-agent` 時（Laysi/trustiot-v2#95 之前），`register --gateway --upload-agent` 每次把 agent 上傳到閘道器執行。agent 是 aarch64 的執行檔，在同一台建置機以 [edge/agent/README.md](../../edge/agent/README.md) 的步驟建置（`cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent`），一樣記下 SHA-256 帶到工作站。

## 檔案與權限

| 檔案 | 位置 | 權限 | 內容 |
| --- | --- | --- | --- |
| `workstation-key.pem` | 工作站目錄 | 0600，已存在就不覆寫 | 工作站的 ECDSA P-256 私鑰（PKCS #8 PEM），不離開這台工作站 |
| `workstation.csr` | 工作站目錄 | 0644 | CSR（公開資料） |
| `workstation.pem` | 工作站目錄 | 0644 | 工作站憑證（公開資料），通過檢查才寫入；寫入的是由檢查過的 DER 重新編碼的那一張，不是輸入的原文 |
| `registrations/<硬體 ID>.json` | 工作站目錄 | 0600 | 一台閘道器的登錄紀錄：`pending`（送出前）或 `registered`（回應通過檢查後，含平台回的整筆登錄與憑證鏈），以及操作員釘住的根憑證 SHA-256（`root_sha256`，Laysi/trustiot-v2#617 安裝根憑證前再比對一次）；整檔原子替換 |
| `registrations.jsonl` | 工作站目錄 | 0600 | 登錄的日誌，只附加：每次執行的開始、每次失敗的嘗試、成功、停止或放棄，各一行 JSON |
| `known_hosts` | 工作站目錄 | ssh 建立 | `register --gateway` 連過的閘道器的主機金鑰，以 `trustiot-<硬體 ID>` 為名（見 `register` 的「經 ssh 取得」）；由 ssh 寫入，工具不讀也不改 |
| `session` | `$XDG_RUNTIME_DIR/trustiot-workstation/` | 0600 | 平台管理員的 session cookie 與它所屬的平台網址 |
| `--headers-file` 指定的檔案 | 操作員自己放 | 0600 | Cloudflare Access 的 service token（見下方） |

- 工作站目錄是 `--dir`；沒有給時是 `$XDG_DATA_HOME/trustiot-workstation`，再沒有就是 `~/.local/share/trustiot-workstation`。工具建立的目錄是 0700；群組或其他人可寫入的目錄、別的使用者的目錄，以及指向目錄的符號連結一律拒絕。
- 存放密鑰的檔案（私鑰、session、`--headers-file`）建立時是 0600。權限比 0600 寬（群組或其他人有任何權限，或有執行權限）時拒絕使用，錯誤訊息寫出是哪個檔案與它的權限，不含內容；以 `chmod 600 <檔案>` 修正。符號連結、不是一般檔案的東西（例如 FIFO，不會讓工具卡住）與別的使用者擁有的檔案也拒絕。
- session 只放在 `XDG_RUNTIME_DIR`（使用者登入時由系統建立、登出或重開機時清空的目錄）。沒有設定時 `login` 直接拒絕，不改放到持久的目錄：以一般的使用者登入工作站（桌面或 ssh 登入都會設定它）。
- 寫入都是先寫暫存檔再 rename，中斷時不會留下半個檔案。
- **每位操作員用自己的 Linux 帳號**，交班時執行 `logout`：以同一個使用者身分執行的任何程式都讀得到 session 檔與私鑰，檔案權限只擋其他帳號。

## 指令

共用的網路選項（`login`、`enroll`、`register`、`verify`、`logout`）：

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

### 5. `register`：登錄閘道器

需要工作站憑證（`enroll` 或 `install-certificate`）與平台管理員的 session（`login`）。CSR 與 endorsement key 有兩種來源：

```sh
# 經 ssh 從閘道器的 TPM 取得（產線的做法）
trustiot-workstation register --hardware-id <硬體 ID> --gateway <ssh 目的地> \
  --root-sha256 <根憑證的 SHA-256> [--upload-agent trustiot-agent]

# 從檔案讀（閘道器的報告由別的方式取得時）
trustiot-workstation register --hardware-id <硬體 ID> --csr gateway.csr --ek ek.pem \
  --root-sha256 <根憑證的 SHA-256>
```

| 選項 | 說明 |
| --- | --- |
| `--hardware-id` | 閘道器的硬體 ID：1 到 64 個 ASCII 英數字、`.`、`_`、`-`，以英數字開頭。**取自閘道器標籤**（見下方），不取自閘道器的回答 |
| `--gateway` | ssh 目的地：`[使用者@]主機`，主機是名稱、位址或 `~/.ssh/config` 裡的 Host。只接受 ASCII 英數字、`.`、`_`、`-`、`:`、`%`、`[`、`]` 與至多一個 `@`，不以 `-` 開頭；埠號、金鑰與其他設定寫在 ssh 設定裡 |
| `--handle` | 閘道器金鑰的 TPM persistent handle，`0x` 加 1 到 8 個十六進位數字，預設 `0x81000100`。只接受 owner 範圍（`0x81000000` 到 `0x817fffff`），不接受 SRK 的 `0x81000001`。與 agent 的預設相同，平常不必給 |
| `--upload-agent` | 每次執行前把這個 agent 執行檔（aarch64，最大 64 MiB）上傳到閘道器的暫存目錄執行，結束後刪除；不給時執行閘道器上已安裝的 `trustiot-agent`。檔案必須屬於執行工具的使用者，群組與其他人不能寫入（例如 0755），否則拒絕：它在閘道器上以 root 執行 |
| `--remote-sudo` | 在閘道器上以 `sudo -n` 執行 agent（ssh 的使用者讀不了 `/dev/tpmrm0` 時）。`-n` 讓 sudo 需要密碼時直接失敗，不會卡住；閘道器要為這個使用者設定免密碼執行 agent |
| `--allow-new-key` | 工作站已有這個硬體 ID 的紀錄、閘道器的 TPM 是紀錄的那顆（EK 相同）卻沒有金鑰或是另一把金鑰時，仍建立新金鑰（或沿用那把）並送出（見「TPM 被清除過」）。EK 不同（另一顆 TPM，結束碼 27）時不放行。只在確認過標籤、要讓平台回答時才給 |
| `--csr` | 從檔案時：閘道器 TPM 金鑰的 CSR，一個 PEM `CERTIFICATE REQUEST`，ECDSA P-256，以 ecdsa-with-SHA256 由它自己的金鑰簽章。不能和 `--gateway` 一起給 |
| `--ek` | 從檔案時：TPM 的 endorsement key，一個 PEM `PUBLIC KEY`。不能和 `--gateway` 一起給 |
| `--root-sha256` | **必填**（兩種來源都是），沒有環境變數、設定檔或略過檢查的替代方式。平台根憑證 DER 的 SHA-256，64 個十六進位數字，可用 `:` 分隔（`openssl x509 -noout -fingerprint -sha256` 的輸出可以直接貼上）。回應的 `root_certificate` 不符就不登錄（結束碼 18）。指紋向平台管理員取得：根 CA 在儀式建立後就固定（services/certs/README.md「CA 階層與私鑰保管」），平台管理員對儀式產出、放進 values 的根憑證執行 `openssl x509 -in root.pem -noout -fingerprint -sha256`，經和平台連線不同的管道（例如書面的產線設定）交給產線 |
| `--attempts` | 登錄 API 最多嘗試幾次，1 到 20，預設 8 |
| `--retry-for` | 從第一次嘗試起，超過這個秒數就不再開始新的嘗試，1 到 3600，預設 300 |
| `--url`、`--dir`、網路選項 | 同 `enroll` |

`--handle`、`--upload-agent`、`--remote-sudo`、`--allow-new-key` 只能和 `--gateway` 一起給。

#### 硬體 ID 從標籤來

硬體 ID 是閘道器標籤上印的那個，由操作員輸入（或掃描標籤）。工具不採用閘道器自己說的硬體 ID：閘道器回報的硬體 ID 要等於 `--hardware-id`，否則不登錄（結束碼 21）。但 agent 的報告只是照抄工具送過去的 `--hardware-id`，所以這項比對擋不住 ssh 接到別台閘道器。

**工作站認不出沒有紀錄的另一台閘道器**：ssh 目的地打錯、台面上放的不是標籤上那台時，若這個硬體 ID 在這台工作站沒有紀錄，工具會把那台的 TPM 登錄成這個硬體 ID。這只能靠操作員核對 ssh 目的地與標籤擋下：每台閘道器接上台面後，先確認目的地指向的就是貼著這張標籤的那台。工作站有這個硬體 ID 的紀錄時，工具以 TPM 的 endorsement key（EK）分辨：

- **EK 和紀錄不同**：是另一顆 TPM，停在結束碼 27，什麼都不建立、不送出，`--allow-new-key` 也不放行。清除 TPM（`tpm2_clear`、BIOS 或韌體清除）不會改變 EK，所以 EK 不同一定不是同一顆 TPM：核對 ssh 目的地與標籤。若紀錄還是 `pending`（從沒登錄成功），而且確定寫下它的那次 ssh 接錯了閘道器，把紀錄移出 `registrations/`（保留下來）再重跑；那次若其實已經在平台登錄，平台會以 409 擋下正確的那台，帶著 `registrations.jsonl` 回報平台管理員。
- **EK 相同，但 TPM 沒有金鑰或是另一把金鑰**：同一顆 TPM 被清除過，停在結束碼 20（見「TPM 被清除過」）。

#### 經 ssh 取得（`--gateway`）

工作站要先能以 ssh 金鑰登入閘道器，不需要輸入任何東西：

- 工具以 `BatchMode=yes` 執行 ssh，不會問密碼、passphrase 或主機金鑰。金鑰放在 ssh-agent（工具保留 `SSH_AUTH_SOCK`）或沒有 passphrase 的金鑰檔。
- **必須在 `~/.ssh/config` 為閘道器設定 `IdentityFile`**（金鑰在 ssh-agent 時指向它的公鑰檔）。工具固定加上 `IdentitiesOnly=yes`，ssh 只送出 `IdentityFile` 指定的金鑰，不會把 ssh-agent 裡的其他金鑰拿去給閘道器試；沒有設定時 ssh 只試預設的 `~/.ssh/id_*`。
- **主機金鑰綁硬體 ID，不綁台面的位址**。工具每次執行 ssh 都加上 `HostKeyAlias=trustiot-<硬體 ID>` 與 `UserKnownHostsFile=<工作站目錄>/known_hosts`：閘道器的主機金鑰記在工具自己的檔案裡，以硬體 ID 為名；另外加上 `GlobalKnownHostsFile=/dev/null` 與 `VerifyHostKeyDNS=no`，系統的 `/etc/ssh/ssh_known_hosts` 與 DNS 上的 SSHFP 紀錄都不採用。同一個台面位址換接下一台時，用的是那台的硬體 ID，不會沿用上一台的紀錄；已經記下的硬體 ID 回了別的主機金鑰時，ssh 拒絕連線。`~/.ssh/known_hosts` 裡的紀錄不會被用到。
- **執行前檢查 ssh 實際會用的設定**：工具先以 `ssh -G`（同樣的選項與目的地）讀出設定，`StrictHostKeyChecking` 是 `no`、`UserKnownHostsFile` 是 `/dev/null` 或不只工具的檔案、`GlobalKnownHostsFile` 不是 `/dev/null`、`VerifyHostKeyDNS` 開著、或設了 `KnownHostsCommand` 時拒絕執行（結束碼 1，閘道器上什麼都沒執行）。設定是預設的 `ask` 時（`BatchMode` 會讓它拒絕每一台新閘道器），工具加上 `StrictHostKeyChecking=accept-new`：一個硬體 ID 第一次連線時記下主機金鑰，之後每次都要相同。設成 `yes` 或 `accept-new` 時照設定；`yes` 要自己先把主機金鑰以 `trustiot-<硬體 ID>` 為名寫進工具的檔案。需要 OpenSSH 7.6 以上（`accept-new`）。工作站目錄的路徑只能有英數字與 `/`、`.`、`_`、`-`、`+`、`@`、`,`、`:`（ssh 會切開空白、展開 `%` 與 `~`），否則結束碼 2。
- **重刷後主機金鑰改變**：重刷 SD 卡後閘道器的主機金鑰會重新產生，ssh 以 `REMOTE HOST IDENTIFICATION HAS CHANGED` 拒絕連線（重試後結束碼 23）。先核對標籤、確認這台確實剛重刷，再只移除這個硬體 ID 的那一筆後重跑：`ssh-keygen -R trustiot-<硬體 ID> -f <工作站目錄>/known_hosts`。沒有重刷卻遇到這個錯誤時不要移除，停下來回報：可能有別的機器冒用這個位址。
- **第一次連線仍是先信任**：新閘道器的第一把主機金鑰沒有別的來源可以核對，產線網路要在實體控制下（決策 12）。主機金鑰只保證之後連到的是同一台；金鑰是否真的在那台的 TPM 裡，見「安全上的做法與限制」的殘餘風險。
- 工具固定加上 `-T -a -x` 與 `IdentitiesOnly=yes`、`ClearAllForwardings=yes`、`ForwardAgent=no`、`ForwardX11=no`、`PermitLocalCommand=no`、`ControlMaster=no`、`ControlPath=none`（不轉送 agent、不共用連線）、`ConnectTimeout=20`、`ServerAliveInterval=15`、`ServerAliveCountMax=4`、`LogLevel=ERROR`。

**閘道器端的 ssh 限制（正式產線的前提，Laysi/trustiot-v2#219）**：能在閘道器上執行 agent 的人，就能決定閘道器的金鑰與信任錨（Laysi/trustiot-v2#617 的安裝以 root 執行），所以正式產線的閘道器不讓工作站的 ssh 帳號執行任意指令。採用的做法：

- 工作站用**專用的 ssh 金鑰**（只給這個工具用），在 `~/.ssh/config` 為閘道器設 `IdentityFile`（工具已固定加上 `IdentitiesOnly=yes`）。
- 閘道器上的專用帳號在 `authorized_keys` 以 `restrict,command="sudo -n /usr/local/libexec/trustiot-provision-ssh"` 綁這把金鑰：不論工作站送什麼，sshd 只執行這個固定的轉接程式，不給 shell、轉送或 pty。
- sudoers 只允許這一個沒有參數的命令（`<帳號> ALL=(root) NOPASSWD: /usr/local/libexec/trustiot-provision-ssh ""`，空字串表示不接受任何參數），並以 `env_keep` 只保留 `SSH_ORIGINAL_COMMAND`；不用 `trustiot-agent provision *` 這類萬用字元。
- 轉接程式從 `SSH_ORIGINAL_COMMAND` 讀工作站的請求，只接受固定的幾種形式、每個字都要符合上面的規則（英數字、`.`、`_`、`-`），再以固定的參數執行 `trustiot-agent provision`（與 #617 的安裝）；其他一律拒絕。
- 工作站這一側已經有的：連線逾時（`ConnectTimeout=20`、`ServerAliveInterval=15` × 4）、每次嘗試 300 秒與一次執行 420 秒的上限、`BatchMode=yes` 與主機金鑰檢查。

**還沒實作**：轉接程式、sudoers，以及把工作站在「已安裝的 agent」模式送的請求（目前是 `sh -c '<固定腳本>' …`，轉接程式無法安全地解析）改成只有幾個字的短請求，都是出廠映像階段的前提條件，記在 Laysi/trustiot-v2#219。在那之前，`--upload-agent` 與 `--remote-sudo` 只用在還沒有出廠映像的 lab 與展示用閘道器（例如 Laysi/trustiot-v2#618），那裡的 ssh 帳號本來就能登入 shell；上傳並執行任意程式與這個限制不相容，正式產線不用它。

步驟：

1. **送出前先檢查所有輸入**：硬體 ID、handle、目的地（上面的規則）與 `--upload-agent` 的檔案；不合規則時結束碼 2（`--upload-agent` 的檔案屬於別人或群組、其他人可寫入時是 1），**ssh 沒有執行**。工作站私鑰、憑證與 session 也先載入，載入不了時同樣什麼都沒送到閘道器。
2. **檢查（`trustiot-agent provision --inspect`）**：只讀 TPM，不建立任何東西（報告說金鑰是這次建立的就不採用，結束碼 21）。工作站有這個硬體 ID 的紀錄時，報告的 EK 要和紀錄相同，否則停在結束碼 27（見上面「硬體 ID 從標籤來」）。
3. **TPM 沒有金鑰時**：工作站沒有這個硬體 ID 的紀錄（新閘道器）就執行 `trustiot-agent provision` 建立金鑰；有紀錄就停在結束碼 20（見「TPM 被清除過」），除非給了 `--allow-new-key`。建立後的報告與檢查時的 endorsement key 要相同。
4. **TPM 已有金鑰時**：沿用它，不再執行第二次；檢查的報告已經帶著以這把金鑰簽的 CSR。工作站有紀錄、這把金鑰卻不是紀錄的那把時，同樣停在結束碼 20，除非給了 `--allow-new-key`。
5. **檢查報告**（agent 的讀者義務，crates/protocol/README.md）：agent 結束碼 0、而且 stdout 是一份報告才採用，否則 stdout 整份丟掉；stdout 最多讀 16 KiB，超過就不採用；`hardware_id` 等於 `--hardware-id`、`key_handle` 等於 `--handle`；endorsement key 是 P-256 的公鑰；有 EK 憑證時它的公鑰要是這把 EK；CSR 由工具自己解析：主體只有一個 CN、等於硬體 ID，公鑰與報告的 `public_key` 逐 byte 相同，簽章驗得過。任何一項不過就停在結束碼 21，不登錄。
6. **登錄**：以報告的 CSR 與 endorsement key，走下方「登錄的步驟」。
7. 印出金鑰是這次建立的還是沿用的、公鑰的 SHA-256，以及登錄的結果。

**到閘道器的只有這些**：ssh 的命令列是上面的固定選項、`--`、目的地，以及一個固定的 `sh -c '<固定的腳本>'`，參數只有硬體 ID、handle、`inspect` 或 `provision`、`sudo` 或 `direct`，上傳時再加 agent 的 SHA-256；標準輸入只有 agent 執行檔（上傳時），否則是空的。ssh 的環境只留 `PATH`、`HOME`、`USER`、`LOGNAME`、`SSH_AUTH_SOCK`、`XDG_RUNTIME_DIR`。session cookie、驗證碼、`--headers-file` 的值、工作站的私鑰與憑證、登錄請求的簽章標頭都不會到閘道器（`tests/ssh.rs` 以假的 ssh 記錄每次呼叫的 argv、環境與 stdin 檢查）。參數只能是英數字、`.`、`_`、`-`，不以 `-` 開頭，所以引號、空白、`;`、`$(…)` 這類值在送出前就被拒絕（結束碼 2），不靠跳脫。

**上傳的 agent**：遠端腳本在 `mktemp -d` 建的私有目錄（umask 077）收下 agent，以 `sha256sum` 比對工作站送的 SHA-256，不符就不執行（遠端腳本以狀態 91 結束，工具的結束碼 22）；執行完、失敗或連線中斷（HUP、INT、PIPE、TERM）都會刪除那個目錄（sshd 以預設的訊號處理啟動 session；若閘道器上的 shell 一開始就忽略某個訊號，POSIX shell 無法再 trap 它，那個訊號就不會觸發刪除，留下的目錄是只有 ssh 使用者讀得到的 0700 目錄）。閘道器的暫存目錄（`TMPDIR` 或 `/tmp`）掛載成 `noexec` 時 agent 無法執行（狀態 126），改用出廠映像安裝的 agent 或換 `TMPDIR`。

**ssh 的重試**：ssh 自己的失敗（結束碼 255：連不上、連線中斷）與逾時都會重試：最多 5 次，第一次開始 2 分鐘後不再開始新的嘗試，間隔從 1 秒起加倍、上限 10 秒。每次嘗試（上傳、執行與結束）最多 300 秒，在閘道器上的一次執行（檢查或 `provision`，含所有重試與等待）最多 420 秒，到了就結束 ssh。重試是安全的：`provision` 是冪等的，回答遺失後重跑會找到上一次建立的金鑰、報告同一把公鑰（`created` 變成 `false`），不會多建立一把。agent 自己的失敗（結束碼 1 到 7，或上傳的 90、91）不重試，重試也不會變；工具印出 agent 結束碼的意思與閘道器 stderr 的最後幾行。

**重跑的意思**：同一個指令再跑一次是安全的。TPM 的金鑰沿用（「已經在、沿用」），平台回 200 與同一筆登錄、同一個序號，紀錄維持 `registered`。重刷 SD 卡不會改變 TPM：重刷後重跑一樣得到 200 與同一個序號（重刷後閘道器沒有安裝 agent 時給 `--upload-agent`）。

**TPM 被清除過**（`tpm2_clear`、BIOS 或韌體清除）：TPM 的金鑰已經不在，清除過的 TPM 是新裝置（決策 12）。清除不會改變 EK，所以工具認得出是同一顆 TPM：工作站有這個硬體 ID 的紀錄、EK 和紀錄相同，檢查時 TPM 卻沒有金鑰（或有一把不是紀錄的金鑰，例如清除後又建立過），工具停在結束碼 20，**不建立金鑰、不送出**。也可能是 `--handle` 給了和上次不同的 handle，先核對。確定是同一台而且要讓平台回答時，加 `--allow-new-key` 再跑：工具建立新金鑰（或沿用 handle 上那把）並送出。平台已經登錄這個硬體 ID 時，以 409 `public_key_mismatch` 拒絕（結束碼 10），這台閘道器要新的硬體 ID；紀錄還是 `pending`、平台其實也還沒登錄過時，平台會接受新金鑰（201）。工作站沒有紀錄（例如在另一台工作站登錄的閘道器）時看不出這種情況，由平台以 409 擋下。

**EK 憑證**：TPM 沒有 EK 憑證時照樣登錄，工具在 stderr 印出提示，日誌的 `provisioned` 記 `ek_certificate: absent`。理由：登錄只送 EK 公鑰，平台目前不驗 EK 憑證鏈、也沒有製造商的根，拒絕只會擋下這些 TPM 而不增加保證；EK 憑證存在時仍要求它屬於這個 EK（agent 檢查，工具再檢查一次）。之後驗製造商的憑證鏈時再改為拒絕（決策 12）。

#### 登錄的步驟（兩種來源相同）

1. 送出前先檢查輸入（CSR 的簽章與金鑰型別、endorsement key 的格式）、工作站私鑰與憑證（同 `install-certificate` 的檢查），以及工作站的紀錄：同一把閘道器公鑰已經以別的硬體 ID 記為 `registered` 時，停在結束碼 11，**什麼都不送**。請求本文只建一次，之後每次嘗試都送同一份。
2. 這個硬體 ID 還沒有 `registered` 的紀錄時，先寫一筆 `pending`。
3. 每次嘗試都用新的時間、新的 nonce 重新簽章（certs 服務拒絕用過的 nonce），開一條新連線只送一次。
4. 連不上、連線中斷、逾時（`--timeout`）、502、503、504，以及 Cloudflare 的 520 到 524 會重試（524 是 Cloudflare 等不到平台的回應，平台可能已經提交），間隔從 1 秒起每次加倍、上限 30 秒，取其中一半到全部之間的隨機值。重試是安全的：登錄 API 是冪等的，平台已經登錄但回應遺失時，下一次嘗試得到 200 與同一筆登錄，不會多登錄一次。TLS 失敗（例如憑證不受信任）不重試，重試也不會變。
5. 200 或 201 先檢查再算數：欄位大小（`workstation` 不超過 256 bytes、`certificate` 與 `root_certificate` 各不超過 8 KiB、`certificate_chain` 不超過 16 KiB、`ca_key_id` 是 64 個小寫十六進位數字，讓寫下的紀錄一定讀得回來）、硬體 ID、`public_key_sha256` 與 `endorsement_key_sha256` 等於工作站自己算的、憑證的 CN 是這個硬體 ID、憑證的公鑰是 CSR 的公鑰、`certificate_chain` 剛好是這張憑證再接一張簽發 CA 憑證，而且簽發 CA 的金鑰驗得過憑證、`root_certificate` 的金鑰驗得過簽發 CA，以及根憑證的 SHA-256 等於 `--root-sha256`。任何一項不符就不記為 `registered`（結束碼 18），也不重試。鏈只有憑證本身（重送一筆簽發 CA 已輪替移除的舊登錄，那張憑證理應已過期）也算不符，那台閘道器要走換發。
   - 鏈的檢查只證明回應自己前後一致；**證明它來自這個平台的 CA，靠的是 `--root-sha256`**，所以它是必填。`root_certificate` 會成為閘道器驗 broker 的唯一信任錨（Laysi/trustiot-v2#617）；只靠到平台的 TLS 時，能改寫回應的人（平台前面的 TLS 終端、被改掉的 `SSL_CERT_FILE` 或 `SSL_CERT_DIR`、給錯的 `--ca-file`）可以用自己的根簽一張相符的憑證，登入錯的平台也不會察覺。
6. 通過時再查一次第 1 步的重複公鑰，然後把紀錄原子替換成 `registered`（含平台回的整筆登錄、憑證、憑證鏈與根憑證，以及釘住的 `root_sha256`），印出硬體 ID、序號、到期時間、嘗試次數與紀錄檔路徑。

**重跑永遠安全**：同一個指令再跑一次，平台回 200 與同一筆登錄，紀錄維持 `registered`。結束碼 75（放棄）、23（閘道器連不上）、1（例如紀錄寫不進去，或平台回 500）或中途被中斷時，修正原因後重跑同一個指令即可。

第 1 步要讀得了工作站上每一筆紀錄。有一筆讀不了（權限、擁有者、內容損壞）時，每次登錄都停在結束碼 1，訊息寫出那個檔案：權限或擁有者錯了就改回 0600 與自己；內容壞了就把它移出 `registrations/`（保留下來，和 `registrations.jsonl` 對照），再重跑。

409、401、403、422 與回應檢查失敗都只送一次、不重試。結束碼見下方「結束碼」。

日誌 `registrations.jsonl` 每行有 `at`、`hardware_id`、`event`，以及該事件的欄位：`provisioned`（`--gateway` 時：handle、金鑰是否這次建立、`ek_certificate` 是 `for_this_key` 或 `absent`、兩個雜湊、是否給了 `--allow-new-key`、上傳的 agent 的 SHA-256）、`provision_stopped`（`--gateway` 時停在登錄之前：步驟、agent 的結束碼或 ssh 的嘗試次數、問題、停止的種類）、`started`（平台網址、釘住的 `root_sha256`、兩個雜湊、工作站憑證序號、之前是否已記為 registered）、`attempt_failed`（第幾次、問題：`connect`、`exchange`、`timeout` 或 `status 503` 之類）、`registered`（第幾次、狀態碼、序號、`ca_key_id`、到期時間）、`stopped`（狀態碼、訊息 key、原因、停止的種類）、`gave_up`。紀錄與日誌都不含 cookie、私鑰、簽章或 nonce。

### 6. `verify`：確認平台記下了登錄

```sh
trustiot-workstation verify --hardware-id <硬體 ID>
```

以平台管理員的 session 讀平台的事件紀錄（`GET /api/identity/v1/platform/event-log`，services/identity/README.md「事件層與操作紀錄」），找這個硬體 ID 的 `certs.gateway_registered` 事件（certs 服務第一次登錄一台閘道器時寫一筆，重跑得到的 200 不再寫）。

- 印出每一筆的時間、序號、登錄的工作站與公鑰的 SHA-256，並和這台工作站的紀錄比對：序號與公鑰都相同時印「it matches this workstation's record」，每一筆都相同才是結束碼 0；有一筆不同時在 stderr 說明那是這個硬體 ID 的另一筆登錄，結束碼 25，請回報平台管理員。
- 這台工作站沒有這個硬體 ID 的 `registered` 紀錄時，找到的事件無從比對：照樣印出，結束碼 26。到登錄那台閘道器的工作站上跑 `verify`。
- 這台工作站有 `registered` 的紀錄時，從紀錄的 `registered_at` 前 1 小時開始找；沒有紀錄時從頭找。每頁 200 筆，最多讀 50 頁，讀到上限還沒找到時訊息會說只找了最新的幾筆。
- 找不到時結束碼 24：事件紀錄在登錄後幾秒才寫入，稍等再跑 `verify`；一直沒有時重跑同一個 `register`（安全），仍沒有就回報平台管理員。
- 平台自己的 401 時和 `enroll` 一樣先撤銷再刪 session 檔，請重新 `login`。

### 7. `logout`：登出

```sh
trustiot-workstation logout
```

以 session 送出 `DELETE /api/identity/v1/sessions/current`：平台撤銷 session（204），或 session 早已結束（平台自己的 401，`code` 是 `unauthenticated`），兩種情況都刪除 session 檔。沒有回應或其他錯誤時保留檔案（session 可能仍有效），之後再試。工作站交班或不再使用時都要登出。

## 結束碼

0 完成、1 失敗、2 命令列用法錯誤適用每個指令；10 到 23 與 75 是 `register` 的，24 到 26 是 `verify` 的。

| 結束碼 | 意義 | 怎麼處理 |
| --- | --- | --- |
| 0 | 完成；`register` 是登錄完成（201 新登錄，或 200 平台已有同一筆），`verify` 是找到了事件、而且每一筆都和這台工作站的紀錄相同 | `register` 之後跑 `verify`；把憑證交給閘道器是 Laysi/trustiot-v2#617 |
| 1 | 其他失敗：檔案被拒絕、有一筆紀錄讀不了、TLS 失敗、平台回了沒有對應處理的錯誤、紀錄寫不進去、找不到 `ssh`、session 已結束（`verify`） | 看訊息。500 或其他 5xx，以及沒有 M1 錯誤本文的回應（平台前面的 proxy 回的），訊息會說平台可能已經登錄、重跑是安全的：重跑同一個指令。平台已登錄但紀錄寫不進去時，修正後重跑 |
| 2 | 命令列用法錯誤：硬體 ID、handle 或 ssh 目的地不合規則，選項的組合不對；ssh 沒有執行、什麼都沒送 | 修正指令 |
| 10 | 409：這個硬體 ID 已經以別的 TPM 金鑰登錄 | 核對標籤；標籤沒錯就是 TPM 被清除過，那是新裝置，要新的硬體 ID（決策 12） |
| 11 | 409：這個 TPM 已經以別的硬體 ID 登錄；或工作站的紀錄裡同一把公鑰已登錄在別的硬體 ID（沒有送出） | 核對標籤 |
| 12 | 409：這個硬體 ID 已經除役，不再登錄 | 回報平台管理員 |
| 13 | 401：session 已結束（工具先撤銷再刪 session 檔，同 `enroll`） | 重新 `login` 再重跑 |
| 14 | 403 `workstation_refused`，原因 `stale`：工作站時鐘與平台差超過 300 秒；訊息附上平台回應的 `Date` 標頭與差距 | 校正時鐘（NTP）再重跑 |
| 15 | 403 `workstation_refused`，原因 `revoked`、`expired`、`untrusted` 或 `not_workstation`：工作站憑證本身被拒絕 | 請平台管理員重新簽發工作站憑證（`enroll`） |
| 16 | 403 `permission_denied`：登入的不是平台管理員 | 以平台管理員登入 |
| 17 | 422：欄位不合規則，沒有登錄 | 看訊息裡的欄位 |
| 18 | 200 或 201，但回應不是這台閘道器的登錄，或根憑證不是 `--root-sha256` 釘住的那張（登錄的第 5 步） | 不要把任何東西裝到閘道器上，回報平台管理員 |
| 19 | 403 `workstation_refused`，原因 `missing`、`malformed`、`signature_invalid`、`replayed`，或工具不認得的原因：簽章標頭沒有照送出的樣子到達平台（平台前面的 proxy 拿掉或改寫了 `trustiot-workstation-*` 標頭或本文）；`replayed` 是中間有人重送了請求，或工具的 bug（工具每次嘗試都用新的 nonce） | 不要重新 `enroll`，工作站憑證不是問題；帶著 `registrations.jsonl` 回報平台管理員 |
| 20 | `--gateway`：工作站有這個硬體 ID 的紀錄，閘道器的 TPM 是紀錄的那顆（EK 相同），在 handle 卻沒有金鑰或是另一把金鑰（TPM 被清除過）；沒有建立金鑰、沒有送出 | 核對標籤與 `--handle`；確定是同一台才加 `--allow-new-key` 重跑（平台已登錄這個硬體 ID 時回 409，結束碼 10，見「TPM 被清除過」） |
| 21 | `--gateway`：閘道器的報告沒通過檢查（硬體 ID 或 handle 不同、CSR 的主體、公鑰或簽章不對、EK 或 EK 憑證不對、檢查的報告說建立了金鑰、超過 16 KiB、不是報告）；沒有登錄。報告的硬體 ID 只是照抄 `--hardware-id`，所以這項檢查認不出 ssh 接到的是沒有紀錄的另一台閘道器，那要靠核對 ssh 目的地與標籤 | 確認目的地是標籤上那台閘道器、agent 是發行的版本；再發生就帶著 `registrations.jsonl` 回報 |
| 22 | `--gateway`：agent 在閘道器上失敗（訊息有它的結束碼與意思、閘道器 stderr 的最後幾行），或上傳失敗、上傳的 agent 的 SHA-256 不符；stdout 不採用，沒有登錄 | 依訊息處理：沒有 TPM 裝置、handle 被別的物件佔用（5）、EK 憑證不符（6）要檢查閘道器；127 是沒裝 agent，給 `--upload-agent`；126 是暫存目錄 `noexec`；sudo 要密碼時設定免密碼或拿掉 `--remote-sudo` |
| 23 | `--gateway`：ssh 重試後仍連不上閘道器或逾時（最多 5 次、420 秒內）；沒有登錄 | 檢查網路、目的地與 ssh 金鑰（以 `ssh <目的地> true` 試），再重跑同一個指令；訊息有 `REMOTE HOST IDENTIFICATION HAS CHANGED` 時見上面「重刷後主機金鑰改變」 |
| 24 | `verify`：平台的事件紀錄裡沒有這個硬體 ID 的 `certs.gateway_registered` 事件 | 稍等幾秒再跑；一直沒有就重跑同一個 `register`，再回報平台管理員 |
| 25 | `verify`：找到這個硬體 ID 的事件，但有一筆的序號或公鑰和這台工作站的紀錄不同（這個硬體 ID 有另一筆登錄） | 不要把憑證裝到閘道器上；帶著 `registrations.jsonl` 與印出的事件回報平台管理員 |
| 26 | `verify`：找到這個硬體 ID 的事件，但這台工作站沒有它的 `registered` 紀錄，無從比對 | 到登錄這台閘道器的工作站上跑 `verify` |
| 27 | `--gateway`：工作站有這個硬體 ID 的紀錄，閘道器回報的 EK 卻不是紀錄的那把：是另一顆 TPM（ssh 接到別台閘道器，或標籤錯了）；沒有建立金鑰、沒有送出，`--allow-new-key` 也不放行 | 核對 ssh 目的地與標籤，改接正確的閘道器或改用正確的硬體 ID；見「硬體 ID 從標籤來」 |
| 75 | 用完 `--attempts` 或 `--retry-for` 仍沒有確定的回應（sysexits 的 `EX_TEMPFAIL`） | 網路恢復後重跑同一個指令 |

## 出錯時怎麼辦

- **先看結束碼與訊息**，再看工作站目錄的 `registrations.jsonl`（每次執行的每一步都有一行，不含任何密鑰）。
- **重跑同一個指令永遠安全**：TPM 的金鑰沿用、登錄 API 是冪等的，重跑不會多建立金鑰，也不會多登錄一筆。結束碼 1、23、75 或被中斷時，修正原因後直接重跑。
- **不要換硬體 ID 重試**：結束碼 10、11、20、21、27 都表示這台閘道器與紀錄對不上，先核對標籤與 ssh 目的地；換一個硬體 ID 會讓同一個 TPM 登錄成另一台閘道器（平台以 409 擋下，結束碼 11）。
- **不要手動改紀錄**：`registrations/` 的紀錄由工具原子替換；壞掉的紀錄移出目錄保存，再重跑。
- 回報時附上結束碼、訊息與 `registrations.jsonl`；不要附 session 檔、私鑰或 `--headers-file`。

## 經 Cloudflare Access（`--headers-file`）

展示環境前面有 Cloudflare Access 時（Laysi/trustiot-v2#619），以 Access 的 service token 通過：

```text
# Cloudflare Access service token
cf-access-client-id: <client id>
cf-access-client-secret: <client secret>
```

- 只接受這兩個標頭，兩個都要有、各一次；空行與 `#` 開頭的行略過。其他標頭名稱一律拒絕。格式不對的行只以行號回報，不顯示那一行的任何內容。
- 檔案權限要是 0600。client secret 只交給持有工作站的操作員，不進 repo、issue、PR 或日誌；工具不把標頭值寫進日誌或錯誤訊息。
- 每個請求（`login` 的兩步、`enroll`、`register` 的每次嘗試、`verify` 的每一頁、`logout`）都帶上；不會送到閘道器。

## 輸出與日誌

- 結果印在 stdout，提示與日誌印在 stderr。結束碼見「結束碼」。
- `TRUSTIOT_WORKSTATION_LOG` 設定日誌等級：`off`、`error`、`warn`、`info`（預設）、`debug`、`trace`。只記這個工具自己的事件（方法、路徑、狀態碼、檔案路徑），連 `trace` 也不記驗證碼、cookie、`Set-Cookie`、私鑰、`--headers-file` 的值，或登錄請求的簽章與 nonce；email 地址也不記。
- 平台回錯誤時只顯示狀態碼與 M1 錯誤的 `code`、訊息 key 與欄位，不顯示回應本文的其他內容。這些來自伺服器的文字與序號在印出前，控制字元（終端機的跳脫序列、換行）、格式字元（例如會反轉顯示方向的 U+202E、零寬空白 U+200B）與行、段落分隔符都改寫成 `\u{..}`，過長的截斷。

## 安全上的做法與限制

- **`http://` 只限 loopback 的 IP 位址**：不接受 `localhost` 這個名稱，它經過名稱解析，可能被導向別處。
- **只有 `register` 重試**：每個請求用一條新的連線、只送一次；沒有連線池（連線池可能在連線被關掉時重送請求）。`login`、`enroll`、`logout` 不重試。`register` 的重試是工具自己的一次新嘗試（新的簽章與 nonce），只在登錄 API 的冪等保證之下才做，見「`register`」。
- **ssh**：工具執行系統的 `ssh`（`PATH` 上的），不自己實作 SSH；金鑰與 `~/.ssh/config` 照 OpenSSH 的設定，主機金鑰固定以硬體 ID 為名、只查工具自己的 `known_hosts`，執行前以 `ssh -G` 確認沒有被設定關掉。工具只控制命令列（固定選項、`--` 之後才是目的地，目的地不能以 `-` 開頭）、遠端指令（固定的腳本，參數只有檢查過的字）、環境（只留六個變數）與標準輸入（只有要上傳的 agent）。閘道器回的 stdout 最多讀 16 KiB、stderr 最多留 8 KiB，印出前改寫控制字元與格式字元。閘道器被控制時它能回任何報告：工具的檢查保證報告前後一致（CSR 的 CN 是這個硬體 ID、公鑰與簽章相符），有紀錄時 EK 要和紀錄相同，但報告的硬體 ID 只是照抄 `--hardware-id`，也證明不了金鑰在 TPM 裡，這要靠出廠流程的實體控制（決策 12）。**殘餘風險**：以 EK 做 TPM2_MakeCredential／TPM2_ActivateCredential、證明金鑰與 EK 在同一顆 TPM 裡，這次沒有做，延後另開 issue；在那之前，被控制的閘道器（或冒用位址的機器）第一次登錄時能送出不在 TPM 裡的金鑰。
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
- `tests/register.rs`：對 loopback 上的登錄替身（`tests/support/registry.rs`，以 `contract::certs::workstation_signing_input` 驗證每個請求的簽章、拒絕用過的 nonce）以 TRACE 跑 `register`：回應遺失後重送得到 200 算成功、503 後重試成功、每次嘗試的 nonce 與簽章都不同；409（四種金鑰衝突與除役）、401、403 `stale`、403 `revoked`、403 `signature_invalid`、403 `permission_denied`、422 各自以固定的結束碼停止、只送一次；回應的硬體 ID、兩個雜湊、CN、憑證公鑰不符，鏈的第一張不是憑證、少了簽發 CA 或換成根沒簽過的 CA，或整套換成別的根（只有 `--root-sha256` 擋得下）時不記為 `registered`；工作站紀錄裡同一把公鑰已登錄在別的硬體 ID 時不送出；達到嘗試上限時結束碼 75、重跑完成；寫入 `registered` 紀錄時程序被 `ulimit -f` 中斷（SIGXFSZ），原本的紀錄完整、只留下會被略過的暫存檔，重跑完成；並在所有輸出、紀錄與日誌中搜尋 cookie、回應的 `Set-Cookie`、私鑰、簽章與 nonce。也直接呼叫函式庫函式 `register::register`（`--gateway` 也用它）。
- `tests/ssh.rs`（Linux）：`register --gateway` 經放在 `PATH` 最前面的假 `ssh`（`tests/support/ssh.rs`，記錄每次呼叫的 argv、環境與 stdin，在假的遠端以 `sh -c` 執行遠端指令）：ssh 的 argv 剛好是固定選項、目的地與預期的遠端指令，stdin 只有 agent，環境只有保留的變數，cookie、驗證碼、Access 標頭、工作站私鑰與憑證、簽章與 nonce 都沒有到閘道器；上傳的 agent 執行後、SHA-256 不符時與被 HUP 時暫存目錄都被刪除；已安裝的 agent 以 `sudo -n` 執行；含空白、`;`、引號、`$(…)`、反引號、換行的硬體 ID、handle 與目的地在 ssh 執行前被拒絕；ssh 連不上與回答遺失時重試、每次的報告相同，重試用完結束碼 23；agent 失敗 22、報告不合讀者義務 21、已登錄的硬體 ID 的 TPM（EK 相同）沒有金鑰或換了一把金鑰 20、`--allow-new-key` 後平台的 409 是 10；同一個硬體 ID 換了 EK（另一顆 TPM）有沒有金鑰、給不給 `--allow-new-key` 都停在 27，只執行了檢查。
- `tests/pipeline_swtpm.rs`（Linux，需要 podman 或 docker；`CI` 有設時不跳過）：假的遠端上的 agent 是 bash 的轉接，把參數交給測試，由測試以 `agent::provision::run` 對 swtpm（`testkit::swtpm`）執行真的 `trustiot-agent provision`。同一台閘道器依序：第一次連線失敗、`provision` 的回答遺失後重試，201，每次重試報告同一把金鑰；`verify` 找到事件；以已安裝的 agent 重跑 200、同一個序號；模擬重刷（遠端檔案清空、新的 TPM 連線）200、同一個序號；`tpm2_clear` 之後停在 20、沒有建立任何東西；`--allow-new-key` 得到 409 `public_key_mismatch`，結束碼 10；之後同一顆 TPM（`tpm2_clear` 沒有改變 EK）帶著那次建立的金鑰，停在 20、什麼都沒送出。
- `tests/verify.rs`（Linux）：`verify` 跟著 `next_cursor` 讀完事件紀錄、只取這個硬體 ID 的事件、從紀錄的 `registered_at` 前 1 小時開始，與紀錄比對；找不到時 24；平台的 401 撤銷並刪除 session 檔。並檢查 `--help` 列的每個指令與選項都在本 README，結束碼表每一列都在。
- `services/certs/tests/workstation_tool.rs`（certs 服務的測試，需要 podman 或 docker）：真的 certs 服務與 Postgres，前面是檢查 cookie、換成內部 token 的 ingress 替身。工具 `enroll` 後登錄、重跑、再讓回應在服務提交後遺失，每個硬體 ID 都剛好一筆登錄、一張閘道器憑證、一個 `certs.gateway_registered` 事件。
- 各模組的單元測試：包裝型別的遮蔽、檔案權限、FIFO 與符號連結目錄被拒絕、擁有者檢查、網址規則（含 `localhost` 被拒絕）、標頭檔格式與錯誤訊息不含內容、伺服器文字的控制字元與格式字元改寫、HTTPS 只信任 `--ca-file`、簽章可由 ring 驗證、nonce 不重複、重試間隔與上限、紀錄的原子替換與格式檢查、日誌的權限檢查。
