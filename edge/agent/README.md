# agent

閘道器 agent（[決策 10](../../docs/architecture/decisions.md)）：在現場閘道器上讀取設備、把資料送上雲端的程式。目前有執行檔 `trustiot-agent`、systemd unit、到 broker 的連線（443 的 WebSocket 內走 mTLS 與 MQTT 5）、TPM 裡的身分金鑰（`tpm` 模組，[決策 12](../../docs/architecture/decisions.md)）、在線狀態與基本心跳；以及 Modbus 讀取（`modbus` 模組，[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) F1、F2、F5）：Modbus TCP 與 RS485 轉換器的 RTU over TCP，同一個端點的設備共用一條連線；以及每台設備的輪詢排程（`polling` 模組，F4）：固定節拍、逾時、重連退避、TCP keepalive 與每台設備的 watchdog（關掉不再回答的設備的連線，與重啟整個服務的 systemd watchdog 是兩回事）；以及設定下發的閘道器端（`desired` 模組，E1）：接收雲端的 desired 設定、驗證後原子套用（每台設備一個輪詢工作）、存到資料分割區、回報結果；以及遙測的上傳與離線緩衝（`upload`、`buffer`、`clock`、`dispatch` 模組，G1）：組批、記憶體與磁碟的緩衝、等雲端的端對端確認、時鐘是否已校正；以及即時值的閘道器端（`live` 模組，I3）：雲端開的租約期間，租約涵蓋的設備以租約的間隔取樣，每次取樣立即送到 `up/live`，不進緩衝、不等確認。執行檔把它們接在一起：套用的設定決定輪詢哪些設備，每次輪詢的讀值進入上傳的收集視窗，形成批次送上雲端，等雲端確認才從緩衝拿掉（見「遙測上傳與離線緩衝」）；租約涵蓋的設備的讀值同時送到 `up/live`（見「即時值」）。

| 在哪裡 | 內容 | issue |
| --- | --- | --- |
| `src/main.rs` | 執行檔 `trustiot-agent`：參數、日誌、兩個 runtime、sd_notify、SIGTERM | Laysi/trustiot-v2#86 |
| `src/config.rs`、`agent.example.toml` | 設定檔（TOML）與它的檢查 | Laysi/trustiot-v2#86 |
| `src/setup.rs`、`src/identity.rs` | 讀設定、憑證與金鑰，閘道器 ID 取自憑證的 CN | Laysi/trustiot-v2#86 |
| `src/tpm.rs`、`src/tpm/` | `TpmKey`：TPM 內的身分金鑰（TPM2_ReadPublic、TPM2_Sign）與 rustls 的 `SigningKey` | Laysi/trustiot-v2#86 |
| `src/tpm/provision.rs` | `ensure_key`：產線工具佈建身分金鑰（沿用、建立或拒絕） | Laysi/trustiot-v2#611 |
| `src/tpm/endorsement.rs` | `endorsement_key`：ECC P-256 的背書金鑰（EK）與 NV 裡 EK 憑證的比對 | Laysi/trustiot-v2#612 |
| `src/provision.rs` | `trustiot-agent provision` 子命令：金鑰、CSR 與 EK 的 JSON 報告，給產線工作站（見「出廠佈建」） | Laysi/trustiot-v2#613 |
| `src/provision/install.rs` | `trustiot-agent provision install` 子命令：檢查並安裝登錄回來的憑證鏈與根憑證（見「安裝憑證鏈與根憑證」） | Laysi/trustiot-v2#617 |
| `src/tls.rs`、`src/transport.rs` | 兩層 TLS 的設定；TCP → 外層 TLS → WebSocket → 內層 mTLS | Laysi/trustiot-v2#86 |
| `src/mqtt/packet.rs`、`src/mqtt/connection.rs` | MQTT 5 編解碼（只含用到的部分）與一條連線的狀態機 | Laysi/trustiot-v2#86 |
| `src/link.rs`、`src/backoff.rs` | 對外的 `Link`（publish、收訊息、狀態）與重連退避 | Laysi/trustiot-v2#86 |
| `src/heartbeat.rs`、`src/sdnotify.rs`、`src/liveness.rs` | 基本心跳；systemd 的 `READY=1`、`WATCHDOG=1`、`STATUS=`；連線的工作是否卡住（給 systemd watchdog 判斷） | Laysi/trustiot-v2#86 |
| `systemd/` | `trustiot-agent.service` 與帳號（sysusers.d） | Laysi/trustiot-v2#86 |
| `systemd/trustiot-agent-egress.nft`、`systemd/trustiot-agent-egress.service` | agent 帳號的出口規則（nftables）與在 agent 之前載入它的 unit（見「systemd」「連線位址的限制」） | Laysi/trustiot-v2#813 |
| `scripts/check-gateway.sh` | 閘道器上的檢查：本機服務只聽 loopback、出口規則已載入、DNS 伺服器不是規則拒絕的位址（見「連線位址的限制」） | Laysi/trustiot-v2#813 |
| `src/modbus.rs` | `DeviceReader`：依型號規劃讀取、送出、解碼並蓋時間戳；`Connection`、`Clock` 介面 | Laysi/trustiot-v2#87 |
| `src/modbus/plan.rs` | 合併讀取的規劃 | Laysi/trustiot-v2#87 |
| `src/modbus/pdu.rs` | 功能碼 1–4 的請求與回應 PDU | Laysi/trustiot-v2#87 |
| `src/modbus/decode.rs` | 型別、位元組順序、scale 與 offset | Laysi/trustiot-v2#87 |
| `src/modbus/tcp.rs` | 兩種傳輸方式共用的 `Connection`（`TcpConnection`）：連線管理、送出、接收到找到回應或逾時 | Laysi/trustiot-v2#87、Laysi/trustiot-v2#88 |
| `src/modbus/address.rs` | 連線位址的限制：主機只解析一次，只連檢查過的位址（見「連線位址的限制」） | Laysi/trustiot-v2#653 |
| `src/allowlist.rs`、`src/bin/trustiot-agent-allowlist.rs`、`systemd/trustiot-agent-allowlist.socket`、`systemd/trustiot-agent-allowlist@.service` | fieldbus 放行集合：清單的格式（agent 與元件共用）、以 `CAP_NET_ADMIN` 更新 nftables 集合的元件與它的 socket、unit（見「fieldbus 放行集合」） | Laysi/trustiot-v2#864 |
| `src/modbus/mbap.rs` | Modbus TCP 的框架（MBAP）：交易 ID、哪個回應算數 | Laysi/trustiot-v2#87、Laysi/trustiot-v2#88 |
| `src/modbus/rtu.rs` | RTU over TCP 的框架：CRC、由功能碼與 byte 數切框、重新同步、哪個回應算數 | Laysi/trustiot-v2#88 |
| `src/modbus/endpoint.rs` | `Endpoints`、`SharedConnection`：同一個端點的設備共用一條連線、逐個請求輪流 | Laysi/trustiot-v2#88 |
| `src/polling.rs` | `run`：每台設備的輪詢迴圈，固定節拍、重連退避、每台設備的 watchdog；`Settings`、`Disconnect` | Laysi/trustiot-v2#89 |
| `src/desired.rs` | `Applier`：依版本規則處理收到的設定、套用、存檔與回報；`run`：接收設定與送出回報的工作；`Inbox`：只留最新一則 | Laysi/trustiot-v2#94 |
| `src/desired/fleet.rs` | `Fleet`：每台設備一個輪詢工作，套用新設定時只動有變的設備；`Plan`：套用前先規劃所有設備；`Sink`：輪詢結果交給誰 | Laysi/trustiot-v2#94 |
| `src/desired/store.rs` | `Store`：已套用的設定存在資料分割區，原子替換 | Laysi/trustiot-v2#94 |
| `src/renewal.rs` | `Renewal`：用戶端憑證的換發排程、重試、檢查、原子寫檔與換上新憑證（見「憑證換發」） | Laysi/trustiot-v2#83 |
| `src/renewal/https.rs` | `HttpsEndpoint`：對 `cloud.url` 的主機送換發請求（HTTP/1.1，外層 TLS）、`Retry-After` | Laysi/trustiot-v2#83 |
| `src/upload.rs` | 收集視窗與逐系列組批、串流與序號、一次一批的上傳與 `TelemetryAck` 的處理；給輪詢的 `Readings`（也是 `Fleet` 的 `Sink`）、給分派的 `Acks`、給心跳的 `Stats` | Laysi/trustiot-v2#90 |
| `src/buffer.rs`、`src/buffer/disk.rs` | 離線緩衝：記憶體與磁碟的批次排成一個 FIFO、上限與丟棄；磁碟上的 segment 檔與啟動時的復原 | Laysi/trustiot-v2#90 |
| `src/clock.rs` | `GatewayClock`：讀值的時間戳與時鐘是否已校正（systemd-timesyncd、雲端確認的時間）；`AgeClock`：批次的年齡 | Laysi/trustiot-v2#90 |
| `src/dispatch.rs` | link 的訂閱與收到的 `down` 訊息的分派：`config` 給設定套用、`telemetry-ack` 給上傳、`live-control` 給即時值的租約 | Laysi/trustiot-v2#90、Laysi/trustiot-v2#121 |
| `src/live.rs` | 即時值：`Leases`（租約）、`Watcher` 與 `DeviceLive`（輪詢看到的租約）、`LiveSink`（租約涵蓋的輪詢交給 `Publisher`，每次輪詢照常交給上傳）、`Publisher`（發布到 `up/live`）；見「即時值」 | Laysi/trustiot-v2#121 |
| 還沒有 | 讀取統計（`DeviceStats`）與心跳的其餘指標（CPU、磁碟、溫度） | Laysi/trustiot-v2#117 |

依賴都要能建置成 aarch64（`aarch64-unknown-linux-gnu`，glibc，不是 musl：[記憶體測試](../../docs/feasibility/2026-10-13-gateway-memory.md)），CI 對 `edge/` 下每個成員做這項建置。唯一編譯 C 的相依是 `ring`（rustls 的加密實作），交叉建置時要有目標的 C 編譯器，用 `.prototools` 鎖定的 zig 與 cargo-zigbuild：

```sh
proto install zig && proto install cargo-zigbuild
CARGO_ZIGBUILD_ZIG_PATH="$(proto bin zig --reporter text | tail -n 1)" \
  PATH="$(dirname "$(proto bin cargo-zigbuild --reporter text | tail -n 1)"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-gnu -p agent
```

`tests/dependencies.rs` 確認閘道器建置裡沒有公開根憑證庫（`webpki-roots`、`rustls-native-certs`）、OpenSSL、aws-lc 與 TPM 的 C 函式庫，而且只有 `ring` 編譯原生程式碼。

## 執行檔與設定

```text
trustiot-agent [--config <file>] [--check]
trustiot-agent provision --hardware-id <id> [--handle <hex>] [--device <path>] [--inspect]
trustiot-agent provision install --hardware-id <id>
```

後兩種是產線工作站用的子命令，不啟動服務，見「出廠佈建」與「安裝憑證鏈與根憑證」。

設定檔預設是 `/etc/trustiot/agent.toml`，格式與每個鍵的說明見 [`agent.example.toml`](agent.example.toml)。未知的鍵、相對路徑、超出範圍的值都拒絕啟動，錯誤訊息指出是哪個鍵：

| 鍵 | 內容 |
| --- | --- |
| `cloud.url` | 雲端的閘道器入口：`wss://<我們的網域>[:port]/mqtt`。只接受 `wss`、DNS 名稱（不接受 IP，也不接受最後一段是數字的名稱，例如 `127.1`、`0x7f000001`：解析器會把它們當成 IPv4 位址）、路徑 `/mqtt`，不能帶查詢字串與帳密。寫了 port 就必須是 1–65535 的數字（`:`、`:0`、`:65536`、`:abc`、`:+443` 都拒絕），沒寫才是 443 |
| `cloud.trust` | 外層 TLS 信任的 PEM 檔，至少一個，每個檔至少要有一張可用的憑證。SaaS 用作業系統的 CA bundle；地端用客戶的 CA 或我們的 CA；工廠防火牆會解開 443 時，把它的攔截 CA 加成另一個檔 |
| `broker.ca` | 內層 mTLS 的信任錨：我們的根 CA 憑證（PEM）。只能放根 CA（主體與簽發者相同、以自己的金鑰簽章、basicConstraints 的 cA），可以放多張（之後換根用），但主體（subject）不能重複；放了簽發 CA 或 leaf、或兩張根同名，就拒絕啟動，錯誤訊息指出第幾張。內層只信任這些根，不用作業系統或 WebPKI 的根，也不用 `cloud.trust`。broker 的名稱固定是 `mqtt-broker.trustiot.internal`（Laysi/trustiot-v2#557），沒有設定的鍵：舊設定檔的 `broker.name` 是未知的鍵，會拒絕啟動，刪掉那一行即可。`cloud.url` 的主機不可以是這個名稱：兩層驗證的是不同的伺服器。規則見下方「內層 TLS 對 broker 的信任」 |
| `broker.crl` | 選填：根 CRL 的檔案（範例是 `/var/lib/trustiot-agent/root-crl.pem`）。一個 PEM 區塊 `X509 CRL`、最多 64 KiB、不帶 issuing distribution point，要由 `broker.ca` 的根簽章，啟動時就驗證。檔案不存在表示還沒收到根 CRL：照常連線、不檢查撤銷，啟動時記一筆 INFO，之後向平台要；但旁邊的 `<路徑>.highest` 記著曾接受過根 CRL 時，不存在或較舊的檔案會扣住連線，直到取得新的（見「根 CRL 的更新」）。檔案在但不能用（不是一個 PEM CRL、太大、不是根簽的）就拒絕啟動。agent 會寫這個檔案與 `.highest`，所以它的目錄要是 agent 寫得到的（範例的 `StateDirectory`）；寫不到時啟動記一筆 WARN、`--check` 失敗（見「根 CRL 的更新」的「存下」）。沒有這個鍵時也不檢查撤銷、不取得 |
| `identity.certificate` | 閘道器的憑證鏈（leaf 在前，接著簽發 CA）。閘道器 ID 取自 leaf 的 CN（必須是合法的 `GatewayId`），不另外設定，兩者不可能不一致 |
| `identity.key` | 正式環境是 `{ source = "tpm", device = "/dev/tpmrm0", handle = 0x81000100 }`（handle 要在 persistent 範圍 0x81000000–0x81FFFFFF）；`{ source = "file", path = "..." }` 只給開發與測試，啟動時會記一筆警告 |
| `connection.*` | 心跳間隔（10–3600 秒，預設 60）、重連退避的起始與上限（預設 1 秒、60 秒）、同時等 PUBACK 的 QoS 1 訊息數（1–256，預設 32） |
| `renewal.*` | 選填，憑證換發（見「憑證換發」）：剩下多少效期時換發 `remaining_percent`（10–80，預設 33）、失敗後重試的退避起始 `retry_base_s`（1–3600 秒，預設 30）與上限 `retry_cap_s`（起始到 86400 秒，預設 1800） |
| `buffer.*` | 離線緩衝：記憶體上限（`memory_limit_mib`，0–64，預設 16）、磁碟上限（`disk_limit_mib`，0–65536，預設 1024）、上傳間隔（`upload_interval_s`，1–60，預設 10）。緩衝的檔案在狀態目錄的 `buffer/`，狀態目錄由 systemd 給，不在這個檔案裡。見下方「遙測上傳與離線緩衝」 |

啟動時讀設定、確認 `broker.ca` 只有根 CA、載入根 CRL（記下 CRL 編號與 nextUpdate；已過期時另記一筆警告）、建好兩層 TLS 的設定、讀憑證並確認金鑰與憑證的公鑰相同（TPM 的金鑰也一樣）；有 EKU 時必須含 clientAuth，有 key usage 時必須含 digitalSignature。接著以 webpki 的用戶端憑證檢查（rustls 的 `WebPkiClientVerifier`）確認憑證鏈接到 `broker.ca`，broker 一定會拒絕的憑證（例如別的 CA 簽的）就不啟動。只有時間不對（過期或還沒生效）時記一筆警告，寫出憑證的 notBefore 與 notAfter，照樣啟動：閘道器有硬體 RTC，但時間仍可能是錯的（例如 RTC 沒設定過或遺失了時間），效期以 broker 的時鐘為準，拒絕啟動只會把時鐘的問題變成停機；真的過期時 broker 會拒絕，由換發處理（見「憑證換發」）。全部通過才通知 systemd `READY=1`，不等第一次連線：沒有網路的現場也要能啟動，之後自己重試。`--check` 做完這些檢查就結束（0 是可用並印出閘道器 ID 與憑證的到期時間，1 是不可用並印出原因），給維運與更新後的健康檢查用。設備的設定（`DesiredConfig`）由 MQTT 送來（見「設定下發與套用」），不在這個檔案裡。

日誌寫到 stderr，由 systemd 交給 journal；等級由 `TRUSTIOT_AGENT_LOG` 設定（預設 `info`）。

執行緒：連線（TLS 握手與 MQTT）在自己的執行緒、單執行緒的 tokio runtime 上跑，因為完整的內層握手要在 rustls 的同步簽章呼叫裡等 TPM 約 70 ms（[TPM 測試](../../docs/feasibility/2026-10-12-tpm-poc.md)），這樣只卡住這條執行緒；心跳、systemd watchdog 的通知、訊號、`down` 訊息的分派、設定的套用、Modbus 輪詢（每台設備一個工作）與遙測的上傳在另一個兩條執行緒的 runtime 上；緩衝的磁碟 I/O 在 tokio 的 blocking 執行緒上。

### 沒有完成出廠登錄或沒有有效憑證就不運作（fail-closed）

舊系統的 agent 沒完成註冊也照常運作（Laysi/trustiot#119）；這個 agent 反過來（rebuild checklist D5，Laysi/trustiot-v2#812）。下列任何一項不成立，agent 在通知 systemd 之前就以結束碼 1 結束，日誌寫 `refusing to start` 與原因（讀不到的檔案，或出問題的鍵）；它不送 `READY=1`（也不送任何其他通知）、不開任何連線、不讀狀態目錄裡上次套用的設備設定（`desired-config.cbor`）、不輪詢任何設備、不開離線緩衝：

- 設定檔存在、每個鍵都合法（沒有設定檔就是還沒完成出廠登錄）；
- `identity.certificate` 的憑證鏈與 `identity.key` 的私鑰都在，私鑰與憑證的公鑰相同（TPM 的金鑰還要能以空密碼簽出測試簽章，見「TPM 簽章」）；
- 憑證鏈以 webpki 的用戶端憑證檢查驗得到 `broker.ca` 的根，用途（EKU、key usage）也是 broker 會接受的；
- `broker.ca` 只有根 CA，`broker.crl` 在的話要由這些根簽章。

這些檢查都在讀狀態目錄、建立 runtime 與 systemd 的通知之前（`src/setup.rs` 的 `Setup::new`，`src/main.rs`），所以已經套用過設備設定的閘道器，憑證或私鑰被拿掉、換掉之後，下一次啟動就不會輪詢設備或連線。執行中的 agent 不會重讀這些檔案：它繼續用記憶體裡的身分，直到重啟或 broker 拒絕憑證（拒絕時走換發）。systemd 依 `Restart=always` 在一分鐘內重啟再試（見「systemd」），出廠登錄補齊（`provision install`）之後下一次啟動就正常運作。

**例外：只有時間不對的憑證照樣啟動。** 憑證已過期或還沒生效時（以閘道器的時鐘判斷），只記一筆警告、寫出 notBefore 與 notAfter，照常連線。這是刻意的：閘道器的時鐘可能是錯的，拒絕啟動會把時鐘的問題變成停機；而真的過期時 broker 會拒絕這張憑證，agent 收到拒絕就走 `/certs/renew` 換發（Laysi/trustiot-v2#83，見「憑證換發」），換發也以同一把 TPM 金鑰、同一條信任鏈證明身分。例外的邊界只在時間：別的 CA 簽的、用途不對的、私鑰不符的憑證，不論時間都拒絕啟動，換發也救不了它們。

**TPM 金鑰沒有密碼，agent 也不收任何密碼。** 金鑰以空密碼（empty auth）使用，設了密碼的金鑰在啟動時的測試簽章就失敗（Laysi/trustiot#121 的 PIN 經命令列外洩因此沒有對應物）。執行檔與兩個子命令只認得各自 `--help` 列出的參數（加上 `-h`、`-V` 兩個縮寫），沒有 PIN、密碼或授權值的參數，未知的參數以結束碼 2 拒絕；設定檔的 `identity.key` 沒有這類欄位，未知的欄位拒絕啟動；agent 自己的程式只讀 `NOTIFY_SOCKET`、`WATCHDOG_USEC`、`WATCHDOG_PID`、`STATE_DIRECTORY`、`TRUSTIOT_AGENT_LOG`、`JOURNAL_STREAM` 這幾個環境變數（glibc 另外讀 `MALLOC_*` 這類自己的設定），不讀整個環境，也不啟動其他程式；unit 的命令列只有設定檔的路徑，環境變數只有 `MALLOC_MMAP_THRESHOLD_`，沒有 `EnvironmentFile`、`PassEnvironment` 或 credentials，sysusers 的帳號沒有密碼欄位也不能登入。

測試：`tests/binary.rs` 的 `without_a_valid_identity_the_agent_never_gets_ready_connects_or_polls_a_saved_configuration` 以真的執行檔、通知 socket、狀態目錄裡的設備設定與一個會接受連線的 port 驗證上面五種情況（沒有設定檔、沒有憑證、沒有私鑰、別的 CA 簽的憑證、別張憑證的私鑰），並以完整的身分對照：同一組檔案會送 `READY=1`、以存下的設定輪詢、連到那個 port。`--check` 的同類拒絕與過期憑證的警告在同一個檔案（`check_accepts_a_working_configuration_and_refuses_a_key_of_another_certificate`、`check_refuses_a_certificate_the_broker_ca_did_not_issue`、`an_expired_certificate_only_warns_because_the_clock_may_be_wrong`、`check_reports_the_root_crl_and_refuses_one_the_root_did_not_sign`），設了密碼的 TPM 金鑰在 `tests/tpm_swtpm.rs`；不收密碼的部分是 `tests/secret_inputs.rs`（參數、設定檔、讀取的環境變數、不啟動其他程式）與 `tests/systemd_unit.rs` 的 `the_unit_passes_no_secret_on_the_command_line_or_in_the_environment`。

## 連線（WebSocket 內的 mTLS、MQTT 5）

只有一種連線方式，SaaS 與地端相同（[決策 6](../../docs/architecture/decisions.md)、[部署](../../docs/architecture/deployment.md)）：

1. TCP（`TCP_NODELAY`，每個解析出的位址各等 10 秒），連到 `cloud.url` 的主機。開 TCP keepalive：閒置 60 秒後每 15 秒探一次，4 次沒回應就由核心斷線，是 MQTT keepalive 之外的保險。
2. 外層 TLS：SNI 是 URL 的主機，ALPN `http/1.1`，信任 `cloud.trust` 的檔案；不用 `webpki-roots` 也不讀作業系統的憑證庫。
3. WebSocket：`GET /mqtt`，subprotocol `mqtt`，只送收 binary message；對方沒回 `mqtt` subprotocol 就失敗。每則送出的 message 最多 32 KiB（一筆內層 TLS record 放得下），收到的 message 與 frame 最多 64 KiB，讀取緩衝 16 KiB。text message 視為協定錯誤；close 視為連線結束。
4. 內層 mTLS：SNI 與名稱驗證都用固定的 `mqtt-broker.trustiot.internal`，只信任 `broker.ca` 的根 CA，有根 CRL 時以它檢查簽發 CA，用閘道器的憑證與金鑰（TPM）。
5. MQTT 5。

開一條連線（上面 1–4 步）最多 30 秒（`CONNECT_TIMEOUT`），之後等 CONNACK 另外最多 30 秒；逾時都算連線失敗，照退避重試。

兩層都只用 TLS 1.3，套件順序是 ChaCha20-Poly1305、AES-256-GCM、AES-128-GCM（閘道器的 CPU 沒有 AES 指令，rustls 預設會選 AES-256-GCM），都開 session resumption（每層各記 8 個）：resumption 不需要 TPM 簽章，重連省下約 70 ms。加密實作只用 `ring`。Mosquitto 依自己的套件順序選，不依用戶端的順序，所以 broker 的 mTLS listener 要以 `ciphers_tls1.3` 把 `TLS_CHACHA20_POLY1305_SHA256` 排第一（chart 給閘道器的 listener 在 2026-10-23 補上，Laysi/trustiot-v2#564），否則協商出 AES-256-GCM（2026-10-21 實測，[測試報告](../../docs/test-reports/2026-10-21-86-agent-mqtt-connection.md)）。

### 內層 TLS 對 broker 的信任

依 Laysi/trustiot-v2#556 的決定（[services/certs](../../services/certs/README.md)「閘道器對 broker 伺服器憑證的信任」與「根 CRL」），在 `src/tls.rs`：

- **信任錨**：`broker.ca` 的根 CA，可以多張；簽發 CA 或 leaf 拒絕啟動。以簽發 CA 當信任錨時，根 CRL 撤銷它也沒有用，換一代簽發 CA 還要改每台閘道器的設定。每張根的主體要不同：CRL 只以主體指出簽發者，同名的兩個根會讓 webpki 拿錯的金鑰驗 CRL，經另一個根的連線全部失敗。
- **憑證鏈**：broker 送它的憑證與簽發 CA，agent 由此接到根；broker 只送自己的憑證時連線失敗（`UnknownIssuer`）。
- **名稱**：SNI 與驗證都用 `tls::BROKER_NAME`（`mqtt-broker.trustiot.internal`），SaaS 與地端相同，與 `cloud.url` 無關；名稱驗證不能關。
- **用途**：broker 的憑證要有 EKU serverAuth（webpki 的檢查），只有 clientAuth 的憑證（例如同一個簽發 CA 簽的閘道器憑證）被拒。沒有 EKU 的憑證 webpki 不檢查用途，由 CA 不簽這種憑證來保證（services/certs）。
- **撤銷**：rustls 的 `WebPkiServerVerifier` 以 `with_crls` 帶入根 CRL、設 `allow_unknown_revocation_status`，不用 `only_check_end_entity_revocation` 與 `enforce_revocation_expiration`。所以根以下的每張憑證都以根 CRL 檢查，被撤銷的簽發 CA 拒絕；broker 自己的憑證由簽發 CA 簽，閘道器沒有簽發 CA 的 CRL，狀態未知，照樣通過；根 CRL 過期仍然以它檢查、照常連線。沒有根 CRL 時不檢查撤銷。
- **載入**：啟動時讀 `broker.crl`，之後由 agent 自己取得更新的根 CRL（見下方「根 CRL 的更新」）。檢查的實作在 [`crates/root-crl`](../../crates/root-crl/README.md)，certs 服務採用根 CRL 前用的是同一份（Laysi/trustiot-v2#830），所以 certs 交給閘道器的根 CRL 都是 agent 接受的。根 CRL 要由 `broker.ca` 中名稱相符的根簽章（根有 key usage 時要有 cRLSign），webpki 也要能解析；不符就拒絕啟動，壞掉或偽造的檔案不會被默默略過。帶 issuing distribution point 的根 CRL 也拒絕：webpki 只把它用在範圍內的憑證，範圍排除簽發 CA 時等於不檢查，又因為允許狀態未知而不會報錯（根 CRL 的格式規則在 services/certs「根 CRL」）。
- **簽章演算法**：載入時驗根的自簽章與根 CRL 的簽章，用的是握手時 webpki 用的同一份清單（ring provider 的 `signature_verification_algorithms`），依 AlgorithmIdentifier 的位元組比對；x509-parser 只負責解析。所以不會有載入時通過、每次握手卻失敗的根或根 CRL（例如 SHA-1，或多帶參數的 ecdsa-with-SHA256）。
- **錯誤訊息裡的名稱**：憑證與 CRL 的名稱來自任何人都能寫的檔案，寫進錯誤訊息前把控制字元換成空白、最多 200 個字元，免得偽造或切開日誌的行。
- **已知的限制**：一張根 CRL 只涵蓋簽它的根底下的簽發 CA。換根期間 `broker.ca` 有兩個根時，另一個根底下的簽發 CA 不檢查撤銷；agent 一次只持有一張根 CRL（簽它的根之下），支援多張根 CRL 與換根一起規劃，留給後續的 issue。根 CRL 檔被刪除或換成較舊的不會關掉撤銷檢查（見下方「根 CRL 的更新」的「記下接受過的最高編號」）。

### 根 CRL 的更新

Laysi/trustiot-v2#563，在 `src/revocation.rs`。certs 服務在閘道器的網址 `GET /certs/root-crl` 交出它採用的根 CRL（[services/certs](../../services/certs/README.md)「根 CRL」，`protocol::root_crl::ROOT_CRL_PATH`），agent 定期取得、只往前換上，存到 `broker.crl`。沒有設定 `broker.crl` 時不取得。

- **向誰要**：與換發同一個 HTTPS 用戶端（`renewal::HttpsEndpoint`）：`cloud.url` 的主機與連接埠、外層 TLS 的信任（`cloud.trust`），不帶用戶端憑證，回應最多 64 KiB（`root_crl::MAX_BYTES`），每次請求有逾時。不連其他目的地，所以出口規則（「出口規則」）不必為它開別的位址。200 是 PEM（`application/x-pem-file`），404 是平台還沒有根 CRL，其他狀態與連線失敗都是失敗。
- **什麼時候要**：啟動後在 5 分鐘內的隨機時間第一次要（同時重開的閘道器不會一起要；根 CRL 被扣住時立刻要，見下方），之後每 6 小時的一半到全部之間的隨機時間要一次；失敗（連不上、非 200／404、內容被拒）時改以退避重試，30 秒起、最多 30 分鐘，成功後回到正常的間隔。根 CA 簽根 CRL 是少有的離線儀式，certs 採用後 6 小時內每台閘道器都會拿到。任何失敗都不會讓 agent 或 MQTT 連線停下：記一筆含原因的 WARN，繼續用手上的根 CRL。已經有根 CRL 時平台回 404 也記 WARN（平台被回滾，或路徑上有人壓住），但照正常間隔再要。根 CRL 有根的簽章，路徑上的人偽造不了、也換不回較舊的，但可以一直不讓新的到達（回 404 或擋掉請求）；閘道器端沒有「多舊就不連」的上限（過期仍照用，見上方「撤銷」），雲端由心跳的 `root_crl` 看得出哪台閘道器的編號落後。
- **只往前**：取得的根 CRL 要通過 agent 啟動時的同一套檢查（`RootCrl::from_bytes`：`broker.ca` 中名稱相符的根簽章、沒有 issuing distribution point 等），要有 CRL 編號（最多 49 位數，與心跳的上限相同），要與手上的根 CRL 是同一個根簽的，編號要比手上的大，也不低於接受過的最高編號。編號相同而內容相同時什麼都不變；編號相同而內容不同時拒絕（一個編號只對應一份根 CRL）。其他情況全部拒絕、記錄原因，手上的照用。比較編號的規則與 `provision install` 相同（十進位、任何長度）。
- **存下**：換上的根 CRL 以 `provision install` 的同一個寫法寫到 `broker.crl`：同一個開好的目錄裡先寫新檔、flush、rename 蓋過舊檔、再 flush 目錄，擁有者是目錄的擁有者、權限 0644，內容是標準 PEM。停電時留下的是舊檔或新檔，不會是寫一半的檔案；重新啟動時載入最後存下的。寫不進去時（例如磁碟滿了）照樣在記憶體裡用新的、記一筆 WARN，以退避（最多 30 分鐘）再要一次並重寫。`broker.crl` 的目錄要讓 agent 寫得到（範例的 `StateDirectory`）；`provision install` 也接受屬於 root 的目錄，放在那裡時 agent 每次都存不了、一直以退避重試，重新啟動後也要等取得才有新的那張。所以啟動時檢查這個目錄（`revocation::check_directory`，Laysi/trustiot-v2#926）：不跟隨符號連結開啟，以 `faccessat`（`AT_EACCESS`）確認執行的帳號有寫入與搜尋權限（唯讀的檔案系統也算不可寫），不建立、不改任何東西。不可寫時 agent 照常啟動、記一筆 WARN（取得的根 CRL 照用，但存不下）；`--check` 則以 1 結束，日誌寫 `check failed: broker.crl:` 與原因。目錄不存在或路徑經過符號連結也一樣。檢查以執行的帳號判斷：systemd 以 agent 帳號啟動，所以啟動時的 WARN 是準的；`--check` 要以 agent 帳號執行（例如 `sudo -u trustiot-agent trustiot-agent --check`）才看得出 agent 寫不寫得到，以 root 執行時幾乎一定可寫。
- **記下接受過的最高編號**：`broker.crl` 旁邊的 `<broker.crl>.highest`（例如 `root-crl.pem.highest`）記著根（公鑰 SubjectPublicKeyInfo 的 SHA-256）與這台閘道器接受過的最高 CRL 編號，是兩個欄位的 TOML，在 `broker.crl` 之後以同樣的方式寫入，只升不降。啟動時，若這個檔案記的根還在 `broker.ca` 裡，而 `broker.crl` 不見了、沒有編號或編號比它低，agent 不會當成「沒有根 CRL」而關掉撤銷檢查，而是扣住根 CRL（`RootCrl::Withheld`）：不開任何內層 TLS 連線（連線的錯誤是 `connecting failed at root CRL: ...`，`systemctl status` 的 `STATUS=` 看得到），並立刻、之後依退避一直向平台要，拿到編號不低於它的根 CRL 就換上、照常連線。這個檔案壞掉時當成「接受過、編號不明」，同樣扣住，拿到任何一張通過檢查的根 CRL 就恢復。記的根不在 `broker.ca` 時（`provision install` 換了根），或 `broker.crl` 是 `broker.ca` 中另一個根簽的可用根 CRL 時，這個檔案不算數：記號只保護同一個根的編號，換根要寫 `broker.crl`（`provision install` 先 rename `broker.crl` 再 rename 記號，兩者之間斷電就留下舊根的記號）。這樣的記號是過時的：agent 換上（或從檔案接手）`broker.ca` 的根簽的根 CRL 時，把 `broker.crl` 與記號一起寫成那個根的，前提是 `broker.ca` 仍是 agent 啟動時的那些根（見下一點），而且 `broker.crl` 不存在或是那個根的可用根 CRL；`broker.crl` 在但不能用時兩個檔案都不動（那可能正是記號那個根自己的根 CRL，由沒拿鎖就改了 `broker.ca` 的方式寫入）。所以 `provision install` 沒送根 CRL 就換了根、留下舊根的記號而 `broker.crl` 不見時，agent 取得的根 CRL 照樣存得下，不必重跑 `provision install`（Laysi/trustiot-v2#926；之前是一直存不下來）。所以 `broker.ca` 同時有兩個根時（只能手動設定，`provision install` 只裝一個根），能寫 `broker.crl` 的人可以換成另一個根的根 CRL，而那時本來就只有一個根之下的簽發 CA 檢查撤銷（見上方「已知的限制」）。這是 fail closed：只刪掉或換舊 `broker.crl` 能讓閘道器暫時連不上，但不能讓它接受被撤銷的簽發 CA 簽的 broker 憑證；能刪檔的人本來就能讓 agent 停下。記號檔與 `broker.crl` 在同一個目錄，所以這只防 `broker.crl` 單獨遺失或回滾：兩者一起刪掉，或一起還原成較舊但一致的一組（例如資料分割區回滾），agent 會從「還沒有根 CRL」或較舊的那張開始，直到下一次取得（啟動後 5 分鐘內）。能寫這個目錄的只有 agent 帳號與 root。要防這一種，最高編號要放到磁碟以外（例如 TPM 的 NV 單調計數器），留給後續的 issue。
- **與 `provision install` 同一把鎖**：每次為了判斷而讀 `broker.crl` 與記號檔，以及每次寫入，都在它們目錄的 `flock` 之下做，也就是 `provision install` 規劃根 CRL 前取的那一把（見「安裝憑證鏈與根憑證」第 5 項），在 blocking pool 上執行。所以安裝與更新不會都以同一份舊檔做決定：後做的那一方看得到先做的寫了什麼，同樣只往前。判斷時也在這把鎖下重讀 `broker.ca`（`BrokerTrust::load`，與啟動時同一套檢查；Laysi/trustiot-v2#926）：`broker.crl` 的目錄存在時，`provision install` 不論有沒有送根 CRL，都拿著這把鎖換 `broker.ca`，所以 agent 的判斷與安裝看到同一份信任錨。`broker.ca` 的根與 agent 使用中的不同（安裝在 agent 執行時換了根），或讀不了、不能用時，什麼都不寫：舊根的根 CRL 寫進 `broker.crl` 會讓 agent 在新的根下拒絕啟動。這時取得的較新根 CRL 只在記憶體裡用（同一個舊根，連線照舊以它檢查）；這不算更新失敗，只記一次 WARN（`root CRLs are used but not saved until the agent restarts`），照原本的間隔排程，因為提早再要也存不了。重新啟動 agent 之後恢復，安裝本來就要求重新啟動。這個重讀只防 `provision install`：其他改 `broker.ca` 的方式（手動編輯、組態管理工具、映像更新）不拿這把鎖，要在 agent 停止時做，或先取 `broker.crl` 目錄的 `flock`。安裝寫進較新的根 CRL 時，agent 下次更新就從檔案接手（每次更新不論平台回什麼、連不連得上，都先對齊記憶體與檔案；先前存不進去的也在這時重寫），不會蓋掉；安裝也會把記號檔升到它寫入的編號。
- **換上之後強制完整交握**：換上新的根 CRL 時以它建立新的內層 TLS 設定，帶一個新的、空的 resumption 快取，並把設定的版本加一（`tls::InnerConfig::use_root_crl`，換發換上新憑證時也走同一條路）。版本一變，連線先以新的設定重驗這條連線的 broker 在握手時送的憑證鏈（`link::Connector::trust_check`、`tls::InnerConfig::check_broker`；Laysi/trustiot-v2#926）：與握手同一個 rustls `WebPkiServerVerifier`、同樣的根、根 CRL 與名稱，不需要新的握手（resumption 的連線用它接續的 session 的憑證鏈；拿不到憑證鏈時算驗不過）。
  - **驗不過**（例如新的根 CRL 撤銷了它的簽發 CA）：立刻中斷。重驗與連線的其他工作並行，一看到版本變了就做，不等連線手上的事（等 CONNACK、寫到一半的封包、收尾）告一段落，所以從換上到中斷沒有等待，也就沒有需要設定的上限，與 120 秒的收尾無關。中斷時不再寫任何東西給這個 broker：不送 `offline`／`shutdown`、不送 DISCONNECT，寫到一半的封包也不寫完；broker 看到的是連線斷掉（真的 broker 會發布 will）。已寫出但還沒有 PUBACK 的 publish 回 `ConnectionLost`，佇列裡還沒寫出的回 `NotConnected`，與連線斷掉時相同：遙測的批次本來就要等雲端的 `telemetry-ack` 才從離線緩衝拿掉，所以留在緩衝，下一條連線再送同樣的 bytes。雲端可能收到重複（以 `duplicate` 確認），不會遺失。不等這個 broker 的 PUBACK，是因為它回的確認（PUBACK 與 `telemetry-ack` 都經過它）已經不可信。接著立刻重連，完整交握以新的根 CRL 檢查，仍是被撤銷的 broker 就在握手被拒，照退避重試。新的根 CRL 到達之前，被撤銷的簽發 CA 簽的假 broker 本來就連著，這一點不變。
  - **驗得過**（新的根 CRL 沒撤銷它，或換發換了用戶端憑證）：與換發時一樣收尾目前的連線：不再接受新的 publish，已接受的寫完並等確認，最多等 `max(PUBACK 逾時, 5 秒)`（預設 120 秒），再送 `offline`／`shutdown` 結束，然後立刻重連（連線還在建立中時，等它連上後才收尾）。這時 broker 已經以新的根 CRL 驗過，收尾期間照常往來沒有問題。

  重連的內層交握是完整交握（不是 resumption），以新的根 CRL 檢查 broker 的憑證鏈：新的根 CRL 撤銷的簽發 CA 簽的 broker 憑證被拒。換發與根 CRL 的更新共用一把鎖改內層設定，兩者同時發生時不會蓋掉對方。測試：`tests/root_crl_update.rs` 的 `a_root_crl_revoking_the_brokers_issuing_ca_closes_the_open_connection_at_once`（真的 WebSocket 與 mTLS：沒撤銷時等確認後收尾；撤銷時 broker 下一個看到的就是連線結束，在途的 publish 是 `ConnectionLost`，重連被拒），`tests/upload.rs` 的 `a_batch_in_flight_when_the_broker_is_revoked_goes_again_on_the_next_connection`（在途的批次在下一條連線以同樣的 bytes 再送，確認後才拿掉）。
- **心跳**：每則心跳帶 `root_crl`：使用中的根 CRL 的編號、`nextUpdate`，以及以閘道器的時鐘看是否已過期（`expired`）。根 CRL 過期仍然以它檢查、照常連線（見上方「撤銷」），雲端由心跳看到要換新的。沒有根 CRL 或被扣住時不帶這個欄位。

### MQTT 用戶端

MQTT 5 的編解碼由本 crate 自己寫，只做用到的部分（2026-10-21）：rumqttc 只能跑在它自己的 TCP、TLS、WebSocket 上，放不進「WebSocket 裡的 TLS」，而用本機 loopback listener 繞過會多一個攻擊面；可以跑在任意串流上的 crate 都不合適：`mqtt5-protocol` 從 2025-11 到 2026-10 發了 33 版、依賴作者自己的 `bebytes`，`mqtt-protocol-core` 約 4 萬行、6 個 proc-macro 相依，`mqttbytes` 最後一版在 2021 年。`src/mqtt/packet.rs` 是不做 I/O 的編解碼，`src/mqtt/connection.rs` 是一條連線的狀態機，`src/link.rs` 是對外的介面與重連。

- **CONNECT**：client ID 是憑證的 CN（= 閘道器 ID = Mosquitto 的 username），不送帳號密碼；keepalive 60 秒；will 見下方「在線狀態」；Receive Maximum 16、Maximum Packet Size `MIN_MAX_PACKET_BYTES`（1 MiB + 8 KiB）。CONNACK 的 Receive Maximum、Maximum Packet Size 照辦，Server Keep Alive 不是 0 時照辦；是 0 時（broker 不檢查連線）agent 仍用自己的 60 秒，否則連線無聲斷掉時不會發現；reason code ≥ 0x80 是拒絕（例如 0x87），記錄後退避重連。
- **Session**：每次連線都是 Clean Start 1、Session Expiry 0，broker 不替離線的閘道器保留 session 與排隊的訊息。理由：MQTT 5 規定接續 session 時用戶端要以原本的 packet ID 重送沒有 PUBACK 的 PUBLISH，那需要跨連線保留訊息內容，屬於離線緩衝（Laysi/trustiot-v2#90）；而且每次連線本來就有新的在線 session。影響：設定以 retained 訊息在訂閱時送到；遙測確認遺失時由 Laysi/trustiot-v2#90 的重送補上；閘道器離線時雲端送的 `log-request`、`live-control` 會遺失，雲端要在閘道器重新上線（`online`）後再送。
- **PUBLISH**：`Link::publish(channel, payload)` 只接受 `up` 的頻道，`status` 保留給 agent 自己；超過頻道上限或 broker 的 Maximum Packet Size 時直接回錯誤。QoS 與 retain 依 `Channel::delivery()`。`publish` 從不等佇列的位置：該 QoS 的佇列滿了就立刻回 `PublishError::Busy`，什麼都沒排進去；要等位置的呼叫端先以 `Link::reserve(channel).await` 拿 `PublishPermit`（等待時不帶 payload，依要求的先後拿到位置），再 `permit.publish(payload)`。心跳用 `Link::publish_in_session(session, ...)`：只送到該 session 的連線，那條連線結束後就回 `NotConnected`，不會帶著舊的 session 送上下一條連線；遇到 `Busy` 就跳過這一次，下一次照原本的間隔（連續跳過時只有第一次記警告）。QoS 1 等到 PUBACK 才回來：只有 0x00 與 0x10（no matching subscribers）是 `Acknowledged`；PUBACK 允許的其他碼（都 ≥ 0x80，例如 ACL 不允許的 0x87）是 `PublishError::Refused`，呼叫端要當成沒送出（[protocol README](../../crates/protocol/README.md)「傳輸：MQTT 5」）；MQTT 5 不允許出現在 PUBACK 的碼（例如 0x01、0x7F）是協定錯誤（見下方），呼叫端拿到 `ConnectionLost`，絕不算送達。連線在 PUBACK 之前斷掉是 `ConnectionLost`（可能送到也可能沒有），正常停止時也一樣。只有 `Connected` 的連線收 publish：連線中、建立連線的過程（等 CONNACK、`online` 的 PUBACK、SUBACK）與等著重連時，`publish` 與 `reserve` 立刻回 `NotConnected`，停止中與停止後立刻回 `Stopped`，都不排隊，緩衝是 Laysi/trustiot-v2#90 的事。佇列屬於一條連線：連線變成 `Connected` 時建立，連線結束時，還在佇列裡、沒有寫出去的請求（包括等位置的 `reserve` 與拿到但還沒用的 `PublishPermit`）都回 `NotConnected`（link 停止時是 `Stopped`），絕不會在下一條連線送出。
- **訂閱**：link 只訂閱有人處理的 `down` 頻道，訂閱哪些明列在 `LinkSettings::subscriptions`（`Subscriptions`：只能是 `down` 頻道、不能重複，照給的順序，各以自己的 topic 訂閱，QoS 1）。理由：QoS 1 的訊息一放進 `Incoming` 的佇列就回 PUBACK，broker 從此不會再送；訂閱了卻沒有人取走的頻道，訊息等於確認之後丟掉（例如 retained 的設定）。`main.rs` 訂閱 `dispatch::subscriptions()`：`config`（見「設定下發與套用」）、`telemetry-ack`（見「遙測上傳與離線緩衝」）與 `live-control`（見「即時值」），在同一個 SUBSCRIBE 裡；`dispatch::run` 一收到就取走，交給它的處理程式（不等任何處理程式，`Incoming` 因此不會卡住）。之後的 issue 寫處理程式時，同一個變更把它的頻道與處理程式加進 `dispatch`：`log-request`（Laysi/trustiot-v2#185）。沒有訂閱時（`Subscriptions::none()`）連上後不送 SUBSCRIBE（MQTT 5 不准沒有 topic filter 的 SUBSCRIBE，MQTT-3.8.3-2；編碼時也拒絕），`online` 拿到 PUBACK 就回報 `Connected`。有訂閱時，連上後先送 `online`、再送 SUBSCRIBE；`online` 拿到 PUBACK、SUBACK 也以 QoS 1 同意全部的訂閱（兩者先後不拘）之後才回報 `Connected`（`LinkState`、`STATUS=`；心跳與退避歸零的 60 秒都從這時算）。任何一個被拒（reason code ≥ 0x80，例如角色沒有該頻道時的 0x87）就記下 topic 與 reason code、送 DISCONNECT 0x04（broker 發布 will，雲端在 `online` 之後看到 `connection_lost`；見下方「連線結束與 will」），照退避重連（`LinkError::SubscriptionRefused`）：收不到設定或請求的閘道器不算連上。只給 QoS 0（0x00）的訂閱也一樣處理（`LinkError::SubscriptionDowngraded`）：QoS 0 的訊息不受 agent 的 Receive Maximum 管，agent 的佇列滿時 broker 可能直接丟掉。停止中才收到的拒絕不影響正常的停止。收到的訊息經 `Incoming::recv()` 交給應用程式，應用程式要及時取走：一則訊息等超過 5 分鐘（`DELIVERY_LIMIT`）算 agent 卡住，systemd 的 watchdog 會重啟它（見下方「systemd」）；topic 不是自己的 `down` 頻道、是沒有訂閱的頻道（broker 的錯誤：它不該送沒有訂閱的 topic）或超過頻道上限的，丟掉並記錄，仍回 PUBACK；QoS 1 的訊息放進佇列之後才回 PUBACK。
- **keepalive**：這一條的 keepalive 都是生效中的值：broker 在 CONNACK 給了非 0 的 Server Keep Alive 時是它，否則是 agent 自己的 60 秒。任一方向 keepalive 時間內沒有封包就送 PINGREQ，keepalive 的一半之內沒收到 PINGRESP 就斷線重連；寫入超過 keepalive 的 1.5 倍（至少 10 秒）也斷線。PUBACK 或 SUBACK 在封包寫完之後超過 keepalive 的兩倍（預設 120 秒）沒來也算連線失敗（從寫完算起：上傳很慢時寫入本身花的時間不算在 broker 回覆的時間裡）：送 DISCONNECT 0x04（broker 發布 will）後重連，等著的呼叫端拿到 `ConnectionLost`；否則只回 PINGRESP 不回 PUBACK 的 broker 會讓 in-flight 一直滿著。收到的訊息等著放進佇列（這時不讀 socket）的期間不判定 PUBACK、SUBACK 與 PINGRESP 的逾時，因為讀不到它們：這期間照樣在 keepalive 到時送一次 PINGREQ（broker 要在 keepalive 內收到封包），恢復讀取之後，這三個期限從恢復的時刻重新起算。
- **協定錯誤**：broker 送來壞掉或不該出現的封包時，在日誌記下 MQTT 5 對應的 reason code，再送 DISCONNECT 0x04 並關閉（broker 才會發布 will，見下方「連線結束與 will」）：格式錯誤 0x81（例如可變長度整數沒有用最短的編碼、固定標頭的旗標不對、字串不是合法的 UTF-8 或含 U+0000、topic 或 Response Topic 有萬用字元、packet ID 0、CONNACK 沒有屬性長度、欄位填不滿或超出 Remaining Length）、協定錯誤 0x82（例如第二個 CONNACK、clean start 之後或拒絕時 Session Present 為 1、QoS 2、對不上的 SUBACK 或 PUBACK：packet ID 不在等回覆的清單裡，包括重複的 PUBACK；沒有 PINGREQ 的 PINGRESP；該封包不允許的屬性、重複的屬性、超出範圍的屬性值，包括 agent 沒有要求的 Authentication Method、Response Information；AUTH 封包）、topic alias 0x94（沒有宣告支援）、超過 Maximum Packet Size 0x95（看到標頭就拒絕，不先配置）。reason code 不在 MQTT 5 給該封包的集合裡（CONNACK、PUBACK、SUBACK 的每一個、broker 的 DISCONNECT）也是協定錯誤 0x82；成功只認明列的碼：CONNACK 0x00、PUBACK 0x00 與 0x10、SUBACK 0x01（0x00 只給 QoS 0，當成訂閱失敗，見上；0x02 超過要求的 QoS 1，協定錯誤），從不以「小於 0x80」判斷。CONNACK 接受了 QoS 1、retained 的 will，卻宣告 Maximum QoS 0 或 Retain Available 0，也是協定錯誤 0x82（MQTT 5 要求這樣的 broker 拒絕 CONNECT），不送 `online`。MQTT 3.1.1 的 broker 以 3.1.1 的 return code（例如 0x01）拒絕時，錯誤訊息直接說 broker 不支援 MQTT 5。broker 送 DISCONNECT 時記下它的 reason code 與說明後重連。broker 送來的說明（reason string）解碼時就把控制字元（換行、tab、跳脫序列等）換成空白，記錄時最多 200 字，不能偽造或拆開日誌的行。UTF-8 字串裡的控制字元與 non-character 在 MQTT 5 只是「不應」出現，解碼時照常接受（reason string 如上換成空白）；Payload Format Indicator 為 1 時也不檢查 payload 是否為 UTF-8（MQTT 5 讓接收端決定，down 頻道都是 CBOR，由 `protocol` 解碼）。
- **broker 的 Maximum Packet Size**：MQTT 5 不准用戶端送超過 broker 在 CONNACK 宣告的 Maximum Packet Size 的封包（MQTT-3.2.2-15）。應用程式的訊息超過時 `publish` 回 `PublishError::TooLarge`，連線不受影響；agent 自己需要的封包（`online`／`offline`、SUBSCRIBE、PUBACK、PINGREQ）放不下時不送，以 DISCONNECT 0x04 結束連線（連 DISCONNECT 都放不下時不送，直接關閉；兩種 broker 都會發布 will），照退避重試（`LinkError::PacketTooLarge`）。
- **連線結束與 will**：只有正常停止（`offline`／`shutdown` 拿到 PUBACK）送 DISCONNECT 0x00，讓 broker 丟掉 will；其他任何結束都要讓 will 發布，否則 retained 的 `online` 會留在 broker 上，雲端以為閘道器還連著。MQTT 5 的規格寫 0x00 以外的結束都發布 will，但 Mosquitto 只在收到 DISCONNECT 0x04、或連線沒有 DISCONNECT 就斷掉時發布；0x80、0x81、0x82、0x83、0x94、0x95 都會讓它丟掉 will（2.1.2 實測，`tests/broker.rs` 的 `which_ends_make_mosquitto_publish_the_will`）。所以 agent 對每一種失敗（協定錯誤、訂閱被拒或只給 QoS 0、等不到 PUBACK 或 SUBACK、停止時等不到 `offline` 的 PUBACK）都送 0x04，MQTT 5 對應的 reason code 只寫進日誌；寫不出去時（連線已斷、PINGRESP 沒來、`online` 被拒）不送 DISCONNECT，直接關閉。

### 在線狀態與心跳

依 [protocol README](../../crates/protocol/README.md) 的在線狀態規則：

- 每次連線產生新的 `SessionId`；CONNECT 的 will 是這個 session 的 `Status::last_will`（`offline`／`connection_lost`），QoS 1、retained，發在 `up/status`。
- CONNACK 之後第一則訊息是同一個 session 的 `online`（QoS 1、retained）；它被拒絕（例如角色少了 `up/status`）時斷線重連，雲端不會以為閘道器在線。
- SIGTERM：送 `offline`／`shutdown`，等它的 PUBACK 最多 5 秒，再送 DISCONNECT 0x00，broker 丟掉 will；等不到 PUBACK 就送 DISCONNECT 0x04（請 broker 發布 will），雲端看到的是 `connection_lost`。這 5 秒從收到 SIGTERM 算起，也限制正在進行的寫入（例如上傳很慢時的大訊息）：期限到了還沒寫完就切斷；封包只寫了一半，之後不能再送任何封包（broker 會把 DISCONNECT 當成那個封包的後半），所以不送 DISCONNECT 直接關閉，broker 同樣發布 will。連線自己的停止最多約 7 秒（5 秒加上送 DISCONNECT 與關閉的 2 秒）；同時上傳把記憶體裡的批次寫進磁碟（見「遙測上傳與離線緩衝」）。`main` 最多等兩者 10 秒（`STOP_LIMIT`）就結束行程，unit 的 `TimeoutStopSec` 是 20 秒。正在等 CONNACK 時收到 SIGTERM，在同樣的 5 秒內繼續等：CONNACK 來了就照上面的步驟（先 `online`，再 `offline`／`shutdown` 與 DISCONNECT 0x00，不訂閱）；沒來就直接關閉。
- 心跳：每次連上馬上送一次，之後每 `heartbeat_interval_s` 送一次（QoS 0），帶這次連線的 session。目前有的欄位：agent 版本與執行時間、作業系統的開機時間與 uptime、記憶體（`/proc/meminfo`）、用戶端憑證的到期時間（換發後是新憑證的）、離線緩衝（`buffer`）與時鐘是否已校正（`clock_synced`，兩者見「遙測上傳與離線緩衝」）、使用中的根 CRL 的編號、nextUpdate 與是否已過期（`root_crl`，見「根 CRL 的更新」）；設備統計、CPU、磁碟、溫度、降頻與開機次數是 Laysi/trustiot-v2#117 的，現在是空的或 0。

### 重連

連線失敗或中斷後，以 full jitter 退避：第 n 次失敗等 0.1 秒到 min(上限, 起始 × 2^(n-1)) 之間的隨機時間（預設起始 1 秒、上限 60 秒），永遠不放棄。連線（從回報 `Connected` 算起）維持 60 秒以上才把退避歸零，所以一連上就被踢的情況不會變成每秒重連（`tests/link.rs` 以固定抽最長延遲驗證：連上就斷的連線延遲照樣加倍，維持 60 秒的才回到起始值）。哪一步失敗（根 CRL 被扣住、解析、TCP、外層 TLS、WebSocket、內層 mTLS、CONNACK）記在日誌與 `systemctl status` 的 `STATUS=` 裡。

### 記憶體上限

每條連線的記憶體有上限，不靠 systemd 的 `MemoryMax`（閘道器上沒有，[記憶體測試](../../docs/feasibility/2026-10-13-gateway-memory.md)）。link 持有的只有下表各項；等位置的呼叫端在 link 裡不持有任何 payload：`publish` 在佇列滿時第一次 poll 就回 `Busy`（payload 隨即丟掉，不排隊），`reserve` 的等待者只占一個等待的位置、還沒有訊息。呼叫端自己留著的資料由各自的上限管：離線緩衝是 `buffer.memory_limit_mib`（預設 16 MiB）的批次，加上收集視窗（最多約 8 MiB）與正在送出的一批（最多 256 KiB），見「遙測上傳與離線緩衝」。unit 設 `MALLOC_MMAP_THRESHOLD_=65536`：glibc 對 64 KiB 以上的配置各自 mmap、釋放就還給系統，所以批次不會讓 heap 破碎，緩衝清空後記憶體也會降回來（[記憶體測試](../../docs/feasibility/2026-10-13-gateway-memory.md)：預設的 glibc 在上限 256 MiB 時多用 27% 而且不歸還）。

| 項目 | 上限 |
| --- | --- |
| 等 PUBACK 的 QoS 1 訊息 | `inflight_limit`（預設 32，broker 的 Receive Maximum 較小時取它）；只記 packet ID、送出時間與回覆的通道，不留訊息內容 |
| 等著送出的 publish | 目前連線的 QoS 0 與 QoS 1 各一個佇列，各 8 則（`REQUEST_QUEUE`；佇列隨連線建立與結束，任何時候只有一組），每則不超過頻道上限（QoS 0：心跳 1 MiB、live 64 KiB；QoS 1：256 KiB），另有一則正在寫入連線；佇列滿了 `publish` 回 `Busy`、`reserve` 等位置。QoS 0（心跳、live）不受 in-flight 上限限制，in-flight 滿了照樣送出。即時值一次只送一則（`live::Publisher`），在 QoS 0 的佇列裡最多佔一個位置，心跳永遠有位置 |
| 收到、還沒被取走的訊息 | 佇列 16 則（`INCOMING_QUEUE`，也是送給 broker 的 Receive Maximum）加 1 則等著放進去；這時不再讀 socket，broker 的 Receive Maximum 讓它不再送 |
| MQTT 讀取緩衝 | 一個封包，最多 `MIN_MAX_PACKET_BYTES`；超過 64 KiB 的封包處理完就縮回 |
| WebSocket | 讀取緩衝 16 KiB，收到的 message 最多 64 KiB，送出的最多 32 KiB |

最壞情況（每則都到頻道上限）約 29 MiB；平常的遙測與心跳只用到其中一小部分。`tests/link.rs` 以停住的時鐘驗證前三項：PUBACK 不回來時正好送出 `inflight_limit` 則、佇列停在 8 則、以 `reserve` 等位置的呼叫端都在等，下一個封包是一整個 keepalive 之後的 PINGREQ；每回一個 PUBACK 剛好再送一則。佇列滿時，一千個同時的 `publish` 在第一次 poll 就全部回 `Busy`，佇列與 in-flight 都不變；`reserve` 的等待者在拿到位置之前不建 payload，位置依要求的先後給。

## 憑證換發

用戶端憑證（`identity.certificate`）由 agent 自己換發（決策 12、檢查清單 D2），走雲端憑證服務的 `POST /certs/renew`（`protocol::renewal`，流程見 [`services/certs/README.md`](../../services/certs/README.md)「換發」）：先要一個 challenge（`step: challenge`），再以目前憑證的金鑰（正式環境是 TPM 的金鑰）簽 challenge 的簽章輸入送回（`step: renew`），雲端為同一把已登記的金鑰簽一張新憑證。請求送到 `cloud.url` 的主機與 port（HTTPS，HTTP/1.1，外層 TLS 與 `cloud.trust` 的信任和 MQTT 連線相同），不需要用戶端憑證，所以憑證過期或被撤銷時也能換。

### 什麼時候換

- **照排程**：依閘道器的時鐘，效期剩下 `renewal.remaining_percent`（預設 33）時換發；90 天的憑證在剩 30 天時換。另外，從憑證放上來的時間（憑證檔的修改時間，寫檔時以閘道器的時鐘設定）起，也要等過效期的同一個比例；這個行程換過一次之後，再依單調時鐘等同樣久。所以時鐘快很多時不會一直重換，重開 agent 也一樣；時鐘慢很多時憑證看起來永遠沒到期，就靠下面 broker 的拒絕觸發。手動放上或複製憑證檔時要保留修改時間（例如 `cp -p`）：修改時間比簽發晚很多時排程換發會延後，最晚延到憑證過期後被 broker 拒絕，那時才換發。最多每小時重新讀一次時鐘（`RECHECK`），開機後 NTP 改了時間也會發現。
- **broker 拒絕憑證時**：內層 mTLS 握手（在外層 TLS 裡面，所以是 broker 那一側送的）或連線中收到憑證相關的 TLS alert、CONNACK 0x86 或 0x87（broker 的 dynamic security 已經沒有這台閘道器），或在雲端連得到（TCP、外層 TLS、WebSocket 都成功）的情況下內層 mTLS 或 CONNACK 之前持續失敗 10 分鐘再加 0 到 5 分鐘的隨機時間（`link::REFUSED_AFTER`、`link::REFUSED_JITTER`；broker 的 alert 可能因為連線被 reset 而沒送到；隨機時間讓同一個問題下的大量閘道器不會同時換發）。這表示憑證過期（以 broker 的時鐘）或被撤銷。換上新憑證之後，這段時間重新計算。外層 TLS 握手中的 alert 不算拒絕：路徑上任何東西都送得出來，不證明是 broker。
  - 明確的拒絕（alert、CONNACK）而憑證還沒被證明可用時，馬上換。
  - 推斷的拒絕（內層 mTLS 一直失敗）而憑證依閘道器的時鐘仍然有效時，等下面重試的退避再換：可能是 broker 的問題，不是憑證。
  - 已被證明可用的憑證被拒時，離上一次因拒絕而換發至少 15 分鐘（`renewal::REFUSAL_RENEWAL_INTERVAL`），冒充 broker 的東西不能讓閘道器一直換發。

用某張憑證開的連線維持 60 秒（`backoff::STABLE_AFTER`）之後，這張憑證才算被證明可用。link 每次連線嘗試都回報它的 session 與所用憑證的版本（`Link::attempts`，在回報 `Connected` 之前），只有嘗試用的是使用中憑證的連線才算：換上新憑證時已經連著的那條連線是用舊憑證開的，不算；換上新憑證的過程中就已經用新憑證重連上的，算。

### 重試

失敗後等一段時間再從新的 challenge 開始：full jitter 的指數退避（第 n 次失敗等 0.1 秒到 min(`retry_cap_s`, `retry_base_s` × 2^(n-1)) 之間，預設 30 秒、1800 秒），永遠不放棄。回應 429 或 503 帶 `Retry-After`（秒數）時，再加上至少那麼久（最多 `retry_cap_s`）。剛換上的憑證被證明可用之前也被 broker 拒絕時，不會馬上再換，同樣等退避（或先退回上一張，見下面「退回上一張憑證」）；新憑證被證明可用之後退避才歸零。每個請求（連線到讀完回應）最多 30 秒，回應最多讀 64 KiB。

### 檢查

新憑證全部通過才用，否則什麼都不改（檔案與使用中的憑證都不動），記一筆錯誤後照重試的規則再換：

- 回應的 HTTP 狀態與每個步驟相符（challenge 200、renew 201）、JSON 格式正確，challenge 是這台閘道器的、用途是換發、nonce 格式正確，才以金鑰簽章；renew 回應的硬體 ID 是自己的。
- `certificate_chain` 只有 PEM 憑證（最多 4 張，不能夾帶私鑰或其他區塊），第一張就是 `certificate`。
- `root_certificate` 必須是 `broker.ca` 裡的一張根：回應不能新增信任錨。
- webpki 以 `broker.ca` 驗證整條鏈可以當用戶端憑證，CN 是自己的閘道器 ID，公鑰與 TPM 的金鑰相同，EKU 與 key usage 是閘道器憑證的（與啟動時的檢查相同，`identity::with_key`）。
- 依閘道器的時鐘現在就能用：notBefore 最多比現在晚 300 秒（`renewal::CLOCK_SKEW`，與憑證服務把 notBefore 往前推的 `CLOCK_SKEW_SECS` 相同），notAfter 還沒過。閘道器有硬體 RTC，時鐘可以合理信任；還不能用的憑證換上去只會讓閘道器連不上。這與啟動時不同：啟動時手上的憑證時間不對只記警告，因為拒絕啟動只會變成停機；換發時拒絕新憑證則保留還能用的舊憑證。
- 新憑證的 notBefore 要比上一張被證明可用的憑證晚（防重送的下限），舊的回應重送也換不回去。下限只看被證明可用的憑證：換上之後從沒成功用過的憑證不會把下限往上推。

### 換上新憑證

1. 憑證鏈以原子替換寫回 `identity.certificate`：同一個目錄裡的暫存檔 `<檔名>.renewal.tmp`（沿用舊檔的權限；舊檔不存在時 0600；修改時間設成閘道器時鐘的現在，排程以此為準），fsync、rename、再 fsync 目錄，整段持有目錄的 `flock`（`provision install` 從檢查較新的憑證到 rename 也持有同一把鎖，兩者不會交錯）。中途斷電時，留下的是完整的舊檔或新檔，不會是寫一半的檔。檔案寫不進去時照樣使用新憑證並記錯誤：重開之後讀到舊憑證，需要時會再換。只有 fsync 目錄失敗時，檔案已經是新的，只是斷電仍可能回到舊的，記一筆警告。
2. 換掉內層 TLS 的設定（之後的連線都用新憑證），心跳的 `cert_not_after` 改成新憑證的到期時間。
3. 使用中的連線輪替：不再接受新的 publish（呼叫端拿到 `NotConnected`，與連線中斷時一樣由離線緩衝重送），已接受的照樣送出並等 PUBACK；全部確認之後像正常停止一樣送 `offline`／`shutdown` 與 DISCONNECT 0x00（broker 丟掉 will，雲端不會看到一次 `connection_lost`），然後不經退避，馬上以新憑證重連，新的 session 送 `online`。緩衝裡的資料一則都不會掉，也不會變成不確定是否送達（`ConnectionLost`）。等 PUBACK 最多 keepalive 的兩倍（至少 5 秒）；超過時送 DISCONNECT 0x04（broker 發布 will），還沒確認的訊息照連線中斷處理（`ConnectionLost`，由緩衝重送）。輪替途中收到停止訊號就照一般的停止結束。

### 退回上一張憑證

被證明可用、而且沒被 broker 拒絕過的憑證（照排程換發時）被換掉之前，先以同樣的原子寫法、同樣的權限存成 `<憑證檔>.prev`；因為被拒而換發時不留，因為 broker 已經不收它。新憑證在被證明可用之前就被 broker 明確拒絕（alert 或 CONNACK，不是推斷的拒絕），而 `.prev` 依閘道器的時鐘仍然有效時，換回 `.prev` 的憑證（不送任何請求），之後的換發等退避。憑證檔確實寫回（含 fsync 目錄）之後才刪掉 `.prev`；寫不回去時保留 `.prev`，重開後讀到被拒的憑證時仍能再退回。換回來的憑證算是還沒被證明可用、也沒有上一張，所以不會再從它退回去，不會來回切換。新憑證被證明可用之後刪掉 `.prev`。`.prev` 在 agent 重開後仍然有效；啟動時只有在它是這台閘道器、這把金鑰、鏈接到 `broker.ca`、而且比使用中的憑證舊時才採用，否則刪掉並記警告。

`tests/renewal.rs` 以替代的換發端點（`tests/support/renewal.rs`，HTTPS、與憑證服務相同的 challenge 與簽章檢查）與檔案金鑰驗證：broker 拒絕憑證（簽發 CA 不被信任，等同被撤銷）時馬上換發並以新憑證重連，請求的 `Host` 是 `cloud.url` 的主機與 port，檔案換成新的鏈、權限不變；503 與 429 依 `Retry-After` 等待、每次重試都從 challenge 開始；對 Mosquitto 容器，效期很短的憑證在到期前換發，換發期間持續 publish 的訊息一則都沒掉也沒重複、沒有 `ConnectionLost`，舊 session 以 `offline`／`shutdown` 結束、新 session 上線，心跳帶新的到期時間；已過期的憑證換發後 broker 接受；外層 TLS 握手收到 `bad_certificate` alert 時不算拒絕、不換發。排程（含時鐘快、重開後不重換）、重試間隔的順序與範圍、每一種回應被竄改時不換（含 notBefore 太晚與已過期）、防重送的下限、被證明可用的條件、拒絕後的等待與最短間隔、退回上一張憑證（含重開之後）、原子寫檔與寫檔失敗時舊檔不變，在單元測試（`cargo test -p agent --lib renewal`）以停住的時鐘與腳本化的端點驗證；連線輪替與拒絕的判斷（外層與內層 alert 的區別、推斷拒絕的隨機時間）在 `tests/link.rs`。TPM 簽 challenge 走與 TLS 相同的 `SigningKey`（`tests/tpm_swtpm.rs` 驗證 TPM 簽章），沒有在閘道器的實體 TPM 上測過。

## systemd

[`systemd/trustiot-agent.service`](systemd/trustiot-agent.service) 裝成 `/usr/lib/systemd/system/trustiot-agent.service`，執行檔在 `/usr/bin/trustiot-agent`，帳號由 [`systemd/trustiot-agent.sysusers`](systemd/trustiot-agent.sysusers)（裝成 `/usr/lib/sysusers.d/trustiot-agent.conf`）建立。由映像（Laysi/trustiot-v2#95）安裝。

- `Type=notify`：`READY=1` 在檢查完設定、憑證與金鑰之後送，不等連線；`STATUS=` 顯示連線狀態。
- `WatchdogSec=30s`：agent 每 15 秒送一次 `WATCHDOG=1`，要同時滿足兩個條件，否則不送，systemd 就重啟它。第一，連線的工作（supervisor 與連線的狀態機）有進展：它在每個等待開始前宣告這一步最晚何時結束（`src/liveness.rs`）：開連線最多 `connect_timeout` 加一次 TPM 回應的時間（共 40 秒）、一次寫入最多 keepalive 的 1.5 倍、等 CONNACK 30 秒、連線中與等 `online` 的 PUBACK 或 SUBACK 時等到下一個計時器（通常是 keepalive 到時的 PINGREQ）、等應用程式取走收到的訊息 5 分鐘（`DELIVERY_LIMIT`）、退避的延遲、停止 5 秒、關閉 2 秒。過了最晚時間再加 10 秒（`SLACK`）還沒結束，就不再送，並在 `STATUS=` 寫出卡在哪一步、已經多久。只看執行緒不夠：工作卡在一個永遠不結束的 await 時，執行緒照樣在跑。正常但很長的等待（沒有網路的現場退避最多 60 秒）有自己的期限，不會害 agent 被重啟。第二，連線執行緒的計數（每秒加一，`PROGRESS_TICK`）有前進：執行緒被佔住（例如 TPM 簽章）時計數會停。送 `WATCHDOG=1` 的工作在主 runtime 上，主 runtime 卡住時它也停住。TPM 簽章會佔住連線執行緒，所以 `tpm::RESPONSE_TIMEOUT` 加一次計數要小於 15 秒，開啟金鑰的兩個 TPM 指令要在 `TimeoutStartSec=30s` 之內（`tests/systemd_unit.rs` 檢查）。
- `Restart=always`，間隔從 2 秒逐步拉長到 1 分鐘，`StartLimitIntervalSec=0`：無人看管的閘道器永遠重啟。`After=network.target`，不等 `network-online.target`，沒有網路的現場不拖慢開機。
- 以專用帳號 `trustiot-agent` 執行（不是 root，也不是 `DynamicUser`：TPM 要固定的群組），`SupplementaryGroups=tss` 讀寫 `/dev/tpmrm0`（Debian 的 `tpm-udev` 建立 tss 群組）。`/etc/trustiot` 由 `ConfigurationDirectory` 建立、屬於 root、對 agent 唯讀；`/var/lib/trustiot-agent`（`StateDirectory`，0700）是 agent 自己的，已套用的設備設定（`desired-config.cbor`，0600）與離線緩衝（`buffer/`，0700）存在這裡。`Environment=MALLOC_MMAP_THRESHOLD_=65536` 見上方「記憶體上限」。
- 沙箱：`NoNewPrivileges`、空的 `CapabilityBoundingSet` 與 `AmbientCapabilities`、`ProtectSystem=strict`、`ProtectHome`、`PrivateTmp`、`InaccessiblePaths=-/run/dbus -/run/avahi-daemon -/run/ssh-unix-local`、`DevicePolicy=closed` 只開 `/dev/tpmrm0`、`RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK`、`SystemCallFilter=@system-service` 並去掉 `@privileged @resources`、`MemoryDenyWriteExecute` 等。`AF_NETLINK` 是給 Modbus 的位址守門員：每次連線前以 `getifaddrs` 讀閘道器自己的介面位址，glibc 的 `getifaddrs` 要開 netlink socket；少了它，每一次 Modbus 連線都被拒（Laysi/trustiot-v2#805，見「連線位址的限制」）。`InaccessiblePaths` 讓 agent 看不到 D-Bus system bus、avahi 與本機 sshd 的 unix socket，agent 三者都不用（Laysi/trustiot-v2#826，見「連線位址的限制」的「這一層擋不到的」）。留著前兩者，agent 就能請 avahi 在 LAN 上代送 mDNS，繞過出口規則。留著 sshd 的，agent 可以在自己的 home 放一把公鑰，從沙箱外的 sshd session 再連那兩個 socket。藏的是目錄，不是 socket 檔：socket unit 重新啟動時會換一個新的 socket 檔，新檔不會被藏住。前綴 `-` 讓沒有這些服務的閘道器照樣啟動 agent。刻意不設的：`PrivateDevices`（會藏起 TPM）、`ProcSubset=pid`（心跳要讀 `/proc/meminfo`、`/proc/uptime`）、`PrivateUsers`（tss 群組會失效）。`systemd-analyze security --offline` 的評分是 1.5（2026-10-21；2026-10-31 加上 `AF_NETLINK` 後仍是 1.5），剩下的是網路、TPM 裝置、tss 群組與 `/proc`。
- `Requires=` 與 `After=trustiot-agent-egress.service`：agent 帳號的出口規則先生效，agent 才啟動；規則載入失敗時 agent 不啟動（見下方「出口規則」）。
- `Wants=trustiot-agent-egress-check.timer` 與 `IPAddressDeny=`：agent 執行中規則被清掉時，`IPAddressDeny=` 照樣拒絕 loopback、metadata、多播與受限廣播位址，timer 每 10 秒檢查一次、在 11 秒內把規則載回（見下方「執行中被清掉的規則」）。

`tests/systemd_unit.rs` 檢查帳號不是 root、上述各行（含 `MALLOC_MMAP_THRESHOLD_`）、`RestrictAddressFamilies` 含 agent 用到的每個 address family（含 `AF_NETLINK`）、對出口規則 unit 的依賴與那個 unit 的設定、`IPAddressDeny=` 的內容、檢查的 timer 與 unit 的設定，以及四個 unit 的 `systemd-analyze verify` 與 agent unit、檢查 unit 的評分不高於 2.0（機器上沒有 systemd-analyze 時略過，`CI` 有設時失敗）。

`InaccessiblePaths` 的效果由 `tests/egress.rs` 的 `the_agents_sandbox_hides_dbus_avahi_and_the_local_sshd_and_keeps_sd_notify` 實際執行：在以 systemd 為 PID 1 的 Debian 13 容器（裝有 D-Bus、avahi-daemon 與 libnss-mdns，另有一個與閘道器同路徑、同權限的 `sshd-unix-local.socket` 替身）裡，以真正的 agent unit（含 `Type=notify` 與 watchdog）執行一個替身。替身以 agent 帳號、在 unit 的沙箱裡連 `/run/dbus/system_bus_socket`、`/var/run/dbus/system_bus_socket`、`/run/avahi-daemon/socket` 與 `/run/ssh-unix-local/socket`，全部得到 `Permission denied`；同一個帳號在沙箱外全部連得上。替身照常送 `READY=1`（`systemctl start` 才會成功）與 `WATCHDOG=1`（systemd 記下的 watchdog 時間往前走）。avahi 的 socket unit 停掉再啟動之後，執行中的替身仍然連不上。以 drop-in 清空 `InaccessiblePaths=` 時，同一個替身全部連得上（設定拿掉，測試就會失敗）。三個目錄都不存在時，agent 照樣啟動。同一個檔案的 `the_agent_resolves_dns_names_with_avahi_out_of_reach` 確認在閘道器的 `/etc/nsswitch.conf`（`files mdns4_minimal [NOTFOUND=return] dns`）下、avahi 連不上時，agent 帳號照樣以 DNS 解析不是 `.local` 的名稱：`mdns4_minimal` 根本不為這些名稱找 avahi，所以這個測試證明的是 `dns` 前面那一步會放行它們，不是 avahi 連線失敗時的行為；它也不在沙箱裡執行（systemd 的容器裡沒有 agent 帳號連得到的 DNS 伺服器：規則拒絕它連閘道器本身的每個位址）。這兩個測試需要的容器條件與 `tests/egress.rs` 其他測試相同，systemd 的那一個在容器跑不起 systemd 時略過，`CI` 有設時也一樣。

位址守門員在沙箱裡讀不讀得到介面，由 `src/modbus/address.rs` 的單元測試 `the_interfaces_can_be_read_in_the_units_sandbox` 實際執行：它從 unit 檔取出 `RestrictAddressFamilies` 那一行，以 `systemd-run --user` 開一個暫時的 unit，在這個限制下重跑讀介面的測試（`the_gateways_interfaces_can_be_read`），要通過。為了確定這個檢查真的有效，它先在拿掉 `AF_NETLINK` 的限制下跑一次，那一次要失敗。沒有 systemd 的使用者 manager（`systemd-run --user` 開不了 unit）時略過並印出原因，`CI` 有設時也一樣略過：不確定 CI 的 runner 有使用者 manager，而上面的檔案檢查在任何機器上都會執行；拿掉 `AF_NETLINK` 也讀得到（manager 沒有套用限制）時同樣略過，因為這時測試不可能失敗。

### 出口規則（`trustiot-agent-egress.service`）

[`systemd/trustiot-agent-egress.nft`](systemd/trustiot-agent-egress.nft) 是 agent 帳號的 nftables 規則（內容與理由見「連線位址的限制」的「核心這一層」），裝成 `/usr/share/trustiot-agent/trustiot-agent-egress.nft`；[`systemd/trustiot-agent-egress.service`](systemd/trustiot-agent-egress.service) 裝成 `/usr/lib/systemd/system/trustiot-agent-egress.service`，以 `nft -f` 載入它。映像（Laysi/trustiot-v2#809）安裝這兩個檔案與 `nftables.service` 的 drop-in（見下方），閘道器要有 `nftables` 套件（Raspberry Pi OS Lite 已內建，`/usr/sbin/nft`）。這個 unit 沒有 `[Install]`：agent 的 `Requires=` 會帶起它，不必另外 enable。

- **時機**：oneshot、`RemainAfterExit=yes`，排在 `nftables.service`（見下方）與 `systemd-sysusers.service` 之後（規則以名稱指定帳號，載入時才換成 uid）；agent 排在它之後，所以 agent 的第一條連線就受規則限制。
- **只動自己的 table**：檔案先確保 `table inet trustiot_agent` 存在，再刪掉、重建，整個檔案是 nft 的一個 transaction，重新載入時沒有空窗，也不碰其他 table。
- **載入失敗時（fail closed）**：例如沒有 `nftables` 套件、`nftables.service` 啟動失敗、規則檔或 drop-in 不見、帳號不存在、核心缺少需要的模組。這個 unit 變成 `failed`（`nftables.service` 失敗時是它變成 `failed`），agent 的啟動因依賴失敗而不執行（`Restart=always` 只管 agent 自己的行程結束，不會重試依賴失敗的啟動），雲端看到閘道器離線。修好原因之後 `systemctl reset-failed trustiot-agent-egress.service && systemctl start trustiot-agent.service`，或重開機。不自動重試：載入是確定性的，同一個原因重試也一樣失敗。（agent 的 unit 自己重新載入失敗時不同，會自動重試，見下一條。）
- **agent 每次啟動前重新載入（Laysi/trustiot-v2#863）**：agent 的 unit 有 `ExecStartPre=+/usr/sbin/nft -f /usr/share/trustiot-agent/trustiot-agent-egress.nft`，每次啟動 agent 的行程之前，包括 `Restart=always` 的自動重啟，都再載入一次同一個規則檔。原因：這個 unit 是 oneshot 加 `RemainAfterExit=yes`，root 不經 systemd 執行 `nft flush ruleset` 或 `nft -f /etc/nftables.conf` 之後它仍是 active，不會重跑；只靠 `Requires=`，agent 自動重啟時就在沒有規則的狀態下執行，而改成預設拒絕（Laysi/trustiot-v2#865）之後，規則不見就等於全部放行。規則檔只替換自己的 table、整個檔案是一個 transaction，規則還在時重新載入只會把 counter 歸零。
  - **失敗時**：規則檔壞了或不見、帳號不存在、沒有 `nft` 時這一行失敗，這次啟動失敗，agent 的行程不執行（這一行沒有 `-` 前綴）。這是 agent 自己的啟動失敗，不是依賴失敗，所以 `Restart=always` 會照 2 秒拉長到 1 分鐘的間隔再試，原因修好之後 agent 自己回來，不必有人去啟動；試的期間 agent 一直沒有執行。
  - **權限**：`+` 只讓這一行以 root、不套用 unit 的沙箱執行：nft 要 `CAP_NET_ADMIN` 與 netlink，而 agent 的 `CapabilityBoundingSet=` 是空的（`!` 只放寬帳號，nft 仍拿不到 capability）。agent 自己的行程照舊是 `trustiot-agent` 帳號、所有 capability 集合都是空的、`NoNewPrivileges`。這一行的輸入只有 root 擁有的 `/usr/sbin/nft` 與規則檔，agent 改不到；agent 能做的只有讓自己結束、觸發一次重新載入，間隔至少 2 秒（`RestartSec=`），結果和規則已在時一樣。
  - **沒有選的做法**：比對執行中的 table 與規則檔、不同就拒絕啟動。nft 列出規則的寫法會隨版本變（例如 `meta l4proto tcp` 列成 `6`，見 `scripts/check-gateway.sh`），比對容易誤判；而且只能拒絕、不能修復，規則被清掉之後 agent 就一直停著，要有人來重新載入。重新載入同時做到確認與修復。這個 unit 仍保留：與 `nftables.service` 的先後、drop-in 的檢查，以及 `nftables.service` 重啟或停止時連帶重啟或停止 agent，都靠它。
  - **檢查**：`tests/systemd_unit.rs` 確認這一行與規則 unit 的 `ExecStart=` 是同一個指令、沒有 `-`、unit 裡沒有其他以 `+` 或 `!` 提升權限的指令，以及 agent 的帳號、空的 capability 與 `NoNewPrivileges`；`tests/egress.rs` 在 systemd 底下清掉規則、結束 agent 的行程，看自動重啟（見下方）。
- **之後新增的 table 與集合（Laysi/trustiot-v2#865 照做）**：agent 帳號的出口依賴的每個 table 都放進這個規則檔（直接寫，或以絕對路徑 `include`），由同一個檔案在一個 transaction 裡載入。這樣這個 unit、`nftables.service` 的 reload 與 agent 的 `ExecStartPre=` 三處都跟著載入，不必各自修改；放在別的檔案、由別的 unit 載入的 table，在 flush 之後不會隨 agent 重啟回來。每次載入都整個重建 table，執行中加入的集合元素（fieldbus 的放行集合）也一起清空；預設拒絕之下，集合空著就是 fieldbus 出口全部被拒（fail closed，不會放行）。所以建議：集合在規則檔裡宣告成空的，更新它的特權元件（Laysi/trustiot-v2#864）在每次載入之後、agent 開始輪詢之前，依已套用的設定重新填入，例如 agent 的 unit 在這一行之後再加一行 `ExecStartPre=`，請常駐的那個元件填入，或 agent 啟動時先請它同步一次再輪詢；`nftables.service` 的 reload 也會清空集合，同樣要重新填入。填入的內容不能來自 agent 帳號寫得到的地方（例如 `/var/lib/trustiot-agent` 裡已套用的設定）：被攻破的 agent 改了那個檔案再讓自己結束，就能把任意目的地加進放行集合。所以來源要是 root 擁有的副本或驗過簽章的設定，逐筆檢查位址；agent 只能請求同步、不能提供內容。不要再加一行以 `+` 執行、自己去讀 agent 資料的指令：由常駐的特權元件經 unix socket 接受請求，自己讀它信任的來源。不要為了保留元素改成不刪 table、只清 chain：那樣規則檔就不再是 table 的完整內容，舊版留下的 chain 或集合不會被移除。（這是 `table inet trustiot_agent` 的寫法。預設拒絕的 table 有集合，刻意用另一種寫法，只替換 chain、保留集合，取捨見「連線位址的限制」的「預設拒絕」；集合由 agent 送出的清單與 `/etc/resolv.conf` 填入，不由 agent 的狀態目錄。）
- **停止**：沒有 `ExecStop=`，停止這個 unit 不移除規則（agent 會一起停，`Requires=` 的效果）；要移除規則必須刻意執行 `nft delete table inet trustiot_agent`。`systemctl reload trustiot-agent-egress.service` 重新載入規則、不重啟 agent；`restart` 會連 agent 一起重啟。
- **與 `nftables.service` 的關係**：Debian 的 `nftables.service` 啟動、reload 與重啟時執行 `/etc/nftables.conf`，預設第一行是 `flush ruleset`，停止時也 `flush ruleset`，都會清掉這個 table。所以這個 unit `Requires=` 並排在 `nftables.service` 之後：它一定先啟動，之後不會在 agent 執行中才啟動而清掉規則（已經 active 的 unit 再 `systemctl start` 不會重跑）；它重啟或停止時這個 unit 跟著重啟或停止（`Requires=` 的效果），agent 也跟著重啟或停止，而且在 flush 之前就停下（停止的順序與啟動相反）。reload 時 agent 繼續執行，所以 [`systemd/nftables-reload.conf`](systemd/nftables-reload.conf)（裝成 `/usr/lib/systemd/system/nftables.service.d/trustiot-agent-egress.conf`）把它的 `ExecReload=` 換成 `nft 'include "/etc/nftables.conf"; include "<規則檔>"'`，兩個檔案在同一個 nft transaction 載入：規則一刻也不會不見，任一個檔案不見、讀不到或載入失敗時整個 transaction 失敗、什麼都不變。檔案由 nft 自己讀，不經 shell 管線：管線只回報 nft 的結果，規則檔讀不到時會只套用 `/etc/nftables.conf`（含 `flush ruleset`）。這個 drop-in 沒裝時，這個 unit 的 `ExecStartPre=` 拒絕啟動（agent 也不啟動）。手動執行 `nft -f /etc/nftables.conf` 或 `nft flush ruleset` 不經過 systemd，仍會清掉規則：不要這樣做，`scripts/check-gateway.sh` 的 egress 檢查會發現。它啟動失敗（例如 `/etc/nftables.conf` 寫壞了）時規則不載入、agent 不啟動。代價是 agent 跟主機的 `/etc/nftables.conf` 綁在一起：那個檔案載入失敗，agent 也不執行。閘道器原本沒有啟用 `nftables.service`（2026-10-31 唯讀檢查 lab 的閘道器：`disabled`、沒有任何規則），agent 啟動時會把它帶起來，載入 Debian 預設的 `/etc/nftables.conf`（一個 chain 全部放行的 `inet filter` table），行為不變。映像（Laysi/trustiot-v2#809）的主機規則一律寫進 `/etc/nftables.conf`（drop-in 以 `include` 載入它；裡面的 `include` 建議寫絕對路徑）：`nftables.service` 啟動時的 `flush ruleset` 會清掉其他元件在執行中自己加入的 table，例如 NetworkManager 的共用連線、podman／netavark，或另一個 unit 載入的規則；開機時 `nftables.service` 很早執行，所以只有在它之後（例如第一次安裝 agent 時）才加入的會被清掉。不要再為 `nftables.service` 另加改寫 `ExecReload=` 的 drop-in：`ExecStartPre=` 只確認 `/usr/lib` 的 drop-in 存在，`/etc/systemd/system/nftables.service.d/` 裡改寫 `ExecReload=` 的 drop-in 會蓋過它，reload 就又會清掉規則（要 root 才能這樣改；`systemctl show -p ExecReload nftables.service` 看得到實際的指令）。
- **權限**：以 root 執行，只留 `CAP_NET_ADMIN`，`RestrictAddressFamilies=AF_NETLINK AF_UNIX`，其他沙箱設定見 unit 檔。

手動安裝（例如在展示用閘道器上更新 agent 的 unit；agent 的 unit 依賴出口規則的 unit，所以先裝規則與它的 unit，再裝新的 agent unit，否則 agent 啟動時找不到依賴而失敗）。檔案先放在 ssh 帳號的暫存目錄，以下在閘道器上執行：

```sh
sudo install -d -o root -g root -m 0755 /usr/share/trustiot-agent
sudo install -o root -g root -m 0644 trustiot-agent-egress.nft /usr/share/trustiot-agent/trustiot-agent-egress.nft
sudo install -o root -g root -m 0644 trustiot-agent-egress.service /usr/lib/systemd/system/trustiot-agent-egress.service
sudo install -d -o root -g root -m 0755 /usr/lib/systemd/system/nftables.service.d
sudo install -o root -g root -m 0644 nftables-reload.conf /usr/lib/systemd/system/nftables.service.d/trustiot-agent-egress.conf
sudo nft -c -f /usr/share/trustiot-agent/trustiot-agent-egress.nft    # 只檢查，不載入；帳號要已存在
sudo install -o root -g root -m 0644 trustiot-agent-egress-check.service /usr/lib/systemd/system/trustiot-agent-egress-check.service
sudo install -o root -g root -m 0644 trustiot-agent-egress-check.timer /usr/lib/systemd/system/trustiot-agent-egress-check.timer
sudo install -o root -g root -m 0644 trustiot-agent.service /usr/lib/systemd/system/trustiot-agent.service
sudo systemctl daemon-reload
systemd-analyze verify /usr/lib/systemd/system/trustiot-agent.service /usr/lib/systemd/system/trustiot-agent-egress.service \
    /usr/lib/systemd/system/trustiot-agent-egress-check.service /usr/lib/systemd/system/trustiot-agent-egress-check.timer
sudo systemctl restart trustiot-agent.service                       # 先載入規則，再啟動 agent 與檢查的 timer
systemctl is-active trustiot-agent-egress.service trustiot-agent.service trustiot-agent-egress-check.timer
sudo nft list table inet trustiot_agent
```

`tests/egress.rs` 在 Debian 13 的容器裡以 systemd 為 PID 1 跑這兩個 unit（agent 換成替身）：依 `nftables.service`、規則、agent 的順序啟動，替身的第一條連線就被拒；`nftables.service` 的 start、reload、restart 之後規則仍在，規則檔寫壞或不見時 reload 失敗而整個 ruleset 照舊、agent 繼續執行，stop 之後 agent 已停；`/etc/nftables.conf` 寫壞、規則檔不見或 `nftables.service` 的 drop-in 不見時 agent 不啟動，也沒有任何工作重試它。另一個測試在 `nft flush ruleset` 之後以 `kill -KILL` 結束替身：自動重啟先重新載入規則（規則的 unit 沒有重跑），替身的連線又被拒，重啟後的行程是 agent 的帳號、capability 全空、`NoNewPrivileges`；再清掉一次、規則檔寫壞時，重啟的 `ExecStartPre=` 失敗、替身沒有執行、規則仍不在，規則檔修好之後下一次自動重啟就帶著規則回來（見「測試」）。

### 執行中被清掉的規則（Laysi/trustiot-v2#878）

root 不經 systemd 執行 `nft flush ruleset` 或 `nft -f /etc/nftables.conf` 時，agent 仍在執行，規則的 unit 仍是 active，`ExecStartPre=` 要等 agent 下一次啟動才會跑。改成預設拒絕（Laysi/trustiot-v2#865）之後，規則不見就等於全部放行，所以這段空窗由兩層補上：

- **`IPAddressDeny=`（agent 的 unit，不靠 nftables）**：`localhost 0.0.0.0/8 multicast 255.255.255.255/32 169.254.169.254/32 fd00:ec2::254/128`。systemd 把它做成掛在 agent 的 cgroup 上的 BPF 程式，與 nftables 無關，flush 清不掉，規則被清掉的那一刻起照樣生效。只列事先就能寫死、agent 永遠用不到的目的地：loopback（127.0.0.0/8、`::1`）、`0.0.0.0/8`（連 `0.0.0.0` 時核心改連 loopback；`::` 與 `::ffff:127.0.0.1` 由 `localhost` 涵蓋）、多播、受限廣播與兩個 metadata 位址。UDP 等送出時以 `EPERM` 失敗；TCP 的 SYN 被丟掉，`connect` 等到逾時（不是 connection refused）。
  - **不列的**：閘道器自己介面上的位址、子網路的廣播位址與 IPv6 anycast 位址，會隨網路改變，unit 裡寫不死，這些等下一層在時間上限內把規則載回；也不整個拒絕 link-local（169.254.0.0/16、`fe80::/10`）：現場設備可能用 APIPA 位址（沒有 DHCP 的 RS485 轉換器），路由器也可能以 link-local 位址公告 DNS 伺服器（NetworkManager 寫成 `nameserver fe80::…%eth0`）。不設 `IPAddressAllow=`：它優先於 `IPAddressDeny=`。
  - **兩個方向都套用**：送出時看目的地，收到時看來源。所以規則還在時，nft 對 agent 連 loopback 或 metadata 位址的 TCP 回的 reset（來源是那個位址）也被丟掉，agent 看到的是逾時而不是 connection refused。agent 自己的位址守門員在連線前就拒絕這些位址（見「連線位址的限制」），只有被攻破的 agent 才會碰到，接受這個差異。
  - **systemd 不套用時不會報錯**：核心不支援 cgroup BPF，或 systemd 沒有權限載入 BPF 程式（例如 rootless 容器裡的 systemd，主機設了 `kernel.unprivileged_bpf_disabled`）時，systemd 照樣啟動 unit、不套用這一行，只在 debug 等級記一筆。2026-10-31 唯讀檢查 lab 的閘道器：核心 6.18（`CONFIG_BPF_SYSCALL=y`、`CONFIG_CGROUP_BPF=y`、`CONFIG_BPF_JIT=y`）、cgroup v2、systemd 257，條件都具備。閘道器上確認它真的生效（例如 agent 的 unit 啟動後檢查 cgroup 掛了 BPF 程式）還沒有做，見測試報告的後續。
- **檢查的 timer（`trustiot-agent-egress-check.timer` 與 `.service`）**：[`systemd/trustiot-agent-egress-check.timer`](systemd/trustiot-agent-egress-check.timer) 每 10 秒（`OnUnitActiveSec=10s`、`AccuracySec=1s`）啟動 [`systemd/trustiot-agent-egress-check.service`](systemd/trustiot-agent-egress-check.service)，它問核心 `table inet trustiot_agent` 在不在，在就結束、什麼都不寫。不在時：
  - 以 `nft -f` 載入同一個規則檔（與規則的 unit、agent 的 `ExecStartPre=` 相同），再 `systemctl --no-block try-reload-or-restart trustiot-agent.service`：agent 的 unit 沒有 `ExecReload=`，所以是重啟，agent 啟動時重新填入放行集合（見下方的約定）。
  - 規則檔載入失敗時，agent 正在執行（`systemctl is-active`）才 `systemctl --no-block try-restart trustiot-agent.service`：重啟的 `ExecStartPre=` 同樣失敗，agent 不執行，`Restart=always` 照 Laysi/trustiot-v2#863 的間隔重試，規則檔修好之後 agent 帶著規則自己回來。agent 在等自動重啟（`activating`，不是 `active`）時檢查不去動它，否則每 10 秒一次的手動重啟會蓋過 `RestartSteps=` 拉長的間隔。這次檢查以失敗結束，journal 看得到。
  - `nft list tables` 本身失敗（nft 或核心介面出問題，不是 table 不見）時只記一筆錯誤、以失敗結束，不載入也不重啟：重啟救不了，而且 agent 的 `ExecStartPre=` 不在這個沙箱裡，可能照樣成功，10 秒後又被重啟，變成迴圈。
  - `try-`：agent 停止時檢查不會把它啟動。
  - **時間上限**：規則被清掉之後最多 11 秒（`OnUnitActiveSec=` 加 `AccuracySec=`）就有一次檢查開始，再加上 systemd 啟動它的時間（測試裡是毫秒等級）。這段時間內只有 `IPAddressDeny=` 那一層。
  - **何時執行**：timer 沒有 `[Install]`，agent 的 unit `Wants=` 它，所以跟 agent 一起啟動；timer `PartOf=` agent 並排在它之後：agent 被停止或重啟（含自動重啟）時 timer 跟著停或重啟，agent 的行程意外結束、等自動重啟的期間 timer 照常執行。在 agent 啟動之前不需要它：那時 `ExecStartPre=` 剛載入過規則。開發時試過、測試抓到問題而沒有用的兩種寫法（`tests/egress.rs` 以 `kill -KILL` 結束替身，要求自動重啟恰好一次且 timer 仍在）：
    - timer `BindsTo=` agent、agent `Requires=` timer：行程結束時 agent 的 unit 短暫變成 inactive，`BindsTo=` 在這時停掉 timer，這個停止又經 `Requires=` 傳回 agent，取消自動重啟，agent 就一直停著。
    - agent `Requires=` timer：每次重啟傳到 timer 的重啟又經 `Requires=` 再重啟 agent 一次，重啟計數歸零，`RestartSteps=` 拉長間隔的效果就沒了（一直以 2 秒重試）。
    - 代價：`Wants=` 不把停止傳回 agent，root 單獨停掉 timer 時 agent 繼續執行、沒有檢查（`IPAddressDeny=` 仍在）；root 本來就能停掉任何東西。
  - **權限**：以 root 執行，因為 `systemctl` 要經 systemd 的私有 socket，只接受 root；其他權限都拿掉：`CapabilityBoundingSet=CAP_NET_ADMIN`（nft 要）、`NoNewPrivileges`、`RestrictAddressFamilies=AF_NETLINK AF_UNIX`、`IPAddressDeny=any`、`ProtectSystem=strict`、`ProtectProc=invisible`、`ProcSubset=pid`、`SystemCallFilter=@system-service` 去掉 `@privileged @resources` 等。沒有 `PrivateNetwork=`：規則屬於閘道器自己的 network namespace。`systemd-analyze security --offline` 的評分是 1.4（2026-10-31）。它的輸入只有核心的回答、root 擁有的規則檔與寫死的 unit 名稱，沒有 agent 寫得到的東西。
  - **journal**：`LogLevelMax=notice` 加 `SyslogLevel=warning`，每 10 秒一次的正常檢查連 systemd 的啟動、結束訊息都不留；只留規則不見（warning）、載入失敗（error）與 nft 的錯誤。
  - **擋不到的**：root 刻意停掉或遮蔽 timer、改寫 unit 或規則檔；root 能改核心規則，這一層本來就防不了 root。
  - **沒有選的做法**：常駐的 `nft monitor` 監看行程（多一個常駐的 root 行程與更多程式碼，timer 的上限已經夠用）；規則不見就停止 agent（`OnFailure=` 加 `Conflicts=`）：agent 停在那裡要有人去啟動，和 Laysi/trustiot-v2#863 的自動恢復衝突，載回規則再重啟同時做到確認與修復。

**給 Laysi/trustiot-v2#865 的約定（檢查載回規則之後，agent 怎麼得知、何時重新填入放行集合）**：放行集合在預設拒絕的 table 裡（`inet trustiot_agent_allow`，見「fieldbus 放行集合」的「給預設拒絕的約定」），規則被清掉時它也不見；檢查載回規則檔之後，集合是空的（或 table 不在），fieldbus 出口全部被拒（fail closed，不會放行），直到 agent 再送一次清單。

- **得知的方式**：檢查不另外通知 agent，而是在載入之後 `systemctl --no-block try-reload-or-restart trustiot-agent.service`。現在 agent 的 unit 沒有 `ExecReload=`，所以是重啟：agent 啟動時、輪詢之前本來就先送清單（「給預設拒絕的約定」的時機），重啟就涵蓋了重新填入，#865 不必為檢查另寫程式。#865 若為 agent 的 unit 加上 `ExecReload=`（例如送 `SIGHUP`，agent 收到就重送清單），檢查就改成 reload、不重啟 agent，檢查本身不必改；這個 `ExecReload=` 不能以 `+` 或 `!` 執行（`tests/systemd_unit.rs` 檢查）。
- **時間**：規則被清掉之後，集合最晚在檢查的上限（11 秒）加上 agent 啟動並拿到 `ok` 的時間之內填回。這段時間 fieldbus 輪詢會失敗（被拒），agent 照它的退避重試。
- **預設拒絕的 table 要放進規則檔**：檢查載入的是出口規則檔（`/usr/share/trustiot-agent/trustiot-agent-egress.nft`），只看 `table inet trustiot_agent` 在不在。預設拒絕的 table 放在同一個檔案（直接寫或以絕對路徑 `include`）時，`flush ruleset` 之後兩個 table 一起不見、一起載回；放在別的檔案時檢查不會載回它。只刪掉預設拒絕的 table（`nft delete table inet trustiot_agent_allow`）而 `inet trustiot_agent` 還在時，檢查不會動作：#865 要把它的 table 名稱也加進檢查的第一行（兩個 `nft list table` 都成功才算在），並在 `tests/egress.rs` 補一個只刪那個 table 的情況。
- **檢查不會動作的載入**：`nftables.service` 的 reload 與 `systemctl reload trustiot-agent-egress.service` 重新載入規則檔、table 一直在，檢查看不出來，agent 也不會重啟，集合卻被清空（若預設拒絕的檔案整個重建 table）。這兩處由 #865 照「給預設拒絕的約定」的「每次載入之後重新填入」處理（例如規則的 unit 加 `PropagatesReloadTo=trustiot-agent.service` 配上述 `ExecReload=`，或 agent 定期重送）；在那之前，這兩個 reload 之後 fieldbus 出口被拒，直到 agent 下一次啟動（安全的方向）。

`tests/egress.rs` 在 systemd 底下驗證兩層：agent 執行中 `nft flush ruleset` 之後，檢查在 timer 的上限內把規則載回並重啟 agent（從 journal 的時間算，規則的 unit 沒有重跑），規則檔寫壞時 agent 停住、修好之後帶著規則回來，檢查換成什麼都不做時規則一直不在；在規則被清掉、檢查還沒載回之前，agent 的 unit 連 loopback、`0.0.0.0`、metadata、多播與 `255.255.255.255` 都被拒，閘道器自己的位址與子網路廣播還能連（等檢查），LAN 上的設備（含 APIPA 位址）、雲端的名稱與連線、網路上的 DNS 伺服器（含 link-local 位址的）在規則在與不在時都照常；拿掉 `IPAddressDeny=` 時同樣的替身連得到 loopback。`IPAddressDeny=` 那一個測試要 systemd 能載入 BPF 程式，rootless 容器做不到，所以它在 rootless 時印出原因並略過，以 root 執行測試時（rootful podman）才會跑（見「測試」）。另一個測試確認 enable 之後重開機，agent 與 timer 都啟動、沒有 ordering cycle。

## Modbus 讀取

每台設備建一個 `DeviceReader::new(&device, &model)`（`crates/protocol` 的 `config::Device` 與它的 `DeviceModel`，已經過 `DesiredConfig` 的驗證），連線向一個 `Endpoints` 要：`let mut connection = endpoints.connection(&device)`（見下方「共用連線」）。每次輪詢呼叫 `reader.poll(&mut connection, &clock, timeout)`，得到 `Poll`：

- `readings`：每個讀到並解碼成功的資料點一筆 `telemetry::Reading`，時間是該請求的回應到達時 `Clock::now()` 的毫秒時間，帶 `clock_synced`。
- `failed`：其他每個資料點與原因（`health::LastFailure`：種類、時間、例外碼、說明）。
- `requests`：送出或連線失敗的每個請求、它的失敗與丟棄的站號不符回應數，給之後的讀取統計（H1）用。

### 只讀

請求只能從 `protocol::config::FunctionCode` 建立，它只有 1–4，所以程式送不出功能碼 5 以上（閘道器對現場設備唯讀）。整合測試在每次輪詢後檢查假端點的請求紀錄，確認沒有 1–4 以外的功能碼。

### 連線位址的限制

設備的主機由租戶的 editor 在雲端設定。雲端的設備 API（Laysi/trustiot-v2#100）只擋字面上指向閘道器本身的寫法，那是輸入檢查，不是安全邊界：雲端無法知道一個 DNS 名稱在現場解析成什麼，也擋不住檢查之後才改變的解析結果（DNS rebinding）。若 agent 照設定連到閘道器本機，輪詢就成了探測閘道器本機服務的管道，回應還會變成遙測回到雲端。所以真正的把關在閘道器連線的那一刻（`src/modbus/address.rs`，Laysi/trustiot-v2#653）：

- 每次開連線時把主機解析一次（`tokio::net::lookup_host`；IP 位址的寫法不查 DNS，但走同一條檢查），逐一檢查解析出的位址，再直接連到檢查過的 `SocketAddr`。名稱不會再交給連線函式，所以不會有第二次解析。重連時重新解析、重新檢查。
- 比對前先把 IPv4-mapped 的 IPv6 位址轉回 IPv4（`::ffff:127.0.0.1` 就是 `127.0.0.1`），連線也連到轉回後的位址。
- 拒絕的位址：
  - loopback：127.0.0.0/8、`::1`
  - 未指定：0.0.0.0/8（`0.0.0.0` 與 RFC 1122 的「本網路」，連過去都會到閘道器本身）、`::`
  - 閘道器自己網卡上的位址：每次連線時以 `getifaddrs` 讀目前的介面位址（不論介面是否啟用），因為 DHCP 可能改變它們
  - 多播：224.0.0.0/4、`ff00::/8`
  - 廣播：`255.255.255.255`，以及閘道器每個 IPv4 網段的廣播位址（由位址與 netmask 算出；/31 與 /32 沒有廣播位址，/31 的另一個位址是對端）
  - 雲端 metadata：`169.254.169.254`、`fd00:ec2::254`
  - IPv4-compatible 的 `::/96`（`::a.b.c.d`，RFC 4291 2.5.5.1 已廢止；`::` 與 `::1` 照上面的規則）：整段拒絕
  - 帶著 IPv4 位址、轉譯器或中繼可能轉送過去的 IPv6 寫法：內嵌的 IPv4 位址被上面任何一條 IPv4 規則（loopback、0.0.0.0/8、多播、廣播、metadata、閘道器自己的 IPv4 位址）拒絕時就拒絕，說明裡寫的是原本的 IPv6 位址。
    - NAT64 `64:ff9b::/96`：最後 32 位元
    - 本地用 NAT64 `64:ff9b:1::/48`（RFC 8215）：最後 32 位元，也就是業者在其中通常使用的 /96 配置；其中其他前綴長度的配置不解讀
    - 6to4 `2002::/16`：第 16–47 位元
- 其他位址都允許：169.254.0.0/16 的其他位址（沒有 DHCP 的 RS485 轉換器常用 APIPA 位址）、私有位址（10/8、172.16/12、192.168/16、IPv6 ULA），以及閘道器自己網段裡的其他位址，現場設備就在這些網段。IPv6 link-local（`fe80::/10`）不在拒絕之列，但這種設備實際上連不上：Linux 不接受沒有 scope ID 的 link-local 連線，而 `Host` 不收 zone ID，DNS 回答的 scope 也是 0。
- 名稱解析成多個位址時，被拒絕的跳過，依解析的順序連允許的位址，第一個接受的就用；都連不上時回報最後一個錯誤。
- 所有位址都被拒絕時，不開 socket、不送任何封包，讀取以 `ReadError::Connect`（`connect`）失敗，說明是「refused: every address the host resolved to points at the gateway itself or is not allowed (<解析出的位址>)」，最多列 8 個位址，不帶解析結果以外的資料。讀不到介面位址時也不連線（fail closed），一樣是 `connect` 失敗；這時每一條連線都會失敗，而預設的 `info` 日誌看不到輪詢的失敗，所以另記一筆 WARN（「cannot read the gateway's interface addresses」，帶錯誤與 `errno`），整個行程每 60 秒最多一筆，期間略過的次數寫在下一筆的 `suppressed`。glibc 的 `getifaddrs` 要開 netlink socket，unit 的 `RestrictAddressFamilies` 少了 `AF_NETLINK` 時它回 `EAFNOSUPPORT`（errno 97），每一次連線都被拒（Laysi/trustiot-v2#805，見「systemd」）。這和連不上一樣算連線失敗：其餘請求不送，下一次輪詢在退避之後重試，所以名稱之後解析成允許的位址時會自己恢復。

正式的路徑（`Endpoints::new`、`TcpConnection::new`）一律檢查，沒有任何設定能關掉它。測試的假 Modbus 端點（`crates/testkit` 的 `FakeModbus`）聽在 127.0.0.1，所以測試要用 `exempt_for_tests([127.0.0.1])` 放行那個位址。這個方法只存在於本 crate 的單元測試，以及 `test-exempt-addresses` feature 打開時；只有 dev-dependency 打開它（`Cargo.toml` 裡本 crate 對自己的，以及 `tools/lab-dataset` 為了錄製工具的測試），所以只有測試建置才有，agent 與使用它的工具的建置（`cargo build -p agent`、`cargo build -p lab-dataset`）沒有。設定檔、環境變數或經 MQTT 下發的設定都碰不到它。`cargo test -p repo-check` 會在任何正式依賴、其他 feature 或 `default` 打開這個 feature 時失敗。在閘道器上以 systemd 執行 agent 的端到端測試（Laysi/trustiot-v2#102）用的是正式路徑，所以假 Modbus 要放在閘道器以外的位址（另一個網路命名空間或另一台主機），不能放在閘道器本機。

這個檢查擋不到的：經路由器或 NAT 轉回閘道器的位址（含 hairpin NAT 與 port forward）、`64:ff9b:1::/48` 裡 /96 以外的 NAT64 配置、以 `ip route add local` 加入本機路由表但不在任何介面上的網段。這些由下面兩層處理。

#### 核心這一層（nftables，Laysi/trustiot-v2#813）

[`systemd/trustiot-agent-egress.nft`](systemd/trustiot-agent-egress.nft) 在 output hook 上對 agent 帳號（`meta skuid "trustiot-agent"`）拒絕下列目的地，與上面的檢查各自獨立：上面看 `getifaddrs`，這裡看核心的路由表，而且涵蓋這個帳號的每一個 IP socket，不只 Modbus 的連線（unix socket 不在這一層，見下方「這一層擋不到的」）。安裝與載入見「systemd」的「出口規則」。

- `fib daddr type local`：閘道器本身。127.0.0.0/8、`::1`、每個介面上的位址，以及 `ip route add local` 加入本機路由表的網段；連 `0.0.0.0` 與 `::` 時核心會改連 loopback，也在這裡擋下。chain 的優先順序是 filter（0），在 NAT 的 output hook（-100）之後，所以本機的 NAT 規則把連線轉回閘道器（port forward 回本機）時，看的是轉換後的目的地。
- `fib daddr type broadcast`：`255.255.255.255` 與每個 IPv4 網段的廣播位址。
- `fib daddr type anycast`：閘道器自己回應的 IPv6 anycast 位址。開了 IPv6 forwarding（例如 NetworkManager 的共用連線）時，核心會把每個前綴的 subnet-router anycast 位址（`fd81:3::/64` 的 `fd81:3::`、`fe80::`）加進本機路由表，送往它們的封包在本機遞送，fib 卻把它們算成 anycast，不是 local。閘道器自己的 link-local 位址（`fe80::…%eth0`）是 local，由上一條擋下。
- `fib daddr type multicast`：224.0.0.0/4、`ff00::/8`。
- `169.254.169.254`、`fd00:ec2::254`：雲端的 metadata 服務。其他雲端的 metadata 位址（`100.100.100.200`、`168.63.129.16`、`169.254.170.2`）只有 VM 形態的閘道器才需要，不在這裡。

TCP 以 reset 拒絕（`connect` 立刻得到 connection refused，不等逾時），其他協定以 ICMP admin-prohibited 拒絕（送出本身就以 `EPERM` 失敗）。每條規則有 counter，`nft list chain inet trustiot_agent agent` 看得到被拒的封包數。比對用的是 socket 的擁有者，沒有 socket 的封包（核心自己送出的 TCP reset 與 ICMP 錯誤，包括上面的拒絕送回給 agent 的那些）不受影響；其他帳號也不受影響。

這一層只拒絕、不放行，其他目的地維持放行，以免擋住現場設備（舊系統的 Laysi/trustiot#140）。依設備清單放行與預設拒絕（Laysi/trustiot-v2#814）在另一個 table，由同一個規則檔載入（見下方「預設拒絕」）：一個封包要通過每一條 output base chain，所以這裡的拒絕不會被那裡的放行蓋掉。

**不需要放行任何本機目的地。** agent 用到的本機資源都不走 IP：sd_notify 與 journal 是 unix socket，`getifaddrs` 是 netlink，TPM 是裝置檔。名稱解析：agent 以 glibc 的 `getaddrinfo` 解析雲端主機名稱（`tokio::net::lookup_host`），Raspberry Pi OS 13 上 `/etc/resolv.conf` 由 NetworkManager 寫入網路的 DNS 伺服器，沒有 systemd-resolved（127.0.0.53）或其他本機 stub，`/etc/nsswitch.conf` 的 `mdns4_minimal` 經 avahi 的 unix socket 查詢（2026-10-31 唯讀檢查 lab 的閘道器：`systemd-resolved` inactive、`NetworkManager` active、nameserver 不是 loopback 位址）。所以這一層不擋 DNS；預設拒絕只放行 `/etc/resolv.conf` 裡的 nameserver（見下方）。映像若改用本機的 DNS stub，agent 就解析不到任何名稱：`scripts/check-gateway.sh` 的 resolver 檢查會失敗，那時要在規則裡為 stub 的位址與 53 埠加一條寫明理由的放行。

#### 預設拒絕（Laysi/trustiot-v2#879）

[`systemd/trustiot-agent-default-deny.nft`](systemd/trustiot-agent-default-deny.nft) 是 `table inet trustiot_agent_allow`：output hook、priority `filter + 10`（在上面那一層與 NAT 的 output hook 之後，看的是改寫後的目的地），對 agent 帳號（`meta skuid "trustiot-agent"`）只放行三種封包，其餘都拒絕：

| 放行 | 規則 | 內容從哪裡來 |
| --- | --- | --- |
| fieldbus | TCP，目的地「位址 . port」在集合 `fieldbus4`／`fieldbus6` 裡 | agent 送出的清單，由 `trustiot-agent-allowlist` 填入（見「fieldbus 放行集合」；agent 端的串接是 Laysi/trustiot-v2#865） |
| 雲端 | TCP，目的 port 443，任何位址 | 固定 |
| DNS | UDP 與 TCP，目的 port 53，位址在集合 `nameserver4`／`nameserver6` 裡 | `/etc/resolv.conf`，由 `trustiot-agent-nameservers` 填入（見下方） |

其他封包經一條有 counter 的規則（`counter goto refuse comment "default deny"`）拒絕：TCP 以 reset（`connect` 立刻得到 connection refused），其他協定以 ICMP admin-prohibited（送出本身就以 `EPERM` 失敗）。包括非 DNS 的 UDP、送往清單裡的位址但不是清單裡的 port、UDP 的 443、NTP（agent 不送 NTP；時間由 systemd-timesyncd 以它自己的帳號同步，不受這裡影響）。帶 IP option 或 IPv6 extension header 的封包不論目的地一律拒絕：socket 上的來源路由（`IP_OPTIONS` 的 LSRR、SSRR，`IPV6_RTHDR`）讓 header 的目的地只是第一跳（例如 nameserver 的 53 埠），真正的目的地在路由裡，第一跳若照來源路由轉送，DNS 的放行就成了送往任意主機的 UDP。有沒有 `CAP_NET_RAW` 才能設定，依核心而定（建置主機的核心拒絕沒有 `CAP_NET_RAW` 的 IPv4 來源路由；閘道器的核心沒有實測），所以規則不依賴它；agent 不設任何 IP option 或 extension header。和上面那一層一樣只比對 socket 的擁有者：其他帳號與沒有 socket 的封包不受影響。上面那一層的拒絕照樣有效：送往閘道器本身、廣播、多播與 metadata 的封包，就算目的 port 是 443 或在清單裡，也先在那裡被拒。

**雲端只限 port、不限位址（取捨）。** MQTT 與憑證換發（`src/transport.rs` 的 `connect_tcp`）連的是 `cloud.url` 的主機。不把雲端的位址放進集合的原因：

- 入口在 CDN 後面，位址很多而且會換；agent 每次連線才解析，集合得在每次連線之前、由 agent 送出解析結果之後才填好。這條路和 fieldbus 的清單一樣經 agent，被攻破的 agent 本來就能經 `trustiot-agent-allowlist` 把任意 TCP 目的地加進集合（「fieldbus 放行集合」的威脅模型），所以限位址擋不住被攻破的 agent，只多了一個雲端連不上的原因。
- 雲端連不上時閘道器拿不到新的設定，也無法遠端修復；fieldbus 的集合是空的（例如 `nftables.service` 的 reload 之後、agent 還沒重送之前）時，雲端仍要連得上。
- 企業網路本來就放行 443。

代價：agent 帳號連得到任何位址的 TCP 443（上面那一層拒絕的除外），一個程式或設定的錯誤可能讓它連到別的 443；被攻破的 agent 也有這條路，但它經清單本來就有。`cloud.url` 寫了 443 以外的 port 時（只有測試這樣用）連線被拒。

**DNS：誰、什麼時候更新。** agent 以 glibc 經 `/etc/resolv.conf` 的 nameserver 解析，那些位址會隨 DHCP 或 NetworkManager 改變。放行的位址由 root 這邊讀 root 擁有的 `/etc/resolv.conf` 決定，agent 從不提供：如果由 agent 提供，被攻破的 agent 就能對任意主機的 53 埠送 UDP，「被攻破的 agent 只剩 TCP」就不成立。

- [`src/bin/trustiot-agent-nameservers.rs`](src/bin/trustiot-agent-nameservers.rs)（裝成 `/usr/libexec/trustiot-agent/nameservers`）不收參數，照 glibc 的讀法（`src/nameservers.rs`）取出 nameserver：只認 `nameserver` 開頭的行，IPv4 照 `inet_aton`，IPv6 去掉 `%zone`，IPv4-mapped 的 IPv6 當成 IPv4，最多前 3 個合格的位址（glibc 的 `MAXNS`）。再以 nft 的一個 transaction 清空並填入兩個集合，填完再讀一次檔案，內容變了就再套用（最多 10 次），所以執行中被改的內容也會套用。檔案不在時兩個集合清空（glibc 這時問本機，上面那一層會拒絕），讀不到或超過 64 KiB 時清空並以失敗結束；nft 失敗（例如 table 不在）時集合不變並以失敗結束。每次結果寫進 journal（`journalctl -t trustiot-agent-nameservers`）。
- [`systemd/trustiot-agent-nameservers.path`](systemd/trustiot-agent-nameservers.path) 以 `PathChanged=/etc/resolv.conf` 在檔案改變時（就地寫入，或新檔案 rename 蓋過，NetworkManager 是後者）啟動 [`systemd/trustiot-agent-nameservers.service`](systemd/trustiot-agent-nameservers.service)：oneshot，`DynamicUser`、只有 `CAP_NET_ADMIN`、只開 netlink、唯讀的檔案系統，沙箱和 `trustiot-agent-allowlist@.service` 相同；`StartLimitIntervalSec=0`，NetworkManager 連續寫幾次也不會讓 path unit 失效。
- 會清空集合的載入之後也要填回，所以 agent 的 unit 啟動前（`ExecStartPre=+`，在重新載入規則那一行之後）與 `nftables.service` 的 reload（drop-in 的第二行 `ExecReload=`）都再執行一次同一個執行檔（安裝見下方「給 Laysi/trustiot-v2#866 的接法」）。
- 剩下的空窗：NetworkManager 寫入新檔案到 path unit 套用完之間（通常是毫秒）送往新 nameserver 的查詢被拒，glibc 換下一個 nameserver 或逾時，agent 照常退避重試。更新失敗時 DNS 被拒、agent 解析不到雲端而離線（fail closed），journal 有原因。

**規則檔的寫法：只替換 chain，保留集合（取捨）。** 檔案以 `add` 宣告 table 與四個集合（型別與大小照 `Allowlist::declarations()` 與 `Nameservers::declarations()`，`tests/default_deny.rs` 檢查兩者一致），三條 chain 每次載入時先 `add`（不帶 hook，已存在時不改變它）、再 `delete`、再整個定義，都在同一個 transaction 裡：沒有封包會遇到半套的 chain，集合與其中的元素留著。和 Laysi/trustiot-v2#813 的 table 每次整個重建不同，因為集合的內容只在核心裡：每次重新載入出口規則（`systemctl reload trustiot-agent-egress.service`、agent 每次啟動前、手動 `nft -f`）都清空的話，fieldbus 的連線在 agent 重送之前一律被拒，而 agent 看不出來。代價：檔案不再是 table 的完整內容，之後的版本拿掉或改名某個集合或 chain 時，要在檔案裡刪掉舊的，否則它留到下一次 flush。之後的版本也不能在同一個名稱下改集合的定義（型別、大小、flags）：執行中的閘道器上集合已經存在，`add` 一個定義不同的集合會讓整個載入失敗，出口規則的每一次載入（含 agent 啟動前）都失敗、agent 不啟動，直到下一次 flush。要改就換一個集合名稱，並在檔案裡刪掉舊的。

| 發生的事 | table | fieldbus 集合 | nameserver 集合 |
| --- | --- | --- | --- |
| 重新載入出口規則（egress 的 reload、agent 每次啟動前、手動 `nft -f` 規則檔） | 在，chain 換新 | 保留 | 保留 |
| `nftables.service` 的 start、restart、reload（`/etc/nftables.conf` 的 `flush ruleset` 與規則檔在同一個 transaction） | 重建 | 空的，`trustiot-agent-allowlist` 之後仍回 `ok` | reload 由 drop-in 填回；start、restart 會重啟 agent，由它的 `ExecStartPre=` 填回 |
| 開機 | 由出口規則的 unit 建立 | 空的 | agent 的 `ExecStartPre=` 填回 |
| root 不經 systemd 的 `nft flush ruleset` | 不在（上面那一層的 table 也不在，agent 帳號的出口完全不受限）；`trustiot-agent-allowlist` 與 `trustiot-agent-nameservers` 失敗 | — | — |

最後一列和上面那一層的「執行中被清掉的規則」相同：agent 下一次啟動前重新載入（table 回來、集合是空的，`ExecStartPre=` 填回 nameserver）。

**給 Laysi/trustiot-v2#865 的約定。** 在 `nftables.service` 的 start、restart、reload 與開機之後，fieldbus 集合是空的，元件回 `ok`，agent 從回答看不出來；agent 啟動時本來就先送清單，但 reload 不重啟 agent，要另有觸發（例如設備的連線被 reset 時先重送清單再重試一次，或定期重送）。`flush ruleset` 之後元件回 `failed:`，agent 以退避重試，直到規則回來。Laysi/trustiot-v2#878 的檢查若在 table 不見時重新載入出口規則，兩個 table 由同一個 transaction 一起回來，它要檢查的 table 也包括 `trustiot_agent_allow`。

**給 Laysi/trustiot-v2#866 的接法。** 在那之前映像不裝這幾個檔案（`edge/image/src/pins.ts` 的 `AGENT_SYSTEMD_NOT_INSTALLED`），agent 的出口只有上面那一層的拒絕。`tests/default_deny.rs` 照下面的接法在 systemd 底下測試：

- `systemd/trustiot-agent-egress.nft` 的最後加上 `include "/usr/share/trustiot-agent/trustiot-agent-default-deny.nft"`（絕對路徑），規則檔裝成那個路徑：出口規則的 unit、agent 的 `ExecStartPre=` 與 `nftables.service` 的 reload 三處都在同一個 transaction 裡載入兩個 table，任一個失敗就整個不載入。
- 執行檔裝成 `/usr/libexec/trustiot-agent/nameservers`，兩個 unit 裝在 `/usr/lib/systemd/system/`（path unit 沒有 `[Install]`）。
- agent 的 unit 加上 `Wants=` 與 `After=trustiot-agent-nameservers.path`（第一次查詢之前就在看檔案），以及在重新載入規則那一行之後的 `ExecStartPre=+/usr/libexec/trustiot-agent/nameservers`。這一行失敗時這次啟動失敗、照 `Restart=always` 重試。
- `systemd/nftables-reload.conf` 在現有的 `ExecReload=` 之後加第二行 `ExecReload=/usr/libexec/trustiot-agent/nameservers`。
- 跟著要做的：`scripts/check-gateway.sh` 加上預設拒絕的 table、chain 在，以及 nameserver 集合等於 `/etc/resolv.conf` 的檢查；在閘道器上實測。

#### 這一層擋不到的

- **unix socket**：nftables 只看得到 IP 封包。agent 經 unix socket 請本機服務代它做事時，服務送出的封包屬於服務自己的帳號，不受這些規則限制。這一條改由 agent 的沙箱處理（Laysi/trustiot-v2#826）。2026-10-31 唯讀檢查 lab 的閘道器（`ss -xa`、`ls -l`、`sshd -T`、`systemctl cat`，[測試報告](../../docs/test-reports/2026-10-31-826-agent-sandbox-unix-sockets.md)）的結果與處理如下：
  - **D-Bus 與 avahi：已擋住。** `/run/dbus/system_bus_socket` 與 `/run/avahi-daemon/socket` 所有人可寫。經前者可以使用 avahi 的 D-Bus API，經後者可以直接請 avahi 查詢，兩者都會讓 avahi 在 LAN 上送 mDNS；依 Debian 預設的 D-Bus 設定，可能也能發布 mDNS 記錄。agent 兩者都不用，unit 以 `InaccessiblePaths` 藏住 `/run/dbus` 與 `/run/avahi-daemon` 這兩個目錄，沙箱裡連任一個都得到 `Permission denied`（見「systemd」的測試）。
  - **本機 sshd 的 unix socket：已擋住。** `/run/ssh-unix-local/socket`（所有人可寫）是 systemd 的 ssh generator 為本機 sshd 開的 socket（`sshd-unix-local.socket`，每條連線一個 `sshd -i`）。閘道器的 sshd 從 `.ssh/authorized_keys` 讀公鑰，沒有 `AllowUsers`／`DenyUsers`，`AllowStreamLocalForwarding` 是 yes；agent 帳號的 home 是它可寫的 `/var/lib/trustiot-agent`，shell 是 `/usr/sbin/nologin`（sshd 只看 shell 能不能執行），帳號以 `!*` 鎖住密碼但 `UsePAM yes` 下不擋公鑰。所以 agent 可以在 home 放一把公鑰、經這個 socket 登入，以 `ssh -N` 的 unix socket 轉送，從不在 agent 沙箱裡的 sshd session 連 D-Bus 與 avahi，等於繞過上一項，那個 session 也沒有 agent unit 的 seccomp 等限制（IP 連線仍屬於 agent 的 uid，出口規則照樣擋）。unit 一併藏住 `/run/ssh-unix-local`。直接連閘道器自己位址上的 TCP 22 會被出口規則拒絕，但經外部繞回來的連線擋不到：現場路由器的 port forward 加 hairpin NAT、LAN 上能轉送 TCP 的主機，或從外面連得到的全域 IPv6 位址上的 22（agent 先把公鑰放好，再由外部登入）。這正是下面「本機服務只聽 loopback 或 unix socket」的不變量，而閘道器的 sshd 目前聽在所有介面上。所以只要 sshd 從閘道器以外的位址連得到，sshd 拒絕這個帳號，或帳號的 home 不是 agent 可寫的目錄，就是這一項成立的必要條件，不只是縱深防禦。閘道器映像（Laysi/trustiot-v2#825）的 sshd 設定是 `AllowUsers trustiot-admin`，已經拒絕這個帳號（經 TCP 與 unix socket 都一樣，同一份設定）；還沒換成這個映像的閘道器（例如 2026-10-31 的 lab 閘道器）沒有這一行。以後改 sshd 設定時（登入面強化，Laysi/trustiot-v2#815）要保留對這個帳號的拒絕；要不要保留 sshd 的 unix socket、home 要不要改成 agent 不可寫的目錄，也在那裡決定。
  - **藏住的範圍在 agent 啟動時決定。** 三個 socket 都由 `sockets.target` 裡的 socket unit 建立（`dbus.socket`、`avahi-daemon.socket`、`sshd-unix-local.socket`），一般的 service 在它之後才啟動，所以開機時目錄已經在了。agent 啟動時不存在、之後才建立的目錄（例如 agent 執行中才安裝 avahi-daemon），以及 root 在 agent 執行中刪掉再重建的目錄（核心刪除掛載點時，會一併卸掉其他 mount namespace 掛在上面的 mount；依核心行為推論，沒有實測），都要等 agent 重新啟動才藏得住。socket unit 自己重新啟動不刪目錄，不受影響（測試涵蓋）。
  - **代價：`.local` 名稱。** `/etc/nsswitch.conf` 的 `mdns4_minimal` 經 avahi 的 socket 解析 `.local` 名稱，所以 agent 不再能經 mDNS 解析 `.local`：設備的主機要用 IP 位址，或現場 DNS 伺服器上的名稱。連不上 avahi 時 nss-mdns 回 `UNAVAIL` 而不是 `NOTFOUND`，`[NOTFOUND=return]` 不生效，`.local` 名稱改由現場的 DNS 伺服器查詢：名稱會送到那台伺服器，而它剛好有 `.local` 的區域時仍然解析得到。已經以 `.local` 名稱設定設備的現場，升級後那些設備會連線失敗（解析失敗）；repo 裡沒有這樣的設定或範例。其他名稱 `mdns4_minimal` 本來就不處理，照樣經 DNS 解析（見「systemd」的測試）。這也有好處：同一個 LAN 上任何人都能回答 mDNS 查詢。
  - **其他所有人可連的 socket：不需要擋。** agent 要用的是 sd_notify（`/run/systemd/notify`）與 journal（`/run/systemd/journal/` 的 `socket`、`stdout`、`dev-log`）。其餘的 `/run/systemd/io.systemd.Credentials`、`io.systemd.Hostname`、`io.systemd.ManagedOOM` 與 `/run/systemd/userdb/io.systemd.DynamicUser` 是 systemd 的 varlink 服務，不會代送網路封包。只有 root 能連的（`/run/systemd/private`、`/run/udev/control`、`io.systemd.sysext`、journal 的 varlink）與使用者的 `/run/user/<uid>/`（0700，`ProtectHome` 也會藏住）不在此列。這份清單是 2026-10-31 的閘道器：映像（Laysi/trustiot-v2#809）加上其他所有人可連的 socket（例如 systemd-resolved 的 `io.systemd.Resolve`，它也能送 mDNS 與 LLMNR）時，要重新評估。
  - **abstract namespace：目前沒有可以濫用的。** abstract socket 沒有檔案路徑，`InaccessiblePaths` 擋不到；它屬於 network namespace，`PrivateIPC` 也擋不到。`RestrictAddressFamilies` 只能整個擋掉 `AF_UNIX`（sd_notify 與 journal 要用），`PrivateNetwork` 會切斷 agent 的網路。閘道器上以 `@` 開頭的 unix socket 只有五個，全是 systemd、systemd-logind 與 systemd-timesyncd 連上 D-Bus 的用戶端連線（`ESTAB`，sd-bus 自動綁定的名稱），沒有任何一個在 listen，連過去只會被拒絕。所以 agent 目前無從經 abstract socket 請別的服務代勞。映像加上會聽 abstract socket 的服務時（例如 X11 的 `@/tmp/.X11-unix/X0`），要重新評估；那時可以由 agent 自己以 Landlock 的 `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`（ABI 6，Linux 6.12 起；閘道器的 kernel 是 6.18）禁止連到自己範圍以外的 abstract socket，不影響有路徑的 socket。
- **執行中被清掉的規則**：root 手動執行 `nft flush ruleset` 或 `nft -f /etc/nftables.conf`（不經 systemd）之後，規則就不在了。agent 的 unit 每次啟動前都重新載入規則（Laysi/trustiot-v2#863）；agent 一直執行時，檢查的 timer 在 11 秒內把規則載回並重啟 agent，載入不了就讓 agent 停住；在那之前，agent 的 unit 的 `IPAddressDeny=` 仍拒絕 loopback、metadata、多播與受限廣播位址，閘道器自己的 LAN 與 link-local 位址、子網路廣播則要等檢查（Laysi/trustiot-v2#878，見「systemd」的「執行中被清掉的規則」）。root 刻意停掉檢查或改寫規則，這一層防不了。`scripts/check-gateway.sh` 的 egress 檢查也會發現規則不在。
- **priority 不小於 0 的 NAT output chain**：NAT 在 filter（0）之前改寫目的地，規則看的是改寫後的位址。如果另一個 table 的 `type nat hook output` 用 0 以上的 priority，改寫就發生在規則之後，轉回本機的連線擋不到。主機規則（`/etc/nftables.conf`）的 NAT output chain 要用 `dstnat`（-100）或更小的 priority。

#### 本機服務只聽 loopback 或 unix socket

hairpin NAT 是路由器把閘道器連出的封包轉回閘道器，回來的封包從網卡進來、目的地是閘道器的 LAN 位址，在閘道器的 output 上看起來是連到路由器的外部位址，核心這一層認不出來。所以另一個不變量是：閘道器上的本機服務只聽 loopback 或 unix socket，不聽其他介面，轉回來的連線就找不到服務。[`scripts/check-gateway.sh`](scripts/check-gateway.sh) 檢查這一點，同時確認出口規則的 table 已載入、`/etc/resolv.conf` 的 nameserver 不是規則拒絕的目的地。它只讀（`ss`、`nft list`、`id`、`/etc/resolv.conf`、對 nameserver 位址的 `getent ahosts` 與 `ip route get`），以 root 在閘道器上執行（映像可以裝成 `/usr/libexec/trustiot-agent/check-gateway`）：

```sh
sudo sh check-gateway.sh                                  # 每個例外都要明列
sudo sh check-gateway.sh --allow tcp:22 --allow udp:5353:avahi-daemon
```

- listeners：每個 listening 的 TCP socket 與每個綁定但未連線的 UDP socket，都要在 127.0.0.0/8、`::1` 或 `::ffff:127.0.0.0/104` 上；`0.0.0.0`、`[::]`、`*` 與其他位址都算違反。`--allow PROTO:PORT[:PROCESS]`（PORT 可以是 `*`，PROCESS 是 `ss -p` 顯示的程式名稱）明列例外，每個被接受的例外都印成 `ALLOWED`。PROCESS 比對的是行程自己設定的名稱，任何本機帳號都能冒用，所以它只是稽核用的標示，不是安全邊界。
- egress：`table inet trustiot_agent` 已載入，而且整個 table 就是 `trustiot-agent-egress.nft`：把 `nft list` 的輸出（去掉縮排、空行與 counter 的數字）與腳本裡的清單逐行比對，帳號換成 `trustiot-agent` 現在的 uid。只找到那些規則不夠：output hook 要從帳號的比對跳到它們、前面沒有別的判決，才有保護作用；逐行比對同時抓到少了、多了、換了順序或搬到別的 chain 的規則，以及帳號重建、uid 變了之後沒有重新載入的規則。印出第一行不同的地方。規則檔改了，腳本裡的清單要一起改（`tests/egress.rs` 載入規則檔後要求通過）。
- resolver：`/etc/resolv.conf` 至少有一個 glibc 用得上的 nameserver（只算數字位址，名稱與寫錯的位址 glibc 不理；一個都沒有時 glibc 問 127.0.0.1），而且沒有一個是規則拒絕 agent 連的目的地。位址照 glibc 的讀法換成標準寫法（`getent ahosts`：`127.0.53`、`0:0:0:0:0:0:0:1`、`::ffff:7f00:35` 都是 loopback），再由核心判斷種類（`ip route get`，與規則的 `fib daddr type` 相同），所以閘道器自己網卡上的位址也算本機；另外檢查未指定位址（`0.0.0.0`、`::`）與 metadata 位址。

結束碼：0 全部通過、1 有檢查失敗（印出 `FAIL` 行）、2 參數錯誤或缺少 `ss`、`nft` 等工具。2026-10-31 在 lab 的閘道器上唯讀執行（腳本經 ssh 的標準輸入傳入，沒有寫任何檔案）的結果見 [測試報告](../../docs/test-reports/2026-10-31-813-agent-egress-rules.md)：Raspberry Pi OS 預設的 sshd（`0.0.0.0:22`、`[::]:22`）與 avahi-daemon（mDNS，5353）聽在所有介面，出口規則還沒有安裝；這些由映像（Laysi/trustiot-v2#809）與登入面強化（Laysi/trustiot-v2#815）決定怎麼處理。

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

### RTU over TCP（RS485 轉換器）

設定的 `transport` 是 `rtu_over_tcp` 時，`TcpConnection` 以 RTU 框架經 TCP 連到 RS485 轉換器（Modbus over Serial Line V1.02）：站號、PDU、CRC-16（多項式 0xA001 反射、初值 0xFFFF，低位元組在前）。讀取請求固定 8 bytes：站號、功能碼、起始位址、筆數、CRC。

RTU 沒有交易 ID，也沒有長度欄位；序列線上以 3.5 個字元的靜默結束一個框架，TCP 不帶這段靜默。所以回應的長度由功能碼與 byte 數推出，由 CRC 判斷一段 bytes 是不是框架：

- 功能碼 1–4：byte 數要是一個讀取回得出來的（位元 1–250；暫存器是 2–250 的偶數），否則不是框架；長度是 5 加 byte 數。
- 功能碼 5、6、15、16（同一條匯流排上其他主站的寫入回應）：8 bytes。
- 例外回應（功能碼 0x81 以上）：5 bytes。
- 其他功能碼、不合理的 byte 數或 CRC 錯：不是框架，跳過一個 byte 再試（重新同步），不關閉連線。

哪個回應算數：來自請求的站號、功能碼與 byte 數都符合請求的完整框架（例外回應要是這個功能碼的）。其他的讀進來丟掉，請求繼續等到逾時：

- 站號不符的計入 `unit_mismatches`（轉換器把每台設備的回應送給每條連線，Laysi/trustiot#183）。
- 站號相同但對不上的（功能碼或 byte 數不同）分不出是同一站號較早請求晚到的回應，還是其他主站的，所以不中止請求，只記下來。
- 會回送（echo）請求的轉換器：開頭與送出的請求完全相同的 8 bytes 跳過；收到的還只是請求的前段時先等後段。
- 湊不成框架的 bytes（雜訊、CRC 錯的框架）逐 byte 跳過。看起來像長框架開頭的雜訊不會蓋住它後面已經完整到達的回應：這時往後找一個站號、功能碼、byte 數與 CRC 都符合請求的完整框架，找到就從那裡繼續；條件這麼嚴，雜訊被誤認成回應的機會實際上不存在。

逾時時，期間收到的東西決定回報什麼：收到過該站號但對不上的框架是 `Malformed`（說明對不上的原因）；只收到湊不成框架的 bytes 是 `Malformed`，說明先寫觀察到的事實（「N bytes 湊不成 RTU 框架，也沒有答案」），再列出可能的原因：轉換器與設備的序列埠設定（鮑率、同位元）不同，或匯流排上的雜訊；說明不指定單一原因。這比單純的 `timeout` 多告訴你收到了什麼；其他是 `Timeout`。兩種都不中止輪詢。

兩個請求之間到達的完整框架丟掉（站號不符的一樣計入下一個請求的 `unit_mismatches`），送出下一個請求前清空整個緩衝，與 Modbus TCP 不同：沒有交易 ID，被切斷的框架與回應的開頭分不出來，留著的話，它的後半段接上去可能被讀成一個完整的晚到回應；丟掉之後，後半段到達時當成雜訊跳過。

### 共用連線

`Endpoints::new(connect_timeout)` 管理每個端點的連線；`endpoints.connection(&device)` 對傳輸方式、主機與 port 都相同的設備回傳同一個 `SharedConnection`，第一次用到時建立，到第一次讀取才連線。主機以 `Host::canonical()` 比較並連線：`::ffff:192.0.2.10` 與 `192.0.2.10`、只差大小寫的 DNS 名稱是同一個端點。`SharedConnection` 的 clone 是同一條連線的另一個 handle，每台設備的輪詢用自己的 handle：`reader.poll(&mut connection, &clock, timeout)`。

為什麼：RS485 轉換器把每台設備的回應送給每條 TCP 連線，舊系統每台設備各開一條連線，結果回應落到錯的設備、請求在匯流排上碰撞（Laysi/trustiot#183，[現場硬限制](../../docs/requirements/field-constraints.md)）。一個端點一條連線、一次一個請求，回應到達時只有它回答的那個請求在等；框架再丟掉其他站號的回應。

- Modbus TCP 的設備也一樣，同一組主機與 port 共用一條連線：前面接序列匯流排的 Modbus TCP 閘道器同樣只有一條匯流排，而且可能只接受少數幾條連線；一個端點只用一條連線就不會跟自己搶，同一組主機、port 與站號的兩台設備也不會再在兩條連線上互相干擾。
- 傳輸方式也是鍵的一部分：設定驗證已經拒絕同一個主機與 port 混用兩種傳輸方式（`MixedTransport`），沒經過驗證的設定也不會讓設備被用錯的框架讀取。
- 輪流的單位是請求，不是輪詢：每個請求等到輪到它才取得連線，送出、等到回應、失敗或逾時就交出去。等待是先到先得（tokio 的 `Mutex` 是公平的），同時輪詢的設備依 A1 B1 C1 A2 B2 C2 的順序輪流，一台不回應的設備，每個請求讓其他設備多等一個請求逾時，而不是等它整個輪詢。
- 請求的逾時從送出時算起，不含等輪到的時間。還在等輪到的讀取被丟掉，就放棄它的順位，不會卡住後面的請求。

### 失敗與輪詢

| 狀況 | `ReadError` | 回報的種類（`FailureKind`） | 輪詢 |
| --- | --- | --- | --- |
| 連不上（含連線逾時） | `Connect` | `connect` | 其餘請求不送 |
| 主機只解析成被拒絕的位址（見「連線位址的限制」），或讀不到閘道器的介面位址 | `Connect` | `connect` | 其餘請求不送，沒有開 socket |
| 請求途中連線被關閉或 reset、送出失敗（重送一次之後仍然如此，見下方） | `ConnectionLost` | `connect` | 其餘請求不送，下一次輪詢重連 |
| 串流不是 Modbus TCP（例如講 RTU over TCP 的轉換器被設成 Modbus TCP；只有 Modbus TCP 會這樣，RTU 改為重新同步） | `Protocol` | `invalid_value` | 其餘請求不送，下一次輪詢重連 |
| 等不到符合的回應 | `Timeout` | `timeout` | 繼續下一個請求，連線保留 |
| 例外回應 | `Exception` | `exception`（帶例外碼） | 繼續 |
| 符合的回應與請求對不上（功能碼、byte 數；Modbus TCP） | `Malformed` | `invalid_value` | 繼續 |
| RTU：等到逾時只收到該站號但對不上的框架 | `Malformed`（說明對不上的原因） | `invalid_value` | 繼續，連線保留 |
| RTU：等到逾時只收到湊不成框架的 bytes | `Malformed`（說明列出可能的原因：轉換器與設備的序列埠設定（鮑率、同位元）不同，或匯流排上的雜訊） | `invalid_value` | 繼續，連線保留 |
| 解碼不出可送的值 | — | `invalid_value`（只有該資料點） | 繼續 |

連線中斷算 `connect`：與連不上一樣是 TCP 連線的問題，`timeout` 則是連線正常但設備不回應；連線正常但回來的東西無法解讀算 `invalid_value`。只收到其他站號回應的請求算 `timeout`，那些回應另外計入 `unit_mismatches`，與 `health::FailureCounts` 的定義相同。逾時之後不中止輪詢：有些設備對某些位址不回應，中止會讓後面的資料點一直讀不到；設備整台不回應時一次輪詢要等「請求數 × 逾時」；超過節拍的輪詢跳過它蓋過的節拍，不會累積，連續不回答太久由 watchdog 處理（見「排程與自我恢復」）。

`TcpConnection::new(transport, host, port, connect_timeout)` 依 `transport` 選框架，兩種傳輸方式的連線管理相同。它在第一次讀取時才連線，連線失敗後的下一次讀取重新連線。設備、閘道器或轉換器可能隨時關掉保持中的連線（閒置逾時，或每次回答後就關）：

- 關閉在請求之前已經到達：送出前就換新連線。
- 請求送出後才發現連線已關閉或被 reset：在新連線上重送一次，讀取不會改變設備的狀態；重送也失敗才是 `ConnectionLost`。剛開好的連線上失敗不重送，所以不會一直重試。

呼叫端以自己的逾時丟掉讀取中的 future 也是安全的：請求寫到一半被丟掉的連線，下一次讀取會換掉；已收到的 bytes 留在緩衝。預設逾時是請求 5 秒、連線 10 秒（`DEFAULT_REQUEST_TIMEOUT`、`DEFAULT_CONNECT_TIMEOUT`，F4）。每條連線都開 TCP keepalive（見「排程與自我恢復」）。

### 限制

- 合併讀取會讀到中間沒設定的位址。設備對不存在的位址回例外碼 02 時，整個合併讀取的資料點都失敗；目前沒有自動拆開重試。
- 規定回應要帶請求的站號。回應固定帶其他站號（例如 0 或 255）的 Modbus TCP 設備讀不到，健康報告裡會看到 `unit_mismatch` 與 `timeout`。
- Modbus TCP 只能以站號與交易 ID 認回應。另一個 client（例如現場的 SCADA）經同一台會把回應送給所有連線的閘道器讀同一個站號，而且剛好用到同一個交易 ID、讀的筆數相同時，它的回應會被當成這個請求的；隨機的起始 ID 只降低機率。agent 自己的設備不會這樣互相干擾：同一組主機與 port 的設備共用一條連線、輪流送出請求，設定成同一組主機、port 與站號的兩台設備也一樣（設定驗證目前沒有擋這種設定）。
- RTU 沒有交易 ID：站號、功能碼與 byte 數都與在等的請求相同的回應，就被當成它的回答。所以（一）設備在請求逾時（預設 5 秒）之後才回答，而當時在等的下一個請求剛好是同一個站號、同一個功能碼、同樣的筆數時，這個晚到的回答會被當成下一個請求的值；（二）另一個主站經同一台轉換器以同樣方式讀同一個站號時也一樣。（一）即使 agent 是轉換器上唯一的主站也會發生，目前沒有對策；由測試 `known_limitation_a_late_answer_of_the_same_shape_is_taken_for_the_next_request` 重現。（二）只在 agent 是同一台轉換器上唯一的主站時才不會發生；目前只有 lab 閘道器的 RS485 轉換器有這樣的規定（[milestones](../../docs/roadmap/milestones.md)「展示切片」：agent 開始輪詢 lab 的 RS485 轉換器後，其他程式不再連同一台轉換器），現場的規則還沒有決定。
- 兄弟設備晚到的回答（它的請求已經逾時），計入當時在等的請求的 `unit_mismatches`，那可能是另一台設備的請求，不是晚到的那台。
- 沒有可設定的請求間隔：前一個請求結束就送下一個，序列線上 3.5 字元的靜默由轉換器負責。
- 半開連線（設備不送 FIN 或 RST 就忘了連線）在 `TcpConnection` 只會表現為逾時，連線保留；換掉它的是輪詢迴圈的 watchdog（見「排程與自我恢復」）。
- 主機寫 DNS 名稱時，名稱解析在 blocking 執行緒上進行，連線逾時之後它仍會跑完；現場設備多半以 IP 設定。

## 排程與自我恢復

現場曾有設備靜默停擺 2.5 小時、外觀卻完全正常（Laysi/trustiot#151），也有設備一直連著卻永遠不回應、keepalive 偵測不到（Laysi/trustiot#159、Laysi/trustiot#164；[現場硬限制](../../docs/requirements/field-constraints.md)）。所以每台設備由一個輪詢迴圈負責（[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) F4）：

```rust
let settings = polling::Settings::default(); // 或讀自設定，先 settings.validate()?
let mut endpoints = Endpoints::new(settings.connect_timeout);
let mut connection = endpoints.connection(&device);
polling::run(&reader, polling::poll_period(&device), &mut connection, &clock, &settings, |event| {
    // Event::Polled：一次輪詢的結果（Poll、預定與實際開始時間、跳過的節拍數、watchdog 是否觸發）
    // Event::Waiting：迴圈睡到下一個節拍（連續失敗次數、退避時間）
    ControlFlow::Continue(())
})
.await;
```

迴圈一直跑到 `on_event` 回傳 `ControlFlow::Break`，或呼叫端丟掉這個 future（丟掉是安全的，連線仍可用）。

### 設定

每台設備用同一份 `polling::Settings`。`Settings::validate()` 檢查範圍，錯誤寫出第一個超出範圍的設定名稱、值與範圍。

| 設定 | 預設 | 範圍 | 意思 |
| --- | --- | --- | --- |
| `request_timeout` | 5 秒 | 100 ms–60 秒 | 一個請求等回應的時間（送出起算，不含共用連線等輪到的時間） |
| `connect_timeout` | 10 秒 | 100 ms–60 秒 | 連線的時間，含名稱解析；建立 `Endpoints`（或 `TcpConnection`）時傳入 |
| `watchdog` | 60 秒 | `connect_timeout + 2 × request_timeout`（預設 20 秒）–1 小時 | 設備在開著的連線上連續不回答多久，watchdog 就關掉連線 |
| `backoff_initial` | 1 秒 | 100 ms–60 秒 | 第一次連線失敗後的等待 |
| `backoff_max` | 60 秒 | `backoff_initial`–1 小時 | 連續連線失敗時等待的上限 |

輪詢間隔是每台設備設定的 `poll_interval_ms`（100 ms–1 天，`DesiredConfig` 驗證），`polling::poll_period(&device)` 轉成 `Duration`。TCP keepalive 的參數是固定值，不是設定（見下方）。

### 固定節拍

第 k 次輪詢預定在「迴圈開始 + k × 間隔」，由開始時間算，不由上一次輪詢的結束時間算，所以輪詢花多少時間都不會讓節拍漂移。一次輪詢超過下一個節拍時，它蓋過的節拍跳過、不補做（`Polled::missed` 記跳過幾個），下一次輪詢在它結束後的第一個節拍開始；剛好在節拍上結束就立刻開始下一次。退避期間的節拍也一樣跳過。

### 每次等待都有上限

| 等待 | 上限 |
| --- | --- |
| 連線（含名稱解析） | `connect_timeout` |
| 送出請求、等回應 | `request_timeout`；另外每個請求也受 watchdog 的期限限制 |
| 輪詢之間 | 睡到下一個節拍 |
| 共用連線等輪到 | 前面每台設備的請求各自受上面的上限限制 |

### 重連退避

一次輪詢的連線失敗（連不上、請求途中連線中斷、串流不是 Modbus，或被 watchdog 關掉，即 `ReadError::ends_poll()` 的失敗）之後，下一次輪詢至少等 `backoff_initial`，之後每次再失敗就加倍，最多 `backoff_max`，然後從下一個節拍開始。**只有讀到值的輪詢（至少一個資料點讀到並解碼成功）才把退避歸零**：連線成功不算，例外回應也不算（設備回答了，但沒有值），所有資料點都解碼失敗的回應也不算。所以設備接受連線之後立刻關掉或 reset，agent 不會以輪詢的頻率一直重連。只有逾時、例外或對不上的回應（連線還在）的輪詢不加長也不歸零退避，下一次輪詢照節拍。

### TCP keepalive

每條 Modbus 連線都開 TCP keepalive：閒置 30 秒後開始探測，每 10 秒一次，連續 3 次沒有回應，kernel 就關掉連線（`tcp.rs` 的 `KEEPALIVE_*`）。設備在兩次輪詢之間消失（斷電、拔線、路由器丟掉連線狀態）時，大約一分鐘後連線被判定失效，下一個請求直接開新連線，不必先在死連線上等到逾時。輪詢間隔短於 30 秒時連線不會閒置，探測不會送出，這時靠請求逾時與 watchdog。keepalive 看不出 TCP 還會回 ACK、但設備永遠不回答的情況（Laysi/trustiot#164），那是 watchdog 的工作。

### Watchdog

這是每台設備各自的 watchdog，只關掉該設備的 Modbus 連線；與重啟整個服務的 systemd watchdog（見上方「systemd」）無關。設備一直連著卻不回答時，每個請求都逾時，而逾時後連線保留（見「失敗與輪詢」），輪詢永遠不會自己恢復。每台設備的 watchdog 處理這種情況：

- 計時起點：設備上一次回答之後，第一個沒得到回答的請求送出的時間（`WatchdogFired::silent_since`）。回答是讀到值或例外回應；連線失敗會讓計時歸零（連線已經沒了，由退避處理）。
- 觸發：沒有回答的請求結束時，如果已經從起點過了 `watchdog`，就觸發；還在等的請求最遲在「起點 + `watchdog`」被中止，但一次輪詢開始後的 `connect_timeout + 2 × request_timeout`（一個請求最長能花的時間）內不會中止它，所以輪詢間隔比 watchdog 長的設備每次輪詢至少有一個完整的請求。
- 觸發時：關掉連線（`Disconnect::disconnect`），這個請求以 `ConnectionLost`（`connect`，說明「no answer for N ms: the watchdog closed the connection」）結束，這次輪詢其餘的請求不送；算一次連線失敗，下一次輪詢在退避之後的第一個節拍開新連線。`Polled::watchdog` 記下起點與觸發時間。
- 時間：連續輪詢的設備（例如間隔 1 秒、每次都逾時）在起點之後 `watchdog` 到 `watchdog + connect_timeout + 2 × request_timeout` 之間觸發，再過 `backoff_initial` 加最多一個間隔就在新連線上輪詢。輪詢間隔比 watchdog 長的設備，在起點之後第一個結束時已超過 `watchdog` 的沒有回答的請求觸發，也就是連續兩次輪詢沒有回答時。

`TcpConnection` 與 `SharedConnection` 都實作 `Disconnect`。`SharedConnection` 先等輪到才關，關掉的是整個端點的連線：同一台轉換器後面一台設備壞了而其他設備正常時，壞掉那台的 watchdog 每次觸發都讓其他設備重連一次（觸發之後重新計時，所以兩次觸發至少相隔 `watchdog`）。等輪到的時間也算在沒有回答的時間裡：同一個端點有很多台設備同時不回答時，正常的設備可能等超過 `watchdog` 才輪到，被誤判而關掉連線；要避免，同一個端點的設備數乘上 `request_timeout` 要小於 `watchdog`（預設值下是少於 12 台）。

## fieldbus 放行集合（`trustiot-agent-allowlist`，Laysi/trustiot-v2#864）

依設備清單放行、其餘預設拒絕（Laysi/trustiot-v2#814）要讓 agent 帳號只連得到設定裡的設備，所以 nftables 裡要有一份隨設定改變的「位址 . TCP port」集合。改集合要 `CAP_NET_ADMIN`，agent 沒有、也不該有（見「systemd」），所以由一個獨立的特權元件來改：agent 把完整的清單交給它，它只接受主機與 port 的清單，其他一律整份拒絕。這裡只有元件與它的 unit；預設拒絕的 chain 與 agent 端的串接（什麼時候送、清單從哪裡來）是 Laysi/trustiot-v2#865，下面「給預設拒絕的約定」寫明它要遵守的事。

| 在哪裡 | 內容 |
| --- | --- |
| `src/allowlist.rs` | 清單的格式、上限、拒絕的位址與 nft 指令；agent 與元件共用（`agent::allowlist`） |
| `src/bin/trustiot-agent-allowlist.rs` | 元件：讀一份請求、檢查、交給 nft、回一行 |
| [`systemd/trustiot-agent-allowlist.socket`](systemd/trustiot-agent-allowlist.socket) | 元件的 socket，裝成 `/usr/lib/systemd/system/trustiot-agent-allowlist.socket` |
| [`systemd/trustiot-agent-allowlist@.service`](systemd/trustiot-agent-allowlist@.service) | 每條連線一個 instance 的 unit，裝成 `/usr/lib/systemd/system/trustiot-agent-allowlist@.service` |

### 形式：獨立的執行檔

元件是另一個執行檔 `trustiot-agent-allowlist`，由 agent 的 package 建置（`Cargo.toml` 的第二個 `[[bin]]`：`cargo build -p agent` 同時建出兩個，CI 的 aarch64 建置也就涵蓋它），映像裝成 `/usr/libexec/trustiot-agent/allowlist`（root 擁有，0755）。它和 agent 共用 `src/allowlist.rs`，agent 送出的與元件接受的一定是同一種格式。

- 不做成 `trustiot-agent` 的子命令：那樣 agent 的執行檔就多一個以 `CAP_NET_ADMIN` 執行的模式，審查 agent 時得把它算進去。獨立的執行檔只有一個檔案的程式碼，`tests/secret_inputs.rs` 單獨檢查它：唯一啟動的程式是 nft（絕對路徑、清空環境變數），不讀任何環境變數，不接受任何參數。
- 不做成另一個 crate：格式模組會變成第三個 crate，多一個 workspace 成員與它的 CI 設定。代價是元件與 agent 連結同一個 library，要守住「元件只用 `agent::allowlist`」（`Cargo.toml` 的註解與審查；連結器只留下用到的程式碼）。

### 介面

- **通道**：unix socket `/run/trustiot-agent-allowlist/socket`，由 systemd 監聽（`Accept=yes`）：每條連線啟動一個 `trustiot-agent-allowlist@<序號>-<對方 PID>-<對方 UID>.service`，連線就是它的標準輸入與輸出。`MaxConnections=1`：一次一個 instance，同時來的連線被 systemd 直接關掉、得不到回答，所以清單依 instance 啟動的順序套用，也堆不出多個 instance。
- **一次交換**：agent 連上、寫完整份請求、關掉自己的寫入方向（`shutdown(SHUT_WR)`），讀一行回答。元件讀到 EOF 為止（最多 `MAX_REQUEST_BYTES`，多讀一個 byte 用來分辨剛好最大與超過），整份檢查過才動手。沒有關掉寫入方向的連線會等到 `RuntimeMaxSec=30s` 被 systemd 結束，什麼都不套用。
- **格式**（ASCII，LF 換行）：

  ```text
  trustiot-agent-allowlist v1
  192.0.2.10 502
  2001:db8::10 4196
  end
  ```

  - 第一行一字不差是 `trustiot-agent-allowlist v1`，最後一行是 `end`，之後不能再有任何 byte。沒有 `end` 的請求可能被截斷，整份拒絕。
  - 中間每行一個目的地：IP 位址、一個空格、TCP port。位址要是標準寫法（IPv4 是不帶前導零的十進位；IPv6 是 RFC 5952 的寫法：小寫、最長的零段縮成 `::`；IPv4-mapped 的 `::ffff:a.b.c.d` 要寫成 IPv4），port 是 1–65535、不帶前導零的十進位。同一個目的地只能出現一次。
  - 只有可列印的 ASCII 與 LF：沒有 CR、tab、NUL。沒有名稱、網段、協定、判決或其他任何東西。
  - 拒絕 agent 的位址守門員（`src/modbus/address.rs`）固定拒絕的那幾類：loopback、未指定、多播、`255.255.255.255`、雲端 metadata、IPv4-compatible，以及內嵌這些 IPv4 位址的 NAT64 與 6to4 寫法。閘道器自己的介面位址與網段廣播不在其中（元件不讀介面），由 Laysi/trustiot-v2#813 的規則在核心拒絕（另一個 table，見「權限邊界」）。
  - 上限：4096 個目的地（`MAX_ENTRIES`；一份設定最多 1000 台設備，一個名稱可能解析成多個位址）、188,448 bytes（`MAX_REQUEST_BYTES`，正好是 4096 個最長的目的地）。
- **語意**：每份請求是完整的清單，取代兩個集合的全部內容；空清單清空它們。不是增減：遺失或重送的請求不會留下舊的目的地，最後套用的那一份就是現狀。
- **回答**（一行）與結束碼：
  - `ok <n>`（0）：套用了，集合裡現在正好是這 n 個目的地。
  - `refused: <原因>`（2）：不是一份完整、合格的清單，例如 `refused: line 3: not an IP address in canonical form`。集合不變。
  - `failed: <原因>`（1）：清單合格但 nft 沒有套用：預設拒絕的 table 與集合不存在（還沒載入，或被 `flush ruleset` 清掉），或核心拒絕（例如集合放不下）。集合不變。
  - 沒有回答（連線被關掉）：另一個 instance 正在執行，或元件被結束。集合是否已改變要以下一次的 `ok` 為準，重送即可。
- **集合屬於預設拒絕**：table 與兩個集合由載入預設拒絕的檔案宣告（Laysi/trustiot-v2#865，用 `Allowlist::declarations()`），元件從不建立它們。所以 `ok` 表示清單進了已載入的預設拒絕的集合；table 不在時元件回 `failed:`，agent 會知道，而不是由元件重建一個沒有任何 chain 使用的 table、回一個看似成功的 `ok`。
- **整份或全無**：任何一行有問題就整份拒絕，不套用其中合格的部分。套用是一個 nft transaction（兩個 `flush set`，接著 `add element`），失敗時其中每一步都不生效，包括排在前面的另一個集合的清空。nft 從一個沒有名稱的暫存檔（`O_TMPFILE`，unit 私有的 `/tmp`）讀指令，檔案寫完才啟動 nft：元件在寫到一半時被結束，nft 根本不會執行，不會只拿到一半的指令（例如只有清空）。
- **沒有狀態**：元件不寫任何持久的檔案，集合只在核心裡。重開機、`nft flush ruleset`、`nftables.service` 的 start、restart、reload 或預設拒絕的重新載入之後，table 不見（元件回 `failed:`）或集合是空的（看預設拒絕的寫法），直到 agent 再送一次（見下方約定）。
- 每次結果記在 journal（`journalctl -t trustiot-agent-allowlist`）：套用了幾個（IPv4、IPv6 各幾個）與清單的 SHA-256（`Allowlist::digest`，agent 也可以記下它送出的清單的同一個值）、拒絕的原因與請求的大小、nft 的錯誤；instance 的名稱帶著連線者的 PID 與 UID。journal 不列出個別的目的地（每份最多 4096 個，每次都列會讓 journal 的量放大），當下集合裡有什麼以 `nft list table inet trustiot_agent_allow` 查。
- **不採用的做法**：agent 寫一個檔案、由 path unit 觸發 root 載入。那是非同步的，agent 不知道何時生效、有沒有生效，而且 root 要讀 agent 能寫的目錄（符號連結、檢查與使用之間被換掉）。

### 名稱與位址

元件只收 IP 位址，從不解析名稱：特權行程裡沒有 DNS 與 NSS，也就沒有它們的攻擊面；而且元件與 agent 在不同時間解析同一個名稱可能得到不同的答案（DNS rebinding），集合裡放的會是元件看到的、而不是 agent 實際要連的位址。nft 自己在集合裡寫名稱也不行：名稱只在載入時解析一次。

所以由 agent 解析：位址守門員本來就在每次連線時解析並檢查（見「連線位址的限制」）。清單是設定裡寫成 IP 的設備，加上每個名稱最近一次解析時守門員接受的位址；解析出集合裡沒有的位址時，agent 先送新的清單、等到 `ok` 才連線；不再解析出來的位址隨下一份清單拿掉。

### 權限邊界

- **帳號**：`DynamicUser=yes`，每個 instance 由 systemd 配一個臨時的 UID（`trustiot-agent-allowlist`），不是 root，也不是 agent 的帳號，不擁有任何檔案。
- **capability**：`CapabilityBoundingSet=` 與 `AmbientCapabilities=` 都只有 `CAP_NET_ADMIN`（nft 的 netlink 請求要它），`NoNewPrivileges=yes`。
- **沙箱**：`ProtectSystem=strict`（整個檔案系統唯讀，只有私有的 `/tmp` 可寫）、`PrivateDevices`、`PrivateIPC`、`ProtectProc=invisible`、`ProcSubset=pid`、`ProtectKernel*`、`ProtectClock`、`ProtectHostname`、`RestrictAddressFamilies=AF_NETLINK`（只能開 nft 要的 netlink socket；連線與 journal 的串流是 systemd 開好交給它的，不受這項限制，所以它開不了 unix socket，碰不到 D-Bus 與 avahi，也開不了 IP socket）加上 `IPAddressDeny=any`、`TasksMax=4`、`SystemCallFilter=@system-service` 去掉 `@privileged @resources`、`MemoryDenyWriteExecute` 等，完整的清單見 unit 檔。`systemd-analyze security --offline` 的評分是 1.1（2026-10-31），剩下的是 `CAP_NET_ADMIN` 本身、netlink socket、閘道器的 network 與 user namespace。刻意不設 `PrivateNetwork=` 與 `PrivateUsers=`：集合在閘道器的 network namespace 裡，nft 要以閘道器 user namespace 的 capability 去改它。
- **誰能連**：socket 屬於 root、群組是 agent 帳號的群組（`trustiot-agent.sysusers` 的 `u` 行同時建立同名群組），0660，在 systemd 為 root 建立的 0755 目錄裡。核心在 connect 時檢查檔案權限，所以只有 root 與 agent 帳號連得上；agent 不能換掉 socket，也不能從它控制的路徑連到別的 socket。群組不存在時 socket 啟動失敗。
- **改什麼**：只有 `table inet trustiot_agent_allow` 的兩個集合的元素，`fieldbus4`（`ipv4_addr . inet_service`）與 `fieldbus6`（`ipv6_addr . inet_service`），各 `size 4096`。指令是固定的樣板（`flush set` 與 `add element`），填入的是解析過的位址與 port，不是請求裡的 bytes。它從不建立 table、集合、chain 或規則，不碰其他 table（Laysi/trustiot-v2#813 的 `inet trustiot_agent`、主機的規則），也不碰 input hook。
- **限度**：核心不會把 `CAP_NET_ADMIN` 限制在一個 table，所以「只改這兩個集合」是程式碼的保證（封閉的格式、固定的樣板、測試），不是核心的。沙箱限制的是元件本身出問題時：它仍能改閘道器的網路設定（規則、位址、路由），但不能寫檔案、沒有其他 capability、不能開 netlink 以外的 socket，30 秒內結束。
- **為什麼另開一個 table**：Laysi/trustiot-v2#813 的規則檔每次載入都先刪掉、重建 `inet trustiot_agent`（`nftables.service` 的 reload、`trustiot-agent-egress.service` 的 reload），集合放在那裡就會每次被清空。一個封包要通過每一條 output base chain，所以兩個 table 的規則各自有效。
- **agent 不變**：agent 仍以自己的帳號執行，沒有任何 capability，`tests/systemd_unit.rs` 檢查。

### 威脅模型

假設 agent 被攻破（攻擊者能以 `trustiot-agent` 帳號執行程式碼），也就是元件的輸入不可信：

- **它做得到的**：
  - 把任意 4096 個以內的 TCP 單播目的地放進集合（loopback、未指定、多播、廣播、metadata 除外）。所以預設拒絕擋不住被攻破的 agent 的 TCP 連線：它擋的是沒被攻破的 agent 連到設定以外的地方（設定錯誤、程式或函式庫的錯誤）。被攻破時剩下的限制是：清單只能放 TCP 目的地（其他協定要由預設拒絕擋下，見下方約定，這一點取決於 Laysi/trustiot-v2#865 的寫法）、Laysi/trustiot-v2#813 的拒絕照樣有效（另一個 table）、同時最多 4096 個目的地、每次改變都在 journal 留下數量、清單的 SHA-256 與連線者的 PID、UID（不是個別的目的地）。要連被攻破的 agent 也關住，清單得來自 agent 偽造不了的來源（例如雲端簽章的設定），不在這個 issue 的範圍。
  - 清空集合，或一直佔住唯一的 instance（每次最多 30 秒）：只害到它自己的輪詢。
  - 一直連線，產生 instance 與 journal 紀錄：socket 的 `PollLimitIntervalSec=2s`、`PollLimitBurst=10` 讓監聽的 socket 每 2 秒最多被喚醒 10 次，超過就暫停到這 2 秒結束（連線在 backlog 裡等），不會失敗（低於讓 socket 進入失敗狀態的 `TriggerLimitBurst`，`Accept=yes` 預設每 2 秒 200 次）。所以每秒最多約 5 個 instance 或被丟掉的連線，各自留下幾筆 journal 紀錄（instance 的啟動與結束、元件的結果，或 systemd 的「Too many incoming connections」）；這個量會讓較舊的 journal 被輪替掉、持續寫入閘道器的儲存，是剩下的風險。
- **它做不到的**：
  - 經由請求取得 root 或在元件裡執行程式碼：解析在 safe Rust 裡、格式封閉，nft 收到的是元件自己產生的指令，沒有 shell，請求的 bytes 不會到 nft。
  - 加入規則、判決、其他協定、網段或名稱；動到其他 table（Laysi/trustiot-v2#813 的拒絕、主機的規則）。
  - 讓特權的一方解析名稱、讀寫檔案，或影響其他帳號的流量（只有比對 agent 帳號的規則使用這兩個集合）。
- **清單由 agent 提供，重新填入也是**：元件分不出一份清單來自正常的還是被攻破的 agent，也不必分：被攻破的 agent 隨時能經 socket 送出任何合格的清單（上面第一點）。所以 Laysi/trustiot-v2#863 對出口規則的建議「每次載入規則之後重新填入集合的內容，不能來自 agent 寫得到的地方（例如狀態目錄裡已套用的設定），要是 root 擁有的副本或驗過簽章的設定」，在這個介面下不增加保護：agent 重送它目前的清單，和平常更新集合走同一條路、同一個權限；被攻破的 agent 改了狀態目錄裡的設定再讓自己重啟，得到的不會比直接送一份清單多。root 擁有的副本也一樣，它的內容只能來自 agent 送過的清單；由元件保留一份（例如 `StateDirectory=`）的好處是重新載入之後不必等 agent 就能填回，是可用性，不是安全性。那條建議要有安全上的效果，前提是 agent 完全不提供內容：特權的一方自己讀雲端簽章的設定、驗章、自己解析名稱。那是另一個設計，代價是有 `CAP_NET_ADMIN` 的行程要解析整份設定的格式、管理驗章用的金鑰，並且做 DNS（「名稱與位址」避開的攻擊面；解析的結果仍可能被 LAN 上的人偽造）；要關住被攻破的 agent 時再做，見上面第一點。
- **其他本機帳號**：連不上 socket（root 除外）。
- **租戶的 editor**：本來就能把任何主機與 port 寫進設定，agent 照設定送出清單。元件沒有在設定之外多給任何能力，除了上面被攻破的情況。
- **剩下的風險**：nft 與核心 nftables 自己的錯誤（nft 的輸入只有元件產生的固定格式）；`CAP_NET_ADMIN` 本身範圍很大，元件自己被攻破時能改其他網路設定（由沙箱限制，見上方）。

### 給預設拒絕的約定（Laysi/trustiot-v2#865）

預設拒絕的規則檔（Laysi/trustiot-v2#879）照下面前五條寫好了，內容與選擇見「連線位址的限制」的「預設拒絕」：table、集合與 chain 在 `systemd/trustiot-agent-default-deny.nft`，由出口規則檔 `include`；非 TCP 一律拒絕，DNS 只放行 `/etc/resolv.conf` 的 nameserver，由 `trustiot-agent-nameservers` 更新；雲端以 TCP 443 的規則放行，不必放進清單；寫法選了第二種（只重建 chain）。Laysi/trustiot-v2#865 剩下的是 agent 端：組清單、時機，以及 `nftables.service` 的 reload 之後重送。

- **table、集合與 chain 在同一個檔案、同一個 transaction**：載入預設拒絕的檔案宣告 table 與兩個集合，名稱、型別與大小照 `Allowlist::declarations()`（元件的 `add element` 照這個型別寫；以 `add` 宣告而現有的集合型別不同時，nft 回 `File exists`、整個 transaction 失敗），使用集合的 chain 也在這個 table 裡（集合只能被同一個 table 的規則引用）。元件不建立它們，所以只要集合只由這個檔案宣告，元件回 `ok` 就表示 chain 也在。Laysi/trustiot-v2#863 建議 agent 帳號的出口依賴的每個 table 都放進出口規則檔，由同一個檔案在開機、`nftables.service` 的 reload 與 agent 每次啟動前載入，和這一條相容。
- **非 TCP 與 DNS**：清單只表達 TCP。agent 帳號的其他協定要一律拒絕，否則「被攻破的 agent 只剩 TCP」不成立（`tests/allowlist.rs` 裡的替身只寫了 TCP 的部分）。例外是 DNS：agent 以 glibc 經 `/etc/resolv.conf` 的 nameserver 解析（見「連線位址的限制」），要放行目前 nameserver 的 UDP 與 TCP 53，並決定 nameserver 隨 DHCP 或 NetworkManager 改變時由誰、何時更新這條規則；兩者都要有測試。
- **雲端的連線也要在清單裡**（或另寫規則）：MQTT 與憑證換發經 `src/transport.rs` 的 `connect_tcp` 連 443，不經過位址守門員；「先送清單、等到 `ok` 才連線」同樣適用，CDN 的位址會輪換。
- **組清單**：被位址守門員拒絕的目的地逐筆略過，不要讓整份清單失敗；一個名稱可能解析出很多位址，要有每個名稱的位址上限與固定的截斷規則，免得一個設備把清單推過 4096 而所有設備的更新一起失敗。
- **每次載入之後重新填入**：開機、`nftables.service` 的 start、restart 與 reload（`flush ruleset`；Laysi/trustiot-v2#813 的 drop-in 把出口規則檔放進 reload 的 transaction，reload 不會重啟 agent）、agent 每次啟動前的重新載入（Laysi/trustiot-v2#863），以及 root 手動的 `nft -f`，都會讓集合不見或變空。之後要由 agent 再送一次清單，否則所有設備的連線被拒（fail closed，不會放行）。Laysi/trustiot-v2#865 在兩種寫法之間選一種：
  - 檔案每次整個重建 table（Laysi/trustiot-v2#863 對出口規則檔的建議：檔案就是 table 的完整內容）：每次載入之後集合是空的，元件之後回的仍是 `ok`，agent 從回答看不出集合被清過。agent 啟動時本來就先送（下面的時機），所以 agent 啟動前的載入沒有問題；`nftables.service` 的 reload 與手動載入不會重啟 agent，要另有觸發，例如 `PropagatesReloadTo=` 讓 agent 收到 reload、agent 定期重送，或某個設備的連線被 reset 時先重送再重試一次。
  - 檔案只重建 chain，以 `add` 宣告 table 與集合（不 `delete table`、不 `flush set`）：reload 之後元素還在，但檔案不再是 table 的完整內容，舊版留下的集合或 chain 不會被移除；`flush ruleset` 之後 table 不見，元件回 `failed:`，agent 重試即可。
  - 重新填入的內容就是 agent 目前的清單，由 agent 送；為什麼不必從 root 擁有的來源填回，見威脅模型「清單由 agent 提供，重新填入也是」。`scripts/check-gateway.sh` 可以加上「chain 在」的檢查。
- socket 沒有 `[Install]`：agent 的 unit 以 `Requires=`（或 `Wants=`）與 `After=trustiot-agent-allowlist.socket` 帶起它。映像（Laysi/trustiot-v2#866）安裝執行檔與兩個 unit 檔；在那之前映像不裝它們（`edge/image/src/pins.ts` 的 `AGENT_SYSTEMD_NOT_INSTALLED`）。
- agent 的時機：啟動時在輪詢存下的設定之前；套用新設定時，新加入的設備在輪詢之前、移除的設備在停止輪詢之後；名稱解析出集合裡沒有的位址時在連線之前。沒有 `ok` 就不輪詢新的目的地（預設拒絕也會擋下），以退避重試（至少 1 秒，不要緊密迴圈；socket 每 2 秒最多接受 10 次）。

### 手動安裝

沒有在閘道器上實測（延後到 Laysi/trustiot-v2#865 與映像一起驗證）。檔案先放在 ssh 帳號的暫存目錄，以下在閘道器上執行：

```sh
sudo install -d -o root -g root -m 0755 /usr/libexec/trustiot-agent
sudo install -o root -g root -m 0755 trustiot-agent-allowlist /usr/libexec/trustiot-agent/allowlist
sudo install -o root -g root -m 0644 trustiot-agent-allowlist.socket /usr/lib/systemd/system/trustiot-agent-allowlist.socket
sudo install -o root -g root -m 0644 'trustiot-agent-allowlist@.service' '/usr/lib/systemd/system/trustiot-agent-allowlist@.service'
sudo systemctl daemon-reload
systemd-analyze verify /usr/lib/systemd/system/trustiot-agent-allowlist.socket
sudo systemctl start trustiot-agent-allowlist.socket
stat -c '%U %G %a' /run/trustiot-agent-allowlist/socket       # root trustiot-agent 660
sudo nft list table inet trustiot_agent_allow                  # 預設拒絕（Laysi/trustiot-v2#879）由映像安裝並載入之後才有
journalctl -t trustiot-agent-allowlist
```

`tests/allowlist.rs` 在 Debian 13 的容器裡（`tests/egress.rs` 的同一個映像）測試元件：沒有預設拒絕的 table 時清單失敗、什麼都不建立；載入形狀像預設拒絕的檔案之後，清單改變時集合跟著改變、agent 帳號只連得到清單裡的目的地；各種不合格的請求與 nft 的失敗都讓整個 ruleset 不變、滿額的清單換成另一份滿額的清單；以 systemd 為 PID 1 時只有 agent 帳號連得上、instance 的 UID 不是 0、capability 只有 `CAP_NET_ADMIN`、檔案系統唯讀（見「測試」）。

## 設定下發與套用

雲端把每台閘道器要輪詢的全部設定（`DesiredConfig`，[protocol README](../../crates/protocol/README.md)「設定下發」）以 retained 訊息發在 `down/config`。閘道器端在 `src/desired.rs`（Laysi/trustiot-v2#94，[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) E1）：

- **收到**：link 每次連線都訂閱 `down/config`；每次連線都是 Clean Start，所以 broker 在每次訂閱時重送 retained 的設定，重連後一定再收到一次目前的版本。訂閱被拒或只給 QoS 0 時算連線失敗，link 照退避重試（見「MQTT 用戶端」的「訂閱」），所以「開機不漏設定、取得失敗要重試」由 link 保證：連上就一定有訂閱。`dispatch` 把設定交給 `Inbox`，它只留最新一則，從不等套用；短時間內連續的變更只套用最後一版（合併節流）。
- **版本**：依 protocol README 的表。較新的版本先驗證（`DesiredConfig::decode` 的全部規則）、再規劃每台設備的讀取（`Plan::new`），都通過才套用；任何一步失敗就回報 `rejected` 與 issues（例如 `unknown_model` 在 `devices[0].model`），維持上一版。相同版本不重新套用，再回報一次 `applied`（重連後回報因此會自己補上）；內容與已套用的不同時記警告，仍以先套用的為準。較舊的版本回報 `stale_version`，不會倒退（Laysi/trustiot#139）。解不開的 payload 以 `peek_header` 取得的版本回報 `rejected`（版本與目前的相同時也是，`active_version` 不變；比目前舊的回報 `stale_version`，告訴雲端要超過哪一版）；連版本都讀不到的，以及空 payload（retained 被清除，例如除役），無法回報，只記錄並維持目前的設定：除役的閘道器由撤銷憑證停下，不靠清空設定。
- **原子套用**（`src/desired/fleet.rs`）：會失敗的步驟（驗證與規劃）都在改動任何東西之前，套用本身不會失敗，所以一份設定要嘛全部生效、要嘛完全沒生效。每台設備一個輪詢工作（`polling::run`）。設定與型號都沒變的設備，工作完全不動：節拍、共用連線、退避與 watchdog 的狀態都延續，不會被重連、也不會多輪詢一次，所以別台設備的變更不會讓它的最新值歸零或變成離線（舊系統任何變更都會，見 [legacy-inventory](../../docs/requirements/legacy-inventory.md)）。設定或型號變了的設備換一個新的工作，馬上以新設定輪詢；它的狀態不清（最新值留到下一筆）。移除的設備在 `apply` 回來之前停止，之後不會再被輪詢，並通知 `Sink::removed` 清掉它的狀態；沒有設備再用的端點，連線關閉。這些都在送出回報之前完成。工作只在兩個請求之間停止，不在請求途中：停止先等正在進行的那一個請求拿到回應、失敗或逾時（等待的上限與共用端點上的每一次讀取相同：輪到它的時間加上請求逾時，另有 watchdog），之後不再開始新的請求，才中止工作；所以同一個端點上繼續輪詢的設備，不會拿到一個沒人在等的回應，連線也不必重開。
- **存檔**（`src/desired/store.rs`）：通過驗證的設定在生效之前，原樣（收到的 CBOR）存到 agent 的狀態目錄的 `desired-config.cbor`（0600）。狀態目錄是 systemd 以 `STATE_DIRECTORY` 傳來的 unit 的 `StateDirectory`（`/var/lib/trustiot-agent`，在資料分割區），沒有這個變數時（不經 systemd 手動執行）用同一個預設路徑；不是絕對路徑就拒絕啟動。不開放在設定檔裡改：unit 的沙箱（`ProtectSystem=strict`）只讓 agent 寫自己的 `StateDirectory`，設定檔指到別處就存不了。寫入時先寫暫存檔、fsync、rename 蓋過舊檔、再 fsync 目錄，所以任何時刻斷電，留下的都是完整的舊檔或完整的新檔；殘留的暫存檔不會被讀，下次存檔時覆蓋。存檔失敗（例如磁碟滿）時記錄錯誤、照樣套用與回報 `applied`：輪詢比撐過下次重開重要；下一次收到同一版本（下次重連）時再存一次。
- **開機**：agent 一啟動就讀存下的設定，再驗證一次（壞掉的檔案與壞掉的投遞一樣被拒），通過就馬上開始輪詢，不等連線：沒有網路的現場也以上一版設定採集。連上之後收到雲端的版本，依上面的規則收斂（較新就套用、相同就回報、較舊就回報 `stale_version` 請雲端以更高的版本重發）。讀不到或無效時記錄錯誤，等雲端的設定。
- **回報**：每個答覆是 `up/config-report` 的 `ConfigReport`，以 `Link::reserve` 等佇列位置再送出（QoS 1）。沒連線、或 PUBACK 之前連線斷了，就在下次 `Connected` 時再送；期間有更新的答覆就只送新的。broker 拒絕（例如 0x87）時記錄錯誤，不重送。
- **連線位址**：每台設備的連線都經 `Endpoints::new`，也就是「連線位址的限制」（Laysi/trustiot-v2#653）；經 MQTT 下發的設定碰不到 `exempt_for_tests`。
- **讀值的去處**：每次輪詢的結果交給 `Sink`（`polled`；`removed` 在設備移除時）。執行檔的 `Sink` 是 `live::LiveSink`：租約涵蓋的設備的讀值送到 `up/live`（見「即時值」），每次輪詢再交給上傳的 `Readings`：排定的輪詢（`Polled::regular`）的讀值進入收集視窗（見「遙測上傳與離線緩衝」），只為即時值多做的輪詢不進去；失敗的資料點與錯過的輪詢留給讀取統計（Laysi/trustiot-v2#117），在那之前只在 debug 等級記錄（`polled`，帶 `readings`、`failed`、`missed`）。讀值的時間戳與 `clock_synced` 來自上傳用的同一個 `GatewayClock`（見「時鐘」）。
- **輪詢參數**：每台設備用 `polling::Settings::default()`（請求 5 秒、連線 10 秒、watchdog 60 秒、退避 1–60 秒），還沒有設定鍵。

`tests/desired_config.rs` 對 testkit 的假 Modbus 端點驗證：移除的設備不再被輪詢、狀態被清掉、它專用的端點連線關閉；請求途中被移除的設備等回應到了才停止，共用連線的設備繼續讀到自己的值；其他設備的節拍、連線與讀值不受變更影響；無效設定被拒、回報錯誤並維持上一版；版本規則；斷網重開後以存下的設定輪詢；重開後先以存下的版本輪詢，連上後收到較新的 retained 版本就套用、存檔並回報。「重開」是停掉設定這一側、在同一個狀態目錄重新啟動，與斷電後新行程看到的磁碟內容相同；存檔途中斷電由 `store` 的單元測試以殘留的暫存檔驗證。`tests/binary.rs` 以執行檔驗證訂閱、套用、存檔與回報，以及沒有 broker 時重開、以存下的設定輪詢（設備位址被位址限制拒絕，所以不送任何封包）。輪詢的讀值送上雲端的部分見「遙測上傳與離線緩衝」的開頭。真的斷電重開沒有在閘道器硬體上測。

## 遙測上傳與離線緩衝

讀值變成批次、在閘道器上等到雲端確認、斷線時留在記憶體與資料分割區（[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) G1，[決策 8、9](../../docs/architecture/decisions.md)，Laysi/trustiot-v2#90，2026-10-27）。批次與確認的格式、雲端的規則見 [protocol README](../../crates/protocol/README.md)「遙測與確認」。

- `upload`：收集視窗、組批、串流與序號，以及一次一批的上傳與確認（`upload::pipeline`、`Pipeline::run`）。
- `buffer`：離線緩衝，記憶體與磁碟上的批次排成一個 FIFO；`buffer/disk.rs` 是磁碟上的 segment 檔。
- `clock`：閘道器的時鐘：讀值的時間戳、時鐘是否已校正（`GatewayClock`），以及量批次年齡的時鐘（`AgeClock`）。
- `dispatch`：link 的訂閱（`dispatch::subscriptions()`）與收到的 `down` 訊息的分派（`dispatch::run`）。

執行檔在主 runtime 上跑上傳與分派（`main.rs`），磁碟的 I/O 在 tokio 的 blocking 執行緒上做，不會卡住心跳與 systemd watchdog 的通知。輪詢的讀值這樣進來（`main.rs`）：

```rust
let (handles, pipeline) = upload::pipeline(upload_settings, link.clone(), clock.clone(), pipeline_stop.clone())?;
let (leases, watcher) = live::leases();
let (sink, live_publisher) = live::publisher(link.clone(), watcher.clone(), Arc::new(handles.readings.clone()));
let fleet = Fleet::new(endpoints, poll_settings, Arc::new(clock.clone()), Arc::new(sink)).with_live(watcher);
```

- 設定套用的 `Fleet`（見「設定下發與套用」）每完成一次輪詢就呼叫 `Sink::polled`；`live::LiveSink` 把租約涵蓋的設備的讀值交給即時值（見「即時值」），每次輪詢再交給 `upload::Readings`。`upload::Readings` 實作 `Sink`，把排定的輪詢（`Polled::regular`）的 `poll.readings` 原樣交給 `Readings::push`；只為即時值多做的輪詢不進收集視窗。`Readings::push` 只短暫拿一下鎖，從不等待、不會失敗，所以上傳或磁碟再慢也不會拖住輪詢。
- 讀值以同一個 `GatewayClock` 蓋時間戳（`modbus::Clock`），它也是上傳拿雲端確認的 `server_time` 比對的時鐘、心跳回報的時鐘：雲端的時間說時鐘已校正之後，之後的讀值與批次就是 `clock_synced = true`（見「時鐘」）。
- `dispatch::run` 把 `down/config` 交給設定套用、`down/telemetry-ack` 交給上傳、`down/live-control` 交給即時值的租約，三者在同一個訂閱清單（`dispatch::subscriptions()`）。

`tests/desired_config.rs` 的 `polled_readings_go_up_as_a_batch_and_the_clouds_acknowledgement_releases_it` 照 `main.rs` 的接法（同一個訂閱清單與分派、`Readings` 當 `Sink`、同一個 `GatewayClock`），對 testkit 的假 Modbus 端點驗證：腳本化的雲端送來設定，兩台設備的讀值在下一個上傳間隔成為 `up/telemetry` 的一批，雲端的 `TelemetryAck` 把它從緩衝拿掉，下一輪的讀值是同一個串流的下一個序號、而且 `clock_synced = true`。執行檔本身輪詢不到測試機器上的假端點（位址限制拒絕 loopback 與本機的位址），所以 `tests/binary.rs` 的 `a_batch_in_the_state_directory_goes_up_and_the_clouds_acknowledgement_releases_it` 以留在狀態目錄的緩衝裡的一批驗證執行檔的上傳：連上後原樣送出，雲端的確認經執行檔的分派把它拿掉，segment 檔刪除。

### 收集視窗與批次

- 每 `buffer.upload_interval_s`（預設 10 秒）關閉一次視窗：讀值依設備與資料點分組，一個 series 一個 series 放進 `BatchBuilder`（逐系列組批，資料量約是依到達順序的 1/4，[記憶體測試](../../docs/feasibility/2026-10-13-gateway-memory.md)「建議」）；讀值依到達的順序分成時鐘狀態相同的段落，一段放完才放下一段。批次到 256 KiB（`up/telemetry` 的上限）或時鐘狀態改變就換下一批，所以一批的 `clock_synced` 對它的每一筆都成立、一批只有一段，同一個 series 的讀值也照讀到的順序送出：時鐘狀態在一個視窗裡改變又改回時（未校正、已校正、又未校正），形成三批，不會把兩段未校正的讀值放進同一批（`a_clock_state_that_changes_back_within_a_window_keeps_every_series_in_order`）。
- 視窗的記憶體有上限：估計的大小到 4 MiB（`WINDOW_LIMIT`）就提早關閉；萬一到了 8 MiB（提早關閉還來不及時），之後的讀值丟掉並計數（`window_full`），直到視窗關閉。
- **上傳不通的期間**（link 沒有連線，或緩衝裡等著的批次超過一批；每個上傳間隔檢查一次），視窗改成開了 5 分鐘（`OUTAGE_WINDOW`）之後的那一次檢查才關閉，或估計的大小到約一批的量（批次上限的 2 倍，`OUTAGE_CLOSE_FACTOR`，即 512 KiB）時提早關閉。所以長時間斷線每 5 分鐘形成一批，而不是每個上傳間隔一批，不論 `upload_interval_s` 設多少：30 天的斷線約 8,640 批（照 10 秒的間隔是約 26 萬批），緩衝的索引與恢復連線後追上進度要的往返次數都跟著變少。上傳恢復後，下一次檢查起就回到每 `upload_interval_s` 關閉。代價是斷線期間斷電時，最多遺失約 5 分鐘還沒關閉的視窗（見下方「斷電或被強制結束時最多遺失」）。

### 串流與序號

- 每次行程啟動都用新的隨機 `StreamId`（32 個十六進位字元，取自作業系統的亂數），序號從 1 開始，只存在記憶體。作業系統給不出亂數時不建立上傳的 pipeline，agent 記一筆錯誤後結束、不啟動（`NoRandomness`，`without_randomness_the_pipeline_is_not_created`）：改用時鐘與行程 ID 組成的 ID 在時鐘沒設定、行程 ID 又重複時，會與之前的串流相同，雲端就把新的批次當成重送丟掉。磁碟上的批次帶著自己的串流與序號（它們就是 payload 的 bytes），重啟後原封不動重送；新的讀值用新的串流。
- 理由：雲端的高水位以（閘道器、串流）為單位，序號不大於高水位的批次一律當成重複（[protocol README](../../crates/protocol/README.md)「遙測與確認」）。新的串流不需要在磁碟記住任何東西，所以重啟、斷電、重新安裝或換 SD 卡之後都不會重用送過的序號；記憶體裡遺失的批次也不會讓新的批次被當成重送。要延續同一個串流，每形成一批就得把下一個序號寫進磁碟並 fsync。
- 序號用完（`u64`）就換一個新的串流：同一組亂數後面加上 `-1`、`-2`……，不再取亂數，在同一個行程裡不會重複。

### 緩衝：記憶體與磁碟

一個 FIFO 依形成的順序排著每一批，不論它在記憶體還是磁碟；上傳永遠拿最舊的一批。上傳都及時確認時，什麼都不會寫進磁碟。

| 情況 | 做法 |
| --- | --- |
| 記憶體裡的批次加上新的一批不超過 `buffer.memory_limit_mib`（預設 16 MiB） | 放在記憶體 |
| 否則，磁碟加上新的一批不超過 `buffer.disk_limit_mib`（預設 1 GiB） | 寫進磁碟，每一批寫完就 fsync |
| 兩邊都沒有空間 | 從最舊的開始丟，直到放得下：磁碟能用時丟全部批次裡最舊的，磁碟不能用時（寫入失敗，或 `disk_limit_mib = 0`）丟記憶體裡最舊的；新的一批本身大過上限時丟它自己 |
| 緩衝裡（記憶體與磁碟合計）已經有 65,536 批（`MAX_BATCHES`） | 新的一批放進記憶體或寫進磁碟之後，丟最舊的一批（計數，`too_many_batches`）。新的一批哪裡都放不下（比記憶體上限大、磁碟又不收或寫入失敗）時只丟它自己，不先丟最舊的（`at_the_batch_limit_a_batch_that_cannot_be_stored_drops_no_other`）。這個批數上限讓索引的記憶體有界（見下方「記憶體」）；斷線期間每 5 分鐘一批時是 227 天，比 30 天的年齡上限長很多，所以只在批次很小又很多時才會先到（例如每批都是滿的 256 KiB 時，要磁碟上限超過 16 GiB 才會先到批數上限） |
| 啟動時磁碟上已經有批次（之前的行程留下的） | 不論磁碟上限是多少，讀進來、照順序送出，最多 `MAX_BATCHES` 批：超過的丟掉形成時間最舊的（計數，`too_many_batches`）。磁碟上限只決定新的一批能不能寫進磁碟。上限調小之後磁碟上超過上限的批次也先留著，等記憶體滿、新的一批要用磁碟時，照「兩邊都沒有空間」那一列從最舊的丟（計數，`disk_full`），直到磁碟回到上限以下 |
| 磁碟寫入失敗（ENOSPC、EIO、唯讀的檔案系統）或目錄開不了（包括另一個 agent 正在用它，見「磁碟格式」的鎖） | 記錄錯誤，這一批改照「兩邊都沒有空間」那一列處理（磁碟不能用）；60 秒後（`DISK_RETRY`）再試磁碟 |
| 從形成起超過 30 天（`BUFFER_MAX_AGE`，年齡的算法見下方「時鐘」） | 丟掉 |
| 磁碟上的紀錄送出前讀出來 checksum 不符 | 丟掉，不送 |

丟掉的批次的筆數都計入心跳的 `buffer.dropped_records`，並記警告：依原因（`memory_full`、`disk_full`、`too_many_batches`、`expired`、`corrupt`、`window_full`）合計批數與筆數，最多每 60 秒記一次；磁碟的錯誤同樣最多每 60 秒一筆，其間略過的次數記在 `suppressed`。不算丟棄、不計入 `dropped_records` 的遺失也都有合計：雲端 `rejected` 的批次、`BatchBuilder` 不收的讀值（例如超過上限的文字，不放進批次）與編碼失敗的批次，各自最多每 60 秒記一行，帶著上一行之後的次數、讀值的筆數（`readings`）與其中第一個的細節，停止時記下還沒記的；所以這樣遺失的每一筆讀值都在某一行的 `readings` 裡。緩衝從不 panic，也不卡住輪詢。`memory_limit_mib = 0` 時每一批都直接寫進磁碟。`disk_limit_mib = 0` 時不把任何一批寫進磁碟：之前的行程留在磁碟上的批次照樣送出（目錄不存在時也不建立），但正常停止時記憶體裡的批次無處可寫，會遺失並記錯誤。兩個都是 0 的設定不接受（設定錯誤，agent 不啟動），因為那樣每一批都無處可放。

**記憶體**：記憶體裡的批次（最多 `memory_limit_mib`），加上每一批一個索引項，不論它的 payload 在記憶體還是磁碟。索引項不含 payload，同一個串流的批次共用一份串流 ID，大小有測試限制（`an_index_entry_stays_small`：一項最多 96 bytes），批數又有 `MAX_BATCHES` 的上限，所以不論磁碟上限與上傳間隔，索引最多約 65,536 × 96 bytes = 6 MiB，加上每個串流一份的 ID 與依串流的最舊時間表。啟動時讀磁碟，同時留著的紀錄檔頭最多是 `MAX_BATCHES` 的 1.5 倍（一筆 72 bytes，多出來的邊讀邊丟最舊的），串流 ID 也只留這些紀錄用到的（每次丟掉紀錄時一起清掉沒有紀錄用到的），所以不論目錄裡有什麼、有多少個不同的串流，讀的時候的記憶體都有上限。release build 的量測（`index_memory_of_a_full_batch_limit`，平常略過的測試，執行方式在它的說明裡）：磁碟上有 196,608 批（上限的 3 倍）、每批 267 bytes 時，啟動讀完留下 65,536 批，常駐記憶體比讀之前多 8.0 MiB（每批約 128 bytes），讀的過程中的高峰多 14.5 MiB。最壞的情況是每一筆紀錄都在不同的 64 字元串流上（`recovery_memory_of_records_on_distinct_streams`）：100 萬筆、121 MB 的目錄，讀的過程中的高峰比讀之前多 37.8 MiB（之前留著讀過的每一個串流 ID 時是 263.8 MiB）。

**斷電或被強制結束時最多遺失**：遺失的是記憶體裡的東西，也就是還沒關閉的視窗與記憶體裡的批次。連線正常時，視窗最多是一個 `upload_interval_s` 的讀值，記憶體裡通常只有正在等確認的那一批，所以約是一個上傳間隔多一點（預設約 10 秒多的資料）。上傳不通的期間（例如先斷網、之後才斷電），視窗最多是 5 分鐘（`OUTAGE_WINDOW`）多一個上傳間隔的讀值，另外加上記憶體裡還沒寫進磁碟的批次，最多一個 `memory_limit_mib`（[記憶體測試](../../docs/feasibility/2026-10-13-gateway-memory.md)估算 16 MiB 是 10 台設備約 1 小時、250 台約 2.5 分鐘）；批次的部分要更短就調低記憶體上限（0 表示每一批都直接寫進磁碟），視窗的 5 分鐘不受它影響。磁碟上的批次寫完就 fsync，不會遺失；只寫了一半的紀錄啟動時認得出來，不會送出。

**正常停止**（SIGTERM）時先停止輪詢：等每台設備正在進行的讀取結束，這時完成的輪詢的讀值也進入最後的視窗；最多等 2 秒（`POLLING_STOP_LIMIT`），超過時記一筆警告，之後才結束的輪詢的讀值遺失（`main.rs` 的 `a_stop_lets_the_polls_in_flight_end_and_writes_their_readings_to_disk`）。接著關閉視窗，把記憶體裡的批次依 FIFO 的順序寫進磁碟再結束：每寫滿一個 segment 就 fsync 它，最後再 fsync 一次。停止的路上沒有任何一步會跑太久：一收到停止，例行的整理就不再開始讀磁碟；正在讀的（例如晚開成的磁碟）在下一筆紀錄就停下來，那次開啟算失敗。磁碟之前開不了（或剛才的讀取因停止而中斷）時，不管 60 秒的重試等待，只為了寫入再開一次：列出目錄裡的名稱（新的 segment 不用到任何佔著的編號）與 segment 的大小（算進磁碟上限），只讀編號最大的那個 segment 的紀錄檔頭找出最新的形成時間（同樣受停止的時間限制，用來抬高年齡時鐘的下限，見「時鐘」），其他什麼都不讀、不刪、不釋放；目錄裡原有的批次留給下次啟動讀。寫入只在 pipeline 收到停止後的 5 秒內（`FLUSH_LIMIT`）進行，時間到了剩下的不再寫，之後只剩最後一次 fsync；與連線的停止同時進行，停止輪詢的 2 秒加上這 5 秒與最後的 fsync 都在 `STOP_LIMIT`（10 秒）之內。寫不進去的批次記一筆錯誤（批數、筆數與原因：放不下、寫入或 fsync 失敗、停止的時間用完）；fsync 失敗時，上一次成功的 fsync 之後寫進去的批次也一樣算遺失：它們的紀錄留在檔案裡，下一個行程讀得到完整的就照樣送出。記憶體上限 16 MiB 裝滿 267 bytes 的小批次（62,836 批）時，測試在 debug build 裡不到 1 秒寫完（`a_stop_writes_a_full_memory_of_small_batches_within_its_time`）。所以正常的重啟與更新不遺失資料。連線的執行緒、設定套用的工作（`desired::run`）或上傳的 pipeline 意外結束時也走同樣的停止：還在跑的在 `STOP_LIMIT` 之內停下，記憶體裡的批次照樣寫進磁碟，之後才以結束碼 1 結束（`an_unexpected_end_of_the_connection_still_writes_the_batches_in_memory_to_disk`）。

### 上傳與確認

一次只有一批在路上，依 FIFO 的順序，所以同一個串流的批次依序號遞增送出（序號可以有缺口，不能倒退）。上傳先以 `Link::reserve(Channel::Telemetry)` 等 link 佇列的位置，拿到之後才讀 payload（磁碟上的批次這時才讀進記憶體，並與紀錄的檔頭核對串流與序號）並送出（QoS 1）。PUBACK 只表示 broker 收到了；批次留在緩衝，直到雲端在 `down/telemetry-ack` 送來同一個串流與序號的 `TelemetryAck`：

| 發生的事 | 上傳的做法 |
| --- | --- |
| PUBACK 0x00 或 0x10 | 等雲端的確認，最多 60 秒（`ACK_TIMEOUT`） |
| 確認 `committed`、`duplicate` | 從緩衝拿掉，送下一批 |
| 確認 `rejected` | 從緩衝拿掉，記一筆錯誤帶 `code` 與 `message`（不算丟棄：重送會讓這個串流永遠卡住） |
| 確認 `retry` | 留著，`after_ms` 之後再送同一批（限制在 1 秒到 10 分鐘） |
| 確認的狀態是這個版本不認得的 | 留著，記一筆警告，等到確認的期限到了再送 |
| 別的批次的確認（串流或序號不同） | 不理會（debug 日誌） |
| 這一批的確認在它的送出還沒結束時（PUBACK 之前）就到了 | 先留著，送出結束後照上面處理；時鐘的比對因此有這次送出的時間可比（見「時鐘」） |
| 期限內沒有確認 | 再送同樣的 bytes |
| PUBACK ≥ 0x80（例如 0x87），或大過 broker 的 Maximum Packet Size | 留著，記錯誤，1 秒後再送，每次加倍，最多 5 分鐘；有 PUBACK 成功就歸零。從不因此丟掉批次，最後由 30 天的年齡上限處理 |
| 沒有連線、連線在 PUBACK 或確認之前斷掉、link 停止中 | 等下一條連線（`LinkState::Connected` 而且是新的 session）再送同一批。等了 `ACK_TIMEOUT` 還沒有新的連線、但有連線在線上時（例如失敗被算到了已經換上的那條連線），就在線上的這條送；沒有連線時再等一個 `ACK_TIMEOUT`。所以一批不會在有連線時無限期等下去 |
| 磁碟上的紀錄讀不出來或 checksum 不符 | 丟掉（計數），送下一批 |

確認的期限 60 秒：雲端正常幾秒內就提交並確認，確認遺失的代價是晚一分鐘重送，雲端回 `duplicate`。它遠短於 `DELIVERY_LIMIT`（5 分鐘），link 的每個等待也都有自己的期限。

分派：`dispatch::run` 一收到訊息就取走，`telemetry-ack` 解碼後交給上傳的佇列（`Acks::offer`，最多 8 則，滿了丟最舊的；只有在路上的那一批的確認算數，舊的確認沒有用），從不等待；解不出來的記一筆警告（每 60 秒最多一筆）後丟掉。

### 時鐘

- **時間戳**：讀值的時間是系統時鐘的時間，從不改寫，雲端存的就是閘道器讀到時的時間。agent 也從不設定時鐘：unit 有 `ProtectClock=yes`、沒有任何 capability，seccomp 讓 `@clock` 的系統呼叫回 EPERM。
- **校正的來源**：systemd-timesyncd 的 NTP，經 443 的連線不校時。所以閘道器要能對外連 UDP 123 到映像設定的 NTP 伺服器，閘道器映像的防火牆（Laysi/trustiot-v2#96）要放行。
- **是否已校正**（批次與心跳的 `clock_synced`，同一個來源）：在這個行程第一次把雲端的確認拿來比對之前，systemd-timesyncd 說已校正（`/run/systemd/timesync/synchronized` 存在）就算已校正。之後看最近一次比對：在路上那一批的確認的 `server_time`，要落在「這一批在目前這條連線上第一次被 broker 接受（PUBACK 0x00 或 0x10）的送出之前的本機時間減 2 秒」到「確認到達時的本機時間加 2 秒」之間（`SYNC_MARGIN`）。broker 拒絕的送出（PUBACK 0x80 以上、超過大小上限）、沒送出去的，以及 PUBACK 之前連線就斷了的，都不算：被拒絕的送出到不了雲端，不會有確認回答它，若從它算起，拒絕之後的退避（最長 5 分鐘）會讓快了這麼多的時鐘也落在範圍內（`a_refused_publish_does_not_widen_the_window_of_the_clock_comparison`）。還沒有被接受的送出時（例如確認比 PUBACK 先處理）不比對，狀態不變。落在外面就算沒校正，記一筆警告帶偏差（`offset_ms`，正值是本機時鐘快了），狀態不變時每 10 分鐘再記一次；回到範圍內時記一筆 INFO。從這條連線上第一次被接受的送出算起比較寬鬆：確認可能是同一條連線上重送之前那一次的。但只算目前這條連線：每條連線都是新的 session，確認只會回答同一條連線上送出的那一次，所以之前的連線上送出的時間不放寬範圍。否則一批在斷線前送出、一小時後才在新的連線上確認時，快了一小時的時鐘也會落在範圍內（`a_clock_an_hour_ahead_is_found_off_when_the_ack_answers_a_publish_on_a_new_connection`）。範圍涵蓋整個來回（等 link 的位置、在慢的上行寫完 256 KiB 的一批、broker、雲端的提交、回程），所以偏差小於來回時間加 2 秒的時鐘都可能落在範圍內：確認要在那次被接受的送出之後 10 秒內（`SYNC_ROUND_TRIP_LIMIT`）到達才比對，系統時鐘與單調時鐘量的來回都要在 0 到 10 秒之間；超過的（例如等了很久、確認逾時後重送才回來的）不比對，狀態不變（debug 日誌）。所以算成已校正的時鐘，偏差最多是 10 秒加 2 秒，12 秒（不計雲端自己時鐘的誤差）。雲端正常幾秒內就確認。其間系統時鐘跳動時也不比對：往前跳讓系統時鐘量的來回超過 10 秒，往回跳讓它變成負的，或藏住一段長的來回而比單調時鐘量的短，這時單調時鐘量的超過 10 秒（`only_an_acknowledgement_within_the_round_trip_limit_is_compared`、`a_wall_clock_step_between_the_publish_and_the_ack_is_never_compared`）。快了 20 秒、timesyncd 還沒校正的時鐘，雲端在送出 30 秒後才以正確的時間確認時，`server_time` 落在 34 秒寬的範圍內，但不比對，之後的批次與心跳都還是未校正（`a_slow_acknowledgement_never_judges_a_clock_20_s_ahead_synchronized`）。
- **批次的年齡**（30 天）：從形成時算，形成時間和批次一起寫進磁碟。量年齡的「現在」是 `max(系統時鐘, 下限 + 啟動以來的單調時間)`，下限是啟動時的系統時鐘、磁碟上最新的形成時間（磁碟上任何一筆檔頭通過 checksum 與欄位檢查的紀錄都算，包括 payload 被截斷的、重複的、超過批數上限而丟掉的：檔頭完整就表示年齡時鐘曾走到那裡）與磁碟上存的年齡時鐘的時間（`clock` 檔，見下一項）三者中最大的，而且每讀一次這個時鐘就往上推，讓它永不倒退：它從不回傳比之前回傳過的值加上其間的單調時間更小的值。形成時間也以這個時鐘記下。所以時鐘往回調整從不讓批次變年輕，也不會讓新的批次看起來比舊的舊；往前的校正算成經過的時間，之後時鐘再調回來，這段時間也照樣算數：時鐘錯誤地快了 40 天時形成的批次，形成之後最晚 30 天（真實的時間）就過期，不會因為時鐘改正而多留 40 天（`a_batch_formed_during_a_forward_step_expires_within_thirty_real_days`）。往前調超過 30 天時，調整前形成的批次會立刻過期。這個時鐘的每一次讀取與抬高下限都在同一個鎖裡完成（讀系統時鐘、算出值、記下新的下限），所以它給出去的值依取得鎖的順序遞增：不會有一個在往前跳之前算出的值，在另一個呼叫已經拿到往前跳之後的值以後才交出去。
- **重啟之後**：磁碟上有批次時，例行整理每 60 秒（`CLOCK_SAVE_EVERY`）把年齡時鐘的時間存進緩衝目錄的 `clock`，正常停止時寫完批次也存一次（格式見「磁碟格式」）。所以往前的校正在時鐘調回來、agent 重啟之後也照樣算數：批次寫進磁碟之後，系統時鐘往前調 20 天、又調回去，然後 agent 當掉，即使之後沒有再形成批次，重啟後這一批也是 20 天大，再 10 天就過期（`a_restart_keeps_a_forward_step_the_age_clock_counted_while_batches_were_on_disk`、`a_stop_saves_the_age_clocks_time_with_the_batches_it_writes`）。存的是寫入前一刻從年齡時鐘讀到的時間，不是例行整理開始時拿到的時間：其間上傳讀到的往前跳也算進去（`housekeeping_saves_the_age_clocks_time_as_it_is_when_saving`）。當掉時，年齡時鐘在最後一次存檔之後算進去的都會遺失：通常是不到 60 秒的真實時間，但也包括這段時間裡往前跳又調回的量；而且只在重啟時的系統時鐘比年齡時鐘落後時才會。磁碟上沒有批次時不存（沒有要量年齡的批次）；磁碟上限是 0 時，前一個行程留下的批次還在的期間照樣存。
- **磁碟晚開成時**（啟動時開不了，之後的例行整理或停止時才開成）：下限一樣抬高到磁碟上最新一批的形成時間與存下的年齡時鐘的時間，而開成之前這個行程形成的批次，形成時間加上下限把時鐘往前推的量，所以年齡不變。例如 RTC 遺失了時間、系統時鐘比磁碟上的批次落後 400 天，磁碟在一分鐘後才開成：抬高下限讓時鐘往前 400 天，這一分鐘裡形成的批次照樣是一分鐘大，不會被當成 400 天前的而丟掉（`a_late_disk_open_keeps_the_age_of_batches_formed_before_it`）。它們排在磁碟上原有的批次之後，FIFO 的順序不變。
- **已知的限制**：關機期間時鐘往回跳（例如 RTC 遺失了時間）時，關機的那段時間不算進年齡：下限只能從磁碟上最新的形成時間或存下的年齡時鐘的時間接續，批次最多多留關機的那段時間（當掉時再加上年齡時鐘在最後一次存檔之後算進去的時間）。形成時間與 `clock` 檔的時間是寫在磁碟上的值，agent 照單全收：有寫入權限的人在 0700 的目錄裡放一筆形成時間接近 `u64::MAX` 的紀錄或這樣的 `clock` 檔（checksum 也要對），會把下限抬到那裡：磁碟上原有的批次立刻過期，而時鐘到了 `u64::MAX` 就停住，之後形成的批次不再變老；這需要 agent 使用者或 root 的權限，不另外防範。

### 磁碟格式

緩衝在 agent 的狀態目錄底下的 `buffer/`（`upload::BUFFER_DIRECTORY`）。狀態目錄與存設定的是同一個：systemd 以 `STATE_DIRECTORY` 傳來的 unit 的 `StateDirectory`（`/var/lib/trustiot-agent`，在資料分割區），不在設定檔裡（理由見「設定下發與套用」的存檔）。目錄不存在時以 0700 建立（上一層要已經存在）；已經存在、但其他使用者可以進入時（mode 的 group 或 other 有任何位元），透過以 `lstat` 確認過的同一個目錄的 handle 改成 0700 並記一筆警告，agent 不是擁有者、改不了時也記一筆警告（要讓 agent 的使用者擁有這個目錄，mode 0700）。開啟時對目錄本身取得排他的鎖（`flock(2)`，Rust 的 `File::try_lock`），直到不再使用為止：同一個目錄只有一個 agent 在用，第二個（同一個行程或另一個 agent）開不了，當成磁碟開不了、照常 60 秒後再試，訊息寫明目錄被另一個 agent 使用中。鎖在目錄本身，不多一個鎖檔要建立、檢查或清掉；行程結束（包括當掉）時核心就放開。只有單元測試（`cfg(test)`）在鎖不是被這個行程的緩衝持有時會等它放開（最多 60 秒）：同一個測試行程裡其他測試啟動的子行程，在執行自己的程式之前握有行程每個 fd 的複本，包括剛關掉的緩衝的目錄 handle，負載高時這段時間可能很長（Laysi/trustiot-v2#817）；被緩衝持有的鎖在測試裡也立刻失敗，正式程式從不等待。檔案以 0600 建立；開檔時以 `lstat` 與 `fstat` 確認是同一個一般檔案，不跟隨 symbolic link。

- `seg-<20 位數>.log` 是 segment：16 bytes 的檔頭（`TIOTSEG\0`、格式版本 1，與 4 個保留的 0），之後是一筆筆紀錄。保留的 bytes 不是 0 的檔案不是這個格式寫的：不讀、不刪，記一筆警告。每筆紀錄是檔頭（magic、payload 長度、序號、形成時間、最舊讀值的時間、筆數、串流 ID、payload 的 checksum，以及檔頭自己的 checksum）加上 payload，payload 就是送上 `up/telemetry` 的 bytes。checksum 是 SHA-256 的前 8 bytes（`ring`，不多一個相依）。每個欄位的位移在 `src/buffer/disk.rs`。
- segment 先以 `O_EXCL` 建成新的 `.tmp`（已經有同名的就失敗，不刪也不覆寫它）、寫入檔頭、fsync，再以 hard link（`link(2)`）給它 `.log` 的名稱、刪掉 `.tmp` 的名稱、fsync 目錄。`link(2)` 在 `.log` 的名稱已經存在時不可分割地失敗，從不取代既有的項目，所以檢查與發布之間不會有別的東西被蓋掉；失敗時只刪自己建的 `.tmp`。在 link 與刪掉 `.tmp` 之間當掉時，留下的 `.tmp` 是同一個 segment 的第二個名稱：啟動時刪掉留下來的 `.tmp`（只刪一般檔案；刪不掉時記一筆警告、留著它，它的編號照樣算佔用，目錄照常開啟）只拿掉這個名稱，`.log` 與它的紀錄都不受影響。所以緩衝需要支援 hard link 的檔案系統（資料分割區是 ext4）；不支援時建不了 segment，當成磁碟失敗（60 秒後再試），不會改用會取代既有項目的 `rename`。segment 的編號從啟動時目錄裡所有 `seg-<n>` 名稱（不論是檔案、目錄還是 symbolic link）裡最大的再加一開始，所以啟動時佔著的名稱永遠不會被拿來用；啟動之後才出現、佔著下一個 `.log` 或 `.tmp` 名稱的東西也不覆寫、不刪，那個編號跳過、記一筆警告，同一次寫入改用下一個編號（連續最多跳過 16 個，之後當成磁碟失敗）。最大的編號（`u64::MAX`）不使用，用完就記錯誤、不再建新的 segment。一般檔案以外的東西都不動、不讀也不刪，記一筆警告。一個行程只 append 到自己建的 segment，到目標大小（磁碟上限的 1/16，介於 16 KiB 與 4 MiB 之間）就換一個新的；裡面每一批都確認了才刪掉整個檔案，從不為了拿掉一批重寫檔案。檔案數約是磁碟上限除以目標大小（預設 1 GiB 時約 256 個）；批數則由斷線期間的視窗（30 天約 8,640 批）與 `MAX_BATCHES` 限制，不會因為上傳間隔短而變成幾十萬批。
- `clock` 是年齡時鐘的時間（32 bytes：`TIOTCLK\0`、格式版本 1、4 個保留的 0、毫秒的時間，與前 24 bytes 的 checksum）。每次存檔先刪掉留下來的 `clock.tmp`，以 `O_EXCL` 建新的 `clock.tmp`、寫入、fsync，再以 `rename` 蓋過 `clock`、fsync 目錄：當在任何一步都留下完整的舊檔或新檔。這是緩衝唯一會取代的檔案：它自己的檔名，在它鎖著的目錄裡。不是一般檔案（例如 symbolic link，不跟隨）、大小或格式不對、checksum 不符時不用它，記一筆警告，下次存檔時取代；存檔失敗時記一筆警告，60 秒後再試。啟動時與停止時的開啟都讀它，`clock` 與 `clock.tmp` 不列在「不是 segment 的檔案」的警告裡。
- 啟動時只讀紀錄的檔頭，不解 payload，payload 的 checksum 在送出前讀取時才檢查。檔頭的 checksum 不符或不完整時，從下一個 byte 起找下一個 record magic、而且檔頭的 checksum 也對得上的位置，從那裡繼續讀：只有壞掉的那一筆遺失，略過的 bytes 數記在警告裡（`unreadable_bytes`；壞掉的那一筆有幾筆讀值無從得知）。檔案結尾只寫了一半的一筆（例如斷電時的最後一筆）算成一筆不完整的紀錄並記錄。檔頭不是這個格式的 segment 與其他名稱的檔案都不動，記一筆警告。讀的時候最多留著 `MAX_BATCHES` 的 1.5 倍筆紀錄，超過時丟掉形成時間最舊的（計數，`too_many_batches`），最後留下最新的 `MAX_BATCHES` 筆；全部紀錄都被丟掉的 segment 刪掉。找到的批次依（串流、序號）去掉重複，同一個串流依序號、不同串流依最舊的形成時間排序，排在這個行程的新批次之前。
- 確認的位置不寫進磁碟（SD 卡少寫一次）。所以斷電或被強制結束後，部分已確認的 segment 裡已確認的批次會再送一次，雲端回 `duplicate`，不會寫入兩次；量通常不超過一個 segment。未確認的批次從不略過。

### 緩衝的設定

`[buffer]` 表（[`agent.example.toml`](agent.example.toml)）；緩衝的目錄不是設定的鍵（見「磁碟格式」）：

| 鍵 | 預設 | 範圍 | 意思 |
| --- | --- | --- | --- |
| `memory_limit_mib` | 16 | 0–64 | 記憶體裡的批次合計的上限（[記憶體測試](../../docs/feasibility/2026-10-13-gateway-memory.md)「G1 的記憶體上限」）；0 表示每一批都直接寫進磁碟。不能與 `disk_limit_mib` 同時是 0 |
| `disk_limit_mib` | 1024 | 0–65536 | 新的批次寫進磁碟時，磁碟上的批次合計的上限；0 表示不把新的批次寫進磁碟（之前留下的照樣送出），這時 `memory_limit_mib` 不能是 0。分割區的大小由映像（Laysi/trustiot-v2#95）決定 |
| `upload_interval_s` | 10 | 1–60 | 收集視窗的長度；上傳不通的期間約 5 分鐘（`OUTAGE_WINDOW`，見「收集視窗與批次」） |

### 心跳

心跳的 `buffer` 來自上傳：待確認的筆數（含還沒關閉的視窗）與批數、最舊的待確認讀值的時間（每一批記下自己最舊的讀值，不必解碼）、磁碟上的 bytes、啟動以來丟掉的筆數。剛啟動、磁碟還沒讀完之前是 0。`clock_synced` 與批次用同一個來源（見上方「時鐘」）。

## 即時值

有人開著設備的即時圖表時，雲端以租約要閘道器把這些設備的每次取樣立即送上來（[rebuild-checklist](../../docs/requirements/rebuild-checklist.md) I3，Laysi/trustiot-v2#121，2026-10-30）。訊息格式與雲端的規則見 [protocol README](../../crates/protocol/README.md)「即時值」；雲端的轉送（`up/live` 到瀏覽器）是 Laysi/trustiot-v2#120。閘道器端在 `src/live.rs`：

- **租約**（`Leases`）：`dispatch::run` 把 `down/live-control` 交給 `Leases::deliver`，當場解碼並套用，不等任何東西。`start`、`renew` 建立或取代租約，從收到起 `ttl_s` 秒後結束；`stop` 結束它，不存在的租約不做任何事；不認得的 `op` 與解不開的訊息忽略（後者記警告，每 60 秒最多一筆）。租約何時結束不靠計時器：每個讀租約的地方（輪詢、`LiveSink`）都拿結束時間與當下比較，結束了就不算，所以到期的那一刻起就停止，不論有沒有東西把它拿掉。同時最多保留 `MAX_LIVE_LEASES`（64）個還沒結束的租約，再多的新租約忽略並記警告（每 60 秒最多一筆），已保留的照常 `renew`、`stop`；每個租約最多 64 台設備（protocol 的檢查），所以記憶體有上限。租約只在記憶體裡：重啟後由雲端的 `renew` 恢復；斷線期間租約照常計時，連線不通時取樣直接丟掉（見下方）。
- **取樣**（`polling::run_live`，`Fleet::with_live`）：租約涵蓋某台設備、而且租約的 `interval_ms`（多個租約時取最短的）比設備的 `poll_interval_ms` 短時，這台設備另外以 `interval_ms` 的固定節拍輪詢，從租約開始涵蓋它的那一刻起算，所以第一筆即時值不必等一個間隔；但離這台設備上一次輪詢開始還不到一個間隔時，等到滿一個間隔。所以兩筆即時值的取樣永遠至少相隔一個間隔（在輪詢途中開始的租約也會讓那次輪詢成為即時值），租約快速地開了又停、或一直換間隔（有問題的雲端），也不會讓設備被輪詢得比間隔還快；代價是第一筆即時值最多晚一個間隔。設備原本的節拍不變：每個排定的時間，第一個在那之後結束的輪詢就是那個時間的輪詢（`Polled::regular`），其他都是只為即時值多做的輪詢。所以一個排定的時間只輪詢一次，不會為了遙測與即時值各讀一次（RS485 的匯流排很慢）；排定的輪詢的取樣時間最多晚一個 `interval_ms`；跨過排定時間的即時輪詢就算那個時間的，不會讓它變成錯過。租約有變化（開始、`renew`、`stop`）時，每台設備的輪詢迴圈都被叫醒重新看一次；租約到期時，迴圈在下一個即時節拍醒來，看到租約結束就回到原本的節拍，不再多輪詢。`interval_ms` 不比 `poll_interval_ms` 短時不多輪詢，每次排定的輪詢也是即時值。
- **送出**（`LiveSink`、`Publisher`）：輪詢結束時，設備仍在某個租約涵蓋之下，就把這次的讀值交給 `Publisher` 的佇列（`LIVE_QUEUE`，16 次輪詢）；交接從不等待，佇列滿了就丟掉這次的即時值並記警告（每 60 秒最多一筆）。之後每次輪詢照常交給上傳的 `Readings`，只有排定的輪詢的讀值進收集視窗。`Publisher` 把一次輪詢的讀值依資料點排成 `LiveSamples` 的序列（值不能送的讀值，例如 NaN，與遙測一樣略過），超過 64 KiB 時以 `LiveSamples::split` 拆開，一則一則以 `Link::publish` 發布到 `up/live`（QoS 0，不 retain）：沒有 PUBACK 可等，只等這則寫進連線；沒有連線或 link 停止中時丟掉這次輪詢剩下的訊息，不排隊、不進緩衝、不寫磁碟。一次只送一則，所以即時值在 link 的 QoS 0 佇列裡最多佔一個位置，心跳永遠有位置；上行很慢時，佇列裡等著的輪詢最多 16 次，之後的丟掉。
- **權限**：閘道器只能發布到自己的 `up/live`。broker 的閘道器角色（`protocol::acl::gateway_role_acls`，由 broker 的 init container 依 `tools/migrate` 設定）的規則是 `trustiot/v1/gw/%u/up/live`，`%u` 是憑證的 CN；發到別台閘道器的 `up/live` 得到 PUBACK 0x87（QoS 0 時直接丟掉，雲端收不到），也訂閱不到別台的 `up/live`（`crates/protocol/tests/broker.rs` 的 `the_gateway_role_allows_exactly_the_protocol_channels`）。

取樣頻率與遙測的決定（2026-10-30，理由見 [protocol README](../../crates/protocol/README.md)「即時值」）：租約期間的取樣間隔是 `interval_ms` 與 `poll_interval_ms` 中較短的；只因租約多出來的取樣只送 `up/live`、不進一般上傳，遙測仍是設定的間隔。

已知的限制：多做的輪詢與其他設備共用同一個端點的連線（例如同一台 RS485 轉換器），匯流排忙不過來時，同一個端點上其他設備的輪詢會晚、甚至錯過排定的時間（照「固定節拍」略過，計入 `missed`）。每租戶同時開啟的即時畫面數與每個畫面的設備數由雲端限制（清單 I3）。

`tests/live.rs` 照 `main.rs` 的接法（同一個訂閱清單與分派、`LiveSink` 包著 `Readings` 當 `Sink`、`Fleet::with_live`），對腳本化的雲端與 testkit 的假 Modbus 端點、停住的時鐘驗證：雲端開了租約，閘道器在時鐘沒有前進的情況下就輪詢並把值送到 `up/live`（QoS 0、沒有 packet ID），緩衝裡的筆數不變；租約期間每秒一筆；`ttl_s` 到期之後沒有任何輪詢與 `up/live`，下一次輪詢是原本排定的那次，之後的遙測批次只有排定的輪詢的讀值；`renew` 把結束延後，`stop` 立刻結束。單元測試（`src/live.rs`、`src/polling.rs`）驗證租約的上限、到期、最短間隔、解不開的訊息，以及租約期間的節拍：每秒的排定輪詢一次也不少、即時輪詢每 200 ms 一次、租約結束後回到原本的節拍、跨過排定時間的即時輪詢算成排定的那次、租約每 10 ms 開關或換間隔時多做的輪詢彼此仍至少相隔 100 ms。

## TPM 簽章

閘道器的身分金鑰在 TPM 裡、不能匯出（決策 12）。`TpmKey::open(TpmTransport::Device("/dev/tpmrm0".into()), handle)` 經核心的資源管理器使用持久 handle 上的金鑰（預設 `DEFAULT_KEY_HANDLE` = `0x81000100`），`TpmKey` 實作 rustls 的 `SigningKey`，inner mTLS 的 CertificateVerify 就在 TPM 內簽。

- 不用 tss-esapi 或 libtss2：只送兩個指令，TPM2_ReadPublic 與 TPM2_Sign，依 TPM 2.0 Part 2、3 自己編解碼，沒有 C 函式庫要交叉編譯或隨附。
- 回應一律當成不可信的輸入：檢查 tag、長度與回應碼，每個長度都不超過剩下的 bytes，結構要剛好結束，回應最多 4096 bytes；格式不符是錯誤，不會 panic。回應碼解成名稱，例如 `TPM_RC_HANDLE (0x0000018b, handle 1)`。
- `open` 先讀公開區域，金鑰必須是 ECC NIST P-256、scheme 固定為 ECDSA-SHA256，屬性要有 `fixedTPM`、`fixedParent`、`sensitiveDataOrigin`、`userWithAuth`、`sign`，不能有 `restricted`、`decrypt`（2026-10-12 PoC 的範本，產線工具 Laysi/trustiot-v2#80 照這個產生）。接著簽一段固定的測試訊息並以公鑰驗證，簽不出來（例如金鑰設了密碼）就在啟動時失敗，不會等到每次握手才失敗。啟動多一次簽章，閘道器上約 70 ms。
- 簽章：SHA-256 在軟體算，以空密碼的 password session（`TPM_RS_PW`）與 null hashcheck ticket 送 TPM2_Sign，r、s 轉成 DER。rustls 只會拿到 `ecdsa_secp256r1_sha256`。
- 憑證要與金鑰相符：`public_key_spki_der()` 是金鑰的 SubjectPublicKeyInfo，rustls 的 `CertifiedKey::keys_match` 經 `SigningKey::public_key` 比對。
- 一條專用執行緒（`tpm`）擁有裝置，請求經有上限的佇列（8 個）一個一個送，佇列滿了回 `Busy`。rustls 的 `Signer::sign` 是同步呼叫，等待期間佔住呼叫它的執行緒（閘道器上約 70 ms），所以握手不能跑在負責 Modbus 輪詢的執行緒上。等待最多 `RESPONSE_TIMEOUT`（10 秒，排隊也算在內；核心對這些指令的逾時更短）：簽章佔住連線執行緒，這段時間加一次進度計數要小於 systemd watchdog 通知的間隔（15 秒），否則 systemd 會在 agent 自己放棄之前重啟它（見上方 systemd）。執行緒停了（panic）時簽章立即回錯誤，不會卡住；最後一個 `TpmKey` 與 `Signer` 被丟掉時執行緒結束並關閉裝置。
- `TpmTransport::Channel` 接受其他 `TpmChannel`（測試以 TCP 連 swtpm）；正式環境只用 `Device`。

### 金鑰佈建（產線工具用）

`tpm::ensure_key(transport, handle)` 讓持久 handle 上有 agent 要用的金鑰（決策 12 的 (5)，Laysi/trustiot-v2#611），產線工具的 `provision` 子命令（Laysi/trustiot-v2#613，見「出廠佈建」）呼叫它；`tpm::inspect_key(transport, handle)` 只看：handle 是空的回 `None`，有金鑰時做與沿用相同的檢查，從不建立、持久化或 evict 任何東西。兩者與 `endorsement_key` 都只接受核心資源管理器的裝置 `/dev/tpmrm<N>`（`tpm::is_resource_manager`），其他路徑（例如 `/dev/tpm0`）在開啟之前就回 `TpmError::NotResourceManager`；名稱對的路徑也要核心認得是資源管理器的裝置：開啟前讀 `/sys/dev/char/<major>:<minor>`，連到 `tpmrm/tpmrm<N>` 才開，開啟後比對裝置號碼沒變才送命令，否則回 `TpmError::NotResourceManagerDevice`（指向 `/dev/tpm0` 的連結或同裝置號碼的節點因此在開啟前被拒絕）：行程死在 Load 與 EvictControl 之間、或 CreatePrimary 的回應遺失時，暫時物件只有資源管理器會在行程結束時清掉；直接用 `/dev/tpm0` 時它們會留在 TPM 直到重置，SLB9670 的暫時物件位置很少，之後會得到 `TPM_RC_OBJECT_MEMORY`。agent 自己的 `TpmKey::open` 不受這個限制（設定的 `identity.key.device` 照舊）：它只讀與簽章，不建立暫時物件。agent 本身在閘道器上不呼叫：它只以 `TpmKey::open` 使用已經佈建好的金鑰。回傳的 `Provisioned` 有金鑰（`TpmKey`，可以簽 CSR）與 `created`。

- **沿用**：handle 上的物件與決策 12 的範本在 `unique` 以外的每個欄位都相同（名稱演算法 SHA-256、屬性剛好 `0x00040072`、沒有 authPolicy），並通過自測簽章，就是這把金鑰，`created = false`。這比 `TpmKey::open` 嚴格：open 不看名稱演算法、policy 與額外的屬性 bit，理由見決策 12。TPM 的狀態在重刷 SD 卡後仍在，重跑就走這條，得到同一個公鑰、不多一個持久 handle。
- **建立**：handle 是空的（ReadPublic 回 `TPM_RC_HANDLE`）時，以 TPM2_Create 在 SRK 下建立同範本的金鑰、TPM2_Load、以 TPM2_EvictControl 持久化，再讀回確認公開區域與建立時相同、通過同樣的檢查，`created = true`。載入的暫時物件用完就 FlushContext。
- **拒絕**：handle 上是其他任何物件（RSA、restricted、P-384、儲存金鑰、有密碼的金鑰、不能簽章的金鑰，以及名稱演算法不同、帶 policy 或多了 `noDA` 等屬性的簽章金鑰）回 `ProvisionError::Refused`，只送過 TPM2_ReadPublic 與最多一次自測簽章，**絕不 evict、不改**。範本不同的物件只讀不簽；範本相同但有密碼的金鑰要靠自測簽章的授權失敗分辨，沒有 `noDA` 的這種金鑰會讓 TPM 的字典攻擊計數加一（agent 啟動時也一樣）；其他狀態不變。
- **SRK**：`0x81000001`（`SRK_HANDLE`）是標準的 ECC SRK（tpm2-tools 預設範本 `tpm2_createprimary -C o -g sha256 -G ecc256:aes128cfb`，比對 `unique` 以外的每個欄位）就以它為父金鑰；否則在 owner hierarchy 以同範本建一個暫時的 primary，Load 之後就 flush。工具從不寫入 `0x81000001`（那個 handle 歸 LUKS 那一側，Laysi/trustiot-v2#95）。
- **中斷與同時執行**：Create 與 EvictControl 之間行程死掉時，載入的金鑰是暫時物件，核心的資源管理器在行程關閉 `/dev/tpmrm0` 時清掉它，什麼都沒持久化，下次重跑建一把新的。兩個行程同時建立時，後到的 EvictControl 得到 `TPM_RC_NV_DEFINED`，它 flush 自己的金鑰、讀回先到的那把，通過檢查就沿用。
- handle 必須在 owner 的持久範圍（`0x81000000`–`0x817fffff`）而且不是 `SRK_HANDLE`，否則在送出任何指令前回 `UnsupportedHandle`。
- TPM 回 `TPM_RC_RETRY`、`TPM_RC_YIELDED` 或 `TPM_RC_TESTING`（指令沒有執行）時，工作執行緒重送同一個指令，最多 8 次、間隔從 10 ms 加倍到 500 ms（swtpm 對 TPM2_Create 回過 `TPM_RC_RETRY`）；`ensure_key` 回傳的金鑰簽章時也這樣重送。agent 的 `TpmKey::open` 不重送，行為與之前相同。每個指令最多等 `PROVISION_RESPONSE_TIMEOUT`（30 秒，重送與排隊都算在內）。
- 指令與回應的編解碼在 `src/tpm/marshal.rs`，與 ReadPublic、Sign 一樣嚴格檢查每個長度與結構；單元測試以 swtpm 錄下的整段交換（`src/tpm/fixtures.rs`）逐 byte 比對指令並解析回應。

### 背書金鑰（EK，產線工具用）

`tpm::endorsement_key(transport)` 算出 TPM 的 ECC NIST P-256 背書金鑰（EK），並與 TPM 裡的 EK 憑證比對（決策 12 的 (6)，Laysi/trustiot-v2#612），產線工具的 `provision` 子命令（Laysi/trustiot-v2#613）把結果寫進報告。回傳的 `Endorsement` 有 EK 的 SubjectPublicKeyInfo（DER，`public_key_spki_der`）與 `certificate`：`EkCertificate::Matches(der)`（憑證是這把 EK 的）或 `EkCertificate::Absent`（沒有 EK 憑證）。

- **EK**：在 endorsement 階層以 TCG EK Credential Profile 預設的 ECC P-256 範本 TPM2_CreatePrimary（範本 L-2：AES-128-CFB 的 restricted decrypt 金鑰、名稱演算法 SHA-256、屬性 `0x000300b2`、policy 為 `PolicyA`、`unique` 是兩個 32 bytes 的零，與 `tpm2_createek -G ecc` 相同），以 TPM2_ReadPublic 讀回後 FlushContext。不持久化，也不送任何 RSA 指令（SLB9670 做 RSA 會逾時）。primary 由背書種子與範本決定，重跑、換連線都得到同一個 EK。
- **檢查**：回傳的公開區域在 `unique` 之前與範本逐 byte 相同、名稱是它的 SHA-256、ReadPublic 讀到的與 CreatePrimary 回傳的相同、點在 P-256 上，否則是錯誤。
- **只支援預設範本**：TPM 在 NV 定義了 EK 範本（`0x01C0000C`）或 EK nonce（`0x01C0000B`）時，依規範 EK 要由它們產生，預設範本算出的不會是製造商憑證上的那把；這時在建立任何東西之前回 `EndorsementError::CustomTemplate`。
- **EK 憑證**：NV `0x01C0000A`（`EK_CERTIFICATE_INDEX`）有定義且寫入過時，以每次 512 bytes（`NV_READ_CHUNK`）的 TPM2_NV_Read 讀出，最多 4096 bytes：index 有 `ownerRead`（TCG 規範的屬性）時以 owner 的空授權讀（owner 不受字典攻擊鎖定影響），否則有 `authRead` 且有 `noDA` 時以 index 本身的空授權讀。只有 `authRead`、沒有 `noDA` 的 index 在送出任何授權之前就回 `CertificateIndex`：index 設了密碼時，空密碼的 NV_Read 會讓字典攻擊計數加一。內容必須剛好是一張 DER 的 X.509 憑證（後面不能有多餘的 bytes），憑證的 SubjectPublicKeyInfo 與 EK 的逐 byte 相同（`id-ecPublicKey`、`prime256v1`、未壓縮的點）才是 `Matches`；別的金鑰、或同一把金鑰的其他編碼（例如壓縮的點）回 `CertificateMismatch`，解析不了回 `CertificateInvalid`，兩者都帶讀到的 bytes。index 不存在或沒有寫入過是 `Absent`。
- **`Matches` 的意思只有一件事**：憑證的 SubjectPublicKeyInfo 與 EK 的公鑰位元組相同。不驗證憑證的簽章與到製造商根的憑證鏈，所以它不證明 TPM 是真品：拿到 platform 授權的人可以改寫 `0x01C0000A`（任何人簽的、甚至自簽的憑證，只要公鑰是 EK 的就相符），假的 TPM 或匯流排上的中間人也能回報別台設備的 EK 和它真正的憑證。信任模型見 [`services/certs/README.md`](../../services/certs/README.md#規則與取捨) 的「EK 只記錄、比對，不驗證」。
- **endorsement 授權**：只支援空的授權（TPM 的預設）。設了授權時 CreatePrimary 授權失敗（`TPM_RC_BAD_AUTH`），回 `EndorsementError::EndorsementAuthorization`；階層的授權不計入字典攻擊計數，TPM 的狀態不變。
- EK 從 CreatePrimary 到 FlushContext 之間是暫時物件；不論檢查結果都會 flush，行程在中間死掉時由核心的資源管理器在關閉 `/dev/tpmrm0` 時清掉，所以只能經資源管理器使用（Laysi/trustiot-v2#613 拒絕其他裝置）。CreatePrimary 的回應格式錯誤或逾時時不知道 handle，EK 不會被 flush，同樣靠資源管理器清掉。TPM 的重試警告與逾時與金鑰佈建相同（`PROVISION_RESPONSE_TIMEOUT`）。
- TPM2_NV_ReadPublic 與 NV_Read 的編解碼在 `src/tpm/marshal.rs`，與其他回應一樣嚴格檢查；單元測試以 swtpm 錄下的交換（`src/tpm/fixtures.rs`）逐 byte 比對指令並解析回應。

## 出廠佈建（`provision` 子命令）

`trustiot-agent provision`（`src/provision.rs`，決策 12 的 (7)，Laysi/trustiot-v2#613）是出廠登錄的閘道器端：產線工作站（`factory/workstation`，Laysi/trustiot-v2#616）經 ssh 執行它、讀 stdout 的報告，再自己呼叫登錄 API。這個子命令沒有網路程式碼、不寫任何檔案，只和 TPM 溝通。

```text
trustiot-agent provision --hardware-id <id> [--handle <hex>] [--device <path>] [--inspect]
```

| 參數 | 內容 |
| --- | --- |
| `--hardware-id <id>` | 必填。閘道器的硬體 ID，必須是合法的 `GatewayId`（1–64 個 ASCII 英數字、`.`、`_`、`-`，英數字開頭），成為 CSR 的 CN |
| `--handle <hex>` | 金鑰的持久 handle，預設 `0x81000100`。`0x` 加 1–8 個十六進位數字，必須在 owner 的持久範圍（`0x81000000`–`0x817fffff`）而且不是 SRK 的 `0x81000001` |
| `--device <path>` | TPM 裝置，預設 `/dev/tpmrm0`。只接受 `/dev/tpmrm` 加 1–5 位數字，其他路徑在開啟之前就拒絕；名稱相符時再以 sysfs（`/sys/dev/char/<major>:<minor>` 連到 `tpmrm/tpmrm<N>`）確認核心認得它是資源管理器的裝置，不是的話（例如指向 `/dev/tpm0` 的連結）也在開啟前拒絕（兩者都是結束碼 4） |
| `--inspect` | 唯讀：金鑰不存在就回報 `absent`，不建立、不持久化任何東西 |

參數只從 argv 來，不讀 stdin、不經 shell；每個參數只能給一次。流程：

1. **EK**：`tpm::endorsement_key` 算出 EK 並比對 EK 憑證（見「背書金鑰」）。先做 EK：EK 憑證不符的 TPM 不會多一把金鑰。
2. **金鑰**：`tpm::ensure_key` 沿用、建立或拒絕（見「金鑰佈建」）；`--inspect` 改用 `tpm::inspect_key`。
3. **CSR**：以 rcgen 組出主體只有 `CN=<hardware-id>` 的 PKCS #10 CSR，經 rcgen 的 `SigningKey` 由 TPM 內的金鑰簽章（ecdsa-with-SHA256）。rcgen 不開任何 feature，不編進它的加密實作與產生金鑰的程式碼。輸出前在本地檢查：解析得了、後面沒有多餘的 bytes、簽章演算法是 ecdsa-with-SHA256、SubjectPublicKeyInfo 與金鑰的逐 byte 相同、主體剛好是那個 CN，而且簽章以金鑰的公鑰驗得過（ring）；任何一項不符就以結束碼 1 結束。`--inspect` 而金鑰存在時也簽 CSR（簽章不改變 TPM 的狀態）。
4. **報告**：全部成功才在 stdout 印一個 JSON 物件（`protocol::provisioning::ProvisioningReport`，欄位見 [crates/protocol](../../crates/protocol/README.md)「出廠佈建報告」）。報告只有公開資料：金鑰與 EK 的公鑰、CSR、EK 憑證；TPM 的私鑰本來就讀不出來。欄位名稱與值不用 verified、trusted 這類字眼，EK 憑證相符寫成 `for_this_key`，意思只有公鑰相同（見「背書金鑰」）。

診斷（日誌與失敗原因）只寫 stderr，日誌等級同樣由 `TRUSTIOT_AGENT_LOG` 設定。結束碼（`provision::Exit`）：

| 結束碼 | 意思 |
| --- | --- |
| 0 | 成功，報告印在 stdout |
| 1 | 其他失敗：TPM 拒絕指令、回應格式錯誤、逾時、裝置無權限開啟、CSR 檢查不過 |
| 2 | 參數錯誤：不認得或重複的參數、不合格的硬體 ID 或 handle |
| 3 | 沒有 TPM 裝置（裝置不存在，或驅動程式後面沒有 TPM） |
| 4 | 不是 `/dev/tpmrm<N>`，或核心不認得它是資源管理器的裝置：沒有送出任何命令（名稱不符或 sysfs 不符時裝置沒有被開啟） |
| 5 | handle 上是其他物件，原封不動 |
| 6 | EK 憑證不是這把 EK 的、不是一張憑證，或讀不出來 |
| 7 | 不支援的 EK 設定：NV 裡有 EK 範本或 nonce、endorsement 階層設了授權 |

結束碼不是 0 時 agent 不會開始寫 stdout，但寫到一半失敗（結束碼 1）時可能已經寫出部分 JSON，所以讀取端先看結束碼，不是 0 就丟掉 stdout。`--help` 只能單獨使用（與其他參數一起給是結束碼 2），印說明、結束碼 0：結束碼 0 本身不代表有報告，成功是結束碼 0 而且 stdout 解析得成報告。報告不超過 `MAX_REPORT_BYTES`（16 KiB），超過就以結束碼 1 結束、不印。讀取端的完整責任（硬體 ID 用自己的、自己檢查 CSR 等）見 [crates/protocol](../../crates/protocol/README.md)「讀取端的責任」。

開啟裝置時帶 `O_NOCTTY`，路徑即使通到終端機也不會成為控制終端。

## 安裝憑證鏈與根憑證（`provision install` 子命令）

登錄成功之後，產線工作站（`factory/workstation`，Laysi/trustiot-v2#617）經 ssh 執行 `trustiot-agent provision install`（`src/provision/install.rs`），在 stdin 送來登錄 API 回的憑證鏈與根憑證，回應帶根 CRL 時再加上根 CRL（Laysi/trustiot-v2#832）。送過來的只有這些公開資料：閘道器憑證、簽發 CA、根，與根 CRL；工作站憑證、私鑰與 session cookie 不會到閘道器。

```text
trustiot-agent provision install --hardware-id <id>
```

| 參數 | 內容 |
| --- | --- |
| `--hardware-id <id>` | 必填。閘道器的硬體 ID（合法的 `GatewayId`），憑證的 CN 必須是它 |

**先檢查，全部通過才寫入**：

1. 設定檔固定是 `/etc/trustiot/agent.toml`（沒有 `--config`），檔案與它的目錄都要屬於 root、只有 root 能寫（沒有群組與其他人的寫入位元），內容與啟動時同一套檢查，`identity.key` 必須是 TPM。`identity.certificate` 與 `broker.ca` 的目錄都要已經存在（agent 的 unit 的 `StateDirectory` 與 `ConfigurationDirectory`），而且只有擁有者能寫：`identity.certificate` 的目錄屬於 `trustiot-agent`（換發要寫它），`broker.ca` 的目錄屬於 root。每個路徑都從 `/` 一層一層開（`O_DIRECTORY`、`O_NOFOLLOW`），路徑中任何一段是符號連結或有 `..` 都拒絕；之後的讀、建立、rename、刪除、flush 都在開好的目錄上做（`openat`、`renameat`、`unlinkat`），不再經過路徑文字，所以檢查之後目錄被搬走或換成連結，也寫不到別處。`broker.crl` 的目錄同樣不跟隨連結開啟，要屬於 root 或 `trustiot-agent`、別人不能寫（範例放在 agent 的 `StateDirectory`）：這樣只有 root 或 agent 能在裡面放東西，而 agent 建不了裝置檔，以 root 讀它不會開到別人放的裝置；內容要是新的根簽的才用。上層目錄（例如 `/etc`、`/var/lib`）的擁有者不檢查，靠作業系統的預設（只有 root 能寫）。
2. stdin 最多 96 KiB（其中憑證的 DER 合計最多 32 KiB），先是 PEM `CERTIFICATE` 區塊，之後最多一個 PEM `X509 CRL` 區塊，其餘只能是空行：其他種類的區塊（例如私鑰）、任何其他文字、根 CRL 之後還有區塊、兩個根 CRL 都拒絕，錯誤訊息不引用輸入的內容。憑證剛好三張：閘道器憑證、簽發它的簽發 CA、根，順序固定；根 CRL 只能在最後。送了根 CRL 而設定檔沒有 `broker.crl`，或 `broker.crl` 的目錄不存在時，以 9 結束、不寫任何檔案：送來的根 CRL 不會被默默丟掉，否則操作人員會以為閘道器有檢查撤銷。
3. 鏈本身：閘道器憑證不是 CA、CN 剛好一個而且等於 `--hardware-id`；簽發 CA 是 CA、不是自簽、以它的金鑰驗得過閘道器憑證的簽章；根是自簽（主體等於簽發者）的 CA、以自己的金鑰驗得過、也驗得過簽發 CA 的簽章。簽章只用 TLS 握手接受的演算法驗證（與 `broker.ca` 的檢查相同）。順序顛倒、缺簽發 CA、鏈接不到送來的根、根不是自簽的 CA，都在這裡拒絕。
4. agent 自己的啟動檢查，對要寫入的內容做：根當作 `broker.ca`（只放根、只能一張）；設定了 `broker.crl` 時，以之後會用的根 CRL 檢查：有送根 CRL 時是第 7 項保留或寫入的那一份；沒送而檔案存在時，那個檔案要是這個根簽的（換了根而舊的根 CRL 還在時，agent 會拒絕啟動，所以在這裡就拒絕）；這個指令以 root 執行，只把 `broker.crl` 當一般檔案讀：先以 `fstatat` 確認是一般檔案（裝置檔不會被開啟），開檔不跟隨符號連結也不等待（`O_NOFOLLOW`、`O_NONBLOCK`），再以 `fstat` 確認一次，是符號連結、FIFO、目錄或裝置時以 9 結束，不讀它指向的東西；鏈當作 `identity.certificate`，以 TPM 金鑰檢查（`identity::from_chain`）：leaf 的公鑰必須等於 TPM 金鑰的公鑰，EKU 與 key usage 允許 TLS 用戶端驗證；最後以 webpki 的用戶端憑證檢查確認鏈接到根，有根 CRL 時一併檢查（根撤銷了的簽發 CA 被拒絕）。只有時間不對（以閘道器的時鐘看已過期或還沒生效）時在 stderr 警告，照樣安裝，理由與啟動時相同。
5. 不倒退換發過的憑證：`identity.certificate` 已經有同一把金鑰、notBefore 比送來的晚的憑證時，保留它、不寫這個檔案，`broker.ca` 照常安裝，結束碼 0，stdout 這個檔案那一行是 `renewed`（stderr 另有說明）。那是 agent 自己換發的（見「憑證換發」），而重送登錄時平台回的永遠是登錄當時那張（[services/certs/README.md](../../services/certs/README.md)「重送回原本的憑證」），換回去只會讓閘道器用舊的、可能已被撤銷或過期的憑證，再被 broker 拒絕後換發一次；閘道器手上已經是它要用的憑證，所以這不是失敗，也不要為了裝回登錄的憑證去刪它。只有現有的鏈在要安裝的根（與根 CRL）之下通過用戶端憑證檢查（不看時間）才算數：換了根之後，舊根之下較新的憑證本來就連不上，照常換掉，重新安裝不會被它卡住。現有的檔案讀不了、解析不了、是別把金鑰的憑證或不在新的根之下時，照常換掉。從第 4 項的檢查到 rename 完成，`identity.certificate` 的目錄以 `flock` 鎖住，`broker.crl` 的目錄存在而且是另一個目錄時也鎖住那個目錄（不論有沒有送根 CRL：agent 取得根 CRL 時在這把鎖下重讀 `broker.ca`，所以不會把舊根的根 CRL 寫在安裝換上的新根旁邊，Laysi/trustiot-v2#926）；agent 換發寫檔時也取 `identity.certificate` 那一把鎖，所以換發不會夾在檢查與 rename 之間而被蓋掉，等鎖的換發會在安裝之後寫上去。同時執行的兩次安裝也輪流進行：第 7 項讀現有的根 CRL 是在鎖住之後，所以不會兩次都以同一份舊檔案比對、再由編號較小的那次最後 rename，把根 CRL 換回舊的（Laysi/trustiot-v2#832）。agent 自己取得根 CRL（Laysi/trustiot-v2#563，見「根 CRL 的更新」）讀寫 `broker.crl` 時也取 `broker.crl` 目錄的這把鎖，以同樣只往前的規則決定，所以與執行中的 agent 同時安裝也不會把根 CRL 換回舊的。
6. 沒送根 CRL 時與 Laysi/trustiot-v2#832 之前相同：`broker.crl` 只讀（第 4 項），stdout 兩行，所以不送根 CRL 的舊工作站照舊讀得懂新的 agent。
7. 送來的根 CRL 要通過 agent 啟動時載入根 CRL 的同一套檢查（以送來的根，對要寫入的標準 PEM 做），而且要有 CRL 編號（閘道器靠它保留最新的）。根 CRL 只往前，比對 `broker.crl` 現有的檔案：
   - 同一個根簽的、編號較大：保留現有的，不寫這個檔案，stdout 這一行是 `newer`（stderr 另有說明），結束碼 0。重送登錄時平台回的是它當時的根 CRL，而閘道器之後取用的（Laysi/trustiot-v2#563）也是較新的，換回較舊的只會少了撤銷。
   - 同一個根簽的、編號相同但內容不同：全部拒絕（8），不寫任何檔案；一個編號只對應一份根 CRL，兩份之中有一份是錯的。
   - 同一個根簽的、編號相同內容也相同：結果是 `unchanged`（內容、擁有者與權限都對時不動）。
   - 其他情況：沒有檔案、編號較小、沒有編號、不是送來的根簽的（例如舊根的）、壞掉的，都換成送來的。所以別的根的根 CRL 只會跟著那個根一起換掉，不會單獨被換。

**寫入**：鏈（leaf、簽發 CA）寫到 `identity.certificate`，擁有者是 `trustiot-agent` 帳號與它的主要群組；根寫到 `broker.ca`，擁有者是 root:root；有送根 CRL 而第 7 項要寫時，寫到 `broker.crl`，擁有者是它所在目錄的擁有者（root 或 `trustiot-agent`）；權限都是 0644（都是公開資料）。有送根 CRL 時，`broker.crl` 旁邊的記號檔 `<broker.crl>.highest`（見「根 CRL 的更新」）也升到 agent 之後會用的那張根 CRL 的編號（同一個根時只升不降；換了根時改記新的根），擁有者與權限同 `broker.crl`，在它之後 rename，不列在 stdout。所以刪掉 `broker.crl` 之後以較舊的根 CRL 重新安裝，agent 會扣住根 CRL，直到取得不低於記號的那一張；這時安裝照樣成功，但 stderr 會說明 agent 在取得之前不會連線。內容是重新以 DER 編碼的標準 PEM（每行 64 字元），不是 stdin 的原文，所以重跑時比得出相同。

- 內容、擁有者、權限都已經對的檔案不動（同一個 inode），所以重跑沒有副作用；不是一般檔案（例如連結）或權限不對的，即使內容相同也換掉。
- 要換的檔案先在同一個（開好的）目錄寫成新檔（`openat` 加 `O_EXCL`、`O_NOFOLLOW`，名稱以 `.` 開頭），寫完、設好擁有者與權限、flush 到磁碟，要換的檔案都準備好之後才 rename 蓋過舊檔，再 flush 目錄。準備過程中任何一步失敗（例如不是 root 而設不了擁有者），新檔全部刪掉、舊檔不變。寫入過程中斷時舊檔是完整的；rename 本身是原子的，但幾個 rename 合起來不是：中間當機時有的新有的舊，重跑即補齊（例如新的根已寫而根 CRL 還是舊根的，agent 啟動會拒絕，直到重跑寫上新的根 CRL）。
- 要以 root 執行（要設定擁有者）；工作站經 ssh 時視需要加 `sudo -n`。
- **前提：能執行這個指令的人就決定了閘道器信任哪個根。** 它以 root 寫入設定檔指定的位置、換掉 `broker.ca`，所以工作站的 ssh 存取只能執行 agent 的 `provision` 子命令：以專用的金鑰登入，在 `authorized_keys` 以 `command=`（加 `restrict`）或 sshd 的 `ForceCommand` 指向只接受 `trustiot-agent provision install --hardware-id <id>`（與取 CSR 的 `provision`）的固定指令，或以 sudoers 規則把整串參數寫死，不能是可以執行任意指令的 root 或 sudo 權限。設定檔不能由參數指定，也是同樣的理由。
- 不重啟 agent：agent 啟動時才讀這些檔案。閘道器剛重刷、還沒有憑證時，agent 啟動失敗後由 systemd 重啟（最多隔 1 分鐘），下一次啟動就會讀到；換了根或憑證而 agent 正在執行時，要 `systemctl restart trustiot-agent`。

stdout 每個檔案一行，`identity.certificate` 在前、`broker.ca` 其次，有送根 CRL 時 `broker.crl` 最後（沒送時只有兩行）：`<路徑>: written`、`<路徑>: unchanged`，或（只有 `identity.certificate`）`<路徑>: renewed`（保留 agent 換發過的較新憑證，見上面第 5 項），或（只有 `broker.crl`）`<路徑>: newer`（保留編號較大的根 CRL，見上面第 7 項）；診斷寫 stderr。結束碼（`provision::install::Exit`，與 `provision` 的 3–7 不重疊，工作站分得出來）：

| 結束碼 | 意思 |
| --- | --- |
| 0 | 每個檔案都是送來的內容（這次寫的或本來就是），或 `identity.certificate` 保留了 agent 換發過的較新憑證（`renewed`）、`broker.crl` 保留了同一個根編號較大的根 CRL（`newer`） |
| 1 | 其他失敗：TPM 錯誤、讀不到 stdin、寫不了檔案（沒有 rename 任何檔案，除非訊息說了第一個已換掉） |
| 2 | 參數錯誤 |
| 8 | 憑證或根 CRL 被拒絕（上面第 2 到 4 項與第 7 項），沒有寫任何檔案 |
| 9 | agent 的安裝不能用：沒有設定檔或載入不了、設定檔或它的目錄不屬於 root 或別人能寫、金鑰不是 TPM、目錄不存在、目錄的擁有者不對或別人能寫、路徑經過符號連結或有 `..`、`identity.certificate`、`broker.ca`、`broker.crl` 有兩個是同一個檔案（以目錄的裝置與 inode 加上檔名比對）、`broker.crl` 不是一般檔案或它的目錄別人能寫、送了根 CRL 而設定檔沒有 `broker.crl` 或它的目錄不存在、沒有 `trustiot-agent` 帳號；沒有寫任何檔案 |

閘道器重刷之後，工作站重跑同一個 `register`（平台回 200 與同一份登錄，見 [factory/workstation](../../factory/workstation/README.md)），憑證就照同樣的方式裝回去；回應帶的是平台當時的根 CRL。

**新工作站對舊 agent**：Laysi/trustiot-v2#832 之前的 agent 只接受 `CERTIFICATE` 區塊，收到根 CRL 時以 8 結束（「a PEM block that is not a CERTIFICATE; only certificates are accepted」），不寫任何檔案；工作站在這個情況提示先更新閘道器上的 agent 再重跑。舊工作站不送根 CRL，新 agent 照舊安裝、回兩行。

## 測試

```sh
cargo test -p agent                                     # 全部
cargo test -p agent --lib                               # 單元測試：設定、憑證、TLS、MQTT 編解碼、退避、心跳、sd_notify、liveness、憑證換發（排程、重試、檢查、原子寫檔），以及 Modbus 的規劃、PDU、解碼、輪詢、MBAP 與 RTU 切框、共用連線的輪流、keepalive、一小時的節拍漂移、設定範圍、連線位址的限制；緩衝的上限與丟棄、磁碟格式與復原、時鐘、組批；即時值的租約（上限、到期、renew、stop）與租約期間的輪詢節拍
cargo test -p agent --test link                         # MQTT 行為，對腳本化的 broker：在線狀態、訂閱、in-flight 上限、PUBACK、keepalive、退避、協定錯誤、停止、換發後的連線輪替與憑證被拒的判斷
cargo test -p agent --test liveness                     # systemd watchdog 的進度判斷：連線的每個等待都宣告期限、卡住的連線工作讓通知停下
cargo test -p agent --test transport                    # 整條連線在行程內：兩層 TLS 協商的內容、resumption、重連、每種拒絕、內層的憑證鏈、名稱、用途與根 CRL
cargo test -p agent --test root_crl_fixture             # certs-ca 簽的根 CRL（tests/fixtures/root-crl/，Laysi/trustiot-v2#829）以 RootCrl::load 載入：編號、撤銷的簽發 CA、過期之後仍載入
cargo test -p agent --test upload                       # 遙測上傳，對腳本化的雲端與停住的時鐘：斷線一小時、強制結束後的重送與新串流、上限與丟棄、時鐘、確認的每一種結果、壞掉的紀錄
cargo test -p agent --test broker                       # 對 Mosquitto 容器（podman 或 docker）：mTLS、retained、0x87、will、broker 與轉接重啟後重連、遙測批次與確認的來回
cargo test -p agent --test renewal                      # 憑證換發，對替代的 HTTPS 換發端點：被拒時換發並重連、429／503 的 Retry-After、外層 TLS 的 alert 不觸發換發；對 Mosquitto 容器：到期前換發不掉訊息、過期後換發
cargo test -p agent --test binary                       # 執行檔：--check（含根 CRL 的狀態）、沒有完成出廠登錄或沒有有效憑證時不運作（不送 READY=1、不連線、不輪詢存下的設定）、sd_notify、訂閱設定與 telemetry-ack、套用設定、重開後以存下的設定輪詢、狀態目錄裡的批次送出並由確認拿掉、SIGTERM；provision 的參數錯誤、沒有 TPM 裝置、非 /dev/tpmrm 的裝置
cargo test -p agent --test systemd_unit                 # unit 檔與 systemd-analyze，含出口規則的 unit 與 agent 對它的依賴、IPAddressDeny=、檢查的 timer 與 unit
cargo test -p agent --test secret_inputs                # 不收 PIN 或密碼：參數、設定檔的欄位、讀取的環境變數、不啟動其他程式
cargo test -p agent --test egress                       # Debian 13 容器（podman 或 docker）：出口規則拒絕 agent 帳號連本機、廣播、多播與 metadata 位址，其他帳號不受影響；經網路的 DNS 解析並連上雲端的替身；check-gateway.sh；systemd 為 PID 1 時規則先於 agent 生效、載入失敗時 agent 不啟動、nftables.service 的 reload、restart、stop、規則被清掉之後 agent 的自動重啟先重新載入規則（載入不了就不執行）；agent 執行中規則被清掉時檢查在上限內載回、IPAddressDeny= 在那之前仍拒絕（後者要以 root 執行，見下方）；enable 之後重開機
cargo test -p agent --test dependencies                 # 閘道器建置的相依
cargo test -p agent --test modbus_address_guard         # 正式路徑（Endpoints::new、TcpConnection::new）拒絕閘道器本機與 metadata 位址，假端點收不到連線
cargo test -p agent --test allowlist                    # Debian 13 容器（podman 或 docker）：fieldbus 放行集合的元件依清單更新集合、沒有預設拒絕的 table 時失敗、不合格的請求與 nft 的失敗讓 ruleset 不變；systemd 為 PID 1 時只有 agent 帳號連得上、instance 只有 CAP_NET_ADMIN
cargo test -p agent --test default_deny                 # Debian 13 容器（podman 或 docker）：預設拒絕之下 agent 帳號只連得到清單、雲端的 443 與 resolv.conf 的 nameserver，其餘被拒並計數、其他帳號不受影響；nameserver 集合跟著 resolv.conf 改變；哪些載入保留或清空集合；systemd 為 PID 1 時 path unit、agent 的啟動與 nftables.service 的 reload 填回 nameserver
cargo test -p agent --test modbus_fake_endpoint         # 對 testkit 假端點：型別、合併讀取、每種故障
cargo test -p agent --test modbus_tcp_framing           # 以 raw socket 驗證哪個回應算數、框架同步
cargo test -p agent --test modbus_rtu_converter         # 對 testkit 假 RS485 轉換器：共用連線、輪流、廣播、站號與形狀的過濾
cargo test -p agent --test modbus_rtu_framing           # 以 raw socket 驗證 RTU 的請求 bytes、回送、雜訊、切斷的框架與重連
cargo test -p agent --test polling_recovery             # 對 testkit 假端點跑輪詢迴圈：watchdog 救回不回答與半開的連線、只有讀到值才重置退避
cargo test -p agent --test desired_config               # 設定下發與套用：移除的設備不再輪詢、其他設備不受影響、無效設定被拒、版本規則、斷網重開與重開後收斂；輪詢的讀值成為批次、由確認拿掉
cargo test -p agent --test live                         # 即時值，對腳本化的雲端與 testkit 假端點：租約涵蓋的設備每次取樣立即送到 up/live（QoS 0）、不進緩衝與批次；租約到期、stop 後停止送出，renew 延後結束
cargo test -p agent --test tpm_swtpm                    # swtpm 容器：簽章、mTLS 握手、拒絕不能用的金鑰
cargo test -p agent --test tpm_provision                # swtpm 容器：金鑰佈建的建立、沿用、拒絕、同時建立、中斷、tpm2_clear
cargo test -p agent --test tpm_endorsement              # swtpm 容器：EK 等於 tpm2_createek 的、重跑與換連線相同；EK 憑證相符、不符、沒有；endorsement 授權與 NV 的 EK 範本被拒
cargo test -p agent --test provision                    # swtpm 容器：provision 的報告（CSR 驗得過、公鑰與 EK 相符）、不含私密資料、重跑沿用、--inspect 不改 TPM、EK 憑證不符與 handle 被佔的結束碼
cargo test -p agent --test provision_install            # swtpm 容器：provision install 拒絕錯的憑證且不寫任何檔案、擁有者與權限、agent 的啟動檢查接受裝好的檔案、重跑不寫、換根時原子替換、設定檔與目錄的擁有者、權限與符號連結的檢查（不讀設定、不寫任何檔案）；根 CRL 寫到 broker.crl、只往前（保留較新的、同編號不同內容與別的根的拒絕）、全有或全無、沒設 broker.crl 時以 9 結束；記號檔只升不降、刪掉 broker.crl 後裝回較舊的會被 agent 扣住；與 agent 的根 CRL 更新同時等同一把鎖時，不論誰先，留下的都是較新的
cargo test -p agent --test root_crl_update              # 根 CRL 的更新：換上較新的之後下一條連線是完整交握並以它檢查（撤銷簽發 CA 的被拒）、較舊與偽造的拒絕、重開後沿用、刪掉或換成較舊的檔案時扣住連線並自己恢復、心跳的 root_crl 與過期仍連線、HTTPS 請求是 cloud.url 主機上的 GET
cargo test -p agent --lib revocation                    # 只往前的判斷、記號檔的格式、寫一半中斷時留下舊檔、與 provision install 同一把鎖
```

`tools/lab-dataset` 的錄製工具（`lab-dataset record`）以 library 使用 `modbus::endpoint::Endpoints`、`DeviceReader` 與 `polling::run`（沒有即時值，等於 `polling::run_live` 配 `NoLive`）；改它們的介面時一併跑 `cargo test -p lab-dataset --test record`。錄製工具同樣經過位址限制，它的測試以 `exempt_for_tests` 放行假轉換器所在的 127.0.0.1。

- `link`、`upload` 與 Modbus 的整合測試都用停住的時鐘（`start_paused`，Modbus 另加 `testkit::clock::manual()`）：keepalive、退避與逾時只在其他事情都停下時才觸發，機器再忙也不會誤判。
- 位址限制的測試例外：被拒絕的連線不等任何計時器，它們用真的時鐘與 60 秒的連線逾時，正常情況下碰不到。連到假 Modbus 端點的測試都以 `exempt_for_tests` 放行 127.0.0.1（見「連線位址的限制」）。位址限制的單元測試以替身解析器與撥號紀錄驗證只查一次、只撥檢查過的位址；替身只真的撥 loopback，所以測試不會對執行機器所在的網路送封包。
- `transport`、`broker`、`renewal`、`binary` 走真的 socket，每個等待都以 `testkit::clock::bounded` 設上限。WebSocket 前端（替代入口與 mqtt-bridge）、測試 CA 與 Mosquitto 容器來自 `testkit::mqtt`，用法見 [`crates/testkit/README.md`](../../crates/testkit/README.md)「MQTT 測試環境」。所有憑證與金鑰在測試時產生，不進 repo。
- `broker` 與 `renewal` 的 Mosquitto 測試需要 podman 或 docker：沒有時略過，`CI` 有設時失敗。
- `egress` 裡驗證 `IPAddressDeny=` 的測試（`with_its_rules_flushed_the_agents_unit_still_refuses_loopback_metadata_multicast_and_broadcast`）要 systemd 能載入 cgroup BPF 程式：rootless podman 的容器裡 systemd 不套用 `IPAddressDeny=`，測試先以一個 `IPAddressDeny=any` 的暫時 unit 確認，套用不了就印出原因並略過（`CI` 有設時也略過）。以 root 執行一次就會跑：先 `cargo test --locked -p agent --test egress --no-run` 取得測試執行檔，再以 root 執行它並加上這個名稱篩選（rootful podman；建議以 `CONTAINERS_STORAGE_CONF` 指向一個暫時的 storage 設定，用完連同映像一起刪掉，不動 root 原本的容器儲存）。
- `egress` 的容器由 `tests/egress/Containerfile` 建置（空的 build context，第一次執行時建一次，留下 `localhost/agent-egress-test:<Containerfile 的雜湊>`）。規則只載入容器自己的 network namespace（`--network none`），不碰執行機器的規則；容器要 `NET_ADMIN` 與 `SYS_ADMIN`（rootless podman 下只作用在容器自己的 namespace）。沒有 podman 或 docker 時略過，`CI` 有設時失敗。systemd 的那個測試只用 podman，容器裡的 systemd 開不起來時略過並印出原因，`CI` 有設時也一樣略過（不確定 CI 的 runner 能跑 systemd 容器；兩個 unit 檔本身由 `systemd_unit` 在任何機器上檢查），和「systemd」裡 `systemd-run --user` 的測試相同。systemd 的容器不帶 SELinux 標籤（`--security-opt label=disable`）：SELinux enforcing 的主機（例如 Fedora）上，帶標籤的容器裡 `trustiot-agent.service` 的依賴起不來。測試網路的位址與在容器裡跑的指令見 `tests/egress/lab.sh`；它以 `nsenter` 進入另一個 namespace，不用 `ip netns exec` 或 `ip -n`，因為那兩個會為 namespace 掛載 sysfs，而 podman 遮蔽了 `/sys` 的部分路徑（例如 `/sys/firmware`）時，核心拒絕這個掛載。
- 假 Modbus 端點的用法見 [`crates/testkit/README.md`](../../crates/testkit/README.md)「假 Modbus 端點」。
- `allowlist`（Laysi/trustiot-v2#864）用 `egress` 的同一個映像，把這次建置的 `trustiot-agent-allowlist` 以 `podman cp`（或 `docker cp`）放進容器。第一個測試以 root 直接執行它（`lab network` 的網路、Laysi/trustiot-v2#813 的規則已載入），先確認沒有預設拒絕的 table 時請求失敗、什麼都不建立，再載入預設拒絕的替身（`Allowlist::declarations()` 加上只讓 agent 帳號連集合裡的 TCP 目的地的 chain），比對每次請求前後的整個 `nft list ruleset`（counter 的數字除外）；第二個以 systemd 為 PID 1、裝好兩個 unit，以 `setpriv` 換成 agent 帳號或 `nobody` 經 socat 連 socket，並以暫時的 drop-in 把 `ExecStart=` 換成印出 `/proc/self/status` 的指令，檢查 instance 的 UID、capability、`NoNewPrivs`、seccomp 與唯讀的檔案系統。略過的條件與 `egress` 相同。兩個 unit 檔由 `systemd_unit` 檢查（各行設定、`systemd-analyze verify` 與 instance 的評分不高於 1.5），元件的原始碼由 `secret_inputs` 檢查（只啟動 nft、不讀環境變數、不收參數）。
- `default_deny`（Laysi/trustiot-v2#879）用 `egress` 的同一個映像與 `lab.sh` 的網路，在對端再開 TCP 502 的 fieldbus 替身（IPv4 與 IPv6）與第二個 DNS 伺服器（`10.81.3.53`、`fd81:3::53`），容器以 `--dns none` 啟動，`/etc/resolv.conf` 是容器自己的檔案。把這次建置的 `trustiot-agent-allowlist` 與 `trustiot-agent-nameservers` 放進容器，出口規則檔照「給 Laysi/trustiot-v2#866 的接法」加上 `include`。第一個測試以 root 直接執行：每個被拒的目的地先在沒有預設拒絕時連得上，載入之後 agent 帳號被拒、`default deny` 的 counter 增加、Laysi/trustiot-v2#813 的 counter 不變、`nobody` 照樣連得上；經 nameserver 來源路由（IPv4 LSRR）到別的主機的 UDP 53 被 `IP options` 的規則拒絕（容器多給 `NET_RAW`，只交給這個探測的行程：有的核心不讓沒有 `CAP_NET_RAW` 的 socket 設定來源路由）；集合空著時雲端與 DNS 以外全被拒，填入之後清單裡的目的地、443 與 nameserver 的 UDP、TCP 53 連得上（含以名稱解析）；resolv.conf 改變（IPv6 與 zone、超過 3 個、滿額換成另一份滿額、檔案不見）時 nameserver 集合跟著變；重新載入出口規則時集合保留、chain 換新，以 `/etc/nftables.conf` 的方式載入時集合清空，`flush ruleset` 之後兩個執行檔都失敗。第二個以 systemd 為 PID 1、照接法裝好 unit 與 drop-in（agent 換成替身）：就地寫入與 rename 蓋過 resolv.conf 都由 path unit 套用；`nftables.service` 的 reload 清空 fieldbus 集合但 drop-in 填回 nameserver；`flush ruleset` 加上結束替身之後，自動重啟填回 nameserver；`nftables.service` 的 restart 也一樣。略過的條件與 `egress` 相同。兩個 unit 檔由 `systemd_unit` 檢查（各行設定、`systemd-analyze verify` 與評分不高於 1.5），執行檔的原始碼由 `secret_inputs` 檢查（只啟動 nft、只讀 `/etc/resolv.conf`、不讀環境變數、不收參數）。
- `tpm_swtpm`、`tpm_provision`、`tpm_endorsement` 與 `provision` 的 swtpm 來自 `testkit::swtpm`（映像、session 與 tpm2-tools 的用法見 [`crates/testkit/README.md`](../../crates/testkit/README.md)「軟體 TPM」）。`tpm_swtpm` 以 tpm2-tools 佈建金鑰，比對 tpm2-tools 算的 SubjectPublicKeyInfo、以公鑰驗證簽章，並做一次真的 TLS 1.3 mTLS 握手（測試 CA 為 TPM 公鑰簽發用戶端憑證，CertificateVerify 剛好一次 TPM2_Sign，resumption 不簽章）。沒有 podman 或 docker 時略過，設了 `CI` 時失敗。解析與執行緒的行為在單元測試（`cargo test -p agent --lib tpm`）以 swtpm 錄下的回應與假 TPM 測。
