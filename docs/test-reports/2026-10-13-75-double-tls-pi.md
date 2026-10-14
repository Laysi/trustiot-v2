# 雙層 TLS 在 Pi 上的 CPU 與延遲（2026-10-12、2026-10-13）

對應 issue：Laysi/trustiot-v2#75。

- 結論與設計影響寫在[可行性測試結果（2026-10-13）](../feasibility/2026-10-13-double-tls-pi.md)。
- 原始資料在 [data/2026-10-13-75-double-tls-pi/](data/2026-10-13-75-double-tls-pi/)。工作站的位址在資料裡已換成 `<workstation>`，指令裡的 `$PI_HOST` 是 Pi 的 ssh 主機。

## 環境

| 項目 | 閘道器端（lab 的 Pi） | 伺服器端（工作站） |
| --- | --- | --- |
| 硬體 | Raspberry Pi 4 Model B Rev 1.4。CPU：Cortex-A72（CPU part 0xd08），4 核，600–1800 MHz，`ondemand` 調速。`Features: fp asimd evtstrm crc32 cpuid`，沒有 AES 與 PMULL。記憶體 2 GB（1844 MiB） | x86\_64 8 核（有 AES-NI） |
| OS | Raspberry Pi OS Lite arm64（Debian 13 Trixie），kernel 6.18.50+rpt-rpi-v8，glibc 2.41，OpenSSL 3.5.7（`openssl speed` 用） | Fedora 44，kernel 7.2.8，podman 5.8.7（rootless，`--network host`），Bun 1.4.2 |
| 程式 | 閘道器替身 `gw` 與 `aeadbench`。Rust 1.99.0，以 cargo-zigbuild 0.23.4 加 Zig 0.16.0 交叉編譯成 `aarch64-unknown-linux-gnu.2.36`，Release 建置。crate 見下方 | Mosquitto `docker.io/library/eclipse-mosquitto:2.1.2-alpine`（digest `sha256:12436df85d6b2b91e63c71184da35ec89400a98abdc688a78b1403a11ecf44b4`，OpenSSL 3.5.8）；Envoy 取自 `docker.io/istio/proxyv2:1.30.5`（digest `sha256:324ec9f74e9194c43b16e859497d67567c93d3d1e5f0804a88ed48002e668f62`，Envoy 1.38.5，BoringSSL）；轉接服務 `relay`（同一個 Rust 專案） |

crate 版本：rustls 0.23.45（`ring`、`aws_lc_rs`、`prefer-post-quantum`）、tokio-rustls 0.26.6、tokio-tungstenite 與 tungstenite 0.30.0、tokio 1.53.2、ring 0.17.14、aws-lc-rs 1.18.1（aws-lc-sys 0.45.0）、rcgen 0.14.10、hdrhistogram 7.6.0。

測試程式不進 repo：原始碼在工作站的 `~/poc-gw/75/`，Pi 上的執行檔在 `~/poc-75/`。

**Pi 端（`gw`）**

- 兩層都只開 TLS 1.3。
- 內層用 ECDSA P-256 的軟體金鑰做用戶端憑證，CN 是 `gw-0001`。
- 外層的 ALPN 是 `http/1.1`；WebSocket 讀緩衝 8 KiB、寫緩衝 0、寫緩衝上限 64 KiB。
- MQTT 3.1.1，clean session，keepalive 60 秒；60 秒內沒送過封包就送 PINGREQ。
- QoS1 同時只有一批在途。
- tokio 單執行緒 runtime，TCP\_NODELAY。

**伺服器端的三個入口**

| 埠 | 路徑 | 外層 TLS 憑證 | 後端 |
| --- | --- | --- | --- |
| 18750 | 基準 | 無（直連 Mosquitto 的 mTLS） | — |
| 18752 | SaaS 模型 | rustls 加 aws-lc-rs。照用戶端的套件順序選，支援 X25519MLKEM768。憑證是 ECDSA P-256 的 leaf 加中繼憑證 | 轉接服務 → Mosquitto |
| 18753 | 地端模型 | Envoy。TLS 參數全用預設，只設憑證與 `alpn_protocols`。憑證是企業 CA 簽的 RSA-2048 | 18751 的明文轉接服務 → Mosquitto |

**轉接服務與 Mosquitto**

- 轉接服務每 50 秒送一次 WebSocket ping，WebSocket 緩衝區設定與用戶端相同。
- Mosquitto 只開 mTLS（`require_certificate true`、`use_identity_as_username true`），另外設了 `set_tcp_nodelay true` 與 `persistence false`。
- 量測時沒有訂閱者，訊息由 broker 直接丟棄。這不影響 Pi 端的成本。

測試用的 CA、伺服器憑證與閘道器憑證，都由 `rcgen` 在工作站上產生（`~/poc-gw/75/pki/`），沒有進 repo。

**Pi 的鎖與狀態**

每個量測窗口都持有 Pi 上的 `~/e2e.lock`，窗口結束立刻釋放。窗口開始時，以及每次量測前，都記錄了 Pi 的狀態：

- 負載、溫度與 `vcgencmd get_throttled`
- CPU 用量最高的 5 個程序

2026-10-13 各窗口中，量測前的 Pi load1 不超過 0.29，溫度 46–50 °C，`throttled=0x0`；工作站的 load1 在 0.05–1.54 之間。

| 窗口（UTC） | 內容 |
| --- | --- |
| 2026-10-12 08:50:23–08:50:47 | 煙霧測試（不列入結果） |
| 2026-10-12 08:51:54–08:53:01 | AEAD 與 `openssl speed` |
| 2026-10-12 08:55:02–09:01:19 | 重連成本，沒排除首次初始化（`reconnect-nowarmup.jsonl`，已被下一輪取代） |
| 2026-10-13 04:47:50–04:54:15 | 重連成本（`reconnect.jsonl`） |
| 2026-10-13 04:56:15–05:20:10 | aws-lc-rs，ChaCha20 的各情境 |
| 2026-10-13 05:22:11–05:46:07 | aws-lc-rs，AES-128 的各情境 |
| 2026-10-13 05:48:08–06:15:51 | ring 的對照，以及不設套件順序的對照 |
| 2026-10-13 06:17:52–06:33:09 | 閒置，每條路徑 5 分鐘 |

各窗口的取得與釋放時間、事前檢查的輸出，都在 [windows.txt](data/2026-10-13-75-double-tls-pi/windows.txt)。

**Pi 的停機**

- 2026-10-12 約 10:12Z，工作站換了網路，Pi 也隨即離線。Pi 在 2026-10-13 約 03:27Z 斷電重開後恢復。
- 這段期間本 issue 沒有持有 lock，也沒有量測。
- 2026-10-12 的窗口都在 09:01:19Z 前結束；2026-10-13 的窗口都在重開之後，而且開始前都確認了 Pi 閒置。所以停機不影響已記錄的結果。

## 指令

祕密只有測試用的私鑰，放在 `~/poc-gw/75/pki/` 與 Pi 的 `~/poc-75/pki/`，不進 repo。

```sh
# 工作站：建置（Rust 版本跟隨 repo 的 rust-toolchain.toml）、產生測試 PKI
cargo build --release --manifest-path ~/poc-gw/75/Cargo.toml
(cd ~/poc-gw/75 && ./target/release/pki)
CARGO_ZIGBUILD_ZIG_PATH=~/.proto/tools/zig/0.16.0/zig ~/.proto/tools/cargo-zigbuild/0.23.4/cargo-zigbuild \
  zigbuild --release --manifest-path ~/poc-gw/75/Cargo.toml --target aarch64-unknown-linux-gnu.2.36 --bin gw --bin aeadbench
# 複製到 Pi：PI_HOST 是 Pi 的 ssh 主機，沒有設定時停在這一行
: "${PI_HOST:?set PI_HOST to the ssh host of the lab Pi}"
scp target/aarch64-unknown-linux-gnu/release/{gw,aeadbench} "$PI_HOST:poc-75/"
scp pki/{outer-roots.pem,inner-ca.pem,gw.pem,gw.key} "$PI_HOST:poc-75/pki/"

# 工作站：伺服器端（srv/ 內是 mosquitto.conf、envoy.yaml 與伺服器憑證）
podman run -d --name poc75-mosquitto --network host -v ~/poc-gw/75/srv:/srv:ro,Z \
  docker.io/library/eclipse-mosquitto:2.1.2-alpine mosquitto -c /srv/mosquitto.conf
podman run -d --name poc75-envoy --network host -v ~/poc-gw/75/srv:/srv:ro,Z \
  --entrypoint /usr/local/bin/envoy docker.io/istio/proxyv2:1.30.5 -c /srv/envoy.yaml --concurrency 2 -l warning
./target/release/relay --listen 127.0.0.1:18751 --upstream 127.0.0.1:18750
./target/release/relay --listen 0.0.0.0:18752 --upstream 127.0.0.1:18750 \
  --tls-cert pki/saas-chain.pem --tls-key pki/saas.key

# 工作站：依序取得 Pi 的 lock、跑各組量測、釋放（每組一個窗口），再彙整成表格與去識別化的資料
bun ~/poc-gw/75/run.ts aead reconnect awslc-chacha awslc-aes128 other idle
bun ~/poc-gw/75/analyze.ts docs/test-reports/data/2026-10-13-75-double-tls-pi
```

`run.ts` 對每次量測在 Pi 上執行的指令如下（`<path>` 與 `<scenario>` 見下表）：

```sh
cd ~/poc-75 && ./gw <path> --provider aws-lc-rs|ring \
  [--outer-suites chacha --inner-suites chacha | --outer-suites aes128 --inner-suites aes128] <scenario>
```

| 路徑 | 參數 |
| --- | --- |
| 基準 | `--mode mtls --addr <workstation>:18750` |
| SaaS | `--mode wss --addr <workstation>:18752 --outer-sni mqtt.saas.test` |
| 地端 | `--mode wss --addr <workstation>:18753 --outer-sni mqtt.onprem.test` |

| 情境 | 參數 |
| --- | --- |
| idle | `--duration 300` |
| s1 | `--duration 120 --batch-ms 10000 --batch-bytes 2048 --live-rate 1 --live-bytes 200` |
| s2 | `--duration 120 --batch-ms 1000 --batch-bytes 8192 --live-rate 32 --live-bytes 200` |
| lat | `--duration 120 --batch-ms 100 --batch-bytes 256` |
| stress | `--duration 60 --batch-ms 100 --batch-bytes 16384 --live-rate 500 --live-bytes 200` |
| max | `--duration 30 --live-rate 5000 --live-bytes 1024` |
| 重連 | `--reconnects 30 --reconnect-gap-ms 500 --resumption true|false [--outer-groups x25519 --inner-groups x25519]` |

重連量測前，先連一次暖機，記為 `cold`，不列入統計。

resumption 與撤銷的檢查在工作站上跑，用另一個 Mosquitto：同一個映像，listener 是 18754，加 `crlfile`。

```sh
./target/release/revtest      # 產生 pki-rev/：CA、broker、gw-rev（serial 200）、空的 CRL、撤銷 serial 200 的 CRL
./target/release/gw --mode mtls --addr 127.0.0.1:18754 --inner-ca pki-rev/ca.pem --cert pki-rev/gw.pem \
  --key pki-rev/gw.key --client-id gw-rev --inner-suites chacha --reconnects 2 --reconnect-gap-ms 15000 &
# 暖機連線完成後的等待期間：換成撤銷的 CRL 並重新載入
cp pki-rev/crl-revoked.pem srv-rev/crl.pem && podman kill --signal HUP poc75-mosq-rev
# 對照組：CRL 不變，只送 SIGHUP；以及不送 SIGHUP、間隔 15 秒與 0.5 秒的重連
```

## 結果

### AEAD

數字取自 [aead.json](data/2026-10-13-75-double-tls-pi/aead.json) 與 [openssl-speed.txt](data/2026-10-13-75-double-tls-pi/openssl-speed.txt)。每次加密的時間（µs），ring 與 aws-lc-rs 的差距不到 2%，下表取 ring：

| 大小 | AES-128-GCM | AES-256-GCM | ChaCha20-Poly1305 |
| --- | --- | --- | --- |
| 64 B | 1.59 | 2.00 | 0.82 |
| 256 B | 4.87 | 6.17 | 1.33 |
| 1 KiB | 17.9 | 22.7 | 4.76 |
| 16 KiB | 279 | 353 | 64 |

`openssl speed` 在 16 KiB 時：AES-128-GCM 70.1 MB/s，AES-256-GCM 59.1 MB/s，ChaCha20-Poly1305 321 MB/s。

### 穩態

逐筆的資料在 [awslc-chacha.jsonl](data/2026-10-13-75-double-tls-pi/awslc-chacha.jsonl)、[awslc-aes128.jsonl](data/2026-10-13-75-double-tls-pi/awslc-aes128.jsonl)、[other.jsonl](data/2026-10-13-75-double-tls-pi/other.jsonl) 與 [idle.jsonl](data/2026-10-13-75-double-tls-pi/idle.jsonl)。每筆記錄包含：

- 協商結果：兩層的套件與 key exchange
- 程序的 user 與 sys CPU 秒數、整機忙碌比例
- 上行與下行的位元組數
- QoS1 往返與送出路徑的 p50、p90、p99、最大值
- 送出的筆數，以及 Pi 與工作站的負載

每秒訊息數取自實際送出的筆數，各路徑相同：S1 1.1、S2 33.0、延遲 10.0、壓力 510、極限約 4,998。下表的 CPU 是閘道器程序占單核的百分比，provider 為 aws-lc-rs；RTT 是 QoS1 往返的 p50 與 p99（ms）。

| 情境 | 路徑 | ChaCha20 CPU | ChaCha20 RTT | AES-128 CPU | AES-128 RTT |
| --- | --- | --- | --- | --- | --- |
| S1 | 基準 | 0.016 | 0.85 / 1.52 | 0.020 | 0.84 / 1.48 |
| S1 | SaaS | 0.019 | 0.92 / 1.50 | 0.022 | 1.20 / 1.70 |
| S1 | 地端 | 0.020 | 1.11 / 1.55 | 0.023 | 1.71 / 1.99 |
| S2 | 基準 | 0.363 | 1.18 / 1.56 | 0.430 | 1.49 / 1.81 |
| S2 | SaaS | 0.439 | 1.48 / 2.04 | 0.553 | 2.16 / 2.54 |
| S2 | 地端 | 0.420 | 1.60 / 2.69 | 0.555 | 2.39 / 2.86 |
| 延遲 | 基準 | 0.190 | 0.70 / 1.23 | 0.215 | 0.52 / 1.26 |
| 延遲 | SaaS | 0.235 | 0.78 / 1.51 | 0.262 | 0.81 / 1.49 |
| 延遲 | 地端 | 0.266 | 0.87 / 1.61 | 0.265 | 1.09 / 1.78 |
| 壓力 | 基準 | 2.57 | 2.00 / 2.33 | 3.79 | 2.61 / 3.00 |
| 壓力 | SaaS | 3.59 | 2.42 / 2.73 | 5.47 | 3.44 / 3.92 |
| 壓力 | 地端 | 3.47 | 2.57 / 3.25 | 4.99 | 3.63 / 4.14 |
| 極限 | 基準 | 17.1 | — | 29.6 | — |
| 極限 | SaaS | 23.0 | — | 27.5 | — |
| 極限 | 地端 | 26.1 | — | 32.0 | — |

- **閒置 5 分鐘**：CPU 是單核的 0.008%（基準）與 0.009%（SaaS、地端）。三條路徑都送了 4 次 PINGREQ，收到 4 次 PINGRESP；上行 96 B（基準）與 376 B（雙層）。
- **ring 的對照（SaaS）**：
  - S2：ChaCha20 0.419%，AES-128 0.551%
  - 壓力：ChaCha20 3.55%，AES-128 4.92%
  - 極限：ChaCha20 26.6%，AES-128 31.9%
- **不設套件順序的對照**（aws-lc-rs 預設，兩層協商出 AES-256-GCM）：
  - 壓力：基準 4.12%、SaaS 5.70%、地端 5.67%
  - 極限：基準 30.7%、SaaS 32.4%、地端 37.6%
- **Pi 端送出路徑**：延遲情境的 p50 / p99（ms）如下。
  - ChaCha20：基準 0.043 / 0.048，SaaS 0.059 / 0.090，地端 0.071 / 0.102
  - AES-128：基準 0.055 / 0.075，SaaS 0.083 / 0.125，地端 0.083 / 0.126

### 重連

逐筆的資料在 [reconnect.jsonl](data/2026-10-13-75-double-tls-pi/reconnect.jsonl)，兩層都用 ChaCha20，n=30。完整握手與 resumption 都列 p50 / p99（ms）與每次的 CPU（ms）。

| 路徑 | provider，key exchange | 完整握手 | 完整 CPU | resumption | resumption CPU | 冷啟動（ms / CPU ms） |
| --- | --- | --- | --- | --- | --- | --- |
| 基準 | aws-lc-rs，預設（ML-KEM） | 6.95 / 7.66 | 3.48 | 4.13 / 5.32 | 1.74 | 141 / 121 |
| 基準 | aws-lc-rs，X25519 | 5.88 / 6.82 | 2.90 | 3.11 / 4.54 | 1.27 | 158 / 156 |
| 基準 | ring，X25519 | 6.16 / 6.83 | 3.26 | 3.59 / 4.37 | 1.54 | 4.2 / 1.8 |
| SaaS | aws-lc-rs，預設（ML-KEM） | 12.62 / 14.37 | 7.62 | 7.23 / 8.00 | 3.59 | 127 / 123 |
| SaaS | aws-lc-rs，X25519 | 10.60 / 11.94 | 6.19 | 5.93 / 7.10 | 2.69 | 174 / 169 |
| SaaS | ring，X25519 | 11.81 / 12.80 | 7.01 | 6.58 / 7.75 | 3.27 | 7.7 / 3.6 |
| 地端 | aws-lc-rs，預設（外層 X25519，內層 ML-KEM） | 12.51 / 13.49 | 5.95 | 7.59 / 9.52 | 3.05 | 128 / 123 |
| 地端 | aws-lc-rs，X25519 | 10.95 / 12.58 | 5.06 | 6.33 / 8.10 | 2.63 | 127 / 123 |
| 地端 | ring，X25519 | 12.21 / 13.85 | 5.93 | 7.30 / 9.70 | 3.21 | 7.9 / 3.1 |

- 開 resumption 時，30 次全部是 resumed，兩層都是。
- 各階段的 p50（SaaS，aws-lc-rs，ML-KEM，完整握手）：
  - TCP：0.39 ms
  - 外層 TLS：5.02 ms
  - WebSocket：0.61 ms
  - 內層 TLS：5.28 ms
  - CONNACK：1.29 ms

### 協商結果

各組合實際協商出的結果記在每筆資料的 `handshake.outer`、`handshake.inner` 與 `first`。

| 用戶端提供的順序 | SaaS 模型（rustls） | Envoy | Mosquitto |
| --- | --- | --- | --- |
| 預設（AES-256、AES-128、ChaCha20；ML-KEM、X25519…） | AES-256-GCM，X25519MLKEM768 | AES-256-GCM，X25519 | AES-256-GCM，X25519MLKEM768 |
| 只給 ChaCha20 | ChaCha20 | ChaCha20 | ChaCha20 |
| ChaCha20、AES-128（2026-10-12 工作站上的煙霧測試，用戶端為 x86\_64） | ChaCha20 | ChaCha20 | — |

### resumption 與撤銷

- 換成撤銷的 CRL 並送 SIGHUP 後，閘道器帶舊 ticket 重連：連線被拒，收到 `AlertReceived(CertificateRevoked)`。Mosquitto 的紀錄是 `certificate verify failed`。證據在 [revtest-revoked-client.txt](data/2026-10-13-75-double-tls-pi/revtest-revoked-client.txt) 與 [revtest-mosquitto-control.log](data/2026-10-13-75-double-tls-pi/revtest-mosquitto-control.log)。
- 對照組，CRL 不變、只送 SIGHUP：之後兩次重連都是完整握手，見 [revtest-control-client.json](data/2026-10-13-75-double-tls-pi/revtest-control-client.json)。
- 對照組，不送 SIGHUP：重連會 resume。
- 結論：SIGHUP 讓舊的 ticket 全部作廢。

### 其他觀察

- **Mosquitto 沒設 `set_tcp_nodelay true` 時**：工作站上的 CONNACK 每次約 41–43 ms，設了之後約 1 ms。這來自工作站上的煙霧測試，沒有另存資料。
- **2026-10-12 那一輪重連**（`reconnect-nowarmup.jsonl`）：aws-lc-rs 的 p99 是 126–176 ms，正是每支程序第一次握手的初始化，所以 2026-10-13 改成先暖機。

## 結論

逐條對照驗收條件：

- [x] **結果檔記錄 CPU 使用率、p50 與 p99 延遲、測試頻率與硬體版本**：通過，見[可行性測試結果（2026-10-13）](../feasibility/2026-10-13-double-tls-pi.md)的「穩態 CPU」「延遲」「連線建立與重連」「頻率假設」「測試環境」各節，以及本報告。
- [x] **勾選 docs/roadmap/milestones.md 的雙層 TLS 項目**：已勾選，並連到結果檔。
- [x] **若 CPU 或延遲超出可接受範圍，在結果檔寫出對閘道器設計的影響**：沒有超出。
  - 可接受範圍的定義：S2 不超過單核的 5%；Pi 端每則的延遲 p99 不超過 10 ms；重連成本遠小於 0.4–3.5 秒。
  - 實測：S2 最多 0.56%；送出路徑 p99 最多 0.97 ms，在 S2 與 AES-128 時；完整握手 14 ms，加 TPM 約 70 ms。
  - 結果檔另外寫了設定上的建議：套件順序、resumption、crypto provider 與 Mosquitto 的 nodelay。

待測：Cloudflare 的實際邊緣。依規則不建立 Cloudflare 資源，協商行為依文件與 BoringSSL 原始碼判斷，網路的部分沿用 2026-10-10 的實測。
