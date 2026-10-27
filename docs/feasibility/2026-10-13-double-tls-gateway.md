# 可行性測試結果（2026-10-13 雙層 TLS 在閘道器上的 CPU 與延遲）

這一頁記錄 MS1 開始前的閘道器端可行性測試「雙層 TLS 在閘道器上的 CPU 與延遲」：在 lab 的閘道器上量 WebSocket 內包 mTLS（決策 6）的成本，結論是通過。

**數字**

- Enterprise 方案的頻率（1 秒採樣、即時圖開著）下，整條傳輸（兩層 TLS、WebSocket、MQTT）只用單核的 0.44%（ChaCha20-Poly1305），比直連 mTLS 多 0.08 個百分點。
- 閘道器端送出一則 QoS1 訊息，p50 不到 0.1 ms。
- 一次完整重連在閘道器上約 11–13 ms（不含 TPM 簽章），用 session resumption 約 6–8 ms。

**要寫進設計的事**

閘道器的 Cortex-A72 沒有 AES 指令，ChaCha20-Poly1305 比 AES-GCM 快約 4 倍，但 rustls 的預設會選 AES-256-GCM，所以閘道器兩層 TLS 都要明確把 ChaCha20-Poly1305 排第一。另外三件事：

- 開啟 session resumption：重連時不用 TPM 簽章，省下約 70 ms（Laysi/trustiot-v2#74）。
- Mosquitto 設 `set_tcp_nodelay true`。
- 停權的「CRL、SIGHUP、踢線」在開了 resumption 之後仍然有效：SIGHUP 會讓舊的 session ticket 全部失效。

這幾點已寫進[架構決策總表](../architecture/decisions.md)的決策 6。

| 項目 | 結論 | 關鍵數字 | 對決策的影響 |
| --- | --- | --- | --- |
| 穩態 CPU 與延遲 | 通過 | Enterprise 頻率：單核 0.44%（ChaCha20）、0.55%（AES-128）；QoS1 往返 p99 在區網上 1.5–2.9 ms；閘道器端送出路徑 p99：ChaCha20 不到 0.3 ms，AES-128 約 1 ms；每秒 5,000 則 1 KiB 訊息時 23–32% 單核 | 兩層都把 ChaCha20-Poly1305 排第一 |
| 重連成本 | 通過 | 完整握手 p50 11–13 ms（CPU 5–8 ms），resumption 6–8 ms（CPU 2.6–3.6 ms）；TPM 簽章每次加約 70 ms，只發生在完整握手 | 兩層都開 session resumption |
| 各端協商的套件 | 確認 | Envoy（BoringSSL）照用戶端的順序選；Cloudflare 依文件也是。Mosquitto（OpenSSL 3.5）依自己的順序選（2026-10-21 更正，見下方） | 外層由閘道器決定；broker 要以 `ciphers_tls1.3` 把 ChaCha20 排第一 |
| resumption 與撤銷 | 通過 | 發新 CRL 並 SIGHUP 後，被撤銷的閘道器收到 `CertificateRevoked` | 決策 6 的停權步驟不變 |

## 測試方法

依[部署與可搬移性](../architecture/deployment.md)的路徑，在本機重現兩種部署的終止點。閘道器端在 lab 的閘道器上執行，伺服器端在工作站的 podman 上執行。不建立 Cloudflare 資源。

閘道器這邊的成本只取決於三件事：協商出的套件、key exchange、記錄大小與握手方式，和由誰終止 TLS 無關。所以兩條路徑都在本機模擬終止點，網路那一段沿用 2026-10-10 的雲端實測。

- **閘道器替身（在 lab 的閘道器上執行）**：自寫的 Rust 程式，與 2026-10-10 雲端側的組合相同：tokio-tungstenite 加 rustls，MQTT 3.1.1，keepalive 60 秒，clean session，QoS1 同時只有一批在途（G1）。
  - 兩層都只開 TLS 1.3。
  - 內層用軟體 P-256 金鑰做 mTLS。正式產品的金鑰在 TPM，TPM 的成本另外引用 Laysi/trustiot-v2#74 的實測。
  - tokio 用單執行緒 runtime，所有 socket 都開 TCP\_NODELAY。
- **基準**：同一支程式直接以 mTLS 連 Mosquitto，不經 WebSocket，也沒有外層 TLS。
- **SaaS 模型**：外層 TLS 用 rustls 終止，憑證是 ECDSA P-256 的 leaf 加中繼憑證。設定比照 Cloudflare 的協商行為：照用戶端的套件順序選，支援 X25519MLKEM768（見下節）。終止後接 WebSocket 轉接服務。
- **地端模型**：外層 TLS 由 Envoy 終止。Envoy 取自 Istio 1.30.5 的 `proxyv2` 映像（Envoy 1.38.5，BoringSSL），TLS 參數全用預設，憑證是企業 CA 簽的 RSA-2048。Envoy 把 WebSocket upgrade 轉給轉接服務。
- **轉接服務與 broker**：轉接服務把 WebSocket 的 binary message 原樣轉給 Mosquitto 2.1.2 的 mTLS listener，所以內層 mTLS 由閘道器直達 broker，與 2026-10-10 相同。
  - 轉接服務每 50 秒送一次 WebSocket ping，讀寫緩衝區用 8 KiB（決策 6）。
  - Mosquitto 設了 `set_tcp_nodelay true`，原因見「要寫進設計的事」第 4 點。
- **CPU 的量法**：
  - 每個程序：取 `getrusage(RUSAGE_SELF)` 的 user 加 sys，只算連線建立之後的量測窗口，換算成單核的百分比。
  - 整機：取 `/proc/stat` 4 核合計的忙碌比例。它以 10 ms 為單位，低負載時接近雜訊。
  - CPU 調速維持預設的 `ondemand`（600–1800 MHz），低負載時 CPU 會降頻，所以低頻情境每則訊息的 CPU 時間比較高。這是現場的真實狀況。
- **延遲的量法**：從送出 QoS1 PUBLISH 到收到 PUBACK，用 HdrHistogram 統計。
  - 這個往返包含工作站上的轉接服務、Envoy、Mosquitto，以及區網。
  - 閘道器端的處理時間另外以「送出路徑」計：從編碼、加密兩層到 write 返回為止。
  - 量測期間，工作站（8 核）的 load1 在 0.05–1.54 之間，伺服器端沒有滿載。
- **隔離**：每次量測都持有閘道器上的 `~/e2e.lock`，並先記錄閘道器的負載與溫度：load1 不超過 0.29，46–50 °C，`throttled=0x0`。
- 每組設定只量一次。同類設定在每秒 5,000 則的情境下差到 3 個百分點，比較時以這個幅度為誤差。

## 頻率假設

文件沒有寫每台閘道器的設備數與正式資料的上傳間隔，以下取一個範圍，並寫明假設。

| 情境 | 內容 | 依據 |
| --- | --- | --- |
| 閒置 | 只有 MQTT keepalive 60 秒與轉接服務 50 秒一次的 WebSocket ping，5 分鐘 | 決策 6 |
| S1 一般（Business） | 10 台設備、10 秒採樣：即時圖每秒 1 則 200 B（QoS0）；每 10 秒一批 2 KiB（QoS1） | [方案與權益](../product/plans.md)的最短採樣間隔：Business 10 秒；I3：即時頻率等於最短採樣間隔 |
| S2 重度（Enterprise） | 32 台設備、1 秒採樣：即時圖每秒 32 則 200 B；每秒一批 8 KiB | Enterprise 與地端的最短採樣間隔是 1 秒。32 台是假設，取一段 RS485 匯流排常見的上限；上傳間隔是假設，決策 9 只說要縮短，舊系統是 15 分鐘 |
| 延遲 | 每秒 10 則 256 B 的 QoS1，2 分鐘 | 取足夠的樣本（約 1,200 筆）算 p99 |
| 壓力 | 即時每秒 500 則 200 B，加上每 100 ms 一批 16 KiB | 約 S2 的 15 倍，看餘裕 |
| 極限 | 每秒 5,000 則 1 KiB（約 5 MB/s） | 看加密套件的差距何時變得明顯 |

可接受範圍：文件沒有給傳輸層的 CPU 或延遲預算，這裡依閘道器的工作與既有實測自訂三條。

1. **CPU**：在最重的文件頻率（S2）下，整條傳輸不超過單核的 5%。閘道器還要做 Modbus 輪詢、離線緩衝、OTA 與健康回報（H1）。
2. **延遲**：閘道器端每則訊息增加的時間 p99 不超過 10 ms。最短的即時圖間隔是 1 秒（I3）。
3. **重連**：閘道器端的握手成本遠小於 2026-10-10 量到的中斷恢復時間（0.4–3.5 秒）。

## 各端協商的套件

| 項目 | ChaCha20-Poly1305 | AES-128-GCM | AES-256-GCM |
| --- | --- | --- | --- |
| ring 或 aws-lc-rs，1 KiB 加密（µs） | 4.8 | 18.0 | 22.7 |
| ring 或 aws-lc-rs，16 KiB 吞吐（MB/s） | 256 | 58.7 | 46.4 |
| `openssl speed`（OpenSSL 3.5.7），16 KiB（MB/s） | 321 | 70.1 | 59.1 |

閘道器的 CPU 是 Cortex-A72，`/proc/cpuinfo` 的 Features 只有 `fp asimd evtstrm crc32 cpuid`，沒有 `aes` 與 `pmull`。所以 AES-GCM 走軟體實作，ChaCha20-Poly1305 快 4 倍以上。ring 0.17.14 與 aws-lc-rs 1.18.1 在各種大小下的差距都不到 2%。

各端實際協商出的結果：

- **rustls 0.23.45（閘道器）**：預設的套件順序是 AES-256-GCM、AES-128-GCM、ChaCha20，不會因為 CPU 沒有 AES 指令就改選 ChaCha20。兩層都沒設定時，實測協商出 AES-256-GCM；在壓力情境下，比 ChaCha20 多用 59% 的 CPU。
- **Envoy（地端入口，BoringSSL）**：在有 AES 硬體的主機上，BoringSSL 的 TLS 1.3 預設把三種套件放在同一個「同等偏好」組，由用戶端的順序決定。實測用戶端把 ChaCha20 排第一，就協商出 ChaCha20。未設定 `ecdh_curves` 時選 X25519，不選 X25519MLKEM768。
- **Mosquitto 2.1.2（OpenSSL 3.5.8）**：實測得到 ChaCha20，但這次的用戶端只提供 ChaCha20（`--inner-suites chacha`），所以不能證明它照用戶端的順序選。2026-10-21 更正（Laysi/trustiot-v2#86，[測試報告](../test-reports/2026-10-21-86-agent-mqtt-connection.md)）：agent 依序提供 ChaCha20、AES-256-GCM、AES-128-GCM 時，Mosquitto 選 AES-256-GCM；listener 加上 `ciphers_tls1.3 TLS_CHACHA20_POLY1305_SHA256:TLS_AES_256_GCM_SHA384:TLS_AES_128_GCM_SHA256` 後選 ChaCha20。用戶端提供 X25519MLKEM768 時會選它。
- **Cloudflare（SaaS 外層，依文件，未實測）**：
  - Cloudflare 的文件說，TLS 1.3 的套件順序是 BoringSSL 寫死的。依 BoringSSL 的原始碼，有 AES 硬體時三種套件屬同一偏好組，由用戶端的順序決定。
  - Cloudflare 的部落格說，它依用戶端宣告的偏好選擇，對沒有 AES 硬體的用戶端優先選 ChaCha20。
  - 用戶端支援 X25519MLKEM768 時，Cloudflare 自動用它。
  - 即使 Cloudflare 改變偏好，閘道器也可以只提供 ChaCha20：TLS 1.3 的三種套件 Cloudflare 一律支援。

## 穩態 CPU

數字是閘道器程序占單核的百分比，provider 為 aws-lc-rs。

| 情境（每秒訊息數） | 基準 ChaCha20 | 基準 AES-128 | SaaS ChaCha20 | SaaS AES-128 | 地端 ChaCha20 | 地端 AES-128 |
| --- | --- | --- | --- | --- | --- | --- |
| 閒置（0） | 0.008 | — | 0.009 | — | 0.009 | — |
| S1 一般（1.1） | 0.016 | 0.020 | 0.019 | 0.022 | 0.020 | 0.023 |
| S2 重度（33） | 0.363 | 0.430 | 0.439 | 0.553 | 0.420 | 0.555 |
| 延遲（10） | 0.190 | 0.215 | 0.235 | 0.262 | 0.266 | 0.265 |
| 壓力（510） | 2.57 | 3.79 | 3.59 | 5.47 | 3.47 | 4.99 |
| 極限（5,000） | 17.1 | 29.6 | 23.0 | 27.5 | 26.1 | 32.0 |

| 對照（SaaS） | S2 | 壓力 | 極限 |
| --- | --- | --- | --- |
| ring，ChaCha20 | 0.419 | 3.55 | 26.6 |
| ring，AES-128 | 0.551 | 4.92 | 31.9 |
| aws-lc-rs，不設定順序（協商出 AES-256-GCM） | — | 5.70 | 32.4 |

- **雙層的額外成本**：S2 下 SaaS 比直連 mTLS 多 0.08 個百分點（每則 133 µs 對 110 µs）。在文件頻率下，雙層 TLS 不是閘道器的 CPU 限制。
- **ChaCha20 對 AES-128**：S2 省 21%，壓力情境省 34%，不設定順序（AES-256）時差距更大。
  - 極限情境的差距比 AEAD 測試小：每則訊息固定的系統呼叫、WebSocket 遮罩與排程成本占了大半。
  - 每則 1 KiB 兩層合計的加密時間，ChaCha20 約 10 µs，AES-128 約 36 µs。
- **每秒 5,000 則 1 KiB 時仍只用 23–32% 單核**：傳輸層的上限遠高於任何方案的頻率。
- **整機 4 核合計的忙碌比例**：S2 不到 0.2%，極限情境 4–10%。
- **上行的線路成本**：
  - SaaS 每則訊息比直連多約 30 B：外層 TLS 記錄 22 B，加上有遮罩的 WebSocket 標頭。
  - 閒置 5 分鐘上行共 376 B：4 次 PINGREQ，以及對轉接服務 ping 回的 pong。

## 延遲

QoS1 往返（ms）走的是區網與工作站上的伺服器，不含網際網路。

| 情境 | 基準 ChaCha20 | 基準 AES-128 | SaaS ChaCha20 | SaaS AES-128 | 地端 ChaCha20 | 地端 AES-128 |
| --- | --- | --- | --- | --- | --- | --- |
| 延遲 256 B，n≈1,200：p50 / p99 | 0.70 / 1.23 | 0.52 / 1.26 | 0.78 / 1.51 | 0.81 / 1.49 | 0.87 / 1.61 | 1.09 / 1.78 |
| S2 8 KiB 一批，n≈120：p50 / p99 | 1.18 / 1.56 | 1.49 / 1.81 | 1.48 / 2.04 | 2.16 / 2.54 | 1.60 / 2.69 | 2.39 / 2.86 |
| 閘道器端送出路徑，延遲情境：p50 / p99 | 0.043 / 0.048 | 0.055 / 0.075 | 0.059 / 0.090 | 0.083 / 0.125 | 0.071 / 0.102 | 0.083 / 0.126 |

- 雙層比直連多 0.1–0.4 ms，其中閘道器端只占 0.02–0.05 ms，其餘在轉接服務與 Envoy。
- 8 KiB 的批次，AES-128 比 ChaCha20 多約 0.7 ms：兩層各加密一次 8 KiB。
- 網路的部分沿用 2026-10-10 的實測：經網際網路到 Cloudflare 台北節點，1,000 條連線建立 p50 343 ms、p99 372 ms，比閘道器端的處理時間高兩個數量級。
- 即時圖最短 1 秒一次，閘道器端的延遲不構成限制。

## 連線建立與重連

量 30 次重連（連線、CONNACK、DISCONNECT，間隔 500 ms），兩層都用 ChaCha20。程序啟動後的第一次握手另外記錄，不列入統計。

| 路徑 | provider，key exchange | 完整握手 p50 / p99（ms） | CPU（ms） | resumption p50 / p99（ms） | CPU（ms） | 握手位元組（完整 / resumption） |
| --- | --- | --- | --- | --- | --- | --- |
| SaaS | aws-lc-rs，X25519MLKEM768（預設） | 12.6 / 14.4 | 7.6 | 7.2 / 8.0 | 3.6 | 10.1 KB / 7.8 KB |
| SaaS | aws-lc-rs，X25519 | 10.6 / 11.9 | 6.2 | 5.9 / 7.1 | 2.7 | 5.5 KB / 3.2 KB |
| SaaS | ring，X25519 | 11.8 / 12.8 | 7.0 | 6.6 / 7.8 | 3.3 | 5.5 KB / 3.2 KB |
| 地端 | aws-lc-rs，預設（外層 X25519、內層 ML-KEM） | 12.5 / 13.5 | 6.0 | 7.6 / 9.5 | 3.1 | 9.5 KB / 6.0 KB |
| 地端 | ring，X25519 | 12.2 / 13.9 | 5.9 | 7.3 / 9.7 | 3.2 | 6.0 KB / 3.7 KB |
| 基準 | ring，X25519 | 6.2 / 6.8 | 3.3 | 3.6 / 4.4 | 1.5 | 3.3 KB / 1.8 KB |

- **完整握手**：SaaS 時外層 TLS 約 4–5 ms、WebSocket upgrade 約 0.6 ms、內層 mTLS 約 4–5 ms、CONNACK 約 1.3 ms。
- **TPM 的成本**：正式產品的內層簽章在 TPM。Laysi/trustiot-v2#74 在同一台閘道器量到以下數字，所以一次完整重連在閘道器端約 80 ms。
  - TPM2\_Sign（ECDSA P-256）：p50 69.6 ms、p99 71.0 ms，n=300。
  - 用 TPM 金鑰的完整 mTLS 握手：p50 71.9 ms；軟體金鑰是 2.2 ms。
  - TLS 1.3 resumption：1.4 ms，不需要 TPM 簽章。
- **resumption 的效果**：TLS 1.3 的 resumption 不送 CertificateRequest，用戶端不簽章。所以開了 resumption，重連不需要 TPM，CPU 也減半。
  - 兩層都能 resume：Envoy、rustls 終止器與 Mosquitto 都支援 TLS 1.3 resumption。
  - 實測 30 次重連全部 resumed。
- **X25519MLKEM768**：只有 aws-lc-rs 支援。每次完整握手多約 2 ms 的時間與 1.4 ms 的 CPU，位元組約加倍。
- **冷啟動**：程序啟動後的第一次握手，aws-lc-rs 要 127–174 ms（CPU 121–169 ms），是一次性的初始化；ring 是 4–8 ms。這只影響閘道器程序啟動那一次。
- **網路的部分**：見上一節，2026-10-10 的 p50 343 ms。所以 SaaS 一次完整重連約 0.4 秒，主要是網路與 TPM，與 2026-10-10 量到的中斷恢復時間（0.4–3.5 秒）同一個量級。

## session resumption 與撤銷

在工作站上用另一個 Mosquitto（同一個映像，開 `crlfile`）測：

1. 閘道器先完整握手，取得 session ticket。
2. 把 CRL 換成撤銷這張憑證的版本，對 Mosquitto 送 SIGHUP。
3. 閘道器帶著舊 ticket 重連。結果被拒，收到 TLS alert `CertificateRevoked`。

對照組：CRL 不變、只送 SIGHUP，之後的重連都是完整握手。所以 SIGHUP 重新載入時，舊的 ticket 全部作廢，被撤銷的閘道器不能靠 resumption 繞過 CRL。決策 6「發新 CRL、SIGHUP 再踢線」的順序因此不必改。

不送 SIGHUP 時，ticket 在效期內有效（OpenSSL 預設的 session 逾時是 7200 秒，未實測）。所以只換 CRL、不送 SIGHUP 的停權一樣無效，這和現行規則一致。

## 要寫進設計的事

1. **兩層 TLS 都只開 TLS 1.3，把 ChaCha20-Poly1305 排第一**，AES-GCM 留作後備。
   - rustls 的預設順序會選 AES-256-GCM。閘道器的 Cortex-A72 沒有 AES 指令，AES-256-GCM 在壓力情境多用 59% 的 CPU。
   - Cloudflare、Envoy、Mosquitto 都依用戶端的順序選。
   - 改用有 AES 指令的硬體（例如 Raspberry Pi 5 平台）時，可以重新檢討這個順序。
2. **兩層都開 session resumption**，rustls 的預設就是開的，不要關掉。
   - 重連時不需要 TPM 簽章（約 70 ms），握手 CPU 減半。
   - 停權照決策 6 走「CRL、SIGHUP、踢線」，SIGHUP 會讓舊 ticket 失效。
3. **crypto provider**：每則訊息的成本，ring 與 aws-lc-rs 相同。
   - 只有要用 X25519MLKEM768（抗量子的 key exchange）時才需要 aws-lc-rs，代價是每次完整握手約多 1.4 ms 的 CPU、程序啟動時一次約 120 ms。
   - 目前 workspace 的 rustls 用 ring。閘道器要不要開 X25519MLKEM768 在 MS1 實作閘道器時決定，CPU 不是限制。
4. **Mosquitto 設 `set_tcp_nodelay true`**。
   - 沒設時，每次 CONNACK 多約 40 ms（工作站上實測，設了之後降到約 1 ms）。推測是 TLS 1.3 握手後的 session ticket 與 CONNACK 被 Nagle 卡住，等用戶端的 delayed ACK。
   - Mosquitto 預設不開這個選項。repo 目前沒有正式的 broker 設定，MS1 寫 chart 的 Mosquitto 設定時要加上。
5. **雙層 TLS 不是閘道器規格的限制**：CPU 與延遲都遠在可接受範圍內，不影響決策 6、27，也不影響閘道器的採購規格。記憶體另由 Laysi/trustiot-v2#76 量。

## 範圍外與後續

- **Cloudflare 的實際邊緣沒有測**：依規則不建立 Cloudflare 資源。協商行為依文件與 BoringSSL 原始碼，網路的部分沿用 2026-10-10 的實測。
- **只測 MQTT 3.1.1**：MQTT 5（2026-10-09 的建議，為了拿到 0x87）的封包大小相近，不影響結論。
- **閘道器的停機**：閘道器在 2026-10-12 約 10:12Z 到 2026-10-13 約 03:27Z 之間離線，之後斷電重開。
  - 本項的量測窗口都在這段時間之外：2026-10-12 的兩個窗口在 09:01:19Z 前結束，其餘在 2026-10-13 04:47:50Z 之後。
  - 每個窗口開始前都記錄了閘道器的負載與溫度，結果不受影響。
  - 2026-10-12 那一輪重連量測沒有排除首次初始化，已被 2026-10-13 的結果取代。資料一併保留在測試報告的 data 目錄。

## 測試環境

詳細指令與原始資料見[測試報告](../test-reports/2026-10-13-75-double-tls-gateway.md)。

- **閘道器**：lab 的閘道器，裝置樹回報的型號是 Raspberry Pi 4 Model B Rev 1.4。
  - CPU：Cortex-A72 4 核，最高 1.8 GHz，沒有 ARMv8 Crypto Extensions；記憶體 2 GB。
  - OS：Raspberry Pi OS Lite arm64（Debian 13 Trixie），kernel 6.18.50+rpt-rpi-v8，glibc 2.41。
- **閘道器替身**：
  - 編譯：Rust 1.99.0（repo 的釘選版本），以 cargo-zigbuild 0.23.4 加 Zig 0.16.0 交叉編譯成 `aarch64-unknown-linux-gnu.2.36`。
  - crate：rustls 0.23.45、tokio-rustls 0.26.6、tokio-tungstenite 0.30.0、tokio 1.53.2、ring 0.17.14、aws-lc-rs 1.18.1（aws-lc-sys 0.45.0）。
- **伺服器端**：x86\_64 Linux 工作站（8 核），以 podman 執行。
  - Mosquitto：`eclipse-mosquitto:2.1.2-alpine`（OpenSSL 3.5.8）。
  - Envoy：`istio/proxyv2:1.30.5`（Envoy 1.38.5，BoringSSL）。
  - 轉接服務與 SaaS 模型的 TLS 終止器：自寫 Rust 程式，rustls 0.23.45 加 aws-lc-rs。
- **測試程式**：原始碼放在工作站的 `~/poc-gw/75/`，閘道器上的執行檔放在 `~/poc-75/`，依 2026-10-09 的決定不進 repo。
