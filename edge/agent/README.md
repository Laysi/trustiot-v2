# agent

閘道器 agent（[決策 10](../../docs/architecture/decisions.md)）：在現場閘道器上讀取設備、把資料送上雲端的程式。目前有 Modbus 讀取（`modbus` 模組，[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) F1、F5）與 TPM 裡的身分金鑰（`tpm` 模組，[決策 12](../../docs/architecture/decisions.md)）；systemd 服務、MQTT 連線、排程與離線緩衝之後建立在它上面。

| 在哪裡 | 內容 | issue |
| --- | --- | --- |
| `src/modbus.rs` | `DeviceReader`：依型號規劃讀取、送出、解碼並蓋時間戳；`Connection`、`Clock` 介面 | Laysi/trustiot-v2#87 |
| `src/modbus/plan.rs` | 合併讀取的規劃 | Laysi/trustiot-v2#87 |
| `src/modbus/pdu.rs` | 功能碼 1–4 的請求與回應 PDU | Laysi/trustiot-v2#87 |
| `src/modbus/decode.rs` | 型別、位元組順序、scale 與 offset | Laysi/trustiot-v2#87 |
| `src/modbus/tcp.rs` | Modbus TCP 的 `Connection`（MBAP） | Laysi/trustiot-v2#87 |
| `src/tpm.rs`、`src/tpm/` | `TpmKey`：TPM 內的身分金鑰（TPM2_ReadPublic、TPM2_Sign）與 rustls 的 `SigningKey` | Laysi/trustiot-v2#86 |
| 還沒有 | RTU-over-TCP 與同一台轉換器共用連線 | Laysi/trustiot-v2#88 |
| 還沒有 | 固定節拍、重連退避、TCP keepalive、watchdog | Laysi/trustiot-v2#89 |
| 還沒有 | 系統時鐘的 `Clock`（時鐘是否已校正、校正來源） | Laysi/trustiot-v2#90 |
| 還沒有 | 讀取統計（`DeviceStats`）與心跳 | Laysi/trustiot-v2#117 |
| 還沒有 | 執行檔、systemd unit、MQTT 連線 | Laysi/trustiot-v2#86 |

依賴都要能建置成 aarch64（`aarch64-unknown-linux-gnu`），不需要額外的系統函式庫；CI 對 `edge/` 下每個成員做這項建置。rustls 的 crypto provider `ring` 會以目標的 C 編譯器編譯 C 與組合語言：arm64 主機上原生建置，x86_64 工作站以 cargo-zigbuild（Zig 當 C 編譯器）建置。

## Modbus 讀取

每台設備建一個 `DeviceReader::new(&device, &model)`（`crates/protocol` 的 `config::Device` 與它的 `DeviceModel`，已經過 `DesiredConfig` 的驗證），每次輪詢呼叫 `reader.poll(&mut connection, &clock, timeout)`，得到 `Poll`：

- `readings`：每個讀到並解碼成功的資料點一筆 `telemetry::Reading`，時間是該請求的回應到達時 `Clock::now()` 的毫秒時間，帶 `clock_synced`。
- `failed`：其他每個資料點與原因（`health::LastFailure`：種類、時間、例外碼、說明）。
- `requests`：送出或連線失敗的每個請求、它的失敗與丟棄的站號不符回應數，給之後的讀取統計（H1）用。

### 只讀

請求只能從 `protocol::config::FunctionCode` 建立，它只有 1–4，所以程式送不出功能碼 5 以上（閘道器對現場設備唯讀）。整合測試在每次輪詢後檢查假端點的請求紀錄，確認沒有 1–4 以外的功能碼。

### 合併讀取

同一個功能碼、位址相距不超過 `MAX_GAP`（10 個沒讀的位址）的資料點合併成一個讀取，一個讀取最多 125 個暫存器或 2000 個位元（一個回應的上限）；依功能碼、位址排序後由前往後合併。中間沒讀的位址也會被讀取，結果丟掉。

### 解碼與縮放

型別與位元組順序依 `config::DataType`、`config::ByteOrder` 的定義（`ByteOrder` 是兩個獨立的對調：`badc`、`dcba` 對調每個暫存器內的兩個 byte，`cdab`、`dcba` 反轉暫存器的順序）。

- 整數送 `Value::Int`；`u64` 大於 `i64::MAX` 是讀取失敗（`invalid_value`），除非有縮放。
- 浮點數送 `Value::Float`；NaN 或無限大是讀取失敗。
- 字串每個暫存器兩個字元（`badc` 時低位元組在前，不做字組對調），到第一個 NUL 為止並去掉前後空白；不是 UTF-8 的 byte 換成 U+FFFD。
- 暫存器上的 `bool` 是「不為 0」。
- 設了 `scale` 或 `offset` 的數值一律送 `Value::Float`：`raw * scale + offset`，沒設的 scale 當 1、offset 當 0；整數先轉成 `f64`，超過 2^53 會失去最低位。結果不是有限數（例如溢位）是讀取失敗。

### 哪個回應算數（Modbus TCP）

回應的站號與交易 ID 都符合請求才算數。其他回應讀進來丟掉，請求繼續等到逾時：站號不符的計入 `unit_mismatches`（轉換器把回應送給所有連線，Laysi/trustiot#183）；站號相同、交易 ID 不同的是先前逾時的請求晚到的回應。所以晚到的回應不會被當成下一個請求的值。兩個請求之間到達的完整框架回答不了任何還在等的請求，在下一個請求送出前就丟掉（站號不符的一樣計入）。交易 ID 從隨機值開始，與同一台設備的其他 client（通常從 0 或 1 數起）同時用到同一個 ID 的機會很小。

收到的 bytes 先放進緩衝、湊滿一個 MBAP 框架才處理，逾時打斷時已收到的部分留在緩衝，框架不會錯位。緩衝最多一個框架加一次 socket 讀取；協定 ID 不是 0、長度不在 2–254 的標頭表示串流不是 Modbus TCP，連線關閉。

### 失敗與輪詢

| 狀況 | `ReadError` | 回報的種類（`FailureKind`） | 輪詢 |
| --- | --- | --- | --- |
| 連不上（含連線逾時） | `Connect` | `connect` | 其餘請求不送 |
| 請求途中連線被關閉或 reset、送出失敗（重送一次之後仍然如此，見下方） | `ConnectionLost` | `connect` | 其餘請求不送，下一次輪詢重連 |
| 串流不是 Modbus TCP（例如講 RTU over TCP 的轉換器被設成 Modbus TCP） | `Protocol` | `invalid_value` | 其餘請求不送，下一次輪詢重連 |
| 等不到符合的回應 | `Timeout` | `timeout` | 繼續下一個請求，連線保留 |
| 例外回應 | `Exception` | `exception`（帶例外碼） | 繼續 |
| 符合的回應與請求對不上（功能碼、byte 數） | `Malformed` | `invalid_value` | 繼續 |
| 解碼不出可送的值 | — | `invalid_value`（只有該資料點） | 繼續 |

連線中斷算 `connect`：與連不上一樣是 TCP 連線的問題，`timeout` 則是連線正常但設備不回應；連線正常但回來的東西無法解讀算 `invalid_value`。只收到其他站號回應的請求算 `timeout`，那些回應另外計入 `unit_mismatches`，與 `health::FailureCounts` 的定義相同。逾時之後不中止輪詢：有些設備對某些位址不回應，中止會讓後面的資料點一直讀不到；設備整台不回應時一次輪詢要等「請求數 × 逾時」，由 Laysi/trustiot-v2#89 的排程處理。

`TcpConnection` 在第一次讀取時才連線，連線失敗後的下一次讀取重新連線。設備或閘道器可能隨時關掉保持中的連線（閒置逾時，或每次回答後就關）：

- 關閉在請求之前已經到達：送出前就換新連線。
- 請求送出後才發現連線已關閉或被 reset：在新連線上重送一次，讀取不會改變設備的狀態；重送也失敗才是 `ConnectionLost`。剛開好的連線上失敗不重送，所以不會一直重試。

呼叫端以自己的逾時丟掉讀取中的 future 也是安全的：請求寫到一半被丟掉的連線，下一次讀取會換掉；已收到的 bytes 留在緩衝。預設逾時是請求 5 秒、連線 10 秒（`DEFAULT_REQUEST_TIMEOUT`、`DEFAULT_CONNECT_TIMEOUT`，F4）。

### 限制

- 合併讀取會讀到中間沒設定的位址。設備對不存在的位址回例外碼 02 時，整個合併讀取的資料點都失敗；目前沒有自動拆開重試。
- 規定回應要帶請求的站號。回應固定帶其他站號（例如 0 或 255）的 Modbus TCP 設備讀不到，健康報告裡會看到 `unit_mismatch` 與 `timeout`。
- Modbus TCP 只能以站號與交易 ID 認回應。另一個 client（例如現場的 SCADA）經同一台會把回應送給所有連線的閘道器讀同一個站號，而且剛好用到同一個交易 ID、讀的筆數相同時，它的回應會被當成這個請求的；隨機的起始 ID 只降低機率。會廣播回應的通常是 RTU over TCP 的轉換器，由 Laysi/trustiot-v2#88 以共用連線、輪流讀取處理；兩台設備設定成同一組主機、port 與站號時也一樣，設定驗證目前沒有擋。
- 半開連線（設備不送 FIN 或 RST 就忘了連線）只會表現為逾時；偵測並重連是 Laysi/trustiot-v2#89 的 keepalive 與 watchdog。
- 主機寫 DNS 名稱時，名稱解析在 blocking 執行緒上進行，連線逾時之後它仍會跑完；現場設備多半以 IP 設定。

## TPM 簽章

閘道器的身分金鑰在 TPM 裡、不能匯出（決策 12）。`TpmKey::open(TpmTransport::Device("/dev/tpmrm0".into()), handle)` 經核心的資源管理器使用持久 handle 上的金鑰（預設 `DEFAULT_KEY_HANDLE` = `0x81000100`），`TpmKey` 實作 rustls 的 `SigningKey`，inner mTLS 的 CertificateVerify 就在 TPM 內簽。

- 不用 tss-esapi 或 libtss2：只送兩個指令，TPM2_ReadPublic 與 TPM2_Sign，依 TPM 2.0 Part 2、3 自己編解碼，沒有 C 函式庫要交叉編譯或隨附。
- 回應一律當成不可信的輸入：檢查 tag、長度與回應碼，每個長度都不超過剩下的 bytes，結構要剛好結束，回應最多 4096 bytes；格式不符是錯誤，不會 panic。回應碼解成名稱，例如 `TPM_RC_HANDLE (0x0000018b, handle 1)`。
- `open` 先讀公開區域，金鑰必須是 ECC NIST P-256、scheme 固定為 ECDSA-SHA256，屬性要有 `fixedTPM`、`fixedParent`、`sensitiveDataOrigin`、`userWithAuth`、`sign`，不能有 `restricted`、`decrypt`（2026-10-12 PoC 的範本，產線工具 Laysi/trustiot-v2#80 照這個產生）。接著簽一段固定的測試訊息並以公鑰驗證，簽不出來（例如金鑰設了密碼）就在啟動時失敗，不會等到每次握手才失敗。啟動多一次簽章，閘道器上約 70 ms。
- 簽章：SHA-256 在軟體算，以空密碼的 password session（`TPM_RS_PW`）與 null hashcheck ticket 送 TPM2_Sign，r、s 轉成 DER。rustls 只會拿到 `ecdsa_secp256r1_sha256`。
- 憑證要與金鑰相符：`public_key_spki_der()` 是金鑰的 SubjectPublicKeyInfo，rustls 的 `CertifiedKey::keys_match` 經 `SigningKey::public_key` 比對。
- 一條專用執行緒（`tpm`）擁有裝置，請求經有上限的佇列（8 個）一個一個送，佇列滿了回 `Busy`。rustls 的 `Signer::sign` 是同步呼叫，等待期間佔住呼叫它的執行緒（閘道器上約 70 ms），所以握手不能跑在負責 Modbus 輪詢的執行緒上。等待最多 `RESPONSE_TIMEOUT`（30 秒；核心對這些指令的逾時更短）。執行緒停了（panic）時簽章立即回錯誤，不會卡住；最後一個 `TpmKey` 與 `Signer` 被丟掉時執行緒結束並關閉裝置。
- `TpmTransport::Channel` 接受其他 `TpmChannel`（測試以 TCP 連 swtpm）；正式環境只用 `Device`。

## 測試

```sh
cargo test -p agent                                     # 全部
cargo test -p agent --lib                               # 單元測試：規劃、PDU、解碼、輪詢、MBAP 切框
cargo test -p agent --test modbus_fake_endpoint         # 對 testkit 假端點：型別、合併讀取、每種故障
cargo test -p agent --test modbus_tcp_framing           # 以 raw socket 驗證哪個回應算數、框架同步
cargo test -p agent --test tpm_swtpm                    # swtpm 容器：簽章、mTLS 握手、拒絕不能用的金鑰
```

Modbus 的整合測試都用 `#[tokio::test(start_paused = true)]` 加 `testkit::clock::manual()`：逾時只在測試推進時鐘時觸發，機器再忙也不會誤判逾時。假端點的用法見 [`crates/testkit/README.md`](../../crates/testkit/README.md)「假 Modbus 端點」。

`tpm_swtpm` 以 `tests/swtpm/Containerfile`（Debian 的 swtpm 與 tpm2-tools，基底以 multi-arch digest 釘選）建映像，build context 是空目錄；映像留在本機（`localhost/trustiot-agent-swtpm:<Containerfile 的雜湊>`），下次直接沿用，不要時以 `podman rmi` 移除。測試時以 tpm2-tools 佈建金鑰，比對 tpm2-tools 算的 SubjectPublicKeyInfo、以公鑰驗證簽章，並做一次真的 TLS 1.3 mTLS 握手（測試 CA 為 TPM 公鑰簽發用戶端憑證，CertificateVerify 剛好一次 TPM2_Sign，resumption 不簽章）。沒有 podman 或 docker 時略過，設了 `CI` 時失敗。解析與執行緒的行為在單元測試（`cargo test -p agent --lib tpm`）以 swtpm 錄下的回應與假 TPM 測。
